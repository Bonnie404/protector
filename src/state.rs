use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::task::{Selection, Task};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedSelection {
    pub id: String,
    pub title: String,
    pub start: DateTime<Local>,
    pub end: DateTime<Local>,
    pub warned: bool,
    pub ended_notified: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PersistedState {
    pub selection: Option<PersistedSelection>,
    pub last_sync: Option<DateTime<Local>>,
}

impl PersistedState {
    pub fn from_selection(sel: Option<&Selection>, last_sync: Option<DateTime<Local>>) -> Self {
        Self {
            selection: sel.map(|s| PersistedSelection {
                id: s.task.id.clone(),
                title: s.task.title.clone(),
                start: s.task.start,
                end: s.task.end,
                warned: s.warned,
                ended_notified: s.ended_notified,
            }),
            last_sync,
        }
    }
}

pub fn state_path() -> PathBuf {
    dirs::state_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap().join(".local/state"))
        .join("protector/state.json")
}

/// Never fails: a missing or corrupt state file is not worth refusing to start over.
pub fn load(path: &Path) -> PersistedState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Atomic: write a sibling temp file, then rename over the target, so a crash or
/// power loss mid-write can never leave a half-written file where the real one
/// belongs.
pub fn save(path: &Path, state: &PersistedState) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(state)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// How long past a persisted task's end restoring it on startup is still
/// worthwhile; older than this, the countdown is stale enough to just ask again.
pub const RESTORE_GRACE_SECS: i64 = 3600;

pub fn restore_selection(p: &PersistedState, now: DateTime<Local>) -> Option<Selection> {
    let s = p.selection.as_ref()?;
    if (now - s.end).num_seconds() > RESTORE_GRACE_SECS {
        return None;
    }
    Some(Selection {
        task: Task { id: s.id.clone(), title: s.title.clone(), start: s.start, end: s.end },
        warned: s.warned,
        ended_notified: s.ended_notified,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32, m: u32) -> DateTime<Local> { Local.with_ymd_and_hms(2026, 8, 25, h, m, 0).unwrap() }

    fn selection(end: DateTime<Local>) -> Selection {
        Selection {
            task: Task { id: "e1".into(), title: "Design review".into(), start: at(14, 0), end },
            warned: true,
            ended_notified: false,
        }
    }

    #[test]
    fn a_saved_state_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let original = PersistedState::from_selection(Some(&selection(at(15, 30))), Some(at(14, 3)));
        save(&path, &original).unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.selection.as_ref().unwrap().id, "e1");
        assert!(loaded.selection.as_ref().unwrap().warned);
    }

    #[test]
    fn a_missing_file_loads_as_empty_rather_than_failing() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = load(&dir.path().join("absent.json"));
        assert!(loaded.selection.is_none());
    }

    #[test]
    fn a_corrupt_file_loads_as_empty_rather_than_failing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, b"{not json").unwrap();
        assert!(load(&path).selection.is_none());
    }

    #[test]
    fn a_recent_selection_is_restored() {
        let p = PersistedState::from_selection(Some(&selection(at(15, 30))), None);
        assert!(restore_selection(&p, at(15, 50)).is_some());
    }

    #[test]
    fn a_selection_that_ended_long_ago_is_dropped() {
        let p = PersistedState::from_selection(Some(&selection(at(15, 30))), None);
        assert!(restore_selection(&p, at(17, 0)).is_none());
    }
}
