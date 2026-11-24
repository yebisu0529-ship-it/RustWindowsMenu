use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};

use crate::config::local_data_dir;
use crate::reg_util::{
    acquire_named_mutex, create_key_write, delete_value_ignore_missing, enum_values, format_clsid,
    open_key_read, open_key_write, parse_clsid, read_raw_value, read_string_value,
    write_bytes_atomic, write_raw_value, RawRegValue,
};
use crate::shell::ShellError;

const BLOCKED: &str = r"Software\Microsoft\Windows\CurrentVersion\Shell Extensions\Blocked";
const LOCK: &str = r"Local\RustWindowsMenuHandlersLock";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockedHandler {
    pub clsid: String,
    pub name: Option<String>,
    pub added_by_rwm: bool,
}

#[derive(Debug, Serialize, Deserialize)]
enum Ownership {
    Preexisting,
    Added(RawRegValue),
}

type Backup = BTreeMap<String, Ownership>;

fn backup_path() -> PathBuf {
    local_data_dir().join("handlers_backup.json")
}

fn read_backup(path: &Path) -> Result<Backup, ShellError> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error).into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Backup::new()),
        Err(error) => Err(error.into()),
    }
}

fn save_backup(path: &Path, backup: &Backup) -> Result<(), ShellError> {
    let bytes = serde_json::to_vec_pretty(backup).map_err(io::Error::other)?;
    Ok(write_bytes_atomic(path, &bytes)?)
}

fn owns(ownership: Option<&Ownership>, actual: &RawRegValue) -> bool {
    matches!(ownership, Some(Ownership::Added(recorded)) if recorded == actual)
}

pub fn list_blocked() -> Result<Vec<BlockedHandler>, ShellError> {
    list_blocked_in_subkey(BLOCKED, &backup_path())
}

pub fn list_blocked_in_subkey(
    subkey: &str,
    backup_path: &Path,
) -> Result<Vec<BlockedHandler>, ShellError> {
    let _lock = acquire_named_mutex(LOCK)?;
    let Some(key) = open_key_read(HKEY_CURRENT_USER, subkey)? else {
        return Ok(Vec::new());
    };
    let backup = read_backup(backup_path)?;
    let mut handlers = Vec::new();
    for name in enum_values(&key)? {
        let Ok(guid) = parse_clsid(&name) else {
            continue;
        };
        let Some(value) = read_raw_value(&key, Some(&name))? else {
            continue;
        };
        let clsid = format_clsid(&guid);
        handlers.push(BlockedHandler {
            name: value.as_string().filter(|name| !name.is_empty()),
            added_by_rwm: clsid.eq_ignore_ascii_case(&name) && owns(backup.get(&clsid), &value),
            clsid,
        });
    }
    handlers.sort_by(|a, b| a.clsid.cmp(&b.clsid));
    Ok(handlers)
}

pub fn block_clsid(clsid: &str, name: Option<&str>) -> Result<(), ShellError> {
    block_clsid_in_subkey(BLOCKED, &backup_path(), clsid, name)
}

pub fn block_clsid_in_subkey(
    subkey: &str,
    backup_path: &Path,
    clsid: &str,
    name: Option<&str>,
) -> Result<(), ShellError> {
    let clsid = format_clsid(&parse_clsid(clsid)?);
    let _lock = acquire_named_mutex(LOCK)?;
    let mut backup = read_backup(backup_path)?;
    let existing = open_key_read(HKEY_CURRENT_USER, subkey)?
        .map(|key| read_raw_value(&key, Some(&clsid)))
        .transpose()?
        .flatten();
    if let Some(existing) = existing {
        if !owns(backup.get(&clsid), &existing) {
            backup.insert(clsid, Ownership::Preexisting);
            save_backup(backup_path, &backup)?;
        }
        return Ok(());
    }
    let value = RawRegValue::string(name.unwrap_or_default());
    backup.insert(clsid.clone(), Ownership::Added(value.clone()));
    save_backup(backup_path, &backup)?;
    let key = create_key_write(HKEY_CURRENT_USER, subkey)?;
    write_raw_value(&key, Some(&clsid), &value)?;
    Ok(())
}

pub fn unblock_clsid(clsid: &str, force: bool) -> Result<(), ShellError> {
    unblock_clsid_in_subkey(BLOCKED, &backup_path(), clsid, force)
}

