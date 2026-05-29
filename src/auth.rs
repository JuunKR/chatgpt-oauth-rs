//! ChatGPT OAuth — issue / refresh / persist access tokens for a ChatGPT account.
//!
//! Tokens live in `~/.codex/auth.json` (the location shared with the official
//! Codex CLI), so logging in here also logs you in for the Codex CLI and vice
//! versa.

// ── use: 다른 모듈/크레이트의 항목을 짧은 이름으로 끌어오는 선언 ──
use std::path::PathBuf; // 표준 라이브러리(std)의 파일 경로 타입(소유권 가진 경로)
use std::sync::OnceLock; // "딱 한 번만 초기화되는 값" 컨테이너 (refresh_lock 에서 사용)
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH}; // 시간 관련 타입들 (중괄호로 여러 개 한 번에)

use anyhow::{Context, Result, anyhow, bail}; // 에러 처리 라이브러리. Result/?/에러 생성 매크로
use base64::Engine; // base64 인코딩/디코딩 trait (JWT 해독에 필요)
use reqwest::StatusCode; // HTTP 상태 코드 타입 (200, 401 등)
use serde::{Deserialize, Serialize}; // 구조체 ↔ JSON 자동 변환용 trait

// ── const: 컴파일 타임 상수. 한 번 정하면 절대 안 바뀜. 관례상 대문자_스네이크 ──
// `pub(crate)` = 크레이트 내부에서만 보임. OAuth 엔드포인트/튜닝 값은 구현 디테일이라
// 공개 API 로 내보내지 않는다(외부 소비자는 device_code_login/resolve_credentials 만 쓰면 됨).
pub(crate) const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann"; // OAuth 앱 식별자
pub(crate) const ISSUER: &str = "https://auth.openai.com"; // 인증 서버 기본 주소
pub(crate) const TOKEN_URL: &str = "https://auth.openai.com/oauth/token"; // 토큰 발급/갱신 엔드포인트
pub(crate) const DEVICE_USERCODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode"; // 기기 코드 발급
pub(crate) const DEVICE_POLL_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token"; // 로그인 완료 폴링
pub(crate) const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback"; // OAuth 리다이렉트 주소
pub(crate) const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex"; // API 기본 베이스 URL
pub(crate) const REFRESH_SKEW_SECONDS: i64 = 120; // 만료 120초 전이면 미리 갱신 (i64 = 64비트 정수)
pub(crate) const MAX_POLL_INTERVAL_SECS: u64 = 60; // 폴링 간격 상한 (u64 = 부호 없는 64비트 정수)
/// Cap untrusted response bodies (e.g. error bodies) to this many bytes when reading.
/// Prevents memory blow-up from a hostile or buggy server.
// `pub(crate)` = 이 크레이트 안에서만 공개(외부 라이브러리엔 숨김). usize = 메모리 크기용 정수
pub(crate) const MAX_ERROR_BODY: usize = 8192; // 에러 본문은 최대 8KB 까지 읽어 그대로 보여줌(진단용)

// `&[&str]` = 문자열 슬라이스들의 배열을 빌려온 것. 신뢰하는 호스트 접미사 목록.
const TRUSTED_HOST_SUFFIXES: &[&str] = &["chatgpt.com", "openai.com"];

// `#[derive(...)]` = 트레잇 구현을 자동 생성. Debug(디버그 출력), thiserror::Error(에러 타입화)
#[derive(Debug, thiserror::Error)]
// `enum` = "여러 경우 중 하나"를 나타내는 타입. 여기선 에러의 종류.
pub enum AuthError {
    // `#[error(...)]` = 이 변형(variant)을 문자열로 출력할 때의 형식. {0}=첫 번째 필드
    #[error("relogin required: {0}")]
    ReloginRequired(String), // 재로그인 필요 (String 메시지를 품음)
    #[error("{0}")]
    Other(String), // 그 외 에러
}

// Clone=복제 가능, Serialize/Deserialize=JSON 변환 가능.
// Debug 는 직접 구현(아래) — 토큰/API 키가 로그·패닉 출력에 노출되지 않도록 가린다.
#[derive(Clone, Serialize, Deserialize)]
// `struct` = 여러 값을 묶은 자료구조. auth.json 파일 전체를 표현.
struct AuthFile {
    // default: `tokens` 키가 통째로 빠진 auth.json(예: OPENAI_API_KEY 만 있는 파일)도
    // 파싱 실패가 아니라 빈 TokensFile 로 받게 한다. doc 의 "parses but lacks tokens
    // → Ok(None)" 약속과 일치시키고, 첫 로그인 시 다른 클라가 써둔 키 보존이 깨지지 않게 함.
    #[serde(default)]
    tokens: TokensFile, // 토큰 묶음 (아래 struct)
    // `#[serde(...)]` = JSON 변환 규칙. default=없으면 기본값, skip_serializing_if=None이면 출력 생략
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_refresh: Option<String>, // `Option<T>` = 값이 있을 수도(Some) 없을 수도(None) 있음
    // rename = JSON 키 이름을 OPENAI_API_KEY 로 매핑
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "OPENAI_API_KEY")]
    openai_api_key: Option<String>,
    /// Preserve unknown fields so we don't clobber data written by other clients
    /// (e.g. the official Codex CLI).
    // `flatten` = 위에서 안 잡힌 나머지 JSON 키들을 여기 Map 에 몰아 담음 (다른 클라 데이터 보존)
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>, // 키=문자열, 값=임의 JSON
}

// 토큰/API 키를 가린 Debug. 파생 Debug 였다면 `{:?}` 한 번에 비밀이 통째로 찍힌다.
impl std::fmt::Debug for AuthFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthFile")
            .field("tokens", &self.tokens) // TokensFile 의 가린 Debug 사용
            .field("last_refresh", &self.last_refresh)
            .field(
                "openai_api_key",
                &self.openai_api_key.as_ref().map(|_| "[redacted]"),
            )
            .field("extra", &self.extra)
            .finish()
    }
}

// Debug 직접 구현(아래) — 토큰 노출 방지.
// Default + 필드 default: 토큰 키가 없거나 일부만 있는(유효 JSON) auth.json 을
// "손상"으로 보지 않고 빈 문자열로 받아, load 는 Ok(None)(첫 로그인 흐름), save 는
// OPENAI_API_KEY/extra 를 보존한 채 토큰을 채워 넣게 한다. 구문이 깨진 JSON 은
// 여전히 파싱 실패로 corrupt 처리된다.
#[derive(Clone, Default, Serialize, Deserialize)]
struct TokensFile {
    #[serde(default)]
    access_token: String,  // 실제 API 호출에 쓰는 접근 토큰. `String`=소유권 가진 문자열
    #[serde(default)]
    refresh_token: String, // 접근 토큰이 만료되면 새로 받기 위한 갱신 토큰
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>, // 미지의 토큰 필드 보존
}

impl std::fmt::Debug for TokensFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokensFile")
            .field("access_token", &"[redacted]")
            .field("refresh_token", &"[redacted]")
            .field("extra", &self.extra)
            .finish()
    }
}

// Debug 직접 구현(아래) — access/refresh 토큰 노출 방지.
#[derive(Clone)]
// 프로그램이 실제로 다루는 "자격증명" 타입. 파일 형식(AuthFile)과 분리해 둠.
pub struct CodexCredentials {
    pub access_token: String,         // 필드마다 `pub` → 외부에서 직접 읽기 가능
    pub refresh_token: String,
    pub base_url: String,             // 어느 서버로 보낼지
    pub last_refresh: Option<String>, // 마지막 갱신 시각 (없을 수도 있어 Option)
    /// 이 자격증명이 디스크(`~/.codex/auth.json`)에서 자동 로드된 "진짜" 토큰인지.
    /// true 면 `CODEX_ALLOW_INSECURE_BASE_URL` 우회가 켜져 있어도 신뢰 호스트(또는
    /// loopback)로만 전송한다 — 플래그를 깜빡 켜둔 채 진짜 구독 토큰이 임의 원격
    /// 호스트로 새는 사고 방지. 테스트 주입처럼 직접 만든 토큰은 false.
    pub from_disk: bool,
}

// 토큰 두 개는 가리고, 라우팅·메타 정보(base_url/last_refresh)만 노출한다.
impl std::fmt::Debug for CodexCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexCredentials")
            .field("access_token", &"[redacted]")
            .field("refresh_token", &"[redacted]")
            .field("base_url", &self.base_url)
            .field("last_refresh", &self.last_refresh)
            .field("from_disk", &self.from_disk)
            .finish()
    }
}

