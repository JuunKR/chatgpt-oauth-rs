# ChatGPT 백엔드 rate-limit / usage 응답 모양 (실측)

2026-05-29 실제 계정(plan: pro)으로 프로브해서 캡처한 응답 형태.
`examples/probe_ratelimit.rs` 로 재현 가능.

---

## 1) `GET https://chatgpt.com/backend-api/wham/usage`

- 헤더: `Authorization: Bearer <token>`, `originator: codex_cli_rs`,
  `User-Agent: codex_cli_rs/...`, `ChatGPT-Account-ID: <id>`
- `status = 200`, `content-type: application/json`

### 응답 본문 전체 형태

```json
{
  "user_id": "user-XXXXXXXXXXXXXXXXXXXXXXXX",
  "account_id": "user-XXXXXXXXXXXXXXXXXXXXXXXX",
  "email": "you@example.com",
  "plan_type": "pro",

  "rate_limit": {
    "allowed": true,
    "limit_reached": false,
    "primary_window": {
      "used_percent": 0,
      "limit_window_seconds": 18000,
      "reset_after_seconds": 1709,
      "reset_at": 1780034186
    },
    "secondary_window": {
      "used_percent": 0,
      "limit_window_seconds": 604800,
      "reset_after_seconds": 357129,
      "reset_at": 1780389606
    }
  },

  "code_review_rate_limit": null,

  "additional_rate_limits": [
    {
      "limit_name": "GPT-5.3-Codex-Spark",
      "metered_feature": "codex_bengalfox",
      "rate_limit": {
        "allowed": true,
        "limit_reached": false,
        "primary_window": {
          "used_percent": 0,
          "limit_window_seconds": 18000,
          "reset_after_seconds": 18000,
          "reset_at": 1780050478
        },
        "secondary_window": {
          "used_percent": 0,
          "limit_window_seconds": 604800,
          "reset_after_seconds": 604800,
          "reset_at": 1780637278
        }
      }
    }
  ],

  "credits": {
    "has_credits": false,
    "unlimited": false,
    "overage_limit_reached": false,
    "balance": "0",
    "approx_local_messages": [0, 0],
    "approx_cloud_messages": [0, 0]
  },

  "spend_control": {
    "reached": false,
    "individual_limit": null
  },

  "rate_limit_reached_type": null,
  "promo": null,
  "referral_beacon": null,
  "rate_limit_reset_credits": {
    "available_count": 0
  }
}
```

### 필드 의미 정리

| 경로 | 의미 |
|---|---|
| `plan_type` | 플랜 ("pro", "plus" 등) |
| `rate_limit.allowed` | 현재 호출 허용 여부 |
| `rate_limit.limit_reached` | 한도 도달 여부 |
| `rate_limit.primary_window` | 짧은 창(여기선 18000s=5h) |
| `rate_limit.secondary_window` | 긴 창(604800s=7d) |
| `*.used_percent` | 그 창에서 사용한 비율 (0~100) |
| `*.limit_window_seconds` | 창 길이(초) |
| `*.reset_after_seconds` | 리셋까지 남은 초 |
| `*.reset_at` | 리셋 시각 (epoch seconds) |
| `additional_rate_limits[]` | 모델별 별도 한도(예: Codex-Spark / bengalfox) |
| `credits` | 크레딧/잔액 |

> 우리 crate 의 `Usage` 구조체는 이 중 `plan_type` + `rate_limit`(primary/secondary)만 파싱한다. 나머지(additional_rate_limits, credits 등)는 serde 가 무시. 필요하면 확장 가능.

---

## 2) `POST https://chatgpt.com/backend-api/codex/responses` 의 응답 헤더

같은 rate-limit 정보를 **매 호출 응답 헤더로도** 준다 (별도 요청 불필요).
헤더라서 값이 전부 문자열.

```
x-codex-active-limit: premium
x-codex-plan-type: pro
x-codex-primary-used-percent: 0
x-codex-secondary-used-percent: 0
x-codex-primary-window-minutes: 300
x-codex-secondary-window-minutes: 10080
x-codex-primary-over-secondary-limit-percent: 0
x-codex-primary-reset-after-seconds: 1698
x-codex-secondary-reset-after-seconds: 357118
x-codex-primary-reset-at: 1780034186
x-codex-secondary-reset-at: 1780389606
x-codex-credits-has-credits: False
x-codex-credits-balance:
x-codex-credits-unlimited: False

# 모델별(bengalfox = Codex-Spark) 변형도 함께 옴
x-codex-bengalfox-primary-used-percent: 0
x-codex-bengalfox-secondary-used-percent: 0
x-codex-bengalfox-primary-window-minutes: 300
x-codex-bengalfox-secondary-window-minutes: 10080
x-codex-bengalfox-primary-reset-after-seconds: 18000
x-codex-bengalfox-secondary-reset-after-seconds: 604800
x-codex-bengalfox-primary-reset-at: 1780050488
x-codex-bengalfox-secondary-reset-at: 1780637288
x-codex-bengalfox-limit-name: GPT-5.3-Codex-Spark

# 기타
x-oai-request-id: <uuid>
x-models-etag: W/"..."
server: cloudflare
```

