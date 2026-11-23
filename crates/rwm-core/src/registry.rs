use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use parking_lot::{Mutex, RwLock};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS, HANDLE, WAIT_OBJECT_0,
};
use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegEnumValueW, RegNotifyChangeKeyValue, RegOpenKeyExW,
    RegQueryValueExW, HKEY, HKEY_CLASSES_ROOT, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_NOTIFY,
    KEY_READ, REG_NOTIFY_CHANGE_LAST_SET, REG_NOTIFY_CHANGE_NAME, REG_VALUE_TYPE,
};
use windows::Win32::System::Threading::{CreateEventW, ResetEvent, SetEvent, WaitForMultipleObjects};
use windows::Win32::UI::Shell::SHLoadIndirectString;

use crate::model::{parse_mnemonic_label, RegistryScope, SelectionTarget};

#[derive(Debug, Clone)]
pub struct StaticVerb {
    pub key_name: String,
    pub label: String,
    pub mnemonic: Option<char>,
    pub icon_spec: Option<String>,
    pub command: Option<String>,
    pub extended_only: bool,
    pub subcommands: Vec<StaticVerb>,
    pub canonical_verb: Option<String>,
    pub position: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DiscoveredComHandler {
    pub name: String,
    pub clsid: String,
    pub dll_path: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ScopeEntries {
    pub static_verbs: Vec<StaticVerb>,
    pub com_handlers: Vec<DiscoveredComHandler>,
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

struct HandleGuard(HANDLE);

unsafe impl Send for HandleGuard {}
unsafe impl Sync for HandleGuard {}

impl Drop for HandleGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

pub struct RegistryVerbCache {
    scopes: Arc<RwLock<HashMap<String, ScopeEntries>>>,
    progids: Arc<RwLock<HashMap<String, Option<String>>>>,
    blocked_clsids: Arc<RwLock<HashSet<String>>>,
    generation: Arc<AtomicU64>,
    stop_flag: Arc<AtomicBool>,
    stop_event: Option<HandleGuard>,
    watcher: Mutex<Option<JoinHandle<()>>>,
}

impl RegistryVerbCache {
    pub fn new(with_watcher: bool) -> Self {
        let scopes = Arc::new(RwLock::new(HashMap::with_capacity(32)));
        let progids = Arc::new(RwLock::new(HashMap::with_capacity(32)));
        let blocked_clsids = Arc::new(RwLock::new(load_blocked_clsids()));
        let generation = Arc::new(AtomicU64::new(0));
        let stop_flag = Arc::new(AtomicBool::new(false));

        let mut cache = Self {
            scopes: Arc::clone(&scopes),
            progids: Arc::clone(&progids),
            blocked_clsids: Arc::clone(&blocked_clsids),
            generation: Arc::clone(&generation),
            stop_flag: Arc::clone(&stop_flag),
            stop_event: None,
            watcher: Mutex::new(None),
        };

        cache.prewarm_hot_scopes();

        if with_watcher {
            if let Ok(raw_stop_event) = unsafe { CreateEventW(None, true, false, PCWSTR::null()) } {
                let stop_handle_val = raw_stop_event.0 as usize;
                cache.stop_event = Some(HandleGuard(raw_stop_event));

                let scopes_w = Arc::clone(&scopes);
                let progids_w = Arc::clone(&progids);
                let blocked_w = Arc::clone(&blocked_clsids);
                let gen_w = Arc::clone(&generation);
                let stop_w = Arc::clone(&stop_flag);

                let handle = std::thread::Builder::new()
                    .name("rwm-reg-watch".to_string())
                    .spawn(move || {
                        watch_hkcu_classes(
                            HANDLE(stop_handle_val as *mut _),
                            stop_w,
                            scopes_w,
                            progids_w,
                            blocked_w,
                            gen_w,
                        );
                    })
                    .ok();
                *cache.watcher.lock() = handle;
            }
        }

        cache
    }

    pub fn request_stop_watcher(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(ref ev) = self.stop_event {
            unsafe {
                let _ = SetEvent(ev.0);
            }
        }
    }

    pub fn stop_watcher(&self) {
        self.request_stop_watcher();
        if let Some(handle) = self.watcher.lock().take() {
            let _ = handle.join();
        }
    }

    pub fn prewarm_hot_scopes(&self) {
        let hot = [
            RegistryScope::AllFiles,
            RegistryScope::AllFilesystemObjects,
            RegistryScope::Directory,
            RegistryScope::Folder,
            RegistryScope::DirectoryBackground,
            RegistryScope::DesktopBackground,
            RegistryScope::Drive,
        ];
        for scope in hot {
            let subkey = scope.registry_subkey();
            let gen_before = self.generation.load(Ordering::Acquire);
            let blocked = self.blocked_clsids.read().clone();
            let entry = scan_scope(&subkey, &blocked);
            let mut writer = self.scopes.write();
            if self.generation.load(Ordering::Acquire) == gen_before {
                writer.insert(subkey, entry);
            }
        }
    }

    pub fn invalidate_all(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.scopes.write().clear();
        self.progids.write().clear();
        *self.blocked_clsids.write() = load_blocked_clsids();
        self.prewarm_hot_scopes();
    }

    pub fn resolve_progid(&self, ext: &str) -> Option<String> {
        let ext_lower = ext.to_ascii_lowercase();
        if let Some(hit) = self.progids.read().get(&ext_lower) {
            return hit.clone();
        }
        let gen_before = self.generation.load(Ordering::Acquire);
        let progid = reg_read_string(HKEY_CLASSES_ROOT, &ext_lower, None)
            .filter(|s| !s.trim().is_empty());
        let mut writer = self.progids.write();
        if self.generation.load(Ordering::Acquire) == gen_before {
            writer.insert(ext_lower, progid.clone());
        }
        progid
    }

    pub fn get_scope_entries(&self, scope: &RegistryScope) -> ScopeEntries {
        let subkey = scope.registry_subkey();
        if let Some(hit) = self.scopes.read().get(&subkey) {
            return hit.clone();
        }
        let gen_before = self.generation.load(Ordering::Acquire);
        let blocked = self.blocked_clsids.read().clone();
        let scanned = scan_scope(&subkey, &blocked);
        let mut writer = self.scopes.write();
        if self.generation.load(Ordering::Acquire) == gen_before {
            writer.insert(subkey, scanned.clone());
        }
        scanned
    }

    pub fn collect_for_target(
        &self,
        target: &SelectionTarget,
    ) -> (Vec<StaticVerb>, Vec<DiscoveredComHandler>) {
        let progid = target
            .extension_lower()
            .and_then(|ext| self.resolve_progid(&ext));
        let scopes = target.registry_scopes(progid.as_deref());

        let mut verbs = Vec::new();
        let mut seen_verb_keys = HashSet::new();
        let mut handlers = Vec::new();
        let mut seen_clsids = HashSet::new();

        for scope in scopes {
            let entry = self.get_scope_entries(&scope);
            for verb in entry.static_verbs {
                let dedup_key = format!(
                    "{}:{}",
                    verb.key_name.to_ascii_lowercase(),
                    verb.label.to_ascii_lowercase()
                );
                if seen_verb_keys.insert(dedup_key) {
                    verbs.push(verb);
                }
            }
            for handler in entry.com_handlers {
                let clsid_key = handler.clsid.to_ascii_uppercase();
                if seen_clsids.insert(clsid_key) {
                    handlers.push(handler);
                }
            }
        }

        (verbs, handlers)
    }
}

impl Drop for RegistryVerbCache {
    fn drop(&mut self) {
        self.stop_watcher();
    }
}

fn watch_hkcu_classes(
    stop_event: HANDLE,
    stop_flag: Arc<AtomicBool>,
    scopes: Arc<RwLock<HashMap<String, ScopeEntries>>>,
    progids: Arc<RwLock<HashMap<String, Option<String>>>>,
    blocked_clsids: Arc<RwLock<HashSet<String>>>,
    generation: Arc<AtomicU64>,
) {
    let Ok(notify_event) = (unsafe { CreateEventW(None, true, false, PCWSTR::null()) }) else {
        return;
    };
    let _notify_guard = HandleGuard(notify_event);

    let mut watch_keys = Vec::new();
    if let Some(classes_key) = reg_open(HKEY_CURRENT_USER, r"Software\Classes", KEY_NOTIFY.0) {
        watch_keys.push(classes_key);
    }
    if let Some(hkcu_blocked) = reg_open(
        HKEY_CURRENT_USER,
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\Shell Extensions\Blocked",
        KEY_NOTIFY.0,
    ) {
        watch_keys.push(hkcu_blocked);
    }
    if let Some(hklm_blocked) = reg_open(
        HKEY_LOCAL_MACHINE,
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\Shell Extensions\Blocked",
        KEY_NOTIFY.0,
    ) {
        watch_keys.push(hklm_blocked);
    }
    if watch_keys.is_empty() {
        return;
    }

    while !stop_flag.load(Ordering::SeqCst) {
        unsafe {
            let _ = ResetEvent(notify_event);
        }
        let mut registered = false;
        for key in &watch_keys {
            let reg_res = unsafe {
                RegNotifyChangeKeyValue(
                    key.0,
                    true,
                    REG_NOTIFY_CHANGE_NAME | REG_NOTIFY_CHANGE_LAST_SET,
                    notify_event,
                    true,
                )
            };
            if reg_res == ERROR_SUCCESS {
                registered = true;
            }
        }
        if !registered {
            break;
        }

        let handles = [notify_event, stop_event];
        let wait = unsafe { WaitForMultipleObjects(&handles, false, u32::MAX) };
        if wait == WAIT_OBJECT_0 {
            if stop_flag.load(Ordering::SeqCst) {
                break;
            }
            // Coalesce burst writes briefly, bump generation, and clear caches so next read refreshes.
            std::thread::sleep(std::time::Duration::from_millis(120));
            generation.fetch_add(1, Ordering::AcqRel);
            scopes.write().clear();
            progids.write().clear();
            *blocked_clsids.write() = load_blocked_clsids();
        } else {
            break;
        }
    }
}

fn to_wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn reg_open(root: HKEY, subkey: &str, access: u32) -> Option<RegKeyGuard> {
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
        Some(RegKeyGuard(hkey))
    } else {
        None
    }
}

fn reg_value_exists(key: HKEY, value_name: &str) -> bool {
    let wide = to_wide_null(value_name);
    let res = unsafe {
        RegQueryValueExW(key, PCWSTR(wide.as_ptr()), None, None, None, None)
    };
    res == ERROR_SUCCESS
}

fn reg_read_string_from_hkey(key: HKEY, value_name: Option<&str>) -> Option<String> {
    let name_wide = value_name.map(to_wide_null);
    let name_ptr = name_wide
        .as_ref()
        .map_or(PCWSTR::null(), |v| PCWSTR(v.as_ptr()));
    let mut val_type = REG_VALUE_TYPE::default();
    let mut byte_len: u32 = 0;
    let res = unsafe {
        RegQueryValueExW(
            key,
            name_ptr,
            None,
            Some(&mut val_type),
            None,
            Some(&mut byte_len),
        )
    };
    if res == ERROR_FILE_NOT_FOUND || res != ERROR_SUCCESS || byte_len == 0 {
        return None;
    }
    let mut buf = vec![0u8; byte_len as usize];
    let res2 = unsafe {
        RegQueryValueExW(
            key,
            name_ptr,
            None,
            Some(&mut val_type),
            Some(buf.as_mut_ptr()),
            Some(&mut byte_len),
        )
    };
    if res2 != ERROR_SUCCESS {
        return None;
    }
    let u16_slice = unsafe {
        std::slice::from_raw_parts(buf.as_ptr() as *const u16, (byte_len as usize) / 2)
    };
    let end = u16_slice
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(u16_slice.len());
    Some(String::from_utf16_lossy(&u16_slice[..end]))
}

fn reg_read_string(root: HKEY, subkey: &str, value_name: Option<&str>) -> Option<String> {
    let guard = reg_open(root, subkey, KEY_READ.0)?;
    reg_read_string_from_hkey(guard.0, value_name)
}

fn reg_enum_subkeys(root: HKEY, subkey: &str) -> Vec<String> {
    let Some(guard) = reg_open(root, subkey, KEY_READ.0) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    let mut idx = 0u32;
    let mut name_buf = vec![0u16; 260];
    loop {
        let mut len = name_buf.len() as u32;
        let res = unsafe {
            RegEnumKeyExW(
                guard.0,
                idx,
                PWSTR(name_buf.as_mut_ptr()),
                &mut len,
                None,
                PWSTR::null(),
                None,
                None,
            )
        };
        if res == ERROR_SUCCESS {
            names.push(String::from_utf16_lossy(&name_buf[..len as usize]));
            idx += 1;
        } else if res == ERROR_MORE_DATA && name_buf.len() < 32768 {
            name_buf.resize(name_buf.len() * 2, 0);
        } else {
            // Stop on ERROR_NO_MORE_ITEMS or permanent failures such as ERROR_KEY_DELETED.
            break;
        }
    }
    names
}

fn reg_enum_value_names(root: HKEY, subkey: &str) -> Vec<String> {
    let Some(guard) = reg_open(root, subkey, KEY_READ.0) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    let mut idx = 0u32;
    let mut name_buf = vec![0u16; 260];
    loop {
        let mut len = name_buf.len() as u32;
        let res = unsafe {
            RegEnumValueW(
                guard.0,
                idx,
                PWSTR(name_buf.as_mut_ptr()),
                &mut len,
                None,
                None,
                None,
                None,
            )
        };
        if res == ERROR_SUCCESS {
            names.push(String::from_utf16_lossy(&name_buf[..len as usize]));
            idx += 1;
        } else if res == ERROR_MORE_DATA && name_buf.len() < 32768 {
            name_buf.resize(name_buf.len() * 2, 0);
        } else {
            break;
        }
    }
    names
}

fn load_blocked_clsids() -> HashSet<String> {
    let mut blocked = HashSet::new();
    let paths = [
        (
            HKEY_LOCAL_MACHINE,
            r"SOFTWARE\Microsoft\Windows\CurrentVersion\Shell Extensions\Blocked",
        ),
        (
            HKEY_CURRENT_USER,
            r"SOFTWARE\Microsoft\Windows\CurrentVersion\Shell Extensions\Blocked",
        ),
    ];
    for (root, path) in paths {
        for name in reg_enum_value_names(root, path) {
            if name.trim().starts_with('{') {
                blocked.insert(name.trim().to_ascii_uppercase());
            }
        }
    }
    blocked
}

fn resolve_mui_string(raw: &str) -> String {
    let trimmed = raw.trim();
    if !trimmed.starts_with('@') {
        return trimmed.to_string();
    }
    let src = to_wide_null(trimmed);
    let mut out = vec![0u16; 512];
    let hr = unsafe { SHLoadIndirectString(PCWSTR(src.as_ptr()), &mut out, None) };
    if hr.is_ok() {
        let end = out.iter().position(|&c| c == 0).unwrap_or(out.len());
        let resolved = String::from_utf16_lossy(&out[..end]);
        if !resolved.trim().is_empty() {
            return resolved;
        }
    }
    trimmed.to_string()
}

fn expand_env_vars(raw: &str) -> String {
    let wide = to_wide_null(raw);
    let mut buf = vec![0u16; 1024];
    let len = unsafe { ExpandEnvironmentStringsW(PCWSTR(wide.as_ptr()), Some(&mut buf)) };
    if len == 0 || (len as usize) > buf.len() {
        return raw.to_string();
    }
    String::from_utf16_lossy(&buf[..(len as usize).saturating_sub(1)])
}

fn scan_scope(scope_subkey: &str, blocked_clsids: &HashSet<String>) -> ScopeEntries {
    let shell_path = format!(r"{scope_subkey}\shell");
    let static_verbs = scan_shell_verbs(HKEY_CLASSES_ROOT, &shell_path, 0);

    let handlers_path = format!(r"{scope_subkey}\shellex\ContextMenuHandlers");
    let com_handlers = scan_com_handlers(&handlers_path, blocked_clsids);

    ScopeEntries {
        static_verbs,
        com_handlers,
    }
}

fn scan_shell_verbs(root: HKEY, shell_path: &str, depth: u8) -> Vec<StaticVerb> {
    if depth > 3 {
        return Vec::new();
    }
    let subkeys = reg_enum_subkeys(root, shell_path);
    let mut verbs = Vec::new();

    for key_name in subkeys {
        let verb_path = format!(r"{shell_path}\{key_name}");
        let Some(verb_key) = reg_open(root, &verb_path, KEY_READ.0) else {
            continue;
        };

        if reg_value_exists(verb_key.0, "LegacyDisable")
            || reg_value_exists(verb_key.0, "ProgrammaticAccessOnly")
        {
            continue;
        }

        let extended_only = reg_value_exists(verb_key.0, "Extended");
        let raw_label = reg_read_string_from_hkey(verb_key.0, Some("MUIVerb"))
            .or_else(|| reg_read_string_from_hkey(verb_key.0, None))
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| key_name.clone());
        let resolved_label = resolve_mui_string(&raw_label);
        let (label, mnemonic) = parse_mnemonic_label(&resolved_label);
        if label.trim().is_empty() {
            continue;
        }

        let icon_spec = reg_read_string_from_hkey(verb_key.0, Some("Icon"))
            .map(|s| expand_env_vars(&s))
            .filter(|s| !s.trim().is_empty());
        let position = reg_read_string_from_hkey(verb_key.0, Some("Position"));
        let canonical_verb = reg_read_string_from_hkey(verb_key.0, Some("CanonicalName"))
            .or_else(|| Some(key_name.clone()));

        let mut subcommands = Vec::new();
        if let Some(subcmd_list) = reg_read_string_from_hkey(verb_key.0, Some("SubCommands")) {
            if subcmd_list.trim().is_empty() {
                let nested_shell = format!(r"{verb_path}\shell");
                subcommands = scan_shell_verbs(root, &nested_shell, depth + 1);
            } else {
                for item in subcmd_list.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                    if item == "|" {
                        continue;
                    }
                    if let Some(cmd_verb) = lookup_command_store_verb(item, depth + 1) {
                        subcommands.push(cmd_verb);
                    }
                }
            }
        } else {
            let nested_shell = format!(r"{verb_path}\shell");
            if reg_open(root, &nested_shell, KEY_READ.0).is_some() {
                subcommands = scan_shell_verbs(root, &nested_shell, depth + 1);
            }
        }

        let command_path = format!(r"{verb_path}\command");
        let command = reg_read_string(root, &command_path, None)
            .map(|s| expand_env_vars(&s))
            .filter(|s| !s.trim().is_empty());

        let has_delegate = reg_read_string(root, &command_path, Some("DelegateExecute")).is_some();

        if command.is_none() && subcommands.is_empty() && !has_delegate {
            continue;
        }

        verbs.push(StaticVerb {
            key_name,
            label,
            mnemonic,
            icon_spec,
            command,
            extended_only,
            subcommands,
            canonical_verb,
            position,
        });
    }