// `impl 타입명` = 그 타입에 메서드를 붙이는 블록.
impl CodexCredentials {
    /// Note: the JWT signature is NOT verified. This value is only a routing
    /// hint for the backend, not an authorization decision. The backend itself
    /// authorizes the request against the `Authorization: Bearer ...` header.
    // `&self` = 이 인스턴스를 빌려서 읽음(소유권 안 가져감). 반환은 있을 수도 없을 수도(Option).
    pub fn chatgpt_account_id(&self) -> Option<String> {
        // `?` = 결과가 None 이면 즉시 함수 전체를 None 으로 끝냄(조기 반환). 성공이면 알맹이를 꺼냄.
        let claims = decode_jwt_claims(&self.access_token)?; // JWT 해독 → 클레임(JSON)
        let auth = claims.get("https://api.openai.com/auth")?; // 해당 키 꺼냄
        // as_str()=문자열로 시도, map(String::from)=&str → String 으로 변환
        auth.get("chatgpt_account_id")?.as_str().map(String::from)
    }
}

// ──────────────────────────────────────────────────────────────────────
// Storage — ~/.codex/auth.json
// ──────────────────────────────────────────────────────────────────────

/// Absolute path to `~/.codex/auth.json`.
/// Precedence: `CODEX_HOME` env var → `dirs::home_dir()`. If both fail this
/// returns Err — we do NOT fall back to cwd, to avoid leaking secrets into a
/// random working directory (e.g. a git repo).
// `-> Result<PathBuf>` = 성공하면 경로(PathBuf), 실패하면 에러를 반환한다는 뜻.
pub fn auth_path() -> Result<PathBuf> {
    // `if let Ok(x) = ...` = 결과가 성공(Ok)일 때만 그 값을 x 에 묶어 블록 실행.
    if let Ok(custom) = std::env::var("CODEX_HOME") { // 환경변수 CODEX_HOME 읽기
        let trimmed = custom.trim(); // 앞뒤 공백 제거
        if !trimmed.is_empty() { // 비어있지 않으면
            // 경로 검증 로직은 순수 함수로 분리(env 없이 테스트 가능).
            return codex_home_to_auth_path(trimmed);
        }
    }
    // 홈 디렉토리 찾기. 못 찾으면(None) ok_or_else 로 에러로 변환.
    let home = dirs::home_dir().ok_or_else(|| {
        anyhow!( // anyhow! = 즉석에서 에러 메시지 만드는 매크로. `\` 는 문자열 줄바꿈 이음.
            "cannot locate home directory (HOME unset?). \
             Set the CODEX_HOME environment variable to an explicit directory. \
             Fallback to cwd is disabled to avoid leaking secrets."
        )
    })?; // 여기 `?` = 에러면 이 함수도 그 에러로 즉시 종료
    // ~/.codex/auth.json 경로 조립 후 성공 반환
    Ok(home.join(".codex").join("auth.json"))
}

/// `CODEX_HOME` 값(공백 제거된 비어있지 않은 문자열)을 auth.json 경로로 변환.
/// 상대 경로는 거부한다 — 주석은 "explicit directory" 를 약속하지만 검증이 없으면
/// `CODEX_HOME=.codex` 같은 값이 토큰을 현재 작업 디렉토리(예: git 레포)에 써서
/// 비밀이 커밋될 수 있다. cwd fallback 을 막아둔 의도와 정면으로 모순이라 막는다.
fn codex_home_to_auth_path(custom: &str) -> Result<PathBuf> {
    let dir = PathBuf::from(custom);
    if !dir.is_absolute() {
        bail!(
            "CODEX_HOME must be an absolute path (got `{custom}`). \
             A relative path would write tokens into the current working \
             directory (e.g. a git repo), risking secret leakage."
        );
    }
    Ok(dir.join("auth.json"))
}

/// Returns `Ok(None)` if the file is missing OR if it parses but lacks tokens
/// (compatible with the first-login flow). A file that exists but does NOT parse
/// is a corrupt token store and returns `Err` — this matches the save side, which
/// also refuses to touch a malformed file. (Previously load swallowed parse
/// errors as `Ok(None)`, so callers saw "no token, please log in" and then the
/// subsequent save failed with "corrupted" — load and save disagreed.)
// 반환: Result< Option<자격증명> > → 3가지 결과: 에러 / 정상인데 없음(None) / 있음(Some)
pub fn load_codex_cli_tokens() -> Result<Option<CodexCredentials>> {
    let path = auth_path()?; // 경로 구하기 (실패 시 ? 로 조기 반환)
    if !path.is_file() { // 파일이 없으면
        return Ok(None); // "에러는 아니고, 그냥 토큰 없음" 으로 반환
    }
    // 읽기 전에 권한을 점검한다. group/other 가 접근 가능한 토큰 파일은 조용히 받아들이지
    // 않고 0600 으로 조인다(다른 도구가 600 이 아닌 권한으로 만들어 둔 경우 방어).
    ensure_owner_only_perms(&path)?;
    // 파일을 바이트로 읽음. with_context = 실패 시 설명 메시지를 덧붙임. `&path`=경로를 빌려줌
    let bytes = std::fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
    // `match` = 값의 경우를 나눠 처리. JSON 파싱 시도.
    let file: AuthFile = match serde_json::from_slice(&bytes) {
        Ok(v) => v, // 성공 → 그 값 사용
        // 파일은 있는데 파싱 실패 = 손상된 토큰 저장소. save 와 동일하게 에러로 보고한다
        // (조용히 None 으로 삼키면 "토큰 없음 → 재로그인 → save 실패" 로 모순 발생).
        Err(e) => bail!(
            "{} exists but is not valid JSON (corrupt token store). \
             Back it up and remove it, then run device_code_login() again. Cause: {}",
            path.display(),
            e
        ),
    };
    // `||` = 논리 OR. 접근 토큰이나 갱신 토큰 둘 중 하나라도 비었으면
    if file.tokens.access_token.trim().is_empty() || file.tokens.refresh_token.trim().is_empty() {
        return Ok(None); // 토큰 없는 것으로 취급
    }
    // 파일 형식(AuthFile) → 프로그램용 형식(CodexCredentials) 으로 변환해 Some 으로 감싸 반환
    Ok(Some(CodexCredentials {
        access_token: file.tokens.access_token.trim().to_string(), // trim=앞뒤 공백 제거
        refresh_token: file.tokens.refresh_token.trim().to_string(),
        base_url: default_base_url(),   // 베이스 URL 은 별도 함수에서 결정
        last_refresh: file.last_refresh,
        from_disk: true, // 디스크에서 로드한 진짜 토큰 → 전송 목적지 검증 강화 대상
    }))
}

/// Atomic write (tmp file + rename). Refuses to overwrite an existing
/// malformed file — that protects fields other clients may have added (e.g.
/// `OPENAI_API_KEY` or unknown `extra` keys) from being clobbered.
///
/// NOTE: this is the **unlocked** primitive. It does NOT take the cross-process
/// auth-file lock, so it must only be called either (a) while already holding the
/// lock (as `resolve_inner` does) or (b) via [`save_codex_cli_tokens_locked`].
/// Calling it bare from a context that races refresh can clobber a freshly
/// rotated `refresh_token`.
// `creds: &CodexCredentials` = 자격증명을 "빌려서"(읽기만) 받음. 반환 Result<()> = 성공시 빈 값.
// pub(crate): 락을 안 잡는 위험한 원시 함수라 공개 API 에서 제외. 외부는 반드시
// `save_codex_cli_tokens_locked` 를 쓴다(동시 refresh 와의 race 로 회전된 refresh_token 클로버 방지).
pub(crate) fn save_codex_cli_tokens(creds: &CodexCredentials) -> Result<()> {
    let path = auth_path()?;
    // `if let Some(parent) = ...` = 부모 디렉토리가 있으면 그 값을 parent 로
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent) // 상위 폴더들까지 전부 생성 (~/.codex 등)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    // `let mut` = 값을 나중에 바꿀 수 있는 변수. (기본 변수는 불변임)
    // 기존 파일이 있으면 읽어 와서 보존, 없으면 빈 구조 생성 — 이 if 표현식의 결과를 file 에 담음.
    let mut file: AuthFile = if path.is_file() {
        let bytes = std::fs::read(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        match serde_json::from_slice(&bytes) {
            Ok(v) => v, // 정상 파싱 → 기존 내용 유지
            // bail! = "에러 메시지와 함께 함수 즉시 중단" 매크로. 깨진 파일은 덮어쓰기 거부.
            Err(e) => bail!(
                "refusing to write tokens because {} is corrupted \
                 (would clobber other clients' preserved fields). \
                 Back it up and remove it, then retry. Cause: {}",
                path.display(),
                e
            ),
        }
    } else {
        empty_auth_file() // 파일 없으면 빈 틀
    };
    // `.clone()` = 빌려온 값을 복제(소유권 가진 새 값 생성). file 은 mut 이라 필드 수정 가능.
    file.tokens.access_token = creds.access_token.clone();
    file.tokens.refresh_token = creds.refresh_token.clone();
    // unwrap_or_else(now_iso) = Option 이 None 이면 now_iso() 호출해 현재 시각으로 채움
    file.last_refresh = Some(creds.last_refresh.clone().unwrap_or_else(now_iso));

    // 구조체 → 보기 좋은(pretty) JSON 바이트로 직렬화
    let serialized = serde_json::to_vec_pretty(&file)?;

    // 원자적 저장 1단계: 임시 파일 이름 만들기.
    // 이름에 pid + 나노초를 섞어 매 쓰기마다 유일하게 한다. 고정된 ".auth.json.tmp" 는
    // 공유 CODEX_HOME 에서 (1) 미리 만들어진 느슨한 권한 파일이나 (2) 공격자가 심어 둔
    // 심볼릭 링크와 충돌할 수 있다 — 그런 파일을 열면 mode(0o600) 가 무시되거나 링크 타깃에
    // 토큰이 새어나간다. 아래 write_file_owner_only 는 create_new(O_EXCL) 로 열어
    // "이미 존재하는 파일/링크"를 절대 따라가지 않는다(존재하면 실패).
    let tmp_path = match path.file_name() {
        Some(name) => { // 파일명이 있으면
            let pid = std::process::id(); // 프로세스 식별자
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0); // 시계 이상 시 0 (유일성은 pid 가 일부 보장)
            let mut tmp_name = std::ffi::OsString::from("."); // OS 문자열 "."
            tmp_name.push(name);      // + 원래 파일명
            tmp_name.push(format!(".{pid}.{nanos}.tmp")); // + 유니크 접미사
            path.with_file_name(tmp_name) // 같은 폴더에 이 이름으로
        }
        None => bail!("auth_path has no filename component: {}", path.display()), // 비정상 경로
    };
    // 임시 파일에 먼저 쓴다 (소유자만 읽기/쓰기 권한으로)
    write_file_owner_only(&tmp_path, &serialized)
        .with_context(|| format!("failed to write {}", tmp_path.display()))?;
    // 원자적 저장 2단계: rename 으로 교체. rename 은 원자적이라 "반쯤 쓰인 파일"이 안 생김.
    if let Err(e) = std::fs::rename(&tmp_path, &path) { // rename 이 실패하면
        let _ = std::fs::remove_file(&tmp_path); // 임시파일 청소 (`let _ =` = 결과 무시)
        // 에러를 설명과 함께 반환
        return Err(anyhow!(e)).with_context(|| {
            format!("failed to rename {} -> {}", tmp_path.display(), path.display())
        });
    }
    Ok(()) // 모두 성공
}

