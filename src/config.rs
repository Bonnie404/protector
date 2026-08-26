use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

fn default_calendar() -> String {
    "primary".into()
}

fn default_warn() -> i64 {
    5
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub client_secret: String,
    #[serde(default = "default_calendar")]
    pub calendar_id: String,
    #[serde(default = "default_warn")]
    pub warn_before_minutes: i64,
}

pub const TEMPLATE: &str = r#"# Protector configuration
#
# Create an OAuth client at https://console.cloud.google.com/apis/credentials
#   1. Create (or pick) a project, enable the Google Calendar API
#   2. Credentials -> Create credentials -> OAuth client ID -> Desktop app
#   3. Paste the client id and secret below
#
# See README.md for the full walkthrough.

client_id     = ""
client_secret = ""
calendar_id   = "primary"

# How many minutes before a block ends Protector sends the heads-up.
# Set it to 0 to switch that notification off; the end-of-block one still fires.
warn_before_minutes = 5
"#;

/// The directory the config file lives under. Falls back twice rather than
/// unwrapping, because `dirs::config_dir()` returning `None` is a strange
/// environment, not a reason to refuse to start.
fn config_root() -> PathBuf {
    dirs::config_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"))
}

/// The config file under a given root. Split out so a test can pin the suffix
/// against the *production* join rather than against a copy of it.
fn config_path_in(base: &Path) -> PathBuf {
    base.join("protector/config.toml")
}

pub fn config_path() -> PathBuf {
    config_path_in(&config_root())
}

pub fn load_or_create(path: &Path) -> anyhow::Result<Config> {
    if !path.exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, TEMPLATE)?;
    }
    let text = std::fs::read_to_string(path)?;
    Ok(toml::from_str(&text)?)
}

impl Config {
    pub fn is_complete(&self) -> bool {
        !self.client_id.is_empty() && !self.client_secret.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_config_is_created_from_the_template_and_reports_incomplete() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let cfg = load_or_create(&path).unwrap();
        assert!(path.exists());
        assert!(!cfg.is_complete());
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("client_id"));
        assert!(written.contains("console.cloud.google.com"));
    }

    #[test]
    fn a_filled_config_is_complete_and_defaults_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "client_id = \"abc.apps.googleusercontent.com\"\nclient_secret = \"s3cret\"\n").unwrap();
        let cfg = load_or_create(&path).unwrap();
        assert!(cfg.is_complete());
        assert_eq!(cfg.calendar_id, "primary");
        assert_eq!(cfg.warn_before_minutes, 5);
    }

    #[test]
    fn a_configured_warning_window_survives_the_round_trip_from_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "client_id = \"a\"\nclient_secret = \"b\"\nwarn_before_minutes = 15\n")
            .unwrap();
        assert_eq!(load_or_create(&path).unwrap().warn_before_minutes, 15);
    }

    #[test]
    fn the_template_documents_the_warning_window_it_writes() {
        // The field is only meaningful to someone who knows it exists.
        assert!(TEMPLATE.contains("warn_before_minutes = 5"));
        assert!(TEMPLATE.contains("heads-up"), "{TEMPLATE}");
    }

    #[test]
    fn malformed_toml_is_an_error_rather_than_silent_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "client_id = ").unwrap();
        assert!(load_or_create(&path).is_err());
    }

    #[test]
    fn config_path_with_a_known_base_returns_the_correct_suffix() {
        // The production join, not a copy of it: changing where the config
        // lives has to break this test rather than slip past it.
        let path = config_path_in(Path::new("/home/user/.config"));
        assert_eq!(path, PathBuf::from("/home/user/.config/protector/config.toml"));
    }

    #[test]
    fn config_path_is_that_suffix_under_the_real_config_root() {
        let path = config_path();
        assert!(path.ends_with("protector/config.toml"), "{}", path.display());
        assert!(path.starts_with(config_root()), "{}", path.display());
        assert_eq!(path, config_path_in(&config_root()));
    }
}
