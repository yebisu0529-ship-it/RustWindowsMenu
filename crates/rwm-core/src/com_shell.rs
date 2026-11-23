use std::collections::{HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::mem::size_of;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr;
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use parking_lot::Mutex;
use windows::core::{s, w, IUnknown, Interface, PCSTR, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    GlobalFree, LocalFree, BOOL, ERROR_CANCELLED, HANDLE, HGLOBAL, HLOCAL, HWND, LPARAM, LRESULT,
    RPC_E_CHANGED_MODE, WAIT_FAILED, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::System::Com::{
    CLSIDFromString, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, IDataObject,
    IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, DVASPECT_CONTENT, FORMATETC,
    STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{CF_HDROP, CF_UNICODETEXT};
use windows::Win32::System::Registry::{RegCloseKey, HKEY};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, INFINITE};
use windows::Win32::UI::Input::KeyboardAndMouse::VK_F2;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    AssocQueryKeyW, CommandLineToArgvW, FileOperation, IContextMenu, IContextMenu2, IContextMenu3,
    IFileOperation, ILFindLastID, ILGetSize, IShellExtInit, IShellFolder, IShellItem, IShellLinkW,
    SHBindToObject, SHBindToParent, SHChangeNotify, SHCreateDataObject,
    SHCreateItemFromParsingName, SHParseDisplayName, ShellExecuteExW, ShellLink,
    ASSOCF_INIT_DEFAULTTOSTAR, ASSOCKEY_CLASS, CFSTR_PREFERREDDROPEFFECT, CFSTR_SHELLIDLIST,
    CMF_EXPLORE, CMF_NORMAL, CMF_SYNCCASCADEMENU, CMINVOKECOMMANDINFO, CMINVOKECOMMANDINFOEX,
    DROPFILES, FOF_ALLOWUNDO, SEE_MASK_INVOKEIDLIST, SEE_MASK_NOASYNC, SHCNE_UPDATEDIR,
    SHCNF_PATHW, SHELLEXECUTEINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, CreateWindowExW, DestroyMenu, DestroyWindow, DispatchMessageW,
    GetMenuItemCount, GetMenuItemInfoW, IsWindow, MsgWaitForMultipleObjectsEx, PeekMessageW,
    PostMessageW, TranslateMessage, HMENU, HWND_MESSAGE, MENUITEMINFOW, MFS_CHECKED, MFS_DEFAULT,
    MFS_DISABLED, MFT_SEPARATOR, MIIM_BITMAP, MIIM_FTYPE, MIIM_ID, MIIM_STATE, MIIM_STRING,
    MIIM_SUBMENU, MSG, MWMO_INPUTAVAILABLE, PM_NOREMOVE, PM_REMOVE, QS_ALLINPUT, QS_SENDMESSAGE,
    SW_SHOWNORMAL, WM_INITMENUPOPUP, WM_KEYDOWN, WM_KEYUP,
};

use crate::config::RwmConfig;
use crate::engine::{is_japanese, snapshot};
use crate::icons::IconCache;
use crate::model::{
    split_label_and_shortcut, BenchmarkReport, CoreVerb, EntryAction, EntrySource, HandlerPolicy,
    HandlerStatus, HandlerTiming, HydrationEvent, MenuEntry, MenuItem, SelectionTarget,
};
use crate::registry::{DiscoveredComHandler, RegistryVerbCache};

const FIRST_COMMAND_ID: u32 = 1000;
const LAST_COMMAND_ID: u32 = 9999;
const FIRST_HYDRATED_ITEM_ID: u32 = 0x8000_0000;
const CMIC_MASK_UNICODE: u32 = 0x4000;
const CMIC_MASK_NOASYNC: u32 = SEE_MASK_NOASYNC;

#[derive(Debug, thiserror::Error)]
pub enum ShellError {
    #[error(transparent)]
    Windows(#[from] windows::core::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("STA worker is unavailable")]
    WorkerUnavailable,
    #[error("shell command {cmd_offset} for handler {handler_index} in session {session_id} is unavailable")]
    UnknownCommand {
        session_id: u64,
        handler_index: usize,
        cmd_offset: u32,
    },
    #[error("no paths are selected")]
    EmptySelection,
    #[error("no window is available for rename")]
    RenameUnavailable,
    #[error("command has no executable")]
    EmptyCommand,
}

fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain([0]).collect()
}

struct Apartment(bool);

impl Apartment {
    fn initialize(allow_existing_mta: bool) -> Result<Self, ShellError> {
        let result = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if allow_existing_mta && result == RPC_E_CHANGED_MODE {
            return Ok(Self(false));
        }
        result.ok()?;
        Ok(Self(true))
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

struct Event(OwnedHandle);

impl Event {
    fn new() -> Result<Self, ShellError> {
        let event = unsafe { CreateEventW(None, false, false, PCWSTR::null())? };
        Ok(Self(unsafe { OwnedHandle::from_raw_handle(event.0) }))
    }

    fn handle(&self) -> HANDLE {
        HANDLE(self.0.as_raw_handle())
    }

    fn signal(&self) {
        let _ = unsafe { SetEvent(self.handle()) };
    }
}

struct Reply {
    sender: mpsc::Sender<Result<(), ShellError>>,
    ready: Arc<Event>,
}

impl Drop for Reply {
    fn drop(&mut self) {
        self.ready.signal();
    }
}

struct HydrationRequest {
    session_id: u64,
    target: SelectionTarget,
    handlers: Vec<DiscoveredComHandler>,
    config: Arc<RwmConfig>,
    notify_hwnd: isize,
    notify_msg: u32,
    events: mpsc::Sender<HydrationEvent>,
}

impl HydrationRequest {
    fn emit(&self, event: HydrationEvent) -> bool {
        if self.events.send(event).is_err() {
            return false;
        }
        if self.notify_hwnd != 0 {
            let _ = unsafe {
                PostMessageW(
                    HWND(self.notify_hwnd as *mut _),
                    self.notify_msg,
                    WPARAM(self.session_id as usize),
                    LPARAM(0),
                )
            };
        }
        true
    }
}

enum Request {
    Hydrate(HydrationRequest),
    Invoke {
        session_id: u64,
        handler_index: usize,
        cmd_offset: u32,
        owner_hwnd: isize,
        parent_dir: Option<PathBuf>,
        reply: Reply,
    },
    Close(u64),
    Shutdown,
}

struct WorkerThread {
    requests: mpsc::Sender<Request>,
    wake: Arc<Event>,
    join: JoinHandle<()>,
}

impl WorkerThread {
    fn send(&self, request: Request) -> Result<(), ShellError> {
        self.requests
            .send(request)
            .map_err(|_| ShellError::WorkerUnavailable)?;
        self.wake.signal();
        Ok(())
    }
}

pub struct ComStaWorker {
    thread: Mutex<Option<WorkerThread>>,
}

impl Default for ComStaWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl ComStaWorker {
    pub fn new() -> Self {
        let thread = Self::spawn()
            .map_err(|error| {
                tracing::error!(%error, "failed to start shell STA worker");
            })
            .ok();
        Self {
            thread: Mutex::new(thread),
        }
    }

    fn spawn() -> Result<WorkerThread, ShellError> {
        let (requests, receiver) = mpsc::channel();
        let wake = Arc::new(Event::new()?);
        let worker_wake = Arc::clone(&wake);
        let join = thread::Builder::new()
            .name("rwm-shell-sta".into())
            .spawn(move || match Apartment::initialize(false) {
                Ok(_apartment) => run_sta(receiver, &worker_wake),
                Err(error) => tracing::error!(%error, "failed to initialize shell STA"),
            })?;
        Ok(WorkerThread {
            requests,
            wake,
            join,
        })
    }

    fn send(&self, request: Request) -> Result<(), ShellError> {
        self.thread
            .lock()
            .as_ref()
            .ok_or(ShellError::WorkerUnavailable)?
            .send(request)
    }

    pub fn start_hydration(
        &self,
        session_id: u64,
        target: SelectionTarget,
        handlers: Vec<DiscoveredComHandler>,
        config: Arc<RwmConfig>,
        notify_hwnd: isize,
        notify_msg: u32,
    ) -> mpsc::Receiver<HydrationEvent> {
        let (events, receiver) = mpsc::channel();
        let _ = self.send(Request::Hydrate(HydrationRequest {
            session_id,
            target,
            handlers,
            config,
            notify_hwnd,
            notify_msg,
            events,
        }));
        receiver
    }

    pub fn invoke(
        &self,
        session_id: u64,
        handler_index: usize,
        cmd_offset: u32,
        owner_hwnd: isize,
        parent_dir: Option<PathBuf>,
    ) -> Result<(), ShellError> {
        let (sender, result) = mpsc::channel();
        let ready = Arc::new(Event::new()?);
        self.send(Request::Invoke {
            session_id,
            handler_index,
            cmd_offset,
            owner_hwnd,
            parent_dir,
            reply: Reply {
                sender,
                ready: Arc::clone(&ready),
            },
        })?;
        loop {
            // Extensions can synchronously send messages to the calling window.
            let wait = unsafe {
                MsgWaitForMultipleObjectsEx(
                    Some(&[ready.handle()]),
                    INFINITE,
                    QS_SENDMESSAGE,
                    MWMO_INPUTAVAILABLE,
                )
            };
            if wait == WAIT_OBJECT_0 {
                return result
                    .try_recv()
                    .map_err(|_| ShellError::WorkerUnavailable)?;
            }
            if wait == WAIT_FAILED {
                return Err(windows::core::Error::from_win32().into());
            }
            let _ = unsafe { PeekMessageW(&mut MSG::default(), None, 0, 0, PM_NOREMOVE) };
        }
    }

    pub fn close_session(&self, session_id: u64) {
        let _ = self.send(Request::Close(session_id));
    }

    pub fn shutdown(&self) {
        let thread = self.thread.lock().take();
        if let Some(thread) = thread {
            let _ = thread.send(Request::Shutdown);
            let _ = thread.join.join();
        }
    }

    /// Signals the STA worker to exit without blocking the caller's window thread.
    pub fn request_shutdown(&self) {
        let thread = self.thread.lock().take();
        if let Some(thread) = thread {
            let _ = thread.send(Request::Shutdown);
            drop(thread.join);
        }
    }
}

impl Drop for ComStaWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct PendingHydration {
    request: HydrationRequest,
    handlers: std::iter::Enumerate<std::vec::IntoIter<DiscoveredComHandler>>,
    selection: Option<Result<ShellSelection, ShellError>>,
    next_item_id: u32,
    start: Instant,
}

impl PendingHydration {
    fn new(mut request: HydrationRequest) -> Self {
        let handlers = std::mem::take(&mut request.handlers)
            .into_iter()
            .enumerate();
        Self {
            request,
            handlers,
            selection: None,
            next_item_id: FIRST_HYDRATED_ITEM_ID,
            start: Instant::now(),
        }
    }

    fn step(&mut self, live: &mut Vec<LiveComHandler>) -> bool {
        let request = &self.request;
        let Some((index, handler)) = self.handlers.next() else {
            request.emit(HydrationEvent::Completed {
                session_id: request.session_id,
                total_us: self.start.elapsed().as_micros() as u64,
            });
            return false;
        };
        let start = Instant::now();
        let policy = request
            .config
            .handlers
            .policy_for(&handler.clsid, &handler.name);
        let mut entries = Vec::new();
        let status = if policy == HandlerPolicy::Disabled {
            HandlerStatus::SkippedPolicy
        } else {
            let selection = self
                .selection
                .get_or_insert_with(|| ShellSelection::new(&request.target));
            let result = match selection {
                Ok(selection) => {
                    query_handler(request, selection, &handler, index, &mut self.next_item_id)
                }
                Err(error) => Err(format!("Prepare shell selection: {error}")),
            };
            match result {
                Ok((live_handler, extracted)) if !extracted.is_empty() => {
                    live.push(live_handler);
                    entries = extracted;
                    HandlerStatus::Ok
                }
                Ok(_) => HandlerStatus::Empty,
                Err(error) => HandlerStatus::Failed(error),
            }
        };
        let timing = HandlerTiming {
            clsid: handler.clsid.clone(),
            name: handler.name.clone(),
            dll_path: handler.dll_path,
            duration_us: start.elapsed().as_micros() as u64,
            item_count: count_items(&entries),
            status,
        };
        request.emit(HydrationEvent::GroupReady {
            session_id: request.session_id,
            group_name: handler.name,
            clsid: handler.clsid,
            policy,
            entries,
            timing,
        })
    }
}

fn run_sta(requests: mpsc::Receiver<Request>, wake: &Event) {
    let mut sessions: HashMap<u64, Vec<LiveComHandler>> = HashMap::new();
    let mut pending: VecDeque<PendingHydration> = VecDeque::new();
    loop {
        let mut message = MSG::default();
        unsafe {
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        loop {
            match requests.try_recv() {
                Ok(Request::Hydrate(request)) => {
                    let id = request.session_id;
                    pending.retain(|job| job.request.session_id != id);
                    sessions.insert(id, Vec::new());
                    pending.push_back(PendingHydration::new(request));
                }
                Ok(Request::Invoke {
                    session_id,
                    handler_index,
                    cmd_offset,
                    owner_hwnd,
                    parent_dir,
                    reply,
                }) => {
                    let result = sessions
                        .get(&session_id)
                        .and_then(|handlers| {
                            handlers
                                .iter()
                                .find(|handler| handler.index == handler_index)
                        })
                        .filter(|handler| handler.commands.contains(&cmd_offset))
                        .ok_or(ShellError::UnknownCommand {
                            session_id,
                            handler_index,
                            cmd_offset,
                        })
                        .and_then(|handler| {
                            handler.invoke(
                                cmd_offset,
                                HWND(owner_hwnd as *mut _),
                                parent_dir.as_deref(),
                            )
                        });
                    let _ = reply.sender.send(result);
                }
                Ok(Request::Close(id)) => {
                    pending.retain(|job| job.request.session_id != id);
                    sessions.remove(&id);
                }
                Ok(Request::Shutdown) | Err(mpsc::TryRecvError::Disconnected) => return,
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        if let Some(mut job) = pending.pop_front() {
            let id = job.request.session_id;
            if job.step(sessions.get_mut(&id).expect("active hydration session")) {
                pending.push_back(job);
            }
        } else if unsafe {
            MsgWaitForMultipleObjectsEx(
                Some(&[wake.handle()]),
                INFINITE,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            )
        } == WAIT_FAILED
        {
            return;
        }
    }
}

struct Pidl(*mut ITEMIDLIST);

impl Pidl {
    fn parse(path: &Path) -> windows::core::Result<Self> {
        let path = wide(path);
        let mut pidl = Self(ptr::null_mut());
        unsafe { SHParseDisplayName(PCWSTR(path.as_ptr()), None, &mut pidl.0, 0, None)? };
        Ok(pidl)
    }
}

impl Drop for Pidl {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0.cast())) };
    }
}

fn data_from_item(item: &Pidl) -> windows::core::Result<IDataObject> {
    let mut child = ptr::null_mut();
    unsafe {
        let parent: IShellFolder = SHBindToParent(item.0, Some(&mut child))?;
        parent.GetUIObjectOf(HWND::default(), &[child.cast_const()], None)
    }
}

struct ShellSelection {
    background: Option<Pidl>,
    data: Option<IDataObject>,
    class_key: Option<AssociationKey>,
}

struct AssociationKey(HKEY);

impl Drop for AssociationKey {
    fn drop(&mut self) {
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

fn association_key(target: &SelectionTarget) -> Option<AssociationKey> {
    let association = match target {
        SelectionTarget::SingleFolder(_) => "Directory".to_owned(),
        SelectionTarget::Drive(_) => "Drive".to_owned(),
        _ => target.extension_lower().unwrap_or_else(|| "*".to_owned()),
    };
    let association = wide(association);
    unsafe {
        AssocQueryKeyW(
            ASSOCF_INIT_DEFAULTTOSTAR,
            ASSOCKEY_CLASS,
            PCWSTR(association.as_ptr()),
            PCWSTR::null(),
        )
    }
    .ok()
    .map(AssociationKey)
}

impl ShellSelection {
    fn new(target: &SelectionTarget) -> Result<Self, ShellError> {
        if target.is_background() {
            let folder = Pidl::parse(target.primary_path())?;
            let data = data_from_item(&folder).ok();
            return Ok(Self {
                background: Some(folder),
                data,
                class_key: None,
            });
        }
        let paths = target.selected_paths();
        let pidls = paths
            .iter()
            .map(|path| Pidl::parse(path))
            .collect::<Result<Vec<_>, _>>()?;
        let data = match pidls.as_slice() {
            [] => return Err(ShellError::EmptySelection),
            [item] => data_from_item(item)?,
            _ => {
                let parent = target.parent_dir();
                let same_folder = paths
                    .iter()
                    .all(|path| path.parent() == Some(parent.as_path()));
                if same_folder {
                    unsafe {
                        let shell_folder: IShellFolder = SHBindToParent(pidls[0].0, None)?;
                        let children: Vec<_> = pidls
                            .iter()
                            .map(|pidl| ILFindLastID(pidl.0).cast_const())
                            .collect();
                        shell_folder
                            .GetUIObjectOf(HWND::default(), &children, None)
                            .or_else(|_| {
                                let folder = Pidl::parse(&parent)?;
                                SHCreateDataObject(Some(folder.0), Some(&children), None)
                            })?
                    }
                } else {
                    data_from_items_in_different_folders(&pidls, &paths)?
                }
            }
        };
        Ok(Self {
            background: None,
            data: Some(data),
            class_key: association_key(target),
        })
    }

    fn initialize(&self, init: &IShellExtInit) -> windows::core::Result<()> {
        unsafe {
            if let Some(folder) = &self.background {
                init.Initialize(Some(folder.0), None, HKEY::default())
                    .or_else(|error| {
                        if let Some(data) = &self.data {
                            init.Initialize(Some(folder.0), data, HKEY::default())
                        } else {
                            Err(error)
                        }
                    })
            } else {
                init.Initialize(
                    None,
                    self.data.as_ref(),
                    self.class_key.as_ref().map_or(HKEY::default(), |key| key.0),
                )
            }
        }
    }
}

fn data_from_items_in_different_folders(
    pidls: &[Pidl],
    paths: &[PathBuf],
) -> Result<IDataObject, ShellError> {
    let data = unsafe { SHCreateDataObject(None, None, None)? };
    file_drop_memory(paths)?.set_data(&data, CF_HDROP.0)?;

    // A CIDA can use the desktop as its parent. Each child is then an absolute PIDL.
    let count = pidls.len() as u32;
    let mut cida = vec![0; size_of::<u32>() * (pidls.len() + 2)];
    cida[..4].copy_from_slice(&count.to_le_bytes());
    let desktop_offset = (cida.len() as u32).to_le_bytes();
    cida[4..8].copy_from_slice(&desktop_offset);
    cida.extend([0, 0]);
    for (index, pidl) in pidls.iter().enumerate() {
        let slot = (index + 2) * size_of::<u32>();
        let offset = (cida.len() as u32).to_le_bytes();
        cida[slot..slot + 4].copy_from_slice(&offset);
        let bytes =
            unsafe { std::slice::from_raw_parts(pidl.0.cast(), ILGetSize(Some(pidl.0)) as usize) };
        cida.extend_from_slice(bytes);
    }
    let format = unsafe { RegisterClipboardFormatW(CFSTR_SHELLIDLIST) };
    if format == 0 {
        return Err(windows::core::Error::from_win32().into());
    }
    GlobalMemory::with_bytes(cida.len(), |bytes| unsafe {
        ptr::copy_nonoverlapping(cida.as_ptr(), bytes, cida.len());
    })?
    .set_data(&data, format as u16)?;
    Ok(data)
}

struct Popup(HMENU);

impl Drop for Popup {
    fn drop(&mut self) {
        let _ = unsafe { DestroyMenu(self.0) };
    }
}

struct LiveComHandler {
    index: usize,
    commands: Vec<u32>,
    menu: Option<IContextMenu>,
    popup: Popup,
}

impl Drop for LiveComHandler {
    fn drop(&mut self) {
        drop(self.menu.take());
    }
}

impl LiveComHandler {
    fn invoke(&self, cmd_offset: u32, hwnd: HWND, dir: Option<&Path>) -> Result<(), ShellError> {
        invoke_context_menu(
            self.menu.as_ref().expect("live COM handler"),
            hwnd,
            PCSTR(cmd_offset as usize as *const u8),
            PCWSTR(cmd_offset as usize as *const u16),
            dir,
        )
    }
}

fn query_handler(
    request: &HydrationRequest,
    selection: &ShellSelection,
    handler: &DiscoveredComHandler,
    index: usize,
    next_item_id: &mut u32,
) -> Result<(LiveComHandler, Vec<MenuEntry>), String> {
    let clsid = wide(handler.clsid.trim());
    let clsid = unsafe { CLSIDFromString(PCWSTR(clsid.as_ptr())) }
        .map_err(|error| format!("CLSIDFromString: {error}"))?;
    let object: IUnknown = unsafe { CoCreateInstance(&clsid, None, CLSCTX_INPROC_SERVER) }
        .map_err(|error| format!("CoCreateInstance: {error}"))?;
    if let Ok(init) = object.cast::<IShellExtInit>() {
        selection
            .initialize(&init)
            .map_err(|error| format!("IShellExtInit::Initialize: {error}"))?;
    }
    let menu = object
        .cast::<IContextMenu>()
        .map_err(|error| format!("QueryInterface(IContextMenu): {error}"))?;
    drop(object);
    let popup =
        Popup(unsafe { CreatePopupMenu() }.map_err(|error| format!("CreatePopupMenu: {error}"))?);
    let mut live = LiveComHandler {
        index,
        commands: Vec::new(),
        menu: Some(menu),
        popup,
    };
    let menu = live.menu.as_ref().expect("live COM handler");
    let hmenu = live.popup.0;
    unsafe {
        menu.QueryContextMenu(
            hmenu,
            0,
            FIRST_COMMAND_ID,
            LAST_COMMAND_ID,
            CMF_NORMAL | CMF_EXPLORE | CMF_SYNCCASCADEMENU,
        )
    }
    .map_err(|error| format!("IContextMenu::QueryContextMenu: {error}"))?;
    let mut reader = MenuReader {
        menu,
        session_id: request.session_id,
        handler_index: index,
        source: EntrySource::ComHandler {
            clsid: handler.clsid.clone(),
            name: handler.name.clone(),
        },
        config: &request.config,
        commands: &mut live.commands,
        next_item_id,
    };
    let entries = reader.read(hmenu, 0);
    Ok((live, entries))
}

struct MenuReader<'a> {
    menu: &'a IContextMenu,
    session_id: u64,
    handler_index: usize,
    source: EntrySource,
    config: &'a RwmConfig,
    commands: &'a mut Vec<u32>,
    next_item_id: &'a mut u32,
}

impl MenuReader<'_> {
    fn read(&mut self, hmenu: HMENU, depth: u32) -> Vec<MenuEntry> {
        let count = unsafe { GetMenuItemCount(hmenu) };
        let mut entries = Vec::new();
        for index in 0..count.max(0) as u32 {
            let mut info = MENUITEMINFOW {
                cbSize: size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_FTYPE | MIIM_ID | MIIM_STATE | MIIM_STRING | MIIM_SUBMENU | MIIM_BITMAP,
                ..Default::default()
            };
            if unsafe { GetMenuItemInfoW(hmenu, index, true, &mut info) }.is_err() {
                continue;
            }
            if info.fType.contains(MFT_SEPARATOR) {
                if matches!(entries.last(), Some(MenuEntry::Item(_))) {
                    entries.push(MenuEntry::Separator);
                }
                continue;
            }

            let mut label_buffer = vec![0; info.cch as usize + 1];
            info.dwTypeData = PWSTR(label_buffer.as_mut_ptr());
            info.cch = label_buffer.len() as u32;
            if unsafe { GetMenuItemInfoW(hmenu, index, true, &mut info) }.is_err() {
                continue;
            }
            let raw = String::from_utf16_lossy(&label_buffer[..info.cch as usize]);
            let (label, mnemonic, shortcut) = split_label_and_shortcut(&raw);
            if label.is_empty() {
                continue;
            }
            let enabled = info.fState.0 & MFS_DISABLED.0 == 0;
            let action = if !info.hSubMenu.is_invalid() {
                if depth >= 6 {
                    continue;
                }
                self.init_submenu(info.hSubMenu, index);
                let children = self.read(info.hSubMenu, depth + 1);
                if children.is_empty() {
                    EntryAction::Noop
                } else {
                    EntryAction::Submenu(children)
                }
            } else if (FIRST_COMMAND_ID..=LAST_COMMAND_ID).contains(&info.wID) {
                let cmd_offset = info.wID - FIRST_COMMAND_ID;
                if enabled {
                    self.commands.push(cmd_offset);
                }
                EntryAction::ComCommand {
                    session_id: self.session_id,
                    handler_index: self.handler_index,
                    cmd_offset,
                }
            } else {
                EntryAction::Noop
            };
            let id = *self.next_item_id;
            *self.next_item_id += 1;
            entries.push(MenuEntry::Item(MenuItem {
                id,
                label,
                mnemonic,
                shortcut_hint: shortcut.filter(|_| self.config.appearance.show_shortcuts),
                icon: self
                    .config
                    .appearance
                    .show_icons
                    .then(|| IconCache::from_hbitmap(info.hbmpItem))
                    .flatten()
                    .map(Arc::new),
                enabled: enabled && !matches!(action, EntryAction::Noop),
                checked: info.fState.contains(MFS_CHECKED),
                is_default: info.fState.contains(MFS_DEFAULT),
                source: self.source.clone(),
                action,
            }));
        }
        if matches!(entries.last(), Some(MenuEntry::Separator)) {
            entries.pop();
        }
        entries
    }

    fn init_submenu(&self, submenu: HMENU, index: u32) {
        // Handlers like SendTo and Sharing pre-seed a dummy placeholder child item, so
        // WM_INITMENUPOPUP must always be dispatched before reading child entries.
        let handle = WPARAM(submenu.0 as usize);
        let position = LPARAM((index & 0xffff) as isize);
        unsafe {
            if let Ok(menu) = self.menu.cast::<IContextMenu3>() {
                let mut result = LRESULT::default();
                if menu
                    .HandleMenuMsg2(WM_INITMENUPOPUP, handle, position, Some(&mut result))
                    .is_ok()
                {
                    return;
                }
            }
            if let Ok(menu) = self.menu.cast::<IContextMenu2>() {
                let _ = menu.HandleMenuMsg(WM_INITMENUPOPUP, handle, position);
            }
        }
    }
}

fn count_items(entries: &[MenuEntry]) -> usize {
    entries
        .iter()
        .map(|entry| match entry {
            MenuEntry::Separator => 0,
            MenuEntry::Item(item) => match &item.action {
                EntryAction::Submenu(children) => 1 + count_items(children),
                _ => 1,
            },
        })
        .sum()
}

pub fn benchmark_target(
    target: &SelectionTarget,
    config: &RwmConfig,
    registry: &RegistryVerbCache,
    icons: &IconCache,
) -> Result<BenchmarkReport, ShellError> {
    let worker = ComStaWorker::new();
    let japanese = is_japanese();
    let (cold_snapshot, handlers) = snapshot(
        1,
        target.clone(),
        false,
        japanese,
        config,
        registry,
        icons,
    );
    let stage0_cold_us = cold_snapshot.stage0_us;
    let stage0_item_count = count_items(&cold_snapshot.entries);

    let mut warm_timings = [0u64; 5];
    for slot in &mut warm_timings {
        let (warm, _) = snapshot(
            1,
            target.clone(),
            false,
            japanese,
            config,
            registry,
            icons,
        );
        *slot = warm.stage0_us;
    }
    warm_timings.sort_unstable();
    let stage0_us = warm_timings[2];

    let events =
        worker.start_hydration(1, target.clone(), handlers, Arc::new(config.clone()), 0, 0);
    let mut com_handlers = Vec::new();
    loop {
        match events.recv().map_err(|_| ShellError::WorkerUnavailable)? {
            HydrationEvent::GroupReady { timing, .. } => com_handlers.push(timing),
            HydrationEvent::Completed { total_us, .. } => {
                return Ok(BenchmarkReport {
                    target_display: target.primary_path().display().to_string(),
                    is_background: target.is_background(),
                    stage0_us,
                    stage0_cold_us,
                    stage0_item_count,
                    com_handlers,
                    total_com_us: total_us,
                    speedup_factor: 1.0 + total_us as f64 / stage0_us.max(1) as f64,
                });
            }
        }
    }
}

fn invoke_context_menu(
    menu: &IContextMenu,
    hwnd: HWND,
    verb: PCSTR,
    verb_wide: PCWSTR,
    dir: Option<&Path>,
) -> Result<(), ShellError> {
    let dir = dir.map(wide);
    let info = CMINVOKECOMMANDINFOEX {
        cbSize: size_of::<CMINVOKECOMMANDINFOEX>() as u32,
        fMask: CMIC_MASK_UNICODE | CMIC_MASK_NOASYNC,
        hwnd,
        lpVerb: verb,
        lpVerbW: verb_wide,
        lpDirectoryW: dir
            .as_ref()
            .map_or(PCWSTR::null(), |dir| PCWSTR(dir.as_ptr())),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe { menu.InvokeCommand(ptr::from_ref(&info).cast::<CMINVOKECOMMANDINFO>())? };
    Ok(())
}

pub fn execute_core_verb(
    verb: CoreVerb,
    target: &SelectionTarget,
    owner_hwnd: HWND,
    config: &RwmConfig,
) -> Result<(), ShellError> {
    use CoreVerb::*;
    match verb {
        Open | Properties => {
            let _apartment = Apartment::initialize(true)?;
            let (verb_name, mask) = if verb == Properties {
                (w!("properties"), SEE_MASK_INVOKEIDLIST)
            } else {
                (PCWSTR::null(), 0)
            };
            let dir = target.parent_dir();
            for path in target.selected_paths() {
                shell_execute(&path, verb_name, owner_hwnd, &dir, mask)?;
            }
            Ok(())
        }
        OpenInTerminal => {
            let dir = destination_directory(target);
            let launch = build_terminal_invocation(&config.behavior.terminal_command, target, &dir)
                .and_then(|(program, args)| spawn(&program, args, &dir));
            launch.or_else(|_| spawn(OsStr::new("powershell.exe"), [], &dir))
        }
        CopyPath => copy_paths_as_text(target, owner_hwnd),
        Copy | Cut => copy_file_drop(target, owner_hwnd, verb == Cut),
        Paste | PasteShortcut => paste(target, owner_hwnd, verb == PasteShortcut),
        Delete => delete(target, owner_hwnd),
        Rename => {
            if !unsafe { IsWindow(owner_hwnd) }.as_bool() {
                return Err(ShellError::RenameUnavailable);
            }
            unsafe {
                PostMessageW(owner_hwnd, WM_KEYDOWN, WPARAM(VK_F2.0 as usize), LPARAM(1))?;
                PostMessageW(
                    owner_hwnd,
                    WM_KEYUP,
                    WPARAM(VK_F2.0 as usize),
                    LPARAM(0xc000_0001u32 as isize),
                )?;
            }
            Ok(())
        }
        CreateShortcut => create_shortcuts(target),
        Refresh => {
            let dir = wide(destination_directory(target));
            unsafe {
                SHChangeNotify(
                    SHCNE_UPDATEDIR,
                    SHCNF_PATHW,
                    Some(dir.as_ptr().cast()),
                    None,
                )
            };
            Ok(())
        }
    }
}

fn destination_directory(target: &SelectionTarget) -> PathBuf {
    if matches!(target, SelectionTarget::SingleFolder(_)) {
        target.primary_path().to_path_buf()
    } else {
        target.parent_dir()
    }
}

fn shell_execute(
    path: &Path,
    verb: PCWSTR,
    owner: HWND,
    dir: &Path,
    mask: u32,
) -> Result<(), ShellError> {
    let path = wide(path);
    let dir = wide(dir);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: mask | SEE_MASK_NOASYNC,
        hwnd: owner,
        lpVerb: verb,
        lpFile: PCWSTR(path.as_ptr()),
        lpDirectory: PCWSTR(dir.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe { ShellExecuteExW(&mut info)? };
    Ok(())
}

pub fn execute_static_command(
    verb: &str,
    command: &str,
    target: &SelectionTarget,
    owner_hwnd: HWND,
) -> Result<(), ShellError> {
    execute_static_command_in(verb, command, target, owner_hwnd, &target.parent_dir())
}

pub(crate) fn execute_static_command_in(
    verb: &str,
    command: &str,
    target: &SelectionTarget,
    owner_hwnd: HWND,
    working_dir: &Path,
) -> Result<(), ShellError> {
    let paths = target.selected_paths();
    if command.trim().is_empty() {
        let _apartment = Apartment::initialize(true)?;
        let verb = wide(verb);
        for path in paths {
            shell_execute(
                &path,
                PCWSTR(verb.as_ptr()),
                owner_hwnd,
                working_dir,
                SEE_MASK_INVOKEIDLIST,
            )?;
        }
        return Ok(());
    }

    let words = split_command(command)?;
    let (program, args) = words.split_first().ok_or(ShellError::EmptyCommand)?;
    let expand = |path: &Path| {
        let mut expanded = Vec::new();
        for arg in args {
            if arg == "%*" {
                expanded.extend(paths.iter().map(|path| path.as_os_str().to_owned()));
            } else {
                expanded.push(expand_static(arg, path, working_dir));
            }
        }
        spawn(
            &expand_static(program, path, working_dir),
            expanded,
            working_dir,
        )
    };
    if !args.iter().any(|arg| arg == "%*")
        && words.iter().any(|word| {
            word.encode_wide()
                .collect::<Vec<_>>()
                .windows(2)
                .any(|pair| pair[0] == b'%' as u16 && is_item_placeholder(pair[1]))
        })
    {
        for path in &paths {
            expand(path)?;
        }
        Ok(())
    } else {
        expand(target.primary_path())
    }
}

fn is_item_placeholder(unit: u16) -> bool {
    matches!(unit, 0x31 | 0x4c | 0x6c | 0x56 | 0x76)
}

fn expand_static(arg: &OsStr, path: &Path, dir: &Path) -> OsString {
    let units: Vec<_> = arg.encode_wide().collect();
    let mut expanded = Vec::new();
    let mut remaining = units.as_slice();
    while let [unit, rest @ ..] = remaining {
        remaining = rest;
        if *unit == b'%' as u16 {
            if let [token, rest @ ..] = rest {
                let replacement = match *token {
                    token if is_item_placeholder(token) => Some(path),
                    0x57 | 0x77 => Some(dir),
                    _ => None,
                };
                if let Some(replacement) = replacement {
                    expanded.extend(replacement.as_os_str().encode_wide());
                    remaining = rest;
                    continue;
                }
            }
        }
        expanded.push(*unit);
    }
    OsString::from_wide(&expanded)
}

pub fn execute_custom_command(
    command: &str,
    args: &[String],
    working_dir: &Path,
    target: &SelectionTarget,
) -> Result<(), ShellError> {
    let program = expand_custom(
        OsStr::new(command.trim().trim_matches('"')),
        target,
        working_dir,
    );
    let mut expanded = Vec::new();
    for arg in args {
        match arg.as_str() {
            "{paths}" | "\"{paths}\"" => expanded.extend(
                target
                    .selected_paths()
                    .into_iter()
                    .map(PathBuf::into_os_string),
            ),
            _ => {
                // Configuration examples quote whole placeholders. Command supplies argv quoting.
                let arg = arg
                    .strip_prefix('"')
                    .and_then(|arg| arg.strip_suffix('"'))
                    .filter(|arg| matches!(*arg, "{path}" | "{dir}"))
                    .unwrap_or(arg);
                expanded.push(expand_custom(OsStr::new(arg), target, working_dir));
            }
        }
    }
    spawn(&program, expanded, working_dir)
}

fn is_windows_terminal_program(program: &OsStr) -> bool {
    Path::new(program)
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("wt") || name.eq_ignore_ascii_case("wt.exe"))
}

fn escape_wt_semicolons(value: &OsStr) -> OsString {
    let mut out = Vec::new();
    for unit in value.encode_wide() {
        if unit == b';' as u16 {
            out.push(b'\\' as u16);
        }
        out.push(unit);
    }
    OsString::from_wide(&out)
}

fn build_terminal_invocation(
    template: &str,
    target: &SelectionTarget,
    dir: &Path,
) -> Result<(OsString, Vec<OsString>), ShellError> {
    let words = split_command(template)?;
    let (raw_program, raw_args) = words.split_first().ok_or(ShellError::EmptyCommand)?;
    let program = expand_custom(raw_program, target, dir);
    let wt = is_windows_terminal_program(&program);
    let expanded_args = raw_args
        .iter()
        .map(|arg| expand_custom_with_escape(arg, target, dir, wt))
        .collect();
    Ok((program, expanded_args))
}

fn expand_custom(arg: &OsStr, target: &SelectionTarget, dir: &Path) -> OsString {
    expand_custom_with_escape(arg, target, dir, false)
}

fn expand_custom_with_escape(
    arg: &OsStr,
    target: &SelectionTarget,
    dir: &Path,
    escape_semicolons_in_paths: bool,
) -> OsString {
    const PATH: [u16; 6] = [123, 112, 97, 116, 104, 125];
    const DIR: [u16; 5] = [123, 100, 105, 114, 125];
    let units: Vec<_> = arg.encode_wide().collect();
    let mut expanded = Vec::new();
    let mut remaining = units.as_slice();
    while let [unit, rest @ ..] = remaining {
        if let Some(tail) = remaining.strip_prefix(&PATH) {
            let val = if escape_semicolons_in_paths {
                escape_wt_semicolons(target.primary_path().as_os_str())
            } else {
                target.primary_path().as_os_str().to_os_string()
            };
            expanded.extend(val.encode_wide());
            remaining = tail;
        } else if let Some(tail) = remaining.strip_prefix(&DIR) {
            let val = if escape_semicolons_in_paths {
                escape_wt_semicolons(dir.as_os_str())
            } else {
                dir.as_os_str().to_os_string()
            };
            expanded.extend(val.encode_wide());
            remaining = tail;
        } else {
            expanded.push(*unit);
            remaining = rest;
        }
    }
    OsString::from_wide(&expanded)
}

fn split_command(command: &str) -> Result<Vec<OsString>, ShellError> {
    let command = command.trim();
    if command.is_empty() {
        return Err(ShellError::EmptyCommand);
    }
    let command = wide(command);
    let mut count = 0;
    let args = unsafe { CommandLineToArgvW(PCWSTR(command.as_ptr()), &mut count) };
    if args.is_null() {
        return Err(windows::core::Error::from_win32().into());
    }
    let words = unsafe {
        let words = std::slice::from_raw_parts(args, count as usize)
            .iter()
            .map(|arg| OsString::from_wide(arg.as_wide()))
            .collect();
        let _ = LocalFree(HLOCAL(args.cast()));
        words
    };
    Ok(words)
}

fn spawn(
    program: &OsStr,
    args: impl IntoIterator<Item = OsString>,
    dir: &Path,
) -> Result<(), ShellError> {
    if program.is_empty() {
        return Err(ShellError::EmptyCommand);
    }
    let mut command = Command::new(program);
    command.args(args);
    if !dir.as_os_str().is_empty() {
        command.current_dir(dir);
    }
    command.spawn()?;
    Ok(())
}

pub fn clipboard_has_files() -> bool {
    unsafe {
        if IsClipboardFormatAvailable(CF_HDROP.0.into()).is_ok() {
            return true;
        }
        let shell_id_list = RegisterClipboardFormatW(CFSTR_SHELLIDLIST);
        shell_id_list != 0 && IsClipboardFormatAvailable(shell_id_list).is_ok()
    }
}

struct Clipboard;
impl Drop for Clipboard {
    fn drop(&mut self) {
        let _ = unsafe { CloseClipboard() };
    }
}

struct MessageWindow(HWND);

impl MessageWindow {
    fn new() -> Result<Self, ShellError> {
        Ok(Self(unsafe {
            CreateWindowExW(
                Default::default(),
                w!("STATIC"),
                PCWSTR::null(),
                Default::default(),
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                None,
                None,
                None,
            )?
        }))
    }
}

impl Drop for MessageWindow {
    fn drop(&mut self) {
        let _ = unsafe { DestroyWindow(self.0) };
    }
}

struct GlobalMemory(HGLOBAL);
impl GlobalMemory {
    fn with_bytes(size: usize, write: impl FnOnce(*mut u8)) -> Result<Self, ShellError> {
        let memory = Self(unsafe { GlobalAlloc(GMEM_MOVEABLE, size)? });
        let bytes = unsafe { GlobalLock(memory.0) }.cast::<u8>();
        if bytes.is_null() {
            return Err(windows::core::Error::from_win32().into());
        }
        write(bytes);
        let _ = unsafe { GlobalUnlock(memory.0) };
        Ok(memory)
    }

    fn publish(mut self, format: u32) -> Result<(), ShellError> {
        unsafe { SetClipboardData(format, HANDLE(self.0 .0))? };
        self.0 = HGLOBAL::default();
        Ok(())
    }

    fn set_data(mut self, data: &IDataObject, format: u16) -> Result<(), ShellError> {
        let format = global_format(format);
        let medium = STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: self.0 },
            ..Default::default()
        };
        unsafe { data.SetData(&format, &medium, true)? };
        self.0 = HGLOBAL::default();
        Ok(())
    }
}
impl Drop for GlobalMemory {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { GlobalFree(self.0) };
        }
    }
}

