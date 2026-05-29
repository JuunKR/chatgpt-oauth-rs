//! HTTP client for `chatgpt.com/backend-api/codex/responses`.
//!
//! Async (tokio + reqwest). The ChatGPT backend rejects `stream: false`, so
//! all calls are SSE. `send_message` is a convenience wrapper that drives the
//! stream to completion and returns the final response Value.

use std::future::Future;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use futures_util::stream::{Stream, StreamExt};
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{Value, json};

use crate::auth::{
    CodexCredentials, MAX_ERROR_BODY, bounded_text, resolve_credentials,
    resolve_credentials_after_401, validate_token_destination,
};
use crate::error::ClientError;

/// Cap on the SSE accumulator buffer. Any single SSE event larger than 16MB
/// is treated as a protocol fault and aborts the stream.
const MAX_SSE_BUFFER: usize = 16 * 1024 * 1024;
/// 한 SSE 이벤트가 경계(빈 줄) 없이 가질 수 있는 최대 `data:` 줄 수. 빈 `data:` 줄은
/// 바이트 캡(`MAX_SSE_BUFFER`)에 거의 안 잡히면서도 `Vec<String>` 엔트리를 계속 쌓으므로,
/// 줄 수 자체를 따로 캡해 빈 줄 폭주로 인한 메모리 증가를 막는다. 실제 이벤트의 data 는
/// 보통 1줄(JSON 한 덩어리)이라 65536 은 극도로 넉넉한 상한.
const MAX_SSE_DATA_LINES: usize = 65_536;
/// 한 응답에서 누적할 수 있는 출력 텍스트(delta) 총 바이트 상한. 끝나지 않는 스트림이
/// 메모리를 소진하는 것을 막는다.
const MAX_RESPONSE_TEXT_BYTES: usize = 64 * 1024 * 1024;
/// 한 응답에서 모을 수 있는 output_item 개수 상한.
const MAX_RESPONSE_OUTPUT_ITEMS: usize = 100_000;
/// Maximum idle time between SSE chunks before we abort. Keeps a silent
/// backend from hanging the caller.
const SSE_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// `list_models` 등 옵션 없는 호출의 기본 재시도 횟수.
const DEFAULT_MAX_RETRIES: u32 = 2;
/// 서버 `Retry-After`(429) 값의 상한. 그대로 신뢰하면 악의적/버그성 429 가
/// `Retry-After: 999999` 로 태스크를 수 시간~수 일 묶어둘 수 있어, 분 단위로 캡한다.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);
/// 공유 client 의 연결(connect) 타임아웃.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// 프로세스 전역에서 재사용하는 `reqwest::Client`.
///
/// reqwest 의 Client 는 커넥션 풀과 TLS 세션을 내부에 들고 있어, 호출마다 새로 만들면
/// 풀이 무력화되고 매 요청 TLS 핸드셰이크를 다시 한다. 한 번 만들어 공유하면 같은 호스트
/// (chatgpt.com) 로의 반복 호출에서 연결을 재활용한다.
pub(crate) fn shared_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            // 빌더 실패는 TLS 백엔드 초기화 불가 등 환경 문제뿐 — 복구 불가하므로 패닉.
            .expect("failed to build shared reqwest client")
    })
}
/// 백오프 초기 지연 / 상한.
const BACKOFF_INITIAL_MS: u64 = 200;
const BACKOFF_MAX_MS: u64 = 16_000;

/// 지수 백오프 지연 계산 (attempt 1,2,3,... → 200ms, 400ms, 800ms ... 상한 16s).
/// jitter ±10% 를 더해 thundering herd 를 방지. 외부 rand 의존성 없이 시스템 시간의
/// 나노초를 엔트로피로 사용 (정밀도보다 분산이 목적).
fn backoff_delay(attempt: u32) -> Duration {
    let exp = BACKOFF_INITIAL_MS.saturating_mul(2u64.saturating_pow(attempt.saturating_sub(1)));
    let base = exp.min(BACKOFF_MAX_MS);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let frac = (nanos % 1000) as f64 / 1000.0; // 0.0..1.0
    let mult = 0.9 + 0.2 * frac; // 0.9..1.1
    Duration::from_millis((base as f64 * mult) as u64)
}

/// 비동기 작업을 재시도 가능 오류에 대해 백오프하며 반복 실행.
/// 재시도 불가 오류이거나 횟수를 다 쓰면 마지막 오류를 그대로 반환.
/// 서버가 `Retry-After`(429)를 주면 그 값을 백오프보다 우선한다.
///
/// `idempotent`: 이 작업을 재시도해도 부작용이 중복되지 않는가.
/// - `true`  → GET 류(list_models/usage). `is_retryable()` 로 폭넓게 재시도.
/// - `false` → 부작용 있는 POST(`/responses`). `is_retryable_non_idempotent()` 로
///   "서버가 받지 못한 게 확실한" 오류(연결 실패/429)만 재시도해 툴 중복 실행·과금을 막는다.
async fn with_retry<T, F, Fut>(
    max_retries: u32,
    idempotent: bool,
    mut op: F,
) -> Result<T, ClientError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ClientError>>,
{
    let mut attempt: u32 = 0;
    loop {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                attempt += 1;
                let retryable = if idempotent {
                    e.is_retryable()
                } else {
                    e.is_retryable_non_idempotent()
                };
                if attempt > max_retries || !retryable {
                    if attempt > 1 {
                        tracing::debug!(attempts = attempt, error = %e, "request failed (no more retries)");
                    }
                    return Err(e);
                }
                let delay = e.retry_after().unwrap_or_else(|| backoff_delay(attempt));
                tracing::warn!(
                    attempt,
                    delay_ms = delay.as_millis() as u64,
                    error = %e,
                    "retryable error — backing off and retrying"
                );
                tokio::time::sleep(delay).await;
            }
        }
    }
}

pub(crate) fn build_headers(creds: &CodexCredentials, stream: bool) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {}", creds.access_token))?,
    );
    headers.insert("Content-Type", HeaderValue::from_static("application/json"));
    headers.insert(
        "Accept",
        if stream {
            HeaderValue::from_static("text/event-stream")
        } else {
            HeaderValue::from_static("application/json")
        },
    );
    headers.insert(
        "User-Agent",
        HeaderValue::from_static("codex_cli_rs/0.0.0 (chatgpt-oauth)"),
    );
    headers.insert("originator", HeaderValue::from_static("codex_cli_rs"));
    // account_id comes from an *unverified* JWT claim — used only as a routing
    // hint. The backend itself authorizes via the access_token.
    if let Some(account_id) = creds.chatgpt_account_id()
        && let Ok(v) = HeaderValue::from_str(&account_id) {
            headers.insert("ChatGPT-Account-ID", v);
        }
    Ok(headers)
}

fn normalize_input(user_message: &str) -> Value {
    json!([
        {
            "role": "user",
            "content": [{"type": "input_text", "text": user_message}]
        }
    ])
}

/// `Retry-After` 헤더를 초 단위 Duration 으로 파싱 (숫자 형식만 처리).
/// 서버 값은 `MAX_RETRY_AFTER` 로 상한을 둬, 비정상적으로 큰 값이 태스크를
/// 무한정 묶어두지 못하게 한다.
fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .map(|d| d.min(MAX_RETRY_AFTER))
}