pub fn unblock_clsid_in_subkey(
    subkey: &str,
    backup_path: &Path,
    clsid: &str,
    force: bool,
) -> Result<(), ShellError> {
    let clsid = format_clsid(&parse_clsid(clsid)?);
    let _lock = acquire_named_mutex(LOCK)?;
    let mut backup = read_backup(backup_path)?;
    if let Some(key) = open_key_write(HKEY_CURRENT_USER, subkey)? {
        if let Some(actual) = read_raw_value(&key, Some(&clsid))? {
            if !force && !owns(backup.get(&clsid), &actual) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "{clsid} was blocked outside RustWindowsMenu; use --force to remove it"
                    ),
                )
                .into());
            }
            delete_value_ignore_missing(&key, Some(&clsid))?;
        }
    }
    if backup.remove(&clsid).is_some() {
        save_backup(backup_path, &backup)?;
    }
    Ok(())
}

pub fn restore_rwm_blocks() -> Result<usize, ShellError> {
    restore_rwm_blocks_in_subkey(BLOCKED, &backup_path())
}

pub fn restore_rwm_blocks_in_subkey(subkey: &str, backup_path: &Path) -> Result<usize, ShellError> {
    let _lock = acquire_named_mutex(LOCK)?;
    let backup = read_backup(backup_path)?;
    let mut removed = 0;
    if let Some(key) = open_key_write(HKEY_CURRENT_USER, subkey)? {
        for (clsid, ownership) in &backup {
            if parse_clsid(clsid).is_ok() {
                if let Some(actual) = read_raw_value(&key, Some(clsid))? {
                    if owns(Some(ownership), &actual) {
                        removed += usize::from(delete_value_ignore_missing(&key, Some(clsid))?);
                    }
                }
            }
        }
    }
    match fs::remove_file(backup_path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(removed)
}

pub(crate) fn is_blocked(clsid: &str) -> io::Result<bool> {
    for root in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        if let Some(key) = open_key_read(root, BLOCKED)? {
            if read_raw_value(&key, Some(clsid))?.is_some() {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

pub(crate) fn handler_name(clsid: &str) -> io::Result<Option<String>> {
    let path = format!(r"CLSID\{clsid}");
    let Some(key) = open_key_read(windows::Win32::System::Registry::HKEY_CLASSES_ROOT, &path)?
    else {
        return Ok(None);
    };
    read_string_value(&key, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reg_util::{write_string_value, Scratch};

    const USER: &str = "{01234567-89AB-CDEF-0123-456789ABCDEF}";
    const RWM: &str = "{11111111-2222-3333-4444-555555555555}";
    const EDITED: &str = "{22222222-2222-3333-4444-555555555555}";

    #[test]
    fn blocks_and_restore_preserve_user_values() {
        let scratch = Scratch::new();
        let key = create_key_write(HKEY_CURRENT_USER, &scratch.subkey).unwrap();
        write_string_value(&key, Some(USER), "user reason").unwrap();
        block_clsid_in_subkey(
            &scratch.subkey,
            &scratch.backup,
            &USER.to_lowercase(),
            Some("ignored"),
        )
        .unwrap();
        block_clsid_in_subkey(&scratch.subkey, &scratch.backup, RWM, None).unwrap();
        block_clsid_in_subkey(&scratch.subkey, &scratch.backup, EDITED, None).unwrap();
        write_string_value(&key, Some(EDITED), "outside edit").unwrap();

        let entries = list_blocked_in_subkey(&scratch.subkey, &scratch.backup).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries.iter().filter(|entry| entry.added_by_rwm).count(), 1);
        let error =
            unblock_clsid_in_subkey(&scratch.subkey, &scratch.backup, USER, false).unwrap_err();
        assert!(error.to_string().contains("--force"));
        assert_eq!(
            restore_rwm_blocks_in_subkey(&scratch.subkey, &scratch.backup).unwrap(),
            1
        );
        assert_eq!(read_raw_value(&key, Some(RWM)).unwrap(), None);
        assert_eq!(
            read_string_value(&key, Some(EDITED)).unwrap().as_deref(),
            Some("outside edit")
        );
        assert_eq!(
            read_string_value(&key, Some(USER)).unwrap().as_deref(),
            Some("user reason")
        );
        unblock_clsid_in_subkey(&scratch.subkey, &scratch.backup, USER, true).unwrap();
        assert_eq!(read_raw_value(&key, Some(USER)).unwrap(), None);
    }

    #[test]
    fn unblocks_owned_clsid_and_rejects_non_guids() {
        let scratch = Scratch::new();
        block_clsid_in_subkey(
            &scratch.subkey,
            &scratch.backup,
            RWM.trim_matches(['{', '}']),
            Some("rwm"),
        )
        .unwrap();
        unblock_clsid_in_subkey(&scratch.subkey, &scratch.backup, RWM, false).unwrap();
        assert!(list_blocked_in_subkey(&scratch.subkey, &scratch.backup)
            .unwrap()
            .is_empty());
        assert!(
            block_clsid_in_subkey(&scratch.subkey, &scratch.backup, "Shell.Application", None)
                .is_err()
        );
    }
}
