# `/responses` 스트림 이벤트 — 실측 덤프 (2026-05-29)

`examples/capture.rs` 로 실제 `chatgpt.com/backend-api/codex/responses` 에 요청을 보내
**모든 SSE 이벤트를 원본 그대로** 받아 정리한 문서. 멀티 에이전트/툴 콜 소비자가 응답 구조를
"직접 까보지 않아도" 알 수 있게 하는 게 목적.

재현 (`capture` feature 필요):

```bash
cargo run --example capture --features capture -- responses "1+1은? 숫자만 한 글자로."         # 일반 텍스트
cargo run --example capture --features capture -- responses --web "최신 뉴스 검색해서 알려줘."   # 빌트인 web_search
cargo run --example capture --features capture -- responses --image "고양이 그려줘"             # 빌트인 image_generation
```
(커스텀 function 툴(§2)은 소비자가 통제하므로 capture 도구에서 제외 — 아래 §2 시퀀스는 과거 실측 기록.)

- 모델: `gpt-5.4` (서버가 결정. 요청은 `SendOptions.model` 기본값으로 보냄)
- `store: false`, `reasoning.effort: none` (기본) 기준 관측.

---

## 0) 한눈에 — 소비자가 알아야 할 3가지

1. **`response.completed` 의 `response.output` 은 항상 비어 있다(`[]`).** `store:false` 라서
   서버가 최종 output 을 완성본으로 다시 주지 않는다. **실제 출력(텍스트/툴콜)은
   `response.output_item.done` 이벤트들에서 모아야 한다.** (이 크레이트의
   `drive_stream_to_response` 가 바로 이 일을 한다 — `output_item.done` 을 누적해
   `response.output` 으로 합성.)
2. **output item 타입은 최소 2종**: `message`(어시스턴트 텍스트), `function_call`(툴 콜).
3. delta 이벤트엔 `obfuscation` 이라는 랜덤 패딩 필드가 붙는다 — **무시하면 된다**(캐시/길이
   유추 방지용 노이즈).

---

## 1) 일반 텍스트 응답 — 이벤트 순서

프롬프트 `"1+1은? 숫자만 한 글자로."` → 답 `"2"`.

| # | type | 핵심 페이로드 |
|---|------|--------------|
| 0 | `response.created` | `response` 객체 전체 에코 (status=in_progress) |
| 1 | `response.in_progress` | 〃 |
| 2 | `response.output_item.added` | `item: {type:"message", role:"assistant", phase:"final_answer", content:[], status:"in_progress", id:"msg_..."}` |
| 3 | `response.content_part.added` | `part: {type:"output_text", text:""}`, `item_id`, `content_index` |
| 4 | `response.output_text.delta` | **`delta: "2"`** ← 실제 텍스트 조각. `item_id`, `content_index`, `obfuscation` |
| 5 | `response.output_text.done` | `text: "2"` (이 content part 의 최종 텍스트) |
| 6 | `response.content_part.done` | `part: {type:"output_text", text:"2"}` |
| 7 | `response.output_item.done` | **`item: {type:"message", content:[{type:"output_text", text:"2"}], status:"completed", id:"msg_..."}`** ← 완성된 메시지 |
| 8 | `response.completed` | `status:"completed"`, **`output: []`(빈!)**, `usage:{input_tokens, output_tokens, total_tokens, ...}` |

→ 어시스턴트 텍스트를 얻는 두 길:
- **스트리밍**: `response.output_text.delta` 의 `delta` 를 이어 붙인다.
- **완성본**: `response.output_item.done` 의 `item.content[].text` (type=`output_text`).

---

## 2) 툴(function) 콜 — 이벤트 순서

`SendOptions.tools = [{type:"function", name:"get_weather", parameters:{...}}]`,
`tool_choice:"auto"`, 프롬프트로 호출 유도 → 모델이 `get_weather({"city":"서울"})` 호출.