/// 비성공 HTTP 응답을 종류별 `ClientError` 로 변환. 본문은 `bounded_text` 로 최대
/// `MAX_ERROR_BODY`(8KB) 까지 읽어 그대로 보존한다(진단을 위해 자르지 않음. 읽기 단계에서
/// 이미 8KB 로 바운드되고 UTF-8 안전하게 변환됨).
/// 429 → RateLimited(+Retry-After), 5xx → Server, 그 외 → Http.
async fn http_error(resp: reqwest::Response) -> ClientError {
    let status = resp.status();
    let code = status.as_u16();
    let retry_after = parse_retry_after(resp.headers());
    let body = bounded_text(resp, MAX_ERROR_BODY).await;
    if code == 429 {
        ClientError::RateLimited { retry_after }
    } else if status.is_server_error() {
        ClientError::Server { status: code, body }
    } else {
        ClientError::Http { status: code, body }
    }
}

/// `GET /models` — list the model slugs the current account can call.
/// 재시도 가능한 오류(429/5xx/네트워크)는 백오프하며 최대 DEFAULT_MAX_RETRIES 번 더 시도.
pub async fn list_models() -> Result<Vec<Value>, ClientError> {
    with_retry(DEFAULT_MAX_RETRIES, true, || async {
        let creds = resolve_credentials(false).await?;
        list_models_once(&creds, true).await
    })
    .await
}

/// 내부 구현: 이미 해석된 creds 로 1회 호출. `refresh_on_401` 이 true 면 401 시
/// 디스크에서 토큰을 갱신해 1회 재시도한다(공개 디스크 경로). false 면 갱신하지 않고
/// 에러를 그대로 surface 한다(테스트 주입 경로 — 토큰 수명은 호출자 책임).
async fn list_models_once(
    creds: &CodexCredentials,
    refresh_on_401: bool,
) -> Result<Vec<Value>, ClientError> {
    validate_token_destination(creds)?;
    let url = format!("{}/models?client_version=1.0.0", creds.base_url);
    // 공유 client 재사용 (커넥션 풀/TLS 세션 재활용). 전체 요청 타임아웃은 per-request 로.
    let resp = shared_client()
        .get(&url)
        .timeout(Duration::from_secs(15))
        .headers(build_headers(creds, false)?)
        .send()
        .await?; // 네트워크 오류는 ClientError::Network 으로 (재시도 가능 분류)
    // On 401 (disk path only) refresh from disk and retry once, using the
    // refreshed credentials' base_url so we never re-send a fresh token to an
    // arbitrary caller URL.
    let resp = if refresh_on_401 && resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        tracing::debug!("HTTP 401 — refreshing credentials, retrying once");
        let refreshed = resolve_credentials_after_401(&creds.access_token).await?;
        validate_token_destination(&refreshed)?;
        let url2 = format!("{}/models?client_version=1.0.0", refreshed.base_url);
        shared_client()
            .get(&url2)
            .timeout(Duration::from_secs(15))
            .headers(build_headers(&refreshed, false)?)
            .send()
            .await?
    } else {
        resp
    };
    if !resp.status().is_success() {
        return Err(http_error(resp).await);
    }
    let v: Value = resp
        .json()
        .await
        .context("failed to parse /models response JSON")?;
    Ok(v.get("models")
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default())
}

/// 테스트 전용 주입 seam: 주어진 creds 를 그대로 사용(401 자동 갱신 없음).
#[cfg(test)]
async fn list_models_with_creds(creds: &CodexCredentials) -> Result<Vec<Value>, ClientError> {
    with_retry(DEFAULT_MAX_RETRIES, true, || list_models_once(creds, false)).await
}

// ──────────────────────────────────────────────────────────────────────
// 사용량 / rate-limit — `GET /backend-api/wham/usage`
//
// 실제 응답을 프로브로 확인해(2026-05) 필드명을 확정했다. 백엔드는 같은 정보를
// `/responses` 응답의 `x-codex-*` 헤더로도 주지만, 그쪽은 스트리밍 응답을 소비하기
// 전에 헤더를 캡처해야 해서 API 가 침습적이라, 여기서는 독립된 GET 엔드포인트를 쓴다.
// ──────────────────────────────────────────────────────────────────────

/// rate-limit 윈도우 하나(primary=짧은 창, secondary=긴 창).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RateWindow {
    /// 이 창에서 사용한 비율 (0~100).
    #[serde(default)]
    pub used_percent: f64,
    /// 창 길이(초).
    #[serde(default)]
    pub limit_window_seconds: u64,
    /// 리셋까지 남은 초.
    #[serde(default)]
    pub reset_after_seconds: u64,
    /// 리셋 시각 (epoch seconds).
    #[serde(default)]
    pub reset_at: i64,
}

/// rate-limit 상태.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RateLimit {
    #[serde(default)]
    pub allowed: bool,
    #[serde(default)]
    pub limit_reached: bool,
    pub primary_window: Option<RateWindow>,
    pub secondary_window: Option<RateWindow>,
}

/// `/wham/usage` 응답에서 필요한 부분(플랜 + rate-limit). 알 수 없는 필드는 무시.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Usage {
    /// 플랜 종류(예: "pro", "plus").
    pub plan_type: Option<String>,
    /// 메인 rate-limit. 응답에 없으면 None.
    pub rate_limit: Option<RateLimit>,
}

impl Usage {
    /// primary/secondary 중 더 많이 쓴 비율(0~100). 정보 없으면 None.
    /// 선제적 throttle 판단에 쓰기 좋다.
    pub fn max_used_percent(&self) -> Option<f64> {
        let rl = self.rate_limit.as_ref()?;
        let p = rl.primary_window.as_ref().map(|w| w.used_percent);
        let s = rl.secondary_window.as_ref().map(|w| w.used_percent);
        match (p, s) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }
}

/// codex base_url(`.../backend-api/codex`) → 사용량 URL(`.../backend-api/wham/usage`).
pub(crate) fn usage_url_from_base(base_url: &str) -> String {
    base_url
        .strip_suffix("/codex")
        .map(|prefix| format!("{prefix}/wham/usage"))
        .unwrap_or_else(|| base_url.replace("/backend-api/codex", "/backend-api/wham/usage"))
}

/// 현재 계정의 사용량/rate-limit 조회. 선제적 quota 모니터링용.
/// 재시도 가능한 오류는 백오프 재시도, 401 은 갱신 후 1회 재시도.
pub async fn fetch_usage() -> Result<Usage, ClientError> {
    with_retry(DEFAULT_MAX_RETRIES, true, || async {
        let creds = resolve_credentials(false).await?;
        fetch_usage_once(&creds, true).await
    })
    .await
}

async fn fetch_usage_once(
    creds: &CodexCredentials,
    refresh_on_401: bool,
) -> Result<Usage, ClientError> {
    validate_token_destination(creds)?;
    let url = usage_url_from_base(&creds.base_url);
    let resp = shared_client()
        .get(&url)
        .timeout(Duration::from_secs(15))
        .headers(build_headers(creds, false)?)
        .send()
        .await?;
    let resp = if refresh_on_401 && resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        tracing::debug!("HTTP 401 — refreshing credentials, retrying once");
        let refreshed = resolve_credentials_after_401(&creds.access_token).await?;
        validate_token_destination(&refreshed)?;
        let url2 = usage_url_from_base(&refreshed.base_url);
        shared_client()
            .get(&url2)
            .timeout(Duration::from_secs(15))
            .headers(build_headers(&refreshed, false)?)
            .send()
            .await?
    } else {
        resp
    };
    if !resp.status().is_success() {
        return Err(http_error(resp).await);
    }
    resp.json::<Usage>()
        .await
        .context("failed to parse /wham/usage JSON")
        .map_err(ClientError::from)
}

/// 테스트 전용 주입 seam: 주어진 creds 를 그대로 사용(401 자동 갱신 없음).
#[cfg(test)]
async fn fetch_usage_with_creds(creds: &CodexCredentials) -> Result<Usage, ClientError> {
    with_retry(DEFAULT_MAX_RETRIES, true, || fetch_usage_once(creds, false)).await
}

