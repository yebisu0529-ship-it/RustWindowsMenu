//! String values under `HKEY_CURRENT_USER`, the only registry data the CLI itself owns.
//! The classic-menu keys belong to `rwm_core::classic`.

use std::io;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW,
    RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE,
    REG_OPEN_CREATE_OPTIONS, REG_OPTION_NON_VOLATILE, REG_OPTION_VOLATILE, REG_SAM_FLAGS, REG_SZ,
};

struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

fn check(code: WIN32_ERROR) -> io::Result<()> {
    if code == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code.0 as i32))
    }
}

/// Opens an existing key. `None` when the key does not exist.
fn open(subkey: &str, access: REG_SAM_FLAGS) -> io::Result<Option<Key>> {
    let subkey = wide(subkey);
    let mut key = HKEY::default();
    let code = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            0,
            access,
            &mut key,
        )
    };
    if code == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    check(code)?;
    Ok(Some(Key(key)))
}

fn create(subkey: &str, options: REG_OPEN_CREATE_OPTIONS) -> io::Result<Key> {
    let subkey = wide(subkey);
    let mut key = HKEY::default();
    check(unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            0,
            PCWSTR::null(),
            options,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
    })?;
    Ok(Key(key))
}

fn set_string(key: &Key, value: &str, data: &str) -> io::Result<()> {
    let value = wide(value);
    let bytes: Vec<u8> = wide(data).into_iter().flat_map(u16::to_le_bytes).collect();
    check(unsafe { RegSetValueExW(key.0, PCWSTR(value.as_ptr()), 0, REG_SZ, Some(&bytes)) })
}

/// Reads a string value. `None` when the key or the value does not exist.
pub fn read_string(subkey: &str, value: &str) -> io::Result<Option<String>> {
    let Some(key) = open(subkey, KEY_QUERY_VALUE)? else {
        return Ok(None);
    };
    let value = wide(value);

    let mut byte_len = 0u32;
    let code = unsafe {
        RegQueryValueExW(
            key.0,
            PCWSTR(value.as_ptr()),
            None,
            None,
            None,
            Some(&mut byte_len),
        )
    };
    if code == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    check(code)?;

    let mut bytes = vec![0u8; byte_len as usize];
    check(unsafe {
        RegQueryValueExW(
            key.0,
            PCWSTR(value.as_ptr()),
            None,
            None,
            Some(bytes.as_mut_ptr()),
            Some(&mut byte_len),
        )
    })?;
    bytes.truncate(byte_len as usize);

    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .take_while(|unit| *unit != 0)
        .collect();
    Ok(Some(String::from_utf16_lossy(&units)))
}

/// Writes a `REG_SZ` value, creating the key when needed.
pub fn write_string(subkey: &str, value: &str, data: &str) -> io::Result<()> {
    let key = create(subkey, REG_OPTION_NON_VOLATILE)?;
    set_string(&key, value, data)
}

/// Deletes a value. False when it was not there.
pub fn delete_value(subkey: &str, value: &str) -> io::Result<bool> {
    let Some(key) = open(subkey, KEY_SET_VALUE)? else {
        return Ok(false);
    };
    let value = wide(value);
    let code = unsafe { RegDeleteValueW(key.0, PCWSTR(value.as_ptr())) };
    if code == ERROR_FILE_NOT_FOUND {
        return Ok(false);
    }
    check(code)?;
    Ok(true)
}

/// Proves HKCU is writable by creating, writing, and deleting a scratch key. The key is
/// volatile, so it never reaches the registry file even if the delete fails.
pub fn probe_write_access() -> io::Result<()> {
    let subkey = format!(
        r"Software\RustWindowsMenu_WriteProbe_{}",
        std::process::id()
    );
    let written =
        create(&subkey, REG_OPTION_VOLATILE).and_then(|key| set_string(&key, "probe", "1"));
    let wide_subkey = wide(&subkey);
    let deleted = unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(wide_subkey.as_ptr())) };
    written?;
    check(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::RegistryRoot;

    #[test]
    fn string_values_round_trip_and_delete_is_idempotent() {
        let root = RegistryRoot::new("reg_round_trip");
        let subkey = format!(r"{}\Run", root.path());

        assert_eq!(read_string(&subkey, "RustWindowsMenu").unwrap(), None);
        assert!(!delete_value(&subkey, "RustWindowsMenu").unwrap());

        let command = r#""C:\Users\太郎\AppData\Local\RustWindowsMenu\bin\rwm.exe" run"#;
        write_string(&subkey, "RustWindowsMenu", command).unwrap();
        assert_eq!(
            read_string(&subkey, "RustWindowsMenu").unwrap().as_deref(),
            Some(command)
        );

        write_string(&subkey, "RustWindowsMenu", "replaced").unwrap();
        assert_eq!(
            read_string(&subkey, "RustWindowsMenu").unwrap().as_deref(),
            Some("replaced")
        );
        assert_eq!(read_string(&subkey, "Other").unwrap(), None);

        assert!(delete_value(&subkey, "RustWindowsMenu").unwrap());
        assert!(!delete_value(&subkey, "RustWindowsMenu").unwrap());
        assert_eq!(read_string(&subkey, "RustWindowsMenu").unwrap(), None);
    }

    #[test]
    fn write_probe_succeeds_and_leaves_no_key_behind() {
        probe_write_access().expect("HKCU is writable for the test user");
        let subkey = format!(
            r"Software\RustWindowsMenu_WriteProbe_{}",
            std::process::id()
        );
        assert!(open(&subkey, KEY_QUERY_VALUE).unwrap().is_none());
    }
}
