use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, LPARAM, WPARAM};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegDeleteTreeW, RegDeleteValueW, RegOpenKeyExW,
    RegQueryInfoKeyW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE,
    KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, REG_VALUE_TYPE,
};
use windows::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};
use windows::Win32::UI::WindowsAndMessaging::{
    SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
};

use crate::config::RwmConfig;

pub const WIN11_MODERN_MENU_CLSID: &str = "{86ca1aa0-34aa-4e8b-a509-50c905bae2a2}";

#[derive(Debug, Error)]
pub enum ClassicError {
    #[error("registry operation failed ({op} on {key}): code {code}")]
    Registry {
        op: &'static str,
        key: String,
        code: u32,
    },
    #[error("I/O error on backup {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("backup JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("failed to restart explorer.exe: {0}")]
    ExplorerRestart(String),
}

#[derive(Debug, Clone)]
pub struct ClassicRegistryScope {
    pub classes_prefix: String,
    pub desktop_prefix: String,
    pub backup_path: PathBuf,
}

impl ClassicRegistryScope {
    pub fn live() -> Self {
        Self {
            classes_prefix: r"Software\Classes".to_string(),
            desktop_prefix: r"Control Panel\Desktop".to_string(),
            backup_path: RwmConfig::local_data_dir().join("classic_backup.json"),
        }
    }

    pub fn isolated(test_root_under_hkcu: &str, backup_path: PathBuf) -> Self {
        let root = test_root_under_hkcu.trim_matches('\\');
        Self {
            classes_prefix: format!(r"{root}\Classes"),
            desktop_prefix: format!(r"{root}\Desktop"),
            backup_path,
        }
    }

    pub fn clsid_key_path(&self) -> String {
        format!(r"{}\CLSID\{WIN11_MODERN_MENU_CLSID}", self.classes_prefix)
    }