#[derive(Debug, Clone)]
pub struct SendOptions {
    pub model: String,
    pub instructions: String,
    pub reasoning_effort: Option<String>,
    /// Provide a stable key across calls in the same session to maximize
    /// prefix cache routing stickiness. If `None`, the server allocates a
    /// fresh UUID per call (less sticky).
    pub prompt_cache_key: Option<String>,
    /// Tool spec to forward to the backend, e.g.
    /// `[{"type":"web_search"},{"type":"image_generation","quality":"high"}]`.
    /// Empty -> no `tools` field is added to the request.
    pub tools: Vec<Value>,
    /// 재시도 가능한 오류(429/5xx/일시적 네트워크)에 대해 연결을 최대 몇 번 더
    /// 시도할지. 0이면 재시도 안 함. 기본 2.
    pub max_retries: u32,
    /// 스트림이 열린 뒤 청크 사이 최대 idle 허용 시간. 이 시간 동안 데이터가 없으면
    /// 멈춘 백엔드로 보고 스트림을 중단한다. 기본 `SSE_IDLE_TIMEOUT`(120s).
    pub idle_timeout: Duration,

    // ── Responses API 선택 제어 필드. 모두 None 이면 미전송(서버 기본값 사용). ──
    // 실측(2026-05)으로 이 백엔드가 실제 수용하는 것만 노출한다.
    // service_tier(flex=400/priority=무시), metadata/client_metadata(400 또는 무효과)는 제외.
    /// 툴 사용 방식: `"auto"`(기본)/`"none"`/`"required"` 문자열, 또는 특정 툴 지정 객체.
    pub tool_choice: Option<Value>,
    /// 한 턴에 여러 툴을 동시에 호출하도록 허용할지 (기본 true).
    pub parallel_tool_calls: Option<bool>,
    /// 출력 텍스트 제어 (`{"verbosity": "low|medium|high", "format": {...}}`). 기본 verbosity=medium.
    pub text: Option<Value>,
}

impl Default for SendOptions {
    fn default() -> Self {
        let model = std::env::var("CODEX_DEFAULT_MODEL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "gpt-5.3-codex".to_string());
        Self {
            model,
            instructions: "You are a helpful assistant.".to_string(),
            reasoning_effort: None,
            prompt_cache_key: None,
            tools: Vec::new(),
            max_retries: 2,
            idle_timeout: SSE_IDLE_TIMEOUT,
            tool_choice: None,
            parallel_tool_calls: None,
            text: None,
        }
    }
}

/// Open an SSE stream for a single user message.
///
/// For multi-turn conversations build an `input` array yourself and use
/// [`open_stream_with_input`].
pub async fn open_stream(
    user_message: &str,
    opts: &SendOptions,
) -> Result<impl Stream<Item = Result<Value, ClientError>>, ClientError> {
    open_stream_with_input(normalize_input(user_message), opts).await
}

/// Open an SSE stream from a fully-formed `input` array (multi-turn capable).
///
/// `input` matches the OpenAI Responses API shape:
/// ```json
/// [
///   {"role": "user",      "content": [{"type": "input_text",  "text": "..."}]},
///   {"role": "assistant", "content": [{"type": "output_text", "text": "..."}]},
///   {"role": "user",      "content": [{"type": "input_text",  "text": "..."}]}
/// ]
/// ```
///
/// 이 호출은 **비멱등**(서버에서 모델 실행·툴 호출·과금 등 부작용을 일으킴)이므로,
/// 연결 단계 오류 중에서도 "서버가 요청을 받지 못한 게 확실한" 것(연결 수립 실패, 429)만
/// `opts.max_retries` 범위에서 재시도한다. timeout·요청 중간 끊김·5xx 처럼 서버가 이미
/// 처리했을 수 있는 오류는 재시도하지 않고 그대로 surface 한다 — 재시도하면 툴이 두 번
/// 실행되고 과금이 중복될 수 있기 때문. 스트림이 열린 뒤의 오류도 스트림 항목으로만 surface.
pub async fn open_stream_with_input(
    input: Value,
    opts: &SendOptions,
) -> Result<impl Stream<Item = Result<Value, ClientError>>, ClientError> {
    // 재시도마다 body 를 다시 보내야 하므로 input 을 매 시도 clone. idempotent=false.
    with_retry(opts.max_retries, false, || async {
        let creds = resolve_credentials(false).await?;
        open_stream_with_input_once(input.clone(), opts, &creds, true).await
    })
    .await
}

async fn open_stream_with_input_once(
    input: Value,
    opts: &SendOptions,
    creds: &CodexCredentials,
    refresh_on_401: bool,
) -> Result<impl Stream<Item = Result<Value, ClientError>> + use<>, ClientError> {
    // `use<>`: 반환 스트림은 resp.bytes_stream() 을 소유할 뿐 creds/opts 를 빌리지 않는다.
    // (Rust 2024 RPIT 기본 캡처를 끄지 않으면 호출부의 지역 creds 수명에 묶여 컴파일 실패.)
    validate_token_destination(creds)?;

    let body = build_request_body(input, opts);

    // 공유 client 재사용. 스트리밍 응답이므로 전체 요청 타임아웃은 적용하지 않는다
    // (긴 응답을 중간에 잘라버림). 연결 타임아웃은 공유 client 에 고정 설정되어 있고,
    // per-chunk idle 은 SSE_IDLE_TIMEOUT 으로 별도 감시한다.
    let url = format!("{}/responses", creds.base_url);
    let resp = shared_client()
        .post(&url)
        .headers(build_headers(creds, true)?)
        .json(&body)
        .send()
        .await?;

    // On 401 (disk path only) refresh from disk and retry once. The retry URL is
    // rebuilt from the *refreshed* credentials' base_url so a caller-supplied
    // untrusted base_url cannot capture a fresh token on retry.
    let resp = if refresh_on_401 && resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        tracing::debug!("HTTP 401 — refreshing credentials, retrying once");
        let refreshed = resolve_credentials_after_401(&creds.access_token).await?;
        validate_token_destination(&refreshed)?;
        let url2 = format!("{}/responses", refreshed.base_url);
        shared_client()
            .post(&url2)
            .headers(build_headers(&refreshed, true)?)
            .json(&body)
            .send()
            .await?
    } else {
        resp
    };

    if !resp.status().is_success() {
        return Err(http_error(resp).await);
    }

    // 스트림을 파싱하기 전에 content-type 을 본다. 백엔드가 200 을 주면서 HTML/JSON
    // 에러 바디(게이트웨이 페이지, JSON 에러)를 흘리면 여기서 본문과 함께 명확히 잡는다.
    //
    // 단, "헤더 없음"은 거부하지 않는다. 실제 ChatGPT 백엔드는 정상 SSE 를 흘리면서도
    // Content-Type 헤더를 아예 안 주는 경우가 있다(헤더 없음 ≠ 에러). 헤더가 없으면
    // 통과시키고, 진짜 SSE 가 아니면 아래 sse 파서가 구체적 오류로 잡는다. 명시적으로
    // text/event-stream 이 아닌 "다른 타입"이 붙은 경우만 에러 바디로 보고 즉시 거부한다.
    let ct_header = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_ascii_lowercase());
    let mismatched = matches!(&ct_header, Some(ct) if !ct.contains("text/event-stream"));
    if mismatched {
        let ct = ct_header.unwrap_or_default();
        let body = bounded_text(resp, MAX_ERROR_BODY).await;
        return Err(ClientError::Protocol(format!(
            "expected text/event-stream from /responses but got content-type `{ct}`: {body}"
        )));
    }

    Ok(sse_event_stream(resp.bytes_stream(), opts.idle_timeout))
}

