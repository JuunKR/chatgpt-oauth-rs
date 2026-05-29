//! 엔드포인트 응답 raw 캡처 도구 — 이 크레이트가 때리는 각 요청의 **응답을 파싱 전
//! 원본 그대로 JSON 으로** 뽑는다. OpenAI 가 응답 모양을 바꿔도 디버그 코드를 새로 짜지
//! 않고, 이 도구 한 번 돌려 "지금 실제로 뭐가 오는지"를 보고 파서를 고치면 된다.
//!
//! `capture` feature 필요(라이브 API 캡처 도구라 기본 빌드에선 제외):
//!   cargo run --example capture --features capture -- usage              > usage.json
//!   cargo run --example capture --features capture -- responses "안녕"   > resp.json
//!   cargo run --example capture --features capture -- responses --tool "서울 날씨? get_weather 써."
//!   cargo run --example capture --features capture -- responses --web  "최신 뉴스 검색해서 알려줘."
//!   cargo run --example capture --features capture -- device-code        > device.json
//!
//! stdout = 순수 JSON(저장/jq/버전 diff 용). 진행 메시지는 stderr.
//!
//! 부작용 메모: usage(GET)·device-code(로그인 시작 POST만)는 안전, responses 는 토큰/쿼터
//! 소모. refresh(`/oauth/token`)는 refresh_token 을 회전시켜 저장 인증을 깰 수 있어 이 도구에
//! 일부러 넣지 않았다(STREAM_EVENTS_OBSERVED.md 참고).

use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use chatgpt_oauth::{SendOptions, capture, device_code_login, load_codex_cli_tokens};
use serde_json::{Value, json};

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let scenario = args.first().cloned().unwrap_or_default();

    let captured_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // 시나리오별 캡처 → (meta 보강용 mode, capture 본문 Value)
    let (mode, payload): (&str, Result<Value, String>) = match scenario.as_str() {
        "usage" => {
            if let Err(e) = ensure_login().await {
                return fail(e);
            }
            ("usage", capture::usage_raw().await.map(to_val).map_err(|e| format!("{e:#}")))
        }
        "device-code" => {
            // 로그인 불필요(로그인 시작 단계).
            (
                "device-code",
                capture::device_usercode_raw().await.map(to_val).map_err(|e| format!("{e:#}")),
            )
        }
        "responses" => {
            if let Err(e) = ensure_login().await {
                return fail(e);
            }
            let rest: Vec<String> = args[1..].to_vec();
            let with_tool = rest.first().map(|a| a == "--tool").unwrap_or(false);
            let with_web = rest.first().map(|a| a == "--web").unwrap_or(false);
            let prompt = if with_tool || with_web { rest[1..].join(" ") } else { rest.join(" ") };
            if prompt.trim().is_empty() {
                return fail("responses 시나리오엔 프롬프트가 필요합니다.".into());
            }
            let mut opts = SendOptions::default();
            let mode = if with_tool {
                opts.tools = vec![json!({
                    "type": "function", "name": "get_weather",
                    "description": "주어진 도시의 현재 날씨를 반환한다.",
                    "parameters": {"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}
                })];
                opts.tool_choice = Some(json!("auto"));
                "responses-tool"
            } else if with_web {
                opts.tools = vec![json!({ "type": "web_search" })];
                "responses-web"
            } else {
                "responses-text"
            };
            let payload = capture::responses_raw(&prompt, &opts)
                .await
                .map(|events| json!({ "prompt": prompt, "event_count": events.len(), "events": events }))
                .map_err(|e| format!("{e:#}"));
            (mode, payload)
        }
        _ => {
            eprintln!(
                "usage: cargo run --example capture --features capture -- <usage|responses|device-code> [...]"
            );
            return ExitCode::from(2);
        }
    };

    let capture_val = match payload {
        Ok(v) => v,
        Err(e) => return fail(e),
    };

    let doc = json!({
        "meta": { "scenario": mode, "captured_at_unix": captured_at_unix },
        "capture": capture_val,
    });
    println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
    eprintln!("\n캡처 완료 (scenario={mode}). stdout 은 순수 JSON — 저장: `... > capture.json`");
    ExitCode::SUCCESS
}

/// RawCapture(Serialize) → Value.
fn to_val(c: capture::RawCapture) -> Value {
    serde_json::to_value(c).unwrap_or(Value::Null)
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
