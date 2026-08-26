//! The Google Calendar HTTP client and the filtering that turns raw events
//! into the `Now` / `Later today` lists the menu shows.
//!
//! Only ever issues GET requests: the OAuth scope is read-only, and this
//! module has no business writing back to a user's calendar.

use crate::config::Config;
use crate::task::Task;
use chrono::{DateTime, Duration, Local, NaiveTime};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct EventsPage {
    #[serde(default)]
    pub items: Vec<GoogleEvent>,
}

#[derive(Debug, Deserialize)]
pub struct EventTime {
    #[serde(rename = "dateTime")]
    pub date_time: Option<DateTime<Local>>,
    pub date: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Attendee {
    #[serde(rename = "self", default)]
    pub is_self: bool,
    #[serde(rename = "responseStatus", default)]
    pub response_status: String,
}

#[derive(Debug, Deserialize)]
pub struct GoogleEvent {
    pub id: String,
    #[serde(default)]
    pub status: String,
    pub summary: Option<String>,
    pub start: EventTime,
    pub end: EventTime,
    #[serde(default)]
    pub attendees: Vec<Attendee>,
}

impl GoogleEvent {
    fn declined_by_me(&self) -> bool {
        self.attendees.iter().any(|a| a.is_self && a.response_status == "declined")
    }
}

/// Drops all-day events (a `date` instead of a `dateTime`), cancelled events,
/// and events the user has declined. Everything else becomes a `Task`, with
/// a missing `summary` rendered as `(no title)`.
pub fn to_tasks(events: &[GoogleEvent]) -> Vec<Task> {
    events
        .iter()
        .filter(|e| e.status != "cancelled" && !e.declined_by_me())
        .filter_map(|e| {
            let start = e.start.date_time?; // None for all-day events
            let end = e.end.date_time?;
            Some(Task {
                id: e.id.clone(),
                title: e.summary.clone().unwrap_or_else(|| "(no title)".into()),
                start,
                end,
            })
        })
        .collect()
}

/// Splits tasks into what's running now (`start <= now < end`) and what's
/// still ahead today (`start > now`). A task that has already ended lands in
/// neither list.
pub fn partition(tasks: Vec<Task>, now: DateTime<Local>) -> (Vec<Task>, Vec<Task>) {
    let mut running = Vec::new();
    let mut later = Vec::new();
    for t in tasks {
        if t.start <= now && now < t.end {
            running.push(t);
        } else if t.start > now {
            later.push(t);
        }
    }
    (running, later)
}

/// The local midnight that ends `now`'s day, i.e. the start of tomorrow.
///
/// `and_local_timezone` can return zero matches (a clock-forward DST jump
/// skips that wall-clock instant entirely) or two (a clock-back jump repeats
/// it). `.single()` only succeeds on the ordinary one-match case, so on
/// either DST edge case this falls back to `now + 12h` — a window that still
/// comfortably covers the rest of the working day without ever panicking.
pub fn end_of_day(now: DateTime<Local>) -> DateTime<Local> {
    (now.date_naive() + Duration::days(1))
        .and_time(NaiveTime::MIN)
        .and_local_timezone(Local)
        .single()
        .unwrap_or(now + Duration::hours(12))
}

/// Fetches the raw events page for `cfg.calendar_id`, spanning from `now` to
/// the end of `now`'s local day. `singleEvents=true` expands recurring
/// events into their instances; `orderBy=startTime` keeps them in the order
/// the menu wants to show them.
pub async fn fetch(
    base_url: &str,
    access_token: &str,
    cfg: &Config,
    now: DateTime<Local>,
) -> anyhow::Result<reqwest::Response> {
    let url = format!("{base_url}/calendars/{}/events", cfg.calendar_id);
    Ok(reqwest::Client::new()
        .get(url)
        .bearer_auth(access_token)
        .query(&[
            ("timeMin", now.to_rfc3339()),
            ("timeMax", end_of_day(now).to_rfc3339()),
            ("singleEvents", "true".into()),
            ("orderBy", "startTime".into()),
            ("maxResults", "50".into()),
        ])
        .send()
        .await?)
}

/// Fetches, and on a 401 asks `refresh_fn` for a new access token and tries
/// exactly once more with it. Any other error status, or a second 401, is
/// surfaced rather than retried again.
pub async fn fetch_with_retry<F, Fut>(
    base_url: &str,
    access_token: &str,
    refresh_fn: F,
    cfg: &Config,
    now: DateTime<Local>,
) -> anyhow::Result<Vec<Task>>
where
    F: FnOnce(&str) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<String>>,
{
    let mut resp = fetch(base_url, access_token, cfg, now).await?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        let fresh = refresh_fn(access_token).await?;
        resp = fetch(base_url, &fresh, cfg, now).await?;
    }
    if !resp.status().is_success() {
        // Unlike the OAuth token endpoint (`auth.rs`'s `TokenRequestError`,
        // which deliberately drops `error_description` because Google echoes
        // the rejected refresh token back inside it), this body carries no
        // credential material — it is a service-configuration message ("API
        // not enabled", quota exceeded, permission denied) generated by the
        // Calendar API, not an echo of anything this request sent. So, unlike
        // there, it is both safe and useful to read the body and surface it.
        // Do not "harmonise" the two error paths: they hold different kinds
        // of content on purpose.
        return Err(api_error(resp).await);
    }
    let page: EventsPage = resp.json().await?;
    Ok(to_tasks(&page.items))
}

