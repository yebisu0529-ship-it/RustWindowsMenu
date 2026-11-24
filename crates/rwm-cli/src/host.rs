use std::collections::{BTreeMap, BTreeSet};
use std::os::windows::process::CommandExt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND,
    HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::System::Console::{
    SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT, CTRL_LOGOFF_EVENT,
    CTRL_SHUTDOWN_EVENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, GetCurrentProcess, OpenEventW, OpenMutexW, OpenProcess,
    OpenProcessToken, SetEvent, WaitForSingleObject, CREATE_NO_WINDOW, EVENT_MODIFY_STATE,
    PROCESS_QUERY_LIMITED_INFORMATION, SYNCHRONIZATION_SYNCHRONIZE,
};
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClassNameW,
    GetShellWindow, GetWindowThreadProcessId, KillTimer, MsgWaitForMultipleObjects, PeekMessageW,
    PostQuitMessage, RegisterClassExW, RegisterWindowMessageW, SetTimer, SetWindowLongPtrW,
    SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, EVENT_OBJECT_CREATE, GWLP_USERDATA,
    HHOOK, MSG, PM_REMOVE, QS_ALLINPUT, WH_CALLWNDPROC, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS, WM_DESTROY, WM_QUIT, WM_TIMER, WNDCLASSEXW, WS_OVERLAPPED,
};

use crate::error::{CliError, CliResult, Context};
use crate::explorer::{self, Delivery, ShellWindow};
use crate::hookdll::{self, HookLibrary};
use crate::output::print_line;

pub const HOST_MUTEX_NAME: &str = r"Local\RustWindowsMenuHost_v2";
pub const HOST_EVENT_NAME: &str = r"Local\RustWindowsMenu_HookHost_v2";
pub const LEGACY_HOST_EVENT_NAME: &str = r"Local\RustWindowsMenu_HookHost_v1";
pub const HOST_READY_EVENT_NAME: &str = r"Local\RustWindowsMenu_HookHost_v2.Ready";
const DEBOUNCE_TIMER: usize = 1;
const SWEEP_TIMER: usize = 2;
const HOST_WINDOW_CLASS: &str = "RustWindowsMenuHost_v2";

static HOST_HWND: AtomicIsize = AtomicIsize::new(0);
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);
static HOST_FINISHED: AtomicBool = AtomicBool::new(true);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostState {
    Stopped,
    Running,
    RunningOtherIntegrity,
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}
fn close(handle: windows::Win32::Foundation::HANDLE) {
    unsafe {
        let _ = CloseHandle(handle);
    }
}

#[derive(Debug)]
pub enum ClaimError {
    AlreadyRunning,
    Os(windows::core::Error),
}

pub struct HostSignal {
    mutex: windows::Win32::Foundation::HANDLE,
    event: windows::Win32::Foundation::HANDLE,
    ready: windows::Win32::Foundation::HANDLE,
}

impl HostSignal {
    pub fn claim(mutex_name: &str, event_name: &str) -> Result<Self, ClaimError> {
        let mutex = unsafe { CreateMutexW(None, false, PCWSTR(wide(mutex_name).as_ptr())) }
            .map_err(ClaimError::Os)?;
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            close(mutex);
            return Err(ClaimError::AlreadyRunning);
        }
        let event =
            match unsafe { CreateEventW(None, true, false, PCWSTR(wide(event_name).as_ptr())) } {
                Ok(event) if unsafe { GetLastError() } != ERROR_ALREADY_EXISTS => event,
                Ok(event) => {
                    close(event);
                    close(mutex);
                    return Err(ClaimError::AlreadyRunning);
                }
                Err(error) => {
                    close(mutex);
                    return Err(ClaimError::Os(error));
                }
            };
        let ready_name = format!("{event_name}.Ready");
        let ready =
            match unsafe { CreateEventW(None, true, false, PCWSTR(wide(&ready_name).as_ptr())) } {
                Ok(ready) => ready,
                Err(error) => {
                    close(event);
                    close(mutex);
                    return Err(ClaimError::Os(error));
                }
            };
        Ok(Self {
            mutex,
            event,
            ready,
        })
    }
    pub fn event(&self) -> windows::Win32::Foundation::HANDLE {
        self.event
    }
    pub fn mark_ready(&self) -> CliResult<()> {
        unsafe { SetEvent(self.ready) }.map_err(|error| CliError::new(error.message()))
    }
}

