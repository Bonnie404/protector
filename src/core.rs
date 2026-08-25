use chrono::{DateTime, Local};

use crate::task::{panel_label, Selection, Task};
use crate::tray::menu_model::{Action, MenuItem, MenuModel};
use crate::tray::UiState;

#[derive(Debug, Clone, Default)]
pub struct AppState {
    pub tasks_now: Vec<Task>,
    pub tasks_later: Vec<Task>,
    pub selection: Option<Selection>,
    pub connected: bool,
    pub last_sync: Option<DateTime<Local>>,
    pub last_error: Option<String>,
    pub revision: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    Sync,
    StartLogin,
    Logout,
    Quit,
    NotifyWarning,
    NotifyEnded,
    /// The selected event is gone from the calendar; the countdown it was
    /// driving has been dropped and the user has to be told, since nothing on
    /// screen would otherwise explain the label falling back to `Pick a task`.
    NotifyRemoved,
    Persist,
}

/// How long before a task ends the panel warns once, via `Effect::NotifyWarning`.
pub const WARN_BEFORE_SECS: i64 = 300;

fn item_label(t: &Task) -> String {
    format!("{}   {} \u{2013} {}", t.title, t.start.format("%H:%M"), t.end.format("%H:%M"))
}

fn push_tasks(items: &mut Vec<MenuItem>, id: &mut i32, tasks: &[Task], selected_id: Option<&str>) {
    for t in tasks {
        let checked = selected_id == Some(t.id.as_str());
        items.push(MenuItem::radio(*id, &item_label(t), checked, Action::SelectTask(t.id.clone())));
        *id += 1;
    }
}

