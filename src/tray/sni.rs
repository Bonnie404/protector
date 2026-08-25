use crate::tray::{Command, UiState};
use tokio::sync::{mpsc, watch};
use zbus::object_server::SignalEmitter;

pub struct StatusNotifierItem {
    pub ui: watch::Receiver<UiState>,
    pub tx: mpsc::Sender<Command>,
}

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl StatusNotifierItem {
    #[zbus(property)]
    async fn category(&self) -> &str {
        "ApplicationStatus"
    }

    #[zbus(property)]
    async fn id(&self) -> &str {
        "protector"
    }

    #[zbus(property)]
    async fn title(&self) -> &str {
        "Protector"
    }

    #[zbus(property)]
    async fn status(&self) -> String {
        if self.ui.borrow().attention {
            "NeedsAttention".into()
        } else {
            "Active".into()
        }
    }

    #[zbus(property)]
    async fn icon_name(&self) -> &str {
        // A stock icon until Task 11 ships the real pair.
        "alarm-symbolic"
    }

    #[zbus(property)]
    async fn attention_icon_name(&self) -> &str {
        "alarm-symbolic"
    }

    #[zbus(property)]
    async fn icon_theme_path(&self) -> &str {
        ""
    }

    #[zbus(property)]
    async fn menu(&self) -> zbus::zvariant::ObjectPath<'_> {
        zbus::zvariant::ObjectPath::from_static_str("/MenuBar").unwrap()
    }

    #[zbus(property)]
    async fn item_is_menu(&self) -> bool {
        true
    }

    /// The panel text. `emits_changed_signal = "false"` because this host listens
    /// for XAyatanaNewLabel, not PropertiesChanged.
    #[zbus(property(emits_changed_signal = "false"), name = "XAyatanaLabel")]
    async fn x_ayatana_label(&self) -> String {
        self.ui.borrow().label.clone()
    }

    #[zbus(property(emits_changed_signal = "false"), name = "XAyatanaLabelGuide")]
    async fn x_ayatana_label_guide(&self) -> &str {
        ""
    }

    /// Middle-click.
    async fn secondary_activate(&self, _x: i32, _y: i32) {
        let _ = self.tx.try_send(Command::SecondaryActivate);
    }

    // Activate is deliberately absent: the host learns activation is unsupported
    // and then opens the menu on a single left-click without waiting out the
    // double-click interval.

    #[zbus(signal, name = "XAyatanaNewLabel")]
    pub async fn x_ayatana_new_label(
        emitter: &SignalEmitter<'_>,
        label: &str,
        guide: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal, name = "NewIcon")]
    pub async fn new_icon(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal, name = "NewStatus")]
    pub async fn new_status(emitter: &SignalEmitter<'_>, status: &str) -> zbus::Result<()>;
}