impl Drop for HostSignal {
    fn drop(&mut self) {
        close(self.ready);
        close(self.event);
        close(self.mutex);
    }
}

fn process_is_elevated(process: HANDLE) -> Option<bool> {
    let mut token = HANDLE::default();
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) }.is_err() {
        return None;
    }
    let mut elevation = TOKEN_ELEVATION::default();
    let mut len = 0u32;
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
    };
    close(token);
    ok.ok().map(|()| elevation.TokenIsElevated != 0)
}

pub fn ensure_matching_elevation(action: &str) -> CliResult<()> {
    let Some(true) = process_is_elevated(unsafe { GetCurrentProcess() }) else {
        return Ok(());
    };
    let shell_hwnd = unsafe { GetShellWindow() };
    if shell_hwnd.is_invalid() {
        return Ok(());
    }
    let mut shell_pid = 0u32;
    unsafe { GetWindowThreadProcessId(shell_hwnd, Some(&mut shell_pid)) };
    if shell_pid == 0 {
        return Ok(());
    }
    if let Ok(shell_proc) = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, shell_pid) } {
        let shell_elevated = process_is_elevated(shell_proc);
        close(shell_proc);
        if shell_elevated == Some(false) {
            return Err(CliError::new(format!(
                "refusing `{action}` from an elevated prompt while Explorer runs unelevated; re-run from a normal (non-Administrator) terminal"
            )));
        }
    }
    Ok(())
}

pub fn host_state(name: &str) -> HostState {
    match unsafe {
        OpenMutexW(
            SYNCHRONIZATION_SYNCHRONIZE,
            false,
            PCWSTR(wide(name).as_ptr()),
        )
    } {
        Ok(handle) => {
            close(handle);
            HostState::Running
        }
        Err(error) if error.code() == ERROR_ACCESS_DENIED.to_hresult() => {
            HostState::RunningOtherIntegrity
        }
        Err(_) => HostState::Stopped,
    }
}

pub fn is_host_running(name: &str) -> bool {
    !matches!(host_state(name), HostState::Stopped)
}

fn is_event_running(name: &str) -> bool {
    match unsafe {
        OpenEventW(
            SYNCHRONIZATION_SYNCHRONIZE,
            false,
            PCWSTR(wide(name).as_ptr()),
        )
    } {
        Ok(event) => {
            close(event);
            true
        }
        Err(_) => false,
    }
}

fn is_host_ready() -> bool {
    let Ok(event) = (unsafe {
        OpenEventW(
            SYNCHRONIZATION_SYNCHRONIZE,
            false,
            PCWSTR(wide(HOST_READY_EVENT_NAME).as_ptr()),
        )
    }) else {
        return false;
    };
    let signalled = unsafe { WaitForSingleObject(event, 0) } == WAIT_OBJECT_0;
    close(event);
    signalled
}

fn request_stop(name: &str) -> CliResult<bool> {
    let event = match unsafe { OpenEventW(EVENT_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(wide(name).as_ptr())) } {
        Ok(event) => event,
        Err(error) if error.code() == ERROR_FILE_NOT_FOUND.to_hresult() => return Ok(false),
        Err(error) if error.code() == ERROR_ACCESS_DENIED.to_hresult() => return Err(CliError::new(format!("access denied opening host stop event {name}; the host is likely running at another integrity level (elevated vs standard) or session"))),
        Err(error) => return Err(CliError::new(format!("cannot open host stop event {name}: {}", error.message()))),
    };
    let result = unsafe { SetEvent(event) }.map_err(|error| CliError::new(error.message()));
    close(event);
    result.map(|()| true)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    NotRunning,
    Stopped,
    TimedOut,
}

