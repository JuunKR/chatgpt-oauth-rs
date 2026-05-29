//! 클라이언트 에러 타입 — HTTP 호출 결과를 "종류별"로 구분해, 소비자가
//! 프로그램적으로 재시도/처리 전략을 세울 수 있게 한다.
//!
//! 기존에는 거의 모든 실패가 `anyhow` 문자열이라 "이게 429야? 5xx야? 네트워크야?"를
//! 코드로 알 수 없었다. 이 enum 으로 status 를 데이터로 보존하고 `is_retryable()` 을 제공한다.

use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// 429 — rate limit. `retry_after` 는 서버의 `Retry-After` 헤더(초)에서 파싱.
    #[error("rate limited (HTTP 429)")]
    RateLimited { retry_after: Option<Duration> },

    /// 5xx — 서버 측 오류 (보통 재시도 가능).
    #[error("server error: HTTP {status}: {body}")]
    Server { status: u16, body: String },

    /// 그 외 비성공 응답(주로 4xx) — 보통 재시도 불가.
    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String },

    /// 네트워크/전송 계층 오류(타임아웃, 연결 실패 등). `#[from]` 로 reqwest 에러 자동 변환.
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),

    /// 응답이 프로토콜 기대와 다름(SSE 파싱 실패, 필수 필드 누락 등).
    #[error("protocol error: {0}")]
    Protocol(String),

    /// 그 외(자격증명 해석/URL 검증 실패 등). 내부에 `AuthError` 가 들어있을 수 있어
    /// 소비자는 `.downcast_ref::<AuthError>()` 로 꺼낼 수 있다.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl ClientError {
    /// 재시도할 가치가 있는 에러인가. (B: 재시도 로직이 이 값을 본다)
    /// **멱등(idempotent) 요청**(GET 류) 전용 — 재시도해도 부작용이 없는 호출에서 쓴다.
    pub fn is_retryable(&self) -> bool {
        match self {
            ClientError::RateLimited { .. } => true,
            ClientError::Server { status, .. } => (500..600).contains(status),
            // 타임아웃/연결/요청 단계 네트워크 오류는 일시적일 수 있어 재시도.
            ClientError::Network(e) => e.is_timeout() || e.is_connect() || e.is_request(),
            _ => false,
        }
    }

    /// **비멱등(non-idempotent) 요청**(부작용 있는 POST, 예: `/responses` — 툴 실행/과금)에서
    /// 재시도해도 "중복 실행" 위험이 없는 오류만 true.
    ///
    /// 핵심 기준은 "서버가 이 요청을 받아서 일했을 가능성이 있나":
    /// - `RateLimited`(429): 서버가 처리를 **거부**했다고 명시 → 재시도 안전.
    /// - `Network(is_connect)`: **연결 수립 단계** 실패 → 서버가 요청을 받지조차 못함 → 안전.
    /// - timeout / 요청 중간 끊김(`is_request`) / 5xx: 서버가 이미 받아서 처리했을 수 있음
    ///   → 재시도하면 툴 2번 실행·과금 2배 위험 → **재시도 안 함**.
    pub fn is_retryable_non_idempotent(&self) -> bool {
        match self {
            ClientError::RateLimited { .. } => true,
            ClientError::Network(e) => e.is_connect(),
            _ => false,
        }
    }

    /// 서버가 알려준 재시도 대기 시간(있으면).
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            ClientError::RateLimited { retry_after } => *retry_after,
            _ => None,
        }
    }

    /// HTTP 상태 코드(있으면) — 소비자가 데이터로 분기 가능.
    pub fn status(&self) -> Option<u16> {
        match self {
            ClientError::RateLimited { .. } => Some(429),
            ClientError::Server { status, .. } | ClientError::Http { status, .. } => Some(*status),
            _ => None,
        }
    }
}
