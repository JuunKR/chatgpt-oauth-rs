//! 엔드포인트 응답 raw 캡처 도구 — 이 크레이트가 때리는 각 요청의 **응답을 파싱 전
//! 원본 그대로 JSON 으로** 뽑는다. OpenAI 가 응답 모양을 바꿔도 디버그 코드를 새로 짜지
//! 않고, 이 도구 한 번 돌려 "지금 실제로 뭐가 오는지"를 보고 파서를 고치면 된다.
//!
//! `capture` feature 필요(라이브 API 캡처 도구라 기본 빌드에선 제외).
//!
//! 초점: **우리가 통제 못 하는 표면**(서버 빌트인 툴 / 엔드포인트 응답)의 원본 캡처.
//! 커스텀 function 툴은 우리가 스키마·실행·결과를 다 통제하므로 이 도구에서 다루지 않는다.
//!
//! 시나리오:
//!   usage            GET /wham/usage 사용량/rate-limit 응답 원본              (✅ 안전, 멱등)
//!   models           GET /models 모델 목록 응답 원본                          (✅ 안전, 멱등)
//!   responses <msg>  POST /responses SSE 이벤트 원본. 빌트인 툴 플래그:        (⚠️ 토큰 소모)
//!                      --web   <msg>  서버 빌트인 web_search → web_search_call 관찰
//!                      --image <msg>  서버 빌트인 image_generation 관찰(결과 base64 클 수 있음)
//!   builtin-probe    후보 빌트인 툴들을 보내 200(수용)/4xx(미지원) 전수 확인 → 카탈로그  (⚠️ 토큰 소모)
//!   device-code      POST /deviceauth/usercode (로그인 시작 POST만) 응답 원본   (✅ 안전, 폴링 안 함)
//!
//! **자동 저장**: 결과를 `captures/<오늘날짜>/<시나리오>.json` 에 직접 쓴다(`>` 리다이렉트 불필요).
//! **한 방에 전부**: 인자 없거나 `all` 이면 모든 시나리오를 순서대로 캡처한다.
//!
//! 예:
//!   cargo run --example capture --features capture                      # all (전부) → captures/<날짜>/*.json
//!   cargo run --example capture --features capture -- usage             # 하나만
//!   cargo run --example capture --features capture -- responses --image # 이미지만 (프롬프트 생략 시 기본값)
//!   cargo run --example capture --features capture -- builtin-probe
//!
//! 각 파일 형식: { "meta": {scenario, captured_at_unix}, "capture": <시나리오별 본문> }
//! 진행/요약은 stderr 로. (captures/ 는 .gitignore 처리됨)
//!
//! 부작용 메모: usage(GET)·device-code(로그인 시작 POST만)는 안전, responses·builtin-probe
//! 는 토큰/쿼터 소모. refresh(`/oauth/token`)는 refresh_token 을 회전시켜 저장 인증을 깰 수
//! 있어 이 도구에 일부러 넣지 않았다(STREAM_EVENTS_OBSERVED.md 참고).

use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use chatgpt_oauth::{SendOptions, capture, device_code_login, load_codex_cli_tokens};
use serde_json::{Value, json};

