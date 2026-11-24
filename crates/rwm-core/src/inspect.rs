use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, HINSTANCE, HWND};
use windows::Win32::Storage::Packaging::Appx::{
    GetPackagePathByFullName2, PackagePathType_Install,
};
use windows::Win32::System::Com::{CoCreateInstance, IDataObject, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows::Win32::System::Registry::{HKEY, HKEY_CLASSES_ROOT, HKEY_CURRENT_USER};
use windows::Win32::UI::Shell::{IContextMenu, IShellExtInit, SHCreateDataObject};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, HMENU, WS_OVERLAPPED,
};
use windows_core::{w, IUnknown, Interface, PCWSTR, PWSTR};

use crate::config::RwmConfig;
use crate::handlers::{handler_name, is_blocked};
use crate::reg_util::{
    enum_subkeys, format_clsid, open_key_read, parse_clsid, read_string_value, wide_null,
    RegKeyGuard,
};
use crate::shell::{
    bind_folder, InspectedMenuItem, NativeMenu, OleGuard, SelectionKey, ShellError,
};

const PACKAGES: &str = r"Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppModel\Repository\Packages";
const SLOW_MS: f64 = 30.0;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackagedVerbInfo {
    pub package_full_name: String,
    pub item_type: String,
    pub verb_id: String,
    pub clsid: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HandlerPhases {
    pub co_create_ms: Option<f64>,
    pub initialize_ms: Option<f64>,
    pub query_context_menu_ms: Option<f64>,
    pub read_root_ms: Option<f64>,
    pub submenu_init_ms: Option<f64>,
    pub root_item_count: usize,
    pub error: Option<String>,
}

impl HandlerPhases {
    fn root_ms(&self) -> Option<f64> {
        Some(
            self.co_create_ms?
                + self.initialize_ms.unwrap_or_default()
                + self.query_context_menu_ms.unwrap_or_default(),
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandlerTiming {
    pub clsid: String,
    pub name: String,
    pub dll_path: Option<PathBuf>,
    pub scopes: Vec<String>,
    pub blocked: bool,
    pub cold: Option<HandlerPhases>,
    pub warm: Option<HandlerPhases>,
    pub warm_ms: Option<f64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InspectReport {
    pub target: PathBuf,
    pub is_background: bool,
    pub extended: bool,
    pub root_items: Vec<InspectedMenuItem>,
    pub cold_query_ms: f64,
    pub warm_query_ms: f64,
    pub handlers: Vec<HandlerTiming>,
    pub slow_culprits: Vec<HandlerTiming>,
    pub packaged_verbs: Vec<PackagedVerbInfo>,
    pub packaged_scan_errors: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SampleStats {
    pub min_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub max_ms: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PerceivedLatency {
    pub hold_ms: u32,
    pub p50_ms: f64,
    pub p95_ms: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BenchReport {
    pub target: PathBuf,
    pub is_background: bool,
    pub warm_runs: usize,
    pub native_cold_query_ms: f64,
    pub native_cold_root_ms: f64,
    pub native_warm_query: SampleStats,
    pub native_warm_root: SampleStats,
    pub eager_submenu_ms: f64,
    pub eager_submenu_init_ms: f64,
    pub handlers: Vec<HandlerTiming>,
    pub msix_count: usize,
    pub packaged_scan_errors: Vec<String>,
    pub simulated_latency: Vec<PerceivedLatency>,
}

#[derive(Clone, Copy)]
enum TargetKind {
    File,
    Directory,
    DirectoryBackground,
    Drive,
}

struct Target {
    path: PathBuf,
    kind: TargetKind,
    extension: Option<String>,
}

impl Target {
    fn new(path: &Path, background: bool) -> Result<Self, ShellError> {
        let path = std::path::absolute(path)?;
        let directory = fs::metadata(&path)?.is_dir();
        let kind = match (directory, background) {
            (true, true) => TargetKind::DirectoryBackground,
            (true, false) if path.parent().is_none() => TargetKind::Drive,
            (true, false) => TargetKind::Directory,
            (false, true) => return Err(ShellError::InvalidTarget),
            (false, false) => TargetKind::File,
        };
        let extension = if matches!(kind, TargetKind::File) {
            path.extension()
                .and_then(|ext| ext.to_str())
                .filter(|ext| !ext.is_empty())
                .map(|ext| format!(".{}", ext.to_ascii_lowercase()))
        } else {
            None
        };
        Ok(Self {
            path,
            kind,
            extension,
        })
    }

    fn is_background(&self) -> bool {
        matches!(self.kind, TargetKind::DirectoryBackground)
    }

    fn matches_item_type(&self, item_type: &str) -> bool {
        let scope = match self.kind {
            TargetKind::File => "*",
            TargetKind::Directory => "Directory",
            TargetKind::DirectoryBackground => r"Directory\Background",
            TargetKind::Drive => "Drive",
        };
        scope.eq_ignore_ascii_case(item_type)
            || self
                .extension
                .as_deref()
                .is_some_and(|ext| ext.eq_ignore_ascii_case(item_type))
    }

    fn progids(&self) -> Result<Vec<String>, ShellError> {
        let Some(ext) = &self.extension else {
            return Ok(Vec::new());
        };
        let user_choice = format!(
            r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\{ext}\UserChoice"
        );
        let user = value(HKEY_CURRENT_USER, &user_choice, Some("ProgId"))?;
        let system = value(HKEY_CLASSES_ROOT, ext, None)?;
        Ok(user
            .into_iter()
            .chain(system)
            .filter(|id| !id.is_empty())
            .collect())
    }

    fn handler_scopes(&self, progids: &[String]) -> Vec<String> {
        let mut scopes = Vec::new();
        match self.kind {
            TargetKind::File => {
                scopes.extend_from_slice(progids);
                if let Some(extension) = &self.extension {
                    scopes.push(extension.clone());
                    scopes.push(format!(r"SystemFileAssociations\{extension}"));
                }
                scopes.extend(["*".into(), "AllFilesystemObjects".into()]);
            }
            TargetKind::Directory => {
                scopes.extend([
                    "Directory".into(),
                    "Folder".into(),
                    "AllFilesystemObjects".into(),
                ]);
            }
            TargetKind::Drive => {
                scopes.extend([
                    "Drive".into(),
                    "Directory".into(),
                    "Folder".into(),
                    "AllFilesystemObjects".into(),
                ]);
            }
            TargetKind::DirectoryBackground => scopes.push(r"Directory\Background".into()),
        }
        let mut seen = HashSet::new();
        scopes.retain(|scope| seen.insert(scope.to_ascii_lowercase()));
        scopes
    }

    fn class_key(&self, progids: &[String]) -> Result<Option<RegKeyGuard>, ShellError> {
        let base = match self.kind {
            TargetKind::File => self.extension.as_deref().unwrap_or("*"),
            TargetKind::Directory => "Directory",
            TargetKind::Drive => "Drive",
            TargetKind::DirectoryBackground => return Ok(None),
        };
        for class in progids
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(base))
        {
            if let Some(key) = open_key_read(HKEY_CLASSES_ROOT, class)? {
                return Ok(Some(key));
            }
        }
        Ok(None)
    }

    fn native_menu(&self, owner: HWND, extended: bool) -> Result<NativeMenu, ShellError> {
        NativeMenu::build_from_paths(
            owner,
            std::slice::from_ref(&self.path),
            self.is_background(),
            extended,
        )
    }
}

fn value(root: HKEY, path: &str, name: Option<&str>) -> std::io::Result<Option<String>> {
    let Some(key) = open_key_read(root, path)? else {
        return Ok(None);
    };
    read_string_value(&key, name)
}

fn expand(raw: &str) -> PathBuf {
    let wide = wide_null(raw);
    let len = unsafe { ExpandEnvironmentStringsW(PCWSTR(wide.as_ptr()), None) } as usize;
    if len == 0 {
        return raw.into();
    }
    let mut buffer = vec![0; len];
    let written =
        unsafe { ExpandEnvironmentStringsW(PCWSTR(wide.as_ptr()), Some(&mut buffer)) } as usize;
    if written == 0 || written > buffer.len() {
        return raw.into();
    }
    OsString::from_wide(&buffer[..written - 1]).into()
}

fn installed_package_manifest(package: &str) -> Option<PathBuf> {
    let full_name = wide_null(package);
    let name = PCWSTR(full_name.as_ptr());
    let mut len = 0;
    if unsafe { GetPackagePathByFullName2(name, PackagePathType_Install, &mut len, PWSTR::null()) }
        != ERROR_INSUFFICIENT_BUFFER
    {
        return None;
    }
    let mut path = vec![0; len as usize];
    let status = unsafe {
        GetPackagePathByFullName2(
            name,
            PackagePathType_Install,
            &mut len,
            PWSTR(path.as_mut_ptr()),
        )
    };
    (status == ERROR_SUCCESS && len > 1 && len as usize <= path.len()).then(|| {
        PathBuf::from(OsString::from_wide(&path[..len as usize - 1])).join("AppxManifest.xml")
    })
}

fn parse_manifest(
    raw: &str,
    package: &str,
    target: &Target,
) -> Result<Vec<PackagedVerbInfo>, roxmltree::Error> {
    let document = roxmltree::Document::parse(raw)?;
    let mut verbs = Vec::new();
    let is_element = |node: roxmltree::Node<'_, '_>, local| {
        node.has_tag_name(local)
            && node.tag_name().namespace().is_some_and(|ns| {
                ns.starts_with("http://schemas.microsoft.com/appx/manifest/desktop/windows10/")
            })
    };
    for extension in document.descendants().filter(|&node| {
        is_element(node, "Extension")
            && node.attribute("Category") == Some("windows.fileExplorerContextMenus")
    }) {
        for menu in extension
            .children()
            .filter(|&node| is_element(node, "FileExplorerContextMenus"))
        {
            for item in menu.children().filter(|&node| is_element(node, "ItemType")) {
                let Some(item_type) = item
                    .attribute("Type")
                    .filter(|kind| target.matches_item_type(kind))
                else {
                    continue;
                };
                for verb in item.children().filter(|&node| is_element(node, "Verb")) {
                    let (Some(id), Some(clsid)) = (verb.attribute("Id"), verb.attribute("Clsid"))
                    else {
                        continue;
                    };
                    let Ok(guid) = parse_clsid(clsid) else {
                        continue;
                    };
                    verbs.push(PackagedVerbInfo {
                        package_full_name: package.to_owned(),
                        item_type: item_type.to_owned(),
                        verb_id: id.to_owned(),
                        clsid: format_clsid(&guid),
                    });
                }
            }
        }
    }
    Ok(verbs)
}

fn decode_manifest(bytes: &[u8]) -> std::io::Result<std::borrow::Cow<'_, str>> {
    let invalid = |error: &dyn std::error::Error| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
    };
    if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) {
        let mut chunks = bytes[2..].chunks_exact(2);
        let big_endian = bytes[0] == 0xfe;
        let units = chunks
            .by_ref()
            .map(|pair| {
                let bytes = [pair[0], pair[1]];
                if big_endian {
                    u16::from_be_bytes(bytes)
                } else {
                    u16::from_le_bytes(bytes)
                }
            })
            .collect::<Vec<_>>();
        if !chunks.remainder().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "odd-length UTF-16 manifest",
            ));
        }
        return String::from_utf16(&units)
            .map(Into::into)
            .map_err(|error| invalid(&error));
    }
    std::str::from_utf8(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes))
        .map(Into::into)
        .map_err(|error| invalid(&error))
}