pub fn stop_running_host(
    mutex_name: &str,
    event_name: &str,
    timeout: Duration,
) -> CliResult<StopOutcome> {
    if !request_stop(event_name)? {
        return Ok(StopOutcome::NotRunning);
    }
    let deadline = Instant::now() + timeout;
    while is_host_running(mutex_name) {
        if Instant::now() >= deadline {
            return Ok(StopOutcome::TimedOut);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Ok(StopOutcome::Stopped)
}

pub fn stop() -> CliResult<ExitCode> {
    let current = stop_running_host(HOST_MUTEX_NAME, HOST_EVENT_NAME, Duration::from_secs(10))?;
    let legacy = if request_stop(LEGACY_HOST_EVENT_NAME)? {
        let deadline = Instant::now() + Duration::from_secs(3);
        while is_event_running(LEGACY_HOST_EVENT_NAME) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
        if is_event_running(LEGACY_HOST_EVENT_NAME) {
            StopOutcome::TimedOut
        } else {
            StopOutcome::Stopped
        }
    } else {
        StopOutcome::NotRunning
    };
    if current == StopOutcome::TimedOut || legacy == StopOutcome::TimedOut {
        return Err(CliError::new(
            "a hook host did not exit within the graceful shutdown timeout",
        ));
    }
    let detached = explorer::orphan_detach_sweep();
    print_line(format!("Host stop: v0.2 {current:?}, v0.1 {legacy:?}; orphan detach sweep: {} acknowledgements / {} messages.", detached.acknowledged, detached.attempted));
    Ok(ExitCode::SUCCESS)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachFailure {
    pub thread_id: u32,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanReport {
    pub attached: Vec<u32>,
    pub failed: Vec<AttachFailure>,
    pub released: Vec<u32>,
    pub hooked: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DetachReport {
    pub delivery: Delivery,
    pub hooks_removed: usize,
}

struct AttachedHook {
    handle: HHOOK,
    views: Vec<ShellWindow>,
}

pub struct HookSession<'a> {
    library: &'a HookLibrary,
    hooks: BTreeMap<u32, AttachedHook>,
}

impl<'a> HookSession<'a> {
    pub fn new(library: &'a HookLibrary) -> Self {
        Self {
            library,
            hooks: BTreeMap::new(),
        }
    }

    pub fn scan(&mut self) -> ScanReport {
        let windows = explorer::enumerate_explorer_windows();
        let views = explorer::explorer_views(&windows);
        let active = explorer::ui_thread_ids(&views);
        let stale = self
            .hooks
            .keys()
            .filter(|tid| !active.contains(tid))
            .copied()
            .collect::<Vec<_>>();
        for tid in &stale {
            self.detach_one(*tid);
        }
        let mut report = ScanReport {
            released: stale,
            ..Default::default()
        };
        for tid in active {
            let targets = views
                .iter()
                .filter(|view| view.thread_id == tid)
                .cloned()
                .collect::<Vec<_>>();
            if let Some(attached) = self.hooks.get_mut(&tid) {
                let current_ids: BTreeSet<isize> = targets.iter().map(|view| view.hwnd).collect();
                let known_ids: BTreeSet<isize> =
                    attached.views.iter().map(|view| view.hwnd).collect();
                if !known_ids.is_empty() && known_ids.is_disjoint(&current_ids) {
                    // All previous views on this TID vanished; re-hook in case Windows recycled the TID.
                    self.detach_one(tid);
                } else {
                    let mut retained: Vec<ShellWindow> = targets
                        .iter()
                        .filter(|view| known_ids.contains(&view.hwnd))
                        .cloned()
                        .collect();
                    let new_targets = targets
                        .iter()
                        .filter(|view| !known_ids.contains(&view.hwnd))
                        .cloned()
                        .collect::<Vec<_>>();
                    for view in new_targets {
                        let delivery = explorer::send_message(
                            std::slice::from_ref(&view),
                            self.library.attach_message_id(),
                        );
                        if delivery.acknowledged == 1 {
                            retained.push(view);
                        } else if delivery.attempted > 0 {
                            report.failed.push(AttachFailure {
                                thread_id: tid,
                                reason: "new SHELLDLL_DefView did not acknowledge attach".into(),
                            });
                        }
                    }
                    attached.views = retained;
                    continue;
                }
            }
            match unsafe {
                SetWindowsHookExW(
                    WH_CALLWNDPROC,
                    self.library.hook_proc(),
                    HINSTANCE::from(self.library.module()),
                    tid,
                )
            } {
                Ok(handle) => {
                    let delivery =
                        explorer::send_message(&targets, self.library.attach_message_id());
                    if delivery.attempted == 0 || delivery.acknowledged != delivery.attempted {
                        let _ = explorer::send_message(&targets, self.library.detach_message_id());
                        unsafe {
                            let _ = UnhookWindowsHookEx(handle);
                        }
                        report.failed.push(AttachFailure {
                            thread_id: tid,
                            reason: format!(
                                "attach acknowledged by {} of {} live SHELLDLL_DefView windows",
                                delivery.acknowledged, delivery.attempted
                            ),
                        });
                    } else {
                        self.hooks.insert(
                            tid,
                            AttachedHook {
                                handle,
                                views: targets,
                            },
                        );
                        report.attached.push(tid);
                    }
                }
                Err(error) => report.failed.push(AttachFailure {
                    thread_id: tid,
                    reason: error.message(),
                }),
            }
        }
        report.hooked = self.hooks.len();
        report
    }

    fn detach_one(&mut self, tid: u32) -> DetachReport {
        let Some(hook) = self.hooks.remove(&tid) else {
            return DetachReport::default();
        };
        let current = explorer::explorer_views(&explorer::enumerate_explorer_windows())
            .into_iter()
            .filter(|view| view.thread_id == tid)
            .collect::<Vec<_>>();
        let mut seen = BTreeSet::new();
        let targets = hook
            .views
            .into_iter()
            .chain(current)
            .filter(|view| seen.insert(view.hwnd))
            .collect::<Vec<_>>();
        let delivery = explorer::send_message(&targets, self.library.detach_message_id());
        let removed = unsafe { UnhookWindowsHookEx(hook.handle) }.is_ok();
        DetachReport {
            delivery,
            hooks_removed: usize::from(removed),
        }
    }

    pub fn detach(&mut self) -> DetachReport {
        let threads = self.hooks.keys().copied().collect::<Vec<_>>();
        let mut total = DetachReport::default();
        for tid in threads {
            let report = self.detach_one(tid);
            total.delivery.attempted += report.delivery.attempted;
            total.delivery.acknowledged += report.delivery.acknowledged;
            total.delivery.skipped_dead += report.delivery.skipped_dead;
            total.hooks_removed += report.hooks_removed;
        }
        total
    }
}

impl Drop for HookSession<'_> {
    fn drop(&mut self) {
        let _ = self.detach();
    }
}

struct WindowState<'a> {
    session: HookSession<'a>,
    taskbar_created: u32,
}
impl WindowState<'_> {
    fn rescan(&mut self) {
        let report = self.session.scan();
        for failure in report.failed {
            tracing::warn!(thread_id = failure.thread_id, reason = %failure.reason, "Explorer attach failed");
        }
    }
}

unsafe extern "system" fn winevent_callback(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    object_id: i32,
    child_id: i32,
    _thread: u32,
    _time: u32,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if event != EVENT_OBJECT_CREATE || object_id != 0 || child_id != 0 || hwnd.is_invalid() {
            return;
        }
        let mut class = [0u16; 128];
        let len = GetClassNameW(hwnd, &mut class);
        if len <= 0 {
            return;
        }
        let class = String::from_utf16_lossy(&class[..len as usize]);
        if explorer::is_view_class(&class) || explorer::is_top_level_class(&class) {
            let host = HWND(HOST_HWND.load(Ordering::Acquire) as *mut std::ffi::c_void);
            if !host.is_invalid() {
                let _ = SetTimer(host, DEBOUNCE_TIMER, 75, None);
            }
        }
    }));
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let handled = catch_unwind(AssertUnwindSafe(|| {
        let state = windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(
            hwnd,
            GWLP_USERDATA,
        ) as *mut WindowState<'_>;
        if !state.is_null()
            && (message == (*state).taskbar_created
                || (message == WM_TIMER && matches!(wparam.0, DEBOUNCE_TIMER | SWEEP_TIMER)))
        {
            if message == WM_TIMER {
                let _ = KillTimer(hwnd, wparam.0);
            }
            if message == WM_TIMER && wparam.0 == SWEEP_TIMER {
                let _ = SetTimer(hwnd, SWEEP_TIMER, 5000, None);
            }
            (*state).rescan();
            return Some(LRESULT(0));
        }
        if message == WM_DESTROY {
            PostQuitMessage(0);
            return Some(LRESULT(0));
        }
        None
    }))
    .ok()
    .flatten();
    handled.unwrap_or_else(|| DefWindowProcW(hwnd, message, wparam, lparam))
}

