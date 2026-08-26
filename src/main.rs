use std::sync::Arc;

use chrono::Local;
use protector::auth;
use protector::config;
use protector::core::{
    apply, derive_ui, end_candidates, tick, token_revoked, warn_before_secs, AppState, Effect,
};
use protector::notify;
use protector::state::{load, restore_selection, save, state_path, PersistedState};
use protector::sync;
use protector::token_store::{self, TokenStore};
use protector::tray::menu_model::{Action, TaskIdMemo};
use protector::tray::{self, Command};
use tokio::sync::mpsc;

#[derive(clap::Parser)]
#[command(name = "protector", version, about = "Calendar countdown in the GNOME panel")]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// Run the panel widget (default)
    Run,
    /// Connect a Google account
    Login,
    /// Forget the stored refresh token
    Logout,
    /// Print connection and sync status
    Status,
}

#[tokio::main]
async fn main() {
    use clap::Parser as _;
    // `Option<Cmd>` defaulting to `Run` is what keeps a bare `protector` doing
    // exactly what it did before this CLI existed.
    let result = match Cli::parse().command.unwrap_or(Cmd::Run) {
        Cmd::Run => run().await,
        Cmd::Login => login().await,
        Cmd::Logout => logout().await,
        Cmd::Status => status().await,
    };
    // One line on stderr rather than anyhow's `Error:` dump: every failure these
    // subcommands can produce is an expected outcome the user has to act on, not
    // a crash worth a backtrace.
    if let Err(e) = result {
        eprintln!("protector: {e:#}");
        std::process::exit(1);
    }
}

/// Picking a token store touches the Secret Service, which blocks. Off the
/// runtime thread it goes — once per process, then it is cached.
async fn selected_store() -> anyhow::Result<&'static dyn TokenStore> {
    Ok(tokio::task::spawn_blocking(token_store::token_store).await?)
}

fn require_configured() -> anyhow::Result<config::Config> {
    let path = config::config_path();
    let cfg = config::load_or_create(&path)?;
    if !cfg.is_complete() {
        anyhow::bail!(
            "no Google OAuth client configured yet.\n  \
             Edit {} and fill in client_id and client_secret.\n  \
             The file's own comments walk through creating them at \
             https://console.cloud.google.com/apis/credentials",
            path.display()
        );
    }
    Ok(cfg)
}

async fn login() -> anyhow::Result<()> {
    let cfg = require_configured()?;
    println!("protector: opening Google's consent screen for read-only calendar access...");
    let (where_, stale) = connect_account(&cfg).await?;
    println!("protector: connected. The refresh token is kept in the {where_}.");
    for other in stale {
        eprintln!("protector: warning — could not clear an older token from the {other}.");
    }
    Ok(())
}

/// Runs the OAuth flow and stores the refresh token. Returns where it was
/// stored, plus any store that would not give up its older copy.
///
/// Shared by `protector login` and the menu's *Connect Google Calendar…*, so
/// that connecting from the panel gets the same single-copy guarantee as
/// connecting from a terminal.
async fn connect_account(cfg: &config::Config) -> anyhow::Result<(&'static str, Vec<&'static str>)> {
    let tokens = auth::login(cfg).await?;
    // Without a refresh token the widget would stop working in an hour, so this
    // is a failed login rather than a partial success.
    let refresh = tokens.refresh_token.ok_or_else(|| {
        anyhow::anyhow!(
            "Google returned no refresh token. Remove Protector at \
             https://myaccount.google.com/permissions and run `protector login` again."
        )
    })?;
    let store = selected_store().await?;
    let where_ = store.describe();
    let stale = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<&'static str>> {
        // Installed through the guard: any token write still in flight from an
        // earlier connection is a generation behind this one and is discarded.
        token_store::token_writes().install(store, &refresh)?;
        // Exactly one copy may survive a login. An older token left in the store
        // that was *not* selected this run stays valid at Google, is invisible to
        // `status`, and would be missed by a later `logout` that happens to
        // select the other store. Best effort: a keyring that will not answer
        // must not fail a login that has already succeeded.
        Ok(token_store::all_token_stores()
            .into_iter()
            .filter(|other| other.describe() != where_)
            .filter(|other| other.clear().is_err())
            .map(|other| other.describe())
            .collect())
    })
    .await??;

    Ok((where_, stale))
}