fn global_format(format: u16) -> FORMATETC {
    FORMATETC {
        cfFormat: format,
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
        ..Default::default()
    }
}

fn set_clipboard(
    owner: HWND,
    formats: impl IntoIterator<Item = (u32, GlobalMemory)>,
) -> Result<(), ShellError> {
    let local_owner = if unsafe { IsWindow(owner) }.as_bool() {
        None
    } else {
        Some(MessageWindow::new()?)
    };
    unsafe { OpenClipboard(local_owner.as_ref().map_or(owner, |window| window.0))? };
    let _clipboard = Clipboard;
    unsafe { EmptyClipboard()? };
    for (format, memory) in formats {
        memory.publish(format)?;
    }
    Ok(())
}

fn copy_paths_as_text(target: &SelectionTarget, owner: HWND) -> Result<(), ShellError> {
    let paths = target.selected_paths();
    if paths.is_empty() {
        return Err(ShellError::EmptySelection);
    }
    let mut text = Vec::new();
    for (index, path) in paths.iter().enumerate() {
        if index != 0 {
            text.extend([13, 10]);
        }
        text.push(b'"' as u16);
        text.extend(path.as_os_str().encode_wide());
        text.push(b'"' as u16);
    }
    text.push(0);
    let memory =
        GlobalMemory::with_bytes(std::mem::size_of_val(text.as_slice()), |bytes| unsafe {
            ptr::copy_nonoverlapping(text.as_ptr(), bytes.cast(), text.len());
        })?;
    set_clipboard(owner, [(CF_UNICODETEXT.0.into(), memory)])
}

