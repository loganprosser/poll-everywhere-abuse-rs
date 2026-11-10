//! App settings: the same JSON file and keys as the Python app
//! (~/Library/Application Support/pollevbot/app_settings.json), so settings
//! carry over between the two versions.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub fn app_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("Library/Application Support/pollevbot")
}

pub fn settings_path() -> PathBuf {
    app_dir().join("app_settings.json")
}

pub fn logs_dir() -> PathBuf {
    app_dir().join("pollev_logs")
}

/// Chrome's own profile directory: cookies and localStorage persist natively,
/// so a completed login survives restarts (replaces Playwright's
/// storage_state file).
pub fn chrome_profile_dir() -> PathBuf {
    app_dir().join("chrome-profile")
}

/// The optional nrp-claude launcher (not part of this repo). NRP_CLAUDE
/// overrides the default location.
pub fn nrp_script() -> PathBuf {
    std::env::var_os("NRP_CLAUDE").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var("HOME").unwrap_or_default())
            .join("Documents/dev/cornell/fall2026/nrp-ai/nrp-claude")
    })
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub poll_url: String,
    pub username: String,
    pub headless: bool,
    /// true = grant + feed the fake position below; false = deny outright.
    /// There is deliberately no "real location" mode.
    pub geolocation_enabled: bool,
    pub geo_lat: f64,
    pub geo_lon: f64,
    pub geo_accuracy: f64,
    pub notify_mac: bool,
    pub notify_sound: bool,
    pub notify_ntfy: bool,
    pub ntfy_topic: String,
    pub notify_imessage: bool,
    pub imessage_recipient: String,
    pub ai_enabled: bool,
    pub ai_require_confirmation: bool,
    pub ai_backend: String,
    pub ai_model: String,
    pub ai_effort: String,
    pub ai_test_on_startup: bool,
    /// (min, max) seconds after a poll opens before our answer goes in, picked
    /// at random per question; it goes in sooner once someone else has answered.
    pub answer_delay_secs: (f64, f64),
    /// Presets shown as one-click choices on the Watch page.
    pub saved_polls: Vec<SavedPoll>,
}

/// A preset: poll link + screen name + fake location, applied together.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedPoll {
    pub url: String,
    pub username: String,
    /// 0, 0 = no location saved with it.
    pub geo_lat: f64,
    pub geo_lon: f64,
}

impl Default for AppConfig {
    /// First-run values, same rules as the Python app's seeding: channels
    /// with nothing to send to start off, and machines without the NRP
    /// launcher start on the Claude CLI.
    fn default() -> Self {
        let nrp = nrp_script().exists();
        Self {
            poll_url: String::new(),
            username: String::new(),
            headless: false,
            geolocation_enabled: true,
            geo_lat: 0.0,
            geo_lon: 0.0,
            geo_accuracy: 50.0,
            notify_mac: true,
            notify_sound: true,
            notify_ntfy: false,
            ntfy_topic: String::new(),
            notify_imessage: false,
            imessage_recipient: String::new(),
            ai_enabled: true,
            ai_require_confirmation: false,
            ai_backend: if nrp { "nrp" } else { "subscription" }.into(),
            ai_model: if nrp { "qwen3" } else { "sonnet" }.into(),
            ai_effort: String::new(),
            ai_test_on_startup: false,
            answer_delay_secs: (13.8, 20.4), // 0.23-0.34 min
            saved_polls: Vec::new(),
        }
    }
}

impl AppConfig {
    /// Missing or corrupt file -> defaults (a bad file must not brick the
    /// app; the GUI re-saves over it on the next change).
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)? + "\n")?;
        Ok(())
    }

    /// Human-readable problems; empty if the config is usable. Same checks
    /// and wording as the Python app.
    pub fn validate(&self) -> Vec<String> {
        let mut p = Vec::new();
        if self.poll_url.trim().is_empty() {
            p.push("Poll URL is empty".to_string());
        } else if !self.poll_url.contains("pollev.com") {
            p.push(format!("Poll URL doesn't look like a pollev.com link: {:?}", self.poll_url));
        }
        if self.username.trim().is_empty() {
            p.push("Username is empty".to_string());
        }
        if !["nrp", "subscription", "codex"].contains(&self.ai_backend.as_str()) {
            p.push(format!("ai_backend must be 'nrp', 'subscription', or 'codex', got {:?}", self.ai_backend));
        }
        if !self.ai_effort.is_empty() && !["low", "medium", "high", "xhigh", "max"].contains(&self.ai_effort.as_str()) {
            p.push(format!("ai_effort must be low/medium/high/xhigh/max or empty, got {:?}", self.ai_effort));
        }
        if self.notify_imessage && self.imessage_recipient.trim().is_empty() {
            p.push("iMessage notifications are on but no recipient is set".to_string());
        }
        if self.notify_ntfy && self.ntfy_topic.trim().is_empty() {
            p.push("ntfy notifications are on but no topic is set".to_string());
        }
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_validate() {
        let path = std::env::temp_dir().join(format!("pea-config-{}.json", std::process::id()));
        let cfg = AppConfig { poll_url: "https://pollev.com/x".into(), username: "me".into(), ..Default::default() };
        cfg.save(&path).unwrap();
        let back = AppConfig::load(&path);
        std::fs::remove_file(&path).ok();
        assert_eq!(back, cfg);
        assert!(back.validate().is_empty(), "{:?}", back.validate());
        assert_eq!(AppConfig::default().validate(), ["Poll URL is empty", "Username is empty"]);
    }

    #[test]
    fn reads_python_app_settings() {
        // Python writes ints for whole floats and may carry keys we don't know.
        let cfg: AppConfig = serde_json::from_str(r#"{"poll_url": "https://pollev.com/a", "geo_lat": 42, "extra": 1}"#).unwrap();
        assert_eq!(cfg.geo_lat, 42.0);
        assert_eq!(cfg.geo_accuracy, 50.0);
    }
}
