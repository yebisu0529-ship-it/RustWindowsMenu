use std::ffi::OsString;
use std::hash::Hasher;
use std::io;
use std::marker::PhantomData;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf, Prefix};
use std::ptr;
use std::sync::OnceLock;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{
    FreeLibrary, GetLastError, SetLastError, ERROR_ACCESS_DENIED, ERROR_INVALID_HANDLE,
    ERROR_INVALID_MENU_HANDLE, ERROR_INVALID_WINDOW_HANDLE, ERROR_POPUP_ALREADY_ACTIVE,
    E_OUTOFMEMORY, HWND, LPARAM, LRESULT, NO_ERROR, POINT, WPARAM,
};
use windows::Win32::Globalization::{GetUserDefaultUILanguage, MultiByteToWideChar, CP_ACP};
use windows::Win32::Storage::FileSystem::GetDriveTypeW;
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemAlloc, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::DataExchange::GetClipboardSequenceNumber;
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
use windows::Win32::UI::Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_SHIFT};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    IContextMenu, IContextMenu2, IContextMenu3, IFolderView, IFolderView2, ILClone, ILCombine,
    ILFindLastID, ILGetSize, ILIsEqual, ILIsParent, ILRemoveLastID, IPersistFolder2, IShellFolder,
    IShellItem, IShellItemArray, IShellView, SHBindToObject, SHBindToParent, SHGetDesktopFolder,
    SHGetIDListFromObject, SHGetNameFromIDList, SHParseDisplayName, CMF_CANRENAME, CMF_EXPLORE,
    CMF_EXTENDEDVERBS, CMF_NORMAL, CMIC_MASK_PTINVOKE, CMINVOKECOMMANDINFO, CMINVOKECOMMANDINFOEX,
    GCS_VERBA, GCS_VERBW, SEE_MASK_ASYNCOK, SEE_MASK_UNICODE, SIGDN_FILESYSPATH, SVGIO_BACKGROUND,
    SVGIO_SELECTION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DeleteMenu, DestroyMenu, GetMenuDefaultItem, GetMenuItemCount,
    GetMenuItemInfoW, RemoveMenu, SystemParametersInfoW, TrackPopupMenuEx, GMDI_USEDISABLED, HMENU,
    MENUITEMINFOW, MF_BYPOSITION, MFS_CHECKED, MFS_DISABLED, MFT_BITMAP, MFT_OWNERDRAW,
    MFT_SEPARATOR, MF_SEPARATOR as WMF_SEPARATOR, MF_STRING, MIIM_BITMAP, MIIM_CHECKMARKS,
    MIIM_FTYPE, MIIM_ID, MIIM_STATE, MIIM_STRING, MIIM_SUBMENU, SPI_GETHIGHCONTRAST, SW_SHOWNORMAL,
    TPM_LEFTALIGN, TPM_NOANIMATION, TPM_RETURNCMD, TPM_RIGHTBUTTON, TPM_TOPALIGN, WM_DRAWITEM,
    WM_INITMENUPOPUP, WM_MEASUREITEM, WM_MENUCHAR,
};
use windows_core::{w, Interface, PCSTR, PCWSTR, PSTR, PWSTR};

use crate::config::RwmConfig;
use crate::fasthash::FastHasher;
use crate::ipc_cache::MAX_PREFETCH_BYTES;
use crate::reg_util::{pwstr_to_string, wide_null};

const MAX_PREFETCH_ITEMS: usize = 16;
const DRIVE_REMOVABLE_TYPE: u32 = 2;
const DRIVE_FIXED_TYPE: u32 = 3;
const DRIVE_CDROM_TYPE: u32 = 5;
const DRIVE_RAMDISK_TYPE: u32 = 6;

const FIRST_ID: u32 = 1;
const LAST_ID: u32 = 0x7F00;
pub const CUSTOMIZE_MENU_CMD_ID: u32 = 0x7F01;
const CMIC_MASK_UNICODE: u32 = SEE_MASK_UNICODE;
const CMIC_MASK_ASYNCOK: u32 = SEE_MASK_ASYNCOK;
const CMIC_MASK_SHIFT_DOWN: u32 = 0x1000_0000;
const CMIC_MASK_CONTROL_DOWN: u32 = 0x4000_0000;

#[derive(Debug, thiserror::Error)]
pub enum ShellError {
    #[error("Windows Shell call failed: {0}")]
    Windows(#[from] windows_core::Error),
    #[error("Shell I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("the Shell view has no selected items")]
    NoActiveSelection,
    #[error("invalid Shell menu target")]
    InvalidTarget,
    #[error("the command could not be invoked: {0}")]
    CommandInvokeFailed(String),
    #[error("inspection worker failed: {0}")]
    WorkerFailed(String),
}

#[derive(Debug)]
pub struct OwnedPidl(*mut ITEMIDLIST);

impl OwnedPidl {
    pub fn parse(path: impl AsRef<Path>) -> Result<Self, ShellError> {
        let path = wide_null(path.as_ref());
        if path.len() < 2 || path[..path.len() - 1].contains(&0) {
            return Err(ShellError::InvalidTarget);
        }
        let mut raw = ptr::null_mut();
        unsafe { SHParseDisplayName(PCWSTR(path.as_ptr()), None, &mut raw, 0, None)? };
        Self::checked(raw)
    }

    fn checked(raw: *mut ITEMIDLIST) -> Result<Self, ShellError> {
        if raw.is_null() {
            return Err(ShellError::InvalidTarget);
        }
        Ok(unsafe { Self::from_raw_owned(raw) })
    }

    /// # Safety
    ///
    /// A non-null pointer must be an exclusively owned PIDL allocated by the Shell task allocator.
    pub unsafe fn from_raw_owned(raw: *mut ITEMIDLIST) -> Self {
        Self(raw)
    }

    pub fn clone_pidl(&self) -> Result<Self, ShellError> {
        if self.is_null() {
            return Ok(Self(ptr::null_mut()));
        }
        let duplicate = unsafe { ILClone(self.0) };
        if duplicate.is_null() {
            return Err(
                windows_core::Error::from(windows::Win32::Foundation::E_OUTOFMEMORY).into(),
            );
        }
        Ok(Self(duplicate))
    }

    pub fn as_ptr(&self) -> *const ITEMIDLIST {
        self.0
    }

    pub fn is_null(&self) -> bool {
        self.0.is_null()
    }