| # | type | 핵심 페이로드 |
|---|------|--------------|
| 0–1 | `response.created` / `in_progress` | (에코) |
| 2 | `response.output_item.added` | **`item: {type:"function_call", name:"get_weather", call_id:"call_xBK...", id:"fc_...", arguments:"", status:"in_progress"}`** |
| 3–7 | `response.function_call_arguments.delta` | **`delta`** 가 인자 JSON 문자열 조각: `{"` → `city` → `":"` → `서울` → `"}`. `item_id`(=fc_...), `obfuscation` |
| 8 | `response.function_call_arguments.done` | **`arguments: "{\"city\":\"서울\"}"`** (완성된 인자 JSON 문자열) |
| 9 | `response.output_item.done` | **`item: {type:"function_call", name:"get_weather", call_id:"call_xBK...", arguments:"{\"city\":\"서울\"}", status:"completed"}`** ← 에이전트가 실행할 완전한 툴 콜 |
| 10 | `response.completed` | `status:"completed"`, `output: []`(빈), `usage:{...}` |

### 에이전트가 챙겨야 할 것
- 툴 콜 1건 = `output_item.done` 의 `item`(type=`function_call`)에서:
  - `name` — 어떤 도구
  - `arguments` — **JSON 문자열**(직접 `serde_json::from_str` 해서 파싱). delta 를 직접
    이을 필요 없음. `.done`/`output_item.done` 에 완성본이 온다.
  - `call_id` — 도구 실행 결과를 **다음 턴 입력**에 매칭시킬 때 쓰는 ID. (멀티턴: 결과를
    `{type:"function_call_output", call_id, output}` 로 만들어 `open_stream_with_input`
    의 input 배열에 넣어 되먹임.)
- `parallel_tool_calls:true` 면 한 턴에 `function_call` item 이 여러 개(output_index 다름)
  나올 수 있다.

---

## 2.5) 서버 빌트인 툴 (web_search / image_generation) — function_call 과 다른 패밀리

`SendOptions.tools = [{"type":"web_search"}]` 로 활성화(opt-in). 재현:
`cargo run --example capture --features capture -- responses --web "...웹 검색 유도 프롬프트..."`.

**우리가 정의한 function 툴과 흐름이 근본적으로 다르다:**

| # | type | 핵심 페이로드 |
|---|------|--------------|
| 2 | `response.output_item.added` | `item: {type:"web_search_call", id:"ws_...", status:"in_progress"}` (name/arguments **없음**) |
| 3 | `response.web_search_call.in_progress` | `item_id` 만 |
| 4 | `response.web_search_call.searching` | 서버가 실제 검색 수행 중 |
| 5 | `response.web_search_call.completed` | 검색 완료 |
| 6 | `response.output_item.done` | `item: {type:"web_search_call", action:{type:"search", query:"...", queries:[...]}, status:"completed"}` — 서버가 실행한 **검색 쿼리**. 결과 본문은 안 옴(서버가 내부 소비) |
| 7+ | `response.output_item.added` (message) → `output_text.delta` … | 검색 결과를 바탕으로 한 일반 텍스트 답변 (output_index 가 1로 증가) |
| 끝 | `response.completed` | `tool_usage.web_search.num_requests` 가 **0→1** 로 증가 |

### function_call vs 빌트인 툴
| | function_call (우리 정의) | web_search_call (서버 빌트인) |
|---|---|---|
| item 타입 | `function_call` (name, arguments, call_id) | `web_search_call` (action.query) |
| 전용 이벤트 | `function_call_arguments.delta/done` | `web_search_call.in_progress/searching/completed` |
| 실행 주체 | **소비자(에이전트)** 가 실행 후 결과 되먹임 | **서버**가 직접 실행·소비 |
| 소비자가 보는 것 | 호출 인자 (실행은 내 몫) | 검색 쿼리뿐, 결과는 안 옴 |

→ 멀티 에이전트: 빌트인 툴은 에이전트 루프가 실행할 필요 없음(서버가 처리해 답에 반영).
function 툴만 우리가 `call_id` 매칭해 결과를 되먹인다.

