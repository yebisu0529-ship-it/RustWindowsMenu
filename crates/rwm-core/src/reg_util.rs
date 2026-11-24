use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, ERROR_PATH_NOT_FOUND,
    ERROR_SUCCESS, ERROR_TIMEOUT, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT, WIN32_ERROR,
};
use windows::Win32::Storage::FileSystem::{
    MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
};
use windows::Win32::System::Com::CLSIDFromString;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegDeleteValueW, RegEnumKeyExW, RegEnumValueW,
    RegOpenKeyExW, RegQueryInfoKeyW, RegQueryValueExW, RegSetValueExW, HKEY, KEY_READ, KEY_WRITE,
    REG_EXPAND_SZ, REG_OPTION_NON_VOLATILE, REG_SAM_FLAGS, REG_SZ, REG_VALUE_TYPE,
};
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
use windows_core::{GUID, PCWSTR, PWSTR};

pub(crate) fn wide_null(value: impl AsRef<OsStr>) -> Vec<u16> {
    value
        .as_ref()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

pub(crate) unsafe fn pwstr_to_string(ptr: PWSTR) -> Option<String> {
    (!ptr.is_null()).then(|| unsafe { String::from_utf16_lossy(ptr.as_wide()) })
}

pub(crate) fn parse_clsid(clsid: &str) -> windows_core::Result<GUID> {
    let clsid = clsid.trim();
    let bare = clsid
        .strip_prefix('{')
        .and_then(|value| value.strip_suffix('}'))
        .unwrap_or(clsid);
    if bare.len() != 36
        || !bare.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
    {
        return Err(windows_core::Error::new(
            windows::Win32::Foundation::E_INVALIDARG,
            format!("Invalid CLSID {clsid:?}"),
        ));
    }
    let canonical = wide_null(format!("{{{bare}}}"));
    unsafe { CLSIDFromString(PCWSTR(canonical.as_ptr())) }
}

pub(crate) fn format_clsid(clsid: &GUID) -> String {
    format!("{{{clsid:?}}}").to_ascii_uppercase()
}

pub(crate) struct RegKeyGuard(pub(crate) HKEY);

impl Drop for RegKeyGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

fn missing(status: WIN32_ERROR) -> bool {
    matches!(status, ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND)
}

fn checked(status: WIN32_ERROR) -> io::Result<()> {
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(status.0 as i32))
    }
}

fn open_key(root: HKEY, subkey: &str, access: REG_SAM_FLAGS) -> io::Result<Option<RegKeyGuard>> {
    let subkey = wide_null(subkey);
    let mut key = HKEY::default();
    let status = unsafe { RegOpenKeyExW(root, PCWSTR(subkey.as_ptr()), 0, access, &mut key) };
    if missing(status) {
        return Ok(None);
    }
    checked(status)?;
    Ok(Some(RegKeyGuard(key)))
}

pub(crate) fn open_key_read(root: HKEY, subkey: &str) -> io::Result<Option<RegKeyGuard>> {
    open_key(root, subkey, KEY_READ)
}

pub(crate) fn open_key_write(root: HKEY, subkey: &str) -> io::Result<Option<RegKeyGuard>> {
    open_key(root, subkey, KEY_READ | KEY_WRITE)
}