    verbs
}

fn lookup_command_store_verb(name: &str, depth: u8) -> Option<StaticVerb> {
    if depth > 3 {
        return None;
    }
    let store_path =
        format!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\CommandStore\shell\{name}");
    let verb_key = reg_open(HKEY_LOCAL_MACHINE, &store_path, KEY_READ.0)?;
    if reg_value_exists(verb_key.0, "LegacyDisable")
        || reg_value_exists(verb_key.0, "ProgrammaticAccessOnly")
    {
        return None;
    }

    let raw_label = reg_read_string_from_hkey(verb_key.0, Some("MUIVerb"))
        .or_else(|| reg_read_string_from_hkey(verb_key.0, None))
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| name.to_string());
    let resolved_label = resolve_mui_string(&raw_label);
    let (label, mnemonic) = parse_mnemonic_label(&resolved_label);
    if label.trim().is_empty() {
        return None;
    }

    let icon_spec = reg_read_string_from_hkey(verb_key.0, Some("Icon"))
        .map(|s| expand_env_vars(&s))
        .filter(|s| !s.trim().is_empty());
    let command_path = format!(r"{store_path}\command");
    let command = reg_read_string(HKEY_LOCAL_MACHINE, &command_path, None)
        .map(|s| expand_env_vars(&s))
        .filter(|s| !s.trim().is_empty());
    let subcommands = scan_shell_verbs(HKEY_LOCAL_MACHINE, &format!(r"{store_path}\shell"), depth + 1);

    if command.is_none() && subcommands.is_empty() {
        return None;
    }

    Some(StaticVerb {
        key_name: name.to_string(),
        label,
        mnemonic,
        icon_spec,
        command,
        extended_only: false,
        subcommands,
        canonical_verb: Some(name.to_string()),
        position: None,
    })
}

