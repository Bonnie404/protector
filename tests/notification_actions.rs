//! Exercises `notify.rs` against a fake `org.freedesktop.Notifications` on a
//! **private** bus of its own — never the developer's live session bus — so
//! nothing this test suite runs can leave a real notification on anyone's
//! desktop. Follows the `PrivateBus` pattern in `tests/sni_registration.rs`.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Command as OsCommand, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Local, TimeZone};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::OwnedValue;

use protector::task::Task;
use protector::tray::Command;
use protector::notify;

/// A private session bus that is killed and reaped when it goes out of scope
/// — including when an assertion panics — so a failing run cannot leave a
/// `--nofork` dbus-daemon behind on the developer's machine.
struct PrivateBus {
    address: String,
    child: std::process::Child,
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn private_bus() -> PrivateBus {
    let mut child = OsCommand::new("dbus-daemon")
        .args(["--session", "--print-address", "--nofork"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("dbus-daemon must be installed");
    let mut line = String::new();
    BufReader::new(child.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    PrivateBus {
        address: line.trim().to_string(),
        child,
    }
}

// ---- A fake org.freedesktop.Notifications ---------------------------------

/// One recorded `Notify` call, in the shapes the assertions below care about.
#[derive(Debug, Clone)]
struct Recorded {
    app_name: String,
    summary: String,
    body: String,
    actions: Vec<String>,
    hints: HashMap<String, OwnedValue>,
    expire_timeout: i32,
}

struct FakeNotifications {
    calls: Arc<Mutex<Vec<Recorded>>>,
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl FakeNotifications {
    #[allow(clippy::too_many_arguments)]
    async fn notify(
        &self,
        app_name: String,
        _replaces_id: u32,
        _app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> u32 {
        let mut calls = self.calls.lock().unwrap();
        calls.push(Recorded { app_name, summary, body, actions, hints, expire_timeout });
        calls.len() as u32
    }

    #[zbus(signal)]
    async fn action_invoked(emitter: &SignalEmitter<'_>, id: u32, action_key: String) -> zbus::Result<()>;
}

/// Starts the fake server on `bus`, returning its own connection (keep it
/// bound for the test's duration — dropping it early un-registers the name)
/// alongside a client connection and the shared call log.
async fn fake_server(bus: &PrivateBus) -> (zbus::Connection, zbus::Connection, Arc<Mutex<Vec<Recorded>>>) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let server = zbus::connection::Builder::address(bus.address.as_str())
        .unwrap()
        .name("org.freedesktop.Notifications")
        .unwrap()
        .serve_at(
            "/org/freedesktop/Notifications",
            FakeNotifications { calls: calls.clone() },
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let client = zbus::connection::Builder::address(bus.address.as_str())
        .unwrap()
        .build()
        .await
        .unwrap();
    (server, client, calls)
}

/// Starts the `ActionInvoked` listener and hands back the commands it
/// forwards.
///
/// The subscription is awaited here, in the test's own task, rather than
/// happening somewhere inside a spawned one: `notify::subscribe_actions` does
/// not resolve until the bus has acknowledged the `AddMatch`, so every signal
/// emitted after this call is guaranteed to reach the listener. That
/// guarantee is what a fixed 300 ms sleep used to approximate — and a signal
/// emitted before a subscription is simply dropped, which would have made the
/// assertions below pass without testing anything.
/// Bounded at 5 s like every other wait here: the handshake is a method call
/// to the bus, and a bus that accepts the connection but never answers must
/// fail this test rather than hang it.
async fn listening(client: &zbus::Connection) -> tokio::sync::mpsc::Receiver<Command> {
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    let actions = tokio::time::timeout(Duration::from_secs(5), notify::subscribe_actions(client))
        .await
        .expect("the ActionInvoked subscription must complete")
        .expect("subscribing to ActionInvoked");
    tokio::spawn(async move {
        let _ = actions.forward(tx).await;
    });
    rx
}

fn urgency_of(call: &Recorded) -> u8 {
    call.hints
        .get("urgency")
        .expect("no urgency hint was set")
        .downcast_ref::<u8>()
        .expect("urgency must be a byte")
}

// ---- helpers ----------------------------------------------------------

fn at(h: u32, m: u32) -> DateTime<Local> {
    Local.with_ymd_and_hms(2026, 8, 25, h, m, 0).unwrap()
}

fn task(id: &str, title: &str, h: u32, m: u32) -> Task {
    Task { id: id.into(), title: title.into(), start: at(h, m), end: at(h + 1, m) }
}

// ---- The three Notify-shaped functions -------------------------------------

#[tokio::test]
async fn notify_ended_carries_the_action_buttons_hints_and_never_expires() {
    let bus = private_bus();
    let (_server, client, calls) = fake_server(&bus).await;

    let ended = task("e1", "Design review", 14, 0);
    let candidates = vec![
        task("e2", "Deep work", 15, 30),
        task("e3", "Standup", 17, 30),
        task("e4", "Email", 18, 0),
        task("e5", "Reading", 19, 0),
    ];

    notify::notify_ended(&client, &ended, &candidates).await.unwrap();

    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(call.app_name, "Protector");
    assert_eq!(call.summary, "\u{2713} Design review ended \u{2014} what's next?");
    assert_eq!(call.body, notify::ended_body(&candidates));
    // Exactly the actions array the pure function produces: three buttons —
    // the soonest three candidates — plus the trailing Nothing dismissal.
    assert_eq!(call.actions, notify::ended_actions(&candidates));
    assert_eq!(call.actions.len(), 8);
    assert_eq!(call.actions[0], "task:e2");
    assert_eq!(call.actions[7], "Nothing");
    assert_eq!(call.expire_timeout, 0, "must wait in the tray until answered, never auto-expire");
    assert_eq!(urgency_of(call), 2, "the T-0 prompt is critical urgency");
}

#[tokio::test]
async fn notify_ended_with_nothing_scheduled_offers_only_the_dismiss_button() {
    let bus = private_bus();
    let (_server, client, calls) = fake_server(&bus).await;

    let ended = task("e1", "Design review", 14, 0);
    notify::notify_ended(&client, &ended, &[]).await.unwrap();

    let calls = calls.lock().unwrap();
    let call = &calls[0];
    assert_eq!(call.actions, vec!["none".to_string(), "Nothing".to_string()]);
    assert_eq!(call.body, "Nothing else is scheduled today.");
}

#[tokio::test]
async fn a_title_with_markup_characters_reaches_the_wire_escaped_in_the_body_but_raw_on_the_button() {
    let bus = private_bus();
    let (_server, client, calls) = fake_server(&bus).await;

    let ended = task("e1", "Design review", 14, 0);
    let candidates = vec![
        task("e2", "Deep work", 15, 30),
        task("e3", "Standup", 17, 30),
        task("e4", "Email", 18, 0),
        // The fourth candidate lands in the body text, not a button — this is
        // the path that goes through a Pango markup parser.
        task("e5", "Q&A <planning>", 19, 0),
    ];

    notify::notify_ended(&client, &ended, &candidates).await.unwrap();

    let calls = calls.lock().unwrap();
    let call = &calls[0];
    assert!(
        call.body.contains("Q&amp;A &lt;planning&gt; 19:00"),
        "unescaped markup characters reached the real Notify call's body: {:?}",
        call.body
    );
    assert!(!call.body.contains("Q&A <planning>"), "the raw title leaked into the body: {:?}", call.body);
}

#[tokio::test]
async fn a_title_with_markup_characters_on_a_button_stays_exactly_as_calendar_sent_it() {
    let bus = private_bus();
    let (_server, client, calls) = fake_server(&bus).await;

    let ended = task("e1", "Design review", 14, 0);
    // Within the first three, so it becomes a button label rather than body
    // text — action labels are not parsed as markup, so escaping them would
    // corrupt what the user sees on the button.
    let candidates = vec![task("e2", "Q&A <planning>", 15, 30)];

    notify::notify_ended(&client, &ended, &candidates).await.unwrap();

    let calls = calls.lock().unwrap();
    let call = &calls[0];
    assert_eq!(call.actions[1], "Q&A <planning> 15:30", "a button label must not be escaped");
}

#[tokio::test]
async fn notify_warning_is_low_urgency_with_no_actions_and_expires_normally() {
    let bus = private_bus();
    let (_server, client, calls) = fake_server(&bus).await;

    let t = task("e1", "Design review", 14, 0);
    // Seconds left, not minutes: what reaches the wire is the time actually
    // remaining on the block, rounded up to whole minutes.
    notify::notify_warning(&client, &t, 299).await.unwrap();

    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(call.summary, "5 minutes left \u{b7} Design review");
    assert!(call.actions.is_empty(), "the T-5 heads-up carries no buttons");
    assert_ne!(call.expire_timeout, 0, "a low-urgency nudge must not be pinned open forever");
    assert_eq!(urgency_of(call), 0, "the T-5 heads-up is low urgency");
}

#[tokio::test]
async fn notify_simple_carries_no_action_buttons() {
    let bus = private_bus();
    let (_server, client, calls) = fake_server(&bus).await;

    notify::notify_simple(&client, "Task removed", "The selected event was deleted from your calendar.")
        .await
        .unwrap();

    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(call.summary, "Task removed");
    assert_eq!(call.body, "The selected event was deleted from your calendar.");
    assert!(call.actions.is_empty());
}

// ---- ActionInvoked -> Command::SelectById ----------------------------------

#[tokio::test]
async fn pressing_an_action_button_selects_that_task() {
    let bus = private_bus();
    let (server, client, _calls) = fake_server(&bus).await;

    let mut rx = listening(&client).await;

    let iface_ref = server
        .object_server()
        .interface::<_, FakeNotifications>("/org/freedesktop/Notifications")
        .await
        .unwrap();
    FakeNotifications::action_invoked(iface_ref.signal_emitter(), 1, "task:e2".into())
        .await
        .unwrap();

    let cmd = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("watch_actions must forward the button press")
        .expect("the channel must still be open");
    match cmd {
        Command::SelectById(id) => assert_eq!(id, "e2"),
        other => panic!("expected SelectById(\"e2\"), got {other:?}"),
    }
}

#[tokio::test]
async fn pressing_nothing_dismisses_without_selecting_anything() {
    let bus = private_bus();
    let (server, client, _calls) = fake_server(&bus).await;

    let mut rx = listening(&client).await;

    let iface_ref = server
        .object_server()
        .interface::<_, FakeNotifications>("/org/freedesktop/Notifications")
        .await
        .unwrap();

    FakeNotifications::action_invoked(iface_ref.signal_emitter(), 1, "none".into())
        .await
        .unwrap();
    // Emitted second, from the same connection, so the bus delivers it second:
    // anything `none` had produced would be queued ahead of it. Waiting for a
    // command that *must* arrive proves the dismissal produced none, and does
    // it without betting on how long "nothing happened" takes to observe.
    FakeNotifications::action_invoked(iface_ref.signal_emitter(), 2, "task:e5".into())
        .await
        .unwrap();
    let cmd = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("the listener must still be running after an ignored dismissal")
        .expect("the channel must still be open");
    assert!(
        matches!(&cmd, Command::SelectById(id) if id == "e5"),
        "the Nothing action must not produce a selection, but {cmd:?} arrived before e5"
    );
}
