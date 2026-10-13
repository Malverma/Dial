//! User configuration (`config.toml`) and last-used brightness (`state.toml`).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::discovery::MonitorInfo;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum BackendPref {
    #[default]
    Auto,
    Backlight,
    Ddc,
    Gamma,
    Overlay,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct General {
    pub master_mode: String,
    pub restore_on_start: bool,
    pub scroll_step: u8,
}

impl Default for General {
    fn default() -> Self {
        Self { master_mode: "absolute".into(), restore_on_start: true, scroll_step: 5 }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Match {
    pub connector: Option<String>,
    pub make: Option<String>,
    pub model: Option<String>,
    pub serial: Option<String>,
}

impl Match {
    /// Every field that is set must match; an empty match matches nothing.
    pub fn matches(&self, info: &MonitorInfo) -> bool {
        let fields = [
            (&self.connector, Some(&info.connector)),
            (&self.make, info.make.as_ref()),
            (&self.model, info.model.as_ref()),
            (&self.serial, info.serial.as_ref()),
        ];
        let mut any = false;
        for (want, have) in fields {
            if let Some(want) = want {
                any = true;
                if have.map(|h| h.trim()) != Some(want.trim()) {
                    return false;
                }
            }
        }
        any
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MonitorOverride {
    #[serde(rename = "match")]
    pub matcher: Match,
    pub name: Option<String>,
    pub backend: Option<BackendPref>,
    pub min: Option<u8>,
    pub curve_gamma: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub general: General,
    #[serde(rename = "monitor")]
    pub monitors: Vec<MonitorOverride>,
}

/// Effective settings for one monitor after applying overrides.
#[derive(Debug, Clone)]
pub struct MonitorSettings {
    pub name: Option<String>,
    pub backend: BackendPref,
    pub min: Option<u8>,
    pub curve_gamma: Option<f64>,
}

pub const DEFAULT_MIN: u8 = 10;

impl Config {
    pub fn load() -> Self {
        let path = config_dir().join("config.toml");
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text).unwrap_or_else(|e| {
                eprintln!("dial: ignoring invalid {}: {e}", path.display());
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    /// Later matching entries override earlier ones field by field.
    pub fn settings_for(&self, info: &MonitorInfo) -> MonitorSettings {
        let mut s = MonitorSettings { name: None, backend: BackendPref::Auto, min: None, curve_gamma: None };
        for o in self.monitors.iter().filter(|o| o.matcher.matches(info)) {
            if o.name.is_some() {
                s.name = o.name.clone();
            }
            if let Some(b) = o.backend {
                s.backend = b;
            }
            if o.min.is_some() {
                s.min = o.min;
            }
            if o.curve_gamma.is_some() {
                s.curve_gamma = o.curve_gamma;
            }
        }
        s
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct State {
    #[serde(default)]
    pub brightness: BTreeMap<String, u8>,
    /// Locked volume per audio device name, in percent.
    #[serde(default)]
    pub volume: BTreeMap<String, u8>,
    /// Volume per application (process binary), in percent.
    #[serde(default)]
    pub app_volume: BTreeMap<String, u8>,
    /// Per-application override of whether Dial holds its volume.
    #[serde(default)]
    pub app_lock: BTreeMap<String, bool>,
    /// Every app seen so far, id to name, so its lock setting survives it closing.
    #[serde(default)]
    pub known_apps: BTreeMap<String, String>,
    /// Only show sliders for apps that are playing audio.
    #[serde(default)]
    pub hide_silent_apps: bool,
    /// Output made the default whenever it appears, and its description for
    /// listing it while it's disconnected.
    #[serde(default)]
    pub preferred_output: Option<String>,
    #[serde(default)]
    pub preferred_output_label: Option<String>,
}

/// Apps that manage their own volumes (and mic gain) and are left alone by default.
pub const UNLOCKED_BY_DEFAULT: &[&str] = &["vesktop"];

impl State {
    pub fn app_locked(&self, app: &str) -> bool {
        self.app_lock.get(app).copied().unwrap_or(!UNLOCKED_BY_DEFAULT.contains(&app))
    }
}

impl State {
    pub fn load() -> Self {
        std::fs::read_to_string(state_dir().join("state.toml"))
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let dir = state_dir();
        let result = std::fs::create_dir_all(&dir).and_then(|_| {
            let text = toml::to_string(self).map_err(std::io::Error::other)?;
            let tmp = dir.join("state.toml.tmp");
            std::fs::write(&tmp, text)?;
            std::fs::rename(tmp, dir.join("state.toml"))
        });
        if let Err(e) = result {
            eprintln!("dial: failed to save state: {e}");
        }
    }
}

fn xdg_dir(var: &str, fallback: &str) -> PathBuf {
    let base = std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(fallback));
    let dir = base.join("dial");
    // Carry settings over from before the rename.
    let old = base.join("dimmer");
    if !dir.exists() && old.is_dir() {
        let _ = std::fs::rename(&old, &dir);
    }
    dir
}

pub fn config_dir() -> PathBuf {
    xdg_dir("XDG_CONFIG_HOME", ".config")
}

pub fn state_dir() -> PathBuf {
    xdg_dir("XDG_STATE_HOME", ".local/state")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(connector: &str, model: &str, serial: &str) -> MonitorInfo {
        MonitorInfo {
            connector: connector.into(),
            make: Some("Dell Inc.".into()),
            model: Some(model.into()),
            serial: Some(serial.into()),
            description: None,
            x: 0,
            y: 0,
        }
    }

    const SAMPLE: &str = r#"
        [general]
        restore_on_start = false
        scroll_step = 2

        [[monitor]]
        match = { make = "Dell Inc.", model = "DELL U2720Q", serial = "ABC123" }
        name = "Left Dell"
        backend = "ddc"
        min = 15

        [[monitor]]
        match = { connector = "eDP-1" }
        backend = "backlight"
        curve_gamma = 2.2
    "#;

    #[test]
    fn parses_sample() {
        let c = Config::parse(SAMPLE).unwrap();
        assert!(!c.general.restore_on_start);
        assert_eq!(c.general.scroll_step, 2);
        assert_eq!(c.general.master_mode, "absolute");
        assert_eq!(c.monitors.len(), 2);
        assert_eq!(c.monitors[0].backend, Some(BackendPref::Ddc));
    }

    #[test]
    fn empty_config_is_default() {
        let c = Config::parse("").unwrap();
        assert!(c.general.restore_on_start);
        assert!(c.monitors.is_empty());
    }

    #[test]
    fn matches_by_edid_identity() {
        let c = Config::parse(SAMPLE).unwrap();
        let s = c.settings_for(&info("DP-3", "DELL U2720Q", "ABC123"));
        assert_eq!(s.name.as_deref(), Some("Left Dell"));
        assert_eq!(s.backend, BackendPref::Ddc);
        assert_eq!(s.min, Some(15));

        let s = c.settings_for(&info("DP-3", "DELL U2720Q", "OTHER"));
        assert_eq!(s.backend, BackendPref::Auto);
        assert_eq!(s.name, None);
    }

    #[test]
    fn matches_by_connector() {
        let c = Config::parse(SAMPLE).unwrap();
        let s = c.settings_for(&info("eDP-1", "x", "y"));
        assert_eq!(s.backend, BackendPref::Backlight);
        assert_eq!(s.curve_gamma, Some(2.2));
    }

    #[test]
    fn empty_match_matches_nothing() {
        assert!(!Match::default().matches(&info("DP-1", "m", "s")));
    }
}
