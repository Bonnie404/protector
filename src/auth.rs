//! Google OAuth: the installed-app flow (PKCE over a loopback redirect), the
//! token endpoint, and the store that keeps the refresh token between runs.
//!
//! Nothing in this module ever prints, logs or formats a token. `Tokens` has a
//! hand-written `Debug` that redacts both halves, and the token endpoint's error
//! bodies are dropped rather than attached to the error, because Google echoes
//! the offending refresh token back inside `error_description`.

use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration as StdDuration;

use base64::Engine;
use chrono::{DateTime, Duration, Local};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::config::Config;

/// Read-only, events-only. Protector is deliberately incapable of changing a
/// calendar: widening this is a security decision, not a convenience.
pub const SCOPE: &str = "https://www.googleapis.com/auth/calendar.events.readonly";
const AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";

/// How long `login` waits for the browser to come back before giving up.
const CONSENT_TIMEOUT: StdDuration = StdDuration::from_secs(300);
/// How long any single Secret Service call may take before it is abandoned.
const KEYRING_TIMEOUT: StdDuration = StdDuration::from_secs(10);
/// The probe in `token_store` runs before the user has asked for anything, so
/// it gets far less patience than a call they explicitly triggered.
const KEYRING_PROBE_TIMEOUT: StdDuration = StdDuration::from_secs(3);

// ---------------------------------------------------------------------------
// PKCE and the authorization URL
// ---------------------------------------------------------------------------

pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

fn random_urlsafe(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    // Linux-only tool; /dev/urandom avoids a dependency for 32 bytes.
    use std::io::Read;
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .expect("/dev/urandom must be readable to generate PKCE and CSRF values");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

pub fn pkce() -> Pkce {
    let verifier = random_urlsafe(48);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    Pkce { verifier, challenge }
}

pub fn authorize_url(cfg: &Config, redirect: &str, p: &Pkce, state: &str) -> String {
    authorize_url_at(AUTH_ENDPOINT, cfg, redirect, p, state)
}

/// Note what is *not* here: `client_secret`. It belongs in the token POST only,
/// never in a URL that is handed to a browser, logged by it, and left in history.
fn authorize_url_at(endpoint: &str, cfg: &Config, redirect: &str, p: &Pkce, state: &str) -> String {
    let query = serde_urlencoded::to_string([
        ("client_id", cfg.client_id.as_str()),
        ("redirect_uri", redirect),
        ("response_type", "code"),
        ("scope", SCOPE),
        ("code_challenge", p.challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("access_type", "offline"),
        ("prompt", "consent"),
        ("state", state),
    ])
    .expect("static query pairs encode");
    format!("{endpoint}?{query}")
}

pub fn parse_callback(request: &str, expected_state: &str) -> anyhow::Result<String> {
    let target = request
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("malformed request line"))?;
    let pairs = query_pairs(target)?;
    let get = |k: &str| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    if let Some(err) = get("error") {
        anyhow::bail!("authorization denied: {err}");
    }
    // The CSRF check: anything on this machine can reach the loopback port, so a
    // callback that does not carry back the 128 random bits we just minted is
    // somebody else's, and its code is not trusted.
    if get("state").as_deref() != Some(expected_state) {
        anyhow::bail!("state mismatch — refusing the callback");
    }
    get("code").ok_or_else(|| anyhow::anyhow!("no code in callback"))
}

fn query_pairs(target: &str) -> anyhow::Result<Vec<(String, String)>> {
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
    Ok(serde_urlencoded::from_str(query)?)
}

// ---------------------------------------------------------------------------
// The loopback listener
// ---------------------------------------------------------------------------

const SUCCESS_PAGE: &str =
    "<html><body><h2>Protector is connected.</h2><p>You can close this tab.</p></body></html>";
const FAILURE_PAGE: &str =
    "<html><body><h2>Connection failed.</h2><p>Check the terminal.</p></body></html>";

fn http_response(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    )
}