    /// Raw serialized bytes of this PIDL including its trailing 2-byte zero terminator.
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        if self.0.is_null() {
            return &[];
        }
        let size = unsafe { ILGetSize(Some(self.0)) } as usize;
        if size == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(self.0.cast::<u8>(), size) }
        }
    }

    /// Validates a serialized PIDL buffer and copies it into a Shell-owned `CoTaskMemAlloc` block.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ShellError> {
        let mut offset = 0usize;
        while offset + 2 <= bytes.len() {
            let cb = u16::from_le_bytes([bytes[offset], bytes[offset + 1]]) as usize;
            if cb == 0 {
                let total = offset + 2;
                if total != bytes.len() {
                    return Err(ShellError::InvalidTarget);
                }
                let raw = unsafe { CoTaskMemAlloc(total) }.cast::<u8>();
                if raw.is_null() {
                    return Err(windows_core::Error::from(E_OUTOFMEMORY).into());
                }
                unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), raw, total) };
                return Ok(unsafe { Self::from_raw_owned(raw.cast()) });
            }
            if cb < 3 || offset + cb > bytes.len() {
                return Err(ShellError::InvalidTarget);
            }
            offset += cb;
        }
        Err(ShellError::InvalidTarget)
    }

    /// Combines a parent absolute PIDL with a relative child PIDL into a newly owned absolute PIDL.
    ///
    /// # Safety
    ///
    /// `relative_child` must be a valid Shell PIDL pointer.
    pub unsafe fn combine_child_raw(
        &self,
        relative_child: *const ITEMIDLIST,
    ) -> Result<Self, ShellError> {
        if self.0.is_null() || relative_child.is_null() {
            return Err(ShellError::InvalidTarget);
        }
        let combined = unsafe { ILCombine(Some(self.0), Some(relative_child)) };
        Self::checked(combined)
    }

    /// Compares raw PIDL bytes first (`~2 ns`) and falls back to `ILIsEqual` only on mismatch.
    #[inline]
    pub fn fast_equals(&self, other: &Self) -> bool {
        let (a, b) = (self.as_bytes(), other.as_bytes());
        if !a.is_empty() && a == b {
            return true;
        }
        unsafe { ILIsEqual(self.0, other.0).as_bool() }
    }

    fn fs_path(&self) -> Option<PathBuf> {
        let name = unsafe { SHGetNameFromIDList(self.0, SIGDN_FILESYSPATH) }.ok()?;
        if name.is_null() {
            return None;
        }
        let path = unsafe { OsString::from_wide(name.as_wide()) };
        unsafe { CoTaskMemFree(Some(name.0.cast())) };
        (!path.is_empty()).then(|| PathBuf::from(path))
    }

    fn parent(&self) -> Result<Self, ShellError> {
        let parent = self.clone_pidl()?;
        if !unsafe { ILRemoveLastID(Some(parent.0)) }.as_bool() {
            return Err(ShellError::InvalidTarget);
        }
        Ok(parent)
    }
}

impl Clone for OwnedPidl {
    fn clone(&self) -> Self {
        self.clone_pidl().expect("could not allocate Shell PIDL")
    }
}

impl Drop for OwnedPidl {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0.cast())) };
    }
}

pub struct OwnedMenu(HMENU);

impl OwnedMenu {
    pub fn create_popup() -> Result<Self, ShellError> {
        Ok(Self(unsafe { CreatePopupMenu()? }))
    }

    pub fn as_handle(&self) -> HMENU {
        self.0
    }
}

impl Drop for OwnedMenu {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyMenu(self.0);
        }
    }
}

pub struct ComStaGuard(PhantomData<*mut ()>);

impl ComStaGuard {
    pub fn initialize() -> Result<Self, ShellError> {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
        Ok(Self(PhantomData))
    }
}

impl Drop for ComStaGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() }
    }
}

pub struct OleGuard(PhantomData<*mut ()>);

impl OleGuard {
    pub fn initialize() -> Result<Self, ShellError> {
        unsafe { OleInitialize(None)? };
        Ok(Self(PhantomData))
    }
}

impl Drop for OleGuard {
    fn drop(&mut self) {
        unsafe { OleUninitialize() }
    }
}

#[derive(Debug, Clone)]
pub struct SelectionKey {
    is_background: bool,
    folder_pidl: OwnedPidl,
    item_pidls: Vec<OwnedPidl>,
    shift_held: bool,
    folder_fs_path: OnceLock<Option<PathBuf>>,
    primary_fs_path: OnceLock<Option<PathBuf>>,
}

impl SelectionKey {
    pub fn new(folder_pidl: OwnedPidl, item_pidls: Vec<OwnedPidl>, shift_held: bool) -> Self {
        Self {
            is_background: item_pidls.is_empty(),
            folder_pidl,
            item_pidls,
            shift_held,
            folder_fs_path: OnceLock::new(),
            primary_fs_path: OnceLock::new(),
        }
    }

    pub fn is_background(&self) -> bool {
        self.is_background
    }

    pub fn shift_held(&self) -> bool {
        self.shift_held
    }

    pub fn item_count(&self) -> usize {
        self.item_pidls.len()
    }

    pub fn folder_fs_path(&self) -> Option<&Path> {
        self.folder_fs_path
            .get_or_init(|| self.folder_pidl.fs_path())
            .as_deref()
    }

    pub fn primary_fs_path(&self) -> Option<&Path> {
        self.primary_fs_path
            .get_or_init(|| {
                if self.is_background {
                    self.folder_fs_path().map(Path::to_path_buf)
                } else {
                    self.item_pidls[0].fs_path()
                }
            })
            .as_deref()
    }

    pub fn matches(&self, other: &Self) -> bool {
        self.is_background == other.is_background
            && self.shift_held == other.shift_held
            && self.item_pidls.len() == other.item_pidls.len()
            && self.folder_pidl.fast_equals(&other.folder_pidl)
            && self
                .item_pidls
                .iter()
                .zip(&other.item_pidls)
                .all(|(a, b)| a.fast_equals(b))
    }

    /// Computes a zero-allocation 64-bit cache key from PIDL bytes, flags, and clipboard sequence.
    pub fn cache_key_hash(&self, clipboard_seq: u32) -> u64 {
        let mut hasher = FastHasher::default();
        hasher.write_u8(u8::from(self.is_background));
        hasher.write_u8(u8::from(self.shift_held));
        hasher.write_u32(clipboard_seq);
        hasher.write_usize(self.item_pidls.len());
        let folder = self.folder_pidl.as_bytes();
        hasher.write_usize(folder.len());
        hasher.write(folder);
        for item in &self.item_pidls {
            let bytes = item.as_bytes();
            hasher.write_usize(bytes.len());
            hasher.write(bytes);
        }
        hasher.finish()
    }

    /// Computes a 64-bit non-zero hash over folder and item PIDLs independent of shift/clipboard state.
    pub fn item_identity_hash(&self) -> u64 {
        let mut hasher = FastHasher::default();
        hasher.write_u8(u8::from(self.is_background));
        hasher.write_usize(self.item_pidls.len());
        let folder = self.folder_pidl.as_bytes();
        hasher.write_usize(folder.len());
        hasher.write(folder);
        for item in &self.item_pidls {
            let bytes = item.as_bytes();
            hasher.write_usize(bytes.len());
            hasher.write(bytes);
        }
        hasher.finish().max(1)
    }

    /// Returns the relative child PIDL pointer for the primary selected item if valid.
    pub fn primary_child_pidl(&self) -> Option<*const ITEMIDLIST> {
        let first = self.item_pidls.first()?;
        unsafe {
            if !ILIsParent(self.folder_pidl.0, first.0, true).as_bool() {
                return None;
            }
            Some(ILFindLastID(first.0).cast_const())
        }
    }

