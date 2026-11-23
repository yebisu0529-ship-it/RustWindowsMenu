//! The hook host behind `rwm run`: keeps `rwm_hook.dll` attached to every Explorer UI thread
//! and detaches it again before the process exits.

use std::collections::{BTreeMap, BTreeSet};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rwm_core::config::RwmConfig;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, BOOL, ERROR_ALREADY_EXISTS, FALSE, FILETIME, HANDLE, HINSTANCE,
    HMODULE, TRUE, WAIT_OBJECT_0,
};
use windows::Win32::System::Console::{
    FreeConsole, SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT,
    CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
};
use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows::Win32::System::ProcessStatus::{K32EnumProcessModules, K32GetModuleBaseNameW};
use windows::Win32::System::Threading::{
    CreateEventW, GetThreadTimes, OpenEventW, OpenProcess, OpenThread, SetEvent,
    WaitForSingleObject, EVENT_MODIFY_STATE, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
    SYNCHRONIZATION_ACCESS_RIGHTS, SYNCHRONIZATION_SYNCHRONIZE, THREAD_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    SetWindowsHookExW, UnhookWindowsHookEx, HHOOK, WH_CALLWNDPROC,
};

use crate::error::{CliError, CliResult, Context};
use crate::explorer::{self, ShellWindow};
use crate::hookdll::{self, HookLibrary, SUPPORTED_ABI};
use crate::output::{print_error_line, print_line};

/// The event a running host owns. Its existence marks the host as running, and signalling it
/// asks the host to stop. `Local\` scopes it to the sign-in session, like the hooks.
pub const HOST_EVENT_NAME: &str = r"Local\RustWindowsMenu_HookHost_v1";

const SCAN_INTERVAL: Duration = Duration::from_millis(1000);

/// Windows terminates a console process about five seconds after a close, logoff, or
/// shutdown event, so the detach has to finish inside this window.
const CONSOLE_CLOSE_GRACE: Duration = Duration::from_millis(4500);

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);
static HOST_FINISHED: AtomicBool = AtomicBool::new(false);

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

#[derive(Debug)]
pub enum ClaimError {
    AlreadyRunning,
    Os(windows::core::Error),
}

/// Ownership of the host event for as long as this value lives.
pub struct HostSignal(HANDLE);

impl HostSignal {
    pub fn claim(name: &str) -> Result<Self, ClaimError> {
        let name = wide(name);
        let event = unsafe { CreateEventW(None, true, false, PCWSTR(name.as_ptr())) }
            .map_err(ClaimError::Os)?;
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe {
                let _ = CloseHandle(event);
            }
            return Err(ClaimError::AlreadyRunning);
        }
        Ok(Self(event))
    }

    /// Blocks for up to `timeout`. True once a stop has been requested.
    pub fn wait_for_stop(&self, timeout: Duration) -> bool {
        let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
        unsafe { WaitForSingleObject(self.0, millis) == WAIT_OBJECT_0 }
    }
}

