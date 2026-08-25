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
    let rest: Vec<String> = candidates.iter().skip(MAX_BUTTONS).map(button_label).collect();
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

/// The T-5 heads-up: low urgency, no actions, and left to expire on its own
/// (`expire_timeout` -1, the server's normal default) — a nudge, not
/// something that has to be dealt with.
pub async fn notify_warning(conn: &Connection, task: &Task, minutes: i64) -> zbus::Result<u32> {
    let proxy = NotificationsProxy::new(conn).await?;
    let mut hints = HashMap::new();
    hints.insert("urgency", Value::U8(0));
    proxy
        .notify(
            "Protector",
            0,
            "alarm-symbolic",
            &format!("{minutes} minutes left \u{b7} {}", task.title),
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

/// Turns a pressed notification button into a selection command. Runs for as
/// long as the connection lives; the caller spawns it once at startup.
pub async fn watch_actions(conn: Connection, tx: mpsc::Sender<Command>) -> zbus::Result<()> {
    use futures_util::StreamExt;
    let proxy = NotificationsProxy::new(&conn).await?;
    let mut stream = proxy.receive_action_invoked().await?;
    while let Some(signal) = stream.next().await {
        let args = signal.args()?;
        if let Some(id) = action_to_event_id(&args.action_key) {
            let _ = tx.send(Command::SelectById(id)).await;
        }
    }
    Ok(())
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
    fn an_empty_day_produces_only_the_dismiss_button() {
        assert_eq!(ended_actions(&[]), vec!["none".to_string(), "Nothing".to_string()]);
        assert_eq!(ended_body(&[]), "Nothing else is scheduled today.");
    }

    #[test]
    fn an_action_key_maps_back_to_an_event_id() {
        assert_eq!(action_to_event_id("task:e2"), Some("e2".to_string()));
        assert_eq!(action_to_event_id("none"), None);
    }
}