    /// Serializes folder + item PIDLs into a caller-owned scratch buffer (`<= 16` items, `<= 16 KB`).
    pub fn serialize_pidls(&self, out: &mut Vec<u8>) -> bool {
        out.clear();
        if self.item_pidls.len() > MAX_PREFETCH_ITEMS {
            return false;
        }
        let folder = self.folder_pidl.as_bytes();
        if folder.is_empty() {
            return false;
        }
        out.extend_from_slice(&(self.item_pidls.len() as u32).to_le_bytes());
        out.extend_from_slice(&(folder.len() as u32).to_le_bytes());
        out.extend_from_slice(folder);
        for item in &self.item_pidls {
            let bytes = item.as_bytes();
            if bytes.is_empty() || out.len() + 4 + bytes.len() > MAX_PREFETCH_BYTES {
                out.clear();
                return false;
            }
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(bytes);
        }
        out.len() <= MAX_PREFETCH_BYTES
    }

    /// Reconstructs and validates a [`SelectionKey`] from serialized PIDL bytes.
    pub fn from_serialized_pidls(bytes: &[u8], shift_held: bool) -> Result<Self, ShellError> {
        if bytes.len() > MAX_PREFETCH_BYTES || bytes.len() < 8 {
            return Err(ShellError::InvalidTarget);
        }
        let read_u32 = |offset: &mut usize| -> Result<usize, ShellError> {
            let end = offset.checked_add(4).ok_or(ShellError::InvalidTarget)?;
            let slice = bytes.get(*offset..end).ok_or(ShellError::InvalidTarget)?;
            *offset = end;
            Ok(u32::from_le_bytes(slice.try_into().unwrap()) as usize)
        };
        let mut cursor = 0usize;
        let count = read_u32(&mut cursor)?;
        if count > MAX_PREFETCH_ITEMS {
            return Err(ShellError::InvalidTarget);
        }
        let folder_len = read_u32(&mut cursor)?;
        let folder_end = cursor
            .checked_add(folder_len)
            .ok_or(ShellError::InvalidTarget)?;
        let folder_bytes = bytes
            .get(cursor..folder_end)
            .ok_or(ShellError::InvalidTarget)?;
        cursor = folder_end;
        let folder = OwnedPidl::from_bytes(folder_bytes)?;

        let mut items = Vec::with_capacity(count);
        for _ in 0..count {
            let item_len = read_u32(&mut cursor)?;
            let item_end = cursor
                .checked_add(item_len)
                .ok_or(ShellError::InvalidTarget)?;
            let item_bytes = bytes
                .get(cursor..item_end)
                .ok_or(ShellError::InvalidTarget)?;
            cursor = item_end;
            let item = OwnedPidl::from_bytes(item_bytes)?;
            if !unsafe { ILIsParent(folder.0, item.0, true).as_bool() } {
                return Err(ShellError::InvalidTarget);
            }
            items.push(item);
        }
        if cursor != bytes.len() {
            return Err(ShellError::InvalidTarget);
        }
        Ok(Self::new(folder, items, shift_held))
    }

    /// True when the primary target maps to a non-network local filesystem drive path.
    pub fn is_local_filesystem(&self) -> bool {
        let Some(path) = self.primary_fs_path() else {
            return false;
        };
        let Some(std::path::Component::Prefix(prefix)) = path.components().next() else {
            return false;
        };
        let drive = match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => letter,
            _ => return false,
        };
        let root = [u16::from(drive), u16::from(b':'), u16::from(b'\\'), 0];
        let drive_type = unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) };
        matches!(
            drive_type,
            DRIVE_REMOVABLE_TYPE | DRIVE_FIXED_TYPE | DRIVE_CDROM_TYPE | DRIVE_RAMDISK_TYPE
        )
    }

    pub fn from_paths(
        paths: &[PathBuf],
        is_background: bool,
        shift_held: bool,
    ) -> Result<Self, ShellError> {
        match paths {
            [folder] if is_background => {
                Ok(Self::new(OwnedPidl::parse(folder)?, Vec::new(), shift_held))
            }
            _ if is_background => Err(ShellError::InvalidTarget),
            [] => Err(ShellError::NoActiveSelection),
            _ => {
                let items = paths
                    .iter()
                    .map(OwnedPidl::parse)
                    .collect::<Result<Vec<_>, _>>()?;
                let folder = items[0].parent()?;
                if !items
                    .iter()
                    .all(|item| unsafe { ILIsParent(folder.0, item.0, true).as_bool() })
                {
                    return Err(ShellError::InvalidTarget);
                }
                Ok(Self::new(folder, items, shift_held))
            }
        }
    }

    pub fn capture_from_view(
        sv: &IShellView,
        fv2: Option<&IFolderView2>,
        is_background: bool,
        shift_held: bool,
    ) -> Result<Self, ShellError> {
        capture_from_view(sv, fv2, is_background, shift_held)
    }

    pub fn folder_pidl(&self) -> &OwnedPidl {
        &self.folder_pidl
    }

    pub(crate) fn child_pidls(&self) -> Result<Vec<*const ITEMIDLIST>, ShellError> {
        self.item_pidls
            .iter()
            .map(|item| unsafe {
                if !ILIsParent(self.folder_pidl.0, item.0, true).as_bool() {
                    return Err(ShellError::InvalidTarget);
                }
                Ok(ILFindLastID(item.0).cast_const())
            })
            .collect()
    }
}

