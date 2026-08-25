use chrono::{DateTime, Local};

use crate::task::{panel_label, Selection, Task};
use crate::tray::menu_model::{ids, Action, MenuItem, MenuModel, TaskIdMemo};
use crate::tray::UiState;

#[derive(Debug, Clone)]
pub struct AppState {
    pub tasks_now: Vec<Task>,
    pub tasks_later: Vec<Task>,
    pub selection: Option<Selection>,
    pub connected: bool,
    pub last_sync: Option<DateTime<Local>>,
    pub last_error: Option<String>,
    /// A short, menu-safe reason for `last_error`, when the failure was
    /// specific enough to produce one that fits the tray's width without
    /// truncating a sentence mid-way — `None` for a generic failure (a
    /// network hiccup, a timeout), where the full detail belongs in the log
    /// alone. Set alongside `last_error` by [`crate::sync::apply_sync`].
    pub last_error_hint: Option<String>,
    pub revision: u32,
    /// Whether `tasks_now`/`tasks_later` are what a sync actually returned.
    ///
    /// An empty list is only evidence of a free day if a sync put it there.
    /// Before the first one lands — a cold start, a first run with no network
    /// — the lists are empty because nothing has filled them yet, which is a
    /// different thing entirely and must not be reported as `Nothing
    /// scheduled today`. Cleared again whenever the lists are emptied by
    /// something other than a sync, so a reconnect does not inherit the
    /// previous account's answer.
    ///
    /// Deliberately not `last_sync.is_some()`: `last_sync` is restored from
    /// `state.json` at startup so the offline marker can say *when*, which
    /// would make a second run claim a free day before it had fetched
    /// anything.
    pub synced: bool,
    /// How long before the selected task ends the heads-up fires, from
    /// `config.warn_before_minutes` (spec §8) via [`warn_before_secs`].
    /// Held here rather than read from the config at the notification site,
    /// so the timing and the wording of the message can never disagree.
    pub warn_before_secs: i64,
}