async fn logout() -> anyhow::Result<()> {
    // Every store, not the one `selected_store()` would select today: the token may
    // well have been written by an earlier run that chose differently, and a
    // revocation that silently misses it is worse than no revocation at all.
    let outcomes = tokio::task::spawn_blocking(|| token_store::token_writes().revoke_all_stores()).await?;

    let mut unrevoked = Vec::new();
    for (where_, outcome) in &outcomes {
        match outcome {
            // Phrased as the end state rather than "removed": `clear()` is
            // idempotent, so this is the truth whether or not there was one.
            Ok(()) => println!("protector: no refresh token is held in the {where_} any more."),
            Err(e) => {
                eprintln!("protector: could not clear the {where_}: {e:#}");
                unrevoked.push(*where_);
            }
        }
    }
    if !unrevoked.is_empty() {
        anyhow::bail!(
            "signed out of {} of {} stores. A refresh token may still be live in: {}. \
             Revoke Protector at https://myaccount.google.com/permissions.",
            outcomes.len() - unrevoked.len(),
            outcomes.len(),
            unrevoked.join(", ")
        );
    }
    println!("protector: signed out.");
    println!("protector: Google still lists the app until you remove it at https://myaccount.google.com/permissions.");
    Ok(())
}

async fn status() -> anyhow::Result<()> {
    let config_file = config::config_path();
    let cfg = config::load_or_create(&config_file)?;
    let store = selected_store().await?;
    let where_ = store.describe();
    // Only ever asked *whether* there is a token. The value is never printed,
    // and never leaves this function.
    let connected = tokio::task::spawn_blocking(move || store.load()).await?;

    println!("config       {}", config_file.display());
    println!(
        "             {}",
        if cfg.is_complete() {
            "complete".to_string()
        } else {
            "incomplete — client_id and client_secret are still empty".to_string()
        }
    );
    println!("calendar     {}", cfg.calendar_id);
    println!("token store  {where_}");
    println!(
        "account      {}",
        match connected {
            Ok(Some(_)) => "connected".to_string(),
            Ok(None) => "not connected — run `protector login`".to_string(),
            Err(e) => format!("unknown — the token store could not be read: {e:#}"),
        }
    );
    let state_file = state_path();
    println!(
        "last sync    {}",
        match load(&state_file).last_sync {
            Some(t) => t.format("%Y-%m-%d %H:%M:%S").to_string(),
            None => "never".to_string(),
        }
    );
    Ok(())
}

/// Asks the sync task for an out-of-band sync, if there is one. Never blocks,
/// and no sync task at all means no account to sync.
fn request_sync(handle: &Option<sync::SyncHandle>) {
    if let Some(handle) = handle {
        handle.request();
    }
}

/// Whether a refresh token is on file. Errors are reported and treated as "no",
/// which lands the user on `Not connected` with a way to fix it, rather than on
/// a widget that refuses to start.
async fn has_stored_token(store: &Arc<dyn TokenStore>) -> bool {
    let store = store.clone();
    match tokio::task::spawn_blocking(move || store.load()).await {
        Ok(Ok(token)) => token.is_some(),
        Ok(Err(e)) => {
            eprintln!("protector: could not read the token store: {e:#}");
            false
        }
        Err(e) => {
            eprintln!("protector: the token store lookup did not finish: {e}");
            false
        }
    }
}

/// Handles *Connect Google Calendar…* from the menu, returning whether a flow
/// is now waiting on the browser.
///
/// Without a client id there is no flow to start, so this opens `config.toml`
/// instead of sending the user to a consent screen that would refuse them.
/// With one, the flow waits on a browser for up to five minutes and so runs in
/// a task of its own; the run loop hears about it as `Command::LoggedIn`.
fn start_login(cfg: &config::Config, tx: mpsc::Sender<Command>) -> bool {
    if !cfg.is_complete() {
        let path = config::config_path();
        eprintln!(
            "protector: no Google OAuth client configured yet — fill in client_id and \
             client_secret in {}",
            path.display()
        );
        // Spawned and never awaited: on several desktops `xdg-open` does not
        // return until the editor it launched exits.
        let _ = tokio::process::Command::new("xdg-open").arg(&path).spawn();
        return false;
    }
    let cfg = cfg.clone();
    tokio::spawn(async move {
        let report = LoginReport::new(tx);
        let outcome = connect_account(&cfg).await.map(|(where_, stale)| {
            println!("protector: connected. The refresh token is kept in the {where_}.");
            for other in stale {
                eprintln!("protector: warning — could not clear an older token from the {other}.");
            }
        });
        report.send(outcome.map_err(|e| format!("{e:#}"))).await;
    });
    true
}

