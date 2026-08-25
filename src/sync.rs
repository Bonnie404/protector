//! The sync step: turn a stored refresh token into today's task list, and fold
//! the outcome back into `AppState`.
//!
//! This lives outside `main.rs` on purpose. Everything the widget does with the
//! network — minting an access token, retrying a 401, persisting a rotated
//! refresh token, keeping the last good list when the request fails — is
//! reachable from a test that points `base_url` and `token_endpoint` at a
//! `wiremock` server, with no Google account anywhere in sight.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Local};
use tokio::sync::mpsc;

use crate::auth::{self, Tokens};
use crate::calendar;
use crate::config::Config;
use crate::core::{backoff, reconcile, AppState, Effect};
use crate::task::Task;
use crate::token_store::{token_writes, TokenStore, TokenWrites};
use crate::tray::Command;

/// The unattended sync period (spec §6).
pub const INTERVAL: Duration = Duration::from_secs(300);

/// How old the last sync has to be before opening the menu is worth a fetch.
/// Short enough that the list is fresh when it matters, long enough that
/// flicking the menu open and shut does not hammer the API.
pub const STALE_SECS: i64 = 30;

pub fn is_stale(last_sync: Option<DateTime<Local>>, now: DateTime<Local>) -> bool {
    last_sync.is_none_or(|t| (now - t).num_seconds() > STALE_SECS)
}

/// How long the sync loop waits before its next attempt: the ordinary period
/// while things are working, and the backoff ladder once they are not.
pub fn wait_after(consecutive_failures: u32) -> Duration {
    match consecutive_failures {
        0 => INTERVAL,
        n => backoff(n - 1),
    }
}

/// How long one sync attempt may take in total.
///
/// `reqwest` applies no timeout of its own, and a connection that is accepted
/// and then blackholed — a captive portal, a suspended laptop's stale socket —
/// would otherwise park the sync task forever: the widget would keep ticking
/// while silently never syncing again, and never show the offline marker
/// either, because nothing ever failed.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Owns the access token between syncs and knows how to mint another one.
pub struct Syncer {
    cfg: Config,
    store: Arc<dyn TokenStore>,
    base_url: String,
    token_endpoint: String,
    /// The current access token, or `None` before the first refresh.
    tokens: Option<Tokens>,
    request_timeout: Duration,
    /// Guards this syncer's token writes against a concurrent disconnect.
    writes: Arc<TokenWrites>,
}

impl Syncer {
    pub fn new(cfg: Config, store: Arc<dyn TokenStore>) -> Self {
        Self::with_endpoints(
            cfg,
            store,
            calendar::API_BASE.to_string(),
            auth::TOKEN_ENDPOINT.to_string(),
        )
    }

    fn with_endpoints(
        cfg: Config,
        store: Arc<dyn TokenStore>,
        base_url: String,
        token_endpoint: String,
    ) -> Self {
        Self {
            cfg,
            store,
            base_url,
            token_endpoint,
            tokens: None,
            request_timeout: REQUEST_TIMEOUT,
            writes: token_writes(),
        }
    }

    /// One bounded sync attempt.
    pub async fn sync(&mut self, now: DateTime<Local>) -> anyhow::Result<Vec<Task>> {
        let budget = self.request_timeout;
        let result = tokio::time::timeout(budget, self.attempt(now)).await.unwrap_or_else(|_| {
            Err(anyhow::anyhow!("the calendar did not answer within {}s", budget.as_secs()))
        });
        if result.is_err() {
            // A cancelled attempt can have persisted a rotated refresh token
            // without ever reaching the line that caches it, which would leave
            // this holding one Google has already retired. The store is the
            // authority; drop everything cached and read it again next time.
            self.tokens = None;
        }
        result
    }