pub(crate) fn create_key_write(root: HKEY, subkey: &str) -> io::Result<RegKeyGuard> {
    let subkey = wide_null(subkey);
    let mut key = HKEY::default();
    checked(unsafe {
        RegCreateKeyExW(
            root,
            PCWSTR(subkey.as_ptr()),
            0,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_READ | KEY_WRITE,
            None,
            &mut key,
            None,
        )
    })?;
    Ok(RegKeyGuard(key))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RawRegValue {
    pub value_type: u32,
    pub data: Vec<u8>,
}

impl RawRegValue {
    pub(crate) fn string(value: &str) -> Self {
        Self::text(value, REG_SZ)
    }

    fn text(value: &str, kind: REG_VALUE_TYPE) -> Self {
        Self {
            value_type: kind.0,
            data: wide_null(value)
                .into_iter()
                .flat_map(u16::to_le_bytes)
                .collect(),
        }
    }

    pub(crate) fn as_string(&self) -> Option<String> {
        if !matches!(REG_VALUE_TYPE(self.value_type), REG_SZ | REG_EXPAND_SZ) {
            return None;
        }
        let units: Vec<_> = self
            .data
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .take_while(|&unit| unit != 0)
            .collect();
        Some(String::from_utf16_lossy(&units))
    }
}

pub(crate) fn read_raw_value(
    key: &RegKeyGuard,
    name: Option<&str>,
) -> io::Result<Option<RawRegValue>> {
    let name = name.map(wide_null);
    let name = name
        .as_ref()
        .map_or(PCWSTR::null(), |name| PCWSTR(name.as_ptr()));
    let mut kind = REG_VALUE_TYPE::default();
    let mut size = 0;
    let status = unsafe { RegQueryValueExW(key.0, name, None, None, None, Some(&mut size)) };
    if missing(status) {
        return Ok(None);
    }
    checked(status)?;

    loop {
        let mut data = vec![0; size as usize];
        let status = unsafe {
            RegQueryValueExW(
                key.0,
                name,
                None,
                Some(&mut kind),
                Some(data.as_mut_ptr()),
                Some(&mut size),
            )
        };
        if missing(status) {
            return Ok(None);
        }
        if status != ERROR_MORE_DATA {
            checked(status)?;
            data.truncate(size as usize);
            return Ok(Some(RawRegValue {
                value_type: kind.0,
                data,
            }));
        }
    }
}

pub(crate) fn read_string_value(
    key: &RegKeyGuard,
    name: Option<&str>,
) -> io::Result<Option<String>> {
    Ok(read_raw_value(key, name)?.and_then(|value| value.as_string()))
}

pub(crate) fn write_raw_value(
    key: &RegKeyGuard,
    name: Option<&str>,
    value: &RawRegValue,
) -> io::Result<()> {
    let name = name.map(wide_null);
    let name = name
        .as_ref()
        .map_or(PCWSTR::null(), |name| PCWSTR(name.as_ptr()));
    checked(unsafe {
        RegSetValueExW(
            key.0,
            name,
            0,
            REG_VALUE_TYPE(value.value_type),
            Some(&value.data),
        )
    })
}

pub(crate) fn write_string_value(
    key: &RegKeyGuard,
    name: Option<&str>,
    value: &str,
) -> io::Result<()> {
    write_raw_value(key, name, &RawRegValue::string(value))
}

#[allow(dead_code)]
pub(crate) fn write_expand_string_value(
    key: &RegKeyGuard,
    name: Option<&str>,
    value: &str,
) -> io::Result<()> {
    write_raw_value(key, name, &RawRegValue::text(value, REG_EXPAND_SZ))
}

pub(crate) fn delete_value_ignore_missing(
    key: &RegKeyGuard,
    name: Option<&str>,
) -> io::Result<bool> {
    let name = name.map(wide_null);
    let name = name
        .as_ref()
        .map_or(PCWSTR::null(), |name| PCWSTR(name.as_ptr()));
    let status = unsafe { RegDeleteValueW(key.0, name) };
    if missing(status) {
        return Ok(false);
    }
    checked(status)?;
    Ok(true)
}

pub(crate) fn is_key_empty(key: &RegKeyGuard) -> io::Result<bool> {
    let (mut subkeys, mut values) = (0, 0);
    checked(unsafe {
        RegQueryInfoKeyW(
            key.0,
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
    })?;
    Ok(subkeys == 0 && values == 0)
}

pub(crate) fn delete_key_if_empty(root: HKEY, subkey: &str) -> io::Result<bool> {
    let Some(key) = open_key_read(root, subkey)? else {
        return Ok(false);
    };
    if !is_key_empty(&key)? {
        return Ok(false);
    }
    let subkey = wide_null(subkey);
    let status = unsafe { RegDeleteKeyW(root, PCWSTR(subkey.as_ptr())) };
    if missing(status) {
        return Ok(false);
    }
    checked(status)?;
    Ok(true)
}

fn enum_names(
    mut read: impl FnMut(u32, PWSTR, &mut u32) -> WIN32_ERROR,
) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    let mut buffer = vec![0; 256];
    loop {
        let mut len = buffer.len() as u32;
        match read(names.len() as u32, PWSTR(buffer.as_mut_ptr()), &mut len) {
            ERROR_SUCCESS => names.push(String::from_utf16_lossy(&buffer[..len as usize])),
            ERROR_MORE_DATA => buffer.resize(buffer.len() * 2, 0),
            ERROR_NO_MORE_ITEMS => return Ok(names),
            status => checked(status)?,
        }
    }
}

pub(crate) fn enum_subkeys(key: &RegKeyGuard) -> io::Result<Vec<String>> {
    enum_names(|index, name, len| unsafe {
        RegEnumKeyExW(key.0, index, name, len, None, PWSTR::null(), None, None)
    })
}

pub(crate) fn enum_values(key: &RegKeyGuard) -> io::Result<Vec<String>> {
    enum_names(|index, name, len| unsafe {
        RegEnumValueW(key.0, index, name, len, None, None, None, None)
    })
}

pub(crate) struct HandleGuard(pub(crate) HANDLE);
impl Drop for HandleGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

pub(crate) struct NamedMutex(HandleGuard);
impl Drop for NamedMutex {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.0 .0);
        }
    }
}

pub(crate) fn acquire_named_mutex(name: &str) -> windows_core::Result<NamedMutex> {
    const MUTEX_TIMEOUT_MS: u32 = 15_000;
    let name = wide_null(name);
    let handle = HandleGuard(unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr()))? });
    match unsafe { WaitForSingleObject(handle.0, MUTEX_TIMEOUT_MS) } {
        WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(NamedMutex(handle)),
        WAIT_TIMEOUT => Err(windows_core::Error::from(ERROR_TIMEOUT.to_hresult())),
        _ => Err(windows_core::Error::from_win32()),
    }
}

pub(crate) fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let (temporary, mut file) = loop {
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut temporary = path.as_os_str().to_os_string();
        temporary.push(format!(".tmp.{}.{sequence}", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => break (temporary, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };
    let result = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    let result = result.and_then(|()| {
        let from = wide_null(&temporary);
        let to = wide_null(path);
        let success = unsafe {
            MoveFileExW(
                PCWSTR(from.as_ptr()),
                PCWSTR(to.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        success.map_err(|_| io::Error::last_os_error())
    });
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
pub(crate) struct Scratch {
    pub subkey: String,
    pub backup: std::path::PathBuf,
}

#[cfg(test)]
impl Scratch {
    pub(crate) fn new() -> Self {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let name = format!("{}-{}-{sequence}", std::process::id(), now.as_nanos());
        Self {
            subkey: format!(r"Software\RustWindowsMenuTest\{name}"),
            backup: std::env::temp_dir().join(format!("rwm-test-{name}.json")),
        }
    }
}

#[cfg(test)]
impl Drop for Scratch {
    fn drop(&mut self) {
        use windows::Win32::System::Registry::{RegDeleteTreeW, HKEY_CURRENT_USER};
        let subkey = wide_null(&self.subkey);
        unsafe {
            let _ = RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(subkey.as_ptr()));
        }
        let _ = fs::remove_file(&self.backup);
    }
}
