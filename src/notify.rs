//! Desktop notifications, via `org.freedesktop.Notifications`: the T-5
//! low-urgency heads-up, the T-0 end-of-task chooser with one action button
//! per upcoming task, and the plain one-line notices (`Task removed`,
//! `Reconnect required`).
//!
//! `ActionInvoked` is the only signal this module listens for; `watch_actions`
//! turns a pressed button back into `Command::SelectById`, the same selection
//! path a menu click takes.

use std::collections::HashMap;

use tokio::sync::mpsc;
use zbus::zvariant::Value;
use zbus::Connection;

use crate::task::Task;
use crate::tray::Command;

/// The three soonest upcoming tasks become buttons; anything past that is
/// named in the body instead (spec §6: "one action button per upcoming task
/// (the three soonest)").
pub const MAX_BUTTONS: usize = 3;

#[zbus::proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<&str, Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;

    #[zbus(signal)]
    fn action_invoked(&self, id: u32, action_key: String) -> zbus::Result<()>;
}

fn button_label(t: &Task) -> String {
    format!("{} {}", t.title, t.start.format("%H:%M"))
}

/// Escapes the three characters that matter to a Pango markup parser — `&`
/// first, so it cannot re-escape the entities this function just introduced,
/// then `<` and `>`. Every task title reaching the notification **body**
/// goes through this: titles come straight from Google Calendar's `summary`
/// field (`calendar.rs`), are otherwise unescaped, and this session's own
/// `GetCapabilities` advertises `body-markup`, so the body is parsed as
/// markup rather than shown literally. `&`, `<`, `>` are the only characters
/// that matter to that parser; nothing else needs touching.
///
/// Deliberately *not* applied to action labels (`ended_actions`/
/// `button_label`'s use in them): the Desktop Notifications spec does not
/// parse action labels as markup, so escaping them would corrupt the visible
/// button text (`Q&A` would show as the literal characters `Q&amp;A`)
/// instead of protecting anything.
fn escape_markup(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The `Notify` `actions` array for the end-of-task chooser: alternating
/// `(action_key, localized_label)` pairs, one per candidate up to
/// [`MAX_BUTTONS`], plus a trailing `none` / `Nothing` pair so there is
/// always a way to dismiss without picking a task.
pub fn ended_actions(candidates: &[Task]) -> Vec<String> {
    let mut actions = Vec::new();
    for t in candidates.iter().take(MAX_BUTTONS) {
        actions.push(format!("task:{}", t.id));
        actions.push(button_label(t));
    }
    actions.push("none".into());
    actions.push("Nothing".into());
    actions
}

/// The notification body: names whatever candidate did not fit as a button,
/// so nothing on today's list is invisible just because it was fourth.
pub fn ended_body(candidates: &[Task]) -> String {
    if candidates.is_empty() {
        return "Nothing else is scheduled today.".into();
    }
    let rest: Vec<String> =
        candidates.iter().skip(MAX_BUTTONS).map(|t| escape_markup(&button_label(t))).collect();
    if rest.is_empty() {
        "Pick what you are working on next.".into()
    } else {
        format!("Also today: {}", rest.join(", "))
    }
}

/// The event id a pressed action button refers to, or `None` for the `none`
/// (Nothing) action, which is a dismissal rather than a selection.
pub fn action_to_event_id(key: &str) -> Option<String> {
    key.strip_prefix("task:").map(|s| s.to_string())
}

/// The T-0 notification: `✓ <title> ended — what's next?`, with the three
/// soonest candidates as buttons and `expire_timeout` 0 so it stays in the
/// tray — critical urgency, since it is the one notification the widget
/// actually needs answered — until the user presses one.
pub async fn notify_ended(conn: &Connection, ended: &Task, candidates: &[Task]) -> zbus::Result<u32> {
    let proxy = NotificationsProxy::new(conn).await?;
    let actions = ended_actions(candidates);
    let action_refs: Vec<&str> = actions.iter().map(|s| s.as_str()).collect();
    let mut hints = HashMap::new();
    hints.insert("urgency", Value::U8(2));
    proxy
        .notify(
            "Protector",
            0,
            "alarm-symbolic",
            &format!("\u{2713} {} ended \u{2014} what's next?", ended.title),
            &ended_body(candidates),
            &action_refs,
            hints,
            0, // never expires: it waits until answered
        )
        .await
}

/// The heads-up's summary line, from the seconds actually left on the block.
///
/// Deliberately *not* the configured window: with `warn_before_minutes = 30`
/// and a block selected four minutes before it ends, the heads-up fires on the
/// very next tick, and rendering the window there announced "30 minutes left"
/// on a block with four. The window is only ever an upper bound on this number
/// — `core::tick` fires the warning when `remaining <= warn_before_secs` — so
/// taking the remaining time needs no separate clamp.
///
/// Rounded **up**, so the ordinary case still reads as the window the user
/// configured: a 1 Hz ticker sees 29:58 rather than a clean 30:00, and
/// `29 minutes left` for a heads-up that exists to say "half an hour" would be
/// its own small lie. Rounding up also keeps the last minute from reading
/// `0 minutes left`.
pub fn warning_summary(task: &Task, remaining_secs: i64) -> String {
    // `.max(1)` covers the theoretical zero — `tick` only fires this with
    // `remaining > 0` — and keeps the ceiling from producing a `0`.
    let minutes = remaining_secs.max(1).saturating_add(59) / 60;
    let unit = if minutes == 1 { "minute" } else { "minutes" };
    format!("{minutes} {unit} left \u{b7} {}", task.title)
}

/// The T-5 heads-up: low urgency, no actions, and left to expire on its own
/// (`expire_timeout` -1, the server's normal default) — a nudge, not
/// something that has to be dealt with.
///
/// Takes the seconds left rather than a minute count so that the one number
/// the caller has — the remaining time `tick` measured — is the one that gets
/// rendered, with no arithmetic in between for a window to sneak into.
pub async fn notify_warning(conn: &Connection, task: &Task, remaining_secs: i64) -> zbus::Result<u32> {
    let proxy = NotificationsProxy::new(conn).await?;
    let mut hints = HashMap::new();
    hints.insert("urgency", Value::U8(0));
    proxy
        .notify(
            "Protector",
            0,
            "alarm-symbolic",
            &warning_summary(task, remaining_secs),
            "",
            &[],
            hints,
            -1,
        )
        .await
}

/// A plain one-line notice with no action buttons — `Task removed`,
/// `Reconnect required` — left to expire on the server's own schedule.
pub async fn notify_simple(conn: &Connection, summary: &str, body: &str) -> zbus::Result<u32> {
    let proxy = NotificationsProxy::new(conn).await?;
    proxy.notify("Protector", 0, "alarm-symbolic", summary, body, &[], HashMap::new(), -1).await
}

/// A live subscription to `ActionInvoked`.
///
/// Its existence is the guarantee: [`subscribe_actions`] does not return until
/// the bus has acknowledged the `AddMatch` behind it, so any signal emitted
/// after that point is delivered to this stream — buffered if nothing is
/// reading yet. Signals emitted *before* it are lost, which is why the
/// subscription is a separate, awaitable step rather than the first line of a
/// spawned loop.
pub struct ActionSubscription {
    stream: ActionInvokedStream,
}

impl ActionSubscription {
    /// Turns pressed notification buttons into selection commands, for as long
    /// as the connection lives.
    pub async fn forward(mut self, tx: mpsc::Sender<Command>) -> zbus::Result<()> {
        use futures_util::StreamExt;
        while let Some(signal) = self.stream.next().await {
            let args = signal.args()?;
            if let Some(id) = action_to_event_id(&args.action_key) {
                let _ = tx.send(Command::SelectById(id)).await;
            } else if args.action_key == "none" {
                // The *Nothing* button: dismiss by clearing the selection,
                // not by silently doing nothing. Before this, `none` matched
                // no arm at all and the button just closed the banner while
                // the countdown it was supposed to stop kept running.
                let _ = tx.send(Command::ClearSelection).await;
            }
        }
        Ok(())
    }
}

/// Subscribes to `ActionInvoked`. The subscription is established by the time
/// this resolves; see [`ActionSubscription`].
pub async fn subscribe_actions(conn: &Connection) -> zbus::Result<ActionSubscription> {
    let proxy = NotificationsProxy::new(conn).await?;
    // The stream owns its match rule and its share of the connection, so
    // dropping the proxy here does not unsubscribe it.
    Ok(ActionSubscription { stream: proxy.receive_action_invoked().await? })
}

/// Turns a pressed notification button into a selection command. Runs for as
/// long as the connection lives; the caller spawns it once at startup.
pub async fn watch_actions(conn: Connection, tx: mpsc::Sender<Command>) -> zbus::Result<()> {
    subscribe_actions(&conn).await?.forward(tx).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Local, TimeZone};

    fn at(h: u32, m: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 8, 25, h, m, 0).unwrap()
    }
    fn task(id: &str, title: &str, h: u32, m: u32) -> Task {
        Task { id: id.into(), title: title.into(), start: at(h, m), end: at(h + 1, m) }
    }

    #[test]
    fn at_most_three_candidates_become_buttons_plus_nothing() {
        let candidates = vec![
            task("e2", "Deep work", 15, 30),
            task("e3", "Standup", 17, 30),
            task("e4", "Email", 18, 0),
            task("e5", "Reading", 19, 0),
        ];
        let actions = ended_actions(&candidates);
        // pairs of (key, label)
        assert_eq!(actions.len(), 8);
        assert_eq!(actions[0], "task:e2");
        assert_eq!(actions[1], "Deep work 15:30");
        assert_eq!(actions[6], "none");
        assert_eq!(actions[7], "Nothing");
    }

    #[test]
    fn the_body_lists_the_candidates_that_did_not_fit() {
        let candidates = vec![
            task("e2", "Deep work", 15, 30),
            task("e3", "Standup", 17, 30),
            task("e4", "Email", 18, 0),
            task("e5", "Reading", 19, 0),
        ];
        assert!(ended_body(&candidates).contains("Reading 19:00"));
    }

    #[test]
    fn the_body_escapes_ampersands_in_a_title_so_the_markup_stays_valid() {
        // Titles are untrusted text straight from Google Calendar's `summary`
        // field (see `calendar.rs`), and the body is parsed as Pango markup —
        // this session's own `GetCapabilities` advertises `body-markup`. An
        // unescaped `&` is invalid XML-ish markup and can garble or drop the
        // whole body.
        let candidates = vec![
            task("e2", "Deep work", 15, 30),
            task("e3", "Standup", 17, 30),
            task("e4", "Email", 18, 0),
            task("e5", "Q&A", 19, 0),
        ];
        let body = ended_body(&candidates);
        assert!(body.contains("Q&amp;A 19:00"), "unescaped ampersand reached the body: {body:?}");
        assert!(!body.contains("Q&A 19:00"), "the raw, unescaped title must not appear: {body:?}");
    }

    #[test]
    fn the_body_escapes_angle_brackets_in_a_title_so_the_markup_stays_valid() {
        let candidates = vec![
            task("e2", "Deep work", 15, 30),
            task("e3", "Standup", 17, 30),
            task("e4", "Email", 18, 0),
            task("e5", "<Draft> review", 19, 0),
        ];
        let body = ended_body(&candidates);
        assert!(
            body.contains("&lt;Draft&gt; review 19:00"),
            "unescaped angle brackets reached the body: {body:?}"
        );
        assert!(!body.contains("<Draft>"), "the raw, unescaped title must not appear: {body:?}");
    }

    #[test]
    fn escaping_does_not_double_escape_an_already_present_ampersand_entity() {
        // Guards the ordering the fix depends on: `&` must be escaped before
        // `<`/`>`, or an `&lt;` this function itself just produced would be
        // escaped a second time into `&amp;lt;`.
        let candidates = vec![
            task("e2", "Deep work", 15, 30),
            task("e3", "Standup", 17, 30),
            task("e4", "Email", 18, 0),
            task("e5", "<A&B>", 19, 0),
        ];
        let body = ended_body(&candidates);
        assert!(body.contains("&lt;A&amp;B&gt;"), "got: {body:?}");
        assert!(!body.contains("&amp;lt;"), "double-escaped the entity: {body:?}");
    }

    #[test]
    fn action_labels_are_left_unescaped_because_the_spec_does_not_parse_them_as_markup() {
        // Deliberate, and the opposite of `ended_body`: `actions` labels are
        // plain text per the Desktop Notifications spec, so escaping them
        // would corrupt what the user actually sees on the button (turning
        // `Q&A` into the literal characters `Q&amp;A`), not protect anything.
        let candidates = vec![task("e2", "Q&A", 15, 30)];
        let actions = ended_actions(&candidates);
        assert_eq!(actions[1], "Q&A 15:30", "button labels must stay exactly as Calendar sent them");
    }

    #[test]
    fn an_empty_day_produces_only_the_dismiss_button() {
        assert_eq!(ended_actions(&[]), vec!["none".to_string(), "Nothing".to_string()]);
        assert_eq!(ended_body(&[]), "Nothing else is scheduled today.");
    }

    // ---- The heads-up's wording ---------------------------------------------

    #[test]
    fn the_heads_up_states_the_time_that_is_actually_left() {
        // The ordinary case: a 30 minute window, crossed on a tick that sees
        // 29:58 rather than a clean 30:00.
        let t = task("e1", "Design review", 14, 0);
        assert_eq!(warning_summary(&t, 1798), "30 minutes left \u{b7} Design review");
        // And the documented default window.
        assert_eq!(warning_summary(&t, 299), "5 minutes left \u{b7} Design review");
    }

    #[test]
    fn a_block_selected_inside_the_window_is_not_announced_as_the_whole_window() {
        // `warn_before_minutes = 30`, and the user picks a block with four
        // minutes to go: the heads-up fires on the very next tick. Rendering
        // the *window* here told them they had half an hour.
        let t = task("e1", "Design review", 14, 0);
        assert_eq!(warning_summary(&t, 239), "4 minutes left \u{b7} Design review");
    }

    #[test]
    fn a_one_minute_window_reads_as_one_minute_singular() {
        let t = task("e1", "Design review", 14, 0);
        assert_eq!(warning_summary(&t, 60), "1 minute left \u{b7} Design review");
        // Anywhere inside the last minute says the same thing, never "0".
        assert_eq!(warning_summary(&t, 1), "1 minute left \u{b7} Design review");
        // And one second past it is plural again.
        assert_eq!(warning_summary(&t, 61), "2 minutes left \u{b7} Design review");
    }

    #[test]
    fn an_action_key_maps_back_to_an_event_id() {
        assert_eq!(action_to_event_id("task:e2"), Some("e2".to_string()));
        assert_eq!(action_to_event_id("none"), None);
    }
}