/// 락을 잡은 채 저장하는 공개 진입점. 외부 호출자/로그인 경로는 이걸 써야 동시 refresh 와
/// 안전하게 직렬화된다. `resolve_inner` 처럼 **이미 락을 쥔** 곳에서는 부르면 안 된다
/// (같은 프로세스에서 flock 을 두 번 잡으려다 데드락).
pub async fn save_codex_cli_tokens_locked(creds: &CodexCredentials) -> Result<()> {
    // refresh 와 동일한 2단 잠금. drop 순서는 선언 역순 → 파일락 먼저 해제, 그 다음 뮤텍스.
    let _guard = refresh_lock().lock().await;
    let _flock = acquire_auth_file_lock().await?;
    save_codex_cli_tokens(creds)
}

// `#[cfg(unix)]` = 유닉스(맥/리눅스)에서 컴파일할 때만 이 버전을 사용. `&[u8]`=바이트 슬라이스
#[cfg(unix)]
fn write_file_owner_only(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;                  // 함수 안에서만 쓰는 use (지역 import)
    use std::os::unix::fs::OpenOptionsExt; // 유닉스 전용 mode() 메서드를 쓰기 위함
    let mut f = std::fs::OpenOptions::new() // 파일 열기 옵션을 하나씩 체이닝
        .write(true)        // 쓰기 모드
        .create_new(true)   // O_EXCL: 이미 존재하면 실패 → 미리 심긴 파일/심볼릭 링크를 안 따라감
        .mode(0o600)        // 권한 600(소유자만)으로 "생성 시점부터" 설정 → 권한 레이스 없음
        .open(path)?;       // create_new 라 truncate 불필요(항상 새 파일)
    f.write_all(bytes)?; // 전체 바이트 쓰기
    f.sync_all()?;       // 디스크에 강제 flush (전원 꺼져도 안전하게)
    Ok(())
}

// `#[cfg(not(unix))]` = 유닉스가 아닐 때(윈도우 등)만 이 버전 사용. 같은 이름 함수의 다른 구현.
#[cfg(not(unix))]
fn write_file_owner_only(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    // Windows: we don't tighten the file ACL from code. Relies on the user's
    // profile directory inheriting a reasonable ACL — do NOT place CODEX_HOME
    // on a shared / world-readable directory. create_new 로 기존 파일/링크 재사용은 막는다.
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

// 기존 토큰 파일의 권한이 소유자 전용(0600)인지 확인하고, group/other 비트가 있으면
// 0600 으로 조인다. 하드 거부 대신 조이기를 택한 이유: 다른 도구가 0644 로 만들어 둔
// 파일을 사용자가 못 쓰게 막으면 사용성이 크게 깨진다. 조이고 경고만 남긴다.
#[cfg(unix)]
fn ensure_owner_only_perms(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt; // mode() / set_mode() 를 위해
    let meta = std::fs::metadata(path)
        .with_context(|| format!("failed to stat {}", path.display()))?;
    let mode = meta.permissions().mode();
    if mode & 0o077 != 0 { // group/other 에 어떤 권한이라도 있으면
        tracing::warn!(
            path = %path.display(),
            mode = format!("{:o}", mode & 0o777),
            "auth.json is group/other-accessible — tightening to 0600"
        );
        let mut perms = meta.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms)
            .with_context(|| format!("failed to chmod {} to 0600", path.display()))?;
    }
    Ok(())
}

// 비유닉스(윈도우 등): 코드에서 ACL 을 조이지 않는다. write_file_owner_only 와 동일 정책.
#[cfg(not(unix))]
fn ensure_owner_only_perms(_path: &std::path::Path) -> Result<()> {
    Ok(())
}

// 빈 AuthFile 한 개를 만들어 돌려주는 도우미 함수.
fn empty_auth_file() -> AuthFile {
    AuthFile { // 구조체 인스턴스 생성 (필드: 값 형태)
        tokens: TokensFile {
            access_token: String::new(),  // 빈 문자열
            refresh_token: String::new(),
            extra: Default::default(),    // 그 타입의 기본값 (여기선 빈 Map)
        },
        last_refresh: None, // Option 의 "없음"
        openai_api_key: None,
        extra: Default::default(),
    }
}

// ──────────────────────────────────────────────────────────────────────
// JWT expiry check
// ──────────────────────────────────────────────────────────────────────

// JWT(점 2개로 나뉜 토큰)에서 가운데 payload 를 해독해 JSON 으로 반환. 실패하면 None.
fn decode_jwt_claims(token: &str) -> Option<serde_json::Value> {
    // split('.')=점 기준으로 자르고 collect()=Vec(동적 배열)로 모음. `Vec<&str>`=문자열 조각들의 배열
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() < 2 { // 조각이 2개 미만이면 JWT 형식 아님
        return None;
    }
    let mut payload = parts[1].to_string(); // 두 번째 조각(payload)을 소유 문자열로 복사 (mut=수정 예정)
    // base64url 은 길이가 4의 배수여야 함. 부족한 만큼 '=' 패딩 길이 계산.
    let padding = (4 - payload.len() % 4) % 4;
    payload.push_str(&"=".repeat(padding)); // '=' 를 padding 개수만큼 이어 붙임
    // URL-safe base64 로 디코드. .ok()? = 실패(Err)면 None 으로 즉시 반환, 성공이면 알맹이.
    let bytes = base64::engine::general_purpose::URL_SAFE
        .decode(payload.as_bytes())
        .ok()?;
    serde_json::from_slice(&bytes).ok() // 바이트 → JSON 파싱. 실패하면 ok()가 None 반환
}

/// Safe default: if we cannot decode the JWT, or the `exp` claim is missing,
/// or the system clock looks invalid, return `true` (treat as expiring). The
/// alternative — silently treating an unverifiable token as fresh — would
/// defer the failure to the next API call.
// 토큰이 곧 만료되는지 판정. skew_seconds=여유분. 반환 bool(참/거짓).
pub fn is_access_token_expiring(token: &str, skew_seconds: i64) -> bool {
    // `let Some(x) = ... else { ... }` = 성공이면 x 에 담고 계속, None 이면 else 블록 실행.
    // 여기선 "해독 못 하면 안전하게 만료된 것으로 간주(true)".
    let Some(claims) = decode_jwt_claims(token) else {
        return true;
    };
    // and_then = Option 안의 값으로 이어서 변환. exp(만료 시각) 클레임을 f64(실수)로.
    let Some(exp) = claims.get("exp").and_then(|v| v.as_f64()) else {
        return true; // exp 가 없으면 만료 취급
    };
    // 현재 시각을 1970 기준 경과 시간으로. 시스템 시계가 이상하면(Err) 만료 취급.
    let Ok(now_dur) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return true;
    };
    let now = now_dur.as_secs_f64(); // 초 단위 실수로 변환
    // 만료시각 <= 현재 + 여유분 이면 true. `as f64`=형 변환, .max(0)=음수면 0으로.
    exp <= now + (skew_seconds.max(0) as f64) // 세미콜론 없음 = 이 값이 함수 반환값
}

