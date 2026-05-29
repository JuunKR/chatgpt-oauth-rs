//! /responses 스트림의 **모든 SSE 이벤트를 원본 그대로** 캡처해 **유효한 단일 JSON**
//! 으로 출력한다. 아무것도 숨기지 않는다(response 에코·tool_usage·usage 전부 포함).
//!
//! 목적: OpenAI 가 API 를 바꿔도 **코드를 새로 짤 필요 없이 한 번 실행**하면 최신 구조를
//! 통째로 까볼 수 있는 재사용 도구. stdout 은 순수 JSON 이라 파일로 저장하거나 jq 로
//! 분석/비교(diff)하기 좋다.
//!
//! 실행:
//!   cargo run --example dump_events -- "안녕"                 > capture.json
//!   cargo run --example dump_events -- --tool "서울 날씨? get_weather 써."  > tool.json
//!   cargo run --example dump_events -- --web  "최신 뉴스 검색해서 알려줘."   > web.json
//!   cargo run --example dump_events -- "안녕" | jq '.events[].type'   # 이벤트 타입만
//!
//! 출력 형태: { "meta": {prompt, mode, captured_at_unix, model, event_count}, "events": [ ... ] }

use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use chatgpt_oauth::{SendOptions, device_code_login, load_codex_cli_tokens, open_stream};
use futures_util::StreamExt;
use serde_json::{Value, json};

#[tokio::main]
async fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();

    // --tool / --web 플래그 처리 (첫 인자).
    //   --tool : get_weather function 툴 등록
    //   --web  : 서버 빌트인 web_search 툴 활성화
    let flag = args.first().cloned().unwrap_or_default();
    let with_tool = flag == "--tool";
    let with_web = flag == "--web";
    if with_tool || with_web {
        args.remove(0);
    }
    let prompt = args.join(" ");
    if prompt.trim().is_empty() {
        eprintln!("usage: cargo run --example dump_events -- [--tool] <message>");
        return ExitCode::from(2);
    }

    // 로그인 보장.
    match load_codex_cli_tokens() {
        Ok(Some(_)) => {}
        Ok(None) => {
            eprintln!("저장된 토큰 없음 — device 로그인 시작...");
            if let Err(e) = device_code_login().await {
                eprintln!("login failed: {e:#}");
                return ExitCode::FAILURE;
            }
        }
        Err(e) => {
            eprintln!("failed to read token: {e:#}");
            return ExitCode::FAILURE;
        }
    }

    let mut opts = SendOptions::default();
    if with_tool {
        // Responses API function 툴 스키마(평탄형: type/name/description/parameters).
        opts.tools = vec![json!({
            "type": "function",
            "name": "get_weather",
            "description": "주어진 도시의 현재 날씨를 반환한다.",
            "parameters": {
                "type": "object",
                "properties": { "city": { "type": "string", "description": "도시 이름" } },
                "required": ["city"]
            }
        })];
        opts.tool_choice = Some(json!("auto"));
    }
    if with_web {
        // 서버 빌트인 web_search 툴 활성화 (function 툴과 달리 우리가 실행하지 않음 —
        // 서버가 직접 검색하고 결과 item 을 스트림에 넣어준다).
        opts.tools = vec![json!({ "type": "web_search" })];
    }

    let stream = match open_stream(&prompt, &opts).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to open stream: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut stream = Box::pin(stream);

    // 모든 이벤트를 그대로 모은다(숨기는 것 없음).
    let mut events: Vec<Value> = Vec::new();
    let mut stream_error: Option<String> = None;
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(v) => events.push(v),
            Err(e) => {
                stream_error = Some(e.to_string());
                break; // 에러 직전까지 모은 이벤트는 그대로 내보낸다.
            }
        }
    }

    // 메타데이터 조립. captured_at_unix = 캡처 시각(에포크 초). 모델은 첫 response 에코에서.
    let captured_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mode = if with_tool {
        "tool"
    } else if with_web {
        "web"
    } else {
        "text"
    };
    let model = events
        .iter()
        .find_map(|e| e.get("response").and_then(|r| r.get("model")).and_then(|m| m.as_str()))
        .unwrap_or("")
        .to_string();

    let mut meta = json!({
        "prompt": prompt,
        "mode": mode,
        "captured_at_unix": captured_at_unix,
        "model": model,
        "event_count": events.len(),
    });
    if let Some(err) = &stream_error {
        meta["stream_error"] = json!(err);
    }

    // stdout = 순수 JSON(파일 저장/jq 용). 진행 메시지는 stderr 로만.
    let doc = json!({ "meta": meta, "events": events });
    println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
    eprintln!(
        "\n{} 개 이벤트 캡처 (mode={mode}). stdout 은 순수 JSON — 저장: `... > capture.json`",
        events.len()
    );
    if stream_error.is_some() {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