### image_generation — 또 다른 빌트인 (실측 확정)
`{"type":"image_generation"}` 활성화. web_search 와 마찬가지로 서버가 직접 실행한다.
이벤트 패밀리:
```
output_item.added                              → item {type:"image_generation_call", id:"ig_...", status:in_progress}
response.image_generation_call.in_progress
response.image_generation_call.generating
response.image_generation_call.partial_image   → (점진적 미리보기)
output_item.done                               → item: 아래 구조
```
`output_item.done` 의 item:
```json
{ "type":"image_generation_call", "id":"ig_...", "status":"...",
  "action":"generate", "output_format":"png", "size":"1254x1254", "quality":"low",
  "background":"opaque", "revised_prompt":"<모델이 다듬은 프롬프트>",
  "result":"<PNG base64 — 수백 KB~MB>" }
```
⚠️ `result` 가 base64 PNG 라 응답 JSON 이 **MB 단위로 큼**(고양이 아이콘 1개에 ~2.3MB).

### 빌트인 툴 카탈로그 — 실측 (2026-05-29, `capture -- builtin-probe`)
각 후보 타입을 `tools` 에 넣어 보내 200/4xx 로 수용 여부 확인:

| 툴 | 결과 |
|---|---|
| `web_search` | ✅ 200 수용 |
| `image_generation` | ✅ 200 수용 |
| `file_search` | ❌ 400 `Unsupported tool type: file_search` |
| `code_interpreter` | ❌ 400 `Unsupported tool type: code_interpreter` |
| `computer_use` | ❌ 400 `Unsupported tool type: computer_use` |
| `local_shell` | ❌ 400 `The local_shell tool is no longer supported.` |

→ **이 codex 백엔드가 받는 빌트인 툴 = `web_search` + `image_generation` 둘.** 매 응답
`tool_usage` 회계 블록(`image_gen`, `web_search`)과 정확히 일치. OpenAI 가 추가/제거하면
`capture -- builtin-probe` 재실행으로 확인.

---

## 2.6) 커스텀 툴 라운드트립 — 인풋/아웃풋 모양 (실측 기록, 2026-05-29)

커스텀 function 툴 한 사이클의 **요청 본문(우리가 보내는 인풋) + 응답(모델이 주는 아웃풋)**
모양. (이 사이클은 소비자가 통제하므로 capture 도구엔 없음 — 아래는 과거 실측 레퍼런스이자
에이전트 구현 시 `function_call_output` 형식 가이드.)

### 인풋 ①: 툴 선언 (요청 `body.tools[]`)
```json
{ "type": "function", "name": "get_weather",
  "description": "...",
  "parameters": { "type":"object", "properties":{"city":{"type":"string"}}, "required":["city"] } }
```

### 아웃풋: 모델의 function_call (응답, §2 참고)
`output_item.done` 의 item: `{type:"function_call", name, call_id, arguments:"<JSON 문자열>"}`.

### 인풋 ②: 결과 되먹임 (turn 2 요청 `body.input[]`)
**`store:false` 라 대화를 통째로 재생**해야 한다. input 배열 =
```json
[
  { "role":"user", "content":[{"type":"input_text","text":"..."}] },   // 원래 사용자 메시지
  { "type":"function_call", "name":"get_weather", "call_id":"call_...", "arguments":"{\"city\":\"서울\"}" },  // 모델 호출 에코
  { "type":"function_call_output", "call_id":"call_...", "output":"<문자열 결과>" }  // 우리 실행 결과
]
```
→ **200 + 모델이 그 결과를 써서 최종 답변** 생성 확인(2026-05-29). `call_id` 로 호출↔결과 매칭.
`output` 은 문자열(JSON 을 넣고 싶으면 문자열로 직렬화해서).

> 참고: 커스텀 function 툴은 **소비자(에이전트)가 스키마·실행·결과를 다 통제**하므로 capture
> 도구에서 제외했다(빌트인과 달리 OpenAI 가 말없이 바꾸는 부분이 거의 없음 — `function_call`
> emit / `function_call_output` accept 와이어 모양 정도). 위 모양은 2026-05-29 실측 기록이며,
> 에이전트 구현 시 이 input ②(`function_call_output`) 형식으로 결과를 되먹이면 된다.

---

## 3) 관측된 전체 이벤트 타입 (이 계정/버전, 2026-05)

```
response.created
response.in_progress
response.output_item.added
response.content_part.added
response.output_text.delta
response.output_text.done
response.content_part.done
response.output_item.done
response.function_call_arguments.delta     ← 커스텀 툴 콜일 때
response.function_call_arguments.done       ← 커스텀 툴 콜일 때
response.completed
```
빌트인 툴 활성화 시 추가 이벤트(`web_search_call.*` / `image_generation_call.*`)는 §2.5 참고.
터미널 실패/미완 시 `response.failed` / `response.incomplete`.