    /// Fetches today's tasks, refreshing the access token first if it is
    /// missing or expired, and once more if the API answers 401.
    async fn attempt(&mut self, now: DateTime<Local>) -> anyhow::Result<Vec<Task>> {
        // The epoch this attempt belongs to. A disconnect (or a newer login)
        // between here and any write below turns that write into a no-op
        // instead of a credential resurrected after its revocation.
        let epoch = self.writes.epoch();
        let access = self.access_token(now, epoch).await?;

        // Everything the retry closure needs, owned outright: it has to be
        // `'static`, and `self` is borrowed immutably for the length of the
        // fetch. The `Tokens` it mints come back through `issued`, and are
        // installed below once those borrows have ended.
        let issued: Arc<std::sync::Mutex<Option<Tokens>>> = Arc::new(std::sync::Mutex::new(None));
        let out = issued.clone();
        let cfg = self.cfg.clone();
        let endpoint = self.token_endpoint.clone();
        let store = self.store.clone();
        let writes = self.writes.clone();
        // Resolved inside the closure, so a sync that never sees a 401 never
        // touches the token store — which on this desktop may mean a Secret
        // Service round trip, or a locked collection that answers with an error.
        let cached_refresh = self.tokens.as_ref().and_then(|t| t.refresh_token.clone());

        let result = calendar::fetch_with_retry(
            &self.base_url,
            &access,
            move |_stale| async move {
                let refresh_token = match cached_refresh {
                    Some(token) => token,
                    None => stored_refresh_token(&store).await?,
                };
                let tokens = refreshed(&cfg, &endpoint, &store, &writes, &refresh_token, epoch).await?;
                let access = tokens.access_token.clone();
                *out.lock().expect("the refresh mutex is never held across a panic") = Some(tokens);
                Ok(access)
            },
            &self.cfg,
            now,
        )
        .await;

        if let Some(tokens) = issued.lock().expect("no panic holds this mutex").take() {
            self.tokens = Some(tokens);
        }
        result
    }

    /// The access token to use for a request starting at `now`, minting one if
    /// the cached token is missing or has expired.
    async fn access_token(&mut self, now: DateTime<Local>, epoch: u64) -> anyhow::Result<String> {
        if let Some(t) = self.tokens.as_ref().filter(|t| t.expires_at > now) {
            return Ok(t.access_token.clone());
        }
        let refresh_token = self.refresh_token().await?;
        let tokens =
            refreshed(&self.cfg, &self.token_endpoint, &self.store, &self.writes, &refresh_token, epoch)
                .await?;
        let access = tokens.access_token.clone();
        self.tokens = Some(tokens);
        Ok(access)
    }

    /// The refresh token: the one in hand, or the stored one.
    async fn refresh_token(&self) -> anyhow::Result<String> {
        match self.tokens.as_ref().and_then(|t| t.refresh_token.clone()) {
            Some(token) => Ok(token),
            None => stored_refresh_token(&self.store).await,
        }
    }
}

/// The stored refresh token, or the reason there is nothing to sync.
async fn stored_refresh_token(store: &Arc<dyn TokenStore>) -> anyhow::Result<String> {
    load_refresh_token(store).await?.ok_or_else(|| {
        anyhow::anyhow!(
            "no Google account is connected — run `protector login`, or choose \
             \u{201c}Connect Google Calendar\u{2026}\u{201d} in the menu"
        )
    })
}

/// Exchanges a refresh token for an access token, persisting the refresh token
/// if Google issued a different one.
///
/// A free function rather than a method because the 401 retry closure has to
/// own everything it touches, and so cannot hold `&Syncer`.
async fn refreshed(
    cfg: &Config,
    endpoint: &str,
    store: &Arc<dyn TokenStore>,
    writes: &Arc<TokenWrites>,
    refresh_token: &str,
    epoch: u64,
) -> anyhow::Result<Tokens> {
    let tokens = auth::refresh_at(cfg, refresh_token, endpoint).await?;
    // `refresh_at` carries the old token forward when the response omits one,
    // so a difference here means Google really did rotate it. Persisting it is
    // not optional: the copy on disk is the only one that survives a restart,
    // and the old one stops working the moment the new one is issued.
    match tokens.refresh_token.as_deref() {
        Some(fresh) if fresh != refresh_token => {
            save_refresh_token(store, writes, fresh, epoch).await?
        }
        _ => {}
    }
    Ok(tokens)
}

/// Both store calls go through `spawn_blocking`: `KeyringStore` talks to the
/// Secret Service and can sit there for seconds, which is not something the
/// runtime's worker threads may be made to wait on.
async fn load_refresh_token(store: &Arc<dyn TokenStore>) -> anyhow::Result<Option<String>> {
    let store = store.clone();
    tokio::task::spawn_blocking(move || store.load()).await?
}

/// Persists a rotated refresh token, unless the account was disconnected while
/// this attempt was in flight.
///
/// Nothing can cancel this once it is dispatched — that is the whole reason the
/// epoch exists — so the decision to write is taken inside the blocking closure,
/// under the same lock a revocation has to hold.
async fn save_refresh_token(
    store: &Arc<dyn TokenStore>,
    writes: &Arc<TokenWrites>,
    token: &str,
    epoch: u64,
) -> anyhow::Result<()> {
    let store = store.clone();
    let writes = writes.clone();
    let token = token.to_string();
    let wrote =
        tokio::task::spawn_blocking(move || writes.save_unless_revoked(&*store, &token, epoch))
            .await??;
    if !wrote {
        // Dropping it is the correct outcome, not a failure: the user asked for
        // there to be no stored credential, and Google retired the old one the
        // moment it issued this.
        eprintln!("protector: discarded a refreshed token — the account was disconnected mid-sync.");
    }
    Ok(())
}