/// Hand-written rather than derived, so that `..Default::default()` — which
/// `main` and several tests use — yields the *documented* warning window
/// rather than `0`, which would silently mean "never warn".
impl Default for AppState {
    fn default() -> Self {
        Self {
            tasks_now: Vec::new(),
            tasks_later: Vec::new(),
            selection: None,
            connected: false,
            last_sync: None,
            last_error: None,
            last_error_hint: None,
            revision: 0,
            synced: false,
            warn_before_secs: WARN_BEFORE_SECS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    Sync,
    StartLogin,
    Logout,
    Quit,
    /// The heads-up before the selected block ends, carrying the seconds that
    /// were actually left when it fired.
    ///
    /// The number travels with the effect rather than being recomputed at the
    /// notification site, because the only other number in reach there is
    /// `warn_before_secs` — the configured *window* — and rendering that is
    /// exactly the bug this payload exists to make unrepresentable. The window
    /// is an upper bound on this value, never equal to it except by
    /// coincidence: a block selected four minutes before it ends warns on the
    /// next tick however wide the window is.
    NotifyWarning(i64),
    NotifyEnded,
    /// The selected event is gone from the calendar; the countdown it was
    /// driving has been dropped and the user has to be told, since nothing on
    /// screen would otherwise explain the label falling back to `Pick a task`.
    NotifyRemoved,
    /// The refresh token was revoked at Google's end — distinct from an
    /// ordinary failed sync, which only means the network could not be
    /// reached. Fired by `token_revoked`, never by `tick` or `reconcile`.
    NotifyRevoked,
    Persist,
}

/// The warning window used when `config.toml` says nothing — spec §8
/// documents `warn_before_minutes = 5`.
pub const WARN_BEFORE_SECS: i64 = 300;

/// Turns the configured `warn_before_minutes` into the seconds `tick`
/// compares against.
///
/// Zero and negative both mean *no heads-up at all*: "warn me 0 minutes
/// before it ends" is the end notification, which already fires, and a
/// negative window would mean warning after the fact. Neither is worth
/// refusing to start over, so both disable the T-5 notification and `run`
/// says so once on stderr.
///
/// Saturating rather than wrapping: `i64::MAX * 60` overflows, and a mistyped
/// config must not panic the widget in a debug build.
pub fn warn_before_secs(minutes: i64) -> i64 {
    minutes.max(0).saturating_mul(60)
}

fn item_label(t: &Task) -> String {
    format!("{}   {} \u{2013} {}", t.title, t.start.format("%H:%M"), t.end.format("%H:%M"))
}

fn push_tasks(
    items: &mut Vec<MenuItem>,
    task_ids: &mut TaskIdMemo,
    tasks: &[Task],
    selected_id: Option<&str>,
) {
    for t in tasks {
        let checked = selected_id == Some(t.id.as_str());
        let id = task_ids.id_for(&t.id);
        items.push(MenuItem::radio(id, &item_label(t), checked, Action::SelectTask(t.id.clone())));
    }
}

/// Turns the current `AppState` into what the panel should show. Deterministic:
/// the same state and instant always produce the same `UiState`, so callers can
/// derive as often as they like. The only thing it writes is the id memo below,
/// and only to reserve an id for an event it has not seen before.
///
/// Every id here is derived from *what the item is* — a constant for the fixed
/// items, a hash of the event id for the tasks — and never from its position
/// in the list. See [`TaskIdMemo`] for why: a DBusMenu `Event` carries no
/// revision, so an id has to survive a rebuild with its meaning intact.
///
/// `task_ids` is the one thing here that is not derived afresh, and it is an
/// argument rather than a local for exactly that reason: it has to outlive the
/// menu it is building, or a departed block's id could be reissued to another
/// block. Passing the same memo on every call is what makes the ids mean one
/// thing for the life of the process; `main` holds it beside the menu it last
/// published. Deriving the same state twice with the same memo still yields
/// the same `UiState`, so this stays as replayable as it was.
pub fn derive_ui(state: &AppState, now: DateTime<Local>, task_ids: &mut TaskIdMemo) -> UiState {
    let mut items = Vec::new();
    let selected_id = state.selection.as_ref().map(|s| s.task.id.as_str());

    if state.connected {
        if state.tasks_now.is_empty() && state.tasks_later.is_empty() {
            // Two different empty menus, and saying the wrong one is a claim
            // the program cannot back: `Nothing scheduled today` above
            // `⚠ Offline — synced never` told a first-run user with no network
            // that their day was free.
            let empty =
                if state.synced { "Nothing scheduled today" } else { "Loading today\u{2026}" };
            items.push(MenuItem::disabled(ids::EMPTY_DAY, empty));
        } else {
            push_tasks(&mut items, task_ids, &state.tasks_now, selected_id);
            if !state.tasks_later.is_empty() {
                if !state.tasks_now.is_empty() {
                    items.push(MenuItem::separator(ids::LIST_SEPARATOR));
                }
                items.push(MenuItem::disabled(ids::LATER_HEADER, "Later today"));
                push_tasks(&mut items, task_ids, &state.tasks_later, selected_id);
            }
        }
    } else {
        items.push(MenuItem::disabled(ids::NOT_CONNECTED, "Not connected"));
    }

    if state.last_error.is_some() {
        let synced = state
            .last_sync
            .map(|t| t.format("%H:%M").to_string())
            .unwrap_or_else(|| "never".into());
        // `last_error_hint` is only ever `Some` when it already fits the
        // tray's width (see its doc comment) — appending it here never
        // truncates. When there is nothing concise to say, the item stays
        // exactly as it always has; the full detail is in the log either way.
        let label = match &state.last_error_hint {
            Some(hint) => format!("\u{26a0} Offline \u{2014} synced {synced} \u{2014} {hint}"),
            None => format!("\u{26a0} Offline \u{2014} synced {synced}"),
        };
        items.push(MenuItem::disabled(ids::OFFLINE, &label));
    }

    items.push(MenuItem::separator(ids::COMMAND_SEPARATOR));
    items.push(MenuItem::command(ids::REFRESH, "Refresh now", Action::Refresh));
    if state.connected {
        items.push(MenuItem::command(ids::DISCONNECT, "Disconnect account", Action::Disconnect));
    } else {
        items.push(MenuItem::command(
            ids::CONNECT,
            "Connect Google Calendar\u{2026}",
            Action::Connect,
        ));
    }
    items.push(MenuItem::command(ids::QUIT, "Quit", Action::Quit));

    let mut menu = MenuModel::new(items);
    // +1 so the very first change after startup (revision 0) is still an increase.
    menu.revision = state.revision + 1;

    UiState {
        label: panel_label(state.selection.as_ref(), state.connected, now),
        attention: state
            .selection
            .as_ref()
            .map(|s| s.task.end <= now)
            .unwrap_or(false),
        menu,
    }
}

/// What the end-of-block chooser offers once `ended` runs out, in
/// chronological order: everything still running now, then everything later
/// today.
///
/// `tasks_now` is chained in ahead of `tasks_later` rather than ignored,
/// because a block that is *already running* is the soonest thing there is to
/// switch to. Two ways that happens in practice: overlapping calendar
/// entries, and a resume from suspend where the sync that ran on wake has
/// already moved the next block out of `tasks_later` and into `tasks_now`.
/// Drawing only from `tasks_later` gave neither of them a button — and
/// `notify::ended_body` did not even name them, so they were invisible.
///
/// The order holds because `calendar::partition` splits an already
/// start-ordered list at `now`: every `tasks_now` entry started at or before
/// every `tasks_later` entry, so "the three soonest" stays true after the
/// chain.
///
/// The block that just ended is excluded by id — it is usually still in
/// `tasks_now`, since the next sync is up to five minutes away — because
/// offering the user the thing they just finished is not a choice.
pub fn end_candidates(state: &AppState, ended: &Task) -> Vec<Task> {
    state
        .tasks_now
        .iter()
        .filter(|t| t.id != ended.id)
        .chain(state.tasks_later.iter())
        .cloned()
        .collect()
}

fn find_task<'a>(state: &'a AppState, id: &str) -> Option<&'a Task> {
    state.tasks_now.iter().chain(state.tasks_later.iter()).find(|t| t.id == id)
}

