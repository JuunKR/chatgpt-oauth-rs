//! 스트리밍(streaming) 채팅 예제 — `chatgpt-oauth` 크레이트의 공개 API만 사용한다.
//!
//! `oneshot_chat` 과 달리 응답을 끝까지 모으지 않고, 토큰 조각(delta)이 도착하는 대로
//! 화면에 흘려 출력한다(타이핑되듯). 전송 계층은 두 예제 모두 SSE 스트리밍으로 동일하고,
//! 차이는 "다 모아서 한 번에" vs "오는 대로 실시간"이다.
//!
//! 실행:
//!   cargo run --example streaming_chat -- "긴 답이 필요한 질문을 해봐"
//!
//! 첫 실행에서 토큰이 없으면 device-code 로그인 흐름이 시작된다(출력되는 URL/코드를
//! 따라가면 됨). 토큰은 ~/.codex/auth.json 에 저장돼 다음 실행부터 재사용된다.

use std::io::Write;
use std::process::ExitCode;

use chatgpt_oauth::{
    SendOptions, StreamEvent, device_code_login, load_codex_cli_tokens, open_event_stream,
};
use futures_util::StreamExt; // 스트림에서 .next() 를 쓰기 위한 trait

#[tokio::main]
async fn main() -> ExitCode {
    // 1) CLI 인자(프로그램 이름 제외)를 공백으로 이어 메시지로 사용.
    let prompt = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    if prompt.trim().is_empty() {
        eprintln!("usage: cargo run --example streaming_chat -- <your message>");
        return ExitCode::from(2);
    }

    // 2) 저장된 토큰 확인. 없으면 device-code 로그인, 있으면 그대로 재사용.
    match load_codex_cli_tokens() {
        Ok(Some(_)) => {}
        Ok(None) => {
            eprintln!("저장된 토큰 없음 — device 로그인을 시작합니다...");
            if let Err(e) = device_code_login().await {
                eprintln!("login failed: {e:#}");
                return ExitCode::FAILURE;
            }
        }
        Err(e) => {
            eprintln!("failed to read saved token: {e:#}");
            return ExitCode::FAILURE;
        }
    }

    // 3) 스트림을 연다. open_stream 도 내부에서 creds 해석 + 401 갱신을 처리한다.
    let opts = SendOptions::default();
    // 타입 스트림(open_event_stream) — 매직 문자열 없이 StreamEvent 로 match.
    let stream = match open_event_stream(&prompt, &opts).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to open stream: {e}");
            return ExitCode::FAILURE;
        }
    };
    // 스트림은 Unpin 이 아니라(내부 async unfold), .next() 전에 Box::pin 으로 고정한다.
    let mut stream = Box::pin(stream);

    // 4) 이벤트를 오는 대로 처리. 텍스트 delta 는 즉시 출력(flush)하고, 터미널 실패/미완은
    //    에러로 surface. 그 외(created/툴 진행 등)는 무시.
    let mut printed_any = false;
    while let Some(ev) = stream.next().await {
        let ev = match ev {
            Ok(v) => v,
            Err(e) => {
                eprintln!("\nstream error: {e}");
                return ExitCode::FAILURE;
            }
        };
        match ev {
            StreamEvent::TextDelta(delta) => {
                print!("{delta}");
                let _ = std::io::stdout().flush(); // 줄바꿈 전이라도 즉시 화면에
                printed_any = true;
            }
            StreamEvent::Failed(err) => {
                eprintln!("\nresponse failed: {err}");
                return ExitCode::FAILURE;
            }
            StreamEvent::Incomplete(detail) => {
                eprintln!("\nresponse incomplete: {detail}");
                return ExitCode::FAILURE;
            }
            _ => {}
        }
    }

    if printed_any {
        println!(); // 마지막 delta 뒤 줄바꿈 마무리
        ExitCode::SUCCESS
    } else {
        eprintln!("(텍스트 delta 가 없었습니다)");
        ExitCode::FAILURE
    }
}