fn packaged_verbs(target: &Target) -> Result<(Vec<PackagedVerbInfo>, Vec<String>), ShellError> {
    let Some(root) = open_key_read(HKEY_CURRENT_USER, PACKAGES)? else {
        return Ok((Vec::new(), Vec::new()));
    };
    let mut verbs = Vec::new();
    let mut errors = Vec::new();
    for package in enum_subkeys(&root)? {
        let bytes = value(root.0, &package, Some("PackageRootFolder")).and_then(|path| {
            let registered = path
                .filter(|path| !path.is_empty())
                .map(|path| expand(&path).join("AppxManifest.xml"));
            let result = registered.as_ref().map_or_else(
                || {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "missing PackageRootFolder",
                    ))
                },
                fs::read,
            );
            result.or_else(|error| {
                // An app's external payload directory can differ from its installed manifest directory.
                match installed_package_manifest(&package) {
                    Some(installed) if Some(&installed) != registered.as_ref() => {
                        fs::read(installed)
                    }
                    _ => Err(error),
                }
            })
        });
        let result = bytes.and_then(|bytes| {
            let raw = decode_manifest(&bytes)?;
            parse_manifest(&raw, &package, target).map_err(std::io::Error::other)
        });
        match result {
            Ok(found) => verbs.extend(found),
            Err(error) => errors.push(format!("{package}: {error}")),
        }
    }
    verbs.sort_by(|a, b| {
        (&a.package_full_name, &a.item_type, &a.verb_id).cmp(&(
            &b.package_full_name,
            &b.item_type,
            &b.verb_id,
        ))
    });
    Ok((verbs, errors))
}