/// Reads one HTTP request head, stopping at the blank line that ends it. A
/// single `read` is not enough: the request line can be split across packets,
/// and Chrome in particular likes to send its headers separately.
async fn read_request_head(stream: &mut tokio::net::TcpStream) -> anyhow::Result<String> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break; // The peer hung up; parse whatever arrived.
        }
        buf.extend_from_slice(&chunk[..n]);
        // A callback URL is a few hundred bytes. Anything larger is not the
        // browser we are waiting for, and is not worth buffering.
        if buf.len() > 8192 {
            anyhow::bail!("callback request head too large");
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Waits for the browser's redirect on an already-bound loopback listener and
/// returns the authorization code.
///
/// Connections that carry no query string at all — speculative preconnects,
/// `/favicon.ico` — are answered with a 404 and do not consume the wait: the
/// real redirect may well arrive on the second or third connection.
async fn accept_callback(
    listener: &tokio::net::TcpListener,
    expected_state: &str,
    wait: StdDuration,
) -> anyhow::Result<String> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let (mut stream, _) = tokio::time::timeout_at(deadline, listener.accept())
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for the browser"))??;
        let request = match tokio::time::timeout_at(deadline, read_request_head(&mut stream)).await {
            Ok(Ok(r)) => r,
            // A connection that opens and says nothing must not end the login.
            Ok(Err(_)) | Err(_) => continue,
        };
        let is_callback = request
            .split_whitespace()
            .nth(1)
            .map(|t| t.contains('?'))
            .unwrap_or(false);
        if !is_callback {
            let _ = stream.write_all(http_response("404 Not Found", FAILURE_PAGE).as_bytes()).await;
            let _ = stream.shutdown().await;
            continue;
        }
        let result = parse_callback(&request, expected_state);
        let page = if result.is_ok() { SUCCESS_PAGE } else { FAILURE_PAGE };
        // The browser is told the outcome before the error propagates, so the
        // user sees a page rather than a connection reset.
        let _ = stream.write_all(http_response("200 OK", page).as_bytes()).await;
        let _ = stream.shutdown().await;
        return result;
    }
}

// ---------------------------------------------------------------------------
// The token endpoint
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
}

#[derive(Clone)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: DateTime<Local>,
}

/// Hand-written so a stray `{:?}` — in a log line, an `anyhow` context, a panic
/// message — can never spill the credentials this struct exists to carry.
impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tokens")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &self.refresh_token.as_ref().map(|_| "<redacted>"))
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl From<TokenResponse> for Tokens {
    fn from(r: TokenResponse) -> Self {
        Tokens {
            access_token: r.access_token,
            refresh_token: r.refresh_token,
            // 60s of slack so a request never starts with an almost-expired token.
            expires_at: Local::now() + Duration::seconds(r.expires_in - 60),
        }
    }
}

/// The short OAuth failure code — `invalid_grant`, `invalid_client` — and
/// deliberately *not* `error_description`, which Google fills with a sentence
/// that quotes the refresh token it just rejected.
#[derive(Deserialize)]
struct TokenError {
    error: Option<String>,
}

async fn post_token(endpoint: &str, form: &[(&str, &str)]) -> anyhow::Result<Tokens> {
    let response = reqwest::Client::new().post(endpoint).form(form).send().await?;
    let status = response.status();
    if !status.is_success() {
        let code = response
            .json::<TokenError>()
            .await
            .ok()
            .and_then(|e| e.error)
            .map(|e| format!(": {e}"))
            .unwrap_or_default();
        anyhow::bail!("the Google token endpoint refused the request ({status}{code})");
    }
    let parsed: TokenResponse = response.json().await?;
    Ok(parsed.into())
}

async fn exchange_code_at(
    cfg: &Config,
    code: &str,
    redirect: &str,
    verifier: &str,
    endpoint: &str,
) -> anyhow::Result<Tokens> {
    post_token(
        endpoint,
        &[
            ("code", code),
            ("client_id", cfg.client_id.as_str()),
            ("client_secret", cfg.client_secret.as_str()),
            ("redirect_uri", redirect),
            ("grant_type", "authorization_code"),
            ("code_verifier", verifier),
        ],
    )
    .await
}

async fn refresh_at(cfg: &Config, refresh_token: &str, endpoint: &str) -> anyhow::Result<Tokens> {
    let mut tokens = post_token(
        endpoint,
        &[
            ("client_id", cfg.client_id.as_str()),
            ("client_secret", cfg.client_secret.as_str()),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ],
    )
    .await?;
    // A refresh normally returns no new refresh token. Dropping the one we hold
    // because the response omitted it would log the user out at the next start.
    tokens.refresh_token.get_or_insert_with(|| refresh_token.to_string());
    Ok(tokens)
}

pub async fn refresh(cfg: &Config, refresh_token: &str) -> anyhow::Result<Tokens> {
    refresh_at(cfg, refresh_token, TOKEN_ENDPOINT).await
}