fn build_request_body(input: Value, opts: &SendOptions) -> Value {
    let mut body_map = serde_json::Map::new();
    body_map.insert("model".into(), json!(opts.model));
    body_map.insert("instructions".into(), json!(opts.instructions));
    body_map.insert("input".into(), input);
    body_map.insert("store".into(), json!(false));
    body_map.insert("stream".into(), json!(true));
    if let Some(eff) = &opts.reasoning_effort {
        body_map.insert("reasoning".into(), json!({ "effort": eff }));
        // 추론을 쓸 때만 암호화된 추론 콘텐츠를 응답에 포함하도록 요청한다.
        // 멀티턴에서 이전 턴의 추론을 (서버 저장 없이) 이어줄 수 있다 — codex-rs 와 동일.
        body_map.insert(
            "include".into(),
            json!(["reasoning.encrypted_content"]),
        );
    }
    if let Some(key) = &opts.prompt_cache_key {
        body_map.insert("prompt_cache_key".into(), json!(key));
    }
    if !opts.tools.is_empty() {
        body_map.insert("tools".into(), Value::Array(opts.tools.clone()));
    }
    // 선택 제어 필드 — 설정된 것만 그대로 전달. (실측으로 백엔드가 수용하는 것만)
    if let Some(tc) = &opts.tool_choice {
        body_map.insert("tool_choice".into(), tc.clone());
    }
    if let Some(p) = opts.parallel_tool_calls {
        body_map.insert("parallel_tool_calls".into(), json!(p));
    }
    if let Some(t) = &opts.text {
        body_map.insert("text".into(), t.clone());
    }
    Value::Object(body_map)
}

/// Byte stream -> SSE event JSON adapter.
///
/// Conformance points:
/// 1. Buffer raw bytes and only decode UTF-8 at line boundaries — never
///    splits a multibyte character across chunks.
/// 2. Multi-line `data:` fields within an event are joined with `\n` and
///    parsed as a single JSON document (per the SSE spec).
/// 3. EOF without a trailing newline still flushes the final event.
/// 4. The accumulator is capped at MAX_SSE_BUFFER; overflow returns Err.
/// 5. Malformed `data:` JSON is surfaced as Err — never silently dropped,
///    so a truncated `response.failed` cannot be lost.
/// 6. Per-chunk idle timeout to avoid hanging on a silent backend.
/// 7. The accumulated `data:` payload for a single event is capped at
///    MAX_SSE_BUFFER AND at MAX_SSE_DATA_LINES lines, so a server that sends
///    endless `data:` lines (even empty ones) without an event boundary cannot
///    grow memory unbounded.
///
/// Stream items are `ClientError` so callers get one error type across the whole
/// operation (connection setup AND streaming), with `status()/retry_after()` intact.
fn sse_event_stream<S>(
    byte_stream: S,
    idle_timeout: Duration,
) -> impl Stream<Item = Result<Value, ClientError>>
where
    S: Stream<Item = reqwest::Result<bytes::Bytes>> + Unpin,
{
    use futures_util::stream;

    struct State<S> {
        stream: S,
        buf: Vec<u8>,
        data_lines: Vec<String>,
        data_bytes: usize, // 현재 이벤트의 누적 data 바이트 수 (상한 검사용)
        eof: bool,
        finished: bool,
        idle_timeout: Duration,
    }

    let init = State {
        stream: byte_stream,
        buf: Vec::new(),
        data_lines: Vec::new(),
        data_bytes: 0,
        eof: false,
        finished: false,
        idle_timeout,
    };

    stream::unfold(init, |mut st| async move {
        if st.finished {
            return None;
        }

        loop {
            // 1) Try to extract a full line from the buffer.
            if let Some(idx) = st.buf.iter().position(|b| *b == b'\n') {
                let line_bytes: Vec<u8> = st.buf.drain(..=idx).collect();
                let mut end = line_bytes.len().saturating_sub(1); // skip \n
                if end > 0 && line_bytes[end - 1] == b'\r' {
                    end -= 1;
                }
                let line_str = match std::str::from_utf8(&line_bytes[..end]) {
                    Ok(s) => s.to_string(),
                    Err(e) => {
                        st.finished = true;
                        return Some((
                            Err(ClientError::Protocol(format!("SSE line is not valid UTF-8: {e}"))),
                            st,
                        ));
                    }
                };

                // Empty line -> event boundary.
                if line_str.is_empty() {
                    st.data_bytes = 0; // 이벤트 경계 — 누적 카운터 리셋
                    if let Some(ev) = take_event(&mut st.data_lines) {
                        match ev {
                            Ok(Some(v)) => return Some((Ok(v), st)),
                            Ok(None) => {
                                // [DONE] — returning None ends the unfold.
                                return None;
                            }
                            Err(e) => {
                                st.finished = true;
                                return Some((Err(e), st));
                            }
                        }
                    }
                    continue;
                }

                // Comment line — ignore.
                if line_str.starts_with(':') {
                    continue;
                }

                if let Some(rest) = line_str.strip_prefix("data:") {
                    let v = rest.strip_prefix(' ').unwrap_or(rest);
                    // 이벤트 경계 없이 data 만 무한히 쌓이는 것을 막는다(메모리 보호).
                    // 바이트뿐 아니라 줄 수도 센다: 빈 `data:` 줄은 v.len()==0 이라 바이트
                    // 캡엔 안 걸리지만 String 엔트리는 계속 늘어 메모리가 샌다. +1 은 join 시
                    // 들어갈 개행 한 바이트 몫.
                    st.data_bytes = st.data_bytes.saturating_add(v.len().saturating_add(1));
                    if st.data_bytes > MAX_SSE_BUFFER || st.data_lines.len() >= MAX_SSE_DATA_LINES {
                        st.finished = true;
                        return Some((
                            Err(ClientError::Protocol(format!(
                                "SSE event exceeded data cap ({}MB / {} lines) without an event boundary — protocol fault",
                                MAX_SSE_BUFFER / (1024 * 1024),
                                MAX_SSE_DATA_LINES
                            ))),
                            st,
                        ));
                    }
                    st.data_lines.push(v.to_string());
                }
                // Other fields (event:, id:, retry:) are not used here.
                continue;
            }

            // 2) Buffer has no complete line. If EOF, flush whatever's left.
            if st.eof {
                if !st.buf.is_empty() {
                    let line_bytes = std::mem::take(&mut st.buf);
                    let line_str = match std::str::from_utf8(&line_bytes) {
                        Ok(s) => s.trim_end_matches('\r').to_string(),
                        Err(e) => {
                            st.finished = true;
                            return Some((
                                Err(ClientError::Protocol(format!(
                                    "trailing SSE line is not valid UTF-8: {e}"
                                ))),
                                st,
                            ));
                        }
                    };
                    if !line_str.is_empty() && !line_str.starts_with(':')
                        && let Some(rest) = line_str.strip_prefix("data:") {
                            let v = rest.strip_prefix(' ').unwrap_or(rest);
                            st.data_lines.push(v.to_string());
                        }
                }
                st.finished = true;
                st.data_bytes = 0;
                if let Some(ev) = take_event(&mut st.data_lines) {
                    return match ev {
                        Ok(Some(v)) => Some((Ok(v), st)),
                        Ok(None) => None,
                        Err(e) => Some((Err(e), st)),
                    };
                }
                return None;
            }

            // 3) Pull the next chunk, with an idle timeout.
            let next = tokio::time::timeout(st.idle_timeout, st.stream.next()).await;
            match next {
                Err(_elapsed) => {
                    st.finished = true;
                    return Some((
                        Err(ClientError::Protocol(format!(
                            "Codex SSE idle timeout ({}s) — backend stopped sending data",
                            st.idle_timeout.as_secs()
                        ))),
                        st,
                    ));
                }
                Ok(None) => {
                    st.eof = true;
                    continue;
                }
                Ok(Some(Err(e))) => {
                    st.finished = true;
                    // 청크 수신 중 전송계층 오류 — Network 으로 보존(분류/메시지 유지).
                    return Some((Err(ClientError::Network(e)), st));
                }
                Ok(Some(Ok(chunk))) => {
                    if st.buf.len().saturating_add(chunk.len()) > MAX_SSE_BUFFER {
                        st.finished = true;
                        return Some((
                            Err(ClientError::Protocol(format!(
                                "SSE buffer cap ({}MB) exceeded — protocol fault",
                                MAX_SSE_BUFFER / (1024 * 1024)
                            ))),
                            st,
                        ));
                    }
                    st.buf.extend_from_slice(&chunk);
                }
            }
        }
    })
}