/// Guarantees the run loop hears about a login exactly once.
///
/// The `login_in_flight` latch is only ever cleared by a `Command::LoggedIn`,
/// so a flow that ended without sending one — a panic anywhere in the OAuth
/// path, a cancelled task — would leave *Connect Google Calendar…* inert for
/// the rest of the process, with the widget insisting a browser it no longer
/// waits for is still being waited on.
struct LoginReport {
    tx: mpsc::Sender<Command>,
    sent: bool,
}

impl LoginReport {
    fn new(tx: mpsc::Sender<Command>) -> Self {
        Self { tx, sent: false }
    }

    /// The ordinary path: awaited, so a busy run loop cannot lose it.
    async fn send(mut self, outcome: Result<(), String>) {
        self.sent = true;
        let _ = self.tx.send(Command::LoggedIn(outcome)).await;
    }
}

impl Drop for LoginReport {
    fn drop(&mut self) {
        if !self.sent {
            // Unwinding, so this cannot await. `try_send` is the best available
            // and is near-certain to succeed: the run loop drains a 32-slot
            // channel once per tick.
            let _ = self
                .tx
                .try_send(Command::LoggedIn(Err("the connection attempt ended unexpectedly".into())));
        }
    }
}

/// Clears every store rather than the selected one, for the same reason
/// `logout` does: the live token may well have been written by a run that
/// selected differently.
///
/// `async fn` — and so `#[must_use]` at every call site — because awaiting it
/// is the whole point. `Effect::Quit` ends the process with
/// `std::process::exit`, which runs no destructors and waits for nothing, so a
/// dropped `JoinHandle` here means *Disconnect account* followed by *Quit* can
/// leave a live refresh token on disk while the panel says `Not connected`.
/// On a locked keyring that window is `token_store::KEYRING_TIMEOUT` wide —
/// ten seconds, exactly when the credential matters most.
///
/// The cost is that the run loop stops for as long as the disconnect takes,
/// and that is **two** keyring timeouts, not one — about twenty seconds:
///
/// * `TokenWrites::revoke_all_stores` first waits on `enter()`, and a sync's
///   rotated-token write can be holding that lock inside `KeyringStore::save`,
///   which `guarded` bounds at `KEYRING_TIMEOUT` (10s) before abandoning the
///   thread and releasing the lock.
/// * It then runs `KeyringStore::clear`, `guarded` by the same 10s.
///
/// So the label can freeze for up to roughly twenty seconds — and only on a
/// keyring that has stopped answering, which is also the only case where
/// either half runs long. That is the right way round: a frozen countdown is a
/// cosmetic fault, a surviving credential is not.
async fn forget_account() {
    let cleared = tokio::task::spawn_blocking(|| {
        // `revoke_all_stores` bumps the write epoch under the same lock a token
        // write has to hold, so a sync attempt that is mid-rotation either
        // finishes before this clear — and is then cleared by it — or is
        // discarded for being a generation behind. Aborting the sync task
        // cannot achieve that: a dispatched `spawn_blocking` write runs to
        // completion whatever happens to its handle.
        for (where_, outcome) in token_store::token_writes().revoke_all_stores() {
            if let Err(e) = outcome {
                eprintln!("protector: could not clear the {where_}: {e:#}");
            }
        }
    })
    .await;
    if let Err(e) = cleared {
        eprintln!("protector: the token clearing task did not finish: {e}");
    }
}

