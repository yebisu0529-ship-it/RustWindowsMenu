use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::reg_util::write_bytes_atomic;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not read {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("could not parse {path}: {source}")]
    Parse {
        path: PathBuf,
        source: Box<toml::de::Error>,
    },
    #[error("could not serialize configuration: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("could not write {path}: {source}")]
    Write { path: PathBuf, source: io::Error },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RwmConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub prebuild_on_rbuttondown: bool,
    #[serde(default = "default_arm_delay")]
    pub arm_delay_ms: u32,
    #[serde(default = "default_true")]
    pub no_animation: bool,
    #[serde(default = "default_true")]
    pub pin_handler_dlls: bool,
    #[serde(default = "default_max_pinned")]
    pub max_pinned_dlls: usize,
    #[serde(default)]
    pub classic_menu_fallback: bool,
    #[serde(default)]
    pub debug_logging: bool,
    #[serde(default = "default_true")]
    pub hover_prebuild: bool,
    #[serde(default = "default_hover_delay")]
    pub hover_delay_ms: u32,
    #[serde(default = "default_true")]
    pub selection_prebuild: bool,
    #[serde(default = "default_true")]
    pub host_menu_cache: bool,
    #[serde(default = "default_host_cache_entries")]
    pub host_cache_entries: usize,
    #[serde(default = "default_host_cache_ttl")]
    pub host_cache_ttl_ms: u32,
    #[serde(default = "default_true")]
    pub show_customize_entry: bool,
    #[serde(default)]
    pub hidden_items: Vec<String>,
}

fn default_true() -> bool {
    true
}
fn default_arm_delay() -> u32 {
    15
}
fn default_max_pinned() -> usize {
    32
}
fn default_hover_delay() -> u32 {
    120
}
fn default_host_cache_entries() -> usize {
    6
}
fn default_host_cache_ttl() -> u32 {
    8000
}

impl Default for RwmConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            prebuild_on_rbuttondown: default_true(),
            arm_delay_ms: default_arm_delay(),
            no_animation: default_true(),
            pin_handler_dlls: default_true(),
            max_pinned_dlls: default_max_pinned(),
            classic_menu_fallback: false,
            debug_logging: false,
            hover_prebuild: default_true(),
            hover_delay_ms: default_hover_delay(),
            selection_prebuild: default_true(),
            host_menu_cache: default_true(),
            host_cache_entries: default_host_cache_entries(),
            host_cache_ttl_ms: default_host_cache_ttl(),
            show_customize_entry: default_true(),
            hidden_items: Vec::new(),
        }
    }
}

fn known_folder(id: &windows_core::GUID) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{SHGetKnownFolderPath, KF_FLAG_DEFAULT};

    let raw = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None) }.ok()?;
    if raw.is_null() {
        return None;
    }
    let path = unsafe { std::ffi::OsString::from_wide(raw.as_wide()) };
    unsafe { CoTaskMemFree(Some(raw.0.cast())) };
    (!path.is_empty()).then(|| PathBuf::from(path))
}

fn data_dir(var: &str, folder_id: &windows_core::GUID) -> PathBuf {
    env::var_os(var)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| known_folder(folder_id))
        .unwrap_or_else(env::temp_dir)
        .join("RustWindowsMenu")
}

pub fn config_dir() -> PathBuf {
    data_dir("APPDATA", &windows::Win32::UI::Shell::FOLDERID_RoamingAppData)
}

pub fn local_data_dir() -> PathBuf {
    data_dir("LOCALAPPDATA", &windows::Win32::UI::Shell::FOLDERID_LocalAppData)
}

pub fn is_kill_switch_active() -> bool {
    local_data_dir().join("DISABLE_HOOK").exists()
}

pub fn config_path() -> PathBuf {
    env::var_os("RWM_CONFIG")
        .filter(|value| !value.to_string_lossy().trim().is_empty())
        .map_or_else(|| config_dir().join("config.toml"), PathBuf::from)
}

