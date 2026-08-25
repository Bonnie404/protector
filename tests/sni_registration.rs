use futures_util::StreamExt;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Command as OsCommand, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use protector::tray::menu_model::{own, Action, MenuItem, MenuModel};
use protector::tray::UiState;

struct FakeWatcher {
    registered: Arc<Mutex<Vec<String>>>,
    /// Announces each registration as it lands, so a test can wait for the
    /// event itself instead of guessing how long it takes to arrive.
    announce: tokio::sync::mpsc::Sender<String>,
}

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl FakeWatcher {
    async fn register_status_notifier_item(&self, service: String) {
        self.registered.lock().unwrap().push(service.clone());
        // `try_send`, never `send`: a D-Bus method handler must not park on a
        // test that has stopped listening.
        let _ = self.announce.try_send(service);
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
    let (announce, mut registrations) = tokio::sync::mpsc::channel(4);
    let _watcher = zbus::connection::Builder::address(address.as_str())
        .unwrap()
        .name("org.kde.StatusNotifierWatcher")
        .unwrap()
        .serve_at(
            "/StatusNotifierWatcher",
            FakeWatcher {
                registered: registered.clone(),
                announce,
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

    // The watcher was told about us. Waited for as the event it is — the same
    // bounded shape as `next_signal` — rather than slept through: a fixed
    // delay is both slower than the handshake in the common case and shorter
    // than it under load.
    let service = tokio::time::timeout(Duration::from_secs(5), registrations.recv())
        .await
        .expect("the item must register itself with the watcher")
        .expect("the watcher outlives this wait");
    assert_eq!(registered.lock().unwrap().len(), 1);
    assert!(
        service.starts_with("org.kde.StatusNotifierItem-"),
        "registered under an unexpected name: {service}"
    );

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

// ---------------------------------------------------------------------------
// Golden values over the wire (spec §10)
//
// Everything below talks to the real objects over a real bus and pins the
// bytes that come back. Hand-written protocol code fails in exactly these
// places, and none of them are reachable from a test that only inspects Rust
// structs: a `Type::SIGNATURE` assertion cannot fail for a runtime bug, and a
// `children.len()` assertion passes for four structurally wrong children.
// ---------------------------------------------------------------------------

/// The menu tree the golden tests pin: one item of every shape `derive_ui`
/// can produce — a checked radio task, a separator, a disabled header, and a
/// plain command whose label carries a mnemonic underscore.
fn known_menu() -> MenuModel {
    MenuModel::new(vec![
        MenuItem::radio(
            1,
            "Design review   14:00 \u{2013} 15:30",
            true,
            Action::SelectTask("e1".into()),
        ),
        MenuItem::separator(2),
        MenuItem::disabled(3, "Later today"),
        MenuItem::command(4, "Deep_work", Action::Refresh),
    ])
}

async fn proxy_for<'a>(conn: &zbus::Connection, path: &'a str, interface: &'a str) -> zbus::Proxy<'a> {
    zbus::proxy::Builder::new(conn)
        .destination(conn.unique_name().unwrap().clone())
        .unwrap()
        .path(path)
        .unwrap()
        .interface(interface)
        .unwrap()
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .unwrap()
}

/// One `(ia{sv}av)` child out of the `av` array `GetLayout` returns: its id,
/// its property map, and how many children of its own it declares.
fn unpack_child(v: &zbus::zvariant::OwnedValue) -> (i32, HashMap<String, zbus::zvariant::OwnedValue>, usize) {
    use zbus::zvariant::{OwnedValue, Structure};
    let structure = <&Structure>::try_from(v).expect("every child is a (ia{sv}av) struct");
    assert_eq!(
        structure.fields().len(),
        3,
        "a DBusMenu item is (id, properties, children)"
    );
    let mut fields = structure
        .fields()
        .iter()
        .map(|f| OwnedValue::try_from(f.try_clone().unwrap()).unwrap());
    let id = i32::try_from(fields.next().unwrap()).expect("the first field is the item id");
    let properties = HashMap::<String, OwnedValue>::try_from(fields.next().unwrap())
        .expect("the second field is a{sv}");
    let grandchildren = Vec::<OwnedValue>::try_from(fields.next().unwrap())
        .expect("the third field is av")
        .len();
    (id, properties, grandchildren)
}

#[tokio::test]
async fn get_layout_serialises_the_known_menu_tree_child_by_child() {
    let bus = private_bus();
    let (_ui_tx, ui_rx) =
        tokio::sync::watch::channel(ui("42:17 \u{b7} Design review", false, known_menu()));
    let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel(16);
    let conn = protector::tray::run_tray_on(bus.address.as_str(), ui_rx, cmd_tx)
        .await
        .unwrap();

    let menu = proxy_for(&conn, "/MenuBar", "com.canonical.dbusmenu").await;
    let reply = menu
        .call_method("GetLayout", &(0i32, -1i32, Vec::<String>::new()))
        .await
        .unwrap();

    // The signature DBusMenu fixes for this reply, read off the wire rather
    // than off the struct definition.
    assert_eq!(reply.body().signature().to_string(), "(u(ia{sv}av))");

    type WireLayout = (i32, HashMap<String, zbus::zvariant::OwnedValue>, Vec<zbus::zvariant::OwnedValue>);
    let (revision, (root_id, root_properties, children)): (u32, WireLayout) =
        reply.body().deserialize().unwrap();

    assert_eq!(revision, 1, "MenuModel::new starts at revision 1");
    assert_eq!(root_id, 0);
    assert_eq!(root_properties.get("children-display").unwrap(), &own("submenu"));
    assert_eq!(children.len(), 4);

    let items: Vec<_> = children.iter().map(unpack_child).collect();

    // The selected task: enabled, visible, radio-checked.
    assert_eq!(items[0].0, 1);
    assert_eq!(items[0].1.get("label").unwrap(), &own("Design review   14:00 \u{2013} 15:30"));
    assert_eq!(items[0].1.get("enabled").unwrap(), &own(true));
    assert_eq!(items[0].1.get("visible").unwrap(), &own(true));
    assert_eq!(items[0].1.get("toggle-type").unwrap(), &own("radio"));
    assert_eq!(items[0].1.get("toggle-state").unwrap(), &own(1i32));

    // The separator: typed, and carrying nothing else at all.
    assert_eq!(items[1].0, 2);
    assert_eq!(items[1].1.get("type").unwrap(), &own("separator"));
    assert_eq!(items[1].1.len(), 1, "got: {:?}", items[1].1.keys().collect::<Vec<_>>());

    // The disabled header: visible but not selectable, and not a toggle.
    assert_eq!(items[2].0, 3);
    assert_eq!(items[2].1.get("label").unwrap(), &own("Later today"));
    assert_eq!(items[2].1.get("enabled").unwrap(), &own(false));
    assert_eq!(items[2].1.get("visible").unwrap(), &own(true));
    assert!(!items[2].1.contains_key("toggle-type"));

    // The command: underscore doubled, because DBusMenu reads a single one as
    // a mnemonic marker and would swallow it.
    assert_eq!(items[3].0, 4);
    assert_eq!(items[3].1.get("label").unwrap(), &own("Deep__work"));
    assert_eq!(items[3].1.get("enabled").unwrap(), &own(true));

    // Every item is a leaf: this menu has exactly one level.
    for (id, _, grandchildren) in &items {
        assert_eq!(*grandchildren, 0, "item {id} unexpectedly declares children");
    }
}

#[tokio::test]
async fn the_item_serves_every_property_the_spec_names_by_value() {
    let bus = private_bus();
    let (ui_tx, ui_rx) =
        tokio::sync::watch::channel(ui("42:17 \u{b7} Design review", false, known_menu()));
    let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel(16);
    let conn = protector::tray::run_tray_on(bus.address.as_str(), ui_rx, cmd_tx)
        .await
        .unwrap();

    let item = proxy_for(&conn, "/StatusNotifierItem", "org.kde.StatusNotifierItem").await;

    // Spec §5's table, read back one property at a time.
    let s = |p: &'static str| {
        let item = &item;
        async move { item.get_property::<String>(p).await.unwrap() }
    };
    assert_eq!(s("Category").await, "ApplicationStatus");
    assert_eq!(s("Id").await, "protector");
    assert_eq!(s("Title").await, "Protector");
    assert_eq!(s("Status").await, "Active");
    assert_eq!(s("IconName").await, "protector");
    assert_eq!(s("AttentionIconName").await, "protector-attention");
    assert_eq!(s("IconThemePath").await, "", "icons come from the hicolor theme");
    assert_eq!(s("XAyatanaLabel").await, "42:17 \u{b7} Design review");
    assert_eq!(s("XAyatanaLabelGuide").await, "");
    let menu_path: zbus::zvariant::OwnedObjectPath = item.get_property("Menu").await.unwrap();
    assert_eq!(menu_path.as_str(), "/MenuBar");
    assert!(item.get_property::<bool>("ItemIsMenu").await.unwrap());

    // The two properties that are not constants have a second value each, and
    // spec §5 names both. Overtime has to move them together, or the panel
    // draws an `Active` item with the attention icon (or the reverse).
    let _ = ui_tx.send(ui("\u{26a0} +00:01 \u{b7} Design review", true, known_menu()));
    assert_eq!(s("Status").await, "NeedsAttention");
    assert_eq!(s("IconName").await, "protector-attention");
    assert_eq!(s("XAyatanaLabel").await, "\u{26a0} +00:01 \u{b7} Design review");

    // And `Activate` stays unimplemented on purpose (spec §5): the host learns
    // activation is unsupported and stops waiting out the double-click
    // interval before opening the menu.
    let err = item
        .call_method("Activate", &(0i32, 0i32))
        .await
        .expect_err("Activate must not be implemented");
    assert!(
        format!("{err}").contains("Unknown") || format!("{err:?}").contains("UnknownMethod"),
        "expected the standard unknown-method error, got: {err:?}"
    );
}

/// The block of introspection XML describing one property, from its opening
/// tag to its close — self-closing or not.
fn property_xml<'a>(xml: &'a str, name: &str) -> &'a str {
    let start = xml
        .find(&format!("<property name=\"{name}\""))
        .unwrap_or_else(|| panic!("{name} is not in the introspection XML:\n{xml}"));
    let rest = &xml[start..];
    let opening = rest.find('>').expect("an opening tag has to close");
    let end = if rest.as_bytes()[opening - 1] == b'/' {
        opening + 1
    } else {
        rest.find("</property>").expect("a non-empty tag has to close") + "</property>".len()
    };
    &rest[..end]
}

/// A host is entitled to cache any property whose introspection XML promises
/// `PropertiesChanged`. `Status` and `IconName` both change — into and out of
/// overtime — and both are announced as `NewStatus`/`NewIcon` instead, so
/// promising the signal would strand a caching host on the wrong icon for the
/// whole of overtime.
#[tokio::test]
async fn no_property_promises_a_propertieschanged_that_is_never_emitted() {
    let bus = private_bus();
    let (_ui_tx, ui_rx) =
        tokio::sync::watch::channel(ui("42:17 \u{b7} Design review", false, known_menu()));
    let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel(16);
    let conn = protector::tray::run_tray_on(bus.address.as_str(), ui_rx, cmd_tx)
        .await
        .unwrap();

    let introspectable =
        proxy_for(&conn, "/StatusNotifierItem", "org.freedesktop.DBus.Introspectable").await;
    let xml: String = introspectable
        .call_method("Introspect", &())
        .await
        .unwrap()
        .body()
        .deserialize()
        .unwrap();

    for name in ["Status", "IconName", "XAyatanaLabel", "XAyatanaLabelGuide"] {
        let block = property_xml(&xml, name);
        assert!(
            block.contains("EmitsChangedSignal") && block.contains("\"false\""),
            "{name} promises a PropertiesChanged this item never emits:\n{block}"
        );
    }
}
