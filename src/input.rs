//! 요청 **입력**을 타입으로 구성하는 빌더 — `tools` 선언과 `input` 배열을 생 `json!` 대신.
//!
//! 출력(StreamEvent/Response)과 대칭. 와이어가 JSON 이라 빌더는 `Value` 를 돌려주고,
//! `SendOptions.tools`(`Vec<Value>`) / `open_stream_with_input(input: Value)` 에 그대로 들어간다.
//! (raw `json!` 도 여전히 escape 로 가능.)

use serde_json::{Value, json};

/// `SendOptions.tools` 에 넣을 툴 선언 빌더. 전부 `Value` 를 반환한다.
pub struct Tool;

impl Tool {
    /// 커스텀 function 툴. `parameters` 는 JSON Schema(Value).
    pub fn function(name: impl Into<String>, parameters: Value) -> Value {
        json!({ "type": "function", "name": name.into(), "parameters": parameters })
    }

    /// 설명을 단 커스텀 function 툴.
    pub fn function_described(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
    ) -> Value {
        json!({
            "type": "function",
            "name": name.into(),
            "description": description.into(),
            "parameters": parameters,
        })
    }

    /// 서버 빌트인 web_search (이 백엔드 수용 확인됨).
    pub fn web_search() -> Value {
        json!({ "type": "web_search" })
    }

    /// 서버 빌트인 image_generation (이 백엔드 수용 확인됨).
    pub fn image_generation() -> Value {
        json!({ "type": "image_generation" })
    }
}

/// `input` 배열에 넣을 항목 빌더 — 멀티턴/툴 결과 되먹임. 전부 `Value` 반환.
pub struct InputItem;

impl InputItem {
    /// 사용자 메시지.
    pub fn user(text: impl Into<String>) -> Value {
        json!({ "role": "user", "content": [{ "type": "input_text", "text": text.into() }] })
    }

    /// 커스텀 툴 실행 **결과** 되먹임. `call_id` 는 모델 function_call 의 것과 매칭.
    /// (function_call 에코는 `ToolCall::to_input_item()` 참고 — store:false 라 둘 다 넣는다.)
    pub fn function_output(call_id: impl Into<String>, output: impl Into<String>) -> Value {
        json!({ "type": "function_call_output", "call_id": call_id.into(), "output": output.into() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_builders_shape() {
        assert_eq!(Tool::web_search()["type"], "web_search");
        assert_eq!(Tool::image_generation()["type"], "image_generation");
        let f = Tool::function("get_weather", json!({"type":"object"}));
        assert_eq!(f["type"], "function");
        assert_eq!(f["name"], "get_weather");
        let fd = Tool::function_described("x", "desc", json!({}));
        assert_eq!(fd["description"], "desc");
    }

    #[test]
    fn input_item_shape() {
        let u = InputItem::user("안녕");
        assert_eq!(u["role"], "user");
        assert_eq!(u["content"][0]["text"], "안녕");
        let o = InputItem::function_output("c1", "{\"temp\":21}");
        assert_eq!(o["type"], "function_call_output");
        assert_eq!(o["call_id"], "c1");
        assert_eq!(o["output"], "{\"temp\":21}");
    }
}