    pub fn inproc_key_path(&self) -> String {
        format!(r"{}\InprocServer32", self.clsid_key_path())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegValueBackup {
    pub value_type: u32,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClassicBackup {
    pub had_clsid_key: bool,
    pub had_inproc_key: bool,
    #[serde(default)]
    pub prior_inproc_default: Option<String>,
    #[serde(default)]
    pub prior_menu_show_delay: Option<String>,
    #[serde(default)]
    pub prior_inproc_value: Option<RegValueBackup>,
    #[serde(default)]
    pub prior_menu_show_delay_value: Option<RegValueBackup>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassicStatus {
    pub windows_build: u32,
    pub windows_display_version: String,
    pub is_windows_11: bool,
    pub classic_menu_enabled: bool,
    pub menu_show_delay_ms: Option<String>,
    pub has_backup: bool,
    pub clsid_registry_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassicApplyResult {
    pub changed: bool,
    pub classic_menu_enabled: bool,
    pub menu_show_delay_ms: Option<String>,
    pub explorer_restarted: bool,
    pub message: String,
}

struct RegKeyGuard(HKEY);

impl Drop for RegKeyGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = RegCloseKey(self.0);
            }
        }
    }
}

fn to_wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn reg_open(root: HKEY, subkey: &str, access: u32) -> Result<Option<RegKeyGuard>, ClassicError> {
    let wide = to_wide_null(subkey);
    let mut hkey = HKEY::default();
    let res = unsafe {
        RegOpenKeyExW(
            root,
            PCWSTR(wide.as_ptr()),
            0,
            windows::Win32::System::Registry::REG_SAM_FLAGS(access),
            &mut hkey,
        )
    };
    if res == ERROR_SUCCESS {
        Ok(Some(RegKeyGuard(hkey)))
    } else if res == ERROR_FILE_NOT_FOUND {
        Ok(None)
    } else {
        Err(ClassicError::Registry {
            op: "RegOpenKeyExW",
            key: subkey.to_string(),
            code: res.0,
        })
    }
}

fn reg_create(root: HKEY, subkey: &str) -> Result<RegKeyGuard, ClassicError> {
    let wide = to_wide_null(subkey);
    let mut hkey = HKEY::default();
    let res = unsafe {
        RegCreateKeyExW(
            root,
            PCWSTR(wide.as_ptr()),
            0,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_READ | KEY_WRITE,
            None,
            &mut hkey,
            None,
        )
    };
    if res == ERROR_SUCCESS {
        Ok(RegKeyGuard(hkey))
    } else {
        Err(ClassicError::Registry {
            op: "RegCreateKeyExW",
            key: subkey.to_string(),
            code: res.0,
        })
    }
}

fn reg_read_string(root: HKEY, subkey: &str, value_name: Option<&str>) -> Result<Option<String>, ClassicError> {
    let Some(guard) = reg_open(root, subkey, KEY_READ.0)? else {
        return Ok(None);
    };
    let name_wide = value_name.map(to_wide_null);
    let name_ptr = name_wide
        .as_ref()
        .map_or(PCWSTR::null(), |v| PCWSTR(v.as_ptr()));
    let mut val_type = REG_VALUE_TYPE::default();
    let mut byte_len: u32 = 0;
    let res = unsafe {
        RegQueryValueExW(
            guard.0,
            name_ptr,
            None,
            Some(&mut val_type),
            None,
            Some(&mut byte_len),
        )
    };
    if res == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if res != ERROR_SUCCESS {
        return Err(ClassicError::Registry {
            op: "RegQueryValueExW(size)",
            key: subkey.to_string(),
            code: res.0,
        });
    }
    if byte_len == 0 {
        return Ok(Some(String::new()));
    }

    let mut buf = vec![0u8; byte_len as usize];
    let res2 = unsafe {
        RegQueryValueExW(
            guard.0,
            name_ptr,
            None,
            Some(&mut val_type),
            Some(buf.as_mut_ptr()),
            Some(&mut byte_len),
        )
    };
    if res2 != ERROR_SUCCESS {
        return Err(ClassicError::Registry {
            op: "RegQueryValueExW(data)",
            key: subkey.to_string(),
            code: res2.0,
        });
    }

    let u16_slice = unsafe {
        std::slice::from_raw_parts(buf.as_ptr() as *const u16, (byte_len as usize) / 2)
    };
    let end = u16_slice
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(u16_slice.len());
    Ok(Some(String::from_utf16_lossy(&u16_slice[..end])))
}

fn reg_read_raw(
    root: HKEY,
    subkey: &str,
    value_name: Option<&str>,
) -> Result<Option<RegValueBackup>, ClassicError> {
    let Some(guard) = reg_open(root, subkey, KEY_READ.0)? else {
        return Ok(None);
    };
    let name_wide = value_name.map(to_wide_null);
    let name_ptr = name_wide
        .as_ref()
        .map_or(PCWSTR::null(), |v| PCWSTR(v.as_ptr()));
    let mut val_type = REG_VALUE_TYPE::default();
    let mut byte_len: u32 = 0;
    let res = unsafe {
        RegQueryValueExW(
            guard.0,
            name_ptr,
            None,
            Some(&mut val_type),
            None,
            Some(&mut byte_len),
        )
    };
    if res == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if res != ERROR_SUCCESS {
        return Err(ClassicError::Registry {
            op: "RegQueryValueExW(size)",
            key: subkey.to_string(),
            code: res.0,
        });
    }
    let mut buf = vec![0u8; byte_len as usize];
    if byte_len > 0 {
        let res2 = unsafe {
            RegQueryValueExW(
                guard.0,
                name_ptr,
                None,
                Some(&mut val_type),
                Some(buf.as_mut_ptr()),
                Some(&mut byte_len),
            )
        };
        if res2 != ERROR_SUCCESS {
            return Err(ClassicError::Registry {
                op: "RegQueryValueExW(data)",
                key: subkey.to_string(),
                code: res2.0,
            });
        }
        buf.truncate(byte_len as usize);
    }
    Ok(Some(RegValueBackup {
        value_type: val_type.0,
        data: buf,
    }))
}

fn reg_write_raw(
    root: HKEY,
    subkey: &str,
    value_name: Option<&str>,
    value: &RegValueBackup,
) -> Result<(), ClassicError> {
    let guard = reg_create(root, subkey)?;
    let name_wide = value_name.map(to_wide_null);
    let name_ptr = name_wide
        .as_ref()
        .map_or(PCWSTR::null(), |v| PCWSTR(v.as_ptr()));
    let data = (!value.data.is_empty()).then_some(value.data.as_slice());
    let res = unsafe {
        RegSetValueExW(
            guard.0,
            name_ptr,
            0,
            REG_VALUE_TYPE(value.value_type),
            data,
        )
    };
    if res != ERROR_SUCCESS {
        return Err(ClassicError::Registry {
            op: "RegSetValueExW",
            key: subkey.to_string(),
            code: res.0,
        });
    }
    Ok(())
}

fn reg_delete_value(
    root: HKEY,
    subkey: &str,
    value_name: Option<&str>,
) -> Result<bool, ClassicError> {
    let Some(guard) = reg_open(root, subkey, (KEY_READ | KEY_WRITE).0)? else {
        return Ok(false);
    };
    let name_wide = value_name.map(to_wide_null);
    let name_ptr = name_wide
        .as_ref()
        .map_or(PCWSTR::null(), |v| PCWSTR(v.as_ptr()));
    let res = unsafe { RegDeleteValueW(guard.0, name_ptr) };
    if res == ERROR_SUCCESS {
        Ok(true)
    } else if res == ERROR_FILE_NOT_FOUND {
        Ok(false)
    } else {
        Err(ClassicError::Registry {
            op: "RegDeleteValueW",
            key: subkey.to_string(),
            code: res.0,
        })
    }
}

fn reg_delete_if_empty(root: HKEY, subkey: &str) -> Result<bool, ClassicError> {
    let Some(guard) = reg_open(root, subkey, KEY_READ.0)? else {
        return Ok(false);
    };
    let mut subkeys: u32 = 0;
    let mut values: u32 = 0;
    let qres = unsafe {
        RegQueryInfoKeyW(
            guard.0,
            PWSTR::null(),
            None,
            None,
            Some(&mut subkeys),
            None,
            None,
            Some(&mut values),
            None,
            None,
            None,
            None,
        )
    };
    if qres != ERROR_SUCCESS {
        return Err(ClassicError::Registry {
            op: "RegQueryInfoKeyW",
            key: subkey.to_string(),
            code: qres.0,
        });
    }
    drop(guard);
    if subkeys != 0 || values != 0 {
        return Ok(false);
    }
    let wide = to_wide_null(subkey);
    let dres = unsafe { RegDeleteKeyW(root, PCWSTR(wide.as_ptr())) };
    if dres == ERROR_SUCCESS {
        Ok(true)
    } else if dres == ERROR_FILE_NOT_FOUND {
        Ok(false)
    } else {
        Err(ClassicError::Registry {
            op: "RegDeleteKeyW",
            key: subkey.to_string(),
            code: dres.0,
        })
    }
}

fn reg_write_string(
    root: HKEY,
    subkey: &str,
    value_name: Option<&str>,
    value: &str,
) -> Result<(), ClassicError> {
    let guard = reg_create(root, subkey)?;
    let name_wide = value_name.map(to_wide_null);
    let name_ptr = name_wide
        .as_ref()
        .map_or(PCWSTR::null(), |v| PCWSTR(v.as_ptr()));
    let data_wide = to_wide_null(value);
    let data_bytes = unsafe {
        std::slice::from_raw_parts(data_wide.as_ptr() as *const u8, data_wide.len() * 2)
    };
    let res = unsafe { RegSetValueExW(guard.0, name_ptr, 0, REG_SZ, Some(data_bytes)) };
    if res != ERROR_SUCCESS {
        return Err(ClassicError::Registry {
            op: "RegSetValueExW",
            key: subkey.to_string(),
            code: res.0,
        });
    }
    Ok(())
}

fn reg_delete_tree(root: HKEY, subkey: &str) -> Result<bool, ClassicError> {
    let wide = to_wide_null(subkey);
    let res = unsafe { RegDeleteTreeW(root, PCWSTR(wide.as_ptr())) };
    if res == ERROR_SUCCESS {
        Ok(true)
    } else if res == ERROR_FILE_NOT_FOUND {
        Ok(false)
    } else {
        Err(ClassicError::Registry {
            op: "RegDeleteTreeW",
            key: subkey.to_string(),
            code: res.0,
        })
    }
}

pub fn read_windows_version() -> (u32, String) {
    let cv_key = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
    let build = reg_read_string(HKEY_LOCAL_MACHINE, cv_key, Some("CurrentBuildNumber"))
        .ok()
        .flatten()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0);
    let display = reg_read_string(HKEY_LOCAL_MACHINE, cv_key, Some("DisplayVersion"))
        .ok()
        .flatten()
        .unwrap_or_else(|| "Unknown".to_string());
    (build, display)
}

