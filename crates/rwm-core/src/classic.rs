use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use windows::Win32::Foundation::{
    ERROR_INVALID_PARAMETER, ERROR_NO_MORE_FILES, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, REG_SZ};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, TerminateProcess,
    WaitForSingleObject, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
};
use windows::Win32::UI::WindowsAndMessaging::GetShellWindow;
use windows_core::PWSTR;

use crate::config::local_data_dir;
use crate::reg_util::{
    acquire_named_mutex, create_key_write, delete_key_if_empty, delete_value_ignore_missing,
    open_key_read, open_key_write, read_raw_value, read_string_value, write_bytes_atomic,
    write_raw_value, write_string_value, HandleGuard, RawRegValue,
};

pub const WIN11_MODERN_MENU_CLSID: &str = "{86ca1aa0-34aa-4e8b-a509-50c905bae2a2}";
const CLASSES: &str = r"Software\Classes";
const LOCK: &str = r"Local\RustWindowsMenuClassicLock";

#[derive(Debug, Error)]
pub enum ClassicError {
    #[error("Windows operation failed: {0}")]
    Windows(#[from] windows_core::Error),
    #[error("classic menu I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("invalid classic menu backup: {0}")]
    Json(#[from] serde_json::Error),
    #[error("the classic menu override has no backup; use --force to remove its default value")]
    BackupNotFoundNeedsForce,
    #[error("classic menu backup {path} is corrupt (moved to {backup}); use --force to remove the override")]
    BackupCorrupt { path: PathBuf, backup: PathBuf },
    #[error("could not restart Explorer: {0}")]
    ExplorerRestart(String),
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
}

#[derive(Debug, Serialize, Deserialize)]
struct ClassicBackup {
    had_clsid_key: bool,
    had_inproc_key: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prior_inproc_default: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prior_menu_show_delay: Option<String>,
    #[serde(default)]
    prior_inproc_value: Option<RawRegValue>,
    #[serde(default)]
    prior_menu_show_delay_value: Option<RawRegValue>,
}

struct OverrideKeys {
    clsid: String,
    inproc: String,
}

impl OverrideKeys {
    fn new(subkey: &str) -> Self {
        let clsid = format!(r"{subkey}\CLSID\{WIN11_MODERN_MENU_CLSID}");
        let inproc = format!(r"{clsid}\InprocServer32");
        Self { clsid, inproc }
    }
}

fn backup_path() -> PathBuf {
    local_data_dir().join("classic_backup.json")
}

fn menu_show_delay() -> io::Result<Option<String>> {
    open_key_read(HKEY_CURRENT_USER, r"Control Panel\Desktop")?
        .map(|key| read_string_value(&key, Some("MenuShowDelay")))
        .transpose()
        .map(Option::flatten)
}

fn is_enabled(value: Option<&RawRegValue>) -> bool {
    value.is_some_and(|value| {
        value.value_type == REG_SZ.0 && value.as_string().as_deref() == Some("")
    })
}

pub(crate) fn windows_version() -> io::Result<(u32, String)> {
    let Some(version) = open_key_read(
        HKEY_LOCAL_MACHINE,
        r"Software\Microsoft\Windows NT\CurrentVersion",
    )?
    else {
        return Ok((0, String::new()));
    };
    let build = read_string_value(&version, Some("CurrentBuildNumber"))?
        .and_then(|value| value.parse().ok())
        .unwrap_or_default();
    let display_version = read_string_value(&version, Some("DisplayVersion"))?.unwrap_or_default();
    Ok((build, display_version))
}

pub fn status() -> Result<ClassicStatus, ClassicError> {
    status_in_subkey(CLASSES, &backup_path())
}

pub fn status_in_subkey(subkey: &str, backup_path: &Path) -> Result<ClassicStatus, ClassicError> {
    let _lock = acquire_named_mutex(LOCK)?;
    let keys = OverrideKeys::new(subkey);
    let inproc = open_key_read(HKEY_CURRENT_USER, &keys.inproc)?;
    let current = inproc
        .as_ref()
        .map(|key| read_raw_value(key, None))
        .transpose()?
        .flatten();
    let (windows_build, windows_display_version) = windows_version()?;
    Ok(ClassicStatus {
        windows_build,
        windows_display_version,
        is_windows_11: windows_build >= 22000,
        classic_menu_enabled: is_enabled(current.as_ref()),
        menu_show_delay_ms: menu_show_delay()?,
        has_backup: backup_path.try_exists()?,
        clsid_registry_path: format!(r"HKCU\{}", keys.clsid),
    })
}

pub fn enable() -> Result<ClassicApplyResult, ClassicError> {
    enable_in_subkey(CLASSES, &backup_path())
}

pub fn enable_in_subkey(
    subkey: &str,
    backup_path: &Path,
) -> Result<ClassicApplyResult, ClassicError> {
    let _lock = acquire_named_mutex(LOCK)?;
    let keys = OverrideKeys::new(subkey);
    let inproc = open_key_read(HKEY_CURRENT_USER, &keys.inproc)?;
    let current = inproc
        .as_ref()
        .map(|key| read_raw_value(key, None))
        .transpose()?
        .flatten();
    let changed = !is_enabled(current.as_ref());
    if changed {
        if !backup_path.try_exists()? {
            let backup = ClassicBackup {
                had_clsid_key: open_key_read(HKEY_CURRENT_USER, &keys.clsid)?.is_some(),
                had_inproc_key: inproc.is_some(),
                prior_inproc_default: None,
                prior_menu_show_delay: None,
                prior_inproc_value: current,
                prior_menu_show_delay_value: None,
            };
            write_bytes_atomic(backup_path, &serde_json::to_vec_pretty(&backup)?)?;
        } else {
            let _: ClassicBackup = serde_json::from_slice(&fs::read(backup_path)?)?;
        }
        let key = create_key_write(HKEY_CURRENT_USER, &keys.inproc)?;
        write_string_value(&key, None, "")?;
    }
    Ok(ClassicApplyResult {
        changed,
        classic_menu_enabled: true,
        menu_show_delay_ms: menu_show_delay()?,
    })
}

fn move_corrupt_backup(backup_path: &Path) -> io::Result<PathBuf> {
    let mut corrupt = backup_path.as_os_str().to_os_string();
    corrupt.push(".corrupt");
    let corrupt_path = PathBuf::from(corrupt);
    let _ = fs::remove_file(&corrupt_path);
    fs::rename(backup_path, &corrupt_path)?;
    Ok(corrupt_path)
}

pub fn disable(force: bool) -> Result<ClassicApplyResult, ClassicError> {
    disable_in_subkeys(CLASSES, r"Control Panel\Desktop", &backup_path(), force)
}

pub fn disable_in_subkey(
    subkey: &str,
    backup_path: &Path,
    force: bool,
) -> Result<ClassicApplyResult, ClassicError> {
    disable_in_subkeys(subkey, r"Control Panel\Desktop", backup_path, force)
}

pub fn disable_in_subkeys(
    subkey: &str,
    desktop_subkey: &str,
    backup_path: &Path,
    force: bool,
) -> Result<ClassicApplyResult, ClassicError> {
    let _lock = acquire_named_mutex(LOCK)?;
    let keys = OverrideKeys::new(subkey);
    let backup: Option<ClassicBackup> = match fs::read(backup_path) {
        Ok(data) => match serde_json::from_slice(&data) {
            Ok(parsed) => Some(parsed),
            Err(_) => {
                let moved = move_corrupt_backup(backup_path)?;
                if !force {
                    return Err(ClassicError::BackupCorrupt {
                        path: backup_path.to_path_buf(),
                        backup: moved,
                    });
                }
                None
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let inproc = open_key_read(HKEY_CURRENT_USER, &keys.inproc)?;
    if backup.is_none() && !force && inproc.is_some() {
        return Err(ClassicError::BackupNotFoundNeedsForce);
    }
    let mut changed = false;
    let mut classic_menu_enabled = false;
    let current = inproc
        .as_ref()
        .map(|key| read_raw_value(key, None))
        .transpose()?
        .flatten();
    if let Some(previous) = backup.as_ref().and_then(|b| {
        b.prior_inproc_value
            .clone()
            .or_else(|| b.prior_inproc_default.as_deref().map(RawRegValue::string))
    }) {
        if (force || is_enabled(current.as_ref())) && current.as_ref() != Some(&previous) {
            let key = create_key_write(HKEY_CURRENT_USER, &keys.inproc)?;
            write_raw_value(&key, None, &previous)?;
            changed = true;
        }
        classic_menu_enabled = is_enabled(Some(&previous));
    } else if backup.is_some() || force {
        if force || is_enabled(current.as_ref()) {
            if let Some(key) = open_key_write(HKEY_CURRENT_USER, &keys.inproc)? {
                changed = delete_value_ignore_missing(&key, None)?;
            }
        }
        drop(inproc);
        if backup.as_ref().is_none_or(|b| !b.had_inproc_key) {
            changed |= delete_key_if_empty(HKEY_CURRENT_USER, &keys.inproc)?;
        }
        if backup.as_ref().is_none_or(|b| !b.had_clsid_key) {
            changed |= delete_key_if_empty(HKEY_CURRENT_USER, &keys.clsid)?;
        }
    }
    if let Some(prior_delay) = backup.as_ref().and_then(|b| {
        b.prior_menu_show_delay_value.clone().or_else(|| {
            b.prior_menu_show_delay
                .as_deref()
                .map(RawRegValue::string)
        })
    }) {
        let current_delay = open_key_read(HKEY_CURRENT_USER, desktop_subkey)?
            .map(|key| read_raw_value(&key, Some("MenuShowDelay")))
            .transpose()?
            .flatten();
        if current_delay.as_ref() != Some(&prior_delay) {
            let desktop = create_key_write(HKEY_CURRENT_USER, desktop_subkey)?;
            write_raw_value(&desktop, Some("MenuShowDelay"), &prior_delay)?;
            changed = true;
        }
    }
    if backup.is_some() {
        fs::remove_file(backup_path)?;
    }
    Ok(ClassicApplyResult {
        changed,
        classic_menu_enabled,
        menu_show_delay_ms: menu_show_delay()?,
    })
}

fn explorer_path() -> Result<PathBuf, ClassicError> {
    let mut path = vec![0; 32768];
    let len = unsafe { GetSystemWindowsDirectoryW(Some(&mut path)) } as usize;
    if len == 0 || len >= path.len() {
        return Err(windows_core::Error::from_win32().into());
    }
    Ok(PathBuf::from(OsString::from_wide(&path[..len])).join("explorer.exe"))
}

fn explorer_processes(session: u32, expected: &Path) -> Result<Vec<HandleGuard>, ClassicError> {
    let snapshot = HandleGuard(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)? });
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut next = unsafe { Process32FirstW(snapshot.0, &mut entry) };
    let mut processes = Vec::new();
    while next.is_ok() {
        let name_len = entry
            .szExeFile
            .iter()
            .position(|&ch| ch == 0)
            .unwrap_or(entry.szExeFile.len());
        if String::from_utf16_lossy(&entry.szExeFile[..name_len])
            .eq_ignore_ascii_case("explorer.exe")
        {
            let pid = entry.th32ProcessID;
            let mut candidate_session = 0;
            if unsafe { ProcessIdToSessionId(pid, &mut candidate_session) }.is_ok()
                && candidate_session == session
            {
                let access =
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE;
                match unsafe { OpenProcess(access, false, pid) } {
                    Ok(handle) => {
                        let handle = HandleGuard(handle);
                        let mut buffer = vec![0; 32768];
                        let mut size = buffer.len() as u32;
                        unsafe {
                            QueryFullProcessImageNameW(
                                handle.0,
                                PROCESS_NAME_FORMAT(0),
                                PWSTR(buffer.as_mut_ptr()),
                                &mut size,
                            )?
                        };
                        let actual = OsString::from_wide(&buffer[..size as usize]);
                        if actual.eq_ignore_ascii_case(expected.as_os_str())
                            && unsafe { ProcessIdToSessionId(pid, &mut candidate_session) }.is_ok()
                            && candidate_session == session
                        {
                            processes.push(handle);
                        }
                    }
                    Err(error) if error.code() == ERROR_INVALID_PARAMETER.to_hresult() => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
        next = unsafe { Process32NextW(snapshot.0, &mut entry) };
    }
    if let Err(error) = next {
        if error.code() != ERROR_NO_MORE_FILES.to_hresult() {
            return Err(error.into());
        }
    }
    Ok(processes)
}

fn spawn_explorer(path: &Path) -> io::Result<std::process::Child> {
    let mut cmd = Command::new(path);
    if let Some(dir) = path.parent() {
        cmd.current_dir(dir);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

pub fn restart_explorer() -> Result<(), ClassicError> {
    let _lock = acquire_named_mutex(LOCK)?;
    let mut session = 0;
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session)? };
    let path = explorer_path()?;
    let processes = explorer_processes(session, &path)?;
    for process in &processes {
        if unsafe { WaitForSingleObject(process.0, 0) } != WAIT_OBJECT_0 {
            if let Err(error) = unsafe { TerminateProcess(process.0, 0) } {
                if unsafe { WaitForSingleObject(process.0, 0) } != WAIT_OBJECT_0 {
                    return Err(error.into());
                }
            }
        }
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    for process in processes {
        let ms = deadline
            .saturating_duration_since(Instant::now())
            .as_millis() as u32;
        match unsafe { WaitForSingleObject(process.0, ms) } {
            WAIT_OBJECT_0 => {}
            WAIT_TIMEOUT => {
                let _ = spawn_explorer(&path);
                return Err(ClassicError::ExplorerRestart(
                    "Explorer did not exit in time".into(),
                ));
            }
            _ => {
                let _ = spawn_explorer(&path);
                return Err(windows_core::Error::from_win32().into());
            }
        }
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if !unsafe { GetShellWindow() }.is_invalid() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    spawn_explorer(&path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reg_util::{write_expand_string_value, Scratch};

    #[test]
    fn disable_restores_exact_value_and_preserves_new_siblings() {
        let scratch = Scratch::new();
        let keys = OverrideKeys::new(&scratch.subkey);
        let key = create_key_write(HKEY_CURRENT_USER, &keys.inproc).unwrap();
        write_expand_string_value(&key, None, r"%SystemRoot%\original.dll").unwrap();
        let original = read_raw_value(&key, None).unwrap();
        assert!(
            enable_in_subkey(&scratch.subkey, &scratch.backup)
                .unwrap()
                .changed
        );
        assert!(
            !enable_in_subkey(&scratch.subkey, &scratch.backup)
                .unwrap()
                .changed
        );
        write_string_value(&key, Some("ThreadingModel"), "Apartment").unwrap();
        let extra = format!(r"{}\Subkey", keys.inproc);
        let child = create_key_write(HKEY_CURRENT_USER, &extra).unwrap();
        write_string_value(&child, None, "keep").unwrap();

        let result = disable_in_subkey(&scratch.subkey, &scratch.backup, false).unwrap();
        assert!(result.changed);
        assert!(!result.classic_menu_enabled);
        assert_eq!(read_raw_value(&key, None).unwrap(), original);
        assert_eq!(
            read_string_value(&key, Some("ThreadingModel"))
                .unwrap()
                .as_deref(),
            Some("Apartment")
        );
        assert_eq!(
            read_string_value(&child, None).unwrap().as_deref(),
            Some("keep")
        );
    }

    #[test]
    fn missing_backup_requires_force_and_force_preserves_siblings() {
        let scratch = Scratch::new();
        let keys = OverrideKeys::new(&scratch.subkey);
        let key = create_key_write(HKEY_CURRENT_USER, &keys.inproc).unwrap();
        write_string_value(&key, None, "").unwrap();
        write_string_value(&key, Some("Sibling"), "keep").unwrap();
        assert!(matches!(
            disable_in_subkey(&scratch.subkey, &scratch.backup, false),
            Err(ClassicError::BackupNotFoundNeedsForce)
        ));
        assert!(
            disable_in_subkey(&scratch.subkey, &scratch.backup, true)
                .unwrap()
                .changed
        );
        assert_eq!(read_raw_value(&key, None).unwrap(), None);
        assert_eq!(
            read_string_value(&key, Some("Sibling")).unwrap().as_deref(),
            Some("keep")
        );
    }

    #[test]
    fn disable_removes_only_empty_keys_created_by_rwm() {
        let scratch = Scratch::new();
        let keys = OverrideKeys::new(&scratch.subkey);
        enable_in_subkey(&scratch.subkey, &scratch.backup).unwrap();
        disable_in_subkey(&scratch.subkey, &scratch.backup, false).unwrap();
        assert!(open_key_read(HKEY_CURRENT_USER, &keys.clsid)
            .unwrap()
            .is_none());
        assert!(
            !disable_in_subkey(&scratch.subkey, &scratch.backup, false)
                .unwrap()
                .changed
        );
    }

    #[test]
    fn v01_backup_restores_menu_show_delay_and_corrupt_backup_requires_force() {
        let scratch = Scratch::new();
        let keys = OverrideKeys::new(&scratch.subkey);
        let inproc = create_key_write(HKEY_CURRENT_USER, &keys.inproc).unwrap();
        write_string_value(&inproc, None, "").unwrap();
        let desktop_subkey = format!(r"{}\Desktop", scratch.subkey);
        let desktop = create_key_write(HKEY_CURRENT_USER, &desktop_subkey).unwrap();
        write_string_value(&desktop, Some("MenuShowDelay"), "0").unwrap();

        let v01_backup = serde_json::json!({
            "had_clsid_key": false,
            "had_inproc_key": false,
            "prior_inproc_default": null,
            "prior_menu_show_delay": "400",
            "prior_inproc_value": null,
            "prior_menu_show_delay_value": {
                "value_type": 1,
                "data": RawRegValue::string("400").data
            }
        });
        fs::write(&scratch.backup, v01_backup.to_string()).unwrap();

        let result =
            disable_in_subkeys(&scratch.subkey, &desktop_subkey, &scratch.backup, false).unwrap();
        assert!(result.changed);
        assert_eq!(
            read_string_value(&desktop, Some("MenuShowDelay"))
                .unwrap()
                .as_deref(),
            Some("400")
        );
        assert!(!scratch.backup.exists());

        enable_in_subkey(&scratch.subkey, &scratch.backup).unwrap();
        fs::write(&scratch.backup, b"{corrupt json").unwrap();
        assert!(matches!(
            disable_in_subkeys(&scratch.subkey, &desktop_subkey, &scratch.backup, false),
            Err(ClassicError::BackupCorrupt { .. })
        ));
        let mut corrupt = scratch.backup.as_os_str().to_os_string();
        corrupt.push(".corrupt");
        let corrupt = PathBuf::from(corrupt);
        assert!(corrupt.is_file());
        let _ = fs::remove_file(&corrupt);

        fs::write(&scratch.backup, b"{still corrupt").unwrap();
        let forced =
            disable_in_subkeys(&scratch.subkey, &desktop_subkey, &scratch.backup, true).unwrap();
        assert!(forced.changed);
        assert!(open_key_read(HKEY_CURRENT_USER, &keys.clsid)
            .unwrap()
            .is_none());
        let _ = fs::remove_file(&corrupt);
    }
}