fn copy_file_drop(target: &SelectionTarget, owner: HWND, cut: bool) -> Result<(), ShellError> {
    let paths = target.selected_paths();
    let files = file_drop_memory(&paths)?;
    let effect = GlobalMemory::with_bytes(size_of::<u32>(), |bytes| unsafe {
        bytes.cast::<u32>().write(if cut { 2 } else { 1 });
    })?;
    let effect_format = unsafe { RegisterClipboardFormatW(CFSTR_PREFERREDDROPEFFECT) };
    if effect_format == 0 {
        return Err(windows::core::Error::from_win32().into());
    }
    set_clipboard(owner, [(CF_HDROP.0.into(), files), (effect_format, effect)])
}

fn file_drop_memory(paths: &[PathBuf]) -> Result<GlobalMemory, ShellError> {
    if paths.is_empty() {
        return Err(ShellError::EmptySelection);
    }
    let list: Vec<_> = paths.iter().flat_map(wide).chain([0]).collect();
    let header_size = size_of::<DROPFILES>();
    GlobalMemory::with_bytes(
        header_size + std::mem::size_of_val(list.as_slice()),
        |bytes| unsafe {
            bytes.cast::<DROPFILES>().write(DROPFILES {
                pFiles: header_size as u32,
                fWide: BOOL(1),
                ..Default::default()
            });
            ptr::copy_nonoverlapping(list.as_ptr(), bytes.add(header_size).cast(), list.len());
        },
    )
}