pub fn status(scope: &ClassicRegistryScope) -> Result<ClassicStatus, ClassicError> {
    let (windows_build, windows_display_version) = read_windows_version();
    let inproc_val = reg_read_string(HKEY_CURRENT_USER, &scope.inproc_key_path(), None)?;
    let classic_menu_enabled = matches!(inproc_val.as_deref(), Some(""));
    let menu_show_delay_ms =
        reg_read_string(HKEY_CURRENT_USER, &scope.desktop_prefix, Some("MenuShowDelay"))?;
    let has_backup = scope.backup_path.exists();

    Ok(ClassicStatus {
        windows_build,
        windows_display_version,
        is_windows_11: windows_build >= 22000,
        classic_menu_enabled,
        menu_show_delay_ms,
        has_backup,
        clsid_registry_path: format!(r"HKCU\{}", scope.inproc_key_path()),
    })
}

fn save_backup_if_absent(scope: &ClassicRegistryScope) -> Result<(), ClassicError> {
    if scope.backup_path.exists() {
        return Ok(());
    }
    let had_clsid_key = reg_open(HKEY_CURRENT_USER, &scope.clsid_key_path(), KEY_READ.0)?.is_some();
    let had_inproc_key =
        reg_open(HKEY_CURRENT_USER, &scope.inproc_key_path(), KEY_READ.0)?.is_some();
    let prior_inproc_default =
        reg_read_string(HKEY_CURRENT_USER, &scope.inproc_key_path(), None)?;
    let prior_menu_show_delay =
        reg_read_string(HKEY_CURRENT_USER, &scope.desktop_prefix, Some("MenuShowDelay"))?;
    let prior_inproc_value =
        reg_read_raw(HKEY_CURRENT_USER, &scope.inproc_key_path(), None)?;
    let prior_menu_show_delay_value =
        reg_read_raw(HKEY_CURRENT_USER, &scope.desktop_prefix, Some("MenuShowDelay"))?;

    let backup = ClassicBackup {
        had_clsid_key,
        had_inproc_key,
        prior_inproc_default,
        prior_menu_show_delay,
        prior_inproc_value,
        prior_menu_show_delay_value,
    };

    if let Some(parent) = scope.backup_path.parent() {
        fs::create_dir_all(parent).map_err(|source| ClassicError::Io {
            path: scope.backup_path.clone(),
            source,
        })?;
    }
    let tmp_path = scope.backup_path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(&backup)?;
    fs::write(&tmp_path, json).map_err(|source| ClassicError::Io {
        path: tmp_path.clone(),
        source,
    })?;
    fs::rename(&tmp_path, &scope.backup_path).map_err(|source| ClassicError::Io {
        path: scope.backup_path.clone(),
        source,
    })?;
    Ok(())
}