/// Turns the current `AppState` into what the panel should show. Pure: same
/// inputs always produce the same `UiState`, so callers can derive as often as
/// they like without side effects.
pub fn derive_ui(state: &AppState, now: DateTime<Local>) -> UiState {
    let mut items = Vec::new();
    let mut id = 1;
    let selected_id = state.selection.as_ref().map(|s| s.task.id.as_str());

    if state.connected {
        if state.tasks_now.is_empty() && state.tasks_later.is_empty() {
            items.push(MenuItem::disabled(id, "Nothing scheduled today"));
            id += 1;
        } else {
            push_tasks(&mut items, &mut id, &state.tasks_now, selected_id);
            if !state.tasks_later.is_empty() {
                if !state.tasks_now.is_empty() {
                    items.push(MenuItem::separator(id));
                    id += 1;
                }
                items.push(MenuItem::disabled(id, "Later today"));
                id += 1;
                push_tasks(&mut items, &mut id, &state.tasks_later, selected_id);
            }
        }
    } else {
        items.push(MenuItem::disabled(id, "Not connected"));
        id += 1;
    }

    if state.last_error.is_some() {
        let synced = state
            .last_sync
            .map(|t| t.format("%H:%M").to_string())
            .unwrap_or_else(|| "never".into());
        items.push(MenuItem::disabled(id, &format!("\u{26a0} Offline \u{2014} synced {synced}")));
        id += 1;
    }

    items.push(MenuItem::separator(id));
    id += 1;
    items.push(MenuItem::command(id, "Refresh now", Action::Refresh));
    id += 1;
    if state.connected {
        items.push(MenuItem::command(id, "Disconnect account", Action::Disconnect));
    } else {
        items.push(MenuItem::command(id, "Connect Google Calendar\u{2026}", Action::Connect));
    }
    id += 1;
    items.push(MenuItem::command(id, "Quit", Action::Quit));

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
    if let Some(sel) = state.selection.as_mut() {
        let remaining = (sel.task.end - now).num_seconds();
        if remaining <= 0 && !sel.ended_notified {
            sel.ended_notified = true;
            sel.warned = true;
            effects.push(Effect::NotifyEnded);
            effects.push(Effect::Persist);
        } else if remaining > 0 && remaining <= WARN_BEFORE_SECS && !sel.warned {
            sel.warned = true;
            effects.push(Effect::NotifyWarning);
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

    fn connected_state() -> AppState {
        AppState {
            tasks_now: vec![task("e1", "Design review", (14, 0), (15, 30))],
            tasks_later: vec![task("e2", "Deep work", (15, 30), (17, 0))],
            selection: None,
            connected: true,
            last_sync: Some(at(14, 3)),
            last_error: None,
            revision: 0,
        }
    }

    #[test]
    fn menu_lists_now_then_later_with_a_header() {
        let ui = derive_ui(&connected_state(), at(14, 6));
        let labels: Vec<&str> = ui.menu.items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels[0].starts_with("Design review"));
        assert!(labels.iter().any(|l| *l == "Later today"));
        assert!(labels.iter().any(|l| l.starts_with("Deep work")));
        assert!(labels.iter().any(|l| *l == "Refresh now"));
        assert!(labels.iter().any(|l| *l == "Quit"));
    }

    #[test]
    fn selecting_a_task_checks_it_and_starts_the_countdown() {
        let mut state = connected_state();
        apply(&mut state, &Action::SelectTask("e1".into()));
        assert_eq!(state.selection.as_ref().unwrap().task.id, "e1");
        let ui = derive_ui(&state, at(14, 6));
        assert_eq!(ui.label, "1:24:00 \u{b7} Design review");
        assert_eq!(ui.menu.items[0].radio, Some(true));
    }

    #[test]
    fn selecting_the_active_task_again_clears_it() {
        let mut state = connected_state();
        apply(&mut state, &Action::SelectTask("e1".into()));
        apply(&mut state, &Action::SelectTask("e1".into()));
        assert!(state.selection.is_none());
        assert_eq!(derive_ui(&state, at(14, 6)).label, "Pick a task");
    }

    #[test]
    fn past_the_end_the_item_asks_for_attention() {
        let mut state = connected_state();
        apply(&mut state, &Action::SelectTask("e1".into()));
        let ui = derive_ui(&state, at(15, 34));
        assert!(ui.attention);
        assert_eq!(ui.label, "\u{26a0} +04:00 \u{b7} Design review");
    }

    #[test]
    fn an_empty_day_says_so() {
        let mut state = connected_state();
        state.tasks_now.clear();
        state.tasks_later.clear();
        let ui = derive_ui(&state, at(14, 6));
        assert!(ui.menu.items.iter().any(|i| i.label == "Nothing scheduled today" && !i.enabled));
    }

    #[test]
    fn a_failed_sync_is_visible_in_the_menu() {
        let mut state = connected_state();
        state.last_error = Some("timeout".into());
        let ui = derive_ui(&state, at(14, 6));
        assert!(ui.menu.items.iter().any(|i| i.label.starts_with("\u{26a0} Offline \u{2014} synced 14:03") && !i.enabled));
    }

    #[test]
    fn without_credentials_the_menu_offers_to_connect() {
        let mut state = connected_state();
        state.connected = false;
        state.tasks_now.clear();
        state.tasks_later.clear();
        let ui = derive_ui(&state, at(14, 6));
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
    fn the_five_minute_warning_fires_once() {
        let mut state = connected_state();
        apply(&mut state, &Action::SelectTask("e1".into()));
        let first = tick(&mut state, at(15, 26));
        let second = tick(&mut state, at(15, 27));
        assert!(first.iter().any(|e| matches!(e, Effect::NotifyWarning)));
        assert!(!second.iter().any(|e| matches!(e, Effect::NotifyWarning)));
    }

    #[test]
    fn disconnecting_clears_a_stale_offline_warning() {
        let mut state = connected_state();
        state.last_error = Some("the calendar did not answer within 30s".into());
        apply(&mut state, &Action::Disconnect);
        let ui = derive_ui(&state, at(14, 6));
        assert!(
            !ui.menu.items.iter().any(|i| i.label.starts_with("\u{26a0} Offline")),
            "disconnecting on purpose is not the same as being offline: {:?}",
            ui.menu.items.iter().map(|i| &i.label).collect::<Vec<_>>()
        );
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

    #[test]
    fn a_revision_bump_accompanies_every_menu_change() {
        let mut state = connected_state();
        let before = derive_ui(&state, at(14, 6)).menu.revision;
        apply(&mut state, &Action::SelectTask("e1".into()));
        let after = derive_ui(&state, at(14, 6)).menu.revision;
        assert!(after > before);
    }
}
