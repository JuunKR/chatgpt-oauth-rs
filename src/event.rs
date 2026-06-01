//! `/responses` 응답의 **얇은 타입 레이어** — 스트리밍 `StreamEvent` + 단일 응답 `Response`.
//!
//! `open_stream`/`send_message` 의 raw `serde_json::Value` 위에 타입을 얹는다(둘 다 escape 로 유지):
//! - **스트리밍**: `open_event_stream*` → `StreamEvent`(판별자만 타입 + 페이로드 Value + `Other` 탈출구).
//! - **단일 응답**: `send_message` → `Response`(`text()`/`tool_calls()`/`usage()` 등). output[] 분류는
//!   `StreamEvent` 와 같은 `from_item` 로직 공유.
//!
//! 매직 문자열(`"response.output_text.delta"` 등)을 외우지 않고 match/접근할 수 있다.
//!
//! 설계 원칙(실측 기반, 2026-05~06):
//! - 전체 스키마를 미러링하지 않는다 — 이벤트 종류가 열린 집합이라(웹검색/이미지/추론 등 계속
//!   추가됨) 드리프트만 늘린다. **고가치(텍스트 delta, 커스텀 툴 콜)만 타입**, 나머지는 Value.
//! - OpenAI 가 새 이벤트를 추가해도 `Other` 로 흘러 **안 깨진다**.
//! - 원본이 필요하면 `open_stream`(raw)을 그대로 쓰면 된다 — 이 레이어는 additive.

use futures_util::{Stream, StreamExt};
use serde_json::{Value, json};

use crate::client::{SendOptions, extract_text, open_stream, open_stream_with_input};
use crate::error::ClientError;

/// 모델이 호출한 **커스텀 function 툴** 1건 (서버 빌트인 아님 — 우리가 실행 후 되먹임).
#[derive(Debug, Clone)]
pub struct ToolCall {
    /// 결과를 되먹일 때 매칭하는 ID (`function_call_output.call_id`).
    pub call_id: String,
    /// 호출된 함수 이름 (디스패치용).
    pub name: String,
    /// 인자 — **JSON 문자열**(API 원본). 파싱은 `arguments_json()`.
    pub arguments: String,
}

impl ToolCall {
    /// output item(Value) 이 `function_call` 이면 `ToolCall` 로 (아니면 None).
    /// (스트림의 output_item.done · 단일 응답의 output[] 양쪽에서 재사용.)
    pub fn from_item(item: &Value) -> Option<ToolCall> {
        if item.get("type").and_then(|t| t.as_str()) != Some("function_call") {
            return None;
        }
        let call_id = str_of(item, "call_id");
        let name = str_of(item, "name");
        // 필수 필드(call_id/name)가 없으면 malformed → None. 빈 call_id 로 결과를
        // 되먹이는 사고를 막는다(에코 시 매칭 깨짐).
        if call_id.is_empty() || name.is_empty() {
            return None;
        }
        Some(ToolCall { call_id, name, arguments: str_of(item, "arguments") })
    }

    /// `arguments` 를 JSON 으로 파싱(아니면 None).
    pub fn arguments_json(&self) -> Option<Value> {
        serde_json::from_str(&self.arguments).ok()
    }

    /// 다음 턴 input 배열에 넣을 **function_call 에코** 아이템(store:false 라 대화 재생용).
    /// `InputItem::function_output` 와 함께 넣어 결과를 되먹인다.
    pub fn to_input_item(&self) -> Value {
        json!({
            "type": "function_call",
            "name": self.name,
            "call_id": self.call_id,
            "arguments": self.arguments,
        })
    }
}

/// 서버 빌트인 **web_search** 가 실행된 결과(완료 시점). 검색 결과 본문은 서버가 내부 소비하고
/// 우리에겐 **무엇을 검색했는지**(query)만 온다 — 모델이 그 결과로 만든 답은 TextDelta 로.
#[derive(Debug, Clone)]
pub struct WebSearch {
    pub id: String,
    pub status: String,
    /// 단일 검색어 (`action.query`).
    pub query: Option<String>,
    /// 여러 검색어 (`action.queries`).
    pub queries: Vec<String>,
}