fn broadcast_shell_and_settings_change() {
    unsafe {
        SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None);
        let param = to_wide_null("WindowMetrics");
        let _ = SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            WPARAM(0),
            LPARAM(param.as_ptr() as isize),
            SMTO_ABORTIFHUNG,
            250,
            None,
        );
    }
}

pub fn restart_explorer() -> Result<(), ClassicError> {
    let status = Command::new("taskkill.exe")
        .args(["/F", "/IM", "explorer.exe"])
        .status()
        .map_err(|e| ClassicError::ExplorerRestart(e.to_string()))?;
    if !status.success() {
        return Err(ClassicError::ExplorerRestart(format!(
            "taskkill exited with {status}"
        )));
    }
    std::thread::sleep(std::time::Duration::from_millis(250));
    Command::new("explorer.exe")
        .spawn()
        .map_err(|e| ClassicError::ExplorerRestart(e.to_string()))?;
    Ok(())
}

pub fn enable(
    scope: &ClassicRegistryScope,
    optimize_delay: bool,
    restart_shell: bool,
    broadcast: bool,
) -> Result<ClassicApplyResult, ClassicError> {
    save_backup_if_absent(scope)?;

    let before_inproc = reg_read_string(HKEY_CURRENT_USER, &scope.inproc_key_path(), None)?;
    let before_delay =
        reg_read_string(HKEY_CURRENT_USER, &scope.desktop_prefix, Some("MenuShowDelay"))?;

    let mut changed = false;
    if before_inproc.as_deref() != Some("") {
        reg_write_string(HKEY_CURRENT_USER, &scope.inproc_key_path(), None, "")?;
        changed = true;
    }

    let final_delay = if optimize_delay {
        if before_delay.as_deref() != Some("0") {
            reg_write_string(
                HKEY_CURRENT_USER,
                &scope.desktop_prefix,
                Some("MenuShowDelay"),
                "0",
            )?;
            changed = true;
        }
        Some("0".to_string())
    } else {
        before_delay
    };

    if changed && broadcast {
        broadcast_shell_and_settings_change();
    }

    let mut explorer_restarted = false;
    if restart_shell {
        restart_explorer()?;
        explorer_restarted = true;
    }

    Ok(ClassicApplyResult {
        changed,
        classic_menu_enabled: true,
        menu_show_delay_ms: final_delay,
        explorer_restarted,
        message: if changed {
            "Classic context menu enabled cleanly.".to_string()
        } else {
            "Classic context menu was already enabled.".to_string()
        },
    })
}