pub async fn login(cfg: &Config) -> anyhow::Result<Tokens> {
    login_at(cfg, AUTH_ENDPOINT, TOKEN_ENDPOINT).await
}

async fn login_at(cfg: &Config, auth_endpoint: &str, token_endpoint: &str) -> anyhow::Result<Tokens> {
    // Bound before the browser is launched: the port has to be known to build
    // the redirect URI, and the socket has to be listening before the user can
    // possibly finish consenting.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let redirect = format!("http://127.0.0.1:{port}");
    let p = pkce();
    let state = random_urlsafe(16);

    let url = authorize_url_at(auth_endpoint, cfg, &redirect, &p, &state);
    println!("If your browser did not open, visit:\n{url}");
    // Spawned, never awaited: on several desktops `xdg-open` does not return
    // until the browser it started exits, so waiting for it here would leave the
    // loopback listener unread for the rest of the session and time the login
    // out even though the user consented straight away.
    let _ = tokio::process::Command::new("xdg-open").arg(&url).spawn();

    let code = accept_callback(&listener, &state, CONSENT_TIMEOUT).await?;
    exchange_code_at(cfg, &code, &redirect, &p.verifier, token_endpoint).await
}

// ---------------------------------------------------------------------------
// Token storage
// ---------------------------------------------------------------------------

pub trait TokenStore: Send + Sync {
    fn save(&self, token: &str) -> anyhow::Result<()>;
    fn load(&self) -> anyhow::Result<Option<String>>;
    fn clear(&self) -> anyhow::Result<()>;
    fn describe(&self) -> &'static str;
}

/// Runs a blocking Secret Service call on a thread of its own and abandons it
/// when it overruns.
///
/// This exists because a locked collection turns `get_password` into a modal
/// unlock dialog on the user's desktop, and that call does not return until the
/// dialog is answered — which may be never, if the screen is locked or the
/// session is remote. Abandoning the thread leaks it until the prompt resolves;
/// that is a far better outcome than a panel widget that never starts.
fn guarded<T: Send + 'static>(
    timeout: StdDuration,
    what: &'static str,
    op: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(op());
    });
    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(_) => anyhow::bail!(
            "the keyring did not answer within {}s while trying to {what} — it may be locked",
            timeout.as_secs()
        ),
    }
}

pub struct KeyringStore;

impl KeyringStore {
    fn entry() -> anyhow::Result<keyring::Entry> {
        Ok(keyring::Entry::new("protector", "google-refresh-token")?)
    }

    fn load_within(timeout: StdDuration) -> anyhow::Result<Option<String>> {
        guarded(timeout, "read the token", || match Self::entry()?.get_password() {
            Ok(t) => Ok(Some(t)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.into()),
        })
    }
}

impl TokenStore for KeyringStore {
    fn save(&self, token: &str) -> anyhow::Result<()> {
        let token = token.to_string();
        guarded(KEYRING_TIMEOUT, "store the token", move || {
            Self::entry()?.set_password(&token)?;
            Ok(())
        })
    }
    fn load(&self) -> anyhow::Result<Option<String>> {
        Self::load_within(KEYRING_TIMEOUT)
    }
    fn clear(&self) -> anyhow::Result<()> {
        guarded(KEYRING_TIMEOUT, "forget the token", || {
            match Self::entry()?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(e) => Err(e.into()),
            }
        })
    }
    fn describe(&self) -> &'static str {
        "GNOME keyring"
    }
}

pub struct FileStore(pub PathBuf);