impl RwmConfig {
    pub fn load_or_default() -> Result<Self, ConfigError> {
        match Self::load_strict() {
            Err(ConfigError::Read { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                Ok(Self::default())
            }
            result => result,
        }
    }

    pub fn load_strict() -> Result<Self, ConfigError> {
        let path = config_path();
        let raw = fs::read_to_string(&path).map_err(|source| ConfigError::Read {
            path: path.clone(),
            source,
        })?;
        toml::from_str::<Self>(&raw)
            .map(Self::validated)
            .map_err(|source| ConfigError::Parse {
                path,
                source: Box::new(source),
            })
    }

    pub fn save(&self) -> Result<(), ConfigError> {
        let path = config_path();
        let raw = toml::to_string_pretty(&self.clone().validated())?;
        write_bytes_atomic(&path, raw.as_bytes())
            .map_err(|source| ConfigError::Write { path, source })
    }

    pub fn validated(mut self) -> Self {
        self.arm_delay_ms = self.arm_delay_ms.min(100);
        self.max_pinned_dlls = self.max_pinned_dlls.min(128);
        self.hover_delay_ms = self.hover_delay_ms.clamp(40, 1000);
        self.host_cache_entries = self.host_cache_entries.clamp(1, 16);
        self.host_cache_ttl_ms = self.host_cache_ttl_ms.clamp(1000, 30000);
        let mut seen = Vec::with_capacity(self.hidden_items.len());
        let mut normalized = Vec::with_capacity(self.hidden_items.len());
        for item in self.hidden_items {
            let trimmed = item.trim();
            if trimmed.is_empty() {
                continue;
            }
            let normalized_item = if let Some((prefix, rest)) = trimmed.split_once(':') {
                if prefix.eq_ignore_ascii_case("verb") {
                    format!("verb:{}", rest.trim())
                } else if prefix.eq_ignore_ascii_case("label") {
                    format!("label:{}", rest.trim())
                } else {
                    trimmed.to_owned()
                }
            } else {
                trimmed.to_owned()
            };
            let lower = normalized_item.to_lowercase();
            if !seen.contains(&lower) {
                seen.push(lower);
                normalized.push(normalized_item);
            }
        }
        self.hidden_items = normalized;
        self
    }

    pub fn detect_legacy_v01_keys(raw: &str) -> bool {
        raw.parse::<toml::Table>().is_ok_and(|table| {
            ["appearance", "behavior", "handlers", "custom_items"]
                .iter()
                .any(|key| table.contains_key(*key))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_and_typo_keys_are_rejected() {
        let legacy = "[appearance]\nshow_icons = true\n[behavior]\nshow_loading_footer = true\n";
        assert!(RwmConfig::detect_legacy_v01_keys(legacy));
        let error = toml::from_str::<RwmConfig>(legacy).unwrap_err();
        assert!(error.message().contains("unknown field"));
        assert!(toml::from_str::<RwmConfig>("arm_dealy_ms = 20").is_err());
        assert!(!RwmConfig::detect_legacy_v01_keys(
            "# [appearance]\nenabled = true"
        ));
    }

    #[test]
    fn defaults_clamps_and_roundtrips() {
        assert_eq!(
            toml::from_str::<RwmConfig>("").unwrap(),
            RwmConfig::default()
        );
        let config = toml::from_str::<RwmConfig>(
            "arm_delay_ms = 250\nmax_pinned_dlls = 500\nno_animation = false\nhover_delay_ms = 10\nhost_cache_entries = 99\nhost_cache_ttl_ms = 999999",
        )
        .unwrap()
        .validated();
        assert_eq!(config.arm_delay_ms, 100);
        assert_eq!(config.max_pinned_dlls, 128);
        assert!(!config.no_animation);
        assert_eq!(config.hover_delay_ms, 40);
        assert_eq!(config.host_cache_entries, 16);
        assert_eq!(config.host_cache_ttl_ms, 30000);
        let raw = toml::to_string_pretty(&config).unwrap();
        assert_eq!(toml::from_str::<RwmConfig>(&raw).unwrap(), config);
    }

    #[test]
    fn hidden_items_normalize_and_show_customize_roundtrips() {
        let config = toml::from_str::<RwmConfig>(
            "show_customize_entry = false\nhidden_items = [\"  verb:Share \", \"\", \"VERB:share\", \"Verb:Delete\", \"verb:delete\", \"label:メモ帳で編集\", \"label:メモ帳で編集\"]",
        )
        .unwrap()
        .validated();
        assert!(!config.show_customize_entry);
        assert_eq!(
            config.hidden_items,
            vec![
                "verb:Share".to_string(),
                "verb:Delete".to_string(),
                "label:メモ帳で編集".to_string(),
            ]
        );
        let raw = toml::to_string_pretty(&config).unwrap();
        assert_eq!(toml::from_str::<RwmConfig>(&raw).unwrap(), config);
    }
}