pub fn capture_from_view(
    sv: &IShellView,
    fv2: Option<&IFolderView2>,
    is_background: bool,
    shift_held: bool,
) -> Result<SelectionKey, ShellError> {
    let fv: IFolderView = match fv2 {
        Some(view) => view.cast()?,
        None => sv.cast()?,
    };
    let folder = unsafe {
        fv.GetFolder::<IPersistFolder2>()
            .or_else(|_| fv.GetFolder::<IShellFolder>()?.cast())
            .and_then(|folder| folder.GetCurFolder())
            .or_else(|_| SHGetIDListFromObject(&fv.GetFolder::<IShellItem>()?))?
    };
    let folder = OwnedPidl::checked(folder)?;
    let mut items = Vec::new();
    if !is_background {
        let selection: IShellItemArray = unsafe {
            fv2.map_or_else(
                || fv.Items(SVGIO_SELECTION),
                |view| {
                    view.GetSelection(false)
                        .or_else(|_| fv.Items(SVGIO_SELECTION))
                },
            )
            .or_else(|_| sv.GetItemObject(SVGIO_SELECTION))
        }
        .map_err(|error| {
            if unsafe { fv.ItemCount(SVGIO_SELECTION) }.is_ok_and(|count| count == 0) {
                ShellError::NoActiveSelection
            } else {
                error.into()
            }
        })?;
        let count = unsafe { selection.GetCount()? };
        if count == 0 {
            return Err(ShellError::NoActiveSelection);
        }
        items.reserve(count as usize);
        for index in 0..count {
            let item = unsafe { selection.GetItemAt(index)? };
            items.push(OwnedPidl::checked(unsafe {
                SHGetIDListFromObject(&item)?
            })?);
        }
    }
    Ok(SelectionKey::new(folder, items, shift_held))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InspectedMenuItem {
    pub index: u32,
    pub is_separator: bool,
    pub label: String,
    pub clean_label: String,
    pub canonical_verb: Option<String>,
    pub w_id: u32,
    pub is_submenu: bool,
    pub is_owner_draw: bool,
    pub has_bitmap: bool,
    pub enabled: bool,
    pub checked: bool,
    pub is_default: bool,
    pub sub_init_ms: Option<f64>,
    pub children: Vec<InspectedMenuItem>,
}

pub struct NativeMenu {
    // Release COM handlers before destroying the `HMENU` they populated.
    menu: IContextMenu,
    menu2: Option<IContextMenu2>,
    menu3: Option<IContextMenu3>,
    hmenu: OwnedMenu,
    detached_submenus: Vec<OwnedMenu>,
    customized: bool,
    first_id: u32,
    end_id: u32,
    cmf: u32,
    selection: SelectionKey,
    build_us: u64,
    _sta: PhantomData<*mut ()>,
}

impl NativeMenu {
    pub fn build_from_view(
        sv: &IShellView,
        fv2: Option<&IFolderView2>,
        is_background: bool,
        shift_held: bool,
    ) -> Result<Self, ShellError> {
        let selection = capture_from_view(sv, fv2, is_background, shift_held)?;
        let menu = unsafe {
            if is_background {
                sv.GetItemObject(SVGIO_BACKGROUND)?
            } else {
                match sv.GetItemObject(SVGIO_SELECTION) {
                    Ok(menu) => menu,
                    Err(_) => {
                        let owner = sv.GetWindow().unwrap_or_default();
                        context_menu_from_pidls(owner, &selection)?
                    }
                }
            }
        };
        Self::query(menu, selection)
    }

    pub fn build_from_paths(
        owner: HWND,
        paths: &[PathBuf],
        is_background: bool,
        shift_held: bool,
    ) -> Result<Self, ShellError> {
        let selection = SelectionKey::from_paths(paths, is_background, shift_held)?;
        Self::build_from_selection(owner, selection)
    }

    /// Builds a native context menu from a pre-captured [`SelectionKey`] (on any STA thread).
    pub fn build_from_selection(
        owner: HWND,
        selection: SelectionKey,
    ) -> Result<Self, ShellError> {
        let menu = context_menu_from_pidls(owner, &selection)?;
        Self::query(menu, selection)
    }

    pub(crate) fn query(menu: IContextMenu, selection: SelectionKey) -> Result<Self, ShellError> {
        let hmenu = OwnedMenu::create_popup()?;
        let cmf = CMF_NORMAL
            | CMF_EXPLORE
            | CMF_CANRENAME
            | if selection.shift_held() {
                CMF_EXTENDEDVERBS
            } else {
                0
            };
        let start = Instant::now();
        // The projection discards the successful HRESULT containing the command ID span.
        let result = unsafe {
            (menu.vtable().QueryContextMenu)(menu.as_raw(), hmenu.0, 0, FIRST_ID, LAST_ID, cmf)
        };
        let build_us = start.elapsed().as_micros() as u64;
        result.ok()?;
        let end_id = (FIRST_ID + (result.0 as u32 & 0xffff)).min(LAST_ID + 1);
        let menu2 = menu.cast().ok();
        let menu3 = menu.cast().ok();
        Ok(Self {
            menu,
            menu2,
            menu3,
            hmenu,
            detached_submenus: Vec::new(),
            customized: false,
            first_id: FIRST_ID,
            end_id,
            cmf,
            selection,
            build_us,
            _sta: PhantomData,
        })
    }

    pub fn apply_customizations(&mut self, config: &RwmConfig) {
        if self.customized {
            return;
        }
        let hmenu = self.hmenu.0;
        let first_id = self.first_id;
        let end_id = self.end_id;
        let menu_ref = &self.menu;
        apply_to_hmenu(hmenu, &mut self.detached_submenus, config, |cmd_id| {
            if !(first_id..end_id).contains(&cmd_id) {
                return None;
            }
            canonical_verb_from_iface(menu_ref, (cmd_id - first_id) as usize)
        });
        self.customized = true;
    }

    pub fn selection(&self) -> &SelectionKey {
        &self.selection
    }

    pub fn build_us(&self) -> u64 {
        self.build_us
    }

    pub fn query_flags(&self) -> u32 {
        self.cmf
    }

    pub fn root_item_count(&self) -> usize {
        menu_item_count(self.hmenu.0) as usize
    }

    pub fn handle_menu_msg(&self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        if !matches!(
            msg,
            WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR
        ) {
            return None;
        }
        let owner_draw = matches!(msg, WM_DRAWITEM | WM_MEASUREITEM);
        if owner_draw && wparam.0 != 0 {
            return None;
        }
        let default_result = LRESULT(isize::from(owner_draw));
        if let Some(menu3) = &self.menu3 {
            let mut result = default_result;
            let status = unsafe {
                (menu3.vtable().HandleMenuMsg2)(menu3.as_raw(), msg, wparam, lparam, &mut result)
            };
            if status == windows::Win32::Foundation::S_OK {
                return Some(if owner_draw { default_result } else { result });
            }
        }
        self.menu2.as_ref().and_then(|menu2| {
            let status =
                unsafe { (menu2.vtable().HandleMenuMsg)(menu2.as_raw(), msg, wparam, lparam) };
            (status == windows::Win32::Foundation::S_OK).then_some(default_result)
        })
    }

    /// Displays the menu via `TrackPopupMenuEx` and returns the chosen command ID without invoking it.
    pub fn track_only(
        &self,
        owner: HWND,
        anchor: (i32, i32),
        no_animation: bool,
    ) -> Result<Option<u32>, ShellError> {
        let flags = TPM_LEFTALIGN
            | TPM_TOPALIGN
            | TPM_RETURNCMD
            | TPM_RIGHTBUTTON
            | if no_animation {
                TPM_NOANIMATION
            } else {
                Default::default()
            };
        unsafe { SetLastError(NO_ERROR) };
        let id = unsafe { TrackPopupMenuEx(self.hmenu.0, flags.0, anchor.0, anchor.1, owner, None) }
            .0 as u32;
        if id == 0 {
            let error = unsafe { GetLastError() };
            if matches!(
                error,
                ERROR_INVALID_HANDLE
                    | ERROR_INVALID_MENU_HANDLE
                    | ERROR_INVALID_WINDOW_HANDLE
                    | ERROR_POPUP_ALREADY_ACTIVE
                    | ERROR_ACCESS_DENIED
            ) {
                return Err(windows_core::Error::from(error.to_hresult()).into());
            }
            return Ok(None);
        }
        Ok(Some(id))
    }

    pub fn track_and_invoke(
        &self,
        owner: HWND,
        anchor: (i32, i32),
        no_animation: bool,
    ) -> Result<Option<u32>, ShellError> {
        let Some(id) = self.track_only(owner, anchor, no_animation)? else {
            return Ok(None);
        };
        self.invoke_id(id, owner, anchor)?;
        Ok(Some(id))
    }

    pub fn canonical_verb_for_id(&self, cmd_id: u32) -> Option<String> {
        self.canonical_verb(cmd_id)
    }

    pub fn invoke_id(
        &self,
        cmd_id: u32,
        owner: HWND,
        anchor: (i32, i32),
    ) -> Result<(), ShellError> {
        let offset = self.command_offset(cmd_id).ok_or_else(|| {
            ShellError::CommandInvokeFailed(format!("unknown command ID {cmd_id}"))
        })?;
        let working_dir = self.selection.folder_fs_path().map(wide_null);
        let ansi_dir = working_dir.as_deref().and_then(to_ansi);
        let mut mask = CMIC_MASK_UNICODE | CMIC_MASK_PTINVOKE | CMIC_MASK_ASYNCOK;
        if unsafe { GetKeyState(i32::from(VK_SHIFT.0)) } < 0 {
            mask |= CMIC_MASK_SHIFT_DOWN;
        }
        if unsafe { GetKeyState(i32::from(VK_CONTROL.0)) } < 0 {
            mask |= CMIC_MASK_CONTROL_DOWN;
        }
        let info = CMINVOKECOMMANDINFOEX {
            cbSize: size_of::<CMINVOKECOMMANDINFOEX>() as u32,
            fMask: mask,
            hwnd: owner,
            lpVerb: PCSTR(offset as *const u8),
            lpVerbW: PCWSTR(offset as *const u16),
            lpDirectory: ansi_dir
                .as_ref()
                .map_or(PCSTR::null(), |path| PCSTR(path.as_ptr())),
            lpDirectoryW: working_dir
                .as_ref()
                .map_or(PCWSTR::null(), |path| PCWSTR(path.as_ptr())),
            nShow: SW_SHOWNORMAL.0,
            ptInvoke: POINT {
                x: anchor.0,
                y: anchor.1,
            },
            ..Default::default()
        };
        unsafe {
            self.menu
                .InvokeCommand(ptr::from_ref(&info).cast::<CMINVOKECOMMANDINFO>())
        }
        .map_err(|error| ShellError::CommandInvokeFailed(error.to_string()))
    }

    pub fn extract_items(&self, init_submenus: bool) -> Vec<InspectedMenuItem> {
        self.read_menu(self.hmenu.0, init_submenus, 0, &mut 0.0)
    }

    pub(crate) fn inspect_submenus(&self) -> (Vec<InspectedMenuItem>, f64) {
        let mut elapsed = 0.0;
        let items = self.read_menu(self.hmenu.0, true, 0, &mut elapsed);
        (items, elapsed)
    }

    fn read_menu(
        &self,
        menu: HMENU,
        init_submenus: bool,
        depth: usize,
        sub_init_ms: &mut f64,
    ) -> Vec<InspectedMenuItem> {
        let default_position = unsafe { GetMenuDefaultItem(menu, 1, GMDI_USEDISABLED) };
        (0..menu_item_count(menu))
            .filter_map(|index| {
                let (info, label) = menu_info(menu, index)?;
                let separator = info.fType.contains(MFT_SEPARATOR);
                let is_submenu = !info.hSubMenu.is_invalid();
                let (submenu_ms, children) = if init_submenus && is_submenu && depth < 32 {
                    let start = Instant::now();
                    self.handle_menu_msg(
                        WM_INITMENUPOPUP,
                        WPARAM(info.hSubMenu.0 as usize),
                        LPARAM(index as isize & 0xffff),
                    );
                    let ms = start.elapsed().as_secs_f64() * 1000.0;
                    *sub_init_ms += ms;
                    (
                        Some(ms),
                        self.read_menu(info.hSubMenu, true, depth + 1, sub_init_ms),
                    )
                } else {
                    (None, Vec::new())
                };
                let canonical_verb = if separator || is_submenu {
                    None
                } else {
                    self.canonical_verb(info.wID)
                };
                Some(InspectedMenuItem {
                    index,
                    is_separator: separator,
                    clean_label: clean_label(&label),
                    label,
                    canonical_verb,
                    w_id: info.wID,
                    is_submenu,
                    is_owner_draw: info.fType.contains(MFT_OWNERDRAW),
                    has_bitmap: !info.hbmpItem.is_invalid()
                        || !info.hbmpChecked.is_invalid()
                        || !info.hbmpUnchecked.is_invalid()
                        || info.fType.contains(MFT_BITMAP),
                    enabled: !info.fState.contains(MFS_DISABLED),
                    checked: info.fState.contains(MFS_CHECKED),
                    is_default: index == default_position,
                    sub_init_ms: submenu_ms,
                    children,
                })
            })
            .collect()
    }

    fn command_offset(&self, id: u32) -> Option<usize> {
        (self.first_id..self.end_id)
            .contains(&id)
            .then(|| (id - self.first_id) as usize)
    }

    fn canonical_verb(&self, id: u32) -> Option<String> {
        let offset = self.command_offset(id)?;
        canonical_verb_from_iface(&self.menu, offset)
    }
}

fn canonical_verb_from_iface(menu: &IContextMenu, offset: usize) -> Option<String> {
    let mut buffer = [0u16; 512];
    let result = unsafe {
        menu.GetCommandString(
            offset,
            GCS_VERBW,
            None,
            PSTR(buffer.as_mut_ptr().cast()),
            buffer.len() as u32,
        )
    };
    if result.is_ok() {
        let len = nul_len(&buffer);
        if len != 0 && len < buffer.len() {
            if let Ok(text) = String::from_utf16(&buffer[..len]) {
                return Some(text);
            }
        }
    }
    let mut buffer = [0u8; 512];
    unsafe {
        menu.GetCommandString(
            offset,
            GCS_VERBA,
            None,
            PSTR(buffer.as_mut_ptr()),
            buffer.len() as u32,
        )
        .ok()?;
    }
    let len = buffer.iter().position(|&byte| byte == 0)?;
    let mut wide = [0u16; 512];
    let len = unsafe {
        MultiByteToWideChar(CP_ACP, Default::default(), &buffer[..len], Some(&mut wide))
    };
    (len > 0).then(|| String::from_utf16_lossy(&wide[..len as usize]))
}

fn to_ansi(wide: &[u16]) -> Option<Vec<u8>> {
    use windows::Win32::Globalization::WideCharToMultiByte;
    let len = unsafe { WideCharToMultiByte(CP_ACP, 0, wide, None, PCSTR::null(), None) };
    if len <= 0 {
        return None;
    }
    let mut ansi = vec![0; len as usize];
    let count =
        unsafe { WideCharToMultiByte(CP_ACP, 0, wide, Some(&mut ansi), PCSTR::null(), None) };
    (count == len).then_some(ansi)
}

pub(crate) fn bind_folder(pidl: &OwnedPidl) -> Result<IShellFolder, ShellError> {
    let desktop = unsafe { SHGetDesktopFolder()? };
    if unsafe { ILFindLastID(pidl.0).cast::<u16>().read_unaligned() } == 0 {
        return Ok(desktop);
    }
    Ok(unsafe { SHBindToObject(&desktop, pidl.0, None)? })
}

fn context_menu_from_pidls(
    owner: HWND,
    selection: &SelectionKey,
) -> Result<IContextMenu, ShellError> {
    if selection.is_background() {
        let folder = bind_folder(&selection.folder_pidl)?;
        return Ok(unsafe { folder.CreateViewObject(owner)? });
    }
    let folder: IShellFolder = unsafe { SHBindToParent(selection.item_pidls[0].0, None)? };
    let children = selection.child_pidls()?;
    Ok(unsafe { folder.GetUIObjectOf(owner, &children, None)? })
}

fn menu_item_count(menu: HMENU) -> u32 {
    unsafe { GetMenuItemCount(menu) }.max(0) as u32
}

fn nul_len(units: &[u16]) -> usize {
    units
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(units.len())
}

fn menu_info(menu: HMENU, index: u32) -> Option<(MENUITEMINFOW, String)> {
    let mut info = MENUITEMINFOW {
        cbSize: size_of::<MENUITEMINFOW>() as u32,
        fMask: MIIM_FTYPE
            | MIIM_ID
            | MIIM_STATE
            | MIIM_SUBMENU
            | MIIM_STRING
            | MIIM_BITMAP
            | MIIM_CHECKMARKS,
        ..Default::default()
    };
    unsafe { GetMenuItemInfoW(menu, index, true, &mut info).ok()? };
    let mut label = String::new();
    if info.cch != 0 {
        let mut units = vec![0; info.cch as usize + 1];
        let mut text = MENUITEMINFOW {
            cbSize: size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_STRING,
            dwTypeData: PWSTR(units.as_mut_ptr()),
            cch: units.len() as u32,
            ..Default::default()
        };
        if unsafe { GetMenuItemInfoW(menu, index, true, &mut text) }.is_ok() {
            label = unsafe { pwstr_to_string(text.dwTypeData) }.unwrap_or_default();
        }
    }
    Some((info, label))
}

fn clean_label(label: &str) -> String {
    let mut text = label.split('\t').next().unwrap_or_default().trim_end();
    let mut ellipsis = "";
    if let Some(base) = text.strip_suffix("...") {
        text = base;
        ellipsis = "...";
    } else if let Some(base) = text.strip_suffix('\u{2026}') {
        text = base;
        ellipsis = "\u{2026}";
    }
    let mut chars = text.chars().rev();
    if chars.next() == Some(')')
        && chars
            .next()
            .is_some_and(|letter| letter.is_ascii_alphanumeric())
        && chars.next() == Some('&')
        && chars.next() == Some('(')
    {
        text = text[..text.len() - 4].trim_end();
    }
    let mut result = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '&' || chars.next_if_eq(&'&').is_some() {
            result.push(ch);
        }
    }
    result.push_str(ellipsis);
    result.trim().to_owned()
}