struct HandlerRegistration {
    clsid: String,
    name: String,
    scopes: Vec<String>,
}

fn discover_handlers(
    target: &Target,
    progids: &[String],
) -> Result<Vec<HandlerRegistration>, ShellError> {
    let mut handlers: Vec<HandlerRegistration> = Vec::new();
    let mut by_clsid: HashMap<String, usize> = HashMap::new();
    for scope in target.handler_scopes(progids) {
        let path = format!(r"{scope}\shellex\ContextMenuHandlers");
        let Some(root) = open_key_read(HKEY_CLASSES_ROOT, &path)? else {
            continue;
        };
        for name in enum_subkeys(&root)? {
            let raw = value(root.0, &name, None)?;
            let guid = raw
                .as_deref()
                .and_then(|raw| parse_clsid(raw).ok())
                .or_else(|| parse_clsid(&name).ok());
            let Some(guid) = guid else { continue };
            let clsid = format_clsid(&guid);
            if let Some(&index) = by_clsid.get(&clsid) {
                if !handlers[index].scopes.contains(&scope) {
                    handlers[index].scopes.push(scope.clone());
                }
            } else {
                by_clsid.insert(clsid.clone(), handlers.len());
                handlers.push(HandlerRegistration {
                    clsid,
                    name,
                    scopes: vec![scope.clone()],
                });
            }
        }
    }
    Ok(handlers)
}