impl Drop for HostSignal {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn open_event(name: &str, access: SYNCHRONIZATION_ACCESS_RIGHTS) -> Option<HANDLE> {
    let name = wide(name);
    unsafe { OpenEventW(access, false, PCWSTR(name.as_ptr())) }.ok()
}

pub fn is_host_running(name: &str) -> bool {
    match open_event(name, SYNCHRONIZATION_SYNCHRONIZE) {
        Some(event) => {
            unsafe {
                let _ = CloseHandle(event);
            }
            true
        }
        None => false,
    }
}

/// Signals the host event. False when no host owns it.
fn request_stop(name: &str) -> bool {
    let Some(event) = open_event(name, EVENT_MODIFY_STATE) else {
        return false;
    };
    unsafe {
        let signalled = SetEvent(event).is_ok();
        let _ = CloseHandle(event);
        signalled
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    NotRunning,
    Stopped,
    TimedOut,
}

/// Asks a running host to detach and exit, then waits until it has. The host closes the
/// event last, after its hooks are gone, so the event disappearing means the detach is done.
pub fn stop_running_host(name: &str, timeout: Duration) -> StopOutcome {
    if !request_stop(name) {
        return StopOutcome::NotRunning;
    }
    let deadline = Instant::now() + timeout;
    while is_host_running(name) {
        if Instant::now() >= deadline {
            return StopOutcome::TimedOut;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    StopOutcome::Stopped
}

/// The creation time of a live thread. Windows reuses thread ids, so an id alone does not
/// say whether a hooked thread is still the same thread.
fn thread_created_at(thread_id: u32) -> Option<u64> {
    let thread = unsafe { OpenThread(THREAD_QUERY_LIMITED_INFORMATION, false, thread_id) }.ok()?;
    let mut created = FILETIME::default();
    let mut unused = [FILETIME::default(); 3];
    let queried = unsafe {
        GetThreadTimes(
            thread,
            &mut created,
            &mut unused[0],
            &mut unused[1],
            &mut unused[2],
        )
    };
    unsafe {
        let _ = CloseHandle(thread);
    }
    queried.ok()?;
    Some((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

struct AttachedHook {
    handle: HHOOK,
    thread_created: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachFailure {
    pub thread_id: u32,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanReport {
    /// Threads that received a hook in this scan.
    pub attached: Vec<u32>,
    /// Threads that could not be hooked. Each is reported once.
    pub failed: Vec<AttachFailure>,
    /// Hooked threads that have exited since the last scan.
    pub released: Vec<u32>,
    /// Hooks held after this scan.
    pub hooked: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DetachReport {
    pub windows: usize,
    pub windows_acknowledged: usize,
    pub hooks_removed: usize,
}

/// The `WH_CALLWNDPROC` hooks one host holds. Dropping the session detaches them, so a panic
/// or an early return cannot leave Explorer hooked.
pub struct HookSession<'a> {
    library: &'a HookLibrary,
    windows: Box<dyn FnMut() -> Vec<ShellWindow> + 'a>,
    hooks: BTreeMap<u32, AttachedHook>,
    reported_failures: BTreeSet<(u32, u64)>,
}

impl<'a> HookSession<'a> {
    /// `windows` lists the windows to serve. It is called for every scan and for the detach.
    pub fn new(
        library: &'a HookLibrary,
        windows: Box<dyn FnMut() -> Vec<ShellWindow> + 'a>,
    ) -> Self {
        Self {
            library,
            windows,
            hooks: BTreeMap::new(),
            reported_failures: BTreeSet::new(),
        }
    }

    /// Hooks every listed UI thread that is not hooked yet.
    pub fn scan(&mut self) -> ScanReport {
        let windows = (self.windows)();
        let released = self.release_exited_threads();

        let mut thread_processes: BTreeMap<u32, u32> = BTreeMap::new();
        for window in &windows {
            thread_processes
                .entry(window.thread_id)
                .or_insert(window.process_id);
        }
        let local_fp = local_pe_fingerprint(self.library.module());
        let mut process_checks: BTreeMap<u32, Result<(), String>> = BTreeMap::new();

        let mut attached = Vec::new();
        let mut failed = Vec::new();
        for thread_id in explorer::ui_thread_ids(&windows) {
            if self.hooks.contains_key(&thread_id) {
                continue;
            }
            let Some(thread_created) = thread_created_at(thread_id) else {
                tracing::debug!(thread_id, "skipping a thread that cannot be queried");
                continue;
            };
            if let (Some(expected), Some(&process_id)) =
                (local_fp.as_ref(), thread_processes.get(&thread_id))
            {
                let check = process_checks.entry(process_id).or_insert_with(|| {
                    remote_hook_dll_state(process_id, hookdll::HOOK_DLL_FILE, expected)
                });
                if let Err(reason) = check {
                    if self.reported_failures.insert((thread_id, thread_created)) {
                        failed.push(AttachFailure {
                            thread_id,
                            reason: reason.clone(),
                        });
                    }
                    continue;
                }
            }
            let hooked = unsafe {
                SetWindowsHookExW(
                    WH_CALLWNDPROC,
                    self.library.hook_proc(),
                    HINSTANCE::from(self.library.module()),
                    thread_id,
                )
            };
            match hooked {
                Ok(handle) => {
                    self.hooks.insert(
                        thread_id,
                        AttachedHook {
                            handle,
                            thread_created,
                        },
                    );
                    attached.push(thread_id);
                }
                Err(err) => {
                    if self.reported_failures.insert((thread_id, thread_created)) {
                        failed.push(AttachFailure {
                            thread_id,
                            reason: err.message(),
                        });
                    }
                }
            }
        }

        if !attached.is_empty() {
            if let Some(attach_id) = self.library.attach_message_id() {
                let target_windows: Vec<ShellWindow> = windows
                    .iter()
                    .filter(|w| attached.contains(&w.thread_id))
                    .cloned()
                    .collect();
                let _ = explorer::send_message_to_windows(&target_windows, attach_id);
            }
        }

        ScanReport {
            attached,
            failed,
            released,
            hooked: self.hooks.len(),
        }
    }

    /// Forgets hooks whose thread has exited. Windows removed those hooks together with the
    /// thread, and the id may already belong to a new thread that needs its own hook.
    fn release_exited_threads(&mut self) -> Vec<u32> {
        let exited: Vec<u32> = self
            .hooks
            .iter()
            .filter(|(thread_id, hook)| thread_created_at(**thread_id) != Some(hook.thread_created))
            .map(|(thread_id, _)| *thread_id)
            .collect();
        for thread_id in &exited {
            self.hooks.remove(thread_id);
        }
        exited
    }

    /// Tells the hook DLL to remove its window subclasses, then removes every hook.
    pub fn detach(&mut self) -> DetachReport {
        if self.hooks.is_empty() {
            return DetachReport::default();
        }
        // The DLL reacts to the detach message from inside its hook, so the message has to
        // arrive while the hooks are still installed.
        let windows = (self.windows)();
        let delivery = explorer::send_detach(&windows, self.library.detach_message_id());
        let hooks_removed = std::mem::take(&mut self.hooks)
            .into_values()
            .filter(|hook| unsafe { UnhookWindowsHookEx(hook.handle) }.is_ok())
            .count();
        DetachReport {
            windows: delivery.windows,
            windows_acknowledged: delivery.delivered,
            hooks_removed,
        }
    }
}

impl Drop for HookSession<'_> {
    fn drop(&mut self) {
        self.detach();
    }
}

/// Scans every `interval` until `stop` is set or `signal` fires, then detaches.
pub fn run_until_stopped(
    session: &mut HookSession<'_>,
    signal: &HostSignal,
    stop: &AtomicBool,
    interval: Duration,
    mut on_scan: impl FnMut(&ScanReport),
) -> DetachReport {
    while !stop.load(Ordering::SeqCst) {
        on_scan(&session.scan());
        if signal.wait_for_stop(interval) {
            stop.store(true, Ordering::SeqCst);
        }
    }
    session.detach()
}

unsafe extern "system" fn on_console_event(event: u32) -> BOOL {
    if !matches!(
        event,
        CTRL_C_EVENT
            | CTRL_BREAK_EVENT
            | CTRL_CLOSE_EVENT
            | CTRL_LOGOFF_EVENT
            | CTRL_SHUTDOWN_EVENT
    ) {
        return FALSE;
    }
    STOP_REQUESTED.store(true, Ordering::SeqCst);
    // Wakes the scan loop instead of letting it sleep out its interval.
    request_stop(HOST_EVENT_NAME);

    if event != CTRL_C_EVENT && event != CTRL_BREAK_EVENT {
        // Windows kills the process as soon as this handler returns, so it holds on until
        // the main thread has detached from Explorer.
        let deadline = Instant::now() + CONSOLE_CLOSE_GRACE;
        while !HOST_FINISHED.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    TRUE
}

/// `rwm stop`: asks a running hook host (including `rwm run --hidden`) to detach and exit.
pub fn stop() -> CliResult<ExitCode> {
    match stop_running_host(HOST_EVENT_NAME, Duration::from_secs(10)) {
        StopOutcome::Stopped => {
            print_line("The running hook host detached and exited.");
            Ok(ExitCode::SUCCESS)
        }
        StopOutcome::NotRunning => {
            print_line("No hook host is running in this session.");
            Ok(ExitCode::SUCCESS)
        }
        StopOutcome::TimedOut => Err(CliError::new(
            "the running hook host did not exit within 10 seconds.",
        )),
    }
}

/// `rwm run`.
pub fn run(hidden: bool) -> CliResult<ExitCode> {
    let exe =
        std::env::current_exe().context(|| "cannot locate the running executable".to_string())?;
    let dll_path = hookdll::locate(&exe).ok_or_else(|| hookdll::not_found_error(&exe))?;

    // Declared first so it is released last: the event disappearing tells `rwm uninstall`
    // that the hooks are gone.
    let signal = HostSignal::claim(HOST_EVENT_NAME).map_err(|err| match err {
        ClaimError::AlreadyRunning => CliError::new(
            "an rwm hook host is already running in this session. \
             Run `rwm stop` (or press Ctrl+C in its console) before starting another.",
        ),
        ClaimError::Os(err) => CliError::new(format!(
            "cannot create the host stop event: {}",
            err.message()
        )),
    })?;
    let library = HookLibrary::load(&dll_path)?;
    unsafe { SetConsoleCtrlHandler(Some(on_console_event), true) }
        .context(|| "cannot install the console control handler".to_string())?;

    if hidden {
        unsafe {
            let _ = FreeConsole();
        }
    } else {
        print_line(format!(
            "RustWindowsMenu hook host {}",
            env!("CARGO_PKG_VERSION")
        ));
        print_line(format!(
            "Hook DLL: {} (ABI {SUPPORTED_ABI})",
            library.path().display()
        ));
        let kill_switch = RwmConfig::kill_switch_path();
        if kill_switch.exists() {
            print_line(format!(
                "Note: the kill switch {} exists, so every right-click goes to the Windows menu \
                 until you delete that file.",
                kill_switch.display()
            ));
        }
        print_line("Press Ctrl+C to detach from Explorer and exit.");
    }

    let mut session = HookSession::new(&library, Box::new(explorer::enumerate_explorer_windows));
    let mut first_scan = true;
    let detached = run_until_stopped(
        &mut session,
        &signal,
        &STOP_REQUESTED,
        SCAN_INTERVAL,
        |report| {
            report_scan(report, first_scan);
            first_scan = false;
        },
    );
    drop(session);
    drop(library);

    print_line(format!(
        "Detached: {} of {} Explorer windows acknowledged, {} hooks removed.",
        detached.windows_acknowledged, detached.windows, detached.hooks_removed
    ));
    HOST_FINISHED.store(true, Ordering::SeqCst);
    Ok(ExitCode::SUCCESS)
}

fn report_scan(report: &ScanReport, first_scan: bool) {
    if !report.attached.is_empty() {
        print_line(format!(
            "Attached to {} Explorer UI thread(s), {} hooked in total.",
            report.attached.len(),
            report.hooked
        ));
    } else if first_scan && report.failed.is_empty() {
        print_line("No Explorer windows found yet. Waiting for explorer.exe.");
    }
    for failure in &report.failed {
        print_error_line(format!(
            "Could not hook Explorer thread {}: {}",
            failure.thread_id, failure.reason
        ));
    }
    if !report.released.is_empty() {
        tracing::debug!(threads = ?report.released, "hooked threads exited");
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PeImageFingerprint {
    machine: u16,
    num_sections: u16,
    timestamp: u32,
    size_of_code: u32,
    size_of_initialized_data: u32,
    entry_point_rva: u32,
    size_of_image: u32,
    checksum: u32,
    export_dir: (u32, u32),
    debug_dir: (u32, u32),
    codeview_guid_age: Option<[u8; 20]>,
}

fn read_u16_le(buf: &[u8], offset: usize) -> Option<u16> {
    let bytes: [u8; 2] = buf.get(offset..offset + 2)?.try_into().ok()?;
    Some(u16::from_le_bytes(bytes))
}

fn read_u32_le(buf: &[u8], offset: usize) -> Option<u32> {
    let bytes: [u8; 4] = buf.get(offset..offset + 4)?.try_into().ok()?;
    Some(u32::from_le_bytes(bytes))
}

fn read_pe_fingerprint(
    header: &[u8],
    read_rva: impl Fn(u32, usize, u32) -> Option<Vec<u8>>,
) -> Option<PeImageFingerprint> {
    if read_u16_le(header, 0)? != 0x5A4D {
        return None;
    }
    let e_lfanew = read_u32_le(header, 0x3C)? as usize;
    if read_u32_le(header, e_lfanew)? != 0x0000_4550 {
        return None;
    }
    let coff = e_lfanew.checked_add(4)?;
    let machine = read_u16_le(header, coff)?;
    let num_sections = read_u16_le(header, coff + 2)?;
    let timestamp = read_u32_le(header, coff + 4)?;
    let size_of_opt = read_u16_le(header, coff + 16)? as usize;

    let opt = coff.checked_add(20)?;
    // Require PE32+ (0x020B) so data directory offsets match 64-bit modules.
    if size_of_opt < 112 || read_u16_le(header, opt)? != 0x020B {
        return None;
    }
    let size_of_code = read_u32_le(header, opt + 4)?;
    let size_of_initialized_data = read_u32_le(header, opt + 8)?;
    let entry_point_rva = read_u32_le(header, opt + 16)?;
    let size_of_image = read_u32_le(header, opt + 56)?;
    let checksum = read_u32_le(header, opt + 64)?;
    let num_dirs = read_u32_le(header, opt + 108)?;

    let export_dir = if num_dirs > 0 && size_of_opt >= 120 {
        (
            read_u32_le(header, opt + 112)?,
            read_u32_le(header, opt + 116)?,
        )
    } else {
        (0, 0)
    };
    let debug_dir = if num_dirs > 6 && size_of_opt >= 168 {
        (
            read_u32_le(header, opt + 160)?,
            read_u32_le(header, opt + 164)?,
        )
    } else {
        (0, 0)
    };

    let mut codeview_guid_age = None;
    if debug_dir.0 != 0 && debug_dir.1 >= 28 {
        let count = (debug_dir.1 as usize / 28).min(8);
        if let Some(table) = read_rva(debug_dir.0, count * 28, size_of_image) {
            for entry in table.chunks_exact(28) {
                let debug_type = read_u32_le(entry, 12);
                let data_size = read_u32_le(entry, 16);
                let data_rva = read_u32_le(entry, 20);
                if debug_type == Some(2)
                    && data_size.is_some_and(|s| s >= 24)
                    && data_rva.is_some_and(|r| r != 0)
                {
                    if let Some(cv) = read_rva(data_rva.unwrap(), 24, size_of_image) {
                        if cv.get(..4) == Some(b"RSDS") {
                            if let Ok(arr) = <[u8; 20]>::try_from(&cv[4..24]) {
                                codeview_guid_age = Some(arr);
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    Some(PeImageFingerprint {
        machine,
        num_sections,
        timestamp,
        size_of_code,
        size_of_initialized_data,
        entry_point_rva,
        size_of_image,
        checksum,
        export_dir,
        debug_dir,
        codeview_guid_age,
    })
}

fn local_pe_fingerprint(module: HMODULE) -> Option<PeImageFingerprint> {
    if module.0.is_null() {
        return None;
    }
    let base = module.0 as *const u8;
    let header = unsafe { std::slice::from_raw_parts(base, 4096) };
    read_pe_fingerprint(header, |rva, len, image_size| {
        let end = (rva as usize).checked_add(len)?;
        if end > image_size as usize {
            return None;
        }
        let slice = unsafe { std::slice::from_raw_parts(base.add(rva as usize), len) };
        Some(slice.to_vec())
    })
}

fn is_hook_module_name(base_name: &str, expected_dll_name: &str) -> bool {
    if base_name.eq_ignore_ascii_case(expected_dll_name) {
        return true;
    }
    let prefix = format!("{expected_dll_name}.");
    base_name.len() > prefix.len()
        && base_name
            .get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(&prefix))
}

struct ProcessHandle(HANDLE);

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn read_remote_bytes(process: HANDLE, base: usize, len: usize) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; len];
    let mut read = 0usize;
    let ok = unsafe {
        ReadProcessMemory(
            process,
            base as *const std::ffi::c_void,
            buf.as_mut_ptr().cast(),
            len,
            Some(&mut read),
        )
    };
    if ok.is_err() || read < len {
        return None;
    }
    Some(buf)
}

fn remote_hook_dll_state(
    process_id: u32,
    expected_dll_name: &str,
    expected: &PeImageFingerprint,
) -> Result<(), String> {
    let Ok(raw) =
        (unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, process_id) })
    else {
        return Ok(());
    };
    let process = ProcessHandle(raw);

    let mut modules = vec![HMODULE::default(); 512];
    let mut needed = 0u32;
    let mut listed_ok = unsafe {
        K32EnumProcessModules(
            process.0,
            modules.as_mut_ptr(),
            (modules.len() * std::mem::size_of::<HMODULE>()) as u32,
            &mut needed,
        )
    }
    .as_bool();
    if !listed_ok {
        return Ok(());
    }
    let total = needed as usize / std::mem::size_of::<HMODULE>();
    if total > modules.len() {
        modules.resize(total, HMODULE::default());
        listed_ok = unsafe {
            K32EnumProcessModules(
                process.0,
                modules.as_mut_ptr(),
                (modules.len() * std::mem::size_of::<HMODULE>()) as u32,
                &mut needed,
            )
        }
        .as_bool();
        if !listed_ok {
            return Ok(());
        }
    }
    let count = (needed as usize / std::mem::size_of::<HMODULE>()).min(modules.len());

    for &module in &modules[..count] {
        let mut name_buf = [0u16; 260];
        let name_len = unsafe { K32GetModuleBaseNameW(process.0, module, &mut name_buf) } as usize;
        if name_len == 0 {
            continue;
        }
        let base_name = String::from_utf16_lossy(&name_buf[..name_len]);
        if !is_hook_module_name(&base_name, expected_dll_name) {
            continue;
        }
        let base_addr = module.0 as usize;
        let remote_fp = read_remote_bytes(process.0, base_addr, 4096).and_then(|hdr| {
            read_pe_fingerprint(&hdr, |rva, len, image_size| {
                let end = (rva as usize).checked_add(len)?;
                if end > image_size as usize {
                    return None;
                }
                read_remote_bytes(process.0, base_addr.checked_add(rva as usize)?, len)
            })
        });
        if remote_fp.as_ref() != Some(expected) {
            return Err(format!(
                "process PID {process_id} still has an older or different build of {expected_dll_name} \
                 mapped ({base_name}); restart explorer.exe before running the updated hook host"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::ffi::c_void;
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    use std::sync::{mpsc, Arc, Mutex};
    use std::thread::JoinHandle;

    use windows::core::w;
    use windows::Win32::Foundation::{HMODULE, HWND, LPARAM, WPARAM};
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, DispatchMessageW, GetMessageW, PostThreadMessageW,
        SendMessageTimeoutW, MSG, SMTO_ABORTIFHUNG, WINDOW_EX_STYLE, WINDOW_STYLE, WM_NULL,
        WM_QUIT,
    };

    use super::*;
    use crate::explorer::WindowKind;
    use crate::hookdll::HOOK_DLL_FILE;
    use crate::testutil::unique_name;

    /// Creates a hidden window on this thread, reports it, and pumps messages until `WM_QUIT`.
    fn run_hidden_window(report: impl FnOnce(isize, u32)) {
        unsafe {
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("Static"),
                w!("rwm host test window"),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                None,
                None,
                None,
                None,
            )
            .expect("create hidden test window");
            report(hwnd.0 as isize, GetCurrentThreadId());
            let mut message = MSG::default();
            while GetMessageW(&mut message, None, 0, 0).as_bool() {
                DispatchMessageW(&message);
            }
            let _ = DestroyWindow(hwnd);
        }
    }

    fn stop_window_thread(thread_id: u32) {
        unsafe {
            let _ = PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
    }

    /// A hidden window with its own message loop, standing in for an Explorer UI thread.
    /// It lives in this test process, so nothing here touches the real Explorer.
    struct WindowThread {
        window: ShellWindow,
        join: Option<JoinHandle<()>>,
    }

    impl WindowThread {
        fn spawn() -> Self {
            let (ready, started) = mpsc::channel();
            let join = std::thread::spawn(move || {
                run_hidden_window(|hwnd, thread_id| {
                    ready.send((hwnd, thread_id)).expect("report window");
                });
            });
            let (hwnd, thread_id) = started.recv().expect("window thread started");
            Self {
                window: ShellWindow {
                    hwnd,
                    kind: WindowKind::TopLevel,
                    class_name: "CabinetWClass".to_string(),
                    process_id: std::process::id(),
                    thread_id,
                },
                join: Some(join),
            }
        }

        fn stop(&mut self) {
            if let Some(join) = self.join.take() {
                stop_window_thread(self.window.thread_id);
                join.join().expect("window thread exits");
            }
        }
    }

    impl Drop for WindowThread {
        fn drop(&mut self) {
            self.stop();
        }
    }

    const WINDOW_HOST_ENV: &str = "RWM_TEST_WINDOW_HOST";
    const WINDOW_HOST_MARKER: &str = "rwm-window-host";

    /// Not a test of its own. `hook_dll_is_injected_into_another_process` starts this test
    /// binary again with the variable set, and that child runs a hidden window here.
    #[test]
    #[ignore = "child process entry point of hook_dll_is_injected_into_another_process"]
    fn window_host_child() {
        if std::env::var_os(WINDOW_HOST_ENV).is_none() {
            return;
        }
        run_hidden_window(|hwnd, thread_id| {
            println!("{WINDOW_HOST_MARKER} {hwnd} {thread_id}");
        });
    }

    /// A second process with one hidden window: a stand-in for `explorer.exe` that the hook
    /// DLL treats as "not Explorer" and therefore leaves alone once it is loaded.
    struct WindowProcess {
        child: Child,
        window: ShellWindow,
    }

    impl WindowProcess {
        fn spawn() -> Self {
            let mut child = Command::new(std::env::current_exe().expect("test executable path"))
                .args([
                    "--exact",
                    "host::tests::window_host_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env(WINDOW_HOST_ENV, "1")
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("start the window host process");
            let stdout = BufReader::new(child.stdout.take().expect("child stdout"));
            let (hwnd, thread_id) = stdout
                .lines()
                .map_while(Result::ok)
                .find_map(|line| {
                    let mut fields = line.strip_prefix(WINDOW_HOST_MARKER)?.split_whitespace();
                    Some((fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
                })
                .expect("the window host reports its window");
            let window = ShellWindow {
                hwnd,
                kind: WindowKind::TopLevel,
                class_name: "CabinetWClass".to_string(),
                process_id: child.id(),
                thread_id,
            };
            Self { child, window }
        }

        /// Makes the window's thread process a sent message. Windows loads a hook DLL into a
        /// process the first time one of its hooked threads does that.
        fn nudge(&self) {
            unsafe {
                SendMessageTimeoutW(
                    HWND(self.window.hwnd as *mut c_void),
                    WM_NULL,
                    WPARAM(0),
                    LPARAM(0),
                    SMTO_ABORTIFHUNG,
                    2000,
                    None,
                );
            }
        }

        fn has_module(&self, file_name: &str) -> bool {
            unsafe {
                let process = OpenProcess(
                    PROCESS_QUERY_INFORMATION | PROCESS_VM_READ,
                    false,
                    self.child.id(),
                )
                .expect("open the window host process");
                let mut modules = [HMODULE::default(); 512];
                let mut bytes = 0u32;
                let listed = K32EnumProcessModules(
                    process,
                    modules.as_mut_ptr(),
                    std::mem::size_of_val(&modules) as u32,
                    &mut bytes,
                );
                assert!(listed.as_bool(), "list the window host's modules");
                let count = (bytes as usize / std::mem::size_of::<HMODULE>()).min(modules.len());
                let found = modules[..count].iter().any(|module| {
                    let mut name = [0u16; 260];
                    let len = K32GetModuleBaseNameW(process, *module, &mut name) as usize;
                    String::from_utf16_lossy(&name[..len]).eq_ignore_ascii_case(file_name)
                });
                let _ = CloseHandle(process);
                found
            }
        }
    }

    impl Drop for WindowProcess {
        fn drop(&mut self) {
            stop_window_thread(self.window.thread_id);
            let exited = (0..100).any(|_| {
                std::thread::sleep(Duration::from_millis(20));
                matches!(self.child.try_wait(), Ok(Some(_)))
            });
            if !exited {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }

    fn built_hook_dll() -> Option<std::path::PathBuf> {
        let exe = std::env::current_exe().expect("test executable path");
        let dll = hookdll::locate(&exe);
        if dll.is_none() {
            eprintln!("skipped: rwm_hook.dll is not built (cargo build -p rwm-hook)");
        }
        dll
    }

    fn next_scan(
        scanned: &mpsc::Receiver<ScanReport>,
        wanted: impl Fn(&ScanReport) -> bool,
    ) -> ScanReport {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let report = scanned
                .recv_timeout(remaining)
                .expect("the host loop keeps scanning");
            if wanted(&report) {
                return report;
            }
        }
    }

    #[test]
    fn only_one_host_can_claim_the_event() {
        let name = format!(r"Local\{}", unique_name("claim"));
        assert!(!is_host_running(&name));

        let signal = HostSignal::claim(&name).expect("first claim");
        assert!(is_host_running(&name));
        assert!(matches!(
            HostSignal::claim(&name),
            Err(ClaimError::AlreadyRunning)
        ));

        drop(signal);
        assert!(!is_host_running(&name));
        drop(HostSignal::claim(&name).expect("claim again after release"));
    }

    #[test]
    fn stop_request_reaches_the_owner_of_the_event() {
        let name = format!(r"Local\{}", unique_name("stop"));
        assert_eq!(
            stop_running_host(&name, Duration::from_millis(50)),
            StopOutcome::NotRunning
        );

        let signal = HostSignal::claim(&name).expect("claim");
        assert!(!signal.wait_for_stop(Duration::ZERO));
        assert_eq!(
            stop_running_host(&name, Duration::from_millis(100)),
            StopOutcome::TimedOut,
            "the owner is still alive"
        );
        assert!(signal.wait_for_stop(Duration::ZERO));
    }

    #[test]
    fn a_reused_thread_id_is_told_apart_by_creation_time() {
        let current = unsafe { GetCurrentThreadId() };
        let created = thread_created_at(current).expect("current thread is alive");
        assert_eq!(thread_created_at(current), Some(created));

        let (thread_id, created_inside) = std::thread::spawn(|| {
            let thread_id = unsafe { GetCurrentThreadId() };
            (thread_id, thread_created_at(thread_id))
        })
        .join()
        .expect("probe thread");
        assert!(created_inside.is_some());
        assert_ne!(
            thread_created_at(thread_id),
            created_inside,
            "the thread has exited"
        );
    }

    #[test]
    fn session_hooks_each_thread_once_and_detaches_everything() {
        let Some(dll) = built_hook_dll() else {
            return;
        };
        let library = HookLibrary::load(&dll).expect("hook DLL loads");
        let mut first = WindowThread::spawn();
        let second = WindowThread::spawn();
        let windows = Arc::new(Mutex::new(vec![first.window.clone()]));

        let listed = Arc::clone(&windows);
        let mut session =
            HookSession::new(&library, Box::new(move || listed.lock().unwrap().clone()));

        let scan = session.scan();
        assert_eq!(scan.attached, vec![first.window.thread_id]);
        assert_eq!(scan.failed, Vec::new());
        assert_eq!(scan.hooked, 1);

        let rescan = session.scan();
        assert_eq!(
            rescan.attached,
            Vec::<u32>::new(),
            "no second hook on the same thread"
        );
        assert_eq!(rescan.hooked, 1);

        windows.lock().unwrap().push(second.window.clone());
        let grown = session.scan();
        assert_eq!(grown.attached, vec![second.window.thread_id]);
        assert_eq!(grown.hooked, 2);

        first.stop();
        windows.lock().unwrap().remove(0);
        let after_exit = session.scan();
        assert_eq!(after_exit.released, vec![first.window.thread_id]);
        assert_eq!(after_exit.hooked, 1);

        let detached = session.detach();
        assert_eq!(
            detached,
            DetachReport {
                windows: 1,
                windows_acknowledged: 1,
                hooks_removed: 1
            }
        );
        assert_eq!(
            session.detach(),
            DetachReport::default(),
            "nothing is left to detach"
        );
    }

    #[test]
    fn host_loop_picks_up_new_threads_and_stops_on_request() {
        let Some(dll) = built_hook_dll() else {
            return;
        };
        let name = format!(r"Local\{}", unique_name("loop"));
        let first = WindowThread::spawn();
        let second = WindowThread::spawn();
        let windows = Arc::new(Mutex::new(vec![first.window.clone()]));
        let (scans, scanned) = mpsc::channel::<ScanReport>();
        let stop = AtomicBool::new(false);

        let detached = std::thread::scope(|scope| {
            let host = scope.spawn(|| {
                let library = HookLibrary::load(&dll).expect("hook DLL loads");
                let signal = HostSignal::claim(&name).expect("claim host event");
                let listed = Arc::clone(&windows);
                let mut session =
                    HookSession::new(&library, Box::new(move || listed.lock().unwrap().clone()));
                run_until_stopped(
                    &mut session,
                    &signal,
                    &stop,
                    Duration::from_millis(10),
                    |report| {
                        let _ = scans.send(report.clone());
                    },
                )
            });

            let first_scan = next_scan(&scanned, |_| true);
            assert_eq!(first_scan.attached, vec![first.window.thread_id]);

            windows.lock().unwrap().push(second.window.clone());
            let picked_up = next_scan(&scanned, |report| !report.attached.is_empty());
            assert_eq!(picked_up.attached, vec![second.window.thread_id]);
            assert_eq!(picked_up.hooked, 2);

            assert_eq!(
                stop_running_host(&name, Duration::from_secs(5)),
                StopOutcome::Stopped
            );
            host.join().expect("host thread")
        });

        assert_eq!(
            detached,
            DetachReport {
                windows: 2,
                windows_acknowledged: 2,
                hooks_removed: 2
            }
        );
        assert!(stop.load(Ordering::SeqCst));
        assert!(!is_host_running(&name));
    }

    #[test]
    fn hook_dll_is_injected_into_another_process() {
        let Some(dll) = built_hook_dll() else {
            return;
        };
        let library = HookLibrary::load(&dll).expect("hook DLL loads");
        let target = WindowProcess::spawn();
        let listed = vec![target.window.clone()];
        let mut session = HookSession::new(&library, Box::new(move || listed.clone()));

        target.nudge();
        assert!(
            !target.has_module(HOOK_DLL_FILE),
            "nothing is injected before the hook"
        );

        assert_eq!(session.scan().attached, vec![target.window.thread_id]);
        target.nudge();
        assert!(
            target.has_module(HOOK_DLL_FILE),
            "the hook brought the DLL into the other process"
        );

        assert_eq!(
            session.detach(),
            DetachReport {
                windows: 1,
                windows_acknowledged: 1,
                hooks_removed: 1
            }
        );
    }

    #[test]
    fn remote_pe_fingerprint_accepts_matching_build_and_rejects_mismatched_build() {
        let Some(dll) = built_hook_dll() else {
            return;
        };
        let library = HookLibrary::load(&dll).expect("hook DLL loads");
        let fp = local_pe_fingerprint(library.module()).expect("parse loaded PE fingerprint");
        assert!(
            remote_hook_dll_state(std::process::id(), HOOK_DLL_FILE, &fp).is_ok(),
            "same build already loaded in this process should pass"
        );

        let mut wrong = fp.clone();
        wrong.timestamp ^= 1;
        let err = remote_hook_dll_state(std::process::id(), HOOK_DLL_FILE, &wrong)
            .expect_err("mismatched PE fingerprint must be rejected");
        assert!(err.contains("restart explorer.exe"), "{err}");

        assert!(is_hook_module_name("rwm_hook.dll", HOOK_DLL_FILE));
        assert!(is_hook_module_name("RWM_HOOK.DLL.1234.old", HOOK_DLL_FILE));
        assert!(!is_hook_module_name("kernel32.dll", HOOK_DLL_FILE));
    }
}