unsafe extern "system" fn console_event(event: u32) -> windows::Win32::Foundation::BOOL {
    catch_unwind(AssertUnwindSafe(|| {
        if matches!(
            event,
            CTRL_C_EVENT
                | CTRL_BREAK_EVENT
                | CTRL_CLOSE_EVENT
                | CTRL_LOGOFF_EVENT
                | CTRL_SHUTDOWN_EVENT
        ) {
            STOP_REQUESTED.store(true, Ordering::Release);
            let _ = request_stop(HOST_EVENT_NAME);
            let deadline = Instant::now() + Duration::from_millis(4000);
            while !HOST_FINISHED.load(Ordering::Acquire) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            return windows::Win32::Foundation::TRUE;
        }
        windows::Win32::Foundation::FALSE
    }))
    .unwrap_or(windows::Win32::Foundation::FALSE)
}

struct ConsoleCtrlGuard;
impl Drop for ConsoleCtrlGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = SetConsoleCtrlHandler(Some(console_event), false);
        }
    }
}

struct HostWindowGuard(HWND);
impl Drop for HostWindowGuard {
    fn drop(&mut self) {
        HOST_HWND.store(0, Ordering::Release);
        unsafe {
            let _ = SetWindowLongPtrW(self.0, GWLP_USERDATA, 0);
            let _ = KillTimer(self.0, DEBOUNCE_TIMER);
            let _ = KillTimer(self.0, SWEEP_TIMER);
            let _ = DestroyWindow(self.0);
        }
    }
}