/// 서버 빌트인 **image_generation** 결과(완료 시점).
#[derive(Debug, Clone)]
pub struct GeneratedImage {
    pub id: String,
    pub status: String,
    /// 생성 이미지 **base64**(서버 원본, 보통 PNG). 수백 KB~MB. 바이트 변환은 소비자가
    /// (예: `base64` crate 로 standard 디코드) — low-level client 라 raw 그대로만 준다.
    pub result_b64: Option<String>,
    /// 모델이 다듬은 프롬프트.
    pub revised_prompt: Option<String>,
    pub size: Option<String>,
    pub quality: Option<String>,
    pub output_format: Option<String>,
}

impl GeneratedImage {
    /// output item(Value) 이 `image_generation_call` 이면 `GeneratedImage` 로 (아니면 None).
    pub fn from_item(item: &Value) -> Option<GeneratedImage> {
        if item.get("type").and_then(|t| t.as_str()) != Some("image_generation_call") {
            return None;
        }
        Some(GeneratedImage {
            id: str_of(item, "id"),
            status: str_of(item, "status"),
            result_b64: item.get("result").and_then(|v| v.as_str()).map(String::from),
            revised_prompt: item.get("revised_prompt").and_then(|v| v.as_str()).map(String::from),
            size: item.get("size").and_then(|v| v.as_str()).map(String::from),
            quality: item.get("quality").and_then(|v| v.as_str()).map(String::from),
            output_format: item.get("output_format").and_then(|v| v.as_str()).map(String::from),
        })
    }

}

impl WebSearch {
    /// output item(Value) 이 `web_search_call` 이면 `WebSearch` 로 (아니면 None).
    pub fn from_item(item: &Value) -> Option<WebSearch> {
        if item.get("type").and_then(|t| t.as_str()) != Some("web_search_call") {
            return None;
        }
        let action = item.get("action");
        Some(WebSearch {
            id: str_of(item, "id"),
            status: str_of(item, "status"),
            query: action.and_then(|a| a.get("query")).and_then(|v| v.as_str()).map(String::from),
            queries: action
                .and_then(|a| a.get("queries"))
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|q| q.as_str().map(String::from)).collect())
                .unwrap_or_default(),
        })
    }
}

// 작은 헬퍼: item[key] 를 String 으로(없으면 "").
fn str_of(v: &Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

/// 타입화된 스트림 이벤트. 알맹이만 타입, 나머지는 `Other { kind, raw }` 로 전방호환.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// `response.output_text.delta` — 어시스턴트 텍스트 조각.
    TextDelta(String),
    /// `output_item.done` 이며 `item.type == "function_call"` — 실행할 **완전한** 커스텀 툴 콜.
    /// (인자가 완성된 시점. delta 를 직접 이을 필요 없음.)
    ToolCall(ToolCall),
    /// 서버 빌트인 web_search 완료 (`output_item.done`, item.type == "web_search_call").
    WebSearchCall(WebSearch),
    /// 서버 빌트인 image_generation 완료 (`output_item.done`, item.type == "image_generation_call").
    ImageGenerated(GeneratedImage),
    /// `response.completed` — 최종. `response` 객체(usage/output/tool_usage 등) 통째.
    /// 토큰은 `TokenUsage::from_response(&value)` 로 타입 파싱 가능(단일 응답과 동일).
    Completed(Value),
    /// `response.failed` — 터미널 실패. error 페이로드.
    Failed(Value),
    /// `response.incomplete` — 터미널 미완. incomplete_details 페이로드.
    Incomplete(Value),
    /// 그 외 전부: created/in_progress/content_part, 빌트인 툴 **진행** 이벤트
    /// (`web_search_call.in_progress/searching` · `image_generation_call.generating/partial_image`),
    /// `function_call_arguments.*`(델타), message 형 `output_item.done`, 미지의 신규 이벤트 등.
    /// `raw` 로 원본 접근 가능. (빌트인 툴의 **완료 결과**는 위 WebSearchCall/ImageGenerated 로 빠짐.)
    Other { kind: String, raw: Value },
}

