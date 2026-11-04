//! Loading and validation of the TOML configuration file.

use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use evdev::KeyCode;
use serde::Deserialize;

pub const DEFAULT_THRESHOLD_MS: u64 = 50;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default = "default_threshold")]
    threshold_ms: u64,
    #[serde(default)]
    keys: HashMap<String, u64>,
    #[serde(default)]
    devices: DeviceSelection,
}

fn default_threshold() -> u64 {
    DEFAULT_THRESHOLD_MS
}

/// Selection of the devices to filter.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceSelection {
    /// Name substrings or paths (`/dev/input/by-id/...`). Empty = all keyboards.
    #[serde(default)]
    pub include: Vec<String>,
    /// Name substrings or paths to ignore, takes precedence over `include`.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Also filter mouse buttons.
    #[serde(default)]
    pub mice: bool,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub threshold: Duration,
    pub key_thresholds: HashMap<KeyCode, Duration>,
    pub devices: DeviceSelection,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            threshold: Duration::from_millis(DEFAULT_THRESHOLD_MS),
            key_thresholds: HashMap::new(),
            devices: DeviceSelection::default(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("invalid configuration: {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let raw: RawConfig = toml::from_str(text)?;
        let mut key_thresholds = HashMap::new();
        for (name, ms) in raw.keys {
            key_thresholds.insert(parse_key(&name)?, Duration::from_millis(ms));
        }
        Ok(Self {
            threshold: Duration::from_millis(raw.threshold_ms),
            key_thresholds,
            devices: raw.devices,
        })
    }

    pub fn threshold_for(&self, key: KeyCode) -> Duration {
        self.key_thresholds.get(&key).copied().unwrap_or(self.threshold)
    }
}

/// Accepts `KEY_A`, `BTN_LEFT` or the short form `A` / `a`.
fn parse_key(name: &str) -> Result<KeyCode> {
    let upper = name.to_ascii_uppercase();
    KeyCode::from_str(&upper)
        .or_else(|_| KeyCode::from_str(&format!("KEY_{upper}")))
        .map_err(|_| anyhow!("unknown key: {name:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_empty() {
        let c = Config::parse("").unwrap();
        assert_eq!(c.threshold, Duration::from_millis(DEFAULT_THRESHOLD_MS));
        assert!(c.devices.include.is_empty());
        assert!(!c.devices.mice);
    }

    #[test]
    fn per_key_thresholds() {
        let c = Config::parse(
            r#"
            threshold_ms = 40
            [keys]
            KEY_SPACE = 100
            e = 0
            BTN_LEFT = 30
            "#,
        )
        .unwrap();
        assert_eq!(c.threshold_for(KeyCode::KEY_SPACE), Duration::from_millis(100));
        assert_eq!(c.threshold_for(KeyCode::KEY_E), Duration::ZERO);
        assert_eq!(c.threshold_for(KeyCode::BTN_LEFT), Duration::from_millis(30));
        assert_eq!(c.threshold_for(KeyCode::KEY_A), Duration::from_millis(40));
    }

    #[test]
    fn unknown_key_is_an_error() {
        assert!(Config::parse("[keys]\nKEY_NOPE = 10").is_err());
    }

    #[test]
    fn unknown_field_is_an_error() {
        assert!(Config::parse("treshold_ms = 10").is_err());
    }
}