/// Consume the accumulated `data:` lines for the current event.
/// `Ok(Some(v))` — a parsed JSON event. `Ok(None)` — [DONE]. `Err(_)` — parse fault.
fn take_event(data_lines: &mut Vec<String>) -> Option<Result<Option<Value>, ClientError>> {
    if data_lines.is_empty() {
        return None;
    }
    let payload = std::mem::take(data_lines).join("\n");
    let trimmed = payload.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed == "[DONE]" {
        return Some(Ok(None));
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(v) => Some(Ok(Some(v))),
        // 파싱 실패 시 payload 원문을 메시지에 넣지 않는다 — 에러가 로그로 흘러가면
        // 프롬프트/출력 조각이 함께 새어나갈 수 있다. 길이만 보고한다.
        Err(e) => Some(Err(ClientError::Protocol(format!(
            "failed to parse SSE data payload as JSON: {e} ({} bytes withheld)",
            trimmed.len()
        )))),
    }
}

/// Send a single user message and return the final response dict, combining
/// deltas internally.
pub async fn send_message(user_message: &str, opts: &SendOptions) -> Result<Value, ClientError> {
    let stream = open_stream(user_message, opts).await?;
    drive_stream_to_response(stream).await
}

/// 테스트 전용 주입 seam: 주어진 creds 를 그대로 사용(401 자동 갱신 없음).
#[cfg(test)]
async fn send_message_with_creds(
    user_message: &str,
    opts: &SendOptions,
    creds: &CodexCredentials,
) -> Result<Value, ClientError> {
    let input = normalize_input(user_message);
    let stream = with_retry(opts.max_retries, false, || {
        open_stream_with_input_once(input.clone(), opts, creds, false)
    })
    .await?;
    drive_stream_to_response(stream).await
}

/// 열린 SSE 스트림을 끝까지 소비해 최종 response 객체로 합성한다. 델타/아이템을 모으고
/// 터미널 상태(failed/incomplete 등)를 에러로 surface 한다.
async fn drive_stream_to_response(
    stream: impl Stream<Item = Result<Value, ClientError>>,
) -> Result<Value, ClientError> {
    let mut stream = Box::pin(stream);
    let mut final_response: Option<Value> = None;
    let mut text_deltas: Vec<String> = Vec::new();
    let mut text_total: usize = 0; // text_deltas 누적 바이트 (상한 검사용)
    let mut output_items: Vec<Value> = Vec::new();

    while let Some(ev) = stream.next().await {
        let ev = ev?;
        let et = ev.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match et {
            "response.completed" => {
                if let Some(r) = ev.get("response").cloned() {
                    final_response = Some(r);
                }
            }
            "response.failed" => {
                let err = ev
                    .get("response")
                    .and_then(|r| r.get("error"))
                    .or_else(|| ev.get("error"))
                    .cloned()
                    .unwrap_or(Value::Null);
                return Err(ClientError::Protocol(format!("Codex response failed: {err}")));
            }
            // 터미널 incomplete 이벤트 — 성공으로 합성되지 않도록 명시적으로 잡는다.
            "response.incomplete" => {
                let detail = ev
                    .get("response")
                    .and_then(|r| r.get("incomplete_details"))
                    .or_else(|| ev.get("incomplete_details"))
                    .cloned()
                    .unwrap_or(Value::Null);
                return Err(ClientError::Protocol(format!(
                    "Codex response incomplete: {detail}"
                )));
            }
            "response.output_text.delta" => {
                if let Some(delta) = ev.get("delta").and_then(|d| d.as_str()) {
                    // 끝나지 않는 스트림이 메모리를 소진하지 않도록 누적 텍스트를 캡한다.
                    text_total = text_total.saturating_add(delta.len());
                    if text_total > MAX_RESPONSE_TEXT_BYTES {
                        return Err(ClientError::Protocol(format!(
                            "Codex response exceeded max accumulated text ({}MB) — aborting",
                            MAX_RESPONSE_TEXT_BYTES / (1024 * 1024)
                        )));
                    }
                    text_deltas.push(delta.to_string());
                }
            }
            "response.output_item.done" => {
                if let Some(item) = ev.get("item").cloned() {
                    // output_item 개수도 캡 — 무한 스트림 방어.
                    if output_items.len() >= MAX_RESPONSE_OUTPUT_ITEMS {
                        return Err(ClientError::Protocol(format!(
                            "Codex response exceeded max output items ({}) — aborting",
                            MAX_RESPONSE_OUTPUT_ITEMS
                        )));
                    }
                    output_items.push(item);
                }
            }
            _ => {}
        }
    }

    // If `response.completed` never arrived but we did collect deltas/items,
    // synthesize a response (tolerates event-name changes or omissions).
    let mut response = match final_response {
        Some(r) => r,
        None => {
            if text_deltas.is_empty() && output_items.is_empty() {
                return Err(ClientError::Protocol(
                    "Codex stream ended without response.completed or any deltas".into(),
                ));
            }
            json!({ "output": [] })
        }
    };

    // Refuse anything that's not an object — would otherwise panic on indexing.
    if !response.is_object() {
        return Err(ClientError::Protocol(format!(
            "Codex response is not an object: {response}"
        )));
    }

    // 터미널 상태 검증: `response.completed` 이벤트를 받았더라도 그 response 객체의
    // status 가 실패/미완을 가리키거나 error 가 박혀 있을 수 있다. 그대로 성공 반환하면
    // 호출자에게 "완료된 빈/오류 응답"이 조용히 흘러간다(데이터 손실). 여기서 잡는다.
    if let Some(status) = response.get("status").and_then(|s| s.as_str())
        && matches!(status, "failed" | "cancelled" | "incomplete" | "expired")
    {
        let detail = response
            .get("error")
            .filter(|e| !e.is_null())
            .or_else(|| response.get("incomplete_details"))
            .cloned()
            .unwrap_or(Value::Null);
        return Err(ClientError::Protocol(format!(
            "Codex response terminal status `{status}`: {detail}"
        )));
    }
    // status 가 없어도 error 가 비어있지 않으면 실패로 본다.
    if let Some(err) = response.get("error")
        && !err.is_null()
    {
        return Err(ClientError::Protocol(format!(
            "Codex response carried an error: {err}"
        )));
    }

    let output_empty = response
        .get("output")
        .and_then(|o| o.as_array())
        .is_none_or(|a| a.is_empty());
    if output_empty {
        let obj = response.as_object_mut().expect("checked is_object above");
        if !output_items.is_empty() {
            obj.insert("output".into(), Value::Array(output_items));
        } else if !text_deltas.is_empty() {
            obj.insert(
                "output".into(),
                json!([
                    {
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": text_deltas.join("")}]
                    }
                ]),
            );
        }
    }
    Ok(response)
}