impl StreamEvent {
    /// 원본 이벤트 Value → `StreamEvent`. (open_stream 이 주는 한 건을 분류.)
    pub fn from_event(ev: &Value) -> StreamEvent {
        let kind = ev.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match kind {
            "response.output_text.delta" => {
                StreamEvent::TextDelta(
                    ev.get("delta").and_then(|d| d.as_str()).unwrap_or("").to_string(),
                )
            }
            // completed 라도 내부 status 가 터미널 실패면 Failed/Incomplete 로
            // (send_message/drive_stream_to_response 의 status 검사와 동일 — 실패를 성공으로
            // 오인하지 않게).
            "response.completed" => {
                let resp = ev.get("response").cloned().unwrap_or(Value::Null);
                match resp.get("status").and_then(|s| s.as_str()).unwrap_or("") {
                    "failed" | "cancelled" | "expired" => {
                        StreamEvent::Failed(resp.get("error").cloned().unwrap_or(Value::Null))
                    }
                    "incomplete" => StreamEvent::Incomplete(
                        resp.get("incomplete_details").cloned().unwrap_or(Value::Null),
                    ),
                    _ => StreamEvent::Completed(resp),
                }
            }
            "response.failed" => StreamEvent::Failed(
                ev.get("response")
                    .and_then(|r| r.get("error"))
                    .or_else(|| ev.get("error"))
                    .cloned()
                    .unwrap_or(Value::Null),
            ),
            "response.incomplete" => StreamEvent::Incomplete(
                ev.get("response")
                    .and_then(|r| r.get("incomplete_details"))
                    .or_else(|| ev.get("incomplete_details"))
                    .cloned()
                    .unwrap_or(Value::Null),
            ),
            // output_item.done 은 item.type 으로 분기 (from_item 헬퍼 재사용 — 단일 응답
            // Response::* 와 동일 로직). function_call/web_search_call/image_generation_call
            // 은 타입화, 그 외(message 등)는 Other.
            "response.output_item.done" => {
                let item = ev.get("item").cloned().unwrap_or(Value::Null);
                if let Some(tc) = ToolCall::from_item(&item) {
                    StreamEvent::ToolCall(tc)
                } else if let Some(ws) = WebSearch::from_item(&item) {
                    StreamEvent::WebSearchCall(ws)
                } else if let Some(img) = GeneratedImage::from_item(&item) {
                    StreamEvent::ImageGenerated(img)
                } else {
                    StreamEvent::Other { kind: kind.to_string(), raw: ev.clone() }
                }
            }
            other => StreamEvent::Other { kind: other.to_string(), raw: ev.clone() },
        }
    }
}

/// `open_stream` 의 타입 버전 — 원본 대신 `StreamEvent` 를 흘린다.
pub async fn open_event_stream(
    user_message: &str,
    opts: &SendOptions,
) -> Result<impl Stream<Item = Result<StreamEvent, ClientError>>, ClientError> {
    Ok(open_stream(user_message, opts)
        .await?
        .map(|r| r.map(|v| StreamEvent::from_event(&v))))
}

/// `open_stream_with_input` 의 타입 버전 (멀티턴/툴 결과 되먹임용).
pub async fn open_event_stream_with_input(
    input: Value,
    opts: &SendOptions,
) -> Result<impl Stream<Item = Result<StreamEvent, ClientError>>, ClientError> {
    Ok(open_stream_with_input(input, opts)
        .await?
        .map(|r| r.map(|v| StreamEvent::from_event(&v))))
}

/// 이번 요청의 토큰 사용량 (`response.completed.usage`). rate-limit/쿼터(`fetch_usage`)와 다름.
#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub total: u64,
    /// 프롬프트 캐시 적중 토큰.
    pub cached: u64,
    /// 추론 토큰(reasoning effort 켰을 때).
    pub reasoning: u64,
}

impl TokenUsage {
    /// completed `response` 객체에서 토큰 사용량 파싱. `Response::usage()` 와
    /// `StreamEvent::Completed(v)` 양쪽에서 쓴다(스트리밍/단일 대칭).
    pub fn from_response(response: &Value) -> Option<TokenUsage> {
        let u = response.get("usage")?;
        let n = |path: &[&str]| -> u64 {
            let mut cur = u;
            for k in path {
                match cur.get(k) {
                    Some(v) => cur = v,
                    None => return 0,
                }
            }
            cur.as_u64().unwrap_or(0)
        };
        Some(TokenUsage {
            input: n(&["input_tokens"]),
            output: n(&["output_tokens"]),
            total: n(&["total_tokens"]),
            cached: n(&["input_tokens_details", "cached_tokens"]),
            reasoning: n(&["output_tokens_details", "reasoning_tokens"]),
        })
    }
}