impl TokenStore for FileStore {
    /// Written to a sibling temp file and renamed over the target, the same way
    /// `state::save` works. A truncate-then-write would turn a crash mid-save
    /// into a lost refresh token and a forced re-login.
    fn save(&self, token: &str) -> anyhow::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(parent) = self.0.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.0.with_extension("tmp");
        // 0600 on the temp file too: the secret is in it from the first write,
        // so it must never exist as a world-readable file, not even briefly.
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        let write = f.write_all(token.as_bytes()).and_then(|()| f.sync_all());
        if let Err(e) = write {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        drop(f);
        if let Err(e) = std::fs::rename(&tmp, &self.0) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(())
    }
    fn load(&self) -> anyhow::Result<Option<String>> {
        match std::fs::read_to_string(&self.0) {
            // Trimmed: a hand-edited file gains a trailing newline, and an
            // empty file is "no token", not a token that happens to be blank.
            Ok(t) if t.trim().is_empty() => Ok(None),
            Ok(t) => Ok(Some(t.trim().to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    fn clear(&self) -> anyhow::Result<()> {
        match std::fs::remove_file(&self.0) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
    fn describe(&self) -> &'static str {
        "file (0600)"
    }
}

pub fn token_file_path() -> PathBuf {
    crate::state::state_path().with_file_name("token.json")
}

/// True when a session bus could plausibly exist. Without one there is no Secret
/// Service to talk to, so the keyring probe below would only burn a timeout.
fn session_bus_present() -> bool {
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some() {
        return true;
    }
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(|d| PathBuf::from(d).join("bus").exists())
        .unwrap_or(false)
}

/// Prefers the keyring; falls back to a 0600 file when Secret Service is absent
/// or does not answer.
///
/// The probe is a `load()`, which is prompt-free for the case that matters — a
/// user who has never logged in has no stored item, so the Secret Service
/// answers `NoEntry` without unlocking anything. A user who *has* logged in and
/// whose collection is locked will see their desktop's unlock dialog here; the
/// watchdog in `guarded` bounds how long Protector waits for it.
///
/// Blocking: call this from `tokio::task::spawn_blocking` in async code.
pub fn token_store() -> Box<dyn TokenStore> {
    if session_bus_present() && KeyringStore::load_within(KEYRING_PROBE_TIMEOUT).is_ok() {
        Box::new(KeyringStore)
    } else {
        Box::new(FileStore(token_file_path()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config {
            client_id: "abc.apps.googleusercontent.com".into(),
            client_secret: "s3cret".into(),
            calendar_id: "primary".into(),
            warn_before_minutes: 5,
        }
    }

    // ---- PKCE, the authorization URL and the callback -----------------------

    #[test]
    fn pkce_pairs_are_random_and_correctly_derived() {
        let a = pkce();
        let b = pkce();
        assert_ne!(a.verifier, b.verifier);
        assert!(a.verifier.len() >= 43 && a.verifier.len() <= 128);
        // challenge = BASE64URL(SHA256(verifier)), no padding
        use base64::Engine;
        use sha2::{Digest, Sha256};
        let expected = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(a.verifier.as_bytes()));
        assert_eq!(a.challenge, expected);
        assert!(!a.challenge.contains('='));
    }

    #[test]
    fn the_requested_scope_is_read_only() {
        assert_eq!(SCOPE, "https://www.googleapis.com/auth/calendar.events.readonly");
    }

    #[test]
    fn the_authorize_url_requests_read_only_offline_access() {
        let p = pkce();
        let url = authorize_url(&cfg(), "http://127.0.0.1:41234", &p, "xyz");
        assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));
        assert!(url.contains("scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fcalendar.events.readonly"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("access_type=offline"));
        assert!(url.contains("state=xyz"));
        assert!(!url.contains("client_secret"));
        assert!(!url.contains("s3cret"));
    }

    #[test]
    fn the_loopback_reply_extracts_the_code_and_checks_the_state() {
        let req = "GET /?state=xyz&code=4/abc123 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
        assert_eq!(parse_callback(req, "xyz").unwrap(), "4/abc123");
        assert!(parse_callback(req, "other").is_err());
        let denied = "GET /?error=access_denied&state=xyz HTTP/1.1\r\n\r\n";
        assert!(parse_callback(denied, "xyz").is_err());
    }

    #[test]
    fn a_callback_without_a_code_is_an_error_rather_than_an_empty_code() {
        let req = "GET /?state=xyz HTTP/1.1\r\n\r\n";
        assert!(parse_callback(req, "xyz").is_err());
    }

    // ---- The loopback listener ---------------------------------------------

    async fn send(port: u16, request: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        // The server sets `Connection: close` and drops the socket, so reading
        // to EOF here is what proves the reply was complete and terminated.
        s.read_to_string(&mut response).await.unwrap();
        response
    }

    fn assert_well_formed_http(response: &str) {
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "status line: {response:?}");
        let (head, body) = response.split_once("\r\n\r\n").expect("headers end with a blank line");
        let declared: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length: "))
            .expect("a Content-Length header")
            .parse()
            .unwrap();
        assert_eq!(declared, body.len(), "Content-Length must match the body");
        assert!(head.contains("Content-Type: text/html"));
    }

    #[tokio::test]
    async fn the_loopback_listener_receives_the_code_and_answers_the_browser() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let browser = tokio::spawn(async move {
            send(port, "GET /?state=st8&code=4%2Fabc123 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await
        });
        let code = accept_callback(&listener, "st8", std::time::Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(code, "4/abc123");
        let response = browser.await.unwrap();
        assert_well_formed_http(&response);
        assert!(response.contains("connected"), "success page: {response:?}");
    }

    #[tokio::test]
    async fn the_loopback_listener_ignores_unrelated_requests_and_keeps_waiting() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let browser = tokio::spawn(async move {
            // Browsers routinely open a speculative connection or ask for a
            // favicon before (or alongside) the redirect. Neither carries the
            // callback, and neither may consume the one accept the login gets.
            let noise = send(port, "GET /favicon.ico HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await;
            let real = send(port, "GET /?state=st8&code=ok HTTP/1.1\r\n\r\n").await;
            (noise, real)
        });
        let code = accept_callback(&listener, "st8", std::time::Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(code, "ok");
        let (noise, real) = browser.await.unwrap();
        assert!(noise.starts_with("HTTP/1.1 404 Not Found\r\n"), "noise: {noise:?}");
        assert_well_formed_http(&real);
    }

    #[tokio::test]
    async fn the_loopback_listener_rejects_a_forged_state_and_still_answers() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let browser = tokio::spawn(async move {
            send(port, "GET /?state=forged&code=evil HTTP/1.1\r\n\r\n").await
        });
        let err = accept_callback(&listener, "st8", std::time::Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("state"), "error was: {err:#}");
        let response = browser.await.unwrap();
        assert_well_formed_http(&response);
        assert!(response.contains("failed"), "failure page: {response:?}");
    }

    #[tokio::test]
    async fn the_loopback_listener_gives_up_instead_of_waiting_forever() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let err = accept_callback(&listener, "st8", std::time::Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("timed out"), "error was: {err:#}");
    }

    // ---- The token endpoint -------------------------------------------------

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    fn form(req: &Request) -> std::collections::HashMap<String, String> {
        serde_urlencoded::from_bytes::<Vec<(String, String)>>(&req.body)
            .expect("the token request must be form-encoded")
            .into_iter()
            .collect()
    }

    async fn token_server(body: serde_json::Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn the_code_exchange_posts_the_verifier_and_the_client_credentials() {
        let server = token_server(serde_json::json!({
            "access_token": "ya29.access",
            "refresh_token": "1//refresh",
            "expires_in": 3599,
            "token_type": "Bearer",
        }))
        .await;

        let tokens = exchange_code_at(
            &cfg(),
            "4/thecode",
            "http://127.0.0.1:41234",
            "the-verifier",
            &format!("{}/token", server.uri()),
        )
        .await
        .unwrap();

        assert_eq!(tokens.access_token, "ya29.access");
        assert_eq!(tokens.refresh_token.as_deref(), Some("1//refresh"));
        assert!(tokens.expires_at > Local::now(), "expiry must be in the future");
        assert!(tokens.expires_at < Local::now() + Duration::seconds(3599));

        let requests = server.received_requests().await.unwrap();
        let sent = form(&requests[0]);
        assert_eq!(sent["grant_type"], "authorization_code");
        assert_eq!(sent["code"], "4/thecode");
        assert_eq!(sent["code_verifier"], "the-verifier");
        assert_eq!(sent["client_id"], "abc.apps.googleusercontent.com");
        assert_eq!(sent["client_secret"], "s3cret");
        assert_eq!(sent["redirect_uri"], "http://127.0.0.1:41234");
    }

    #[tokio::test]
    async fn a_refresh_posts_the_refresh_grant_and_keeps_the_old_token_when_none_comes_back() {
        // Google returns no `refresh_token` on an ordinary refresh; losing the
        // one we already hold there would silently log the user out.
        let server = token_server(serde_json::json!({
            "access_token": "ya29.fresh",
            "expires_in": 3599,
            "token_type": "Bearer",
        }))
        .await;

        let tokens = refresh_at(&cfg(), "1//old-refresh", &format!("{}/token", server.uri()))
            .await
            .unwrap();

        assert_eq!(tokens.access_token, "ya29.fresh");
        assert_eq!(tokens.refresh_token.as_deref(), Some("1//old-refresh"));
        assert!(tokens.expires_at > Local::now());

        let requests = server.received_requests().await.unwrap();
        let sent = form(&requests[0]);
        assert_eq!(sent["grant_type"], "refresh_token");
        assert_eq!(sent["refresh_token"], "1//old-refresh");
        assert_eq!(sent["client_id"], "abc.apps.googleusercontent.com");
        assert_eq!(sent["client_secret"], "s3cret");
        assert!(!sent.contains_key("code"));
    }

    #[tokio::test]
    async fn a_rotated_refresh_token_replaces_the_old_one() {
        let server = token_server(serde_json::json!({
            "access_token": "ya29.fresh",
            "refresh_token": "1//rotated",
            "expires_in": 3599,
        }))
        .await;
        let tokens = refresh_at(&cfg(), "1//old-refresh", &format!("{}/token", server.uri()))
            .await
            .unwrap();
        assert_eq!(tokens.refresh_token.as_deref(), Some("1//rotated"));
    }

    #[tokio::test]
    async fn a_rejected_refresh_is_an_error_that_does_not_echo_the_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "invalid_grant",
                "error_description": "Token has been expired or revoked. 1//old-refresh",
            })))
            .mount(&server)
            .await;