fn paste(target: &SelectionTarget, owner: HWND, shortcut: bool) -> Result<(), ShellError> {
    let _apartment = Apartment::initialize(true)?;
    let dir = destination_directory(target);
    let folder_id = Pidl::parse(&dir)?;
    let folder: IShellFolder = unsafe { SHBindToObject(None, folder_id.0, None)? };
    let popup = Popup(unsafe { CreatePopupMenu()? });
    let menu: IContextMenu = unsafe { folder.CreateViewObject(owner)? };
    unsafe {
        menu.QueryContextMenu(
            popup.0,
            0,
            FIRST_COMMAND_ID,
            LAST_COMMAND_ID,
            CMF_NORMAL | CMF_EXPLORE,
        )?
    };
    let (verb, verb_wide) = if shortcut {
        (s!("pastelink"), w!("pastelink"))
    } else {
        (s!("paste"), w!("paste"))
    };
    invoke_context_menu(&menu, owner, verb, verb_wide, Some(&dir))
}

fn delete(target: &SelectionTarget, owner: HWND) -> Result<(), ShellError> {
    let paths = target.selected_paths();
    if paths.is_empty() || target.is_background() || matches!(target, SelectionTarget::Drive(_)) {
        return Err(ShellError::EmptySelection);
    }
    match Apartment::initialize(false) {
        Ok(_apartment) => perform_delete_sta(&paths, owner),
        Err(ShellError::Windows(err)) if err.code() == RPC_E_CHANGED_MODE => {
            let owner_raw = owner.0 as usize;
            let handle = thread::Builder::new()
                .name("rwm-delete-sta".to_string())
                .spawn(move || {
                    let _apartment = Apartment::initialize(false)?;
                    perform_delete_sta(&paths, HWND(owner_raw as *mut _))
                })?;
            handle.join().map_err(|_| ShellError::WorkerUnavailable)?
        }
        Err(other) => Err(other),
    }
}