pub fn enable_dark_mode_menus() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        if !crate::classic::windows_version().is_ok_and(|(build, _)| build >= 22000) {
            return false;
        }
        let mut contrast = HIGHCONTRASTW {
            cbSize: size_of::<HIGHCONTRASTW>() as u32,
            ..Default::default()
        };
        let contrast_status = unsafe {
            SystemParametersInfoW(
                SPI_GETHIGHCONTRAST,
                contrast.cbSize,
                Some(ptr::from_mut(&mut contrast).cast()),
                Default::default(),
            )
        };
        if contrast_status.is_err() || contrast.dwFlags.contains(HCF_HIGHCONTRASTON) {
            return false;
        }
        let Ok(module) =
            (unsafe { LoadLibraryExW(w!("uxtheme.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) })
        else {
            return false;
        };
        let enabled = unsafe {
            let set = GetProcAddress(module, PCSTR(135usize as *const u8));
            let flush = GetProcAddress(module, PCSTR(136usize as *const u8));
            if let (Some(set), Some(flush)) = (set, flush) {
                if set as usize == flush as usize {
                    false
                } else {
                    let set: unsafe extern "system" fn(i32) -> i32 = std::mem::transmute(set);
                    let flush: unsafe extern "system" fn() = std::mem::transmute(flush);
                    set(1);
                    flush();
                    true
                }
            } else {
                false
            }
        };
        if !enabled {
            unsafe {
                let _ = FreeLibrary(module);
            }
        }
        enabled
    })
}

pub fn is_japanese_ui() -> bool {
    const LANG_JAPANESE: u16 = 0x11;
    unsafe { GetUserDefaultUILanguage() & 0x03ff == LANG_JAPANESE }
}

pub fn customize_menu_label() -> &'static str {
    if is_japanese_ui() {
        "メニューをカスタマイズ (RWM)..."
    } else {
        "Customize Menu (RWM)..."
    }
}

