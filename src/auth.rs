//! Google OAuth: the installed-app flow (PKCE over a loopback redirect) and
//! the token endpoint. Where the refresh token is *kept* between runs is
//! [`crate::token_store`]'s business, not this module's.
//!
//! Nothing in this module ever prints, logs or formats a token. `Tokens` has a
//! hand-written `Debug` that redacts both halves, and the token endpoint's error
//! bodies are dropped rather than attached to the error, because Google echoes
//! the offending refresh token back inside `error_description`.

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
/// `pub(crate)` so `sync::Syncer` can hold it as its production default and
/// swap in a mock server's address under test. Not `pub`: nothing outside this
/// crate has any business pointing the token exchange somewhere else.
pub(crate) const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";

/// How long `login` waits for the browser to come back before giving up.
const CONSENT_TIMEOUT: StdDuration = StdDuration::from_secs(300);
/// How long one accepted connection may take to send its request head. Separate
/// from `CONSENT_TIMEOUT` on purpose: a peer that connects and stays silent must
/// cost that peer's own task, never the login's remaining patience.
const CONNECTION_READ_BUDGET: StdDuration = StdDuration::from_secs(30);

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

/// Answers one connection.
///
/// `None` means "this was not the redirect": a speculative preconnect that never
/// says anything, a `/favicon.ico` fetch, a socket that errored. The login is not
/// over — the real redirect may still be on its way — so the caller keeps waiting.
async fn serve_connection(
    mut stream: tokio::net::TcpStream,
    expected_state: &str,
) -> Option<anyhow::Result<String>> {
    let request = tokio::time::timeout(CONNECTION_READ_BUDGET, read_request_head(&mut stream))
        .await
        .ok()? // Said nothing in time — give up on *this connection only*.
        .ok()?; // Read error — likewise.
    let is_callback = request
        .split_whitespace()
        .nth(1)
        .map(|t| t.contains('?'))
        .unwrap_or(false);
    if !is_callback {
        let _ = stream.write_all(http_response("404 Not Found", FAILURE_PAGE).as_bytes()).await;
        let _ = stream.shutdown().await;
        return None;
    }
    let result = parse_callback(&request, expected_state);
    let page = if result.is_ok() { SUCCESS_PAGE } else { FAILURE_PAGE };
    // The browser is told the outcome before the error propagates, so the user
    // sees a page rather than a connection reset.
    let _ = stream.write_all(http_response("200 OK", page).as_bytes()).await;
    let _ = stream.shutdown().await;
    Some(result)
}

/// Waits for the browser's redirect on an already-bound loopback listener and
/// returns the authorization code.
///
/// Each connection is served by its own task, and that is not a refinement — it
/// is the whole point. Browsers routinely open a speculative connection and then
/// send nothing on it. Reading such a socket inline would park the single accept
/// loop until the login-wide deadline expired, so the real redirect arriving on
/// the *next* connection would never even be accepted. Handing every connection
/// to a task keeps `accept()` free no matter what any one peer does.
async fn accept_callback(
    listener: &tokio::net::TcpListener,
    expected_state: &str,
    wait: StdDuration,
) -> anyhow::Result<String> {
    let deadline = tokio::time::Instant::now() + wait;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<anyhow::Result<String>>(4);
    // Dropped — and so aborted — the moment this function returns, so a half-read
    // socket can never outlive the login it belongs to.
    let mut connections = tokio::task::JoinSet::new();

    loop {
        tokio::select! {
            // Biased so that a code already in hand always beats the deadline
            // arm: without it `select!` could pick the timeout at random on the
            // very tick a successful callback landed.
            biased;

            // Whichever connection turns out to be the redirect settles it. The
            // handler only sends after it has written the browser's reply, so by
            // the time this fires the user already has their page.
            Some(outcome) = rx.recv() => return outcome,

            accepted = tokio::time::timeout_at(deadline, listener.accept()) => {
                let (stream, _) = accepted
                    .map_err(|_| anyhow::anyhow!("timed out waiting for the browser"))??;
                let expected = expected_state.to_string();
                let tx = tx.clone();
                connections.spawn(async move {
                    if let Some(outcome) = serve_connection(stream, &expected).await {
                        let _ = tx.send(outcome).await;
                    }
                });
            }
        }
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
            // 60s of slack so a request never starts with an almost-expired
            // token, but never *behind* now: a server answering with less
            // than 60 would otherwise mint a token that is already expired,
            // `Syncer::access_token`'s freshness check would fail on every
            // call, and the sync loop would refresh on every attempt. A
            // quota'd token endpoint answers that with `invalid_grant`, which
            // this branch reads as *revoked* — so a short-lived token would
            // end in the account being disconnected.
            expires_at: Local::now() + Duration::seconds((r.expires_in - 60).max(1)),
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

/// The token endpoint's answer to a request it refused, kept as a typed,
/// downcastable error rather than folded into a string immediately: a caller
/// — `sync.rs` — needs to tell a revoked refresh token apart from an ordinary
/// failure (network, 5xx, timeout), and matching on rendered prose would be
/// one rewording away from silently breaking that.
#[derive(Debug)]
pub struct TokenRequestError {
    pub status: reqwest::StatusCode,
    pub code: Option<String>,
}

impl std::fmt::Display for TokenRequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.code {
            Some(code) => write!(f, "the Google token endpoint refused the request ({}: {code})", self.status),
            None => write!(f, "the Google token endpoint refused the request ({})", self.status),
        }
    }
}