// ──────────────────────────────────────────────────────────────────────
// Device code login (one-time)
// ──────────────────────────────────────────────────────────────────────

// 서버 응답을 받을 구조체들. Deserialize 만 있으면 JSON → 구조체 자동 변환 가능.
#[derive(Debug, Deserialize)]
struct DeviceCodeResp {       // 기기 코드 발급 응답
    user_code: String,        // 사용자가 브라우저에 입력할 코드
    device_auth_id: String,   // 폴링 때 쓸 식별자
    #[serde(default)]         // 응답에 없으면 기본값(None)
    interval: Option<serde_json::Value>, // 권장 폴링 간격 (타입이 들쭉날쭉해서 Value 로 받음)
}

#[derive(Deserialize)]
struct PollResp {                  // 로그인 완료 후 폴링 응답
    authorization_code: String,    // 토큰으로 교환할 인가 코드
    code_verifier: String,         // PKCE 검증값
}

// 인가 코드/PKCE 검증값도 비밀 — 가린다.
impl std::fmt::Debug for PollResp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PollResp")
            .field("authorization_code", &"[redacted]")
            .field("code_verifier", &"[redacted]")
            .finish()
    }
}

#[derive(Deserialize)]
struct TokenResp {                      // 토큰 발급/갱신 응답
    access_token: String,               // 접근 토큰 (항상 있음)
    refresh_token: Option<String>,      // 갱신 토큰 (없을 수도 있어 Option)
}

// 토큰 응답 — Debug 출력에서 가린다.
impl std::fmt::Debug for TokenResp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenResp")
            .field("access_token", &"[redacted]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[redacted]"),
            )
            .finish()
    }
}

// `async fn` = 비동기 함수. 네트워크처럼 기다리는 작업을 효율적으로 처리. 호출할 때 .await 필요.
pub async fn device_code_login() -> Result<CodexCredentials> {
    // HTTP 클라이언트 생성. 15초 타임아웃 설정 후 build.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;

    // 1) 기기 코드 발급 요청 (POST)
    let resp = client
        .post(DEVICE_USERCODE_URL)
        .json(&serde_json::json!({ "client_id": CLIENT_ID })) // json! = JSON 리터럴 매크로
        .send()    // 요청 전송 (Future 반환)
        .await     // 완료될 때까지 기다림
        .context("failed to send device_code request")?; // 실패 시 설명 붙여 에러
    if !resp.status().is_success() { // 2xx 가 아니면
        bail!("device_code request failed: HTTP {}", resp.status());
    }
    // 응답 본문을 DeviceCodeResp 로 파싱
    let device: DeviceCodeResp = resp
        .json()
        .await
        .context("failed to parse device_code response JSON")?;
    // Clamp the server-supplied poll interval into [3, MAX_POLL_INTERVAL_SECS]
    // so a hostile or buggy server cannot make us wait forever.
    // 서버가 준 폴링 간격을 3~60초로 강제 보정 (악의적 값 방어).
    let mut poll_interval = device
        .interval
        .as_ref() // Option<Value> 를 빌려서 보기
        // 숫자(u64)면 그대로, 문자열이면 숫자로 파싱 시도 (or_else=앞이 None 일 때 대안)
        .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
        .unwrap_or(5)                       // 못 구하면 기본 5초
        .clamp(3, MAX_POLL_INTERVAL_SECS);  // 3 이상 60 이하로 가둠 (slow_down 시 증가)
    if device.user_code.is_empty() || device.device_auth_id.is_empty() {
        bail!("device_code response is missing required fields"); // 필수값 누락 방어
    }

    // 사용자 안내 출력. `\x1b[94m ... \x1b[0m` = 터미널 색상(파란색) 제어 문자. {ISSUER}=문자열 보간.
    println!("\nTo sign in with your ChatGPT account:\n");
    println!("  1. Open in your browser: \x1b[94m{ISSUER}/codex/device\x1b[0m");
    println!("  2. Enter this code:      \x1b[94m{}\x1b[0m\n", device.user_code);
    println!("Waiting for sign-in... (Ctrl+C to cancel)");

    let max_wait = Duration::from_secs(15 * 60); // 최대 15분 대기
    let start = std::time::Instant::now();        // 시작 시각 기록 (경과 측정용)
    // `loop { ... break 값 }` = 무한 반복. break 에 값을 주면 그게 code_resp 가 됨.
    let code_resp: PollResp = loop {
        if start.elapsed() > max_wait { // 15분 초과면 포기
            bail!("sign-in not completed within 15 minutes");
        }
        tokio::time::sleep(Duration::from_secs(poll_interval)).await; // 간격만큼 비동기 대기
        // 3) 로그인 됐는지 폴링
        let poll = client
            .post(DEVICE_POLL_URL)
            .json(&serde_json::json!({
                "device_auth_id": device.device_auth_id,
                "user_code": device.user_code,
            }))
            .send()
            .await
            .context("failed to send device_code poll request")?;
        match poll.status() { // 상태 코드로 분기
            StatusCode::OK => { // 200 = 로그인 완료
                break poll      // break 로 loop 를 끝내며 이 값을 code_resp 로 반환
                    .json()
                    .await
                    .context("failed to parse poll response JSON")?;
            }
            // 403/404 = 이 백엔드가 쓰는 "아직 로그인 전" 신호 → 계속 폴링
            StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => continue,
            // 그 외 상태는 본문의 표준 OAuth 디바이스 에러 코드를 확인한다(RFC 8628).
            // 백엔드가 pending 을 4xx + 코드로 줄 수도 있어, 무조건 bail 하지 않는다.
            other => {
                let body = bounded_text(poll, MAX_ERROR_BODY).await;
                let (code, msg) = parse_oauth_error(&body);
                match code.as_deref() {
                    // 아직 사용자가 승인 전 → 계속 폴링
                    Some("authorization_pending") => continue,
                    // 너무 자주 폴링함 → 간격 늘리고 계속
                    Some("slow_down") => {
                        poll_interval = (poll_interval + 5).min(MAX_POLL_INTERVAL_SECS);
                        continue;
                    }
                    // 코드 만료/거부 → 명확한 메시지로 종료
                    Some("expired_token") => {
                        bail!("device code expired before sign-in completed; restart login");
                    }
                    Some("access_denied") => bail!("sign-in was denied"),
                    // 그 외는 진짜 에러 (메시지 있으면 덧붙임)
                    _ => match msg {
                        Some(m) => bail!("device poll error: HTTP {other}: {m}"),
                        None => bail!("device poll error: HTTP {other}"),
                    },
                }
            }
        }
    };

    // 4) 인가 코드 → 실제 토큰 교환. form = (키, 값) 쌍의 배열 (HTTP form 전송용).
    let form = [
        ("grant_type", "authorization_code"),
        ("code", &code_resp.authorization_code), // `&` = 빌려서 참조
        ("redirect_uri", DEVICE_REDIRECT_URI),
        ("client_id", CLIENT_ID),
        ("code_verifier", &code_resp.code_verifier),
    ];
    let token_resp = client
        .post(TOKEN_URL)
        .form(&form) // JSON 이 아니라 form 형식으로 전송
        .send()
        .await
        .context("failed to send token exchange request")?;
    if !token_resp.status().is_success() {
        bail!("token exchange failed: HTTP {}", token_resp.status());
    }
    let tokens: TokenResp = token_resp
        .json()
        .await
        .context("failed to parse token exchange response JSON")?;
    let access = tokens.access_token.trim().to_string(); // 공백 제거 후 소유 문자열로
    let refresh = tokens
        .refresh_token
        .map(|s| s.trim().to_string()) // Some 이면 trim, None 이면 그대로 None
        .unwrap_or_default();          // None 이면 빈 문자열로
    if access.is_empty() || refresh.is_empty() {
        bail!("token exchange response is missing access/refresh");
    }

    // 받은 토큰으로 자격증명 구성
    let creds = CodexCredentials {
        access_token: access,
        refresh_token: refresh,
        base_url: default_base_url(),
        last_refresh: Some(now_iso()), // 지금 시각 기록
        from_disk: true,               // 곧 디스크에 저장되는 진짜 토큰
    };
    // refresh 경로와 동일한 in-process + cross-process 락을 잡고 저장한다. 락 없이 저장하면
    // 동시에 도는 refresh 의 read-modify-write 와 경쟁해 갓 회전한 refresh_token 을 덮어쓸 수 있다.
    save_codex_cli_tokens_locked(&creds).await?; // 파일에 저장
    Ok(creds)                                     // 호출자에게 자격증명 반환
}

// ──────────────────────────────────────────────────────────────────────
// Refresh
// ──────────────────────────────────────────────────────────────────────