fn perform_delete_sta(paths: &[PathBuf], owner: HWND) -> Result<(), ShellError> {
    unsafe {
        let operation: IFileOperation =
            CoCreateInstance(&FileOperation, None, CLSCTX_INPROC_SERVER)?;
        operation.SetOwnerWindow(owner)?;
        operation.SetOperationFlags(FOF_ALLOWUNDO)?;
        for path in paths {
            let path = wide(path);
            let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(path.as_ptr()), None)?;
            operation.DeleteItem(&item, None)?;
        }
        operation.PerformOperations()?;
        if operation.GetAnyOperationsAborted()?.as_bool() {
            return Err(windows::core::Error::from_hresult(ERROR_CANCELLED.to_hresult()).into());
        }
    }
    Ok(())
}

fn shortcut_filename_for(source_name: &OsStr, number: u32) -> OsString {
    const MAX_FILENAME_UTF16: usize = 255;
    let suffix = if number <= 1 {
        " - Shortcut.lnk".to_string()
    } else {
        format!(" - Shortcut ({number}).lnk")
    };
    let suffix_units = suffix.encode_utf16().count();
    let max_stem_units = MAX_FILENAME_UTF16.saturating_sub(suffix_units);

    let raw_units: Vec<u16> = source_name.encode_wide().collect();
    let stem_units = if raw_units.len() <= max_stem_units {
        &raw_units[..]
    } else {
        // Never slice right in the middle of a UTF-16 surrogate pair.
        let mut cut = max_stem_units;
        if cut > 0 && (0xD800..=0xDBFF).contains(&raw_units[cut - 1]) {
            cut -= 1;
        }
        &raw_units[..cut]
    };

    let mut combined = Vec::with_capacity(stem_units.len() + suffix_units);
    combined.extend_from_slice(stem_units);
    combined.extend(suffix.encode_utf16());
    OsString::from_wide(&combined)
}