/// "all" 에서 도는 시나리오 목록. (name, 기본 프롬프트). 프롬프트가 필요 없는 건 "".
const ALL_SCENARIOS: &[(&str, &str)] = &[
    ("usage", ""),
    ("models", ""),
    ("responses-text", "안녕, 한 문장으로 자기소개 해줘"),
    ("responses-web", "오늘 최신 뉴스 한 줄만 웹 검색해서 알려줘"),
    ("responses-image", "작은 고양이 아이콘 하나 그려줘"),
    ("builtin-probe", ""),
    ("device-code", ""),
];

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let scenario = args.first().cloned().unwrap_or_else(|| "all".into());

    // 실행할 (name, prompt) 목록을 정한다.
    let plan: Vec<(String, String)> = match scenario.as_str() {
        "all" => ALL_SCENARIOS.iter().map(|(n, p)| (n.to_string(), p.to_string())).collect(),
        "usage" | "models" | "builtin-probe" | "device-code" => {
            vec![(scenario.clone(), String::new())]
        }
        "responses" => {
            // 플래그(--web/--image) + 선택 프롬프트 → 하나의 (name, prompt).
            let rest = &args[1..];
            let flag = rest.first().map(|s| s.as_str()).unwrap_or("");
            let (name, skip, default_prompt) = match flag {
                "--web" => ("responses-web", 1, "오늘 최신 뉴스 한 줄만 웹 검색해서 알려줘"),
                "--image" => ("responses-image", 1, "작은 고양이 아이콘 하나 그려줘"),
                _ => ("responses-text", 0, "안녕, 한 문장으로 자기소개 해줘"),
            };
            let prompt = rest[skip..].join(" ");
            let prompt = if prompt.trim().is_empty() { default_prompt.to_string() } else { prompt };
            vec![(name.to_string(), prompt)]
        }
        _ => {
            eprintln!(
                "usage: cargo run --example capture --features capture -- [all|usage|responses [--web|--image] [prompt]|builtin-probe|device-code]\n  인자 없으면 all (전부 캡처). 결과는 captures/<날짜>/<시나리오>.json 에 자동 저장."
            );
            return ExitCode::from(2);
        }
    };

    // device-code 단독이 아니면 로그인 필요(usage/responses).
    let needs_login = plan.iter().any(|(n, _)| n != "device-code");
    if needs_login && let Err(e) = ensure_login().await {
        return fail(e);
    }

    // 오늘 날짜 디렉터리 자동 생성. 파일명은 우리가 정한다(사용자가 > 리다이렉트 안 해도 됨).
    let dir = format!("captures/{}", capture::today_utc());
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return fail(format!("failed to create {dir}: {e}"));
    }

    let captured_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    eprintln!("▶ 캡처 시작 → {dir}/ ({} 시나리오)\n", plan.len());
    let mut ok = 0;
    let mut fail_n = 0;
    for (name, prompt) in &plan {
        let result = run_one(name, prompt).await;
        let (cap_val, status) = match result {
            Ok(v) => (v, "OK".to_string()),
            Err(e) => (json!({ "error": e }), "ERROR".to_string()),
        };
        let doc = json!({
            "meta": { "scenario": name, "captured_at_unix": captured_at_unix },
            "capture": cap_val,
        });
        let path = format!("{dir}/{name}.json");
        match std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap_or_default()) {
            Ok(()) => {
                let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                eprintln!("  {status:5}  {path}  ({bytes} bytes)");
                if status == "OK" { ok += 1 } else { fail_n += 1 }
            }
            Err(e) => {
                eprintln!("  WRITE-FAIL  {path}: {e}");
                fail_n += 1;
            }
        }
    }
    eprintln!("\n완료: {ok} OK, {fail_n} 실패 → {dir}/");
    if fail_n > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

/// 시나리오 이름 → 캡처 실행. responses-* 는 빌트인 툴 플래그가 이름에 반영돼 있다.
async fn run_one(name: &str, prompt: &str) -> Result<Value, String> {
    let to_val = |c: capture::RawCapture| serde_json::to_value(c).unwrap_or(Value::Null);
    match name {
        "usage" => capture::usage_raw().await.map(to_val).map_err(|e| format!("{e:#}")),
        "models" => capture::models_raw().await.map(to_val).map_err(|e| format!("{e:#}")),
        "device-code" => capture::device_usercode_raw().await.map(to_val).map_err(|e| format!("{e:#}")),
        "builtin-probe" => builtin_probe().await,
        "responses-text" | "responses-web" | "responses-image" => {
            let tools = match name {
                "responses-web" => vec![json!({ "type": "web_search" })],
                "responses-image" => vec![json!({ "type": "image_generation" })],
                _ => Vec::new(),
            };
            let opts = SendOptions { tools, ..SendOptions::default() };
            capture::responses_raw(prompt, &opts)
                .await
                .map(|events| json!({ "prompt": prompt, "event_count": events.len(), "events": events }))
                .map_err(|e| format!("{e:#}"))
        }
        other => Err(format!("unknown scenario: {other}")),
    }
}

/// 서버 빌트인 툴 카탈로그 발견: 각 후보 타입을 보내 200(수용)/4xx(미지원·설정필요) 를 표로.
/// 통제 불가한 빌트인 툴 표면이라, 이 결과로 "이 백엔드가 실제로 받는 빌트인 툴"을 확정한다.
async fn builtin_probe() -> Result<Value, String> {
    // Responses API 빌트인 후보(이 codex 백엔드에서 되는지는 모름 → 실측).
    let candidates = [
        "web_search",
        "image_generation",
        "file_search",
        "code_interpreter",
        "computer_use",
        "local_shell",
    ];
    let mut results = Vec::new();
    for t in candidates {
        let entry = match capture::responses_probe_raw(vec![json!({ "type": t })], "hi").await {
            Ok(rc) => json!({
                "tool": t,
                "status": rc.status,
                "accepted": rc.status == 200,
                "body": rc.body,        // 200 이면 placeholder, 비-200 이면 에러 바디
            }),
            Err(e) => json!({ "tool": t, "error": format!("{e:#}") }),
        };
        results.push(entry);
    }
    Ok(json!({ "probed": results }))
}

async fn ensure_login() -> Result<(), String> {
    match load_codex_cli_tokens() {
        Ok(Some(_)) => Ok(()),
        Ok(None) => {
            eprintln!("저장된 토큰 없음 — device 로그인 시작...");
            device_code_login().await.map(|_| ()).map_err(|e| format!("login failed: {e:#}"))
        }
        Err(e) => Err(format!("failed to read token: {e:#}")),
    }
}

fn fail(msg: String) -> ExitCode {
    eprintln!("{msg}");
    ExitCode::FAILURE
}
