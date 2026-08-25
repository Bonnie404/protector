use chrono::Local;
use protector::core::{apply, derive_ui, tick, AppState, Effect};
use protector::task::Task;
use protector::tray::{self, Command};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut state = AppState { connected: true, ..Default::default() };
    // Hardcoded until Task 8; proves the loop end to end.
    let now = Local::now();
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