pub fn disable(
    scope: &ClassicRegistryScope,
    restart_shell: bool,
    broadcast: bool,
) -> Result<ClassicApplyResult, ClassicError> {
    let mut changed = false;

    let backup: Option<ClassicBackup> = if scope.backup_path.exists() {
        let raw = fs::read_to_string(&scope.backup_path).map_err(|source| ClassicError::Io {
            path: scope.backup_path.clone(),
            source,
        })?;
        Some(serde_json::from_str(&raw)?)
    } else {
        None
    };

    if let Some(ref b) = backup {
        if let Some(ref prev_raw) = b.prior_inproc_value {
            let current_raw = reg_read_raw(HKEY_CURRENT_USER, &scope.inproc_key_path(), None)?;
            if current_raw.as_ref() != Some(prev_raw) {
                reg_write_raw(HKEY_CURRENT_USER, &scope.inproc_key_path(), None, prev_raw)?;
                changed = true;
            }
        } else if let Some(ref prev) = b.prior_inproc_default {
            reg_write_string(HKEY_CURRENT_USER, &scope.inproc_key_path(), None, prev)?;
            changed = true;
        } else if reg_delete_value(HKEY_CURRENT_USER, &scope.inproc_key_path(), None)? {
            changed = true;
        }

        if !b.had_inproc_key
            && reg_delete_if_empty(HKEY_CURRENT_USER, &scope.inproc_key_path())?
        {
            changed = true;
        }
        if !b.had_clsid_key
            && reg_delete_if_empty(HKEY_CURRENT_USER, &scope.clsid_key_path())?
        {
            changed = true;
        }

        if let Some(ref prev_delay_raw) = b.prior_menu_show_delay_value {
            let current_raw =
                reg_read_raw(HKEY_CURRENT_USER, &scope.desktop_prefix, Some("MenuShowDelay"))?;
            if current_raw.as_ref() != Some(prev_delay_raw) {
                reg_write_raw(
                    HKEY_CURRENT_USER,
                    &scope.desktop_prefix,
                    Some("MenuShowDelay"),
                    prev_delay_raw,
                )?;
                changed = true;
            }
        } else if let Some(ref prev_delay) = b.prior_menu_show_delay {
            let current_delay =
                reg_read_string(HKEY_CURRENT_USER, &scope.desktop_prefix, Some("MenuShowDelay"))?;
            if current_delay.as_deref() != Some(prev_delay.as_str()) {
                reg_write_string(
                    HKEY_CURRENT_USER,
                    &scope.desktop_prefix,
                    Some("MenuShowDelay"),
                    prev_delay,
                )?;
                changed = true;
            }
        } else if reg_delete_value(HKEY_CURRENT_USER, &scope.desktop_prefix, Some("MenuShowDelay"))?
        {
            changed = true;
        }
        let _ = fs::remove_file(&scope.backup_path);
    } else {
        if reg_delete_value(HKEY_CURRENT_USER, &scope.inproc_key_path(), None)? {
            changed = true;
        }
        if reg_delete_if_empty(HKEY_CURRENT_USER, &scope.inproc_key_path())? {
            changed = true;
        }
        if reg_delete_if_empty(HKEY_CURRENT_USER, &scope.clsid_key_path())? {
            changed = true;
        }
    }

    if changed && broadcast {
        broadcast_shell_and_settings_change();
    }

    let mut explorer_restarted = false;
    if restart_shell {
        restart_explorer()?;
        explorer_restarted = true;
    }

    let delay_now =
        reg_read_string(HKEY_CURRENT_USER, &scope.desktop_prefix, Some("MenuShowDelay"))?;
    let inproc_now = reg_read_string(HKEY_CURRENT_USER, &scope.inproc_key_path(), None)?;
    let enabled_now = matches!(inproc_now.as_deref(), Some(""));
    let message = match (changed, enabled_now) {
        (true, true) => "Restored previous classic context menu setting from backup.".to_string(),
        (true, false) => "Restored Windows 11 default context menu state.".to_string(),
        (false, true) => "Classic context menu setting was unchanged.".to_string(),
        (false, false) => "Classic context menu override was already absent.".to_string(),
    };

    Ok(ClassicApplyResult {
        changed,
        classic_menu_enabled: enabled_now,
        menu_show_delay_ms: delay_now,
        explorer_restarted,
        message,
    })
}