fn selection_data(
    owner: HWND,
    selection: &SelectionKey,
) -> Result<Option<IDataObject>, ShellError> {
    if selection.is_background() {
        return Ok(None);
    }
    let folder = bind_folder(selection.folder_pidl())?;
    let children = selection.child_pidls()?;
    let data = unsafe {
        folder.GetUIObjectOf(owner, &children, None).or_else(|_| {
            SHCreateDataObject(
                Some(selection.folder_pidl().as_ptr()),
                Some(&children),
                None,
            )
        })?
    };
    Ok(Some(data))
}

fn timed<T>(field: &mut Option<f64>, f: impl FnOnce() -> T) -> T {
    let start = Instant::now();
    let result = f();
    *field = Some(ms(start));
    result
}

fn ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

fn measure_handler(
    clsid: &windows_core::GUID,
    selection: &SelectionKey,
    association: HKEY,
    data: Option<&IDataObject>,
) -> HandlerPhases {
    let mut phases = HandlerPhases::default();
    let result = (|| -> Result<(), ShellError> {
        let object: IUnknown = timed(&mut phases.co_create_ms, || unsafe {
            CoCreateInstance(clsid, None, CLSCTX_INPROC_SERVER)
        })
        .map_err(|e| ShellError::WorkerFailed(format!("CoCreateInstance: {e}")))?;
        if let Ok(init) = object.cast::<IShellExtInit>() {
            let folder = selection
                .is_background()
                .then(|| selection.folder_pidl().as_ptr());
            timed(&mut phases.initialize_ms, || unsafe {
                init.Initialize(folder, data, association)
            })
            .map_err(|e| ShellError::WorkerFailed(format!("IShellExtInit::Initialize: {e}")))?;
        }
        let menu: IContextMenu = object.cast()?;
        let selection = selection.clone();
        let menu = timed(&mut phases.query_context_menu_ms, || {
            NativeMenu::query(menu, selection)
        })?;
        phases.query_context_menu_ms = Some(menu.build_us() as f64 / 1000.0);
        phases.root_item_count =
            timed(&mut phases.read_root_ms, || menu.extract_items(false)).len();
        phases.submenu_init_ms = Some(menu.inspect_submenus().1);
        Ok(())
    })();
    phases.error = result.err().map(|error| error.to_string());
    phases
}

