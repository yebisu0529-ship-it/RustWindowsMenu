use std::io;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW,
    RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_EXPAND_SZ,
    REG_OPTION_NON_VOLATILE, REG_SAM_FLAGS, REG_SZ,
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

fn create(subkey: &str) -> io::Result<Key> {
    let subkey = wide(subkey);
    let mut key = HKEY::default();
    check(unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            0,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE | KEY_QUERY_VALUE,
            None,
            &mut key,
            None,
        )
    })?;
    Ok(Key(key))
}

fn decode(bytes: &[u8]) -> String {
    let units = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .take_while(|unit| *unit != 0)
        .collect::<Vec<_>>();
    String::from_utf16_lossy(&units)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedString {
    pub value: String,
    pub expand: bool,
}

pub fn read_typed_string(subkey: &str, value: &str) -> io::Result<Option<TypedString>> {
    let Some(key) = open(subkey, KEY_QUERY_VALUE)? else {
        return Ok(None);
    };
    let value_wide = wide(value);
    let mut kind = windows::Win32::System::Registry::REG_VALUE_TYPE::default();
    let mut len = 0u32;
    let code = unsafe {
        RegQueryValueExW(
            key.0,
            PCWSTR(value_wide.as_ptr()),
            None,
            Some(&mut kind),
            None,
            Some(&mut len),
        )
    };
    if code == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    check(code)?;
    if kind != REG_SZ && kind != REG_EXPAND_SZ {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{subkey}\\{value} is not a string value"),
        ));
    }
    let mut bytes = vec![0u8; len as usize];
    check(unsafe {
        RegQueryValueExW(
            key.0,
            PCWSTR(value_wide.as_ptr()),
            None,
            None,
            Some(bytes.as_mut_ptr()),
            Some(&mut len),
        )
    })?;
    bytes.truncate(len as usize);
    Ok(Some(TypedString {
        value: decode(&bytes),
        expand: kind == REG_EXPAND_SZ,
    }))
}

pub fn read_string(subkey: &str, value: &str) -> io::Result<Option<String>> {
    Ok(read_typed_string(subkey, value)?.map(|value| value.value))
}

fn write_typed(subkey: &str, value: &str, data: &str, expand: bool) -> io::Result<()> {
    let key = create(subkey)?;
    let value = wide(value);
    let bytes = wide(data)
        .into_iter()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    check(unsafe {
        RegSetValueExW(
            key.0,
            PCWSTR(value.as_ptr()),
            0,
            if expand { REG_EXPAND_SZ } else { REG_SZ },
            Some(&bytes),
        )
    })
}

pub fn write_string(subkey: &str, value: &str, data: &str) -> io::Result<()> {
    write_typed(subkey, value, data, false)
}

pub fn delete_value(subkey: &str, value: &str) -> io::Result<bool> {
    let Some(key) = open(subkey, KEY_SET_VALUE)? else {
        return Ok(false);
    };
    let code = unsafe { RegDeleteValueW(key.0, PCWSTR(wide(value).as_ptr())) };
    if code == ERROR_FILE_NOT_FOUND {
        return Ok(false);
    }
    check(code)?;
    Ok(true)
}

pub fn delete_tree(subkey: &str) -> io::Result<()> {
    let code = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(wide(subkey).as_ptr())) };
    if code == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        check(code)
    }
}

fn same_path_entry(left: &str, right: &str) -> bool {
    let normalize = |entry: &str| {
        entry
            .trim()
            .trim_matches('"')
            .trim_end_matches(['\\', '/'])
            .to_ascii_lowercase()
    };
    normalize(left) == normalize(right)
}

pub fn update_path_entries(existing: Option<&str>, bin: &std::path::Path, remove: bool) -> String {
    let bin = bin.to_string_lossy();
    let mut entries = existing
        .unwrap_or_default()
        .split(';')
        .filter(|entry| !entry.trim().is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    entries.retain(|entry| !same_path_entry(entry, &bin));
    if !remove {
        entries.push(bin.into_owned());
    }
    entries.join(";")
}

pub fn update_user_path_in_key(
    subkey: &str,
    bin: &std::path::Path,
    remove: bool,
) -> io::Result<bool> {
    let previous = read_typed_string(subkey, "Path")?;
    let bin_text = bin.to_string_lossy();
    let already_present = previous.as_ref().is_some_and(|entry| {
        entry
            .value
            .split(';')
            .any(|segment| same_path_entry(segment, &bin_text))
    });
    if remove && !already_present {
        return Ok(false);
    }
    let updated = update_path_entries(
        previous.as_ref().map(|entry| entry.value.as_str()),
        bin,
        remove,
    );
    if previous
        .as_ref()
        .is_some_and(|entry| entry.value == updated)
    {
        return Ok(false);
    }
    if remove && previous.is_none() {
        return Ok(false);
    }
    write_typed(
        subkey,
        "Path",
        &updated,
        previous.as_ref().is_none_or(|entry| entry.expand),
    )?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::RegistryRoot;
    use std::path::Path;

    #[test]
    fn path_update_preserves_expand_type_and_is_idempotent() {
        let root = RegistryRoot::new("path");
        let subkey = format!(r"{}\Environment", root.path());
        write_typed(&subkey, "Path", r"%USERPROFILE%\bin;C:\Tools", true).unwrap();
        let bin = Path::new(r"C:\Users\Test\RustWindowsMenu\bin");
        assert!(update_user_path_in_key(&subkey, bin, false).unwrap());
        let once = read_typed_string(&subkey, "Path").unwrap().unwrap();
        assert!(once.expand);
        assert_eq!(
            once.value,
            format!(r"%USERPROFILE%\bin;C:\Tools;{}", bin.display())
        );
        assert!(!update_user_path_in_key(&subkey, bin, false).unwrap());
        assert!(update_user_path_in_key(&subkey, bin, true).unwrap());
        assert_eq!(
            read_string(&subkey, "Path").unwrap().as_deref(),
            Some(r"%USERPROFILE%\bin;C:\Tools")
        );
        assert!(!update_user_path_in_key(&subkey, bin, true).unwrap());
    }

    #[test]
    fn matching_ignores_case_quotes_and_trailing_slash_only_for_our_entry() {
        let updated = update_path_entries(
            Some("C:\\Other;\"c:\\rwm\\bin\""),
            Path::new(r"C:\RWM\bin"),
            true,
        );
        assert_eq!(updated, r"C:\Other");
    }
}
