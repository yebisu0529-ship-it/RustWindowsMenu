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
            "arm_delay_ms = 250\nmax_pinned_dlls = 500\nno_animation = false",
        )
        .unwrap()
        .validated();
        assert_eq!(config.arm_delay_ms, 100);
        assert_eq!(config.max_pinned_dlls, 128);
        assert!(!config.no_animation);
        let raw = toml::to_string_pretty(&config).unwrap();
        assert_eq!(toml::from_str::<RwmConfig>(&raw).unwrap(), config);
    }
}