/// Remove an isolated test root from HKCU (safe cleanup helper for tests).
pub fn cleanup_isolated_root(test_root_under_hkcu: &str) -> Result<(), ClassicError> {
    let _ = reg_delete_tree(HKEY_CURRENT_USER, test_root_under_hkcu.trim_matches('\\'))?;
    Ok(())
}

pub fn _unused_pwstr_check(_p: PWSTR) -> &'static Path {
    Path::new(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolated_classic_enable_is_idempotent_and_restores_cleanly() {
        let test_root = r"Software\RustWindowsMenu_Test_Classic";
        let _ = cleanup_isolated_root(test_root);

        let temp_dir = std::env::temp_dir().join("rwm_classic_test");
        let _ = fs::create_dir_all(&temp_dir);
        let backup_path = temp_dir.join("classic_backup.json");
        let _ = fs::remove_file(&backup_path);

        let scope = ClassicRegistryScope::isolated(test_root, backup_path.clone());
        reg_write_string(
            HKEY_CURRENT_USER,
            &scope.desktop_prefix,
            Some("MenuShowDelay"),
            "400",
        )
        .expect("seed delay");

        let s0 = status(&scope).expect("initial status");
        assert!(!s0.classic_menu_enabled);
        assert_eq!(s0.menu_show_delay_ms.as_deref(), Some("400"));

        let r1 = enable(&scope, true, false, false).expect("first enable");
        assert!(r1.changed);
        assert!(r1.classic_menu_enabled);
        assert_eq!(r1.menu_show_delay_ms.as_deref(), Some("0"));

        // Second enable must be idempotent and keep backup intact.
        let r2 = enable(&scope, true, false, false).expect("second enable");
        assert!(!r2.changed);
        assert!(r2.classic_menu_enabled);

        let s1 = status(&scope).expect("enabled status");
        assert!(s1.classic_menu_enabled);
        assert!(s1.has_backup);

        let d1 = disable(&scope, false, false).expect("disable");
        assert!(d1.changed);
        assert!(!d1.classic_menu_enabled);
        assert_eq!(d1.menu_show_delay_ms.as_deref(), Some("400"));

        // Second disable must also be idempotent.
        let d2 = disable(&scope, false, false).expect("second disable");
        assert!(!d2.changed);

        cleanup_isolated_root(test_root).expect("cleanup");
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn isolated_restore_preserves_prior_classic_menu_state() {
        let test_root = r"Software\RustWindowsMenu_Test_PriorClassic";
        let _ = cleanup_isolated_root(test_root);

        let temp_dir = std::env::temp_dir().join("rwm_prior_classic_test");
        let _ = fs::create_dir_all(&temp_dir);
        let backup_path = temp_dir.join("classic_backup.json");
        let _ = fs::remove_file(&backup_path);

        let scope = ClassicRegistryScope::isolated(test_root, backup_path);
        reg_write_string(HKEY_CURRENT_USER, &scope.inproc_key_path(), None, "")
            .expect("seed existing classic override");

        let e = enable(&scope, true, false, false).expect("enable over existing classic");
        assert!(e.classic_menu_enabled);

        let d = disable(&scope, false, false).expect("restore prior classic");
        assert!(d.changed);
        assert!(d.classic_menu_enabled);
        assert_eq!(d.menu_show_delay_ms, None);

        cleanup_isolated_root(test_root).expect("cleanup");
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn isolated_disable_preserves_unrelated_keys_deletes_absent_delay_and_restores_value_types() {
        let test_root = r"Software\RustWindowsMenu_Test_NonDestructive";
        let _ = cleanup_isolated_root(test_root);

        let temp_dir = std::env::temp_dir().join("rwm_nondestructive_test");
        let _ = fs::create_dir_all(&temp_dir);
        let backup_path = temp_dir.join("classic_backup.json");
        let _ = fs::remove_file(&backup_path);

        let scope = ClassicRegistryScope::isolated(test_root, backup_path);
        // Seed an empty InprocServer32 key with an unrelated value and REG_EXPAND_SZ test value.
        let custom_val = RegValueBackup {
            value_type: 2, // REG_EXPAND_SZ
            data: to_wide_null("%SystemRoot%\\dummy.dll")
                .iter()
                .flat_map(|u| u.to_le_bytes())
                .collect(),
        };
        reg_write_raw(
            HKEY_CURRENT_USER,
            &scope.inproc_key_path(),
            Some("ThreadingModel"),
            &custom_val,
        )
        .expect("seed unrelated ThreadingModel");
        // Seed an unrelated subkey under the CLSID key too.
        let extra_subkey = format!(r"{}\ExtraSubkey", scope.clsid_key_path());
        reg_write_string(HKEY_CURRENT_USER, &extra_subkey, Some("KeepMe"), "yes")
            .expect("seed unrelated subkey");

        // MenuShowDelay is absent initially; enable() writes "0" and backup records absence.
        let enabled = enable(&scope, true, false, false).expect("enable");
        assert!(enabled.changed);
        assert_eq!(enabled.menu_show_delay_ms.as_deref(), Some("0"));

        // Add another unrelated value on CLSID while enabled.
        reg_write_string(
            HKEY_CURRENT_USER,
            &scope.clsid_key_path(),
            Some("AppNote"),
            "keep",
        )
        .expect("seed value under clsid");

        // Disable must delete the empty default value and MenuShowDelay, while leaving all
        // unrelated values and subkeys intact!
        let disabled = disable(&scope, false, false).expect("disable");
        assert!(disabled.changed);
        assert!(!disabled.classic_menu_enabled);
        assert_eq!(disabled.menu_show_delay_ms, None);
        assert_eq!(
            reg_read_raw(
                HKEY_CURRENT_USER,
                &scope.inproc_key_path(),
                Some("ThreadingModel")
            )
            .expect("read ThreadingModel"),
            Some(custom_val)
        );
        assert_eq!(
            reg_read_string(HKEY_CURRENT_USER, &extra_subkey, Some("KeepMe"))
                .expect("read extra subkey"),
            Some("yes".to_string())
        );
        assert_eq!(
            reg_read_string(HKEY_CURRENT_USER, &scope.clsid_key_path(), Some("AppNote"))
                .expect("read AppNote"),
            Some("keep".to_string())
        );

        cleanup_isolated_root(test_root).expect("cleanup");
        let _ = fs::remove_dir_all(&temp_dir);
    }
}
