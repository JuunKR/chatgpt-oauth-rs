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
    use futures_util::StreamExt;
    let stream = client::open_stream(prompt, opts)
        .await
        .context("failed to open /responses stream")?;
    let mut stream = Box::pin(stream);
    let mut events = Vec::new();
    while let Some(ev) = stream.next().await {
        events.push(ev.context("stream event error")?);
    }
    Ok(events)
}
