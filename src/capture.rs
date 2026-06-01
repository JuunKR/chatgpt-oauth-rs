//! 진단용 raw-응답 캡처 (feature = "capture" 에서만 컴파일).
//!
//! 목적: 이 크레이트가 때리는 각 엔드포인트의 **응답을 "파싱 전 원본"으로** 떠서,
//! OpenAI 가 응답 모양을 바꿨을 때 디버그 코드를 새로 짜지 않고 **눈으로** 확인하게 한다.
//! 일반 함수(`fetch_usage` 등)는 응답을 곧장 타입으로 파싱하므로, 모양이 바뀌면 파싱
//! 단계에서 에러가 나 정작 원본 바디를 못 본다. 여기 함수들은 그 파싱을 건너뛴다.
//!
//! URL/헤더/클라이언트는 실제 코드가 쓰는 내부 헬퍼를 그대로 재사용한다(드리프트 방지).

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::auth::{self, resolve_credentials};
use crate::client;

/// 파싱 전 HTTP 응답 스냅샷. `body` 는 원본 문자열, `body_json` 은 JSON 으로 읽히면 그 값.
#[derive(Debug, serde::Serialize)]
pub struct RawCapture {
    pub method: String,
    pub endpoint: String,
    pub status: u16,
    pub body: String,
    /// best-effort 파싱(JSON 이 아니면 None). 보기 좋게 출력하려는 용도.
    pub body_json: Option<Value>,
}

/// 오늘 날짜(UTC) "YYYY-MM-DD". 캡처 파일/디렉터리 이름에 쓴다(크레이트의 날짜 로직 재사용).
pub fn today_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, m, d, _, _, _) = crate::auth::epoch_to_ymdhms(secs);
    format!("{y:04}-{m:02}-{d:02}")
}

async fn snapshot(method: &str, url: &str, resp: reqwest::Response) -> Result<RawCapture> {
    let status = resp.status().as_u16();
    let body = resp.text().await.context("failed to read response body")?;
    let body_json = serde_json::from_str::<Value>(&body).ok();
    Ok(RawCapture {
        method: method.to_string(),
        endpoint: url.to_string(),
        status,
        body,
        body_json,
    })
}

/// `GET /wham/usage` — 사용량/rate-limit 응답을 **파싱 없이** 원본으로.
/// 멱등(GET)이라 반복 캡처해도 안전. 토큰 필요(없으면 ReloginRequired).
pub async fn usage_raw() -> Result<RawCapture> {
    let creds = resolve_credentials(false).await?;
    // 정상 client 경로와 동일하게, Authorization 붙이기 전에 토큰 목적지 검증.
    // (안 하면 CODEX_ALLOW_INSECURE_BASE_URL + 악성 base_url 로 디스크 토큰 유출 가능.)
    auth::validate_token_destination(&creds)?;
    let url = client::usage_url_from_base(&creds.base_url);
    let resp = client::shared_client()
        .get(&url)
        .timeout(Duration::from_secs(15))
        .headers(client::build_headers(&creds, false)?)
        .send()
        .await
        .context("usage request failed")?;
    snapshot("GET", &url, resp).await
}

/// `GET /models` — 모델 목록 응답을 **파싱 없이** 원본으로(멱등, 안전).
pub async fn models_raw() -> Result<RawCapture> {
    let creds = resolve_credentials(false).await?;
    // 정상 client 경로와 동일하게, Authorization 붙이기 전에 토큰 목적지 검증.
    // (안 하면 CODEX_ALLOW_INSECURE_BASE_URL + 악성 base_url 로 디스크 토큰 유출 가능.)
    auth::validate_token_destination(&creds)?;
    let url = format!("{}/models?client_version=1.0.0", creds.base_url);
    let resp = client::shared_client()
        .get(&url)
        .timeout(Duration::from_secs(15))
        .headers(client::build_headers(&creds, false)?)
        .send()
        .await
        .context("models request failed")?;
    snapshot("GET", &url, resp).await
}

/// device-code 발급 POST(`/deviceauth/usercode`) 응답을 원본으로.
/// **안전**: 로그인을 *시작*만 하고 폴링/완료를 하지 않으므로 부작용이 없다(코드만 발급되고
/// 버려짐). 인증 토큰도 필요 없다(로그인 전 단계).
pub async fn device_usercode_raw() -> Result<RawCapture> {
    let url = auth::DEVICE_USERCODE_URL;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context("failed to build http client")?;
    let resp = client
        .post(url)
        .json(&serde_json::json!({ "client_id": auth::CLIENT_ID }))
        .send()
        .await
        .context("device usercode request failed")?;
    snapshot("POST", url, resp).await
}

/// `POST /responses` — SSE 이벤트를 **집계 없이 원본 Value 배열**로 수집.
/// (open_stream 이 이미 원본 이벤트를 주므로 그걸 모은다. 토큰/쿼터 소모하지만 안전.)
pub async fn responses_raw(prompt: &str, opts: &crate::SendOptions) -> Result<Vec<Value>> {
    responses_with_input_raw(crate::client::normalize_input(prompt), opts).await
}

/// `POST /responses` 를 **완성된 input 배열**로 호출해 원본 이벤트를 수집(멀티턴/툴 결과
/// 되먹임 캡처용). 단발 텍스트는 `responses_raw` 가 편의 래퍼.
pub async fn responses_with_input_raw(
    input: Value,
    opts: &crate::SendOptions,
) -> Result<Vec<Value>> {
    use futures_util::StreamExt;
    let stream = client::open_stream_with_input(input, opts)
        .await
        .context("failed to open /responses stream")?;
    let mut stream = Box::pin(stream);
    let mut events = Vec::new();
    while let Some(ev) = stream.next().await {
        events.push(ev.context("stream event error")?);
    }
    Ok(events)
}

/// 빌트인 툴 **수용 여부 프로브**: 주어진 tools 로 `/responses` 에 최소 요청을 보내
/// 200(수용)인지 4xx(미지원/설정필요)인지 + 에러 바디를 캡처한다. 스트림을 끝까지 읽지
/// 않는다 — 200 이면 본문은 SSE(이미지 등 클 수 있음)라 "수용됨"만 기록하고, 비-200 이면
/// 에러 바디를 8KB 까지 보존한다. "이 백엔드가 어떤 빌트인 툴을 받는지" 카탈로그 발견용.
pub async fn responses_probe_raw(tools: Vec<Value>, prompt: &str) -> Result<RawCapture> {
    let creds = resolve_credentials(false).await?;
    // 정상 client 경로와 동일하게, Authorization 붙이기 전에 토큰 목적지 검증.
    // (안 하면 CODEX_ALLOW_INSECURE_BASE_URL + 악성 base_url 로 디스크 토큰 유출 가능.)
    auth::validate_token_destination(&creds)?;
    let opts = crate::SendOptions { tools, ..Default::default() };
    let body = client::build_request_body(client::normalize_input(prompt), &opts);
    let url = format!("{}/responses", creds.base_url);
    let resp = client::shared_client()
        .post(&url)
        .timeout(Duration::from_secs(30))
        .headers(client::build_headers(&creds, true)?)
        .json(&body)
        .send()
        .await
        .context("responses probe request failed")?;
    let status = resp.status().as_u16();
    let (body, body_json) = if status == 200 {
        ("(accepted — 200, SSE body not captured)".to_string(), None)
    } else {
        let b = crate::auth::bounded_text(resp, 8192).await;
        let j = serde_json::from_str::<Value>(&b).ok();
        (b, j)
    };
    Ok(RawCapture { method: "POST".into(), endpoint: url, status, body, body_json })
}