### `/wham/usage` JSON vs `/responses` 헤더 대응

| 헤더 | JSON 경로 |
|---|---|
| `x-codex-primary-used-percent` | `rate_limit.primary_window.used_percent` |
| `x-codex-primary-window-minutes` (분) | `rate_limit.primary_window.limit_window_seconds` (초 ÷ 60) |
| `x-codex-primary-reset-at` | `rate_limit.primary_window.reset_at` |
| `x-codex-plan-type` | `plan_type` |
| `x-codex-bengalfox-*` | `additional_rate_limits[].rate_limit.*` |

---

## 3) `/responses` SSE 이벤트

rate-limit 정보는 **SSE 이벤트에는 없음**. (codex-rs 는 `response.headers` 이벤트를 처리하지만
이 계정/버전에선 안 옴.) 받은 이벤트 타입:

```
response.created
response.in_progress
response.output_item.added
response.content_part.added
response.output_text.delta
response.output_text.done
response.content_part.done
response.output_item.done
response.completed
```

---

---

## 4) Responses API 선택 제어 필드 — 실측 (2026-05, `examples/probe_fields.rs`)

`/responses` 응답 객체는 요청 파라미터를 에코해준다. 그걸로 각 필드가 수용/적용되는지 확인.

| 필드 | 백엔드 디폴트(에코) | 커스텀값 테스트 | 결론 |
|---|---|---|---|
| `text` (`{verbosity, format}`) | `{"format":{"type":"text"},"verbosity":"medium"}` | `verbosity:"low"`→출력 363자, `"high"`→507자 (baseline 558). 에코 반영됨 | ✅ **수용+효과** → 노출 |
| `tool_choice` | `"auto"` | `"none"`→200, 에코 `"none"` | ✅ **수용** → 노출 |
| `parallel_tool_calls` | `true` | `false`→200, 에코 `false` | ✅ **수용** → 노출 |
| `service_tier` | `"default"` | `"flex"`→**400 "Unsupported service_tier: flex"**, `"priority"`→200이나 에코 여전히 `"default"`(무시) | ❌ **커스텀 실패/무시** → 제외 |
| `client_metadata` | (응답 에코 키는 `metadata`) | `client_metadata:{...}`→200이나 `metadata` 에코 여전히 `{}`(무효과). `metadata:{...}`→**400 "Unsupported parameter: metadata"** | ❌ **무효과 / 미지원** → 제외 |

### 적용 원칙
- 디폴트가 있는(=서버가 기본값을 가진) 필드는 우리 crate 에서 `Option`, **None 이면 미전송 → 서버 디폴트 사용**.
- 커스텀값이 실패(400)하거나 무시되는 필드(`service_tier`, `client_metadata`)는 **아예 노출하지 않음**(노출하면 400만 유발).
- 최종 노출: `text`, `tool_choice`, `parallel_tool_calls` 만.

---

## 5) 스트림 재개(turn-state/resume) — 실측 (2026-05, `examples/probe_turnstate.rs`, `probe_resume.rs`)

스트림이 중간에 끊겼을 때 이어받기가 가능한지 확인.

| 신호 | 결과 |
|---|---|
| turn-state/resume 응답 헤더 | ❌ 없음 |
| SSE 이벤트 `sequence_number` | ✅ 있음 (0,1,2,... 번호 매겨짐) |
| `store: true` 요청 | ❌ **400 "Store must be set to false"** — 저장 강제 거부 |
| `GET /responses/{id}?starting_after=N` 재개 | ❌ 도달 불가 (store 없이는 저장된 응답이 없음) |

**결론: 이 백엔드에서 스트림 중도 재개는 불가능.** 재개는 서버측 저장(store:true)이 전제인데
백엔드가 `store:false` 만 허용한다. sequence_number 가 있어도 재개할 원본이 없다.
→ 우리 crate 의 "연결 단계만 재시도"가 가능한 최대치이며, 스트림 중도 끊김은 호출자가 재시도해야 함.

---

## 결론 — rate-limit 가시성 구현 방식

- **선제적 조회** → `GET /wham/usage` (우리 `fetch_usage()`). 깔끔한 JSON, 별도 요청 1회.
- **호출 곁다리 조회** → `/responses` 응답의 `x-codex-*` 헤더 (무료, 단 스트림 소비 전 헤더 캡처 필요 → API 침습적이라 현재 미사용).
- SSE 이벤트 경로는 사용 안 함 (정보 없음).