        let err = refresh_at(&cfg(), "1//old-refresh", &format!("{}/token", server.uri()))
            .await
            .unwrap_err();
        let rendered = format!("{err:#}");
        assert!(!rendered.contains("1//old-refresh"), "leaked the token: {rendered}");
        assert!(!rendered.contains("s3cret"), "leaked the secret: {rendered}");
        assert!(!rendered.contains("expired or revoked"), "echoed the description: {rendered}");
        // The short OAuth code is safe and is the one thing worth surfacing.
        assert!(rendered.contains("invalid_grant"), "unhelpful error: {rendered}");
    }

    #[test]
    fn tokens_do_not_reveal_their_secrets_when_printed() {
        let t = Tokens {
            access_token: "ya29.access".into(),
            refresh_token: Some("1//refresh".into()),
            expires_at: Local::now(),
        };
        let rendered = format!("{t:?}");
        assert!(!rendered.contains("ya29.access"), "{rendered}");
        assert!(!rendered.contains("1//refresh"), "{rendered}");
        assert!(rendered.contains("Tokens"), "{rendered}");
    }

    // ---- Token stores -------------------------------------------------------

    #[test]
    fn a_file_store_round_trips_and_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore(dir.path().join("token.json"));
        assert!(store.load().unwrap().is_none());
        store.save("refresh-me").unwrap();
        assert_eq!(store.load().unwrap().unwrap(), "refresh-me");
        let mode = std::fs::metadata(dir.path().join("token.json")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        store.clear().unwrap();
        assert!(store.load().unwrap().is_none());
    }

    #[test]
    fn a_file_store_creates_its_directory_and_overwrites_an_older_token() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore(dir.path().join("nested/deeper/token.json"));
        store.save("first").unwrap();
        store.save("second").unwrap();
        assert_eq!(store.load().unwrap().unwrap(), "second");
        let mode = std::fs::metadata(&store.0).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "an overwrite must not widen the mode");
        // No temporary leftovers holding a copy of the token beside it.
        let strays: Vec<_> = std::fs::read_dir(store.0.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|n| n != "token.json")
            .collect();
        assert!(strays.is_empty(), "left behind: {strays:?}");
    }

    #[test]
    fn an_empty_token_file_reads_as_no_token() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore(dir.path().join("token.json"));
        std::fs::write(&store.0, "  \n").unwrap();
        assert!(store.load().unwrap().is_none());
    }

    #[test]
    fn clearing_a_store_that_holds_nothing_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        FileStore(dir.path().join("absent.json")).clear().unwrap();
    }

    #[test]
    fn a_file_store_says_how_it_stores_the_token() {
        assert!(FileStore(PathBuf::from("/tmp/x")).describe().contains("0600"));
    }

    /// The keyring itself is deliberately never touched by the test suite: a
    /// locked Secret Service raises a modal unlock dialog on the developer's
    /// desktop. What *is* tested is the watchdog that keeps such a dialog from
    /// wedging Protector forever.
    #[test]
    fn a_keyring_call_that_never_answers_is_abandoned_rather_than_waited_on() {
        let started = std::time::Instant::now();
        let err = guarded(std::time::Duration::from_millis(50), "read the token", || {
            std::thread::sleep(std::time::Duration::from_secs(30));
            Ok(())
        })
        .unwrap_err();
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(format!("{err:#}").contains("read the token"), "error was: {err:#}");
    }

    #[test]
    fn a_keyring_call_that_answers_in_time_returns_its_value() {
        let got = guarded(std::time::Duration::from_secs(5), "read the token", || {
            Ok(Some("value".to_string()))
        })
        .unwrap();
        assert_eq!(got.as_deref(), Some("value"));
    }
}