/// Google's JSON error envelope for a failed Calendar API request:
/// `{"error": {"message": ..., "errors": [{"reason": ...}, ...]}}`. Only
/// these two fields are ever read out of the body — whatever else the
/// response contains, however it looks, never becomes part of the error this
/// crate carries around.
#[derive(Debug, Deserialize)]
struct ApiErrorEnvelope {
    error: ApiErrorBody,
}

#[derive(Debug, Deserialize)]
struct ApiErrorBody {
    message: Option<String>,
    #[serde(default)]
    errors: Vec<ApiErrorReason>,
}

#[derive(Debug, Deserialize)]
struct ApiErrorReason {
    reason: Option<String>,
}

/// The Calendar API's answer to a request it refused: the HTTP status, plus
/// whatever Google's own error envelope said, when the body parsed as one.
#[derive(Debug)]
pub struct CalendarApiError {
    pub status: reqwest::StatusCode,
    pub message: Option<String>,
    pub reasons: Vec<String>,
}

impl CalendarApiError {
    /// True for Google's "Calendar API has not been used in project ... or is
    /// disabled" refusal — the overwhelmingly common first-run failure —
    /// identified by the machine-readable `accessNotConfigured` reason rather
    /// than by matching the prose, so a reworded message cannot silently
    /// break the check.
    pub fn is_access_not_configured(&self) -> bool {
        self.reasons.iter().any(|r| r == "accessNotConfigured")
    }

    /// The service-enablement URL embedded in Google's message for the
    /// `accessNotConfigured` case, if there is one.
    pub fn enable_url(&self) -> Option<&str> {
        if !self.is_access_not_configured() {
            return None;
        }
        let msg = self.message.as_deref()?;
        let start = msg.find("https://")?;
        let rest = &msg[start..];
        Some(rest.split_whitespace().next().unwrap_or(rest))
    }
}

impl std::fmt::Display for CalendarApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.message {
            Some(msg) => write!(f, "HTTP status {} from the Calendar API: {msg}", self.status),
            None => write!(f, "HTTP status {} from the Calendar API", self.status),
        }
    }
}

impl std::error::Error for CalendarApiError {}

/// Reads the body of a non-2xx Calendar API response and turns it into a
/// [`CalendarApiError`]. Never panics and never lets a body-read or parse
/// failure replace the real HTTP error: an absent, unreadable, or
/// non-matching body just yields a status-only error.
async fn api_error(resp: reqwest::Response) -> anyhow::Error {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let envelope: Option<ApiErrorEnvelope> =
        if body.trim().is_empty() { None } else { serde_json::from_str(&body).ok() };
    let (message, reasons) = match envelope {
        Some(env) => {
            (env.error.message, env.error.errors.into_iter().filter_map(|e| e.reason).collect())
        }
        None => (None, Vec::new()),
    };
    CalendarApiError { status, message, reasons }.into()
}