/// Folds a sync outcome into the state, returning the effects it produced.
///
/// A failure keeps every task exactly where it was and only raises
/// `last_error`, which is what puts the disabled `⚠ Offline — synced 14:03`
/// item in the menu. Blanking the list on a dropped Wi-Fi connection would
/// throw away the one thing the user still needs.
pub fn apply_sync(
    state: &mut AppState,
    result: Result<Vec<Task>, String>,
    now: DateTime<Local>,
) -> Vec<Effect> {
    match result {
        Ok(tasks) => {
            let (running, later) = calendar::partition(tasks.clone(), now);
            state.tasks_now = running;
            state.tasks_later = later;
            state.last_error = None;
            state.last_sync = Some(now);
            // The lists above are now an actual answer about today, which is
            // what lets the menu say `Nothing scheduled today` when they are
            // empty rather than `Loading today…`.
            state.synced = true;
            if out_of_window(state, &tasks, now) {
                // Nothing to reconcile against: see `out_of_window`.
                state.revision += 1;
                vec![Effect::Persist]
            } else {
                reconcile(state, &tasks)
            }
        }
        Err(message) => {
            state.last_error = Some(message);
            state.revision += 1;
            vec![]
        }
    }
}

/// True when the selection has already ended *and* the fresh list does not
/// mention it.
///
/// `calendar::fetch` asks for events that end after `now`, so an event in
/// overtime is legitimately missing from the answer. Handing that list to
/// `reconcile` would read the absence as a deletion and cancel the overtime
/// countdown the user is still looking at — so the sync layer, which is what
/// knows where the fetch window starts, filters the case out here.
fn out_of_window(state: &AppState, fresh: &[Task], now: DateTime<Local>) -> bool {
    state
        .selection
        .as_ref()
        .is_some_and(|s| s.task.end <= now && !fresh.iter().any(|t| t.id == s.task.id))
}

