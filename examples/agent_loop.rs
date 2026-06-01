//! 커스텀 툴 에이전트 루프 — 크레이트의 **타입 입력/출력만으로** 생 JSON 없이 한 사이클 완주.
//!
//!   선언: Tool::function(...)  → 모델이 function_call 요청(StreamEvent::ToolCall)
//!   실행: 내 코드(run_tool)     → 결과를 InputItem::function_output + tc.to_input_item() 로 되먹임
//!   완료: 모델이 결과로 최종 답(StreamEvent::TextDelta)
//!
//! 실행:
//!   cargo run --example agent_loop -- "서울 날씨 알려줘"

use std::io::Write;
use std::process::ExitCode;

use chatgpt_oauth::{
    InputItem, SendOptions, StreamEvent, Tool, ToolCall, device_code_login, load_codex_cli_tokens,
    open_event_stream_with_input,
};
use futures_util::StreamExt;
use serde_json::{Value, json};

#[tokio::main]
async fn main() -> ExitCode {
    let prompt = {
        let p = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
        if p.trim().is_empty() { "서울 날씨 알려줘".to_string() } else { p }
    };

    match load_codex_cli_tokens() {
        Ok(Some(_)) => {}
        Ok(None) => {
            eprintln!("토큰 없음 — device 로그인...");
            if let Err(e) = device_code_login().await {
                eprintln!("login failed: {e:#}");
                return ExitCode::FAILURE;
            }
        }
        Err(e) => {
            eprintln!("토큰 읽기 실패: {e:#}");
            return ExitCode::FAILURE;
        }
    }

    // 툴 선언 — 생 json! 대신 타입 빌더.
    let opts = SendOptions {
        instructions: "너는 날씨 비서다. 날씨를 물으면 반드시 get_weather 툴을 호출해라.".into(),
        tools: vec![Tool::function_described(
            "get_weather",
            "주어진 도시의 현재 날씨를 반환한다.",
            json!({"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}),
        )],
        tool_choice: Some(json!("auto")),
        ..Default::default()
    };

    // 대화 input — 타입 빌더로 시작.
    let mut input: Vec<Value> = vec![InputItem::user(&prompt)];

    for turn in 1..=5 {
        let mut stream =
            match open_event_stream_with_input(Value::Array(input.clone()), &opts).await {
                Ok(s) => Box::pin(s),
                Err(e) => {
                    eprintln!("stream open 실패: {e}");
                    return ExitCode::FAILURE;
                }
            };

        let mut text = String::new();
        let mut calls: Vec<ToolCall> = Vec::new();
        while let Some(ev) = stream.next().await {
            match ev {
                Ok(StreamEvent::TextDelta(d)) => {
                    print!("{d}");
                    let _ = std::io::stdout().flush();
                    text.push_str(&d);
                }
                Ok(StreamEvent::ToolCall(tc)) => calls.push(tc),
                Ok(StreamEvent::Failed(e)) => {
                    eprintln!("\nresponse failed: {e}");
                    return ExitCode::FAILURE;
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("\nstream error: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }

        // 툴 콜 없으면 최종 답 — 끝.
        if calls.is_empty() {
            println!();
            return ExitCode::SUCCESS;
        }

        // 툴 실행 후 결과 되먹임 (에코 + function_output) — 전부 타입 빌더.
        for tc in &calls {
            let args = tc.arguments_json().unwrap_or(Value::Null);
            let result = run_tool(&tc.name, &args);
            eprintln!("[turn {turn}] {}({}) -> {result}", tc.name, tc.arguments);
            input.push(tc.to_input_item()); // function_call 에코
            input.push(InputItem::function_output(&tc.call_id, result)); // 실행 결과
        }
    }

    eprintln!("최대 턴 초과");
    ExitCode::FAILURE
}

/// 실제 툴 실행(스텁). 요청한 city 를 그대로 반영해 일관된 결과를 돌려준다.
fn run_tool(name: &str, args: &Value) -> String {
    match name {
        "get_weather" => {
            let city = args.get("city").and_then(|c| c.as_str()).unwrap_or("unknown");
            json!({ "city": city, "temp_c": 21, "condition": "맑음" }).to_string()
        }
        _ => json!({ "error": "unknown tool" }).to_string(),
    }
}