struct WinEventGuard(HWINEVENTHOOK);
impl Drop for WinEventGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = UnhookWinEvent(self.0);
        }
    }
}

struct FinishGuard;
impl Drop for FinishGuard {
    fn drop(&mut self) {
        HOST_FINISHED.store(true, Ordering::Release);
    }
}

pub fn run() -> CliResult<ExitCode> {
    ensure_matching_elevation("run")?;
    STOP_REQUESTED.store(false, Ordering::Release);
    HOST_FINISHED.store(false, Ordering::Release);
    let _finish = FinishGuard;
    let signal =
        HostSignal::claim(HOST_MUTEX_NAME, HOST_EVENT_NAME).map_err(|error| match error {
            ClaimError::AlreadyRunning => CliError::new("RustWindowsMenu host is already running"),
            ClaimError::Os(error) => {
                CliError::new(format!("cannot claim host instance: {}", error.message()))
            }
        })?;
    let exe = std::env::current_exe().context(|| "cannot locate host executable".to_string())?;
    let dll = hookdll::locate(&exe).ok_or_else(|| hookdll::not_found_error(&exe))?;
    let library = HookLibrary::load(&dll)?;
    unsafe { SetConsoleCtrlHandler(Some(console_event), true) }
        .context(|| "cannot install console control handler".to_string())?;
    let _console_guard = ConsoleCtrlGuard;
    let taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
    if !hookdll::is_registered_message(taskbar_created) {
        return Err(CliError::new("could not register TaskbarCreated"));
    }
    let instance =
        unsafe { GetModuleHandleW(None) }.context(|| "GetModuleHandleW failed".to_string())?;
    let class = wide(HOST_WINDOW_CLASS);
    let window_class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(window_proc),
        hInstance: instance.into(),
        lpszClassName: PCWSTR(class.as_ptr()),
        ..Default::default()
    };
    if unsafe { RegisterClassExW(&window_class) } == 0 {
        return Err(windows::core::Error::from_win32().into());
    }
    // Hidden top-level overlapped window receives `TaskbarCreated` broadcasts unlike `HWND_MESSAGE`.
    let hwnd = HostWindowGuard(
        unsafe {
            CreateWindowExW(
                Default::default(),
                PCWSTR(class.as_ptr()),
                w!("RustWindowsMenu host"),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                None,
                None,
                instance,
                None,
            )
        }
        .context(|| "cannot create host event window".to_string())?,
    );
    HOST_HWND.store(hwnd.0 .0 as isize, Ordering::Release);
    let event_hook = unsafe {
        SetWinEventHook(
            EVENT_OBJECT_CREATE,
            EVENT_OBJECT_CREATE,
            None,
            Some(winevent_callback),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        )
    };
    if event_hook.is_invalid() {
        return Err(CliError::new("SetWinEventHook failed"));
    }
    let _event_guard = WinEventGuard(event_hook);
    let mut state = WindowState {
        session: HookSession::new(&library),
        taskbar_created,
    };
    unsafe {
        SetWindowLongPtrW(
            hwnd.0,
            GWLP_USERDATA,
            &mut state as *mut WindowState<'_> as isize,
        );
    }
    signal.mark_ready()?;
    state.rescan();
    unsafe {
        SetTimer(hwnd.0, SWEEP_TIMER, 5000, None);
    }
    let mut message = MSG::default();
    while !STOP_REQUESTED.load(Ordering::Acquire) {
        let wait = unsafe {
            MsgWaitForMultipleObjects(Some(&[signal.event()]), false, u32::MAX, QS_ALLINPUT)
        };
        if wait == WAIT_OBJECT_0 {
            break;
        }
        if wait.0 == WAIT_OBJECT_0.0 + 1 {
            while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                if message.message == WM_QUIT {
                    STOP_REQUESTED.store(true, Ordering::Release);
                    break;
                }
                unsafe {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
        } else {
            return Err(CliError::new(format!(
                "host message wait failed: {}",
                windows::core::Error::from_win32().message()
            )));
        }
    }
    let detached = state.session.detach();
    drop(_event_guard);
    drop(hwnd);
    drop(state);
    drop(library);
    print_line(format!(
        "Host detached {} Explorer hook(s); {} window acknowledgement(s).",
        detached.hooks_removed, detached.delivery.acknowledged
    ));
    Ok(ExitCode::SUCCESS)
}