// 갱신 토큰으로 새 (접근토큰, 갱신토큰) 한 쌍을 받아옴. 반환 타입 (String, String) = 튜플.
// pub(crate): 락도 안 잡고 디스크 저장도 안 하는 저수준 원시 함수라 공개 API 에서 제외.
// 외부는 `resolve_credentials`(잠금+로드+갱신+저장 일괄 처리)를 쓴다.
pub(crate) async fn refresh_tokens(refresh_token: &str) -> Result<(String, String)> {
    if refresh_token.trim().is_empty() {
        // `.into()` = &str → String 자동 변환. 비었으면 재로그인 필요 에러.
        return Err(anyhow!(AuthError::ReloginRequired(
            "refresh_token is empty".into()
        )));
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;
    let form = [
        ("grant_type", "refresh_token"), // 갱신 방식임을 명시
        ("refresh_token", refresh_token),
        ("client_id", CLIENT_ID),
    ];
    let resp = client
        .post(TOKEN_URL)
        .form(&form)
        .send()
        .await
        .context("failed to send token refresh request")?;
    let status = resp.status(); // 상태 코드 미리 저장 (resp 는 곧 소비됨)
    if !status.is_success() {
        let body = bounded_text(resp, MAX_ERROR_BODY).await; // 에러 본문을 4KB 한도로 읽음
        let (code, msg) = parse_oauth_error(&body);          // 튜플 분해 대입
        // matches! = 패턴에 맞으면 true. 특정 에러 코드거나 401/403 이면 "재로그인 필요"로 판단.
        let relogin = matches!(
            code.as_deref(), // Option<String> → Option<&str>
            Some("invalid_grant" | "invalid_token" | "invalid_request" | "refresh_token_reused")
        ) || matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN);
        // 메시지가 있으면 그걸, 없으면 기본 문구 사용
        let pretty = msg.unwrap_or_else(|| format!("token refresh failed: HTTP {status}"));
        if relogin {
            tracing::warn!(%status, "token refresh failed — relogin required");
            return Err(anyhow!(AuthError::ReloginRequired(pretty))); // 재로그인 에러로 구분 반환
        }
        tracing::warn!(%status, "token refresh failed");
        bail!(pretty); // 일반 에러
    }
    let tr: TokenResp = resp
        .json()
        .await
        .context("failed to parse refresh response JSON")?;
    let access = tr.access_token.trim().to_string();
    if access.is_empty() {
        return Err(anyhow!(AuthError::ReloginRequired(
            "refresh response had no access_token".into()
        )));
    }
    // 새 갱신토큰이 오면 그걸 쓰고, 안 오면 기존 것을 재사용 (서버가 생략하는 경우 대비).
    let new_refresh = tr
        .refresh_token
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())                            // 빈 문자열이면 None 으로
        .unwrap_or_else(|| refresh_token.trim().to_string()); // 없으면 기존 토큰
    Ok((access, new_refresh)) // 튜플로 둘 다 반환
}

/// Read up to `max_bytes` of a response body and return it as a (lossy) String.
/// Used to defang malicious / huge error bodies before logging them.
// 응답 본문을 최대 max_bytes 까지만 읽어 문자열로. 거대한/악의적 본문 방어용.
pub(crate) async fn bounded_text(resp: reqwest::Response, max_bytes: usize) -> String {
    use futures_util::StreamExt;       // .next() 를 쓰기 위한 trait import
    let mut buf: Vec<u8> = Vec::new();  // 누적 버퍼 (빈 바이트 벡터)
    let mut stream = resp.bytes_stream(); // 본문을 조각(chunk) 스트림으로
    // `while let Some(x) = ...` = 스트림에서 값이 나오는 동안 반복. 끝나면 None → 종료.
    while let Some(chunk) = stream.next().await {
        let Ok(bytes) = chunk else { break }; // 조각이 에러면 반복 중단
        let remain = max_bytes.saturating_sub(buf.len()); // 남은 허용량 (음수 방지 뺄셈)
        if remain == 0 { // 한도 다 찼으면
            break;
        }
        if bytes.len() <= remain {
            buf.extend_from_slice(&bytes);       // 통째로 추가
        } else {
            buf.extend_from_slice(&bytes[..remain]); // 남은 만큼만 잘라 추가 ([..n]=앞 n개 슬라이스)
            break;
        }
    }
    // from_utf8_lossy = 깨진 바이트는 �로 대체(패닉 없이). into_owned = String 으로 소유화.
    String::from_utf8_lossy(&buf).into_owned()
}

// OAuth 에러 본문(JSON)에서 (에러코드, 사람이 읽을 메시지)를 뽑음. 둘 다 Option.
fn parse_oauth_error(body: &str) -> (Option<String>, Option<String>) {
    // 본문을 JSON 으로 파싱. 실패하면 (None, None) 반환. `::<Value>` = 타입 명시.
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return (None, None);
    };
    // 형태 1) OpenAI: {"error": {"code": "...", "message": "..."}}
    if let Some(err) = v.get("error").and_then(|e| e.as_object()) { // error 가 객체면
        let code = err
            .get("code")
            .or_else(|| err.get("type")) // code 없으면 type 으로 대체
            .and_then(|x| x.as_str())
            .map(String::from);
        let msg = err
            .get("message")
            .and_then(|x| x.as_str())
            .map(|s| format!("token refresh failed: {s}")); // 메시지 다듬기
        return (code, msg);
    }
    // 형태 2) 평범한 OAuth: {"error": "code", "error_description": "..."}
    if let Some(s) = v.get("error").and_then(|x| x.as_str()) { // error 가 문자열이면
        let desc = v
            .get("error_description")
            .or_else(|| v.get("message"))
            .and_then(|x| x.as_str())
            .map(|d| format!("token refresh failed: {d}"));
        return (Some(s.to_string()), desc);
    }
    (None, None) // 둘 다 아니면 빈 결과
}

// ──────────────────────────────────────────────────────────────────────
// Public entry point — usable credentials with auto-refresh
// ──────────────────────────────────────────────────────────────────────

/// Serialize concurrent refreshes within a single process. Cross-process races
/// are handled naturally by the OAuth server returning `refresh_token_reused`,
/// which surfaces as a `ReloginRequired` error to the user.
// 프로세스 안에서 동시 갱신을 막는 잠금장치. `&'static` = 프로그램 끝까지 사는 참조.
fn refresh_lock() -> &'static tokio::sync::Mutex<()> {
    // `static` + OnceLock = "처음 호출될 때 딱 한 번 생성되고 이후 재사용". Mutex<()>=내용 없는 자물쇠.
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(())) // 없으면 만들고, 있으면 그대로 반환
}

/// auth.json 의 크로스-프로세스 잠금 파일 경로 (`auth.json` → `auth.json.lock`).
fn auth_lock_path() -> Result<PathBuf> {
    Ok(auth_path()?.with_extension("json.lock"))
}

/// 파일 락 가드. 이 값이 drop 되면 내부 File 의 fd 가 닫히고, OS 가 그 fd 로 잡았던
/// flock 을 자동 해제한다(POSIX flock / Windows LockFileEx 모두 동일). 그래서 별도
/// unlock 호출 없이 가드 수명만 유지하면 된다. 필드는 그 수명 유지가 목적.
struct FileLockGuard(#[allow(dead_code)] std::fs::File);

/// 크로스-프로세스 배타 락 획득. 여러 *프로세스*가 같은 `~/.codex/auth.json` 을 공유할 때,
/// 동시 토큰 갱신으로 한쪽이 `refresh_token_reused` 로 강제 로그아웃되는 것을 막는다.
///
/// blocking flock 으로 executor 스레드를 막지 않도록 try-lock + async sleep 으로 대기한다.
/// 락을 쥔 프로세스가 죽으면 OS 가 fd 를 닫으며 자동 해제하므로 영구 데드락은 없다(타임아웃은 안전망).
async fn acquire_auth_file_lock() -> Result<FileLockGuard> {
    use fs2::FileExt;
    let path = auth_lock_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        // 락 파일은 flock 핸들로만 쓰고 내용은 절대 건드리지 않는다 → 명시적으로 truncate 안 함.
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("failed to open lock file {}", path.display()))?;
    let start = Instant::now();
    let mut logged_wait = false;
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(FileLockGuard(file)),
            // 다른 프로세스가 쥐고 있음 → 잠깐 쉬고 재시도 (스레드 블록 안 함).
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if !logged_wait {
                    tracing::debug!("waiting for cross-process auth.json lock");
                    logged_wait = true;
                }
                if start.elapsed() > Duration::from_secs(30) {
                    bail!("timed out waiting for auth file lock: {}", path.display());
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => {
                return Err(e).with_context(|| format!("failed to lock {}", path.display()));
            }
        }
    }
}