/// Pull the concatenated assistant text out of a response dict.
pub fn extract_text(response: &Value) -> String {
    let Some(items) = response.get("output").and_then(|o| o.as_array()) else {
        return String::new();
    };
    let mut out = String::new();
    for item in items {
        if item.get("type").and_then(|t| t.as_str()) != Some("message") {
            continue;
        }
        let Some(contents) = item.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for c in contents {
            if c.get("type").and_then(|t| t.as_str()) == Some("output_text")
                && let Some(t) = c.get("text").and_then(|t| t.as_str()) {
                    out.push_str(t);
                }
        }
    }
    out
}

// ──────────────────────────────────────────────────────────────────────
// 테스트 — 순수 함수 + SSE 파서. 네트워크 불필요 (가짜 바이트 스트림 주입).
// ──────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    // ── 순수 함수들 ──

    #[test]
    fn normalize_input_shape() {
        let v = normalize_input("안녕");
        // [{"role":"user","content":[{"type":"input_text","text":"안녕"}]}]
        let first = &v[0];
        assert_eq!(first["role"], "user");
        assert_eq!(first["content"][0]["type"], "input_text");
        assert_eq!(first["content"][0]["text"], "안녕");
    }

    #[test]
    fn retry_after_is_capped() {
        use reqwest::header::{HeaderValue, RETRY_AFTER};
        // [P2] 회귀 가드: 비정상적으로 큰 Retry-After 는 상한(MAX_RETRY_AFTER)으로 잘린다.
        let mut huge = HeaderMap::new();
        huge.insert(RETRY_AFTER, HeaderValue::from_static("999999"));
        assert_eq!(parse_retry_after(&huge), Some(MAX_RETRY_AFTER));
        // 상한 이하의 값은 그대로 통과.
        let mut small = HeaderMap::new();
        small.insert(RETRY_AFTER, HeaderValue::from_static("5"));
        assert_eq!(parse_retry_after(&small), Some(Duration::from_secs(5)));
        // 헤더가 없으면 None.
        assert_eq!(parse_retry_after(&HeaderMap::new()), None);
        // 숫자가 아니면(HTTP-date 형식 등) None.
        let mut date = HeaderMap::new();
        date.insert(RETRY_AFTER, HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"));
        assert_eq!(parse_retry_after(&date), None);
    }

    #[test]
    fn build_request_body_required_fields() {
        // ..default() 로 새 필드 추가에도 깨지지 않게.
        let opts = SendOptions {
            model: "gpt-5.3-codex".into(),
            instructions: "sys".into(),
            ..SendOptions::default()
        };
        let body = build_request_body(normalize_input("hi"), &opts);
        assert_eq!(body["model"], "gpt-5.3-codex");
        assert_eq!(body["instructions"], "sys");
        assert_eq!(body["stream"], true);  // ChatGPT 백엔드는 항상 스트림
        assert_eq!(body["store"], false);
        assert!(body.get("input").is_some());
        // 옵션 미설정이면 선택 필드들은 모두 없어야 함
        for k in [
            "reasoning", "include", "prompt_cache_key", "tools",
            "tool_choice", "parallel_tool_calls", "text",
        ] {
            assert!(body.get(k).is_none(), "{k} 키가 없어야 함");
        }
    }

    #[test]
    fn build_request_body_optional_control_fields() {
        let opts = SendOptions {
            tool_choice: Some(json!("required")),
            parallel_tool_calls: Some(false),
            text: Some(json!({ "verbosity": "low" })),
            ..SendOptions::default()
        };
        let body = build_request_body(normalize_input("hi"), &opts);
        assert_eq!(body["tool_choice"], "required");
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(body["text"]["verbosity"], "low");
    }

    #[test]
    fn build_request_body_adds_fields_when_set() {
        let opts = SendOptions {
            reasoning_effort: Some("high".into()),
            prompt_cache_key: Some("k1".into()),
            tools: vec![json!({"type": "web_search"})],
            ..SendOptions::default()
        };
        let body = build_request_body(normalize_input("hi"), &opts);
        assert_eq!(body["reasoning"]["effort"], "high");
        // reasoning 을 쓰면 include 에 암호화 추론 콘텐츠 요청이 들어가야 함 (G)
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
        assert_eq!(body["prompt_cache_key"], "k1");
        assert_eq!(body["tools"][0]["type"], "web_search");
    }

    #[test]
    fn extract_text_joins_message_output_text() {
        let resp = json!({
            "output": [
                { "type": "reasoning", "content": [{"type": "output_text", "text": "무시됨"}] },
                { "type": "message", "role": "assistant",
                  "content": [
                      {"type": "output_text", "text": "안녕"},
                      {"type": "output_text", "text": "하세요"}
                  ]
                }
            ]
        });
        assert_eq!(extract_text(&resp), "안녕하세요");
    }

    #[test]
    fn extract_text_empty_when_no_output() {
        assert_eq!(extract_text(&json!({})), "");
    }

    // ── SSE 파서 ──

    /// 가짜 바이트 청크들을 sse_event_stream 에 흘려넣고 이벤트를 모은다.
    async fn run_sse(chunks: Vec<bytes::Bytes>) -> Vec<Result<Value, ClientError>> {
        use futures_util::stream;
        // 아이템 타입을 reqwest::Result<Bytes> 로 맞춤 (Ok 만 생성하므로 Err 구성 불필요).
        let items: Vec<reqwest::Result<bytes::Bytes>> = chunks.into_iter().map(Ok).collect();
        let byte_stream = stream::iter(items);
        let mut s = Box::pin(sse_event_stream(byte_stream, Duration::from_secs(5)));
        let mut out = Vec::new();
        while let Some(ev) = s.next().await {
            out.push(ev);
        }
        out
    }

    #[tokio::test]
    async fn sse_single_event() {
        let events = run_sse(vec![bytes::Bytes::from("data: {\"type\":\"x\",\"n\":1}\n\n")]).await;
        assert_eq!(events.len(), 1);
        let v = events[0].as_ref().unwrap();
        assert_eq!(v["type"], "x");
        assert_eq!(v["n"], 1);
    }

    #[tokio::test]
    async fn sse_done_ends_stream() {
        // 이벤트 1개 후 [DONE] → [DONE] 이후로는 이벤트가 없어야 함.
        let events = run_sse(vec![bytes::Bytes::from(
            "data: {\"type\":\"a\"}\n\ndata: [DONE]\n\ndata: {\"type\":\"b\"}\n\n",
        )])
        .await;
        assert_eq!(events.len(), 1); // a 만, b 는 [DONE] 이후라 안 옴
        assert_eq!(events[0].as_ref().unwrap()["type"], "a");
    }

    #[tokio::test]
    async fn sse_multiline_data_joined() {
        // SSE 스펙: 같은 이벤트의 여러 data: 줄은 \n 으로 join 후 JSON 파싱.
        let events = run_sse(vec![bytes::Bytes::from("data: {\"type\":\ndata: \"x\"}\n\n")]).await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].as_ref().unwrap()["type"], "x");
    }

    #[tokio::test]
    async fn sse_chunk_split_multibyte_safe() {
        // "data: {\"k\":\"한\"}\n\n" 을 한글 '한'(3바이트) 중간에서 두 청크로 분리.
        let full = "data: {\"k\":\"한\"}\n\n".as_bytes().to_vec();
        let mid = 13; // `data: {"k":"` 가 12바이트, 13은 '한' 바이트 중간
        let c1 = bytes::Bytes::copy_from_slice(&full[..mid]);
        let c2 = bytes::Bytes::copy_from_slice(&full[mid..]);
        let events = run_sse(vec![c1, c2]).await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].as_ref().unwrap()["k"], "한");
    }

    #[tokio::test]
    async fn sse_malformed_json_surfaces_error() {
        // 잘못된 JSON 은 조용히 버리지 않고 Err 로 나와야 함.
        let events = run_sse(vec![bytes::Bytes::from("data: {not json}\n\n")]).await;
        assert_eq!(events.len(), 1);
        assert!(events[0].is_err());
    }

    #[tokio::test]
    async fn sse_empty_data_line_flood_is_capped() {
        // 이벤트 경계(빈 줄) 없이 빈 `data:` 줄만 쏟아지면 메모리가 새지 않도록 캡에 걸려야 함.
        // 바이트는 거의 0 이지만 줄 수 캡(MAX_SSE_DATA_LINES)이 막는다.
        let flood = "data:\n".repeat(MAX_SSE_DATA_LINES + 10);
        let events = run_sse(vec![bytes::Bytes::from(flood)]).await;
        let last = events.last().expect("should yield at least the cap error");
        assert!(last.is_err(), "empty-data-line flood must surface an error");
        let msg = last.as_ref().unwrap_err().to_string();
        assert!(
            msg.contains("lines") || msg.contains("data cap"),
            "expected data-cap error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn sse_flush_on_eof_without_newline() {
        // 끝에 빈 줄(\n\n) 없이 EOF 가 와도 마지막 이벤트를 흘려야 함.
        let events = run_sse(vec![bytes::Bytes::from("data: {\"type\":\"last\"}")]).await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].as_ref().unwrap()["type"], "last");
    }

    // ── 재시도 / 백오프 (B) ──

    #[test]
    fn backoff_exponential_capped() {
        // jitter ±10% 안에서 200·400·800ms ... 상한 16s.
        let in_range = |d: Duration, base_ms: u64| {
            let ms = d.as_millis() as u64;
            ms >= (base_ms as f64 * 0.9) as u64 && ms <= (base_ms as f64 * 1.1) as u64
        };
        assert!(in_range(backoff_delay(1), 200));
        assert!(in_range(backoff_delay(2), 400));
        assert!(in_range(backoff_delay(3), 800));
        // 아주 큰 attempt 도 상한(16s)을 넘지 않음 (+10% 여유).
        assert!(backoff_delay(30).as_millis() as u64 <= (BACKOFF_MAX_MS as f64 * 1.1) as u64);
    }

    #[tokio::test]
    async fn with_retry_retries_then_succeeds() {
        use std::cell::Cell;
        let calls = Cell::new(0u32);
        // 처음 2번은 429, 3번째 성공.
        let r: Result<u8, ClientError> = with_retry(5, true, || {
            let n = calls.get() + 1;
            calls.set(n);
            async move {
                if n < 3 {
                    Err(ClientError::RateLimited { retry_after: Some(Duration::from_millis(1)) })
                } else {
                    Ok(42u8)
                }
            }
        })
        .await;
        assert_eq!(r.unwrap(), 42);
        assert_eq!(calls.get(), 3); // 2번 실패 + 1번 성공
    }

    #[tokio::test]
    async fn with_retry_non_retryable_immediate() {
        use std::cell::Cell;
        let calls = Cell::new(0u32);
        let r: Result<u8, ClientError> = with_retry(5, true, || {
            calls.set(calls.get() + 1);
            async { Err(ClientError::Http { status: 400, body: "bad".into() }) }
        })
        .await;
        assert!(r.is_err());
        assert_eq!(calls.get(), 1); // 400 은 재시도 안 함 → 한 번만 호출
    }

    #[tokio::test]
    async fn with_retry_non_idempotent_skips_ambiguous_errors() {
        use std::cell::Cell;
        // 비멱등 모드: 5xx 는 서버가 이미 처리했을 수 있어 재시도하지 않는다(중복 실행 방지).
        let calls = Cell::new(0u32);
        let r: Result<u8, ClientError> = with_retry(5, false, || {
            calls.set(calls.get() + 1);
            async { Err(ClientError::Server { status: 503, body: "x".into() }) }
        })
        .await;
        assert!(r.is_err());
        assert_eq!(calls.get(), 1, "5xx must NOT be retried for non-idempotent ops");

        // 비멱등 모드라도 429 는 서버가 처리를 거부한 것이므로 재시도 안전.
        let calls2 = Cell::new(0u32);
        let r2: Result<u8, ClientError> = with_retry(3, false, || {
            let n = calls2.get() + 1;
            calls2.set(n);
            async move {
                if n < 2 {
                    Err(ClientError::RateLimited { retry_after: Some(Duration::from_millis(1)) })
                } else {
                    Ok(7u8)
                }
            }
        })
        .await;
        assert_eq!(r2.unwrap(), 7);
        assert_eq!(calls2.get(), 2, "429 should still be retried for non-idempotent ops");
    }

    #[test]
    fn usage_json_parsing() {
        // 실제 /wham/usage 응답(프로브로 확인)의 핵심 부분을 파싱.
        let raw = r#"{
            "plan_type": "pro",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {
                    "used_percent": 12.5,
                    "limit_window_seconds": 18000,
                    "reset_after_seconds": 1700,
                    "reset_at": 1780034186
                },
                "secondary_window": {
                    "used_percent": 40.0,
                    "limit_window_seconds": 604800,
                    "reset_after_seconds": 357129,
                    "reset_at": 1780389606
                }
            },
            "credits": { "balance": "0" }
        }"#;
        let u: Usage = serde_json::from_str(raw).unwrap();
        assert_eq!(u.plan_type.as_deref(), Some("pro"));
        let rl = u.rate_limit.as_ref().unwrap();
        assert!(rl.allowed && !rl.limit_reached);
        assert_eq!(rl.primary_window.as_ref().unwrap().used_percent, 12.5);
        assert_eq!(rl.secondary_window.as_ref().unwrap().reset_at, 1780389606);
        // primary(12.5)와 secondary(40.0) 중 큰 값.
        assert_eq!(u.max_used_percent(), Some(40.0));
    }

    #[test]
    fn usage_url_derivation() {
        assert_eq!(
            usage_url_from_base("https://chatgpt.com/backend-api/codex"),
            "https://chatgpt.com/backend-api/wham/usage"
        );
    }

    #[test]
    fn usage_parses_without_rate_limit() {
        // rate_limit 필드가 없는 응답도 안전하게 파싱(None).
        let u: Usage = serde_json::from_str(r#"{"plan_type":"plus"}"#).unwrap();
        assert_eq!(u.plan_type.as_deref(), Some("plus"));
        assert!(u.rate_limit.is_none());
        assert_eq!(u.max_used_percent(), None);
    }

    #[tokio::test]
    async fn with_retry_returns_last_error_when_exhausted() {
        use std::cell::Cell;
        let calls = Cell::new(0u32);
        let r: Result<u8, ClientError> = with_retry(2, true, || {
            calls.set(calls.get() + 1);
            async { Err(ClientError::Server { status: 503, body: "x".into() }) }
        })
        .await;
        assert!(matches!(r, Err(ClientError::Server { status: 503, .. })));
        assert_eq!(calls.get(), 3); // 최초 1 + 재시도 2 = 3
    }
}

