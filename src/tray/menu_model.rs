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

/// Remembers which menu id each event was given, for the life of the process.
///
/// The guarantee it exists to provide, in one sentence: **an id, once handed
/// to an event, is never handed to a different event.** A DBusMenu `Event`
/// carries no revision, so a host can click an id against a layout it fetched
/// arbitrarily long ago — and `AboutToShow` fires a sync at exactly the moment
/// a menu opens, so a rebuild mid-open is likely rather than exotic. An id
/// that could change hands is a click that selects the wrong block.
///
/// **It must outlive a single menu**, which is why this is threaded through
/// `derive_ui` rather than created inside it. `main` owns one for the run
/// loop. A fresh one per derivation would give the ordinary event its slot
/// back every time — that much is stable on its own — but would reopen the
/// collision case below.
///
/// **Collisions are probed, not assumed away.** Two event ids can hash to one
/// slot, and two menu items may not share an id: the second would be
/// unreachable and `action_for` would answer clicks on it with the first
/// one's task, which is exactly the wrong-block selection this scheme exists
/// to prevent. So the loser of a collision takes the next free id above its
/// slot.
///
/// Probing is what makes the memo load-bearing rather than an optimisation.
/// Without it: A and B collide on slot S, A wins S and B is probed to S+1,
/// the host renders A at S; a sync drops A; a freshly built assignment would
/// hand S to B, and a stale click on the row labelled A would select B. With
/// the memo, S stays A's for good — the click finds nothing and is dropped —
/// and B keeps S+1 whether or not A is still listed.
///
/// Growth is one entry per distinct event id ever shown, and `calendar::fetch`
/// asks for one day at a time with `maxResults=50`, so it is bounded by
/// 50 a day: a few hundred entries a week, and a couple of megabytes a year
/// at that ceiling — far less in practice, since a real day holds nowhere
/// near fifty distinct blocks.
/// Nothing is evicted, deliberately — freeing an id is precisely how it would
/// come to mean two different blocks, which is the bug this closes.
#[derive(Debug, Default)]
pub struct TaskIdMemo {
    /// Every id ever handed out, including those whose event has since
    /// vanished from the calendar. Probing skips these, so a departed block's
    /// id is never reissued.
    taken: std::collections::HashSet<i32>,
    assigned: std::collections::HashMap<String, i32>,
}

impl TaskIdMemo {
    /// The id for `event_id`: the one it was given before, or a newly
    /// reserved one.
    ///
    /// Asked twice for the same event — which `calendar::partition` cannot
    /// produce, since Now and Later are disjoint — it answers with the same id
    /// both times rather than reserving a second one.
    pub fn id_for(&mut self, event_id: &str) -> i32 {
        if let Some(id) = self.assigned.get(event_id) {
            return *id;
        }
        let mut id = task_slot(event_id);
        while !self.taken.insert(id) {
            // Wraps within the task range, so probing can never walk into the
            // fixed ids or onto the root. Terminates because the ids handed
            // out are counted in hundreds and the range holds two billion.
            id = if id == i32::MAX { ids::FIRST_TASK } else { id + 1 };
        }
        self.assigned.insert(event_id.to_string(), id);
        id
    }

    /// How many events have been given an id. Only the growth test uses this.
    pub fn len(&self) -> usize {
        self.assigned.len()
    }

    pub fn is_empty(&self) -> bool {
        self.assigned.is_empty()
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

/// Flattens control characters out of a menu label.
///
/// Labels are single-line display strings, but their text comes from outside:
/// an event title is whatever the calendar owner typed, and a sync failure can
/// put a message from Google's API in front of the user. A newline or a stray
/// control byte in either would render as a mangled row rather than being
/// rejected, so they collapse to a space here — at the one point every label
/// passes through on its way to the panel.
fn sanitize_label(label: &str) -> String {
    label
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
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
        p.insert("label".into(), own(sanitize_label(&self.label).replace('_', "__")));
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
    fn control_characters_in_a_title_cannot_mangle_a_menu_row() {
        // An event title is whatever the calendar owner typed, and an offline
        // hint can carry a message from Google's API. Neither is trusted to be
        // one line.
        let model = MenuModel::new(vec![MenuItem::command(
            1,
            "Stand-up\nsecond line\tand a tab",
            Action::Refresh,
        )]);
        let props = model.group_properties(&[1]);
        assert_eq!(
            props[0].1.get("label").unwrap(),
            &own("Stand-up second line and a tab")
        );
    }

    #[test]
    fn a_sanitised_label_is_still_mnemonic_escaped() {
        let model = MenuModel::new(vec![MenuItem::command(1, "deep_work\nnow", Action::Refresh)]);
        let props = model.group_properties(&[1]);
        assert_eq!(props[0].1.get("label").unwrap(), &own("deep__work now"));
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
        // And a `TaskIdMemo` that has not seen the event before hands out exactly
        // that slot, whatever order the menu happens to list things in.
        assert_eq!(TaskIdMemo::default().id_for("e2"), task_slot("e2"));
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

        let mut ids = TaskIdMemo::default();
        let first = ids.id_for("e39516");
        let second = ids.id_for("e64020");
        assert_ne!(first, second, "two menu items may never share an id");
        assert_eq!(second, first + 1, "the loser of a collision takes the next free slot");

        // And the item that got there first keeps the slot its event hashes
        // to, so a collision costs the *other* block nothing but one id.
        assert_eq!(first, task_slot("e39516"));
    }

    #[test]
    fn the_memo_keeps_an_id_reserved_for_its_event_even_after_the_event_is_gone() {
        // The residual a per-menu assignment left open. A and B collide: A
        // takes the slot, B is probed one above, and the host is showing A at
        // that slot. A is then deleted from the calendar, so every later menu
        // asks only about B — and a freshly built assignment would give B the
        // slot the host still labels A.
        let mut memo = TaskIdMemo::default();
        let winner = memo.id_for("e39516");
        let loser = memo.id_for("e64020");
        assert_eq!(loser, winner + 1);

        for _ in 0..5 {
            assert_eq!(
                memo.id_for("e64020"),
                loser,
                "the survivor drifted onto the departed block's id"
            );
        }
        // And a block that comes back — a deletion undone, a recurring event
        // re-entering the day's window — is itself again, not somebody else.
        assert_eq!(memo.id_for("e39516"), winner);
    }

    #[test]
    fn the_memo_grows_by_one_per_distinct_event_not_per_menu() {
        // It is never evicted, so what it costs is worth pinning: an entry per
        // event ever shown, and nothing at all for redrawing the same menu —
        // which happens once a second.
        let mut memo = TaskIdMemo::default();
        assert!(memo.is_empty());
        for _ in 0..100 {
            memo.id_for("e1");
            memo.id_for("e2");
        }
        assert_eq!(memo.len(), 2, "a redraw must not cost an entry");
    }

    #[test]
    fn a_colliding_pair_stays_separately_clickable() {
        // The failure this prevents: one id in the menu, two blocks behind it,
        // and every click on either selecting whichever was listed first.
        let mut ids = TaskIdMemo::default();
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
