//! 단발(one-shot) 채팅 예제 — `chatgpt-oauth` 크레이트의 공개 API만 사용한다.
//!
//! 사용자 메시지 하나를 ChatGPT 백엔드로 보내고 응답 텍스트를 출력한다.
//! 히스토리도, REPL 도 없다. 정확히 한 턴. (크레이트 컨셉: SDK-style, no session.)
//!
//! 실행:
//!   cargo run --example oneshot_chat -- "안녕, 한 문장으로 자기소개 해줘"
//!
//! 첫 실행에서 토큰이 없으면 device-code 로그인 흐름이 시작된다(출력되는 URL/코드를
//! 따라가면 됨). 토큰은 ~/.codex/auth.json 에 저장돼 다음 실행부터 재사용된다.
//!
//! 환경변수:
//!   CODEX_DEFAULT_MODEL  기본 모델 override (미설정 시 SendOptions::default 값 사용)

use std::process::ExitCode;

use chatgpt_oauth::{
    SendOptions, device_code_login, extract_text, load_codex_cli_tokens, send_message,
};

#[tokio::main]
async fn main() -> ExitCode {
    // 1) CLI 인자(프로그램 이름 제외)를 공백으로 이어 메시지로 사용.
    let prompt = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    if prompt.trim().is_empty() {
        eprintln!("usage: cargo run --example oneshot_chat -- <your message>");
        return ExitCode::from(2);
    }

    // 2) 저장된 토큰 확인. 없으면 device-code 로그인, 있으면 그대로 재사용.
    //    (만료된 access_token 은 아래 send_message 가 내부에서 알아서 갱신한다.)
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
            // 손상된 auth.json 등. 메시지대로 백업/삭제 후 재시도하면 됨.
            eprintln!("failed to read saved token: {e:#}");
            return ExitCode::FAILURE;
        }
    }

    // 3) 단발 전송. send_message 가 creds 해석 + 401 자동 갱신 + 재시도를 모두 처리한다.
    let opts = SendOptions::default();
    match send_message(&prompt, &opts).await {
        Ok(response) => {
            let text = extract_text(&response);
            if text.is_empty() {
                eprintln!("(응답에 텍스트가 없습니다)");
                ExitCode::FAILURE
            } else {
                println!("{text}");
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            eprintln!("request failed: {e}");
            ExitCode::FAILURE
        }
    }
}