impl std::error::Error for TokenRequestError {}

/// True when `err` is a token-endpoint refusal whose short OAuth code is
/// `invalid_grant` — Google's signal that the refresh token itself is no
/// longer valid (revoked from the Account permissions page, or expired from
/// long disuse), as distinct from a transient failure that is worth retrying.
///
/// Only meaningful for an error that came out of a *refresh* grant. Nothing
/// in `sync.rs` — the only caller — ever surfaces the authorization-code
/// exchange through this path, so that distinction does not need to be made
/// here.
pub fn is_revoked_refresh(err: &anyhow::Error) -> bool {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<TokenRequestError>())
        .is_some_and(|e| e.code.as_deref() == Some("invalid_grant"))
}

async fn post_token(endpoint: &str, form: &[(&str, &str)]) -> anyhow::Result<Tokens> {
    let response = reqwest::Client::new().post(endpoint).form(form).send().await?;
    let status = response.status();
    if !status.is_success() {
        let code = response.json::<TokenError>().await.ok().and_then(|e| e.error);
        return Err(TokenRequestError { status, code }.into());
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

pub(crate) async fn refresh_at(
    cfg: &Config,
    refresh_token: &str,
    endpoint: &str,
) -> anyhow::Result<Tokens> {
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
    async fn a_silent_connection_does_not_block_the_real_callback() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // A browser preconnect: the socket is opened and then says nothing at
        // all, for as long as the browser feels like holding it. Served inline,
        // this one connection would swallow the entire login-wide deadline and
        // the real redirect below would never be accepted.
        let preconnect = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let browser = tokio::spawn(async move {
            send(port, "GET /?state=st8&code=ok HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await
        });

        let code = accept_callback(&listener, "st8", std::time::Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(code, "ok");
        assert_well_formed_http(&browser.await.unwrap());
        drop(preconnect);
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

    // ---- Telling a revoked refresh token apart from an ordinary failure -----

    #[tokio::test]
    async fn a_refresh_rejected_as_invalid_grant_is_detected_as_revoked() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "invalid_grant",
            })))
            .mount(&server)
            .await;

        let err = refresh_at(&cfg(), "1//old-refresh", &format!("{}/token", server.uri()))
            .await
            .unwrap_err();
        assert!(is_revoked_refresh(&err), "an invalid_grant refusal must read as revoked");
    }

    #[tokio::test]
    async fn a_5xx_refresh_failure_is_not_treated_as_revoked() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let err = refresh_at(&cfg(), "1//old-refresh", &format!("{}/token", server.uri()))
            .await
            .unwrap_err();
        assert!(!is_revoked_refresh(&err), "a transient outage must not look like a revocation");
    }

    #[tokio::test]
    async fn a_different_oauth_code_is_not_treated_as_revoked() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "invalid_client",
            })))
            .mount(&server)
            .await;

        let err = refresh_at(&cfg(), "1//old-refresh", &format!("{}/token", server.uri()))
            .await
            .unwrap_err();
        assert!(!is_revoked_refresh(&err), "only invalid_grant means the refresh token was revoked");
    }

    #[test]
    fn a_network_error_is_not_treated_as_revoked() {
        // No mock is mounted, so the request never gets an HTTP response at
        // all — this exercises the branch that never reaches `post_token`'s
        // `TokenRequestError`.
        let err = anyhow::anyhow!("connection refused");
        assert!(!is_revoked_refresh(&err));
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

    // ---- The token endpoint's answer ----------------------------------------

    fn response(expires_in: i64) -> Tokens {
        TokenResponse {
            access_token: "ya29.example".into(),
            refresh_token: None,
            expires_in,
        }
        .into()
    }

    #[test]
    fn an_ordinary_hour_long_token_keeps_a_minute_of_slack() {
        let remaining = (response(3599).expires_at - Local::now()).num_seconds();
        assert!((3530..=3539).contains(&remaining), "got {remaining}s");
    }

    /// Google returns 3599, but nothing forces that. A lifetime shorter than
    /// the 60s of slack must not produce a token that is born expired: the
    /// freshness check in `Syncer::access_token` would then fail on every
    /// call, the sync loop would refresh on every attempt, and a token
    /// endpoint that answers that abuse with `invalid_grant` would get the
    /// account disconnected as revoked.
    #[test]
    fn a_short_lived_token_is_never_born_expired() {
        for expires_in in [59, 30, 1, 0, -1] {
            let expires_at = response(expires_in).expires_at;
            assert!(
                expires_at > Local::now(),
                "expires_in = {expires_in} produced an already-expired token"
            );
        }
    }

}
