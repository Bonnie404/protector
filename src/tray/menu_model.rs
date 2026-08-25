use std::collections::HashMap;
use zbus::zvariant::{OwnedValue, StructureBuilder, Type, Value};

/// Wraps a value for a DBusMenu property map. Menu properties are plain data —
/// never file descriptors — so this conversion cannot fail in practice.
pub fn own<'a, T: Into<Value<'a>>>(v: T) -> OwnedValue {
    OwnedValue::try_from(v.into()).expect("menu property values are plain data")
}

// Connect/Disconnect/Quit are part of the Action surface Task 3's D-Bus
// handlers dispatch on; nothing in Task 2 constructs them yet.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    SelectTask(String),
    Refresh,
    Connect,
    Disconnect,
    Quit,
    Inert,
}

#[derive(Debug, Clone)]
pub struct MenuItem {
    pub id: i32,
    pub label: String,
    pub enabled: bool,
    pub separator: bool,
    pub radio: Option<bool>,
    pub action: Action,
}

impl MenuItem {
    pub fn command(id: i32, label: &str, action: Action) -> Self {
        Self { id, label: label.into(), enabled: true, separator: false, radio: None, action }
    }
    pub fn disabled(id: i32, label: &str) -> Self {
        Self { id, label: label.into(), enabled: false, separator: false, radio: None, action: Action::Inert }
    }
    pub fn separator(id: i32) -> Self {
        Self { id, label: String::new(), enabled: false, separator: true, radio: None, action: Action::Inert }
    }
    pub fn radio(id: i32, label: &str, checked: bool, action: Action) -> Self {
        Self { id, label: label.into(), enabled: true, separator: false, radio: Some(checked), action }
    }

    fn properties(&self) -> HashMap<String, OwnedValue> {
        let mut p = HashMap::new();
        if self.separator {
            p.insert("type".into(), own("separator"));
            return p;
        }
        // DBusMenu reads a single underscore as a mnemonic marker.
        p.insert("label".into(), own(self.label.replace('_', "__")));
        p.insert("enabled".into(), own(self.enabled));
        p.insert("visible".into(), own(true));
        if let Some(checked) = self.radio {
            p.insert("toggle-type".into(), own("radio"));
            p.insert("toggle-state".into(), own(if checked { 1i32 } else { 0i32 }));
        }
        p
    }
}

// No Clone: OwnedValue is not reliably Clone in zvariant 5, and Layout is built
// fresh per GetLayout call, so it is never needed.
#[derive(Debug, serde::Serialize, Type)]
pub struct Layout {
    pub id: i32,
    pub properties: HashMap<String, OwnedValue>,
    pub children: Vec<OwnedValue>,
}

#[derive(Debug, Clone, Default)]
pub struct MenuModel {
    // Read by Task 3 when reporting the layout revision to DBusMenu clients.
    #[allow(dead_code)]
    pub revision: u32,
    pub items: Vec<MenuItem>,
}

impl MenuModel {
    pub fn new(items: Vec<MenuItem>) -> Self {
        Self { revision: 1, items }
    }

    pub fn action_for(&self, id: i32) -> Option<&Action> {
        self.items.iter().find(|i| i.id == id).map(|i| &i.action)
    }

    pub fn group_properties(&self, ids: &[i32]) -> Vec<(i32, HashMap<String, OwnedValue>)> {
        self.items
            .iter()
            .filter(|i| ids.is_empty() || ids.contains(&i.id))
            .map(|i| (i.id, i.properties()))
            .collect()
    }

    pub fn layout(&self) -> zbus::Result<Layout> {
        let mut children = Vec::with_capacity(self.items.len());
        for item in &self.items {
            let structure = StructureBuilder::new()
                .add_field(item.id)
                .add_field(item.properties())
                .add_field(Vec::<Value<'static>>::new())
                .build()?;
            children.push(OwnedValue::try_from(Value::from(structure))?);
        }
        let mut properties = HashMap::new();
        properties.insert("children-display".into(), own("submenu"));
        Ok(Layout { id: 0, properties, children })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::Type;

    fn sample() -> MenuModel {
        MenuModel::new(vec![
            MenuItem::radio(1, "Design review   14:00 \u{2013} 15:30", true, Action::SelectTask("e1".into())),
            MenuItem::separator(2),
            MenuItem::disabled(3, "Later today"),
            MenuItem::command(4, "Refresh now", Action::Refresh),
        ])
    }

    #[test]
    fn layout_has_the_signature_the_spec_requires() {
        assert_eq!(<(u32, Layout) as Type>::SIGNATURE.to_string(), "(u(ia{sv}av))");
    }

    #[test]
    fn root_reports_itself_as_a_submenu_with_every_item_as_a_child() {
        let layout = sample().layout().unwrap();
        assert_eq!(layout.id, 0);
        assert_eq!(layout.properties.get("children-display").unwrap(), &own("submenu"));
        assert_eq!(layout.children.len(), 4);
    }

    #[test]
    fn underscores_are_escaped_because_dbusmenu_treats_them_as_mnemonics() {
        let model = MenuModel::new(vec![MenuItem::command(1, "deep_work", Action::Refresh)]);
        let props = model.group_properties(&[1]);
        assert_eq!(props[0].1.get("label").unwrap(), &own("deep__work"));
    }

    #[test]
    fn a_radio_item_carries_its_toggle_state() {
        let props = sample().group_properties(&[1]);
        assert_eq!(props[0].1.get("toggle-type").unwrap(), &own("radio"));
        assert_eq!(props[0].1.get("toggle-state").unwrap(), &own(1i32));
    }

    #[test]
    fn a_separator_is_typed_and_carries_no_label() {
        let props = sample().group_properties(&[2]);
        assert_eq!(props[0].1.get("type").unwrap(), &own("separator"));
        assert!(props[0].1.get("label").is_none());
    }

    #[test]
    fn a_disabled_item_is_visible_but_not_enabled() {
        let props = sample().group_properties(&[3]);
        assert_eq!(props[0].1.get("enabled").unwrap(), &own(false));
        assert_eq!(props[0].1.get("visible").unwrap(), &own(true));
    }

    #[test]
    fn clicking_an_id_resolves_to_its_action() {
        let model = sample();
        assert!(matches!(model.action_for(1), Some(Action::SelectTask(id)) if id == "e1"));
        assert!(matches!(model.action_for(4), Some(Action::Refresh)));
        assert!(model.action_for(99).is_none());
    }
}
