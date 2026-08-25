use std::collections::HashMap;
use zbus::zvariant::{OwnedValue, StructureBuilder, Type, Value};

/// Wraps a value for a DBusMenu property map. Menu properties are plain data —
/// never file descriptors — so this conversion cannot fail in practice.
pub fn own<'a, T: Into<Value<'a>>>(v: T) -> OwnedValue {
    OwnedValue::try_from(v.into()).expect("menu property values are plain data")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    SelectTask(String),
    Refresh,
    Connect,
    Disconnect,
    Quit,
    Inert,
}

/// Ids for the items that mean the same thing in every menu Protector draws.
///
/// They are constants rather than positions because a DBusMenu `Event` carries
/// no revision: the host sends only an id, and if it has not re-fetched the
/// layout since the menu was last rebuilt, that id is read against a menu that
/// no longer exists. A fixed id therefore has to keep meaning one thing for
/// the life of the process — and the task ids derived below have to stay out
/// of this range, which is what [`FIRST_TASK`] is for.
pub mod ids {
    /// `0` is the DBusMenu root and is never a menu item, so these start at 1.
    pub const EMPTY_DAY: i32 = 1;
    pub const NOT_CONNECTED: i32 = 2;
    pub const LATER_HEADER: i32 = 3;
    pub const LIST_SEPARATOR: i32 = 4;
    pub const OFFLINE: i32 = 5;
    pub const COMMAND_SEPARATOR: i32 = 6;
    pub const REFRESH: i32 = 7;
    /// *Connect Google Calendar…* and *Disconnect account* occupy the same
    /// place in the menu but get ids of their own on purpose. Only one is ever
    /// listed, so sharing an id would let a click on a stale *Connect…* item
    /// disconnect a freshly connected account — precisely the confusion these
    /// ids exist to prevent. With two ids the stale click resolves to nothing
    /// and is dropped.
    pub const CONNECT: i32 = 8;
    pub const DISCONNECT: i32 = 9;
    pub const QUIT: i32 = 10;

    /// Every fixed id, for the tests that pin their distinctness and their
    /// separation from the task range.
    pub const ALL: [i32; 10] = [
        EMPTY_DAY,
        NOT_CONNECTED,
        LATER_HEADER,
        LIST_SEPARATOR,
        OFFLINE,
        COMMAND_SEPARATOR,
        REFRESH,
        CONNECT,
        DISCONNECT,
        QUIT,
    ];

    /// The lowest id a task item can take. The gap above the constants is
    /// deliberate slack: a new fixed item can be added without moving a single
    /// task id.
    pub const FIRST_TASK: i32 = 256;
}

/// FNV-1a, 64 bit. Chosen for being fully specified in six lines, so the id a
/// given event gets is the same in every build of every version — unlike
/// `DefaultHasher`, which is explicitly allowed to change between releases.
/// Nothing here is security-sensitive: the hash picks a slot, and a collision
/// is handled rather than trusted away.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

/// The id a task item takes when nothing else has claimed it: a hash of the
/// **event id**, folded into `FIRST_TASK..=i32::MAX`.
///
/// Never `0` (the DBusMenu root) and never inside the fixed range, both by
/// construction.
pub fn task_slot(event_id: &str) -> i32 {
    let span = (i32::MAX as u64) - (ids::FIRST_TASK as u64) + 1;
    (ids::FIRST_TASK as u64 + fnv1a64(event_id.as_bytes()) % span) as i32
}

/// Hands out one id per task item while a menu is being built.
///
/// The point of deriving an id from the event rather than from the item's
/// position is that a menu rebuild — which happens on every sync, and
/// `AboutToShow` fires a sync at exactly the moment a menu opens — must not
/// change what an id means. A host that clicks against a layout it fetched
/// before the rebuild then still selects the block whose label it was
/// showing.
///
/// **Collisions are probed, not assumed away.** Two event ids can hash to one
/// slot, and two menu items may not share an id: the second one would be
/// unreachable and `action_for` would answer clicks on it with the first one's
/// task, which is exactly the wrong-block selection this whole scheme exists
/// to prevent. So the loser of a collision takes the next free id above its
/// slot. That makes the *pair* order-dependent — drop the winner from
/// tomorrow's list and the loser moves down to its own slot — which is a far
/// smaller exposure than position-derived ids: it needs a hash collision
/// between two blocks on the same day (roughly 1 in 2^31 per pair) before it
/// costs anything at all, and even then the stale id usually resolves to
/// nothing and is dropped.
#[derive(Debug, Default)]
pub struct TaskIds {
    taken: std::collections::HashSet<i32>,
}

