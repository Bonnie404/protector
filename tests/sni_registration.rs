use futures_util::StreamExt;
use std::io::{BufRead, BufReader};
use std::process::{Command as OsCommand, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use protector::tray::menu_model::MenuModel;
use protector::tray::UiState;

struct FakeWatcher {
    registered: Arc<Mutex<Vec<String>>>,
}

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl FakeWatcher {
    async fn register_status_notifier_item(&self, service: String) {
        self.registered.lock().unwrap().push(service);
    }

    #[zbus(property)]
    async fn is_status_notifier_host_registered(&self) -> bool {
        true
    }

    #[zbus(property)]
    async fn protocol_version(&self) -> i32 {
        0
    }

    #[zbus(property)]
    async fn registered_status_notifier_items(&self) -> Vec<String> {
        self.registered.lock().unwrap().clone()
    }
}

/// A private session bus that is killed and reaped when it goes out of scope —
/// including when an assertion panics, so a failing run cannot leave a `--nofork`
/// dbus-daemon behind on the developer's machine.
struct PrivateBus {
    address: String,
    child: std::process::Child,
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Starts a private session bus.
fn private_bus() -> PrivateBus {
    let mut child = OsCommand::new("dbus-daemon")
        .args(["--session", "--print-address", "--nofork"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("dbus-daemon must be installed");
    let mut line = String::new();
    BufReader::new(child.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    PrivateBus {
        address: line.trim().to_string(),
        child,
    }
}

/// Waits for one signal, failing the test instead of hanging forever if the
/// emitter never sends it.
async fn next_signal(stream: &mut zbus::proxy::SignalStream<'_>, what: &str) -> zbus::message::Body {
    let msg = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
        .unwrap_or_else(|| panic!("signal stream for {what} closed"));
    msg.body()
}

fn ui(label: &str, attention: bool, menu: MenuModel) -> UiState {
    UiState {
        label: label.into(),
        attention,
        menu,
    }
}

#[tokio::test]
async fn item_registers_itself_and_serves_the_label() {
    let bus = private_bus();
    let address = bus.address.clone();

    let registered = Arc::new(Mutex::new(Vec::new()));
    let _watcher = zbus::connection::Builder::address(address.as_str())
        .unwrap()
        .name("org.kde.StatusNotifierWatcher")
        .unwrap()
        .serve_at(
            "/StatusNotifierWatcher",
            FakeWatcher {
                registered: registered.clone(),
            },
        )
        .unwrap()
        .build()
        .await
        .unwrap();

    let (ui_tx, ui_rx) = tokio::sync::watch::channel(ui(
        "42:17 \u{b7} Design review",
        false,
        MenuModel::default(),
    ));
    let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel(16);

    let conn = protector::tray::run_tray_on(address.as_str(), ui_rx, cmd_tx)
        .await
        .unwrap();

    // The watcher was told about us.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(registered.lock().unwrap().len(), 1);

    // And the label property serves what the UiState says. Property caching is
    // off so every read is a real `Properties.Get`, which is how the panel host
    // reads XAyatanaLabel — it never relies on PropertiesChanged for it.
    let proxy: zbus::Proxy = zbus::proxy::Builder::new(&conn)
        .destination(conn.unique_name().unwrap().clone())
        .unwrap()
        .path("/StatusNotifierItem")
        .unwrap()
        .interface("org.kde.StatusNotifierItem")
        .unwrap()
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .unwrap();
    let label: String = proxy.get_property("XAyatanaLabel").await.unwrap();
    assert_eq!(label, "42:17 \u{b7} Design review");

    // Subscribe before mutating, so the emitter cannot beat the subscription.
    let mut new_labels = proxy.receive_signal("XAyatanaNewLabel").await.unwrap();

    let _ = ui_tx.send(ui("09:58 \u{b7} Design review", false, MenuModel::default()));

    // The panel host does not re-read the property on a change — it takes the
    // text straight out of this signal's payload — so the signal, not the
    // property, is the path that actually keeps the panel ticking.
    let body = next_signal(&mut new_labels, "XAyatanaNewLabel").await;
    let (signalled, guide): (String, String) = body.deserialize().unwrap();
    assert_eq!(signalled, "09:58 \u{b7} Design review");
    assert_eq!(guide, "");

    // The property agrees with the signal.
    let label: String = proxy.get_property("XAyatanaLabel").await.unwrap();
    assert_eq!(label, "09:58 \u{b7} Design review");
}

#[tokio::test]
async fn every_ui_change_is_broadcast_as_the_signal_the_panel_listens_for() {
    let bus = private_bus();
    let address = bus.address.clone();

    let (ui_tx, ui_rx) = tokio::sync::watch::channel(ui(
        "42:17 \u{b7} Design review",
        false,
        MenuModel::default(),
    ));
    let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel(16);
    let conn = protector::tray::run_tray_on(address.as_str(), ui_rx, cmd_tx)
        .await
        .unwrap();

    let item: zbus::Proxy = zbus::proxy::Builder::new(&conn)
        .destination(conn.unique_name().unwrap().clone())
        .unwrap()
        .path("/StatusNotifierItem")
        .unwrap()
        .interface("org.kde.StatusNotifierItem")
        .unwrap()
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .unwrap();
    let menu: zbus::Proxy = zbus::proxy::Builder::new(&conn)
        .destination(conn.unique_name().unwrap().clone())
        .unwrap()
        .path("/MenuBar")
        .unwrap()
        .interface("com.canonical.dbusmenu")
        .unwrap()
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .unwrap();

    let mut new_statuses = item.receive_signal("NewStatus").await.unwrap();
    let mut new_icons = item.receive_signal("NewIcon").await.unwrap();
    let mut layout_updates = menu.receive_signal("LayoutUpdated").await.unwrap();

    // Going into overtime must flip the status *and* re-announce the icon, or
    // the panel keeps drawing the normal icon.
    let _ = ui_tx.send(ui("\u{26a0} +00:01 \u{b7} Design review", true, MenuModel::default()));
    let status: String = next_signal(&mut new_statuses, "NewStatus")
        .await
        .deserialize()
        .unwrap();
    assert_eq!(status, "NeedsAttention");
    next_signal(&mut new_icons, "NewIcon").await;
    let served: String = item.get_property("Status").await.unwrap();
    assert_eq!(served, "NeedsAttention");

    // Coming back out of overtime flips it back.
    let _ = ui_tx.send(ui("12:00 \u{b7} Standup", false, MenuModel::default()));
    let status: String = next_signal(&mut new_statuses, "NewStatus")
        .await
        .deserialize()
        .unwrap();
    assert_eq!(status, "Active");

    // A new menu revision tells the host to re-fetch the layout. `default()` is
    // revision 0; `new()` is revision 1.
    let _ = ui_tx.send(ui("12:00 \u{b7} Standup", false, MenuModel::new(Vec::new())));
    let (revision, parent): (u32, i32) = next_signal(&mut layout_updates, "LayoutUpdated")
        .await
        .deserialize()
        .unwrap();
    assert_eq!(revision, 1);
    assert_eq!(parent, 0);
}

#[tokio::test]
async fn a_second_instance_is_refused_by_the_single_instance_lock() {
    let bus = private_bus();
    let address = bus.address.clone();

    // Stand in for a Protector that is already running. It holds only the lock
    // name — the tray name embeds *its* pid, so it can never be the thing that
    // collides. If the lock is what stops us, this is what stops us.
    let _running = zbus::connection::Builder::address(address.as_str())
        .unwrap()
        .name(protector::tray::INSTANCE_LOCK_NAME)
        .unwrap()
        .build()
        .await
        .unwrap();

    let (_ui_tx, ui_rx) = tokio::sync::watch::channel(ui("", false, MenuModel::default()));
    let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel(16);

    let err = protector::tray::run_tray_on(address.as_str(), ui_rx, cmd_tx)
        .await
        .expect_err("the lock name is already owned, so startup must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("already running"),
        "the message must say plainly that another instance holds the lock, got: {msg}"
    );
    assert!(msg.contains(protector::tray::INSTANCE_LOCK_NAME), "got: {msg}");
}