async fn run() -> anyhow::Result<()> {
    // Fixed for the process lifetime: computed once so every `Effect::Persist`
    // in the run loop below writes to the same file `state_path()` names now.
    let state_file = state_path();
    // Deliberately not `require_configured`: an unconfigured widget still
    // starts and says `Not connected` in the panel, which is where a first-time
    // user is looking, rather than exiting with a message they never see.
    let cfg = config::load_or_create(&config::config_path())?;
    let store = token_store::shared_token_store();

    let now = Local::now();
    let persisted = load(&state_file);
    // Read once, here, rather than at the notification site: the countdown and
    // the wording of the message are then driven by the same number and cannot
    // contradict each other.
    let warn_before = warn_before_secs(cfg.warn_before_minutes);
    if warn_before == 0 {
        eprintln!(
            "protector: warn_before_minutes is {} in {}, so the heads-up before a block ends is off.",
            cfg.warn_before_minutes,
            config::config_path().display()
        );
    }
    let mut state = AppState {
        selection: restore_selection(&persisted, now),
        warn_before_secs: warn_before,
        // Restored so a failed first sync can still say *when* the list it is
        // showing was current.
        last_sync: persisted.last_sync,
        // Short-circuits on purpose: an empty config means no account no matter
        // what is in the keyring, and asking would cost a Secret Service probe
        // — and possibly an unlock dialog — for an answer already known.
        connected: cfg.is_complete() && has_stored_token(&store).await,
        ..Default::default()
    };

    // Which id each block's menu item has been given. Held for the whole run,
    // and threaded through every `derive_ui` below, because that is what makes
    // an id mean one block and one block only for the life of the process —
    // see `TaskIdMemo`. A memo rebuilt per derivation would let a departed
    // block's id be reissued to the block it once collided with.
    let mut task_ids = TaskIdMemo::default();

    // The menu the host was last given. Every click is resolved against this
    // exact copy, never a freshly derived one.
    //
    // The guarantee that matters is no longer this copy's, though — it is the
    // ids'. A DBusMenu `Event` carries no revision, so the host sends only an
    // id, and a host whose menu is already open need not have re-fetched the
    // layout since the last rebuild — while `AboutToShow` deliberately fires a
    // sync at precisely that moment. Ids are therefore derived from *what an
    // item is*, never from where it sits: a hash of the event id for tasks, a
    // constant for each fixed item (`tray::menu_model::ids`). A block keeps
    // its id for as long as this process runs — through any sync, including
    // ones that drop it and ones that bring it back — so a click that raced a
    // rebuild still selects the block whose label the user was looking at, and
    // an id whose block is gone resolves to nothing rather than to whatever
    // took its place.
    //
    // Resolving against the published copy is still what makes the *labels*
    // agree: it is the revision whose text the host is displaying, so the
    // check mark and the task list a click is judged against are the ones on
    // screen. `MenuModel::action_for` returning `None`, and
    // `Action::SelectTask` on an id no longer listed, stay quiet no-ops.
    let mut published = derive_ui(&state, now, &mut task_ids);
    let (ui_tx, ui_rx) = tokio::sync::watch::channel(published.clone());
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel(32);
    // A refused single-instance lock is an expected outcome, not a crash: say so
    // in one line on stderr and exit non-zero, with no `Error:` dump or backtrace.
    let conn = match tray::run_tray(ui_rx, cmd_tx.clone()).await {
        Ok(conn) => conn,
        Err(e) => {
            eprintln!("protector: {e:#}");
            std::process::exit(1);
        }
    };

    // Turns a pressed notification button into `Command::SelectById`. Holds
    // its own clone of `conn` for as long as the process runs, the same way
    // `register_and_watch` and `spawn_emitter` already do; `conn.close()` at
    // shutdown does not wait on it.
    {
        let conn = conn.clone();
        let tx = cmd_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = notify::watch_actions(conn, tx).await {
                eprintln!("protector: the notification action listener stopped: {e}");
            }
        });
    }

    let ticker_tx = cmd_tx.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            ticker.tick().await;
            if ticker_tx.send(Command::Tick).await.is_err() {
                break;
            }
        }
    });

    // The sync task exists only while an account is connected. Without one
    // there is nothing to fetch, and a timer failing every five minutes would
    // only hang a `⚠ Offline` item under `Not connected`.
    let mut sync = state.connected.then(|| {
        sync::spawn(sync::Syncer::new(cfg.clone(), store.clone()), cmd_tx.clone())
    });
    // Today's list, now, rather than in five minutes' time.
    request_sync(&sync);
    // Nothing on screen moves while a login waits on the browser, so a second
    // click on *Connect* is the natural thing for a user to do. Without this it
    // would open a second consent tab on a second loopback port.
    let mut login_in_flight = false;

    while let Some(cmd) = cmd_rx.recv().await {
        let now = Local::now();
        let effects = match cmd {
            Command::Tick => tick(&mut state, now),
            Command::MenuClicked(id) => match published.menu.action_for(id).cloned() {
                Some(action) => apply(&mut state, &action),
                None => vec![],
            },
            // The menu is about to be read, so this is the last moment a stale
            // list can still be fixed — but not a reason to fetch again for
            // someone flicking the menu open and shut.
            Command::AboutToShow => {
                if sync::is_stale(state.last_sync, now) {
                    request_sync(&sync);
                }
                vec![]
            }
            // Middle-click: the user asking outright.
            Command::SecondaryActivate => {
                request_sync(&sync);
                vec![]
            }
            // A sync still in flight when the account was disconnected must not
            // repopulate the menu behind the user's back.
            Command::Synced(result) if state.connected => sync::apply_sync(&mut state, result, now),
            Command::Synced(_) => vec![],
            Command::LoggedIn(Ok(())) => {
                login_in_flight = false;
                state.connected = true;
                state.last_error = None;
                state.revision += 1;
                // Aborted before the new one starts: an attempt left running
                // from an earlier connection would otherwise deliver one more
                // `Synced`, which `connected == true` now admits.
                drop(sync.take());
                sync = Some(sync::spawn(
                    sync::Syncer::new(cfg.clone(), store.clone()),
                    cmd_tx.clone(),
                ));
                request_sync(&sync);
                vec![]
            }
            Command::LoggedIn(Err(e)) => {
                login_in_flight = false;
                eprintln!("protector: connecting the account failed: {e}");
                vec![]
            }
            // A notification action button, resolved the same way a menu
            // click is: through `Action::SelectTask`, so a stale id (the
            // task list changed under the notification) is silently a no-op
            // rather than a crash, exactly as a stale menu id already is.
            Command::SelectById(id) => apply(&mut state, &Action::SelectTask(id)),
            // The end-of-task notification's *Nothing* button: clear
            // unconditionally, the same state transition `core::apply`
            // already gives `Action::ClearSelection`.
            Command::ClearSelection => apply(&mut state, &Action::ClearSelection),
            // Distinct from an ordinary `Command::Synced(Err(_))`: the
            // refresh token itself is gone, not merely unreachable, so this
            // disconnects outright rather than showing `⚠ Offline`.
            Command::TokenRevoked => token_revoked(&mut state),
        };
        // Checked as membership, not vector position, and always ahead of the
        // Quit check below: `Action::Quit` currently produces `[Effect::Quit]`
        // alone, but a tick or a selection change can hand back `Effect::Persist`
        // in the very same batch a caller also asked to quit in, and
        // `Effect::Quit` leads straight to `std::process::exit`, which does not
        // unwind, run destructors, or wait on anything in flight. Writing here —
        // synchronously, still inside this awaited loop iteration, before the
        // Quit check ever runs — guarantees the state is on disk before that
        // exit gets a chance to fire, no matter what order the effects vector
        // lists them in.
        if effects.contains(&Effect::Persist) {
            let snapshot = PersistedState::from_selection(state.selection.as_ref(), state.last_sync);
            if let Err(e) = save(&state_file, &snapshot) {
                eprintln!("protector: failed to save state: {e:#}");
            }
        }
        for effect in &effects {
            match effect {
                Effect::Sync => request_sync(&sync),
                Effect::StartLogin if login_in_flight => {
                    eprintln!("protector: a connection attempt is already waiting for the browser.");
                }
                Effect::StartLogin => login_in_flight = start_login(&cfg, cmd_tx.clone()),
                Effect::Logout => {
                    // Aborted *before* the stores are cleared, and not merely
                    // asked to stop: an attempt already inside a 401 retry
                    // could otherwise persist a rotated refresh token after
                    // the clear had run, leaving a live credential behind an
                    // explicit disconnect.
                    drop(sync.take());
                    // Awaited, never spawned and forgotten: see `forget_account`.
                    forget_account().await;
                }
                // `tick` never emits `NotifyWarning`/`NotifyEnded` without a
                // selection in hand, so the `if let` below is not a silent
                // no-op path in practice — it just avoids trusting that from
                // a distance.
                Effect::NotifyWarning(remaining) => {
                    if let Some(sel) = state.selection.as_ref() {
                        // The seconds `tick` measured, carried on the effect
                        // itself. `state.warn_before_secs` — the configured
                        // *window* — is deliberately not consulted here: it is
                        // only an upper bound on the time left, so rendering it
                        // announced `30 minutes left` on a block picked four
                        // minutes before it ended.
                        if let Err(e) = notify::notify_warning(&conn, &sel.task, *remaining).await {
                            eprintln!("protector: failed to send the warning notification: {e}");
                        }
                    }
                }
                Effect::NotifyEnded => {
                    if let Some(sel) = state.selection.as_ref() {
                        // Not `state.tasks_later` alone: a block already
                        // running when this one ended is the soonest thing to
                        // switch to, and used to get neither a button nor a
                        // mention. See `core::end_candidates`.
                        let candidates = end_candidates(&state, &sel.task);
                        if let Err(e) = notify::notify_ended(&conn, &sel.task, &candidates).await {
                            eprintln!("protector: failed to send the end-of-task notification: {e}");
                        }
                    }
                }
                Effect::NotifyRemoved => {
                    if let Err(e) = notify::notify_simple(
                        &conn,
                        "Task removed",
                        "The selected event was deleted from your calendar.",
                    )
                    .await
                    {
                        eprintln!("protector: failed to send the removal notification: {e}");
                    }
                }
                Effect::NotifyRevoked => {
                    if let Err(e) = notify::notify_simple(
                        &conn,
                        "Reconnect required",
                        "Google access was revoked. Choose \u{201c}Connect Google Calendar\u{2026}\u{201d} \
                         in the menu to sign in again.",
                    )
                    .await
                    {
                        eprintln!("protector: failed to send the reconnect notification: {e}");
                    }
                }
                _ => {}
            }
        }
        if effects.contains(&Effect::Quit) {
            break;
        }
        published = derive_ui(&state, now, &mut task_ids);
        let _ = ui_tx.send(published.clone());
    }

    // Explicit, deliberate shutdown rather than letting `main` return and
    // relying on the runtime to tear the still-running background tasks down
    // on its own: `register_and_watch`, `spawn_emitter` and the notification
    // action listener each hold a `Connection` clone forever (their loops
    // never exit), so
    // `conn.graceful_shutdown()` — which waits for every other clone to drop —
    // would hang here. `close()` instead closes the shared socket immediately
    // without waiting on those clones, flushing any in-flight write so the bus
    // daemon sees an orderly disconnect and releases both of our names right
    // away. `process::exit` then ends the process immediately instead of
    // waiting out the background loops' retry timers.
    let _ = conn.close().await;
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A login task that dies without reporting must not strand the latch.
    #[tokio::test]
    async fn a_login_that_never_reports_still_frees_the_menu_item() {
        let (tx, mut rx) = mpsc::channel(4);
        drop(LoginReport::new(tx));
        match rx.try_recv() {
            Ok(Command::LoggedIn(Err(e))) => assert!(e.contains("unexpectedly"), "{e}"),
            other => panic!("expected a LoggedIn failure, got {other:?}"),
        }
    }

    /// And a login that does report sends exactly that, once.
    #[tokio::test]
    async fn a_login_that_reports_sends_its_own_outcome_only() {
        let (tx, mut rx) = mpsc::channel(4);
        LoginReport::new(tx).send(Ok(())).await;
        assert!(matches!(rx.try_recv(), Ok(Command::LoggedIn(Ok(())))));
        assert!(rx.try_recv().is_err(), "the drop guard must not send a second time");
    }
}