fn profile_handlers(
    owner: HWND,
    target: &Target,
    extended: bool,
) -> Result<Vec<HandlerTiming>, ShellError> {
    let paths = std::slice::from_ref(&target.path);
    let selection = SelectionKey::from_paths(paths, target.is_background(), extended)?;
    let progids = target.progids()?;
    let association = target.class_key(&progids)?;
    let association = association.as_ref().map_or_else(HKEY::default, |key| key.0);
    let data = selection_data(owner, &selection)?;
    let mut results = Vec::new();
    for registration in discover_handlers(target, &progids)? {
        let HandlerRegistration {
            clsid,
            name,
            scopes,
        } = registration;
        let blocked = is_blocked(&clsid)?;
        let dll_key = format!(r"CLSID\{clsid}\InprocServer32");
        let dll_path = value(HKEY_CLASSES_ROOT, &dll_key, None)?.map(|raw| expand(&raw));
        let name = handler_name(&clsid)?
            .filter(|name| !name.is_empty())
            .unwrap_or(name);
        let (mut cold, mut warm) = (None, None);
        if !blocked {
            let guid = parse_clsid(&clsid)?;
            cold = Some(measure_handler(
                &guid,
                &selection,
                association,
                data.as_ref(),
            ));
            warm = Some(measure_handler(
                &guid,
                &selection,
                association,
                data.as_ref(),
            ));
        }
        let warm_ms = warm.as_ref().and_then(HandlerPhases::root_ms);
        results.push(HandlerTiming {
            clsid,
            name,
            dll_path,
            scopes,
            blocked,
            cold,
            warm,
            warm_ms,
        });
    }
    Ok(results)
}

struct DiagnosticWindow(HWND);

impl Drop for DiagnosticWindow {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.0);
        }
    }
}

fn on_sta<T: Send + 'static>(
    run: impl FnOnce(HWND) -> Result<T, ShellError> + Send + 'static,
) -> Result<T, ShellError> {
    std::thread::Builder::new()
        .name("rwm-inspect-sta".into())
        .spawn(move || {
            let _apartment = OleGuard::initialize()?;
            let owner = DiagnosticWindow(unsafe {
                CreateWindowExW(
                    Default::default(),
                    w!("STATIC"),
                    PCWSTR::null(),
                    WS_OVERLAPPED,
                    0,
                    0,
                    1,
                    1,
                    HWND::default(),
                    HMENU::default(),
                    HINSTANCE::default(),
                    None,
                )?
            });
            run(owner.0)
        })?
        .join()
        .map_err(|_| ShellError::WorkerFailed("STA thread panicked".into()))?
}

pub fn inspect_target(
    path: &Path,
    is_background: bool,
    extended: bool,
) -> Result<InspectReport, ShellError> {
    let target = Target::new(path, is_background)?;
    on_sta(move |owner| {
        let cold = target.native_menu(owner, extended)?;
        let cold_query_ms = cold.build_us() as f64 / 1000.0;
        drop(cold);
        let warm = target.native_menu(owner, extended)?;
        let warm_query_ms = warm.build_us() as f64 / 1000.0;
        let root_items = warm.extract_items(false);
        drop(warm);
        let handlers = profile_handlers(owner, &target, extended)?;
        let mut slow_culprits: Vec<_> = handlers
            .iter()
            .filter(|handler| handler.warm_ms.is_some_and(|ms| ms >= SLOW_MS))
            .cloned()
            .collect();
        slow_culprits.sort_by(|a, b| {
            b.warm_ms
                .unwrap_or_default()
                .total_cmp(&a.warm_ms.unwrap_or_default())
        });
        let (packaged_verbs, packaged_scan_errors) = packaged_verbs(&target)?;
        Ok(InspectReport {
            target: target.path,
            is_background,
            extended,
            root_items,
            cold_query_ms,
            warm_query_ms,
            handlers,
            slow_culprits,
            packaged_verbs,
            packaged_scan_errors,
        })
    })
}

fn stats(mut samples: Vec<f64>) -> SampleStats {
    samples.sort_by(f64::total_cmp);
    let percentile = |pct: usize| samples[(samples.len() * pct).div_ceil(100) - 1];
    SampleStats {
        min_ms: samples[0],
        p50_ms: percentile(50),
        p95_ms: percentile(95),
        max_ms: samples[samples.len() - 1],
    }
}

fn sample_root(owner: HWND, target: &Target) -> Result<(f64, f64), ShellError> {
    let start = Instant::now();
    let menu = target.native_menu(owner, false)?;
    let _ = menu.root_item_count();
    let root = ms(start);
    Ok((menu.build_us() as f64 / 1000.0, root))
}