pub fn menu_item_filter_key(canonical_verb: Option<&str>, clean_label: &str) -> Option<String> {
    if let Some(verb) = canonical_verb.map(str::trim).filter(|s| !s.is_empty()) {
        Some(format!("verb:{verb}"))
    } else {
        let label = clean_label.trim();
        (!label.is_empty()).then(|| format!("label:{label}"))
    }
}

fn eq_ignore_case(a: &str, b: &str) -> bool {
    if a.is_ascii() && b.is_ascii() {
        a.eq_ignore_ascii_case(b)
    } else {
        a.to_lowercase() == b.to_lowercase()
    }
}

pub fn item_matches_hidden(
    hidden_items: &[String],
    canonical_verb: Option<&str>,
    clean_label: &str,
) -> bool {
    let verb = canonical_verb.map(str::trim).filter(|s| !s.is_empty());
    let label = clean_label.trim();
    hidden_items.iter().any(|rule| {
        let rule = rule.trim();
        if rule.is_empty() {
            return false;
        }
        if let Some((prefix, rest)) = rule.split_once(':') {
            if prefix.eq_ignore_ascii_case("verb") {
                return verb.is_some_and(|v| eq_ignore_case(v, rest.trim()));
            }
            if prefix.eq_ignore_ascii_case("label") {
                let target_label = rest.trim();
                return !label.is_empty() && eq_ignore_case(label, target_label);
            }
        }
        verb.is_some_and(|v| eq_ignore_case(v, rule))
            || (!label.is_empty() && eq_ignore_case(label, rule))
    })
}

fn orphan_separator_positions(is_separator: &[bool]) -> Vec<u32> {
    let mut remove = Vec::new();
    let Some(first_non_sep) = is_separator.iter().position(|&sep| !sep) else {
        // Every entry is a separator.
        for i in 0..is_separator.len() {
            remove.push(i as u32);
        }
        return remove;
    };
    let last_non_sep = is_separator
        .iter()
        .rposition(|&sep| !sep)
        .unwrap_or(first_non_sep);
    let mut prev_was_sep = false;
    for (idx, &is_sep) in is_separator.iter().enumerate() {
        if !is_sep {
            prev_was_sep = false;
            continue;
        }
        if idx < first_non_sep || idx > last_non_sep || prev_was_sep {
            remove.push(idx as u32);
        } else {
            prev_was_sep = true;
        }
    }
    remove
}