fn looks_like_clsid(s: &str) -> bool {
    let t = s.trim();
    t.starts_with('{') && t.ends_with('}') && t.len() >= 38
}

fn scan_com_handlers(
    handlers_path: &str,
    blocked_clsids: &HashSet<String>,
) -> Vec<DiscoveredComHandler> {
    let subkeys = reg_enum_subkeys(HKEY_CLASSES_ROOT, handlers_path);
    let mut handlers = Vec::new();

    for sub in subkeys {
        let full = format!(r"{handlers_path}\{sub}");
        let default_val = reg_read_string(HKEY_CLASSES_ROOT, &full, None).unwrap_or_default();
        let default_trim = default_val.trim();
        let sub_trim = sub.trim();

        let (name, clsid) = if looks_like_clsid(default_trim) {
            (sub_trim.to_string(), default_trim.to_string())
        } else if looks_like_clsid(sub_trim) {
            let label = if !default_trim.is_empty() {
                default_trim.to_string()
            } else {
                sub_trim.to_string()
            };
            (label, sub_trim.to_string())
        } else {
            continue;
        };

        let clsid_upper = clsid.to_ascii_uppercase();
        if blocked_clsids.contains(&clsid_upper) {
            continue;
        }

        let inproc_path = format!(r"CLSID\{clsid}\InprocServer32");
        let dll_path = reg_read_string(HKEY_CLASSES_ROOT, &inproc_path, None)
            .map(|p| expand_env_vars(&p))
            .filter(|p| !p.trim().is_empty());

        handlers.push(DiscoveredComHandler {
            name,
            clsid,
            dll_path,
        });
    }

    handlers
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn registry_cache_prewarms_and_collects_real_windows_handlers() {
        let cache = RegistryVerbCache::new(false);
        let file_target = SelectionTarget::SingleFile(PathBuf::from(r"C:\Windows\notepad.exe"));
        let (_verbs, handlers) = cache.collect_for_target(&file_target);
        assert!(
            !handlers.is_empty(),
            "expected at least one COM ContextMenuHandler registered for *"
        );

        let bg_target = SelectionTarget::FolderBackground(PathBuf::from(r"C:\Windows"));
        let (_bg_verbs, bg_handlers) = cache.collect_for_target(&bg_target);
        assert!(
            !bg_handlers.is_empty(),
            "expected at least one COM handler for Directory\\Background"
        );
    }
}
