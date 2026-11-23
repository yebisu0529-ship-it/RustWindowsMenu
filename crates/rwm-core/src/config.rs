use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::model::HandlerPolicy;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse config TOML in {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("failed to write config file {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid configuration: {0}")]
    Validation(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePreference {
    #[default]
    Auto,
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Density {
    Compact,
    #[default]
    Classic,
    Comfortable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceConfig {
    pub theme: ThemePreference,
    pub density: Density,
    pub font_family: Option<String>,
    pub show_icons: bool,
    pub show_shortcuts: bool,
    pub show_timings: bool,
    pub corner_radius: u32,
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            theme: ThemePreference::Auto,
            density: Density::Classic,
            font_family: None,
            show_icons: true,
            show_shortcuts: true,
            show_timings: false,
            corner_radius: 6,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BehaviorConfig {
    pub shift_bypass_to_native: bool,
    pub enable_async_com_hydration: bool,
    pub show_loading_footer: bool,
    pub com_handler_budget_ms: u64,
    pub submenu_hover_delay_ms: u32,
    pub terminal_command: String,
}

impl Default for BehaviorConfig {
    fn default() -> Self {
        Self {
            shift_bypass_to_native: true,
            enable_async_com_hydration: true,
            show_loading_footer: true,
            com_handler_budget_ms: 0,
            submenu_hover_delay_ms: 140,
            terminal_command: "wt.exe -d \"{dir}\"".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HandlersConfig {
    pub default_policy: HandlerPolicy,
    pub submenu_title: String,
    pub overrides: BTreeMap<String, HandlerPolicy>,
}

impl Default for HandlersConfig {
    fn default() -> Self {
        Self {
            default_policy: HandlerPolicy::Inline,
            submenu_title: "拡張メニュー".to_string(),
            overrides: BTreeMap::new(),
        }
    }
}

impl HandlersConfig {
    pub fn policy_for(&self, clsid: &str, name: &str) -> HandlerPolicy {
        let clsid_norm = clsid.trim();
        let name_norm = name.trim();
        for (k, &policy) in &self.overrides {
            let kt = k.trim();
            if kt.eq_ignore_ascii_case(clsid_norm) || (!name_norm.is_empty() && kt.eq_ignore_ascii_case(name_norm)) {
                return policy;
            }
        }
        self.default_policy
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CustomPosition {
    #[default]
    Top,
    Bottom,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CustomMenuItem {
    pub label: String,
    pub command: String,
    pub args: Vec<String>,
    pub icon: Option<String>,
    pub scopes: Vec<String>,
    pub extensions: Vec<String>,
    pub position: CustomPosition,
}

impl Default for CustomMenuItem {
    fn default() -> Self {
        Self {
            label: String::new(),
            command: String::new(),
            args: Vec::new(),
            icon: None,
            scopes: vec!["all".to_string()],
            extensions: Vec::new(),
            position: CustomPosition::Top,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RwmConfig {
    pub appearance: AppearanceConfig,
    pub behavior: BehaviorConfig,
    pub handlers: HandlersConfig,
    pub custom_items: Vec<CustomMenuItem>,
}

#[allow(clippy::result_large_err, reason = "Preserve the public ConfigError field types")]
impl RwmConfig {
    pub fn app_data_dir() -> PathBuf {
        if let Ok(appdata) = std::env::var("APPDATA") {
            return PathBuf::from(appdata).join("RustWindowsMenu");
        }
        PathBuf::from(".").join("RustWindowsMenu")
    }

    pub fn local_data_dir() -> PathBuf {
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            return PathBuf::from(local).join("RustWindowsMenu");
        }
        PathBuf::from(".").join("RustWindowsMenu")
    }

    pub fn kill_switch_path() -> PathBuf {
        Self::local_data_dir().join("DISABLE_HOOK")
    }

    pub fn config_path() -> PathBuf {
        if let Ok(override_path) = std::env::var("RWM_CONFIG") {
            if !override_path.trim().is_empty() {
                return PathBuf::from(override_path);
            }
        }
        Self::app_data_dir().join("config.toml")
    }

    pub fn load_or_default() -> Result<Self, ConfigError> {
        let path = Self::config_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Result<Self, ConfigError> {
        let raw = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let cfg: RwmConfig = toml::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn write_default_if_missing() -> Result<(PathBuf, bool), ConfigError> {
        let path = Self::config_path();
        if path.exists() {
            return Ok((path, false));
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
                path: path.clone(),
                source,
            })?;
        }
        let content = include_str!("../../../config.example.toml");
        fs::write(&path, content).map_err(|source| ConfigError::Write {
            path: path.clone(),
            source,
        })?;
        Ok((path, true))
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.appearance.corner_radius > 24 {
            return Err(ConfigError::Validation(
                "appearance.corner_radius must be <= 24".into(),
            ));
        }
        if self.behavior.submenu_hover_delay_ms > 2000 {
            return Err(ConfigError::Validation(
                "behavior.submenu_hover_delay_ms must be <= 2000".into(),
            ));
        }
        if self.handlers.submenu_title.trim().is_empty() {
            return Err(ConfigError::Validation(
                "handlers.submenu_title must not be empty".into(),
            ));
        }
        for (idx, item) in self.custom_items.iter().enumerate() {
            if item.label.trim().is_empty() {
                return Err(ConfigError::Validation(format!(
                    "custom_items[{idx}].label must not be empty"
                )));
            }
            if item.command.trim().is_empty() {
                return Err(ConfigError::Validation(format!(
                    "custom_items[{idx}].command must not be empty"
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses_and_validates() {
        let content = include_str!("../../../config.example.toml");
        let cfg: RwmConfig = toml::from_str(content).expect("example config must parse");
        cfg.validate().expect("example config must validate");
        assert_eq!(cfg.handlers.default_policy, HandlerPolicy::Inline);
    }

    #[test]
    fn handler_override_matches_name_or_clsid_case_insensitively() {
        let mut cfg = RwmConfig::default();
        cfg.handlers
            .overrides
            .insert("filesyncex".into(), HandlerPolicy::Submenu);
        cfg.handlers.overrides.insert(
            "{23170F69-40C1-278A-1000-000100020000}".into(),
            HandlerPolicy::Inline,
        );

        assert_eq!(
            cfg.handlers
                .policy_for("{CB3D0F55-BC2C-4C1A-85ED-23ED75B5106B}", "FileSyncEx"),
            HandlerPolicy::Submenu
        );
        assert_eq!(
            cfg.handlers
                .policy_for("{23170f69-40c1-278a-1000-000100020000}", "7-Zip"),
            HandlerPolicy::Inline
        );
        assert_eq!(
            cfg.handlers.policy_for("{00000000-0000-0000-0000-000000000000}", "Other"),
            HandlerPolicy::Inline
        );
    }
}