fn apply_to_hmenu(
    hmenu: HMENU,
    detached_submenus: &mut Vec<OwnedMenu>,
    config: &RwmConfig,
    resolve_verb: impl Fn(u32) -> Option<String>,
) {
    if !config.hidden_items.is_empty() {
        let has_verb_rules = config
            .hidden_items
            .iter()
            .any(|rule| !rule.to_ascii_lowercase().starts_with("label:"));
        let count = menu_item_count(hmenu);
        for index in (0..count).rev() {
            let Some((info, label)) = menu_info(hmenu, index) else {
                continue;
            };
            if info.fType.contains(MFT_SEPARATOR) {
                continue;
            }
            let clean = clean_label(&label);
            let mut matched = item_matches_hidden(&config.hidden_items, None, &clean);
            if !matched && has_verb_rules && info.hSubMenu.is_invalid() {
                if let Some(verb) = resolve_verb(info.wID) {
                    matched = item_matches_hidden(&config.hidden_items, Some(&verb), &clean);
                }
            }
            if matched {
                if !info.hSubMenu.is_invalid() {
                    if unsafe { RemoveMenu(hmenu, index, MF_BYPOSITION) }.is_ok() {
                        detached_submenus.push(OwnedMenu(info.hSubMenu));
                    }
                } else {
                    let _ = unsafe { DeleteMenu(hmenu, index, MF_BYPOSITION) };
                }
            }
        }

        let after_count = menu_item_count(hmenu);
        let mut is_sep = Vec::with_capacity(after_count as usize);
        for index in 0..after_count {
            let sep = menu_info(hmenu, index)
                .is_some_and(|(info, _)| info.fType.contains(MFT_SEPARATOR));
            is_sep.push(sep);
        }
        for pos in orphan_separator_positions(&is_sep).into_iter().rev() {
            let _ = unsafe { DeleteMenu(hmenu, pos, MF_BYPOSITION) };
        }
    }

    if config.show_customize_entry {
        if menu_item_count(hmenu) > 0 {
            let _ = unsafe { AppendMenuW(hmenu, WMF_SEPARATOR, 0, PCWSTR::null()) };
        }
        let label = wide_null(customize_menu_label());
        let _ = unsafe {
            AppendMenuW(
                hmenu,
                MF_STRING,
                CUSTOMIZE_MENU_CMD_ID as usize,
                PCWSTR(label.as_ptr()),
            )
        };
    }
}

