use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionTarget {
    SingleFile(PathBuf),
    SingleFolder(PathBuf),
    MultipleItems {
        parent: PathBuf,
        items: Vec<PathBuf>,
    },
    FolderBackground(PathBuf),
    DesktopBackground(PathBuf),
    Drive(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RegistryScope {
    Extension(String),
    ProgId(String),
    SystemFileAssocExt(String),
    AllFiles,
    AllFilesystemObjects,
    Directory,
    Folder,
    Drive,
    DirectoryBackground,
    DesktopBackground,
}

impl RegistryScope {
    pub fn registry_subkey(&self) -> String {
        match self {
            RegistryScope::Extension(ext) => ext.clone(),
            RegistryScope::ProgId(prog) => prog.clone(),
            RegistryScope::SystemFileAssocExt(ext) => {
                format!(r"SystemFileAssociations\{ext}")
            }
            RegistryScope::AllFiles => "*".to_string(),
            RegistryScope::AllFilesystemObjects => "AllFilesystemObjects".to_string(),
            RegistryScope::Directory => "Directory".to_string(),
            RegistryScope::Folder => "Folder".to_string(),
            RegistryScope::Drive => "Drive".to_string(),
            RegistryScope::DirectoryBackground => r"Directory\Background".to_string(),
            RegistryScope::DesktopBackground => r"DesktopBackground".to_string(),
        }
    }
}

impl SelectionTarget {
    pub fn from_path(path: impl AsRef<Path>, background: bool) -> Self {
        let p = path.as_ref().to_path_buf();
        if background {
            if is_desktop_path(&p) {
                return SelectionTarget::DesktopBackground(p);
            }
            return SelectionTarget::FolderBackground(p);
        }
        if is_drive_root(&p) {
            return SelectionTarget::Drive(p);
        }
        if p.is_dir() {
            SelectionTarget::SingleFolder(p)
        } else {
            SelectionTarget::SingleFile(p)
        }
    }

    pub fn from_paths(items: Vec<PathBuf>) -> Option<Self> {
        if items.is_empty() {
            return None;
        }
        if items.len() == 1 {
            let only = items.into_iter().next()?;
            return Some(Self::from_path(only, false));
        }
        let parent = items[0]
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        Some(SelectionTarget::MultipleItems { parent, items })
    }

    pub fn primary_path(&self) -> &Path {
        match self {
            SelectionTarget::SingleFile(p)
            | SelectionTarget::SingleFolder(p)
            | SelectionTarget::FolderBackground(p)
            | SelectionTarget::DesktopBackground(p)
            | SelectionTarget::Drive(p) => p.as_path(),
            SelectionTarget::MultipleItems { items, parent } => {
                items.first().map(PathBuf::as_path).unwrap_or(parent.as_path())
            }
        }
    }

    pub fn parent_dir(&self) -> PathBuf {
        match self {
            SelectionTarget::FolderBackground(p)
            | SelectionTarget::DesktopBackground(p)
            | SelectionTarget::Drive(p) => p.clone(),
            SelectionTarget::MultipleItems { parent, .. } => parent.clone(),
            SelectionTarget::SingleFile(p) | SelectionTarget::SingleFolder(p) => {
                p.parent().map(Path::to_path_buf).unwrap_or_else(|| p.clone())
            }
        }
    }

    pub fn selected_paths(&self) -> Vec<PathBuf> {
        match self {
            SelectionTarget::SingleFile(p)
            | SelectionTarget::SingleFolder(p)
            | SelectionTarget::Drive(p) => vec![p.clone()],
            SelectionTarget::MultipleItems { items, .. } => items.clone(),
            SelectionTarget::FolderBackground(p) | SelectionTarget::DesktopBackground(p) => {
                vec![p.clone()]
            }
        }
    }

    pub fn extension_lower(&self) -> Option<String> {
        match self {
            SelectionTarget::SingleFile(p) => p
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| format!(".{}", e.to_ascii_lowercase())),
            SelectionTarget::MultipleItems { items, .. } => {
                let first_ext = items
                    .first()
                    .and_then(|p| p.extension())
                    .and_then(|e| e.to_str())
                    .map(|e| format!(".{}", e.to_ascii_lowercase()))?;
                if items.iter().all(|p| {
                    p.extension()
                        .and_then(|e| e.to_str())
                        .map(|e| format!(".{}", e.to_ascii_lowercase()))
                        .as_deref()
                        == Some(first_ext.as_str())
                }) {
                    Some(first_ext)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    pub fn is_background(&self) -> bool {
        matches!(
            self,
            SelectionTarget::FolderBackground(_) | SelectionTarget::DesktopBackground(_)
        )
    }

    pub fn registry_scopes(&self, prog_id: Option<&str>) -> Vec<RegistryScope> {
        let mut scopes = Vec::new();
        match self {
            SelectionTarget::SingleFile(_) => {
                if let Some(ext) = self.extension_lower() {
                    scopes.push(RegistryScope::Extension(ext.clone()));
                    if let Some(pid) = prog_id {
                        if !pid.is_empty() {
                            scopes.push(RegistryScope::ProgId(pid.to_string()));
                        }
                    }
                    scopes.push(RegistryScope::SystemFileAssocExt(ext));
                }
                scopes.push(RegistryScope::AllFiles);
                scopes.push(RegistryScope::AllFilesystemObjects);
            }
            SelectionTarget::SingleFolder(_) => {
                scopes.push(RegistryScope::Directory);
                scopes.push(RegistryScope::Folder);
                scopes.push(RegistryScope::AllFilesystemObjects);
            }
            SelectionTarget::Drive(_) => {
                scopes.push(RegistryScope::Drive);
                scopes.push(RegistryScope::Folder);
                scopes.push(RegistryScope::AllFilesystemObjects);
            }
            SelectionTarget::MultipleItems { items, .. } => {
                let any_dir = items.iter().any(|p| p.is_dir());
                let all_dirs = items.iter().all(|p| p.is_dir());
                if !any_dir {
                    if let Some(ext) = self.extension_lower() {
                        scopes.push(RegistryScope::Extension(ext.clone()));
                        if let Some(pid) = prog_id {
                            if !pid.is_empty() {
                                scopes.push(RegistryScope::ProgId(pid.to_string()));
                            }
                        }
                        scopes.push(RegistryScope::SystemFileAssocExt(ext));
                    }
                    scopes.push(RegistryScope::AllFiles);
                } else if all_dirs {
                    scopes.push(RegistryScope::Directory);
                    scopes.push(RegistryScope::Folder);
                }
                scopes.push(RegistryScope::AllFilesystemObjects);
            }
            SelectionTarget::FolderBackground(_) => {
                scopes.push(RegistryScope::DirectoryBackground);
            }
            SelectionTarget::DesktopBackground(_) => {
                scopes.push(RegistryScope::DesktopBackground);
                scopes.push(RegistryScope::DirectoryBackground);
            }
        }
        scopes
    }
}

fn is_drive_root(path: &Path) -> bool {
    let s = path.to_string_lossy();
    let bytes = s.as_bytes();
    (bytes.len() == 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic())
        || (bytes.len() == 3
            && bytes[1] == b':'
            && (bytes[2] == b'\\' || bytes[2] == b'/')
            && bytes[0].is_ascii_alphabetic())
}

fn is_desktop_path(path: &Path) -> bool {
    if let Ok(user_profile) = std::env::var("USERPROFILE") {
        let desktop = PathBuf::from(user_profile).join("Desktop");
        if path.eq_ignore_ascii_case(&desktop) {
            return true;
        }
    }
    false
}

trait PathAsciiExt {
    fn eq_ignore_ascii_case(&self, other: &Path) -> bool;
}

impl PathAsciiExt for Path {
    fn eq_ignore_ascii_case(&self, other: &Path) -> bool {
        self.to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .eq_ignore_ascii_case(other.to_string_lossy().trim_end_matches(['\\', '/']))
    }
}

#[derive(Debug, Clone)]
pub struct BgraIcon {
    pub width: u32,
    pub height: u32,
    /// Premultiplied BGRA32 pixel data.
    pub pixels: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntrySource {
    Core,
    StaticVerb,
    Custom,
    ComHandler { clsid: String, name: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreVerb {
    Open,
    OpenInTerminal,
    Cut,
    Copy,
    CopyPath,
    Paste,
    PasteShortcut,
    CreateShortcut,
    Delete,
    Rename,
    Refresh,
    Properties,
}

#[derive(Debug, Clone)]
pub enum EntryAction {
    CoreVerb(CoreVerb),
    StaticCommand {
        verb: String,
        command: String,
        working_dir: PathBuf,
    },
    CustomCommand {
        command: String,
        args: Vec<String>,
        working_dir: PathBuf,
    },
    ComCommand {
        session_id: u64,
        handler_index: usize,
        cmd_offset: u32,
    },
    Submenu(Vec<MenuEntry>),
    Noop,
}

#[derive(Debug, Clone)]
pub struct MenuItem {
    pub id: u32,
    pub label: String,
    pub mnemonic: Option<char>,
    pub shortcut_hint: Option<String>,
    pub icon: Option<Arc<BgraIcon>>,
    pub enabled: bool,
    pub checked: bool,
    pub is_default: bool,
    pub source: EntrySource,
    pub action: EntryAction,
}

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant, reason = "Preserve the public MenuEntry::Item(MenuItem) type")]
pub enum MenuEntry {
    Item(MenuItem),
    Separator,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HandlerPolicy {
    #[default]
    Inline,
    Submenu,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "detail", rename_all = "snake_case")]
pub enum HandlerStatus {
    Ok,
    Empty,
    SkippedPolicy,
    Failed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandlerTiming {
    pub clsid: String,
    pub name: String,
    pub dll_path: Option<String>,
    pub duration_us: u64,
    pub item_count: usize,
    pub status: HandlerStatus,
}

#[derive(Debug, Clone)]
pub struct MenuSnapshot {
    pub session_id: u64,
    pub target: SelectionTarget,
    pub entries: Vec<MenuEntry>,
    pub stage0_us: u64,
    pub pending_handlers: usize,
}

#[derive(Debug, Clone)]
pub enum HydrationEvent {
    GroupReady {
        session_id: u64,
        group_name: String,
        clsid: String,
        policy: HandlerPolicy,
        entries: Vec<MenuEntry>,
        timing: HandlerTiming,
    },
    Completed {
        session_id: u64,
        total_us: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkReport {
    pub target_display: String,
    pub is_background: bool,
    pub stage0_us: u64,
    #[serde(default)]
    pub stage0_cold_us: u64,
    pub stage0_item_count: usize,
    pub com_handlers: Vec<HandlerTiming>,
    pub total_com_us: u64,
    pub speedup_factor: f64,
}

/// Strips `&` accelerator markers while keeping `&&` as literal `&` and returning the first mnemonic char.
pub fn parse_mnemonic_label(raw: &str) -> (String, Option<char>) {
    let mut clean = String::with_capacity(raw.len());
    let mut mnemonic = None;
    let mut chars = raw.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '&' {
            if let Some(&next) = chars.peek() {
                if next == '&' {
                    clean.push('&');
                    chars.next();
                } else if mnemonic.is_none() && !next.is_whitespace() {
                    mnemonic = Some(next.to_ascii_uppercase());
                }
                continue;
            }
        }
        clean.push(c);
    }

    // Strip trailing Win32 tab shortcut if embedded (e.g. "Copy\tCtrl+C")
    let clean_label = if let Some((left, _right)) = clean.split_once('\t') {
        left.trim_end().to_string()
    } else {
        clean
    };

    (clean_label, mnemonic)
}

pub fn split_label_and_shortcut(raw: &str) -> (String, Option<char>, Option<String>) {
    let (main_part, shortcut) = match raw.split_once('\t') {
        Some((left, right)) => {
            let trimmed = right.trim();
            let sc = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            };
            (left, sc)
        }
        None => (raw, None),
    };
    let (label, mnemonic) = parse_mnemonic_label(main_part);
    (label, mnemonic, shortcut)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mnemonic_and_tab_shortcut() {
        let (label, mnemonic, shortcut) = split_label_and_shortcut("&Copy\tCtrl+C");
        assert_eq!(label, "Copy");
        assert_eq!(mnemonic, Some('C'));
        assert_eq!(shortcut.as_deref(), Some("Ctrl+C"));

        let (amp_label, amp_mnemonic, amp_sc) = split_label_and_shortcut("Save && &Exit");
        assert_eq!(amp_label, "Save & Exit");
        assert_eq!(amp_mnemonic, Some('E'));
        assert_eq!(amp_sc, None);
    }

    #[test]
    fn selection_target_scopes_cover_expected_keys() {
        let target = SelectionTarget::SingleFile(PathBuf::from(r"C:\tmp\demo.TXT"));
        let scopes = target.registry_scopes(Some("txtfile"));
        assert_eq!(
            scopes,
            vec![
                RegistryScope::Extension(".txt".into()),
                RegistryScope::ProgId("txtfile".into()),
                RegistryScope::SystemFileAssocExt(".txt".into()),
                RegistryScope::AllFiles,
                RegistryScope::AllFilesystemObjects,
            ]
        );
    }
}
