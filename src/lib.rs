//! chatgpt-oauth — pure low-level client for the ChatGPT backend, using a
//! ChatGPT subscription OAuth token (not a paid API key).

// 모듈은 private. 공개 표면은 아래 `pub use` 재export 목록이 유일한 경계다.
// (pub mod 였을 땐 `chatgpt_oauth::auth::<내부함수>` 로 모든 pub 항목이 새어나갔다.)
mod auth;
mod client;
mod error;
mod event;
mod input;

// 진단용 raw-응답 캡처. feature = "capture" 에서만 컴파일되며, 켜면 공개 모듈로 노출된다.
// 기본 빌드/공개 API 엔 영향 없음.
#[cfg(feature = "capture")]
pub mod capture;

pub use auth::{
    AuthError, CodexCredentials, auth_path, device_code_login, is_access_token_expiring,
    is_relogin_required, load_codex_cli_tokens, resolve_credentials, resolve_credentials_after_401,
    save_codex_cli_tokens_locked, validate_base_url, validate_token_destination,
};
pub use client::{
    Model, RateLimit, RateWindow, SendOptions, Usage, extract_text, fetch_usage, list_models,
    open_stream, open_stream_with_input, send_message,
};
pub use error::ClientError;
// 타입 이벤트 레이어 (open_stream 의 ergonomic 버전 — additive, 원본 open_stream 은 그대로).
pub use event::{
    GeneratedImage, Response, StreamEvent, TokenUsage, ToolCall, WebSearch, open_event_stream,
    open_event_stream_with_input,
};
// 입력 빌더 (tools 선언 / input 배열을 생 json! 대신 타입으로).
pub use input::{InputItem, Tool};