/// **단일 요청**(`send_message`)의 타입 응답. 내부에 최종 `response` Value 를 들고,
/// 타입 접근자를 제공한다(스트리밍의 StreamEvent 와 대칭). 원본은 `raw()`.
#[derive(Debug, Clone)]
pub struct Response {
    raw: Value,
}

impl Response {
    pub(crate) fn new(raw: Value) -> Response {
        Response { raw }
    }

    /// 어시스턴트 텍스트(누적).
    pub fn text(&self) -> String {
        extract_text(&self.raw)
    }

    /// `output[]` 의 모든 항목(message/function_call/web_search_call/image_generation_call).
    fn output_items(&self) -> &[Value] {
        self.raw.get("output").and_then(|o| o.as_array()).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// 실행할 커스텀 툴 콜들 (`output[]` 의 function_call).
    pub fn tool_calls(&self) -> Vec<ToolCall> {
        self.output_items().iter().filter_map(ToolCall::from_item).collect()
    }

    /// 서버 빌트인 web_search 결과들.
    pub fn web_searches(&self) -> Vec<WebSearch> {
        self.output_items().iter().filter_map(WebSearch::from_item).collect()
    }

    /// 서버 빌트인 image_generation 결과들.
    pub fn images(&self) -> Vec<GeneratedImage> {
        self.output_items().iter().filter_map(GeneratedImage::from_item).collect()
    }

    /// 토큰 사용량(있으면). 스트리밍에선 `TokenUsage::from_response(completed_value)` 로 동일.
    pub fn usage(&self) -> Option<TokenUsage> {
        TokenUsage::from_response(&self.raw)
    }

    /// 원본 `response` Value (타입에 없는 필드 접근용 escape).
    pub fn raw(&self) -> &Value {
        &self.raw
    }

    /// 소유권째 원본 Value.
    pub fn into_raw(self) -> Value {
        self.raw
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_text_delta() {
        let ev = json!({"type":"response.output_text.delta","delta":"안"});
        match StreamEvent::from_event(&ev) {
            StreamEvent::TextDelta(t) => assert_eq!(t, "안"),
            other => panic!("expected TextDelta, got {other:?}"),
        }
    }

    #[test]
    fn classifies_function_call_tool_call() {
        // 실측 모양: output_item.done + item.type == function_call.
        let ev = json!({
            "type":"response.output_item.done",
            "item":{"type":"function_call","name":"get_weather",
                    "call_id":"call_123","arguments":"{\"city\":\"서울\"}","status":"completed"}
        });
        match StreamEvent::from_event(&ev) {
            StreamEvent::ToolCall(tc) => {
                assert_eq!(tc.name, "get_weather");
                assert_eq!(tc.call_id, "call_123");
                assert_eq!(tc.arguments_json().unwrap()["city"], "서울");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn completed_with_failed_status_maps_to_failed() {
        // completed 이벤트지만 내부 status=failed → 성공으로 새지 않고 Failed.
        let ev = json!({"type":"response.completed","response":{"status":"failed","error":{"message":"boom"}}});
        assert!(matches!(StreamEvent::from_event(&ev), StreamEvent::Failed(_)));
        let inc = json!({"type":"response.completed","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"}}});
        assert!(matches!(StreamEvent::from_event(&inc), StreamEvent::Incomplete(_)));
    }

    #[test]
    fn malformed_function_call_is_not_toolcall() {
        // call_id/name 없는 function_call 은 ToolCall 로 조작되지 않고 Other.
        let ev = json!({"type":"response.output_item.done","item":{"type":"function_call","arguments":"{}"}});
        assert!(matches!(StreamEvent::from_event(&ev), StreamEvent::Other { .. }));
        assert!(ToolCall::from_item(&json!({"type":"function_call","name":"x"})).is_none()); // call_id 없음
    }

    #[test]
    fn message_item_done_is_other_not_toolcall() {
        // message 형 output_item.done 은 ToolCall 이 아니라 Other.
        let ev = json!({"type":"response.output_item.done","item":{"type":"message","content":[]}});
        assert!(matches!(StreamEvent::from_event(&ev), StreamEvent::Other { .. }));
    }

    #[test]
    fn classifies_web_search_call() {
        // 실측 모양: web_search_call output_item.done + action.query/queries.
        let ev = json!({"type":"response.output_item.done","item":{
            "type":"web_search_call","id":"ws_1","status":"completed",
            "action":{"type":"search","query":"latest news","queries":["a","b"]}}});
        match StreamEvent::from_event(&ev) {
            StreamEvent::WebSearchCall(w) => {
                assert_eq!(w.id, "ws_1");
                assert_eq!(w.query.as_deref(), Some("latest news"));
                assert_eq!(w.queries, vec!["a".to_string(), "b".to_string()]);
            }
            other => panic!("expected WebSearchCall, got {other:?}"),
        }
    }

    #[test]
    fn classifies_image_generated() {
        let ev = json!({"type":"response.output_item.done","item":{
            "type":"image_generation_call","id":"ig_1","status":"completed",
            "result":"aGk=","output_format":"png","size":"1254x1254","quality":"low",
            "revised_prompt":"a cat icon"}});
        match StreamEvent::from_event(&ev) {
            StreamEvent::ImageGenerated(img) => {
                assert_eq!(img.id, "ig_1");
                assert_eq!(img.output_format.as_deref(), Some("png"));
                assert_eq!(img.revised_prompt.as_deref(), Some("a cat icon"));
                // raw base64 그대로 전달(디코드는 소비자 몫).
                assert_eq!(img.result_b64.as_deref(), Some("aGk="));
            }
            other => panic!("expected ImageGenerated, got {other:?}"),
        }
    }

    #[test]
    fn classifies_completed_and_terminals() {
        let c = json!({"type":"response.completed","response":{"status":"completed","usage":{"total_tokens":33}}});
        match StreamEvent::from_event(&c) {
            StreamEvent::Completed(r) => {
                assert_eq!(r["usage"]["total_tokens"], 33);
                // 스트리밍에서도 동일하게 타입 파싱(단일 응답과 대칭).
                assert_eq!(TokenUsage::from_response(&r).unwrap().total, 33);
            }
            other => panic!("expected Completed, got {other:?}"),
        }
        let f = json!({"type":"response.failed","response":{"error":{"message":"boom"}}});
        assert!(matches!(StreamEvent::from_event(&f), StreamEvent::Failed(_)));
        let i = json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}});
        assert!(matches!(StreamEvent::from_event(&i), StreamEvent::Incomplete(_)));
    }