/// Handles one menu action against the state, returning the effects the caller
/// (main's run loop) must carry out. Every action bumps `state.revision`, since
/// every action changes what the menu should look like (a check mark moves, the
/// task list clears, etc.) even when the visible label doesn't.
pub fn apply(state: &mut AppState, action: &Action) -> Vec<Effect> {
    state.revision += 1;
    match action {
        Action::SelectTask(id) => {
            let already = state.selection.as_ref().map(|s| s.task.id == *id).unwrap_or(false);
            if already {
                state.selection = None;
            } else if let Some(t) = find_task(state, id).cloned() {
                state.selection = Some(Selection { task: t, warned: false, ended_notified: false });
            }
            vec![Effect::Persist]
        }
        Action::Refresh => vec![Effect::Sync],
        Action::Connect => vec![Effect::StartLogin],
        Action::Disconnect => {
            state.connected = false;
            state.selection = None;
            state.tasks_now.clear();
            state.tasks_later.clear();
            // The lists are empty because they were emptied, not because the
            // day is free; reconnecting must wait for a real answer before
            // claiming otherwise.
            state.synced = false;
            // Leaving this set would hang a `⚠ Offline` item under
            // `Not connected` for the rest of the session. Choosing to
            // disconnect is not a failure to reach Google.
            state.last_error = None;
            vec![Effect::Logout, Effect::Persist]
        }
        Action::Quit => vec![Effect::Quit],
        Action::Inert => vec![],
    }
}

/// Advances the clock: fires the one-time warning and the one-time overtime
/// notification as the selected task's countdown crosses each threshold.
pub fn tick(state: &mut AppState, now: DateTime<Local>) -> Vec<Effect> {
    let mut effects = Vec::new();
    let warn_before = state.warn_before_secs;
    if let Some(sel) = state.selection.as_mut() {
        let remaining = (sel.task.end - now).num_seconds();
        if remaining <= 0 && !sel.ended_notified {
            sel.ended_notified = true;
            sel.warned = true;
            effects.push(Effect::NotifyEnded);
            effects.push(Effect::Persist);
        // No special case for a disabled window: `warn_before == 0` makes
        // `remaining > 0 && remaining <= 0` unsatisfiable, and the branch
        // above has already handled everything at or past the end.
        } else if remaining > 0 && remaining <= warn_before && !sel.warned {
            sel.warned = true;
            effects.push(Effect::NotifyWarning(remaining));
            effects.push(Effect::Persist);
        }
    }
    effects
}

/// Folds a fresh event list into the selection.
///
/// Matching is by event id, never by position: the two lists are rebuilt from
/// scratch on every sync, and a task that ended or was added shifts everything
/// after it. An id is the only thing that still means the same event five
/// minutes later.
///
/// Note what this does *not* touch: `tasks_now` and `tasks_later`. Replacing
/// those is the caller's job (`sync::apply_sync`), because it is the caller
/// that knows whether the fresh list arrived at all.
pub fn reconcile(state: &mut AppState, fresh: &[Task]) -> Vec<Effect> {
    state.revision += 1;
    let mut effects = vec![Effect::Persist];
    if let Some(sel) = state.selection.as_mut() {
        match fresh.iter().find(|t| t.id == sel.task.id) {
            Some(updated) => {
                if updated.end != sel.task.end {
                    // The block moved; the countdown follows it, and the
                    // one-shot notifications get another chance. Left alone,
                    // an event pushed back an hour would never warn again.
                    sel.warned = false;
                    sel.ended_notified = false;
                }
                sel.task = updated.clone();
            }
            None => {
                state.selection = None;
                effects.push(Effect::NotifyRemoved);
            }
        }
    }
    effects
}

/// Handles a refresh token that Google has revoked (spec §9): distinct from
/// an ordinary sync failure, which only ever raises `last_error` and leaves
/// the last good task list in place. A revocation instead disconnects
/// outright — the same state change `Action::Disconnect` makes, so the label
/// falls back to `Connect calendar` — and adds exactly one notification.
///
/// This does not go through `sync::apply_sync`: that function's `Result<Vec<Task>,
/// String>` has already lost the distinction by the time it would see it, and
/// folding "revoked" into it would risk it silently decaying into the
/// ordinary `⚠ Offline` path on the next refactor. `sync.rs` detects the
/// revocation itself and calls this instead.
///
/// Idempotent by checking `state.connected` first: called again once already
/// disconnected — a duplicate command, a race between two callers — this is a
/// no-op, so nothing here can double-notify on its own. (`sync.rs` separately
/// guarantees the command itself is sent at most once, by ending the sync
/// task the moment it detects the revocation.)
pub fn token_revoked(state: &mut AppState) -> Vec<Effect> {
    if !state.connected {
        return vec![];
    }
    state.connected = false;
    state.selection = None;
    state.tasks_now.clear();
    state.tasks_later.clear();
    // Same reasoning as `Action::Disconnect`: emptied, not free.
    state.synced = false;
    // Leaving this set would hang a `⚠ Offline` item under `Not connected`
    // for the rest of the session, same reasoning as `Action::Disconnect`.
    state.last_error = None;
    state.revision += 1;
    vec![Effect::Logout, Effect::NotifyRevoked, Effect::Persist]
}