/// A running sync task.
///
/// Dropping this aborts the task, rather than merely closing its request
/// channel: closing the channel alone would end the loop only *between*
/// attempts, and one attempt can run for `REQUEST_TIMEOUT`.
///
/// The abort reaches the task's own control flow at its next await point, and
/// no further than that. Work already handed to `tokio::task::spawn_blocking`
/// — which is how every token-store write is made — runs to completion
/// regardless, so this is emphatically **not** a guarantee that nothing more
/// will be written after the drop. What guarantees that is
/// `token_store::TokenWrites`:
/// a write dispatched before a revocation either lands before the clear that
/// follows it, or is dropped for being a generation behind.
pub struct SyncHandle {
    requests: mpsc::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

impl SyncHandle {
    /// Asks for an out-of-band sync. Never blocks: the channel holds one slot,
    /// so a full channel already carries a request for exactly this.
    pub fn request(&self) {
        let _ = self.requests.try_send(());
    }
}

impl Drop for SyncHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Starts the sync task. The task lives exactly as long as the handle.
pub fn spawn(syncer: Syncer, tx: mpsc::Sender<Command>) -> SyncHandle {
    // Capacity 1: a second request arriving while one is already queued asks
    // for the same thing, so `try_send` drops it rather than making the run
    // loop — which is also the 1 Hz tick loop — wait for room.
    let (requests, requests_rx) = mpsc::channel(1);
    SyncHandle { requests, task: tokio::spawn(run(syncer, requests_rx, tx)) }
}

/// Syncs on request and on a timer, reporting every outcome back to the run
/// loop. Returns when the request handle is dropped or the run loop is gone.
async fn run(mut syncer: Syncer, mut requests: mpsc::Receiver<()>, tx: mpsc::Sender<Command>) {
    let mut failures: u32 = 0;
    loop {
        tokio::select! {
            _ = tokio::time::sleep(wait_after(failures)) => {}
            request = requests.recv() => {
                // `None` means the run loop dropped the handle: the account was
                // disconnected, or the widget is shutting down.
                if request.is_none() {
                    return;
                }
            }
        }
        let outcome = syncer.sync(Local::now()).await;
        if let Err(e) = &outcome {
            if auth::is_revoked_refresh(e) {
                // Distinct from the ordinary offline path below, and reported
                // at most once: rather than looping back around to retry a
                // credential that cannot become valid again on its own, the
                // task ends here. The run loop clears the token and connects
                // no more syncers until a fresh login hands it a new one.
                let _ = tx.send(Command::TokenRevoked).await;
                return;
            }
        }
        failures = if outcome.is_ok() { 0 } else { failures.saturating_add(1) };
        if let (Err(e), 1) = (&outcome, failures) {
            // Once per outage, not once per retry: the menu's `⚠ Offline` item
            // says *that* something is wrong, and this is the only place that
            // says what. A laptop offline all day must not fill the journal.
            eprintln!("protector: sync failed: {e:#}");
        }
        // `{:#}` so an `anyhow` chain arrives as one line. None of these errors
        // can carry a token: `auth` drops Google's error bodies precisely
        // because they quote the refresh token back at you.
        if tx.send(Command::Synced(outcome.map_err(|e| format!("{e:#}")))).await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token_store::FileStore;
    use crate::core::{apply, derive_ui};
    use crate::task::Selection;
    use crate::tray::menu_model::Action;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A fixed +02:00 instant, matching the fixture, so these tests mean the
    /// same thing under any TZ the test process runs in.
    fn at(h: u32, m: u32) -> DateTime<Local> {
        format!("2026-08-25T{h:02}:{m:02}:00+02:00")
            .parse::<DateTime<chrono::FixedOffset>>()
            .unwrap()
            .with_timezone(&Local)
    }

    fn cfg() -> Config {
        Config {
            client_id: "abc.apps.googleusercontent.com".into(),
            client_secret: "s3cret".into(),
            calendar_id: "primary".into(),
            warn_before_minutes: 5,
        }
    }

    fn task(id: &str, title: &str, s: (u32, u32), e: (u32, u32)) -> Task {
        Task { id: id.into(), title: title.into(), start: at(s.0, s.1), end: at(e.0, e.1) }
    }

    /// A store holding `token`, in a directory that is deleted with the guard.
    fn store(token: Option<&str>) -> (tempfile::TempDir, Arc<dyn TokenStore>) {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore(dir.path().join("token.json"));
        if let Some(t) = token {
            store.save(t).unwrap();
        }
        (dir, Arc::new(store))
    }

    fn syncer(server: &MockServer, store: Arc<dyn TokenStore>) -> Syncer {
        syncer_guarded(server, store, Arc::new(TokenWrites::new()))
    }

    /// A syncer whose token writes are guarded by `writes`. Tests always pass an
    /// instance of their own rather than the process-wide `token_writes()`: on the shared
    /// guard, one test's revocation would invalidate another's write.
    fn syncer_guarded(
        server: &MockServer,
        store: Arc<dyn TokenStore>,
        writes: Arc<TokenWrites>,
    ) -> Syncer {
        let mut s =
            Syncer::with_endpoints(cfg(), store, server.uri(), format!("{}/token", server.uri()));
        s.writes = writes;
        s
    }

    /// A store whose `save` parks until the test lets it through, so a write can
    /// be held open across the exact instant a disconnect lands. What is being
    /// pinned is the ordering between clear and save, not the storage backend.
    struct BlockingStore {
        inner: FileStore,
        entered: tokio::sync::mpsc::Sender<()>,
        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
        landed: tokio::sync::mpsc::Sender<()>,
    }

    impl TokenStore for BlockingStore {
        fn save(&self, token: &str) -> anyhow::Result<()> {
            let _ = self.entered.try_send(());
            let _ = self.release.lock().expect("no panic holds this").recv();
            let outcome = self.inner.save(token);
            // Announced *after* the bytes are down, so the assertion can never
            // win by outrunning the write it is meant to catch.
            let _ = self.landed.try_send(());
            outcome
        }
        fn load(&self) -> anyhow::Result<Option<String>> {
            self.inner.load()
        }
        fn clear(&self) -> anyhow::Result<()> {
            self.inner.clear()
        }
        fn describe(&self) -> &'static str {
            "blocking test store"
        }
    }

    /// The token endpoint, answering once with `body`.
    async fn mock_token(server: &MockServer, body: serde_json::Value) {
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(server)
            .await;
    }

    /// The events endpoint, answering `template` to a request bearing `token`.
    async fn mock_events(server: &MockServer, token: &str, template: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path("/calendars/primary/events"))
            .and(header("authorization", format!("Bearer {token}").as_str()))
            .respond_with(template)
            .expect(1)
            .mount(server)
            .await;
    }