fn create_shortcuts(target: &SelectionTarget) -> Result<(), ShellError> {
    let _apartment = Apartment::initialize(true)?;
    for path in target.selected_paths() {
        let parent = path.parent().ok_or(ShellError::EmptySelection)?;
        let name = path.file_name().ok_or(ShellError::EmptySelection)?;
        let link: IShellLinkW =
            unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)? };
        let target_path = wide(&path);
        let working_dir = wide(parent);
        unsafe {
            link.SetPath(PCWSTR(target_path.as_ptr()))?;
            link.SetWorkingDirectory(PCWSTR(working_dir.as_ptr()))?;
        }
        let persist = link.cast::<IPersistFile>()?;
        for number in 1.. {
            let filename = shortcut_filename_for(name, number);
            let destination = parent.join(filename);
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&destination)
            {
                Ok(file) => drop(file),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
            let filename = wide(&destination);
            if let Err(error) = unsafe { persist.Save(PCWSTR(filename.as_ptr()), true) } {
                let _ = std::fs::remove_file(&destination);
                return Err(error.into());
            }
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use windows::Win32::System::Ole::ReleaseStgMedium;
    use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
    use windows::Win32::UI::WindowsAndMessaging::{QS_POSTMESSAGE, WM_APP};

    #[test]
    fn hydration_notifies_the_window_with_the_session_id() {
        let window = MessageWindow::new().unwrap();
        let worker = ComStaWorker::new();
        let message_id = WM_APP + 1;
        let events = worker.start_hydration(
            17,
            SelectionTarget::FolderBackground(PathBuf::from(".")),
            Vec::new(),
            Arc::new(RwmConfig::default()),
            window.0 .0 as isize,
            message_id,
        );
        assert!(matches!(
            events.recv_timeout(Duration::from_secs(5)).unwrap(),
            HydrationEvent::Completed { session_id: 17, .. }
        ));
        let wait =
            unsafe { MsgWaitForMultipleObjectsEx(None, 5000, QS_POSTMESSAGE, MWMO_INPUTAVAILABLE) };
        assert_eq!(wait, WAIT_OBJECT_0);
        let mut message = MSG::default();
        assert!(
            unsafe { PeekMessageW(&mut message, window.0, message_id, message_id, PM_REMOVE) }
                .as_bool()
        );
        assert_eq!(message.wParam, WPARAM(17));
        assert_eq!(message.lParam, LPARAM(0));
    }

    #[test]
    fn shell_data_object_contains_all_selected_files() {
        let _apartment = Apartment::initialize(false).unwrap();
        let windows = PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot"));
        for second in [
            windows.join("regedit.exe"),
            windows.join(r"System32\cmd.exe"),
        ] {
            let paths = vec![windows.join("notepad.exe"), second];
            let selection = ShellSelection::new(&SelectionTarget::MultipleItems {
                parent: windows.clone(),
                items: paths.clone(),
            })
            .unwrap();
            let data = selection.data.unwrap();
            let mut medium = unsafe { data.GetData(&global_format(CF_HDROP.0)) }.unwrap();
            let actual = unsafe {
                let drop = HDROP(medium.u.hGlobal.0);
                let count = DragQueryFileW(drop, u32::MAX, None);
                let actual: Vec<_> = (0..count)
                    .map(|index| {
                        let mut path = vec![0; DragQueryFileW(drop, index, None) as usize + 1];
                        let len = DragQueryFileW(drop, index, Some(&mut path)) as usize;
                        PathBuf::from(OsString::from_wide(&path[..len]))
                    })
                    .collect();
                ReleaseStgMedium(&mut medium);
                actual
            };
            assert_eq!(actual.len(), paths.len());
            for (actual, expected) in actual.iter().zip(&paths) {
                assert!(actual
                    .as_os_str()
                    .eq_ignore_ascii_case(expected.as_os_str()));
            }
            let cida = unsafe { RegisterClipboardFormatW(CFSTR_SHELLIDLIST) };
            let mut medium = unsafe { data.GetData(&global_format(cida as u16)) }.unwrap();
            let count = unsafe {
                let memory = medium.u.hGlobal;
                let cida = GlobalLock(memory).cast::<u32>();
                assert!(!cida.is_null());
                let count = cida.read();
                let _ = GlobalUnlock(memory);
                ReleaseStgMedium(&mut medium);
                count
            };
            assert_eq!(count as usize, paths.len());
        }
    }

    #[test]
    fn benchmarks_real_windows_context_menus() {
        let registry = RegistryVerbCache::new(false);
        let icons = IconCache::new();
        let config = RwmConfig::default();
        let windows = PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot"));
        for target in [
            SelectionTarget::SingleFile(windows.join("notepad.exe")),
            SelectionTarget::FolderBackground(windows),
        ] {
            let expected = registry.collect_for_target(&target).1;
            assert!(!expected.is_empty());
            let report = benchmark_target(&target, &config, &registry, &icons).expect("benchmark");
            assert_eq!(report.com_handlers.len(), expected.len());
            assert!(report.stage0_item_count > 0);
            assert!(report.total_com_us > 0);
            assert!(
                report.com_handlers.iter().any(|handler| {
                    handler.status == HandlerStatus::Ok && handler.item_count > 0
                }),
                "{:#?}",
                report.com_handlers
            );
            eprintln!(
                "{} (background={}): {} discovered, {} with items, {} empty, {} failed",
                report.target_display,
                report.is_background,
                report.com_handlers.len(),
                report
                    .com_handlers
                    .iter()
                    .filter(|h| h.status == HandlerStatus::Ok)
                    .count(),
                report
                    .com_handlers
                    .iter()
                    .filter(|h| h.status == HandlerStatus::Empty)
                    .count(),
                report
                    .com_handlers
                    .iter()
                    .filter(|h| matches!(h.status, HandlerStatus::Failed(_)))
                    .count(),
            );
            for handler in &report.com_handlers {
                if let HandlerStatus::Failed(error) = &handler.status {
                    eprintln!("  {} ({}): {error}", handler.name, handler.clsid);
                }
            }
        }
    }

    #[test]
    fn disabled_policy_and_worker_lifecycle() {
        let worker = ComStaWorker::new();
        let mut config = RwmConfig::default();
        config.handlers.default_policy = HandlerPolicy::Disabled;
        let events = worker.start_hydration(
            42,
            SelectionTarget::SingleFile(PathBuf::from("invalid-path")),
            vec![DiscoveredComHandler {
                clsid: "invalid-clsid".into(),
                name: "Test".into(),
                dll_path: None,
            }],
            Arc::new(config),
            0,
            0,
        );
        let timeout = Duration::from_secs(5);
        assert!(matches!(
            events.recv_timeout(timeout).unwrap(),
            HydrationEvent::GroupReady { session_id: 42, entries, timing, .. }
                if entries.is_empty() && timing.status == HandlerStatus::SkippedPolicy
        ));
        assert!(matches!(
            events.recv_timeout(timeout).unwrap(),
            HydrationEvent::Completed { session_id: 42, .. }
        ));
        worker.close_session(42);
        assert!(matches!(
            worker.invoke(42, 0, 0, 0, None),
            Err(ShellError::UnknownCommand { .. })
        ));
        worker.shutdown();
        assert!(matches!(
            worker.invoke(42, 0, 0, 0, None),
            Err(ShellError::WorkerUnavailable)
        ));
    }

    #[test]
    fn expands_command_paths_after_parsing_windows_argv() {
        let target = SelectionTarget::SingleFile(PathBuf::from(r"C:\Test files\アイテム.txt"));
        let dir = target.parent_dir();
        let args = split_command(r#""C:\Program Files\app.exe" --item="%L" "%W""#).unwrap();
        assert_eq!(args[0], r"C:\Program Files\app.exe");
        assert_eq!(
            expand_static(&args[1], target.primary_path(), &dir),
            r"--item=C:\Test files\アイテム.txt"
        );
        assert_eq!(
            expand_static(&args[2], target.primary_path(), &dir),
            dir.as_os_str()
        );
        assert_eq!(
            expand_custom(OsStr::new("dir={dir};path={path}"), &target, &dir),
            r"dir=C:\Test files;path=C:\Test files\アイテム.txt",
        );
    }

    #[test]
    fn windows_terminal_escapes_semicolons_in_substituted_paths() {
        let dir = PathBuf::from(r"C:\tmp\a;new-tab cmd");
        let target = SelectionTarget::FolderBackground(dir.clone());
        let (prog, args) =
            build_terminal_invocation(r#"wt.exe -d "{dir}""#, &target, &dir).expect("wt invocation");
        assert_eq!(prog, OsStr::new("wt.exe"));
        assert_eq!(
            args,
            vec![
                OsString::from("-d"),
                OsString::from(r"C:\tmp\a\;new-tab cmd"),
            ]
        );

        let (pwsh_prog, pwsh_args) = build_terminal_invocation(
            r#"pwsh.exe -WorkingDirectory "{dir}""#,
            &target,
            &dir,
        )
        .expect("pwsh invocation");
        assert_eq!(pwsh_prog, OsStr::new("pwsh.exe"));
        assert_eq!(
            pwsh_args,
            vec![
                OsString::from("-WorkingDirectory"),
                OsString::from(r"C:\tmp\a;new-tab cmd"),
            ]
        );
    }

    #[test]
    fn shortcut_filename_respects_255_utf16_limit_on_long_names() {
        let long_name = OsString::from("あ".repeat(248) + "😀");
        let short1 = shortcut_filename_for(&long_name, 1);
        let short99 = shortcut_filename_for(&long_name, 99);
        assert!(short1.encode_wide().count() <= 255);
        assert!(short99.encode_wide().count() <= 255);
        assert!(short99.to_string_lossy().ends_with(" - Shortcut (99).lnk"));
    }
}