`BACKEND_API_NOTES.md §3` 의 목록 + 툴 콜 시 추가되는 2종(`function_call_arguments.*`).
- `reasoning.effort` 가 `none` 이 아니면 `response.reasoning_*` 류가 추가로 올 것으로 예상
  (이 덤프에선 effort=none 이라 미관측 — 추후 effort 설정해 재덤프 필요).
- 터미널 실패/미완 시: `response.failed` / `response.incomplete` (이 크레이트가 Err 로 surface).

---

## 4) 소비자 노출 방식 — raw vs 타입(StreamEvent)

두 레이어를 제공한다(additive):

- **raw**: `send_message`(이벤트 다 모아 최종 `response` 하나 반환) / `open_stream` ·
  `open_stream_with_input`(원본 `serde_json::Value` 이벤트 스트림). 캡처·디버그·escape용.
- **타입**(권장, `src/event.rs`): `open_event_stream` · `open_event_stream_with_input` 이
  **`StreamEvent`** 를 흘린다. 매직 문자열이 크레이트 안 한 곳(`StreamEvent::from_event`)에만 있다.

```rust
pub enum StreamEvent {
    TextDelta(String),               // output_text.delta
    ToolCall(ToolCall),              // 커스텀 function_call (call_id/name/arguments)
    WebSearchCall(WebSearch),        // 빌트인 web_search 완료 (query/queries)
    ImageGenerated(GeneratedImage),  // 빌트인 image_generation 완료 (result_b64 = raw base64, 디코드는 소비자)
    Completed(Value), Failed(Value), Incomplete(Value),
    Other { kind, raw },             // 진행 이벤트·미지 신규 이벤트 → 전방호환(안 깨짐)
}
```

설계: **고가치(텍스트/툴콜/빌트인 결과)만 타입, 나머지 Value, `Other` 로 전방호환.** OpenAI 가
이벤트를 추가하면 `Other` 로 흘러 안 깨지고, 이름/구조가 바뀌면 `from_event` 한 곳만 고친다
(변경 감지는 capture diff — §6). 전체 스키마 미러링은 일부러 안 한다(드리프트 방지).

**입력·단일응답도 타입화(출력과 대칭):**
- 입력: `Tool::{function, web_search, image_generation}` + `InputItem::{user, function_output}`
  + `ToolCall::to_input_item()`(function_call 에코). 생 `json!` 없이 `tools`/`input` 구성.
- 단일 응답: `send_message` → **`Response`**(`text()`/`tool_calls()`/`web_searches()`/`images()`/
  `usage() -> TokenUsage`/`raw()`). 스트리밍의 StreamEvent 와 같은 `from_item` 로직 재사용.
- 로그인 에러: auth 계열은 `anyhow::Error` 지만 `is_relogin_required(&e)` 로 재로그인 분기.
- 풀 예제: `examples/agent_loop.rs` (선언→호출→실행→되먹임→최종답, 생 json 0).

---

## 5) `response` 에코 객체 — 어떤 정보가 어디 있나

`response.created` / `response.in_progress` / `response.completed` 이벤트는 **거대한
`response` 객체를 통째로 에코**한다(`event["response"]`). 여기 묻혀 있는 주요 필드:

| 경로 | 내용 |
|------|------|
| `response.usage` | **이번 요청 토큰 사용량.** `created`/`in_progress` 에선 `null`, **`completed` 에서 채워짐.** `{input_tokens, output_tokens, total_tokens, input_tokens_details.cached_tokens, output_tokens_details.reasoning_tokens}` |
| `response.tool_usage` | 서버 **빌트인 툴 회계**. 항상 존재(안 쓰면 0). 키: `image_gen{input/output/total_tokens}`, `web_search{num_requests}`. 빌트인 툴 켜고 쓰면 여기 카운트 증가 |
| `response.tools` | 요청에 보낸 tools 에코(안 보내면 `[]`) |
| `response.model` | 서버가 실제 쓴 모델(예: `gpt-5.4`) |
| `response.reasoning` | `{effort, summary, context}` — effort 가 `none` 이 아니면 reasoning 이벤트도 추가됨 |
| `response.text` | `{verbosity, format}` 출력 제어 에코 |
| `response.status` | `in_progress` → `completed`/`failed`/`incomplete` 등 |