    fn fixture() -> ResponseTemplate {
        ResponseTemplate::new(200)
            .set_body_string(include_str!("../tests/fixtures/events.json"))
            .insert_header("content-type", "application/json")
    }

    fn connected_state() -> AppState {
        AppState { connected: true, ..Default::default() }
    }

    // ---- The four scenarios -------------------------------------------------

    #[tokio::test]
    async fn a_successful_sync_fills_the_now_and_later_lists() {
        let server = MockServer::start().await;
        let (_dir, store) = store(Some("1//stored"));
        mock_token(
            &server,
            serde_json::json!({"access_token": "ya29.first", "expires_in": 3599}),
        )
        .await;
        mock_events(&server, "ya29.first", fixture()).await;

        let tasks = syncer(&server, store).sync(at(14, 30)).await.unwrap();

        let mut state = connected_state();
        let effects = apply_sync(&mut state, Ok(tasks), at(14, 30));
        assert_eq!(
            state.tasks_now.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec!["e1"]
        );
        assert_eq!(
            state.tasks_later.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec!["e2", "e6"]
        );
        assert_eq!(state.last_sync, Some(at(14, 30)));
        assert!(state.last_error.is_none());
        assert!(effects.contains(&Effect::Persist));
        // The menu the panel would draw from it.
        let labels: Vec<String> =
            derive_ui(&state, at(14, 30)).menu.items.iter().map(|i| i.label.clone()).collect();
        assert!(labels.iter().any(|l| l.starts_with("Design review")), "{labels:?}");
        assert!(labels.iter().any(|l| l.starts_with("Deep work")), "{labels:?}");
    }

    #[tokio::test]
    async fn a_401_refreshes_retries_and_saves_the_rotated_refresh_token() {
        let server = MockServer::start().await;
        let (_dir, store) = store(Some("1//stored"));
        // Google rotates a refresh token rarely, but when it does, the copy on
        // disk is the only thing standing between the user and a re-login.
        mock_token(
            &server,
            serde_json::json!({
                "access_token": "ya29.fresh",
                "refresh_token": "1//rotated",
                "expires_in": 3599,
            }),
        )
        .await;
        mock_events(&server, "ya29.stale", ResponseTemplate::new(401)).await;
        mock_events(&server, "ya29.fresh", fixture()).await;

        let mut s = syncer(&server, store.clone());
        // An access token the widget still believes in, which Google has since
        // rejected — the exact case the retry exists for.
        s.tokens = Some(Tokens {
            access_token: "ya29.stale".into(),
            refresh_token: Some("1//stored".into()),
            expires_at: at(15, 30),
        });

        let tasks = s.sync(at(14, 30)).await.unwrap();
        assert!(tasks.iter().any(|t| t.id == "e1"));
        assert_eq!(store.load().unwrap().as_deref(), Some("1//rotated"));
        // And the new access token is kept, so the next sync does not refresh again.
        assert_eq!(s.tokens.as_ref().unwrap().access_token, "ya29.fresh");
    }

    #[tokio::test]
    async fn a_failed_sync_keeps_the_previous_tasks_and_shows_offline() {
        let server = MockServer::start().await;
        let (_dir, store) = store(Some("1//stored"));
        mock_token(
            &server,
            serde_json::json!({"access_token": "ya29.first", "expires_in": 3599}),
        )
        .await;
        mock_events(&server, "ya29.first", ResponseTemplate::new(500)).await;

        let mut state = connected_state();
        state.tasks_now = vec![task("e1", "Design review", (14, 0), (15, 30))];
        state.tasks_later = vec![task("e2", "Deep work", (15, 30), (17, 0))];
        state.last_sync = Some(at(14, 3));
        apply(&mut state, &Action::SelectTask("e1".into()));
        let before = state.revision;

        let error = syncer(&server, store).sync(at(14, 30)).await.unwrap_err();
        apply_sync(&mut state, Err(format!("{error:#}")), at(14, 30));

        assert_eq!(state.tasks_now.len(), 1, "the last good list must survive");
        assert_eq!(state.tasks_later.len(), 1);
        assert_eq!(state.selection.as_ref().unwrap().task.id, "e1");
        assert_eq!(state.last_sync, Some(at(14, 3)), "a failure is not a sync");
        assert!(state.last_error.is_some());
        assert!(state.revision > before, "the menu has to redraw with the offline item");

        let ui = derive_ui(&state, at(14, 30));
        assert!(ui.menu.items.iter().any(|i| i.label.starts_with("Design review")));
        assert!(ui
            .menu
            .items
            .iter()
            .any(|i| i.label == "\u{26a0} Offline \u{2014} synced 14:03" && !i.enabled));
    }