// ──────────────────────────────────────────────────────────────────────
// 통합 테스트 — mock HTTP 서버(wiremock)로 네트워크 경로를 검증한다.
//
// `~/.codex/auth.json` 디스크 경로는 공개 API 에 base_url 주입 통로가 없으므로,
// 여기서는 크레이트 내부 전용(test-only) seam (`*_with_creds`)으로 mock 서버를 가리키는
// 가짜 creds 를 주입한다. 이 seam 은 `#[cfg(test)]` 라 배포 바이너리엔 존재하지 않고,
// 외부 사용자에게도 보이지 않는다.
// ──────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod http_tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// mock 서버를 가리키는 가짜 자격증명. base_url 은 실제와 같은 `/backend-api/codex` 모양으로
    /// 만들어 URL 파생(예: /wham/usage)이 올바르게 동작하게 한다.
    fn fake_creds(server_uri: &str) -> CodexCredentials {
        // http://127.0.0.1 을 허용하려면 base-url 신뢰 검사를 끈다. (edition 2024: set_var 는 unsafe)
        unsafe {
            std::env::set_var("CODEX_ALLOW_INSECURE_BASE_URL", "1");
        }
        CodexCredentials {
            access_token: "test-access".into(),
            refresh_token: "test-refresh".into(),
            base_url: format!("{server_uri}/backend-api/codex"),
            last_refresh: None,
            from_disk: false, // 테스트 주입 토큰 — insecure 우회 전면 적용 대상
        }
    }

    #[tokio::test]
    async fn list_models_success_parsing() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/backend-api/codex/models"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"models":[{"slug":"gpt-5.3-codex"}]})),
            )
            .mount(&server)
            .await;

        let c = fake_creds(&server.uri());
        let models = list_models_with_creds(&c).await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["slug"], "gpt-5.3-codex");
    }

    #[tokio::test]
    async fn list_models_429_retry_then_success() {
        let server = MockServer::start().await;
        // 첫 1회 429(우선순위 높음, 1회 소진) → 이후 200.
        Mock::given(method("GET"))
            .and(path("/backend-api/codex/models"))
            .respond_with(ResponseTemplate::new(429))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/backend-api/codex/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models":[{"slug":"x"}]})))
            .mount(&server)
            .await;

        let c = fake_creds(&server.uri());
        let models = list_models_with_creds(&c).await.unwrap();
        assert_eq!(models.len(), 1); // 429 한 번 맞고 재시도해서 성공
    }

    #[tokio::test]
    async fn list_models_500_retry_then_success() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/backend-api/codex/models"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/backend-api/codex/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models":[]})))
            .mount(&server)
            .await;

        let c = fake_creds(&server.uri());
        assert!(list_models_with_creds(&c).await.is_ok());
    }

    #[tokio::test]
    async fn list_models_400_no_retry() {
        let server = MockServer::start().await;
        // expect(1): 정확히 1회만 호출되어야 함(재시도 없음). 어기면 drop 시 panic.
        Mock::given(method("GET"))
            .and(path("/backend-api/codex/models"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
            .expect(1)
            .mount(&server)
            .await;

        let c = fake_creds(&server.uri());
        let err = list_models_with_creds(&c).await.unwrap_err();
        assert_eq!(err.status(), Some(400));
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn send_message_sse_parsing() {
        let server = MockServer::start().await;
        let sse = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello\"}\n\n\
data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hello\"}]}]}}\n\n\
data: [DONE]\n\n";
        Mock::given(method("POST"))
            .and(path("/backend-api/codex/responses"))
            .respond_with(
                // set_body_raw(bytes, mime) 로 content-type 을 명시 지정한다. set_body_string 은
                // content-type 을 text/plain 으로 고정해버려 SSE 검사를 통과 못 한다.
                ResponseTemplate::new(200).set_body_raw(sse.as_bytes(), "text/event-stream"),
            )
            .mount(&server)
            .await;

        let c = fake_creds(&server.uri());
        let resp = send_message_with_creds("hi", &SendOptions::default(), &c)
            .await
            .unwrap();
        assert_eq!(extract_text(&resp), "Hello");
    }

    #[tokio::test]
    async fn fetch_usage_parsing() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/backend-api/wham/usage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "plan_type": "pro",
                "rate_limit": {
                    "allowed": true,
                    "limit_reached": false,
                    "primary_window": {"used_percent": 25.0, "limit_window_seconds": 18000, "reset_after_seconds": 1000, "reset_at": 123},
                    "secondary_window": {"used_percent": 5.0, "limit_window_seconds": 604800, "reset_after_seconds": 2000, "reset_at": 456}
                }
            })))
            .mount(&server)
            .await;

        let c = fake_creds(&server.uri());
        let usage = fetch_usage_with_creds(&c).await.unwrap();
        assert_eq!(usage.plan_type.as_deref(), Some("pro"));
        assert_eq!(usage.max_used_percent(), Some(25.0)); // primary(25) > secondary(5)
    }

    #[tokio::test]
    async fn send_message_wrong_content_type_errors() {
        // 200 인데 SSE 가 아니라 JSON 에러 바디를 흘리는 경우. content-type 검사로 즉시 잡혀야 함.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/backend-api/codex/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(r#"{"error":"gateway exploded"}"#.as_bytes(), "application/json"),
            )
            .mount(&server)
            .await;

        let c = fake_creds(&server.uri());
        let err = send_message_with_creds("hi", &SendOptions::default(), &c)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("text/event-stream"),
            "expected content-type error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn send_message_accepts_missing_content_type() {
        // 회귀 가드: 실제 ChatGPT 백엔드는 정상 SSE 를 흘리면서 Content-Type 헤더를 아예
        // 안 주는 경우가 있다. "헤더 없음"을 "SSE 아님"으로 오판해 거부하면 안 된다.
        // set_body_bytes 는 set_body_raw/string 과 달리 Content-Type 을 붙이지 않는다.
        let server = MockServer::start().await;
        let sse = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello\"}\n\n\
data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hello\"}]}]}}\n\n\
data: [DONE]\n\n";
        Mock::given(method("POST"))
            .and(path("/backend-api/codex/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(sse.as_bytes()))
            .mount(&server)
            .await;

        let c = fake_creds(&server.uri());
        let resp = send_message_with_creds("hi", &SendOptions::default(), &c)
            .await
            .expect("missing content-type with valid SSE body must be accepted");
        assert_eq!(extract_text(&resp), "Hello");
    }

    #[tokio::test]
    async fn send_message_completed_but_failed_status_errors() {
        // response.completed 이벤트지만 내부 status=failed + error → 성공으로 새어나가면 안 됨.
        let server = MockServer::start().await;
        let sse = "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"failed\",\"error\":{\"message\":\"model exploded\"},\"output\":[]}}\n\n\
data: [DONE]\n\n";
        Mock::given(method("POST"))
            .and(path("/backend-api/codex/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse.as_bytes(), "text/event-stream"))
            .mount(&server)
            .await;

        let c = fake_creds(&server.uri());
        let err = send_message_with_creds("hi", &SendOptions::default(), &c)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("terminal status") && msg.contains("failed"),
            "expected terminal-status error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn send_message_response_incomplete_errors() {
        // 터미널 response.incomplete 이벤트 → 에러로 surface.
        let server = MockServer::start().await;
        let sse = "data: {\"type\":\"response.incomplete\",\"response\":{\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n\
data: [DONE]\n\n";
        Mock::given(method("POST"))
            .and(path("/backend-api/codex/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse.as_bytes(), "text/event-stream"))
            .mount(&server)
            .await;

        let c = fake_creds(&server.uri());
        let err = send_message_with_creds("hi", &SendOptions::default(), &c)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("incomplete"));
    }
}
