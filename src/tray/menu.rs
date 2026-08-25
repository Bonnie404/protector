use crate::tray::menu_model::Layout;
use crate::tray::{Command, UiState};
use std::collections::HashMap;
use tokio::sync::{mpsc, watch};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Value};

pub struct DBusMenu {
    pub ui: watch::Receiver<UiState>,
    pub tx: mpsc::Sender<Command>,
}

#[zbus::interface(name = "com.canonical.dbusmenu")]
impl DBusMenu {
    #[zbus(property)]
    async fn version(&self) -> u32 {
        3
    }

    #[zbus(property)]
    async fn status(&self) -> &str {
        "normal"
    }

    #[zbus(property)]
    async fn text_direction(&self) -> &str {
        "ltr"
    }

    #[zbus(property)]
    async fn icon_theme_path(&self) -> Vec<String> {
        Vec::new()
    }

    async fn get_layout(
        &self,
        _parent_id: i32,
        _recursion_depth: i32,
        _property_names: Vec<String>,
    ) -> zbus::fdo::Result<(u32, Layout)> {
        let (revision, layout) = {
            let ui = self.ui.borrow();
            (ui.menu.revision, ui.menu.layout())
        };
        let layout = layout.map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        Ok((revision, layout))
    }

    async fn get_group_properties(
        &self,
        ids: Vec<i32>,
        _property_names: Vec<String>,
    ) -> Vec<(i32, HashMap<String, OwnedValue>)> {
        self.ui.borrow().menu.group_properties(&ids)
    }

    async fn get_property(&self, id: i32, name: String) -> zbus::fdo::Result<OwnedValue> {
        self.ui
            .borrow()
            .menu
            .group_properties(&[id])
            .into_iter()
            .next()
            .and_then(|(_, mut props)| props.remove(&name))
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs(format!("no property {name} on {id}")))
    }

    async fn event(&self, id: i32, event_id: String, _data: Value<'_>, _timestamp: u32) {
        if event_id == "clicked" {
            let _ = self.tx.try_send(Command::MenuClicked(id));
        }
    }

    async fn event_group(
        &self,
        events: Vec<(i32, String, Value<'_>, u32)>,
    ) -> zbus::fdo::Result<Vec<i32>> {
        for (id, event_id, _, _) in events {
            if event_id == "clicked" {
                let _ = self.tx.try_send(Command::MenuClicked(id));
            }
        }
        Ok(Vec::new())
    }

    async fn about_to_show(&self, _id: i32) -> bool {
        let _ = self.tx.try_send(Command::AboutToShow);
        false
    }

    async fn about_to_show_group(&self, _ids: Vec<i32>) -> (Vec<i32>, Vec<i32>) {
        let _ = self.tx.try_send(Command::AboutToShow);
        (Vec::new(), Vec::new())
    }

    #[zbus(signal)]
    pub async fn items_properties_updated(
        emitter: &SignalEmitter<'_>,
        updated: Vec<(i32, HashMap<String, OwnedValue>)>,
        removed: Vec<(i32, Vec<String>)>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn layout_updated(
        emitter: &SignalEmitter<'_>,
        revision: u32,
        parent: i32,
    ) -> zbus::Result<()>;
}
