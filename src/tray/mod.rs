pub mod menu;
pub mod menu_model;
pub mod sni;

use futures_util::StreamExt;
use menu_model::MenuModel;
use tokio::sync::{mpsc, watch};
use zbus::Connection;

#[derive(Debug, Clone, Default)]
pub struct UiState {
    pub label: String,
    pub attention: bool,
    pub menu: MenuModel,
}

#[derive(Debug, Clone)]
pub enum Command {
    Tick,
    MenuClicked(i32),
    AboutToShow,
    SecondaryActivate,
}

/// Protector claims two well-known names on one connection, and they are not
/// interchangeable:
///
/// * `org.kde.StatusNotifierItem-<pid>-1` is the **tray identity**. The PID in it
///   is the convention every SNI host expects, which is exactly why it can never
///   collide between two instances and so can never act as a lock.
/// * `org.protector.Instance` is the **single-instance lock**, and the only reason
///   it is fixed. A second Protector fails to acquire it and exits (spec §9).
///
/// Removing either one breaks something that is not obvious from its call site.
pub const INSTANCE_LOCK_NAME: &str = "org.protector.Instance";

#[zbus::proxy(
    interface = "org.kde.StatusNotifierWatcher",
    default_service = "org.kde.StatusNotifierWatcher",
    default_path = "/StatusNotifierWatcher"
)]
trait StatusNotifierWatcher {
    fn register_status_notifier_item(&self, service: &str) -> zbus::Result<()>;
}

pub async fn run_tray(
    ui: watch::Receiver<UiState>,
    tx: mpsc::Sender<Command>,
) -> anyhow::Result<Connection> {
    run_tray_inner(None, ui, tx).await
}

/// Same as `run_tray`, but on an explicit bus address. Used by the integration test.
pub async fn run_tray_on(
    address: &str,
    ui: watch::Receiver<UiState>,
    tx: mpsc::Sender<Command>,
) -> anyhow::Result<Connection> {
    run_tray_inner(Some(address), ui, tx).await
}

async fn run_tray_inner(
    address: Option<&str>,
    ui: watch::Receiver<UiState>,
    tx: mpsc::Sender<Command>,
) -> anyhow::Result<Connection> {
    let well_known = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
    let builder = match address {
        Some(a) => zbus::connection::Builder::address(a)?,
        None => zbus::connection::Builder::session()?,
    };
    let conn = builder
        // zbus defaults to AllowReplacement | ReplaceExisting | DoNotQueue. With
        // ReplaceExisting a second instance would *steal* the lock instead of
        // failing, and with AllowReplacement ours could be stolen in turn. Only
        // DoNotQueue is wanted, so that a taken name is an error, not a queue slot.
        .allow_name_replacements(false)
        .replace_existing_names(false)
        .name(well_known.as_str())?
        .name(INSTANCE_LOCK_NAME)?
        .serve_at(
            "/StatusNotifierItem",
            sni::StatusNotifierItem {
                ui: ui.clone(),
                tx: tx.clone(),
            },
        )?
        .serve_at(
            "/MenuBar",
            menu::DBusMenu {
                ui: ui.clone(),
                tx: tx.clone(),
            },
        )?
        // Both names are requested here, inside `build()` — `name()` above only
        // validates their syntax, so this is the one place the lock can fail.
        .build()
        .await
        .map_err(|e| match e {
            zbus::Error::NameTaken => anyhow::anyhow!(
                "another Protector instance is already running (it owns {INSTANCE_LOCK_NAME} on the session bus)"
            ),
            other => anyhow::Error::new(other),
        })?;

    register_and_watch(conn.clone(), well_known);
    spawn_emitter(conn.clone(), ui);
    Ok(conn)
}

/// Registers with the watcher now, and again every time the watcher reappears
/// (GNOME Shell restart, extension toggled off and on).
fn register_and_watch(conn: Connection, well_known: String) {
    tokio::spawn(async move {
        loop {
            let Ok(watcher) = StatusNotifierWatcherProxy::new(&conn).await else {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            };
            if let Err(e) = watcher.register_status_notifier_item(&well_known).await {
                eprintln!("protector: registration failed: {e}");
            }
            let mut owner_changes = match watcher.inner().receive_owner_changed().await {
                Ok(s) => s,
                Err(_) => {
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
            };
            while let Some(owner) = owner_changes.next().await {
                if owner.is_some() {
                    if let Err(e) = watcher.register_status_notifier_item(&well_known).await {
                        eprintln!("protector: re-registration failed: {e}");
                    }
                }
            }
            // The owner-changed stream only ends when the connection is going
            // away; pause before rebuilding so a dying bus cannot spin this loop.
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    });
}

/// Turns UiState changes into the signals this host listens for.
fn spawn_emitter(conn: Connection, mut ui: watch::Receiver<UiState>) {
    tokio::spawn(async move {
        let mut last = ui.borrow().clone();
        while ui.changed().await.is_ok() {
            let current = ui.borrow().clone();
            let item = conn
                .object_server()
                .interface::<_, sni::StatusNotifierItem>("/StatusNotifierItem")
                .await;
            let menu = conn
                .object_server()
                .interface::<_, menu::DBusMenu>("/MenuBar")
                .await;
            if let Ok(item) = item {
                if current.label != last.label {
                    let _ = sni::StatusNotifierItem::x_ayatana_new_label(
                        item.signal_emitter(),
                        &current.label,
                        "",
                    )
                    .await;
                }
                if current.attention != last.attention {
                    let status = if current.attention {
                        "NeedsAttention"
                    } else {
                        "Active"
                    };
                    let _ =
                        sni::StatusNotifierItem::new_status(item.signal_emitter(), status).await;
                    let _ = sni::StatusNotifierItem::new_icon(item.signal_emitter()).await;
                }
            }
            if let Ok(menu_iface) = menu {
                if current.menu.revision != last.menu.revision {
                    let _ = menu::DBusMenu::layout_updated(
                        menu_iface.signal_emitter(),
                        current.menu.revision,
                        0,
                    )
                    .await;
                }
            }
            last = current;
        }
    });
}