pub fn start() -> CliResult<ExitCode> {
    ensure_matching_elevation("start")?;
    let exe = std::env::current_exe().context(|| "cannot locate rwm.exe".to_string())?;
    start_host_exe(&exe.with_file_name("rwm-host.exe"))?;
    print_line("rwm-host started and initialized successfully.");
    Ok(ExitCode::SUCCESS)
}

pub fn start_host_exe(host_exe: &Path) -> CliResult<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    match host_state(HOST_MUTEX_NAME) {
        HostState::RunningOtherIntegrity => {
            return Err(CliError::new(
                "rwm-host is already running at another integrity level; stop it from that terminal first",
            ));
        }
        HostState::Running => {
            while Instant::now() < deadline {
                if is_host_ready() {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            return Err(CliError::new(
                "the existing rwm-host did not finish initialization within 5 seconds",
            ));
        }
        HostState::Stopped => {}
    }
    if !host_exe.is_file() {
        return Err(CliError::new(format!(
            "host executable not found: {}",
            host_exe.display()
        )));
    }
    let mut command = Command::new(host_exe);
    if let Some(dir) = host_exe.parent() {
        command.current_dir(dir);
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW.0)
        .spawn()
        .context(|| format!("cannot launch {}", host_exe.display()))?;
    while Instant::now() < deadline {
        if is_host_ready() {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            return Err(CliError::new(format!(
                "rwm-host exited during initialization ({status})"
            )));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let _ = stop_running_host(HOST_MUTEX_NAME, HOST_EVENT_NAME, Duration::from_secs(2));
    let _ = child.kill();
    let _ = child.wait();
    Err(CliError::new(
        "rwm-host did not initialize within 5 seconds",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::unique_name;

    #[test]
    fn single_instance_claim_and_graceful_stop_are_scoped() {
        let mutex = format!(r"Local\{}", unique_name("mutex"));
        let event = format!(r"Local\{}", unique_name("event"));
        let signal = HostSignal::claim(&mutex, &event).unwrap();
        assert!(is_host_running(&mutex));
        assert!(matches!(
            HostSignal::claim(&mutex, &event),
            Err(ClaimError::AlreadyRunning)
        ));
        let waiter = std::thread::spawn({
            let mutex = mutex.clone();
            let event = event.clone();
            move || stop_running_host(&mutex, &event, Duration::from_millis(50)).unwrap()
        });
        assert_eq!(waiter.join().unwrap(), StopOutcome::TimedOut);
        drop(signal);
        assert!(!is_host_running(&mutex));
        assert_eq!(
            stop_running_host(&mutex, &event, Duration::ZERO).unwrap(),
            StopOutcome::NotRunning
        );
    }
}