impl TaskIds {
    /// The id for `event_id` in the menu being built. Calling it twice with
    /// the same event in one menu yields two different ids — the same
    /// treatment any other collision gets — rather than a duplicate.
    pub fn id_for(&mut self, event_id: &str) -> i32 {
        let mut id = task_slot(event_id);
        while !self.taken.insert(id) {
            // Wraps within the task range, so probing can never walk into the
            // fixed ids or onto the root. Terminates because a day's menu
            // holds a few dozen items and the range holds two billion.
            id = if id == i32::MAX { ids::FIRST_TASK } else { id + 1 };
        }
        id
    }
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
        assert!(!props[0].1.contains_key("label"));
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

    // ---- Content-derived item ids -------------------------------------------

    #[test]
    fn a_task_id_depends_on_the_event_and_on_nothing_else() {
        // The whole guarantee in one line: derive it again, in another process
        // or after any number of syncs, and it is the same id.
        assert_eq!(task_slot("e1"), task_slot("e1"));
        assert_ne!(task_slot("e1"), task_slot("e2"));
        // And a `TaskIds` that has not seen the event before hands out exactly
        // that slot, whatever order the menu happens to list things in.
        assert_eq!(TaskIds::default().id_for("e2"), task_slot("e2"));
    }

    #[test]
    fn a_derived_id_is_never_the_root_and_never_lands_on_a_fixed_item() {
        // `0` is the DBusMenu root, and an id that strayed into the fixed
        // range would let a click on a task fire *Quit*.
        for n in 0..20_000 {
            let id = task_slot(&format!("{n:x}abc123@google.com"));
            assert!(id >= ids::FIRST_TASK, "{id} is inside the fixed range");
            assert!(id > 0, "{id} claims the DBusMenu root");
        }
    }

    #[test]
    fn the_fixed_ids_are_distinct_nonzero_and_below_the_task_range() {
        let mut seen = std::collections::HashSet::new();
        for id in ids::ALL {
            assert_ne!(id, 0, "0 is the DBusMenu root, not a menu item");
            assert!(id < ids::FIRST_TASK, "{id} reaches into the task range");
            assert!(seen.insert(id), "{id} is used by two different fixed items");
        }
    }

    #[test]
    fn two_events_that_hash_to_the_same_slot_still_get_ids_of_their_own() {
        // Found by walking `task_slot` over `e0`, `e1`, … until two of them
        // landed on one slot. The first assertion is deliberate: if the hash
        // ever changes, this test fails loudly asking for a fresh pair rather
        // than quietly stopping to exercise the collision path.
        assert_eq!(
            task_slot("e39516"),
            task_slot("e64020"),
            "these two event ids no longer collide — find another pair"
        );

        let mut ids = TaskIds::default();
        let first = ids.id_for("e39516");
        let second = ids.id_for("e64020");
        assert_ne!(first, second, "two menu items may never share an id");
        assert_eq!(second, first + 1, "the loser of a collision takes the next free slot");

        // And the item that got there first keeps the slot its event hashes
        // to, so a collision costs the *other* block nothing but one id.
        assert_eq!(first, task_slot("e39516"));
    }

    #[test]
    fn a_colliding_pair_stays_separately_clickable() {
        // The failure this prevents: one id in the menu, two blocks behind it,
        // and every click on either selecting whichever was listed first.
        let mut ids = TaskIds::default();
        let a = ids.id_for("e39516");
        let b = ids.id_for("e64020");
        let model = MenuModel::new(vec![
            MenuItem::radio(a, "Deep work", false, Action::SelectTask("e39516".into())),
            MenuItem::radio(b, "Standup", false, Action::SelectTask("e64020".into())),
        ]);
        assert!(matches!(model.action_for(a), Some(Action::SelectTask(id)) if id == "e39516"));
        assert!(matches!(model.action_for(b), Some(Action::SelectTask(id)) if id == "e64020"));
    }
}