/// How long to wait before retrying after `consecutive_failures` failed syncs:
/// 30s, 60s, 120s, 240s, then 300s forever (spec §9).
pub fn backoff(consecutive_failures: u32) -> std::time::Duration {
    const BASE_SECS: u64 = 30;
    const CEILING_SECS: u64 = 300;
    // Clamped *before* the shift rather than after: `30u64 << 64` panics in a
    // debug build, and a widget that has been offline all day is exactly the
    // case that would reach it.
    let doublings = consecutive_failures.min(4);
    std::time::Duration::from_secs((BASE_SECS << doublings).min(CEILING_SECS))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32, m: u32) -> DateTime<Local> { Local.with_ymd_and_hms(2026, 8, 25, h, m, 0).unwrap() }

    fn task(id: &str, title: &str, s: (u32, u32), e: (u32, u32)) -> Task {
        Task { id: id.into(), title: title.into(), start: at(s.0, s.1), end: at(e.0, e.1) }
    }

    /// `derive_ui` with a memo of its own. Used by every test that is about
    /// labels, the panel text or effects rather than about ids; the id tests
    /// below keep a memo across calls on purpose, which is the whole point of
    /// it.
    fn ui(state: &AppState, now: DateTime<Local>) -> UiState {
        derive_ui(state, now, &mut TaskIdMemo::default())
    }

    fn connected_state() -> AppState {
        AppState {
            tasks_now: vec![task("e1", "Design review", (14, 0), (15, 30))],
            tasks_later: vec![task("e2", "Deep work", (15, 30), (17, 0))],
            selection: None,
            connected: true,
            last_sync: Some(at(14, 3)),
            last_error: None,
            last_error_hint: None,
            revision: 0,
            synced: true,
            warn_before_secs: WARN_BEFORE_SECS,
        }
    }

    #[test]
    fn menu_lists_now_then_later_with_a_header() {
        let ui = ui(&connected_state(), at(14, 6));
        let labels: Vec<&str> = ui.menu.items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels[0].starts_with("Design review"));
        assert!(labels.contains(&"Later today"));
        assert!(labels.iter().any(|l| l.starts_with("Deep work")));
        assert!(labels.contains(&"Refresh now"));
        assert!(labels.contains(&"Quit"));
    }

    #[test]
    fn selecting_a_task_checks_it_and_starts_the_countdown() {
        let mut state = connected_state();
        apply(&mut state, &Action::SelectTask("e1".into()));
        assert_eq!(state.selection.as_ref().unwrap().task.id, "e1");
        let ui = ui(&state, at(14, 6));
        assert_eq!(ui.label, "1:24:00 \u{b7} Design review");
        assert_eq!(ui.menu.items[0].radio, Some(true));
    }

    #[test]
    fn selecting_the_active_task_again_clears_it() {
        let mut state = connected_state();
        apply(&mut state, &Action::SelectTask("e1".into()));
        apply(&mut state, &Action::SelectTask("e1".into()));
        assert!(state.selection.is_none());
        assert_eq!(ui(&state, at(14, 6)).label, "Pick a task");
    }

    #[test]
    fn past_the_end_the_item_asks_for_attention() {
        let mut state = connected_state();
        apply(&mut state, &Action::SelectTask("e1".into()));
        let ui = ui(&state, at(15, 34));
        assert!(ui.attention);
        assert_eq!(ui.label, "\u{26a0} +04:00 \u{b7} Design review");
    }

    #[test]
    fn an_empty_day_says_so() {
        let mut state = connected_state();
        state.tasks_now.clear();
        state.tasks_later.clear();
        let ui = ui(&state, at(14, 6));
        assert!(ui.menu.items.iter().any(|i| i.label == "Nothing scheduled today" && !i.enabled));
    }

    #[test]
    fn a_day_that_has_never_been_fetched_does_not_claim_to_be_free() {
        // A first run with no network: the lists are empty because nothing
        // has filled them, not because the calendar is.
        let state = AppState { connected: true, last_error: Some("timeout".into()), ..Default::default() };
        let ui = ui(&state, at(14, 6));
        let labels: Vec<&str> = ui.menu.items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            !labels.contains(&"Nothing scheduled today"),
            "claimed a free day without ever having asked: {labels:?}"
        );
        assert!(labels.iter().any(|l| l.starts_with("Loading today")), "{labels:?}");
        assert!(labels.iter().any(|l| l.starts_with("\u{26a0} Offline")), "{labels:?}");
    }

    #[test]
    fn a_second_run_does_not_inherit_yesterdays_sync_as_an_answer_about_today() {
        // `last_sync` is restored from `state.json` so the offline marker can
        // say *when* — which must not be mistaken for having fetched today.
        let state = AppState { connected: true, last_sync: Some(at(9, 0)), ..Default::default() };
        let labels: Vec<String> =
            ui(&state, at(14, 6)).menu.items.iter().map(|i| i.label.clone()).collect();
        assert!(!labels.iter().any(|l| l == "Nothing scheduled today"), "{labels:?}");
    }

    #[test]
    fn a_sync_that_really_did_come_back_empty_still_says_so() {
        let mut state = connected_state();
        state.tasks_now.clear();
        state.tasks_later.clear();
        assert!(state.synced);
        let ui = ui(&state, at(14, 6));
        assert!(ui.menu.items.iter().any(|i| i.label == "Nothing scheduled today" && !i.enabled));
    }

    #[test]
    fn disconnecting_stops_the_menu_claiming_the_next_account_has_a_free_day() {
        let mut state = connected_state();
        apply(&mut state, &Action::Disconnect);
        assert!(!state.synced, "the lists were emptied, not fetched");
    }

    #[test]
    fn a_failed_sync_is_visible_in_the_menu() {
        let mut state = connected_state();
        state.last_error = Some("timeout".into());
        let ui = ui(&state, at(14, 6));
        assert!(ui.menu.items.iter().any(|i| i.label.starts_with("\u{26a0} Offline \u{2014} synced 14:03") && !i.enabled));
    }

    #[test]
    fn without_credentials_the_menu_offers_to_connect() {
        let mut state = connected_state();
        state.connected = false;
        state.tasks_now.clear();
        state.tasks_later.clear();
        let ui = ui(&state, at(14, 6));
        assert_eq!(ui.label, "Connect calendar");
        assert!(ui.menu.items.iter().any(|i| i.label == "Connect Google Calendar\u{2026}"));
        assert!(ui.menu.items.iter().any(|i| i.label == "Not connected" && !i.enabled));
    }

    #[test]
    fn crossing_the_end_emits_one_ended_effect_only() {
        let mut state = connected_state();
        apply(&mut state, &Action::SelectTask("e1".into()));
        let first = tick(&mut state, at(15, 31));
        let second = tick(&mut state, at(15, 32));
        assert!(first.iter().any(|e| matches!(e, Effect::NotifyEnded)));
        assert!(!second.iter().any(|e| matches!(e, Effect::NotifyEnded)));
    }

    #[test]
    fn a_configured_warning_window_is_what_tick_actually_gates_on() {
        // The whole point of `warn_before_minutes`: 15 in the config has to
        // mean fifteen, not the hardcoded five.
        let mut state = connected_state();
        state.warn_before_secs = warn_before_secs(15);
        apply(&mut state, &Action::SelectTask("e1".into()));
        // 14:45 is ten minutes before the 15:30 end: inside a 15 minute
        // window, well outside the default 5 minute one.
        let effects = tick(&mut state, at(15, 20));
        assert!(
            effects.iter().any(|e| matches!(e, Effect::NotifyWarning(_))),
            "a 15 minute window must warn 10 minutes out: {effects:?}"
        );

        let mut default_window = connected_state();
        apply(&mut default_window, &Action::SelectTask("e1".into()));
        let effects = tick(&mut default_window, at(15, 20));
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::NotifyWarning(_))),
            "and the default 5 minute window must not: {effects:?}"
        );
    }

    #[test]
    fn the_warning_carries_the_time_left_rather_than_the_configured_window() {
        // A block picked with four minutes to go, under a 30 minute window:
        // the heads-up fires on the very next tick, and the number it carries
        // has to be the four minutes, not the thirty. `main` had nothing but
        // the window to render, so the panel said `30 minutes left`.
        let mut state = connected_state();
        state.warn_before_secs = warn_before_secs(30);
        apply(&mut state, &Action::SelectTask("e1".into()));
        let effects = tick(&mut state, at(15, 26));
        assert!(
            effects.contains(&Effect::NotifyWarning(240)),
            "expected the four minutes actually left: {effects:?}"
        );
    }

    #[test]
    fn a_zero_warning_window_turns_the_heads_up_off_without_touching_the_end() {
        let mut state = connected_state();
        state.warn_before_secs = warn_before_secs(0);
        apply(&mut state, &Action::SelectTask("e1".into()));
        for minute in 20..30 {
            let effects = tick(&mut state, at(15, minute));
            assert!(
                !effects.iter().any(|e| matches!(e, Effect::NotifyWarning(_))),
                "a zero window must never warn (at 15:{minute}): {effects:?}"
            );
        }
        // The end notification is a separate promise and still has to land.
        let effects = tick(&mut state, at(15, 31));
        assert!(effects.iter().any(|e| matches!(e, Effect::NotifyEnded)), "{effects:?}");
    }

    #[test]
    fn a_nonsense_warning_window_is_clamped_rather_than_panicking() {
        assert_eq!(warn_before_secs(15), 900);
        assert_eq!(warn_before_secs(5), WARN_BEFORE_SECS);
        assert_eq!(warn_before_secs(0), 0, "zero means no heads-up");
        assert_eq!(warn_before_secs(-5), 0, "and so does a negative window");
        // `i64::MAX * 60` overflows; a mistyped config must not panic a debug build.
        assert_eq!(warn_before_secs(i64::MAX), i64::MAX);
    }

    #[test]
    fn an_unconfigured_state_still_warns_at_the_documented_five_minutes() {
        // `..Default::default()` is how `main` builds its state, so the
        // default must be the documented window and not a silent zero.
        assert_eq!(AppState::default().warn_before_secs, WARN_BEFORE_SECS);
    }

    #[test]
    fn the_five_minute_warning_fires_once() {
        let mut state = connected_state();
        apply(&mut state, &Action::SelectTask("e1".into()));
        let first = tick(&mut state, at(15, 26));
        let second = tick(&mut state, at(15, 27));
        assert!(first.iter().any(|e| matches!(e, Effect::NotifyWarning(_))));
        assert!(!second.iter().any(|e| matches!(e, Effect::NotifyWarning(_))));
    }

    #[test]
    fn disconnecting_clears_a_stale_offline_warning() {
        let mut state = connected_state();
        state.last_error = Some("the calendar did not answer within 30s".into());
        apply(&mut state, &Action::Disconnect);
        let ui = ui(&state, at(14, 6));
        assert!(
            !ui.menu.items.iter().any(|i| i.label.starts_with("\u{26a0} Offline")),
            "disconnecting on purpose is not the same as being offline: {:?}",
            ui.menu.items.iter().map(|i| &i.label).collect::<Vec<_>>()
        );
    }

    // ---- What the end-of-block chooser offers --------------------------------

    #[test]
    fn a_block_already_running_when_the_selected_one_ends_is_offered_first() {
        // Overlapping calendar entries, or a resume from suspend where the
        // sync on wake already moved the next block into `tasks_now`.
        let mut state = connected_state();
        state.tasks_now.push(task("e9", "Pair programming", (14, 30), (16, 30)));
        apply(&mut state, &Action::SelectTask("e1".into()));
        let ended = state.selection.as_ref().unwrap().task.clone();

        let ids: Vec<String> =
            end_candidates(&state, &ended).into_iter().map(|t| t.id).collect();
        assert_eq!(
            ids,
            vec!["e9", "e2"],
            "the running block has to come first, and chronological order has to survive"
        );
    }

    #[test]
    fn the_block_that_just_ended_is_never_offered_back() {
        let mut state = connected_state();
        apply(&mut state, &Action::SelectTask("e1".into()));
        let ended = state.selection.as_ref().unwrap().task.clone();
        // e1 is still in `tasks_now` — the next sync is up to five minutes away.
        assert!(state.tasks_now.iter().any(|t| t.id == "e1"));
        let ids: Vec<String> =
            end_candidates(&state, &ended).into_iter().map(|t| t.id).collect();
        assert_eq!(ids, vec!["e2"]);
    }

    #[test]
    fn an_empty_rest_of_day_offers_nothing_rather_than_failing() {
        let mut state = connected_state();
        state.tasks_later.clear();
        let ended = state.tasks_now[0].clone();
        assert!(end_candidates(&state, &ended).is_empty());
    }

    // ---- Reconciliation against a fresh sync --------------------------------

    #[test]
    fn a_moved_end_time_retargets_the_countdown() {
        let mut state = connected_state();
        apply(&mut state, &Action::SelectTask("e1".into()));
        let moved = vec![task("e1", "Design review", (14, 0), (16, 0))];
        reconcile(&mut state, &moved);
        assert_eq!(state.selection.as_ref().unwrap().task.end, at(16, 0));
    }

    #[test]
    fn a_moved_end_time_re_arms_the_one_shot_notifications() {
        let mut state = connected_state();
        apply(&mut state, &Action::SelectTask("e1".into()));
        // The countdown ran out and both one-shots fired against the old end.
        tick(&mut state, at(15, 31));
        assert!(state.selection.as_ref().unwrap().ended_notified);
        reconcile(&mut state, &[task("e1", "Design review", (14, 0), (16, 0))]);
        let sel = state.selection.as_ref().unwrap();
        assert!(!sel.warned, "the new end deserves its own warning");
        assert!(!sel.ended_notified, "and its own end notification");
    }

    #[test]
    fn a_deleted_event_clears_the_selection_and_says_so() {
        let mut state = connected_state();
        apply(&mut state, &Action::SelectTask("e1".into()));
        let effects = reconcile(&mut state, &[task("e2", "Deep work", (15, 30), (17, 0))]);
        assert!(state.selection.is_none());
        assert!(effects.iter().any(|e| matches!(e, Effect::NotifyRemoved)));
    }

    #[test]
    fn a_renamed_event_keeps_the_selection() {
        let mut state = connected_state();
        apply(&mut state, &Action::SelectTask("e1".into()));
        reconcile(&mut state, &[task("e1", "Design review (moved)", (14, 0), (15, 30))]);
        assert_eq!(state.selection.as_ref().unwrap().task.title, "Design review (moved)");
    }

    #[test]
    fn an_unchanged_event_keeps_its_one_shot_flags() {
        let mut state = connected_state();
        apply(&mut state, &Action::SelectTask("e1".into()));
        tick(&mut state, at(15, 26));
        assert!(state.selection.as_ref().unwrap().warned);
        reconcile(&mut state, &[task("e1", "Design review", (14, 0), (15, 30))]);
        assert!(
            state.selection.as_ref().unwrap().warned,
            "a sync that changed nothing must not warn the user twice"
        );
    }

    #[test]
    fn reconciliation_matches_by_event_id_not_by_list_position() {
        let mut state = connected_state();
        apply(&mut state, &Action::SelectTask("e2".into()));
        // e1 dropped off the front, so e2 is now the first entry.
        reconcile(
            &mut state,
            &[task("e2", "Deep work", (15, 30), (17, 0)), task("e3", "Standup", (17, 30), (18, 0))],
        );
        assert_eq!(state.selection.as_ref().unwrap().task.id, "e2");
    }

    #[test]
    fn a_sync_with_nothing_selected_is_harmless() {
        let mut state = connected_state();
        let effects = reconcile(&mut state, &[]);
        assert!(state.selection.is_none());
        assert!(!effects.iter().any(|e| matches!(e, Effect::NotifyRemoved)));
    }

    #[test]
    fn backoff_grows_and_then_holds_at_five_minutes() {
        assert_eq!(backoff(0).as_secs(), 30);
        assert_eq!(backoff(1).as_secs(), 60);
        assert_eq!(backoff(2).as_secs(), 120);
        assert_eq!(backoff(3).as_secs(), 240);
        assert_eq!(backoff(4).as_secs(), 300);
        assert_eq!(backoff(9).as_secs(), 300);
        // A counter that has run away for hours must still be a 5 minute wait,
        // not an overflow panic or a zero-length sleep that hammers the API.
        assert_eq!(backoff(u32::MAX).as_secs(), 300);
    }

    // ---- Menu ids ------------------------------------------------------------

    /// The id the menu gives the item for `event`.
    fn id_of(menu: &MenuModel, event: &str) -> i32 {
        menu.items
            .iter()
            .find(|i| i.action == Action::SelectTask(event.into()))
            .unwrap_or_else(|| panic!("{event} is not in the menu"))
            .id
    }

    #[test]
    fn deriving_the_same_menu_twice_gives_every_item_the_same_id() {
        let state = connected_state();
        let first: Vec<i32> = ui(&state, at(14, 6)).menu.items.iter().map(|i| i.id).collect();
        let again: Vec<i32> = ui(&state, at(14, 6)).menu.items.iter().map(|i| i.id).collect();
        assert_eq!(first, again);
    }

    #[test]
    fn a_click_that_races_a_sync_still_selects_the_block_the_user_saw() {
        // The hazard this closes: a DBusMenu `Event` carries no revision, so a
        // host that has not re-fetched the layout clicks against the menu it
        // last drew. With position-derived ids, a sync that dropped the first
        // block renumbered everything below it, and "Deep work" selected
        // whatever had taken its place.
        let mut state = connected_state();
        state.tasks_now = vec![
            task("e1", "Design review", (14, 0), (15, 30)),
            task("e2", "Deep work", (15, 30), (17, 0)),
        ];
        state.tasks_later = vec![task("e3", "Standup", (17, 30), (18, 0))];
        // One memo across both derivations, the way the run loop holds one
        // across the whole process.
        let mut ids = TaskIdMemo::default();
        // What the host was given, and what the user is looking at.
        let published = derive_ui(&state, at(14, 6), &mut ids).menu;
        let clicked = id_of(&published, "e2");

        // A sync lands while the menu is open: e1 has ended and dropped off
        // the list, so every block below it shifts up a place. Position-derived
        // ids handed e2's old id straight to e3.
        state.tasks_now = vec![task("e2", "Deep work", (15, 30), (17, 0))];
        state.tasks_later = vec![task("e3", "Standup", (17, 30), (18, 0))];
        let fresh = derive_ui(&state, at(15, 40), &mut ids).menu;

        assert_eq!(
            fresh.action_for(clicked),
            Some(&Action::SelectTask("e2".into())),
            "the click landed on a different block than the one it named"
        );
        assert_eq!(clicked, id_of(&fresh, "e2"), "the same event must keep its id across syncs");
    }

    #[test]
    fn a_stale_connect_click_cannot_disconnect_a_freshly_connected_account() {
        // The same reasoning applied to the one fixed item whose *meaning*
        // changes: only one of Connect/Disconnect is ever listed, so they get
        // ids of their own and a click on the vanished one resolves to nothing.
        let mut state = connected_state();
        state.connected = false;
        let published = ui(&state, at(14, 6)).menu;
        let connect = published
            .items
            .iter()
            .find(|i| i.action == Action::Connect)
            .expect("a disconnected menu offers Connect")
            .id;

        state.connected = true;
        let fresh = ui(&state, at(14, 6)).menu;
        assert_eq!(fresh.action_for(connect), None, "a stale Connect click must not disconnect");
    }

    #[test]
    fn no_menu_item_claims_the_root_id_or_two_items_the_same_id() {
        // `0` is the DBusMenu root; an item using it would be invisible to the
        // host at best. Duplicates would make `action_for` answer one item's
        // clicks with another's action.
        let mut connected = connected_state();
        connected.last_error = Some("timeout".into());
        let mut empty = connected_state();
        empty.tasks_now.clear();
        empty.tasks_later.clear();
        let mut disconnected = connected_state();
        disconnected.connected = false;
        let mut later_only = connected_state();
        later_only.tasks_now.clear();

        for state in [connected, empty, disconnected, later_only] {
            let menu = ui(&state, at(14, 6)).menu;
            let mut seen = std::collections::HashSet::new();
            for item in &menu.items {
                assert_ne!(item.id, 0, "an item claimed the DBusMenu root: {:?}", item.label);
                assert!(seen.insert(item.id), "two items share id {}", item.id);
            }
            // And the two ranges never overlap: a task id can never fire Quit.
            for item in &menu.items {
                let is_task = matches!(item.action, Action::SelectTask(_));
                assert_eq!(
                    is_task,
                    item.id >= ids::FIRST_TASK,
                    "{:?} is on the wrong side of FIRST_TASK with id {}",
                    item.label,
                    item.id
                );
            }
        }
    }

    #[test]
    fn a_block_that_lost_a_collision_never_inherits_the_winners_id() {
        // `e39516` and `e64020` hash to one slot, so the first takes it and
        // the second is probed one above. Delete the winner from the calendar
        // and an assignment rebuilt from scratch would hand its id — the row
        // the host is still showing as "Deep work" — to the loser, so a click
        // there would select "Standup". The memo `main` keeps for the life of
        // the run is what stops that.
        let mut ids = TaskIdMemo::default();
        let mut state = connected_state();
        state.tasks_now = vec![
            task("e39516", "Deep work", (14, 0), (15, 30)),
            task("e64020", "Standup", (15, 30), (16, 0)),
        ];
        state.tasks_later.clear();

        let published = derive_ui(&state, at(14, 6), &mut ids).menu;
        let winner = id_of(&published, "e39516");
        let loser = id_of(&published, "e64020");
        assert_eq!(loser, winner + 1, "the pair must really collide, or this proves nothing");

        // "Deep work" is deleted; the next sync drops it from the list.
        state.tasks_now = vec![task("e64020", "Standup", (15, 30), (16, 0))];
        let fresh = derive_ui(&state, at(14, 40), &mut ids).menu;

        assert_eq!(
            id_of(&fresh, "e64020"),
            loser,
            "a surviving block took the id of the one that left"
        );
        assert_eq!(
            fresh.action_for(winner),
            None,
            "a click on the deleted block's row selected a different block"
        );
    }

    #[test]
    fn two_blocks_whose_ids_collide_are_still_separately_selectable() {
        // Two event ids that really do hash to the same slot (see
        // `menu_model`'s own test). The menu has to keep them apart.
        let mut state = connected_state();
        state.tasks_now = vec![
            task("e39516", "Deep work", (14, 0), (15, 30)),
            task("e64020", "Standup", (15, 30), (16, 0)),
        ];
        state.tasks_later.clear();
        let menu = ui(&state, at(14, 6)).menu;

        let deep = id_of(&menu, "e39516");
        let standup = id_of(&menu, "e64020");
        assert_ne!(deep, standup);
        assert_eq!(menu.action_for(deep), Some(&Action::SelectTask("e39516".into())));
        assert_eq!(menu.action_for(standup), Some(&Action::SelectTask("e64020".into())));

        // And clicking each one really does select that block, not its twin.
        apply(&mut state, &menu.action_for(standup).cloned().unwrap());
        assert_eq!(state.selection.as_ref().unwrap().task.id, "e64020");
    }

    #[test]
    fn a_revision_bump_accompanies_every_menu_change() {
        let mut state = connected_state();
        let before = ui(&state, at(14, 6)).menu.revision;
        apply(&mut state, &Action::SelectTask("e1".into()));
        let after = ui(&state, at(14, 6)).menu.revision;
        assert!(after > before);
    }

    // ---- A revoked refresh token --------------------------------------------

    #[test]
    fn a_revoked_token_disconnects_clears_the_selection_and_notifies_once() {
        let mut state = connected_state();
        apply(&mut state, &Action::SelectTask("e1".into()));
        let effects = token_revoked(&mut state);
        assert!(!state.connected);
        assert!(state.selection.is_none());
        assert!(state.tasks_now.is_empty());
        assert!(state.tasks_later.is_empty());
        assert_eq!(ui(&state, at(14, 6)).label, "Connect calendar");
        assert!(effects.contains(&Effect::NotifyRevoked));
        // The same clearing path a deliberate disconnect takes, so the token
        // store is cleared through the one serialised revocation path.
        assert!(effects.contains(&Effect::Logout));
    }

    #[test]
    fn a_revoked_token_does_not_notify_a_second_time() {
        let mut state = connected_state();
        token_revoked(&mut state);
        let second = token_revoked(&mut state);
        assert!(second.is_empty(), "a second revocation must not notify again: {second:?}");
    }

    #[test]
    fn a_revoked_token_is_harmless_when_nothing_was_connected() {
        let mut state = AppState { connected: false, ..Default::default() };
        assert!(token_revoked(&mut state).is_empty());
    }
}