    #[test]
    fn the_retry_schedule_backs_off_and_then_holds() {
        assert_eq!(wait_after(0), INTERVAL, "a healthy widget syncs on the period");
        assert_eq!(wait_after(1).as_secs(), 30);
        assert_eq!(wait_after(2).as_secs(), 60);
        assert_eq!(wait_after(3).as_secs(), 120);
        assert_eq!(wait_after(4).as_secs(), 240);
        assert_eq!(wait_after(5).as_secs(), 300);
        assert_eq!(wait_after(50).as_secs(), 300);
    }

    // ---- Token handling -----------------------------------------------------

    #[tokio::test]
    async fn a_sync_without_a_stored_token_never_reaches_the_calendar() {
        let server = MockServer::start().await;
        let (_dir, store) = store(None);
        let error = syncer(&server, store).sync(at(14, 30)).await.unwrap_err();
        assert!(format!("{error:#}").contains("login"), "unhelpful: {error:#}");
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_access_token_is_reused_until_it_expires() {
        let server = MockServer::start().await;
        let (_dir, store) = store(Some("1//stored"));
        // `expect(1)` on the token mock is the assertion: a second refresh here
        // would fail the test when the server is dropped.
        mock_token(
            &server,
            serde_json::json!({"access_token": "ya29.first", "expires_in": 3599}),
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/calendars/primary/events"))
            .and(header("authorization", "Bearer ya29.first"))
            .respond_with(fixture())
            .expect(2)
            .mount(&server)
            .await;

        let mut s = syncer(&server, store);
        s.sync(at(14, 30)).await.unwrap();
        s.sync(at(14, 35)).await.unwrap();
    }

    #[tokio::test]
    async fn a_request_that_never_answers_becomes_a_failure_rather_than_a_stuck_task() {
        let server = MockServer::start().await;
        let (_dir, store) = store(Some("1//stored"));
        mock_token(
            &server,
            serde_json::json!({"access_token": "ya29.first", "expires_in": 3599}),
        )
        .await;
        mock_events(
            &server,
            "ya29.first",
            fixture().set_delay(Duration::from_millis(400)),
        )
        .await;

        let mut s = syncer(&server, store);
        s.request_timeout = Duration::from_millis(50);
        let error = s.sync(at(14, 30)).await.unwrap_err();
        assert!(format!("{error:#}").contains("did not answer"), "{error:#}");
        // The attempt may have been cut short between a token rotation and its
        // cache update, so nothing cached survives a failure.
        assert!(s.tokens.is_none());
    }

    #[tokio::test]
    async fn an_expired_access_token_is_refreshed_before_the_request() {
        let server = MockServer::start().await;
        let (_dir, store) = store(Some("1//stored"));
        mock_token(
            &server,
            serde_json::json!({"access_token": "ya29.fresh", "expires_in": 3599}),
        )
        .await;
        mock_events(&server, "ya29.fresh", fixture()).await;

        let mut s = syncer(&server, store);
        s.tokens = Some(Tokens {
            access_token: "ya29.expired".into(),
            refresh_token: Some("1//stored".into()),
            expires_at: at(14, 29),
        });
        s.sync(at(14, 30)).await.unwrap();
    }

    #[tokio::test]
    async fn a_logout_stops_an_attempt_that_would_have_rotated_the_token_afterwards() {
        // The race this pins: a 401 sends the syncer to the token endpoint,
        // Google answers slowly with a *rotated* refresh token, and by the time
        // it lands the user has already chosen "Disconnect account". Writing it
        // then would leave a live credential behind an explicit revocation.
        let server = MockServer::start().await;
        let (_dir, store) = store(Some("1//stored"));
        mock_events(&server, "ya29.stale", ResponseTemplate::new(401)).await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({
                        "access_token": "ya29.fresh",
                        "refresh_token": "1//rotated",
                        "expires_in": 3599,
                    }))
                    .set_delay(Duration::from_millis(300)),
            )
            .mount(&server)
            .await;

        let mut s = syncer(&server, store.clone());
        s.tokens = Some(Tokens {
            access_token: "ya29.stale".into(),
            refresh_token: Some("1//stored".into()),
            // The loop syncs at `Local::now()`, not at a fixture instant, so
            // this has to be unexpired against the wall clock — otherwise the
            // attempt refreshes up front and never reaches the 401 path.
            expires_at: Local::now() + chrono::Duration::hours(1),
        });
        let (cmd_tx, _cmd_rx) = mpsc::channel(4);
        let handle = spawn(s, cmd_tx);
        handle.request();
        // Long enough to be inside the delayed token exchange, far short of the
        // response landing.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Exactly what `Effect::Logout` does, in that order.
        drop(handle);
        store.clear().unwrap();

        // Well past the point the rotated token would have been written.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(
            store.load().unwrap(),
            None,
            "a rotated refresh token was written after the account was disconnected"
        );
    }

    #[tokio::test]
    async fn a_logout_during_the_write_itself_still_leaves_no_token_behind() {
        // The sub-case an abort cannot touch: the token response has *landed*,
        // the rotated token is already on its way to the store, and only then
        // does the user choose "Disconnect account". `spawn_blocking` runs a
        // dispatched closure to completion regardless of its handle, so nothing
        // can call that write back — the guard has to serialise it instead.
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let (entered_tx, mut entered) = tokio::sync::mpsc::channel(1);
        let (release, release_rx) = std::sync::mpsc::channel();
        let (landed_tx, mut landed) = tokio::sync::mpsc::channel(1);
        // Seeded through a plain `FileStore` on the same path: routing it
        // through the blocking one would park the test itself, and would burn
        // the `entered` signal the write under test has to deliver.
        let token_path = dir.path().join("token.json");
        FileStore(token_path.clone()).save("1//stored").unwrap();
        let store: Arc<dyn TokenStore> = Arc::new(BlockingStore {
            inner: FileStore(token_path),
            entered: entered_tx,
            release: std::sync::Mutex::new(release_rx),
            landed: landed_tx,
        });

        mock_events(&server, "ya29.stale", ResponseTemplate::new(401)).await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "ya29.fresh",
                "refresh_token": "1//rotated",
                "expires_in": 3599,
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/calendars/primary/events"))
            .and(header("authorization", "Bearer ya29.fresh"))
            .respond_with(fixture())
            .mount(&server)
            .await;

        let writes = Arc::new(TokenWrites::new());
        let mut s = syncer_guarded(&server, store.clone(), writes.clone());
        s.tokens = Some(Tokens {
            access_token: "ya29.stale".into(),
            refresh_token: Some("1//stored".into()),
            expires_at: Local::now() + chrono::Duration::hours(1),
        });
        let (cmd_tx, _cmd_rx) = mpsc::channel(4);
        let handle = spawn(s, cmd_tx);
        handle.request();

        // The rotated token is now inside `store.save`, parked, holding the
        // write lock. No abort can reach it.
        entered.recv().await.expect("the rotated token reached the store");
        drop(handle);

        // Exactly what `Effect::Logout` does, on a blocking thread as it would
        // be in the widget: it has to queue behind the write already under way.
        let (started_tx, mut started) = tokio::sync::mpsc::channel(1);
        let logout = tokio::task::spawn_blocking({
            let writes = writes.clone();
            let store = store.clone();
            move || {
                let _ = started_tx.try_send(());
                writes.invalidate();
                store.clear().unwrap();
            }
        });
        started.recv().await.expect("the disconnect is under way");
        // Deliberate: this hands an *unguarded* implementation every chance to
        // finish clearing before the write is released, which is exactly the
        // interleaving that used to lose the revocation. Under the guard the
        // disconnect is parked on the lock and this changes nothing, so the
        // assertion below holds on timing grounds for a broken implementation
        // and on ordering grounds for a correct one.
        tokio::time::sleep(Duration::from_millis(200)).await;
        release.send(()).unwrap();
        // The write has now finished, one way or the other. Under the guard the
        // disconnect is still queued behind it and only clears after this.
        landed.recv().await.expect("the write completed");
        logout.await.unwrap();

        assert_eq!(
            store.load().unwrap(),
            None,
            "a rotated refresh token outlived the disconnect that was meant to revoke it"
        );
    }

    // ---- A revoked refresh token ---------------------------------------------

    #[tokio::test]
    async fn a_revoked_refresh_token_is_reported_as_revoked_not_as_an_ordinary_failure() {
        let server = MockServer::start().await;
        let (_dir, store) = store(Some("1//revoked"));
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "invalid_grant",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let s = syncer(&server, store);
        let (cmd_tx, mut cmd_rx) = mpsc::channel(4);
        let handle = spawn(s, cmd_tx);
        handle.request();

        let cmd = tokio::time::timeout(Duration::from_secs(2), cmd_rx.recv())
            .await
            .expect("the sync task must report the revocation promptly")
            .expect("the channel must still be open");
        assert!(matches!(cmd, Command::TokenRevoked), "got {cmd:?} instead of TokenRevoked");

        // And it must not decay into the ordinary offline path: no
        // `Command::Synced(Err(_))` ever follows for this attempt. Nothing
        // more arriving shows up as either a timeout (the task is still
        // alive but silent) or the channel closing (the task already ended
        // and dropped its sender) — either is fine; only a further `Synced`
        // would mean the revocation was reported twice.
        let second = tokio::time::timeout(Duration::from_millis(50), cmd_rx.recv()).await;
        assert!(
            !matches!(second, Ok(Some(_))),
            "a revocation must be reported exactly once, not as Synced too: got {second:?}"
        );
        drop(handle);
    }

    #[tokio::test]
    async fn a_revoked_refresh_token_ends_the_sync_task_so_it_cannot_retry_and_report_again() {
        let server = MockServer::start().await;
        let (_dir, store) = store(Some("1//revoked"));
        // `expect(1)`: dropped at the end of the test, wiremock asserts the
        // token endpoint was hit exactly once — proof the task did not loop
        // back around and ask again.
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "invalid_grant",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let s = syncer(&server, store);
        let (cmd_tx, mut cmd_rx) = mpsc::channel(4);
        let handle = spawn(s, cmd_tx);
        handle.request();
        cmd_rx.recv().await.expect("TokenRevoked arrives");

        // The task has already returned; a further request on the handle
        // must not resurrect it into a second attempt. The task ending drops
        // its sender, so the strongest proof available is the channel
        // reporting closed (`Ok(None)`) rather than handing back another
        // `Command` — a bare timeout would also be consistent with "still
        // alive but silent", which is not what this test is pinning.
        handle.request();
        let after_it_ended = tokio::time::timeout(Duration::from_millis(200), cmd_rx.recv()).await;
        assert!(
            !matches!(after_it_ended, Ok(Some(_))),
            "a finished sync task answered a request sent after it ended: got {after_it_ended:?}"
        );
        drop(handle);
    }

    // ---- Reconciliation through a real sync ---------------------------------

    #[test]
    fn the_selection_survives_a_sync_that_reshuffles_the_lists() {
        let mut state = connected_state();
        state.tasks_now = vec![task("e1", "Design review", (14, 0), (15, 30))];
        state.tasks_later = vec![task("e2", "Deep work", (15, 30), (17, 0))];
        apply(&mut state, &Action::SelectTask("e2".into()));
        // Half an hour later e1 has ended and e2 is the one running.
        let fresh = vec![task("e2", "Deep work", (15, 30), (17, 0))];
        apply_sync(&mut state, Ok(fresh), at(15, 40));
        assert!(state.tasks_now.iter().any(|t| t.id == "e2"));
        assert!(state.tasks_later.is_empty());
        assert_eq!(state.selection.as_ref().unwrap().task.id, "e2");
    }

    #[test]
    fn an_overtime_selection_survives_a_sync_that_no_longer_lists_it() {
        // `fetch` asks for events ending after `now`, so a task in overtime is
        // legitimately absent from the answer. Reading that as a deletion would
        // cancel the overtime countdown the user is still looking at.
        let mut state = connected_state();
        state.selection = Some(Selection {
            task: task("e1", "Design review", (14, 0), (15, 30)),
            warned: true,
            ended_notified: true,
        });
        let effects = apply_sync(&mut state, Ok(vec![task("e2", "Deep work", (15, 30), (17, 0))]), at(15, 40));
        assert_eq!(state.selection.as_ref().unwrap().task.id, "e1");
        assert!(!effects.contains(&Effect::NotifyRemoved));
    }

    #[test]
    fn a_deleted_event_still_clears_the_selection_while_it_is_running() {
        let mut state = connected_state();
        state.selection = Some(Selection {
            task: task("e1", "Design review", (14, 0), (15, 30)),
            warned: false,
            ended_notified: false,
        });
        let effects = apply_sync(&mut state, Ok(vec![task("e2", "Deep work", (15, 30), (17, 0))]), at(14, 30));
        assert!(state.selection.is_none());
        assert!(effects.contains(&Effect::NotifyRemoved));
    }

    // ---- When the menu asks for a sync --------------------------------------

    #[test]
    fn opening_the_menu_syncs_only_when_the_last_sync_is_stale() {
        let now = at(14, 30);
        let ago = |s: i64| Some(now - chrono::Duration::seconds(s));
        assert!(is_stale(None, now), "never synced");
        assert!(!is_stale(ago(29), now), "29s ago is fresh enough");
        assert!(is_stale(ago(31), now));
    }
}