/// 공통 구현: 잠금 획득 → 토큰 로드 → `should_refresh`(잠금 안에서 재읽은 토큰으로 판정)
/// 가 참이면 갱신 후 저장. 잠금을 잡은 뒤 파일을 다시 읽기 때문에, 다른 태스크/프로세스가
/// 이미 갱신했다면 `should_refresh` 가 그 최신 토큰을 보고 스킵할 수 있다.
async fn resolve_inner(
    should_refresh: impl Fn(&CodexCredentials) -> bool,
) -> Result<CodexCredentials> {
    // 2단 잠금:
    //  ① in-process: 같은 프로세스의 동시 태스크 직렬화 (빠름)
    //  ② cross-process: 다른 프로세스와의 동시 갱신 직렬화 (파일 락)
    // drop 순서는 선언 역순 → 파일락 먼저 해제, 그 다음 뮤텍스.
    let _guard = refresh_lock().lock().await;
    let _flock = acquire_auth_file_lock().await?;
    // 저장된 토큰 로드. 없으면(None) 재로그인 필요 에러.
    let Some(mut creds) = load_codex_cli_tokens()? else {
        return Err(anyhow!(AuthError::ReloginRequired(format!(
            "no Codex token at {}. Call device_code_login() first.",
            auth_path()?.display()
        ))));
    };
    if should_refresh(&creds) {
        tracing::debug!("access token needs refresh — calling token endpoint");
        // 새 토큰 받아서 creds 업데이트 (creds 가 mut 라 수정 가능)
        let (new_access, new_refresh) = refresh_tokens(&creds.refresh_token).await?;
        creds.access_token = new_access;
        creds.refresh_token = new_refresh;
        creds.last_refresh = Some(now_iso());
        save_codex_cli_tokens(&creds)?; // 갱신된 토큰을 디스크에 저장
    }
    Ok(creds)
}

/// 이 라이브러리의 핵심 진입점: "바로 쓸 수 있는 자격증명"을 돌려줌.
/// `force_refresh` 가 true 이거나 토큰이 곧 만료면 갱신한다. (선제적 경로)
pub async fn resolve_credentials(force_refresh: bool) -> Result<CodexCredentials> {
    resolve_inner(|creds| {
        force_refresh || is_access_token_expiring(&creds.access_token, REFRESH_SKEW_SECONDS)
    })
    .await
}

/// 401(Unauthorized) 를 받은 뒤 호출. `failed_access_token` 은 방금 요청에 썼다가
/// 거부당한 access 토큰이다.
///
/// 잠금을 잡고 파일을 다시 읽은 뒤, **그 토큰이 아직 방금 실패한 것과 같을 때만**
/// (또는 파일 토큰 자체가 만료 임박이면) 갱신한다. 같은 프로세스의 다른 에이전트가
/// 이미 갱신해 두었다면 그 새 토큰을 그대로 사용해 **중복 갱신을 피한다**
/// — 401 다발 시 N번 갱신하던 낭비를 1번으로 collapse.
pub async fn resolve_credentials_after_401(
    failed_access_token: &str,
) -> Result<CodexCredentials> {
    resolve_inner(|creds| should_refresh_after_401(&creds.access_token, failed_access_token)).await
}

/// 401 후 갱신 여부 판정 (순수 함수 — 파일/네트워크 없이 테스트 가능).
/// 파일의 현재 토큰이 방금 실패한 토큰과 같으면(=아무도 안 갱신함) 갱신.
/// 다르더라도 그 토큰이 만료 임박이면 갱신.
fn should_refresh_after_401(current_access: &str, failed_access: &str) -> bool {
    current_access == failed_access
        || is_access_token_expiring(current_access, REFRESH_SKEW_SECONDS)
}

/// Validate that `url` points at a trusted host. Allows only https + the
/// `chatgpt.com` / `openai.com` host suffixes. The user can explicitly opt out
/// of this check for self-hosting / test mocking by setting
/// `CODEX_ALLOW_INSECURE_BASE_URL=1`.
// URL 이 신뢰할 수 있는 호스트를 가리키는지 검증. 토큰을 엉뚱한 서버로 보내는 사고 방지.
pub fn validate_base_url(url: &str) -> Result<()> {
    // 환경변수로 검사 우회 허용 (자체 호스팅/테스트용).
    if std::env::var("CODEX_ALLOW_INSECURE_BASE_URL")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true")) // "1" 또는 "true"(대소문자 무시)면
        .unwrap_or(false) // 환경변수 없으면 false
    {
        return Ok(()); // 우회 → 무조건 통과
    }
    // 직접 문자열 split 대신 표준 URL 파서로 호스트를 뽑는다. 수동 파싱은
    // `https://evil.com#@chatgpt.com/...` 처럼 fragment(`#`)·query(`?`)·userinfo(`@`)
    // 를 섞으면 검증기와 실제 HTTP 클라이언트가 호스트를 다르게 해석해, 토큰이 엉뚱한
    // 서버로 새어나갈 수 있었다. 파서로 통일하고, 의심스러운 구성요소는 전부 거부한다.
    let parsed = url::Url::parse(url)
        .with_context(|| format!("base_url `{url}` is not a valid URL"))?;
    if parsed.scheme() != "https" {
        bail!(
            "base_url must be https (`{url}`). \
             To allow an insecure URL for testing, set CODEX_ALLOW_INSECURE_BASE_URL=1."
        );
    }
    // userinfo(user:pass@host) 금지 — 신뢰 호스트를 가장하는 핵심 트릭.
    if !parsed.username().is_empty() || parsed.password().is_some() {
        bail!("base_url `{url}` must not contain userinfo (user:pass@host).");
    }
    // fragment/query 금지 — `https://evil.com#@chatgpt.com` 류 우회 차단. base_url 자체엔
    // 이런 구성요소가 올 이유가 없다(경로 쿼리는 호출부에서 따로 붙인다).
    if parsed.fragment().is_some() {
        bail!("base_url `{url}` must not contain a fragment (#...).");
    }
    if parsed.query().is_some() {
        bail!("base_url `{url}` must not contain a query string (?...).");
    }
    let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();
    // 신뢰 목록 중 하나와 정확히 같거나, ".접미사" 로 끝나면 OK.
    let ok = TRUSTED_HOST_SUFFIXES
        .iter()
        .any(|suf| host == *suf || host.ends_with(&format!(".{suf}")));
    if !ok {
        bail!(
            "host `{host}` of base_url `{url}` is not in the trust list \
             (chatgpt.com, openai.com). \
             To bypass this for self-hosting / testing, set CODEX_ALLOW_INSECURE_BASE_URL=1."
        );
    }
    Ok(())
}

