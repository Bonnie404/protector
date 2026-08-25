use chrono::{DateTime, Local};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub start: DateTime<Local>,
    pub end: DateTime<Local>,
}

#[derive(Debug, Clone)]
pub struct Selection {
    pub task: Task,
    pub warned: bool,
    pub ended_notified: bool,
}

pub fn format_countdown(remaining_secs: i64) -> String {
    let overtime = remaining_secs < 0;
    let secs = remaining_secs.unsigned_abs();
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    let clock = if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    };
    if overtime {
        format!("\u{26a0} +{clock}")
    } else {
        clock
    }
}

pub fn truncate_title(title: &str, max_chars: usize) -> String {
    if title.chars().count() <= max_chars {
        return title.to_string();
    }
    let kept: String = title.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{kept}\u{2026}")
}

pub const MAX_TITLE_CHARS: usize = 24;

pub fn panel_label(selection: Option<&Selection>, connected: bool, now: DateTime<Local>) -> String {
    match selection {
        Some(sel) => {
            let remaining = (sel.task.end - now).num_seconds();
            format!(
                "{} \u{b7} {}",
                format_countdown(remaining),
                truncate_title(&sel.task.title, MAX_TITLE_CHARS)
            )
        }
        None if connected => "Pick a task".to_string(),
        None => "Connect calendar".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32, m: u32, s: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 8, 25, h, m, s).unwrap()
    }

    fn task(title: &str, end: DateTime<Local>) -> Task {
        Task { id: "e1".into(), title: title.into(), start: at(9, 0, 0), end }
    }

    #[test]
    fn countdown_over_an_hour_shows_hours() {
        assert_eq!(format_countdown(5025), "1:23:45");
    }

    #[test]
    fn countdown_under_an_hour_shows_padded_minutes() {
        assert_eq!(format_countdown(2537), "42:17");
        assert_eq!(format_countdown(598), "09:58");
        assert_eq!(format_countdown(0), "00:00");
    }

    #[test]
    fn countdown_past_zero_is_overtime() {
        assert_eq!(format_countdown(-271), "\u{26a0} +04:31");
        assert_eq!(format_countdown(-3661), "\u{26a0} +1:01:01");
    }

    #[test]
    fn long_titles_are_truncated_by_characters() {
        assert_eq!(truncate_title("Design review", 24), "Design review");
        assert_eq!(
            truncate_title("Quarterly planning workshop with the team", 24),
            "Quarterly planning work\u{2026}"
        );
        assert_eq!(truncate_title("\u{00fc}bung \u{00fc}bung \u{00fc}bung \u{00fc}bung \u{00fc}bung x", 24).chars().count(), 24);
    }

    #[test]
    fn label_shows_countdown_and_title() {
        let sel = Selection { task: task("Design review", at(15, 30, 0)), warned: false, ended_notified: false };
        assert_eq!(panel_label(Some(&sel), true, at(14, 6, 15)), "1:23:45 \u{b7} Design review");
    }

    #[test]
    fn label_without_selection_asks_for_one() {
        assert_eq!(panel_label(None, true, at(14, 0, 0)), "Pick a task");
    }

    #[test]
    fn label_without_credentials_asks_to_connect() {
        assert_eq!(panel_label(None, false, at(14, 0, 0)), "Connect calendar");
    }
}