pub fn benchmark_target(
    path: &Path,
    is_background: bool,
    warm_runs: usize,
) -> Result<BenchReport, ShellError> {
    let target = Target::new(path, is_background)?;
    let warm_runs = warm_runs.max(1);
    on_sta(move |owner| {
        let (native_cold_query_ms, native_cold_root_ms) = sample_root(owner, &target)?;
        let mut query = Vec::new();
        let mut root = Vec::new();
        for _ in 0..warm_runs {
            let (query_ms, root_ms) = sample_root(owner, &target)?;
            query.push(query_ms);
            root.push(root_ms);
        }
        let native_warm_query = stats(query);
        let native_warm_root = stats(root);
        let eager = target.native_menu(owner, false)?;
        let start = Instant::now();
        let (_, eager_submenu_init_ms) = eager.inspect_submenus();
        let eager_submenu_ms = ms(start);
        drop(eager);

        let handlers = profile_handlers(owner, &target, false)?;
        let (msix_verbs, packaged_scan_errors) = packaged_verbs(&target)?;
        let delay = f64::from(RwmConfig::default().arm_delay_ms);
        let simulated_latency = [80, 100, 120]
            .into_iter()
            .map(|hold_ms| {
                let remaining = |build_ms| f64::max(0.0, delay + build_ms - f64::from(hold_ms));
                PerceivedLatency {
                    hold_ms,
                    p50_ms: remaining(native_warm_root.p50_ms),
                    p95_ms: remaining(native_warm_root.p95_ms),
                }
            })
            .collect();
        Ok(BenchReport {
            target: target.path,
            is_background,
            warm_runs,
            native_cold_query_ms,
            native_cold_root_ms,
            native_warm_query,
            native_warm_root,
            eager_submenu_ms,
            eager_submenu_init_ms,
            handlers,
            msix_count: msix_verbs.len(),
            packaged_scan_errors,
            simulated_latency,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_matches_only_applicable_item_types() {
        let manifest = r#"<Package xmlns:d="http://schemas.microsoft.com/appx/manifest/desktop/windows10/4"
            xmlns:d5="http://schemas.microsoft.com/appx/manifest/desktop/windows10/5">
            <d:Extension Category="windows.fileExplorerContextMenus"><d:FileExplorerContextMenus>
              <d5:ItemType Type="Directory\Background"><d5:Verb Id="Directory" Clsid="01234567-89ab-cdef-0123-456789abcdef"/></d5:ItemType>
              <d5:ItemType Type=".txt"><d5:Verb Id="Text" Clsid="01234567-89ab-cdef-0123-456789abcdef"/></d5:ItemType>
            </d:FileExplorerContextMenus></d:Extension></Package>"#;
        let target = Target {
            path: PathBuf::from("x.txt"),
            kind: TargetKind::File,
            extension: Some(".txt".into()),
        };
        let verbs = parse_manifest(manifest, "fake_package", &target).unwrap();
        assert_eq!(verbs.len(), 1);
        assert_eq!(verbs[0].verb_id, "Text");
        assert_eq!(verbs[0].clsid, "{01234567-89AB-CDEF-0123-456789ABCDEF}");
    }

    #[test]
    fn warm_percentiles_use_nearest_rank() {
        let samples = stats(vec![14.0, 3.0, 12.0, 6.0]);
        assert_eq!(
            (
                samples.min_ms,
                samples.p50_ms,
                samples.p95_ms,
                samples.max_ms
            ),
            (3.0, 6.0, 14.0, 14.0)
        );
    }

    #[test]
    #[ignore = "profiles the local Windows Shell and installed extensions"]
    fn diagnostics_read_real_menus_and_package_registrations() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let report = inspect_target(repo, true, false).unwrap();
        assert!(report
            .root_items
            .iter()
            .any(|item| !item.clean_label.is_empty()));

        let packages = open_key_read(HKEY_CURRENT_USER, PACKAGES).unwrap();
        if let Some(packages) = packages {
            for package in enum_subkeys(&packages).unwrap() {
                if package.starts_with("Microsoft.WindowsTerminal_")
                    && installed_package_manifest(&package).is_some_and(|path| path.is_file())
                {
                    assert!(
                        report
                            .packaged_verbs
                            .iter()
                            .any(|verb| verb.package_full_name == package),
                        "{package} missing from report: {:?}",
                        report.packaged_scan_errors
                    );
                }
            }
        }

        let manifest = repo.join("Cargo.toml");
        let bench = benchmark_target(&manifest, false, 2).unwrap();
        assert_eq!(bench.warm_runs, 2);
        assert!(bench.native_warm_root.p95_ms >= bench.native_warm_query.p95_ms);
        assert_eq!(
            bench
                .simulated_latency
                .iter()
                .map(|sample| sample.hold_ms)
                .collect::<Vec<_>>(),
            [80, 100, 120]
        );
        assert!(bench.handlers.iter().any(|handler| handler.warm.is_some()));
    }
}