### 토큰 사용량 읽는 법 (스트림 안에 있다!)
- `send_message` → 반환된 Value 가 `usage` 보존: `resp["usage"]["total_tokens"]`.
- `open_stream` → `response.completed` 이벤트의 `ev["response"]["usage"]`.
- **rate-limit/쿼터(플랜 사용률·리셋)는 스트림에 없음** → 별도 `GET /usage` = `fetch_usage()`
  (타입 `Usage`/`RateLimit`/`RateWindow`). 토큰 카운트(위)와 혼동 주의.

---

## 6) 캡처 도구 — 백엔드 변경 재분석 워크플로

OpenAI 가 API 를 바꾸면 **디버그 코드를 새로 짤 필요 없이** 아래 도구를 다시 실행해
"지금 실제로 뭐가 오는지"를 원본 JSON 으로 보고 파서를 고치면 된다.

### `examples/capture.rs` — 엔드포인트별 **파싱 전 원본** 캡처 (`--features capture`)
일반 함수(`fetch_usage` 등)는 응답을 곧장 타입으로 파싱해서, 모양이 바뀌면 파싱 단계에서
에러가 나 **원본 바디를 못 본다.** 이 도구는 파싱을 건너뛰고 원본을 그대로 JSON 으로 뽑는다.
크레이트 내부 모듈 `chatgpt_oauth::capture`(feature-gated) 를 쓰며, 기본 빌드/공개 API 엔 영향 없음.

초점은 **우리가 통제 못 하는 표면**(서버 빌트인 툴 / 엔드포인트 응답). 커스텀 function 툴은
소비자가 다 통제하므로 제외(§2.6).

| 시나리오 | 엔드포인트 | 부작용 |
|---|---|---|
| `usage` | `GET /wham/usage` | ✅ 안전(멱등). `body_json.plan_type` 등 원본 |
| `models` | `GET /models` | ✅ 안전(멱등). 모델당 ~38필드(타입 `Model` 은 고가치만, 나머지 raw) |
| `responses` `--web` | `POST /responses` (SSE) | ⚠️ 토큰 소모. web_search 이벤트 원본 |
| `responses` `--image` | `POST /responses` (SSE) | ⚠️ 토큰 소모. image_generation 이벤트(결과 base64 → JSON MB 단위) |
| `builtin-probe` | `POST /responses` × N | ⚠️ 토큰 소모. 후보 빌트인 툴 200/4xx 전수 → 카탈로그(§2.5) |
| `device-code` | `POST /deviceauth/usercode` | ✅ 안전(로그인 *시작*만, 폴링 안 함). 코드 응답 모양 |
| ~~refresh~~ | `POST /oauth/token` | 🔴 **미포함** — refresh_token 회전 부작용. 필요 시 로그인 흐름에서 모양만 기록 |

```bash
cargo run --example capture --features capture -- usage                          > usage.json
cargo run --example capture --features capture -- responses --web "최신 뉴스 검색" > web.json
cargo run --example capture --features capture -- responses --image "고양이 그려줘" > image.json
cargo run --example capture --features capture -- builtin-probe                  > probe.json
cargo run --example capture --features capture -- device-code                    > device.json
```

`responses` 캡처는 `{meta, capture:{prompt, event_count, events:[...]}}` 형태. jq 로 분석/diff:

```bash
cargo run --example capture --features capture -- responses "안녕" > resp.json
jq -r '.capture.events[].type' resp.json                                                  # 이벤트 타입
jq '.capture.events[0].response.tool_usage' resp.json                                     # 빌트인 툴 회계
jq '.capture.events[] | select(.type=="response.completed") | .response.usage' resp.json  # 토큰
```

이 문서의 표/시퀀스는 위 도구의 2026-05-29 실측 출력 기준. 구조가 바뀌면 캡처를 다시
떠서 이 문서를 갱신한다. (각 표는 특정 시나리오/엔드포인트 이름으로 인용 → 드리프트 방지.)
