use std::io::{BufRead, BufReader};
use std::process::{Command as OsCommand, Stdio};
use std::sync::{Arc, Mutex};

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

/// Starts a private session bus and returns its address plus the child process.
fn private_bus() -> (String, std::process::Child) {
    let mut child = OsCommand::new("dbus-daemon")
        .args(["--session", "--print-address", "--nofork"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("dbus-daemon must be installed");
    let mut line = String::new();
    BufReader::new(child.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    (line.trim().to_string(), child)
}

#[tokio::test]
async fn item_registers_itself_and_serves_the_label() {
    let (address, mut bus) = private_bus();

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

    let (ui_tx, ui_rx) = tokio::sync::watch::channel(protector::tray::UiState {
        label: "42:17 \u{b7} Design review".into(),
        attention: false,
        menu: protector::tray::menu_model::MenuModel::default(),
    });
    let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel(16);

    let conn = protector::tray::run_tray_on(address.as_str(), ui_rx, cmd_tx)
        .await
        .unwrap();

    // The watcher was told about us.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
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

    let _ = ui_tx.send(protector::tray::UiState {
        label: "09:58 \u{b7} Design review".into(),
        attention: false,
        menu: protector::tray::menu_model::MenuModel::default(),
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let label: String = proxy.get_property("XAyatanaLabel").await.unwrap();
    assert_eq!(label, "09:58 \u{b7} Design review");

    let _ = bus.kill();
}