    #[test]
    fn response_typed_accessors() {
        // 단일 응답: output[] 에 message + function_call, usage 채워진 형태.
        let resp = Response::new(json!({
            "status":"completed",
            "output":[
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"안녕"}]},
                {"type":"function_call","name":"get_weather","call_id":"c1","arguments":"{\"city\":\"서울\"}"}
            ],
            "usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15,
                     "input_tokens_details":{"cached_tokens":2},
                     "output_tokens_details":{"reasoning_tokens":3}}
        }));
        assert_eq!(resp.text(), "안녕");
        let calls = resp.tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "get_weather");
        assert_eq!(calls[0].call_id, "c1");
        let u = resp.usage().unwrap();
        assert_eq!((u.input, u.output, u.total, u.cached, u.reasoning), (10, 5, 15, 2, 3));
        // 에코 라운드트립: ToolCall → input item 모양.
        let echo = calls[0].to_input_item();
        assert_eq!(echo["type"], "function_call");
        assert_eq!(echo["call_id"], "c1");
    }

    #[test]
    fn unknown_and_builtin_events_are_other_forward_compat() {
        // 빌트인 라이프사이클 + 미지의 신규 이벤트 → 안 깨지고 Other.
        for kind in [
            "response.web_search_call.searching",
            "response.image_generation_call.partial_image",
            "response.function_call_arguments.delta",
            "response.some_future_event_42",
        ] {
            let ev = json!({"type": kind, "x": 1});
            match StreamEvent::from_event(&ev) {
                StreamEvent::Other { kind: k, raw } => {
                    assert_eq!(k, kind);
                    assert_eq!(raw["x"], 1);
                }
                other => panic!("expected Other for {kind}, got {other:?}"),
            }
        }
    }
}