/// `CODEX_ALLOW_INSECURE_BASE_URL` 우회가 켜져 있는지.
fn insecure_bypass_enabled() -> bool {
    std::env::var("CODEX_ALLOW_INSECURE_BASE_URL")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// 호스트가 loopback(127.0.0.0/8, ::1, localhost)인지. 평문(http) 전송을 예외적으로
/// 허용할지 판단하는 데 쓴다(로컬 mock/proxy 는 네트워크로 나가지 않으므로 안전).
fn host_is_loopback(parsed: &url::Url) -> bool {
    match parsed.host() {
        // IP 리터럴은 std 의 loopback 판정을 그대로 쓴다(127.x.x.x, ::1).
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

/// 호스트가 신뢰 목록(chatgpt.com/openai.com)에 속하거나 loopback(127.0.0.0/8, ::1,
/// localhost)인지. 디스크 토큰 전송 목적지 검증에 쓴다.
fn host_is_trusted_or_loopback(parsed: &url::Url) -> bool {
    if host_is_loopback(parsed) {
        return true;
    }
    match parsed.host() {
        Some(url::Host::Domain(d)) => {
            let h = d.to_ascii_lowercase();
            TRUSTED_HOST_SUFFIXES
                .iter()
                .any(|suf| h == *suf || h.ends_with(&format!(".{suf}")))
        }
        _ => false,
    }
}

/// 토큰을 보낼 목적지 검증. `validate_base_url` 의 형식/신뢰 검사에 더해, **디스크에서
/// 로드한 진짜 토큰**(`from_disk == true`)은 `CODEX_ALLOW_INSECURE_BASE_URL` 우회가
/// 켜져 있어도 신뢰 호스트(또는 loopback)로만 보낸다.
///
/// 이유: 우회 플래그는 전역 스위치라, 테스트/셀프호스팅 후 끄는 걸 깜빡한 채
/// `CODEX_BASE_URL=https://evil.com` 같은 값이 설정되면 디스크의 진짜 ChatGPT 구독
/// 토큰이 임의 원격 호스트로 그대로 새어나갈 수 있다. 우회는 직접 주입한(비-디스크)
/// 자격증명에만 전면 적용하고, 디스크 토큰엔 loopback/신뢰 호스트 가드를 유지한다.
/// (localhost mock/proxy 는 계속 동작, 원격 유출만 차단.)
pub fn validate_token_destination(creds: &CodexCredentials) -> Result<()> {
    // 1) 항상 기존 형식/신뢰 검사를 통과해야 한다(우회 플래그는 여기서 존중됨).
    validate_base_url(&creds.base_url)?;
    // 2) 디스크 토큰 + 우회 ON 인 경우에만 추가 가드. (우회 OFF 면 1)에서 이미 신뢰 호스트만 통과)
    if creds.from_disk && insecure_bypass_enabled() {
        guard_disk_token_under_bypass(&creds.base_url)?;
    }
    Ok(())
}

/// 디스크 토큰 + `CODEX_ALLOW_INSECURE_BASE_URL` 우회가 켜졌을 때의 추가 가드.
/// env 를 읽지 않는 순수 함수라 단위 테스트가 가능하다. 두 가지를 강제한다:
/// 1) 호스트는 신뢰 목록(chatgpt.com/openai.com) 또는 loopback 이어야 한다.
/// 2) 비-loopback 호스트로는 https 만 허용한다 — `validate_base_url` 의 https 강제는
///    우회 플래그가 켜지면 통째로 건너뛰므로, 여기서 막지 않으면 `http://chatgpt.com`
///    으로 진짜 구독 토큰이 평문 전송될 수 있다. (loopback 의 로컬 mock/proxy 만 http 허용.)
fn guard_disk_token_under_bypass(base_url: &str) -> Result<()> {
    let parsed = url::Url::parse(base_url)
        .with_context(|| format!("base_url `{base_url}` is not a valid URL"))?;
    if !host_is_trusted_or_loopback(&parsed) {
        bail!(
            "refusing to send a disk-loaded ChatGPT token to non-trusted host `{}` \
             even though CODEX_ALLOW_INSECURE_BASE_URL is set. The insecure override \
             only fully applies to explicitly injected credentials, not to tokens \
             auto-loaded from ~/.codex/auth.json. Use a loopback/trusted host, or \
             inject credentials directly.",
            parsed.host_str().unwrap_or("")
        );
    }
    if parsed.scheme() != "https" && !host_is_loopback(&parsed) {
        bail!(
            "refusing to send a disk-loaded ChatGPT token over cleartext `{}` \
             (scheme `{}`) even though CODEX_ALLOW_INSECURE_BASE_URL is set. \
             Disk-loaded tokens require https for non-loopback hosts. Use https, \
             a loopback host, or inject credentials directly.",
            base_url,
            parsed.scheme()
        );
    }
    Ok(())
}

// 베이스 URL 결정: 환경변수 CODEX_BASE_URL 이 있으면 그걸, 없으면 기본 상수.
fn default_base_url() -> String {
    std::env::var("CODEX_BASE_URL")
        .ok()                                   // Result → Option (에러는 None 으로)
        .filter(|s| !s.trim().is_empty())       // 비어있으면 버림(None)
        .map(|s| s.trim_end_matches('/').to_string()) // 끝의 '/' 제거
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string()) // 위 과정에서 없으면 기본값
}

// 현재 시각을 "2024-01-02T03:04:05Z" 같은 ISO 문자열로. last_refresh 기록용 메타데이터.
fn now_iso() -> String {
    // If the system clock is pre-epoch we report 0 (1970-01-01). This is only
    // metadata so we do not surface that as a fatal error here.
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)  // 1970 기준 경과 시간
        .map(|d| d.as_secs() as i64) // 초(정수)로 변환
        .unwrap_or(0);               // 시계가 이상하면 0
    let (year, month, day, hour, min, sec) = epoch_to_ymdhms(secs); // 튜플을 6개 변수로 분해
    // {year:04} = 4자리 0채움. {month:02} = 2자리 0채움. 예: 2024-01-02T03:04:05Z
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
}

/// epoch seconds -> (year, month, day, hour, min, sec).
/// Clamped to `[0, 9999-12-31T23:59:59Z]`. Used only for metadata, so we
/// prioritize panic / overflow safety over absolute precision.
// 1970 기준 경과 초 → (연,월,일,시,분,초) 튜플. 외부 날짜 라이브러리 없이 직접 계산.
fn epoch_to_ymdhms(epoch: i64) -> (i32, u32, u32, u32, u32, u32) {
    const MAX_EPOCH: i64 = 253_402_300_799; // 9999-12-31T23:59:59Z (`_`=자릿수 구분, 무시됨)
    let epoch = epoch.clamp(0, MAX_EPOCH); // 0~최대 사이로 가둠 (오버플로 방지)
    // div_euclid=나눗셈 몫, rem_euclid=나머지 (음수에도 안전). 86400=하루 초.
    let mut days = epoch.div_euclid(86_400);          // 총 며칠
    let mut secs_in_day = epoch.rem_euclid(86_400) as u32; // 그날 안의 초
    let hour = secs_in_day / 3600; // 시 = 초 / 3600
    secs_in_day %= 3600;           // `%=` 나머지 대입: 시간 부분 제거
    let min = secs_in_day / 60;    // 분
    let sec = secs_in_day % 60;    // 초

    let mut year = 1970i32; // 1970 부터 시작 (i32 접미사로 타입 명시)
    loop {
        let dy = if is_leap(year) { 366 } else { 365 }; // 그 해 일수 (if 가 값을 반환)
        if days < dy { // 남은 날이 한 해보다 적으면 그 해 안에 있음
            break;
        }
        days -= dy; // 한 해만큼 빼고
        year += 1;  // 다음 해로
    }
    // 각 달의 일수 (평년/윤년 두 벌)
    let months_normal = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let months_leap = [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let months = if is_leap(year) { months_leap } else { months_normal };
    let mut month = 1u32;
    for m in months { // 달 배열을 순회하며 남은 날에서 한 달씩 빼기
        if days < m {
            break;
        }
        days -= m;
        month += 1;
    }
    let day = days as u32 + 1; // 남은 날 + 1 = 일 (1일부터 시작이라 +1)
    (year, month, day, hour, min, sec) // 튜플로 반환
}

// 윤년인가? 4의 배수면서 (100의 배수가 아니거나 400의 배수). 반환 bool.
fn is_leap(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0) // % = 나머지, && = AND, || = OR
}

// ──────────────────────────────────────────────────────────────────────
// 테스트 — `cargo test` 로 실행. 네트워크/로그인 없이 순수 함수만 검증한다.
// ──────────────────────────────────────────────────────────────────────

// `#[cfg(test)]` = "테스트로 컴파일할 때만 이 모듈을 포함하라".
// 그래서 실제 배포 바이너리에는 아래 코드가 들어가지 않는다.
#[cfg(test)]
mod tests {
    // `super` = 바로 위(부모) 모듈, 즉 이 파일 자체.
    // `use super::*;` 로 auth.rs 안의 (비공개 함수 포함) 모든 항목을 끌어온다.
    // 유닛 테스트가 같은 파일 안에 있으면 `pub` 이 아닌 함수도 테스트할 수 있다.
    use super::*;

    #[test]
    fn leap_year_detection() {
        assert!(is_leap(2024)); // 4의 배수
        assert!(is_leap(2000)); // 400의 배수
        assert!(!is_leap(2023)); // 평년
        assert!(!is_leap(1900)); // 100의 배수지만 400의 배수는 아님 → 평년
    }

    #[test]
    fn epoch_to_ymdhms_works() {
        // 0초 = 유닉스 시간의 시작점.
        assert_eq!(epoch_to_ymdhms(0), (1970, 1, 1, 0, 0, 0));
        // 3661초 = 1시간 1분 1초.
        assert_eq!(epoch_to_ymdhms(3661), (1970, 1, 1, 1, 1, 1));
        // 86400초 = 정확히 하루 → 다음 날.
        assert_eq!(epoch_to_ymdhms(86_400), (1970, 1, 2, 0, 0, 0));
        // 1월(31일)을 다 넘기면 2월 1일.
        assert_eq!(epoch_to_ymdhms(31 * 86_400), (1970, 2, 1, 0, 0, 0));
    }

    #[test]
    fn base_url_only_trusted_hosts() {
        // 신뢰 목록(chatgpt.com / openai.com)은 통과.
        assert!(validate_base_url("https://chatgpt.com/backend-api/codex").is_ok());
        assert!(validate_base_url("https://auth.openai.com").is_ok());
        // https 가 아니면 거부.
        assert!(validate_base_url("http://chatgpt.com").is_err());
        // 신뢰 목록 밖 호스트는 거부.
        assert!(validate_base_url("https://evil.com").is_err());
        // 호스트를 흉내 낸 도메인(suffix 트릭)도 거부.
        assert!(validate_base_url("https://chatgpt.com.evil.com").is_err());
    }

    #[test]
    fn oauth_error_body_parsing() {
        // OpenAI 형태: {"error": {"code": ..., "message": ...}}
        let (code, msg) =
            parse_oauth_error(r#"{"error":{"code":"invalid_grant","message":"nope"}}"#);
        assert_eq!(code.as_deref(), Some("invalid_grant"));
        assert!(msg.unwrap().contains("nope"));

        // 평범한 OAuth 형태: {"error": "코드", "error_description": "..."}
        let (code, _) =
            parse_oauth_error(r#"{"error":"invalid_token","error_description":"expired"}"#);
        assert_eq!(code.as_deref(), Some("invalid_token"));

        // JSON 이 아니면 둘 다 None.
        let (code, msg) = parse_oauth_error("이건 JSON 이 아님");
        assert!(code.is_none() && msg.is_none());
    }

    /// 테스트용 가짜 JWT 만들기 — `header.payload.sig` 모양에서 가운데(payload)만
    /// 의미가 있다. base64url 로 인코딩한 JSON 을 끼워 넣는다.
    fn make_jwt(payload: &serde_json::Value) -> String {
        let body = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(payload).unwrap());
        format!("aaa.{body}.sig")
    }

    #[test]
    fn jwt_account_id_extraction() {
        let jwt = make_jwt(&serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": "acc_123" }
        }));
        let creds = CodexCredentials {
            access_token: jwt,
            refresh_token: "r".into(),
            base_url: DEFAULT_BASE_URL.into(),
            last_refresh: None,
            from_disk: false,
        };
        assert_eq!(creds.chatgpt_account_id().as_deref(), Some("acc_123"));
    }

    #[test]
    fn token_expiry_detection() {
        // exp 가 아주 먼 미래 → 만료 임박 아님(false).
        let future = make_jwt(&serde_json::json!({ "exp": 9_999_999_999_i64 }));
        assert!(!is_access_token_expiring(&future, 120));

        // exp 가 과거(0) → 만료됨(true).
        let past = make_jwt(&serde_json::json!({ "exp": 0 }));
        assert!(is_access_token_expiring(&past, 120));

        // 형식이 깨진 토큰 → 안전하게 만료 취급(true).
        assert!(is_access_token_expiring("not-a-jwt", 120));
    }

    #[test]
    fn refresh_after_401_compare_and_skip() {
        let stale = make_jwt(&serde_json::json!({ "exp": 0 })); // 방금 실패한(만료된) 토큰
        let fresh = make_jwt(&serde_json::json!({ "exp": 9_999_999_999_i64 })); // 갓 갱신된 신선한 토큰

        // 파일 토큰이 아직 방금 실패한 것과 같음 → 아무도 안 갱신함 → 갱신해야 함.
        assert!(should_refresh_after_401(&stale, &stale));

        // 파일 토큰이 다르고 신선함 → 다른 태스크가 이미 갱신함 → 스킵(중복 방지).
        assert!(!should_refresh_after_401(&fresh, &stale));

        // 파일 토큰이 다르지만 그것도 만료 임박이면 → 갱신.
        let other_expiring = make_jwt(&serde_json::json!({ "exp": 0 }));
        assert!(should_refresh_after_401(&other_expiring, "completely-different-token"));
    }

    #[test]
    fn codex_home_must_be_absolute() {
        // 절대 경로는 통과하고 auth.json 이 붙는다.
        #[cfg(unix)]
        let abs = "/tmp/some/codex/home";
        #[cfg(not(unix))]
        let abs = "C:\\codex\\home";
        let p = codex_home_to_auth_path(abs).unwrap();
        assert!(p.is_absolute());
        assert_eq!(p.file_name().unwrap(), "auth.json");

        // 상대 경로는 거부 — 토큰이 cwd 에 떨어지는 사고 방지.
        assert!(codex_home_to_auth_path(".codex").is_err());
        assert!(codex_home_to_auth_path("relative/dir").is_err());
    }

    #[test]
    fn token_destination_host_classification() {
        let trusted_or_loop = |u: &str| {
            host_is_trusted_or_loopback(&url::Url::parse(u).unwrap())
        };
        // 신뢰 호스트.
        assert!(trusted_or_loop("https://chatgpt.com/backend-api/codex"));
        assert!(trusted_or_loop("https://auth.openai.com"));
        // loopback (IPv4/IPv6/localhost) — mock/proxy 테스트가 계속 동작해야 함.
        assert!(trusted_or_loop("http://127.0.0.1:8080/backend-api/codex"));
        assert!(trusted_or_loop("http://localhost:3000"));
        assert!(trusted_or_loop("http://[::1]:9000"));
        // 임의 원격 호스트 / suffix 트릭은 거부.
        assert!(!trusted_or_loop("https://evil.com"));
        assert!(!trusted_or_loop("https://chatgpt.com.evil.com"));
        assert!(!trusted_or_loop("http://10.0.0.5")); // 사설망이지만 loopback 아님
    }

    #[test]
    fn base_url_rejects_evasion_tricks() {
        // fragment 우회: 수동 split 파서는 chatgpt.com 으로 오인했지만 실제 호스트는 evil.com.
        assert!(validate_base_url("https://evil.com#@chatgpt.com/backend-api/codex").is_err());
        // userinfo 트릭 (user@host / user:pass@host).
        assert!(validate_base_url("https://chatgpt.com@evil.com").is_err());
        assert!(validate_base_url("https://user:pass@chatgpt.com").is_err());
        // query 우회.
        assert!(validate_base_url("https://evil.com?host=chatgpt.com").is_err());
        // 아예 URL 이 아님.
        assert!(validate_base_url("not a url").is_err());
        // 정상 케이스는 여전히 통과.
        assert!(validate_base_url("https://chatgpt.com/backend-api/codex").is_ok());
    }

    #[test]
    fn redacted_debug_hides_tokens() {
        let creds = CodexCredentials {
            access_token: "SUPER_SECRET_ACCESS".into(),
            refresh_token: "SUPER_SECRET_REFRESH".into(),
            base_url: DEFAULT_BASE_URL.into(),
            last_refresh: None,
            from_disk: false,
        };
        let dumped = format!("{creds:?}");
        // 토큰 원문은 절대 나오면 안 됨.
        assert!(!dumped.contains("SUPER_SECRET_ACCESS"), "access token leaked: {dumped}");
        assert!(!dumped.contains("SUPER_SECRET_REFRESH"), "refresh token leaked: {dumped}");
        assert!(dumped.contains("[redacted]"));
        // 비밀 아닌 필드는 보여야 디버깅에 쓸모 있음.
        assert!(dumped.contains(DEFAULT_BASE_URL));
    }

    #[test]
    fn load_rejects_corrupt_file() {
        use std::io::Write;
        // CODEX_HOME 을 유니크한 임시 디렉토리로 가리킨 뒤 깨진 JSON 을 심는다.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("codex-oauth-test-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let auth = dir.join("auth.json");
        std::fs::File::create(&auth)
            .unwrap()
            .write_all(b"{ this is not valid json")
            .unwrap();

        // edition 2024: set_var/remove_var 는 unsafe. 이 테스트 외엔 CODEX_HOME 을 읽지 않는다.
        unsafe { std::env::set_var("CODEX_HOME", &dir) };
        let result = load_codex_cli_tokens();
        unsafe { std::env::remove_var("CODEX_HOME") };
        let _ = std::fs::remove_dir_all(&dir);

        // 손상 파일은 조용히 None 이 아니라 명확한 Err 여야 한다(save 와 일치).
        assert!(result.is_err(), "corrupt token store should be an error");
    }

    #[test]
    fn disk_token_under_bypass_blocks_cleartext() {
        // [P1] 회귀 가드: 우회 플래그가 켜져 있어도 디스크 토큰을 평문(http)으로,
        // 신뢰 호스트라 해도 절대 보내지 않는다.
        assert!(
            guard_disk_token_under_bypass("http://chatgpt.com/backend-api/codex").is_err(),
            "cleartext http to a trusted host must be refused for disk tokens"
        );
        assert!(
            guard_disk_token_under_bypass("http://auth.openai.com").is_err(),
            "cleartext http to openai.com must be refused for disk tokens"
        );
        // https 신뢰 호스트는 허용.
        assert!(guard_disk_token_under_bypass("https://chatgpt.com/backend-api/codex").is_ok());
        // loopback 은 로컬 mock/proxy 를 위해 http 라도 허용.
        assert!(guard_disk_token_under_bypass("http://127.0.0.1:8080").is_ok());
        assert!(guard_disk_token_under_bypass("http://localhost:3000").is_ok());
        // 신뢰 목록 밖 호스트는 https 여도 거부(기존 가드).
        assert!(guard_disk_token_under_bypass("https://evil.com").is_err());
    }

    #[test]
    fn auth_file_without_tokens_is_not_corrupt() {
        // [P2] 회귀 가드: tokens 키가 없는 유효 JSON(예: OPENAI_API_KEY 만 있는 파일)은
        // 손상이 아니라 빈 토큰으로 파싱돼야 한다 → load 가 Ok(None) 을 내고 첫 로그인이
        // 다른 클라의 보존 필드를 클로버하지 않는다.
        let f: AuthFile = serde_json::from_str(r#"{"OPENAI_API_KEY":"sk-test"}"#).unwrap();
        assert!(f.tokens.access_token.is_empty());
        assert!(f.tokens.refresh_token.is_empty());
        assert_eq!(f.openai_api_key.as_deref(), Some("sk-test"));
        // 부분 tokens(access 만 존재) 도 손상 아님.
        let f2: AuthFile = serde_json::from_str(r#"{"tokens":{"access_token":"a"}}"#).unwrap();
        assert_eq!(f2.tokens.access_token, "a");
        assert!(f2.tokens.refresh_token.is_empty());
        // 구문이 깨진 JSON 은 여전히 파싱 실패(손상으로 취급).
        assert!(serde_json::from_str::<AuthFile>("{ not valid json").is_err());
    }
}