pub const API_BASE: &str = "https://www.googleapis.com/calendar/v3";

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<GoogleEvent> {
        let raw = include_str!("../tests/fixtures/events.json");
        let page: EventsPage = serde_json::from_str(raw).unwrap();
        page.items
    }

    fn at(h: u32, m: u32) -> DateTime<Local> {
        // Built from a fixed +02:00 offset (matching the fixture's events)
        // rather than the host's local wall clock, so the instant this
        // represents is the same absolute moment under any TZ the test
        // process runs in.
        format!("2026-08-25T{h:02}:{m:02}:00+02:00")
            .parse::<DateTime<chrono::FixedOffset>>()
            .unwrap()
            .with_timezone(&Local)
    }

    fn cfg() -> Config {
        Config {
            client_id: "id".into(),
            client_secret: "secret".into(),
            calendar_id: "primary".into(),
            warn_before_minutes: 5,
        }
    }

    #[test]
    fn offsets_in_the_payload_are_honoured() {
        // e1 is 14:00+02:00, i.e. 12:00 UTC, whatever this machine's zone is.
        let t = to_tasks(&fixture()).into_iter().find(|t| t.id == "e1").unwrap();
        assert_eq!(t.start.naive_utc().format("%H:%M").to_string(), "12:00");
    }

    #[test]
    fn all_day_cancelled_and_declined_events_are_dropped() {
        let ids: Vec<String> = to_tasks(&fixture()).into_iter().map(|t| t.id).collect();
        assert_eq!(ids, vec!["e1", "e2", "e6"]);
    }

    #[test]
    fn an_event_without_a_title_still_shows_something() {
        let untitled = to_tasks(&fixture()).into_iter().find(|t| t.id == "e6").unwrap();
        assert_eq!(untitled.title, "(no title)");
    }

    #[test]
    fn now_and_later_are_split_at_the_current_instant() {
        let (now_tasks, later) = partition(to_tasks(&fixture()), at(14, 30));
        assert_eq!(now_tasks.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), vec!["e1"]);
        assert_eq!(later.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), vec!["e2", "e6"]);
    }

    #[test]
    fn an_event_that_just_ended_is_in_neither_list() {
        let (now_tasks, later) = partition(to_tasks(&fixture()), at(15, 31));
        assert!(!now_tasks.iter().any(|t| t.id == "e1"));
        assert!(!later.iter().any(|t| t.id == "e1"));
    }

    #[test]
    fn the_window_ends_at_local_midnight() {
        let eod = end_of_day(at(14, 0));
        assert_eq!(eod.format("%Y-%m-%d %H:%M:%S").to_string(), "2026-08-26 00:00:00");
    }

    #[tokio::test]
    async fn a_401_triggers_a_refresh_and_the_request_is_retried() {
        use wiremock::{matchers::{method, path, header}, Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/calendars/primary/events"))
            .and(header("authorization", "Bearer stale"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/calendars/primary/events"))
            .and(header("authorization", "Bearer fresh"))
            .respond_with(ResponseTemplate::new(200).set_body_string(include_str!("../tests/fixtures/events.json")))
            .expect(1)
            .mount(&server)
            .await;

        let out = fetch_with_retry(&server.uri(), "stale", |_| async { Ok("fresh".to_string()) }, &cfg(), at(14, 30)).await.unwrap();
        assert!(out.iter().any(|t| t.id == "e1"));
    }

    // ---- Surfacing the Calendar API's own error message ---------------------

    #[tokio::test]
    async fn a_403_with_the_access_not_configured_envelope_surfaces_googles_message() {
        use wiremock::{matchers::{method, path}, Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/calendars/primary/events"))
            .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
                "error": {
                    "code": 403,
                    "message": "Google Calendar API has not been used in project 209385970212 before or it is disabled. Enable it by visiting https://console.developers.google.com/apis/api/calendar-json.googleapis.com/overview?project=209385970212 then retry.",
                    "errors": [{"domain": "usageLimits", "reason": "accessNotConfigured"}],
                    "status": "PERMISSION_DENIED"
                }
            })))
            .mount(&server)
            .await;

        let err = fetch_with_retry(&server.uri(), "token", |_| async { unreachable!() }, &cfg(), at(14, 30))
            .await
            .unwrap_err();
        let rendered = format!("{err:#}");
        assert!(
            rendered.contains("Google Calendar API has not been used in project 209385970212"),
            "lost Google's message: {rendered}"
        );

        // And the structured pieces are reachable too, for the log's
        // actionable-URL line and any future menu hint.
        let api_err = err.downcast_ref::<CalendarApiError>().expect("a CalendarApiError");
        assert!(api_err.is_access_not_configured());
        assert_eq!(
            api_err.enable_url(),
            Some("https://console.developers.google.com/apis/api/calendar-json.googleapis.com/overview?project=209385970212")
        );
    }

    #[tokio::test]
    async fn a_403_with_an_empty_body_still_names_the_status_without_panicking() {
        use wiremock::{matchers::{method, path}, Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/calendars/primary/events"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let err = fetch_with_retry(&server.uri(), "token", |_| async { unreachable!() }, &cfg(), at(14, 30))
            .await
            .unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("403"), "must still name the status: {rendered}");
    }

    #[tokio::test]
    async fn a_403_with_a_non_json_body_still_names_the_status_without_panicking() {
        use wiremock::{matchers::{method, path}, Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/calendars/primary/events"))
            .respond_with(ResponseTemplate::new(403).set_body_string("<html>Forbidden</html>"))
            .mount(&server)
            .await;

        let err = fetch_with_retry(&server.uri(), "token", |_| async { unreachable!() }, &cfg(), at(14, 30))
            .await
            .unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("403"), "must still name the status: {rendered}");
    }

    #[tokio::test]
    async fn a_500_with_an_unrelated_body_is_an_error_and_never_triggers_a_refresh() {
        use wiremock::{matchers::{method, path}, Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/calendars/primary/events"))
            .respond_with(ResponseTemplate::new(500).set_body_json(serde_json::json!({"foo": "bar"})))
            .mount(&server)
            .await;

        // Only a 401 is supposed to trigger a refresh-and-retry; a 5xx must
        // go straight to an error without ever calling this closure.
        let err = fetch_with_retry(&server.uri(), "token", |_| async { panic!("must not refresh on a 500") }, &cfg(), at(14, 30))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("500"));
    }

    #[tokio::test]
    async fn a_token_shaped_string_outside_the_error_envelope_never_leaks_into_the_error() {
        use wiremock::{matchers::{method, path}, Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        // A pathological body: a real `message` (which this path is meant to
        // surface) alongside token-shaped strings sitting in fields this
        // module never extracts. Only an explicit allowlist of fields
        // (`error.message`, `error.errors[].reason`) is read out of the
        // envelope, so nothing else in the body — however it looks — can
        // reach the resulting error, the same allowlisting principle
        // `auth.rs` relies on for the token endpoint, just applied to a body
        // that is safe to show in full rather than one that never is.
        Mock::given(method("GET"))
            .and(path("/calendars/primary/events"))
            .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
                "error": {
                    "code": 403,
                    "message": "Permission denied.",
                    "errors": [{"domain": "ya29.a0AWY7CkZ-should-not-leak", "reason": "forbidden"}],
                    "unexpected_field": "1//09fake-refresh-token-shaped-string"
                }
            })))
            .mount(&server)
            .await;

        let err = fetch_with_retry(&server.uri(), "the-access-token-in-use", |_| async { unreachable!() }, &cfg(), at(14, 30))
            .await
            .unwrap_err();
        let rendered = format!("{err:#}");
        assert!(!rendered.contains("ya29.a0AWY7CkZ"), "leaked a token-shaped string from an unmodeled field: {rendered}");
        assert!(!rendered.contains("1//09fake-refresh-token-shaped-string"), "leaked a token-shaped string from an unmodeled field: {rendered}");
        assert!(!rendered.contains("the-access-token-in-use"), "leaked our own access token: {rendered}");
        assert!(rendered.contains("Permission denied"), "lost the real message: {rendered}");
    }
}