/// Current Windows clipboard change counter, used to invalidate Paste cache entries automatically.
#[inline]
pub fn clipboard_sequence_number() -> u32 {
    unsafe { GetClipboardSequenceNumber() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use windows::Win32::Foundation::{HINSTANCE, RECT};
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
    use windows::Win32::UI::Shell::{
        ExplorerBrowser, IExplorerBrowser, EBO_NOPERSISTVIEWSTATE, SBSP_ABSOLUTE,
        SVSI_DESELECTOTHERS, SVSI_SELECT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, DispatchMessageW, PeekMessageW, TranslateMessage, MSG,
        PM_REMOVE, WS_OVERLAPPED,
    };

    struct BrowserHost {
        hwnd: HWND,
        browser: IExplorerBrowser,
    }

    impl BrowserHost {
        fn open(folder: &OwnedPidl) -> Result<Self, ShellError> {
            let browser: IExplorerBrowser =
                unsafe { CoCreateInstance(&ExplorerBrowser, None, CLSCTX_INPROC_SERVER)? };
            let hwnd = unsafe {
                CreateWindowExW(
                    Default::default(),
                    w!("STATIC"),
                    w!("RustWindowsMenu test"),
                    WS_OVERLAPPED,
                    0,
                    0,
                    640,
                    480,
                    HWND::default(),
                    HMENU::default(),
                    HINSTANCE::default(),
                    None,
                )?
            };
            let host = Self { hwnd, browser };
            let rect = RECT {
                left: 0,
                top: 0,
                right: 640,
                bottom: 480,
            };
            unsafe {
                host.browser.Initialize(host.hwnd, &rect, None)?;
                host.browser.SetOptions(EBO_NOPERSISTVIEWSTATE)?;
                host.browser
                    .BrowseToIDList(folder.as_ptr(), SBSP_ABSOLUTE)?;
            }
            Ok(host)
        }

        fn view(&self) -> Result<IShellView, ShellError> {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                pump_messages();
                match unsafe { self.browser.GetCurrentView() } {
                    Ok(view) => return Ok(view),
                    Err(error) if Instant::now() >= deadline => return Err(error.into()),
                    Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        }
    }

    fn pump_messages() {
        let mut message = MSG::default();
        while unsafe { PeekMessageW(&mut message, HWND::default(), 0, 0, PM_REMOVE) }.as_bool() {
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }

    fn select_item(
        view: &IShellView,
        view2: &IFolderView2,
        expected: &SelectionKey,
    ) -> SelectionKey {
        let deadline = Instant::now() + Duration::from_secs(5);
        let pidl = expected.child_pidls().unwrap()[0];
        loop {
            let result =
                unsafe { view.SelectItem(pidl, (SVSI_SELECT.0 | SVSI_DESELECTOTHERS.0) as u32) };
            pump_messages();
            let captured = capture_from_view(view, Some(view2), false, false);
            if captured.as_ref().is_ok_and(|key| key.matches(expected)) {
                return captured.unwrap();
            }
            assert!(
                Instant::now() < deadline,
                "the owned view did not select {:?}; SelectItem {result:?}, capture {captured:?}",
                expected.primary_fs_path()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    impl Drop for BrowserHost {
        fn drop(&mut self) {
            unsafe {
                let _ = self.browser.Destroy();
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }

    #[test]
    fn strips_mnemonics_and_preserves_literal_ampersands() {
        assert_eq!(clean_label("&Copy as path\tCtrl+Shift+C"), "Copy as path");
        assert_eq!(clean_label("表示(&V)…"), "表示…");
        assert_eq!(clean_label("Pin && Remove"), "Pin & Remove");
    }

    #[test]
    fn pidl_bytes_serialization_roundtrips_and_rejects_malformed() {
        let _sta = ComStaGuard::initialize();
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let manifest = repo.join("Cargo.toml");
        let lock = repo.join("Cargo.lock");
        let key = SelectionKey::from_paths(&[manifest.clone(), lock], false, true).unwrap();
        assert!(key.is_local_filesystem());

        let mut buf = Vec::new();
        assert!(key.serialize_pidls(&mut buf));
        let restored = SelectionKey::from_serialized_pidls(&buf, true).unwrap();
        assert!(key.matches(&restored));
        assert_eq!(key.cache_key_hash(42), restored.cache_key_hash(42));
        assert_ne!(key.cache_key_hash(42), restored.cache_key_hash(43));

        // Reject virtual namespace (This PC) as local filesystem hover target.
        let this_pc = OwnedPidl::parse("::{20D04FE0-3AEA-1069-A2D8-08002B30309D}").unwrap();
        let virt = SelectionKey::new(this_pc, Vec::new(), false);
        assert!(!virt.is_local_filesystem());

        // Reject corrupt PIDL buffers safely.
        assert!(OwnedPidl::from_bytes(&[]).is_err());
        assert!(OwnedPidl::from_bytes(&[10, 0, 1, 2]).is_err());
        assert!(OwnedPidl::from_bytes(&[1, 0, 0, 0]).is_err());
        assert!(OwnedPidl::from_bytes(&[4, 0, 1, 2, 0, 0, 9]).is_err());
        assert!(SelectionKey::from_serialized_pidls(&buf[..buf.len() - 1], true).is_err());
    }

    #[test]
    #[ignore = "requires a local Windows desktop to host IExplorerBrowser"]
    fn owned_explorer_browser_builds_native_and_virtual_menus() {
        let _ole = OleGuard::initialize().unwrap();
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let folder = OwnedPidl::parse(repo).unwrap();
        let host = BrowserHost::open(&folder).unwrap();
        let view = host.view().unwrap();
        let view2: IFolderView2 = view.cast().unwrap();

        let manifest = repo.join("Cargo.toml");
        let path_menu =
            NativeMenu::build_from_paths(host.hwnd, std::slice::from_ref(&manifest), false, false)
                .unwrap();
        assert!(path_menu.root_item_count() > 0);
        select_item(&view, &view2, path_menu.selection());
        let selected = NativeMenu::build_from_view(&view, Some(&view2), false, false).unwrap();
        assert!(selected.root_item_count() > 0);
        assert_eq!(selected.selection().item_count(), 1);
        assert!(selected.selection().matches(path_menu.selection()));
        assert!(selected.selection().matches(&path_menu.selection().clone()));
        let shift = capture_from_view(&view, Some(&view2), false, true).unwrap();
        assert!(!shift.matches(selected.selection()));
        let lock = SelectionKey::from_paths(&[repo.join("Cargo.lock")], false, false).unwrap();
        let changed = select_item(&view, &view2, &lock);
        assert!(!changed.matches(selected.selection()));

        let background = NativeMenu::build_from_view(&view, Some(&view2), true, false).unwrap();
        let same = capture_from_view(&view, None, true, false).unwrap();
        assert!(background.selection().matches(&same));
        assert!(!background.selection().matches(selected.selection()));
        assert_eq!(background.selection().item_count(), 0);
        let root = background.extract_items(false);
        assert!(root
            .iter()
            .all(|item| item.sub_init_ms.is_none() && item.children.is_empty()));
        let labels: Vec<_> = root.iter().map(|item| item.clean_label.as_str()).collect();
        assert!(
            labels
                .iter()
                .any(|label| matches!(*label, "表示" | "View" | "並べ替え" | "Sort by" | "Sort")),
            "{labels:?}"
        );
        assert!(
            labels.iter().any(|label| matches!(
                *label,
                "ターミナルで開く" | "Open in Terminal" | "プロパティ" | "Properties"
            )),
            "{labels:?}"
        );

        let this_pc = OwnedPidl::parse("::{20D04FE0-3AEA-1069-A2D8-08002B30309D}").unwrap();
        let virtual_host = BrowserHost::open(&this_pc).unwrap();
        let virtual_view = virtual_host.view().unwrap();
        let virtual_menu = NativeMenu::build_from_view(&virtual_view, None, true, false).unwrap();
        assert!(virtual_menu.root_item_count() > 0);
        assert!(virtual_menu.selection().folder_fs_path().is_none());
        assert!(!virtual_menu.selection().matches(background.selection()));
    }

    #[test]
    fn orphan_separator_positions_removes_leading_trailing_and_consecutive() {
        assert_eq!(orphan_separator_positions(&[true, false, true, true, false, true]), vec![0, 3, 5]);
        assert_eq!(orphan_separator_positions(&[true, true]), vec![0, 1]);
        assert_eq!(orphan_separator_positions(&[false, true, false]), Vec::<u32>::new());
    }

    #[test]
    fn menu_item_filter_matching_handles_verbs_labels_and_bare_rules() {
        assert_eq!(
            menu_item_filter_key(Some("pintohomefile"), "お気に入りに追加"),
            Some("verb:pintohomefile".to_string())
        );
        assert_eq!(
            menu_item_filter_key(None, "7-Zip"),
            Some("label:7-Zip".to_string())
        );
        let rules = vec![
            "verb:PinToHomeFile".to_string(),
            "Verb:Delete".to_string(),
            "label:メモ帳で編集".to_string(),
            "Send with Tailscale...".to_string(),
        ];
        assert!(item_matches_hidden(&rules, Some("pintohomefile"), "Pin to Favorites"));
        assert!(item_matches_hidden(&rules, Some("delete"), "削除(D)"));
        assert!(item_matches_hidden(&rules, None, "メモ帳で編集"));
        assert!(item_matches_hidden(&rules, Some("tailscale"), "Send with Tailscale..."));
        assert!(!item_matches_hidden(&rules, Some("open"), "開く"));
    }

    #[test]
    fn hmenu_pruning_and_customize_append_work_on_real_popup_menu() {
        let menu = OwnedMenu::create_popup().unwrap();
        let append_str = |id: u32, text: &str| {
            let w = wide_null(text);
            unsafe { AppendMenuW(menu.as_handle(), MF_STRING, id as usize, PCWSTR(w.as_ptr())).unwrap() };
        };
        let append_sep = || {
            unsafe { AppendMenuW(menu.as_handle(), WMF_SEPARATOR, 0, PCWSTR::null()).unwrap() };
        };
        append_str(10, "&Open");
        append_sep();
        append_str(11, "HideByVerb");
        append_sep();
        append_str(12, "HideByLabel");
        append_sep();
        append_str(13, "&Properties");

        let cfg = RwmConfig {
            hidden_items: vec!["verb:testverb".into(), "label:HideByLabel".into()],
            show_customize_entry: true,
            ..RwmConfig::default()
        };
        let mut detached = Vec::new();
        apply_to_hmenu(menu.as_handle(), &mut detached, &cfg, |id| {
            (id == 11).then(|| "TestVerb".to_string())
        });

        let mut remaining = Vec::new();
        for i in 0..menu_item_count(menu.as_handle()) {
            let (info, text) = menu_info(menu.as_handle(), i).unwrap();
            if info.fType.contains(MFT_SEPARATOR) {
                remaining.push("---".to_string());
            } else {
                remaining.push(format!("{}:{text}", info.wID));
            }
        }
        assert_eq!(
            remaining,
            vec![
                "10:&Open".to_string(),
                "---".to_string(),
                "13:&Properties".to_string(),
                "---".to_string(),
                format!("{CUSTOMIZE_MENU_CMD_ID}:{}", customize_menu_label()),
            ]
        );
    }
}
