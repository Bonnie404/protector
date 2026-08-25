use chrono::Local;
use protector::auth::{self, TokenStore};
use protector::config;
use protector::core::{apply, derive_ui, tick, AppState, Effect};
use protector::state::{load, restore_selection, save, state_path, PersistedState};
use protector::task::Task;
use protector::tray::{self, Command};

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
/// runtime thread it goes.
async fn token_store() -> anyhow::Result<Box<dyn TokenStore>> {
    Ok(tokio::task::spawn_blocking(auth::token_store).await?)
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
    let tokens = auth::login(&cfg).await?;
    // Without a refresh token the widget would stop working in an hour, so this
    // is a failed login rather than a partial success.
    let refresh = tokens.refresh_token.ok_or_else(|| {
        anyhow::anyhow!(
            "Google returned no refresh token. Remove Protector at \
             https://myaccount.google.com/permissions and run `protector login` again."
        )
    })?;
    let store = token_store().await?;
    let where_ = store.describe();
    tokio::task::spawn_blocking(move || store.save(&refresh)).await??;
    println!("protector: connected. The refresh token is kept in the {where_}.");
    Ok(())
}

async fn logout() -> anyhow::Result<()> {
    let store = token_store().await?;
    let where_ = store.describe();
    tokio::task::spawn_blocking(move || store.clear()).await??;
    // Phrased as the end state rather than "removed": `clear()` is deliberately
    // idempotent, so this same line is the truth whether or not there was one.
    println!("protector: signed out — no refresh token is held in the {where_} any more.");
    println!("protector: Google still lists the app until you remove it at https://myaccount.google.com/permissions.");
    Ok(())
}

async fn status() -> anyhow::Result<()> {
    let config_file = config::config_path();
    let cfg = config::load_or_create(&config_file)?;
    let store = token_store().await?;
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

async fn run() -> anyhow::Result<()> {
    // Fixed for the process lifetime: computed once so every `Effect::Persist`
    // in the run loop below writes to the same file `state_path()` names now.
    let state_file = state_path();

    let mut state = AppState { connected: true, ..Default::default() };
    let now = Local::now();
    state.selection = restore_selection(&load(&state_file), now);
    // Hardcoded until Task 8; proves the loop end to end.
    state.tasks_now = vec![Task {
        id: "e1".into(),
        title: "Design review".into(),
        start: now,
        end: now + chrono::Duration::minutes(3),
    }];

    // The menu the host currently has. Every click is resolved against this
    // exact copy, never a freshly derived one: the host can only ever be
    // clicking on ids it was actually shown, so this is the only version whose
    // ids are guaranteed to still mean what they meant when the host rendered
    // them. Re-deriving before resolving would race a concurrent tick/sync that
    // renumbers the menu between render and click, misattributing the click to
    // a neighbouring item.
    let mut published = derive_ui(&state, now);
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

    while let Some(cmd) = cmd_rx.recv().await {
        let now = Local::now();
        let effects = match cmd {
            Command::Tick => tick(&mut state, now),
            Command::MenuClicked(id) => match published.menu.action_for(id).cloned() {
                Some(action) => apply(&mut state, &action),
                None => vec![],
            },
            Command::AboutToShow | Command::SecondaryActivate => vec![],
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
        if effects.contains(&Effect::Quit) {
            break;
        }
        published = derive_ui(&state, now);
        let _ = ui_tx.send(published.clone());
    }

    // Explicit, deliberate shutdown rather than letting `main` return and
    // relying on the runtime to tear the still-running background tasks down
    // on its own: `register_and_watch` and `spawn_emitter` each hold a
    // `Connection` clone forever (their loops never exit), so
    // `conn.graceful_shutdown()` — which waits for every other clone to drop —
    // would hang here. `close()` instead closes the shared socket immediately
    // without waiting on those clones, flushing any in-flight write so the bus
    // daemon sees an orderly disconnect and releases both of our names right
    // away. `process::exit` then ends the process immediately instead of
    // waiting out the background loops' retry timers.
    let _ = conn.close().await;
    std::process::exit(0);
}
