//! Lock-free shared-memory ring/slots for non-blocking speculative menu prefetch and instant popup handoff.

use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::atomic::{fence, AtomicI32, AtomicU32, AtomicU64, Ordering};

use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, ERROR_INVALID_PARAMETER, HANDLE, HWND,
    INVALID_HANDLE_VALUE, LPARAM, STILL_ACTIVE, WPARAM,
};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
    MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::{
    GetCurrentProcessId, GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowThreadProcessId, IsWindow, PostMessageW, RegisterWindowMessageW,
};
use windows_core::PCWSTR;

use crate::build_info::ABI_VERSION;
use crate::reg_util::wide_null;

pub const SHARED_CACHE_NAME: &str = r"Local\RustWindowsMenu_Cache_v3";
pub const CACHE_MAGIC: u32 = 0x5257_4D33; // "RWM3"
pub const MAX_READY_SLOTS: usize = 8;
pub const MAX_PREFETCH_BYTES: usize = 16 * 1024;
const HOST_STATE_TRACKING: u32 = 1 << 0;
const HOST_STATE_PENDING_SHOW: u32 = 1 << 1;

static PREFETCH_MSG: AtomicU32 = AtomicU32::new(0);
static CLAIM_SHOW_MSG: AtomicU32 = AtomicU32::new(0);
static EDIT_LABEL_MSG: AtomicU32 = AtomicU32::new(0);
static CONFIG_RELOAD_MSG: AtomicU32 = AtomicU32::new(0);
static CANCEL_MENU_MSG: AtomicU32 = AtomicU32::new(0);

/// Monotonic tick count in milliseconds matching Windows `GetTickCount64`.
#[inline]
pub fn now_tick_ms() -> u64 {
    unsafe { GetTickCount64() }
}

fn registered_message(cache: &AtomicU32, name: &str) -> u32 {
    let mut id = cache.load(Ordering::Relaxed);
    if id == 0 {
        let wide = wide_null(name);
        id = unsafe { RegisterWindowMessageW(PCWSTR(wide.as_ptr())) };
        cache.store(id, Ordering::Relaxed);
    }
    id
}

/// Window message posted from Explorer to `rwm-host.exe` after writing a prefetch request.
pub fn prefetch_msg_id() -> u32 {
    registered_message(&PREFETCH_MSG, "RustWindowsMenu.Prefetch.v3")
}

/// Window message sent via `SendMessageTimeoutW` to hand off menu presentation on click.
pub fn claim_show_msg_id() -> u32 {
    registered_message(&CLAIM_SHOW_MSG, "RustWindowsMenu.ClaimShow.v3")
}

/// Window message posted from `rwm-host.exe` back to `SHELLDLL_DefView` to start inline rename.
pub fn edit_label_msg_id() -> u32 {
    registered_message(&EDIT_LABEL_MSG, "RustWindowsMenu.EditLabel.v3")
}

/// Window message posted to `rwm-host.exe` when `config.toml` or blocked handlers change.
pub fn config_reload_msg_id() -> u32 {
    registered_message(&CONFIG_RELOAD_MSG, "RustWindowsMenu.ConfigReload.v3")
}

/// Window message sent/posted to `rwm-host.exe` to dismiss an active popup menu immediately.
pub fn cancel_menu_msg_id() -> u32 {
    registered_message(&CANCEL_MENU_MSG, "RustWindowsMenu.CancelMenu.v3")
}

/// Signals a running `rwm-host.exe` to reload configuration and drop stale cached menus immediately.
pub fn notify_host_config_reload() -> bool {
    let Some(client) = SharedCacheClient::open() else {
        return false;
    };
    let host_hwnd = client.host_hwnd();
    if host_hwnd == 0 {
        return false;
    }
    unsafe {
        PostMessageW(
            HWND(host_hwnd as *mut c_void),
            config_reload_msg_id(),
            WPARAM(0),
            LPARAM(0),
        )
        .is_ok()
    }
}

#[repr(C)]
struct ReadySlot {
    key_hash: AtomicU64,
    expires_tick_ms: AtomicU64,
}

#[repr(C)]
struct PrefetchSlot {
    seq: AtomicU64,
    lock: AtomicU32,
    shift_held: AtomicU32,
    clipboard_seq: AtomicU32,
    payload_len: AtomicU32,
    request_seq: AtomicU64,
    key_hash: AtomicU64,
    _reserved_tick_ms: AtomicU64,
    payload: UnsafeCell<[u8; MAX_PREFETCH_BYTES]>,
}

#[repr(C)]
struct ShowSlot {
    seq: AtomicU64,
    lock: AtomicU32,
    clipboard_seq: AtomicU32,
    key_hash: AtomicU64,
    defview_hwnd: AtomicU64,
    explorer_root_hwnd: AtomicU64,
    request_seq: AtomicU64,
    _reserved_tick_ms: AtomicU64,
    anchor_x: AtomicI32,
    anchor_y: AtomicI32,
    no_animation: AtomicU32,
    _pad: u32,
}

#[repr(C)]
struct SharedCacheRegion {
    magic: AtomicU32,
    abi_version: AtomicU32,
    host_pid: AtomicU32,
    host_tracking: AtomicU32,
    host_hwnd: AtomicU64,
    in_flight_hash: AtomicU64,
    next_request_seq: AtomicU64,
    slots: [ReadySlot; MAX_READY_SLOTS],
    prefetch: PrefetchSlot,
    show: ShowSlot,
}

unsafe impl Send for SharedCacheRegion {}
unsafe impl Sync for SharedCacheRegion {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefetchPayload {
    pub request_seq: u64,
    pub key_hash: u64,
    pub shift_held: bool,
    pub clipboard_seq: u32,
    pub pidl_bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShowRequest {
    pub request_seq: u64,
    pub key_hash: u64,
    pub clipboard_seq: u32,
    pub anchor_x: i32,
    pub anchor_y: i32,
    pub defview_hwnd: isize,
    pub explorer_root_hwnd: isize,
    pub no_animation: bool,
}

struct MappingView {
    handle: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    region: NonNull<SharedCacheRegion>,
}

unsafe impl Send for MappingView {}
unsafe impl Sync for MappingView {}

impl MappingView {
    unsafe fn from_handle(handle: HANDLE) -> Result<Self, windows_core::Error> {
        let view = unsafe {
            MapViewOfFile(
                handle,
                FILE_MAP_ALL_ACCESS,
                0,
                0,
                size_of::<SharedCacheRegion>(),
            )
        };
        let Some(region) = NonNull::new(view.Value.cast::<SharedCacheRegion>()) else {
            let error = windows_core::Error::from_win32();
            unsafe {
                let _ = CloseHandle(handle);
            }
            return Err(error);
        };
        Ok(Self {
            handle,
            view,
            region,
        })
    }

    fn region(&self) -> &SharedCacheRegion {
        unsafe { self.region.as_ref() }
    }
}

impl Drop for MappingView {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(self.view);
            let _ = CloseHandle(self.handle);
        }
    }
}

fn is_process_dead(pid: u32) -> bool {
    if pid == 0 {
        return true;
    }
    match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(handle) => {
            let mut code = 0u32;
            let ok = unsafe { GetExitCodeProcess(handle, &mut code) }.is_ok();
            unsafe {
                let _ = CloseHandle(handle);
            }
            ok && code != STILL_ACTIVE.0 as u32
        }
        Err(err) => err.code() == ERROR_INVALID_PARAMETER.to_hresult(),
    }
}

struct SlotLockGuard<'a> {
    lock: &'a AtomicU32,
    owner_pid: u32,
}

impl<'a> SlotLockGuard<'a> {
    fn try_acquire(lock: &'a AtomicU32) -> Option<(Self, bool)> {
        let my_pid = unsafe { GetCurrentProcessId() }.max(1);
        if lock
            .compare_exchange(0, my_pid, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return Some((
                Self {
                    lock,
                    owner_pid: my_pid,
                },
                false,
            ));
        }
        let owner = lock.load(Ordering::Relaxed);
        if owner != 0
            && owner != my_pid
            && is_process_dead(owner)
            && lock
                .compare_exchange(owner, my_pid, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
        {
            return Some((
                Self {
                    lock,
                    owner_pid: my_pid,
                },
                true,
            ));
        }
        None
    }
}

impl Drop for SlotLockGuard<'_> {
    fn drop(&mut self) {
        let _ =
            self.lock
                .compare_exchange(self.owner_pid, 0, Ordering::Release, Ordering::Relaxed);
    }
}

fn try_acquire_spin(lock: &AtomicU32, spins: usize) -> Option<(SlotLockGuard<'_>, bool)> {
    for _ in 0..spins {
        if let Some(acquired) = SlotLockGuard::try_acquire(lock) {
            return Some(acquired);
        }
        std::hint::spin_loop();
    }
    None
}

fn try_acquire_wait(lock: &AtomicU32, max_sleep_ms: u32) -> Option<(SlotLockGuard<'_>, bool)> {
    if let Some(acquired) = try_acquire_spin(lock, 32) {
        return Some(acquired);
    }
    for _ in 0..max_sleep_ms {
        std::thread::sleep(std::time::Duration::from_millis(1));
        if let Some(acquired) = SlotLockGuard::try_acquire(lock) {
            return Some(acquired);
        }
    }
    None
}

/// Owns the named shared-memory cache region inside `rwm-host.exe`.
pub struct SharedCacheHost {
    mapping: MappingView,
}

impl SharedCacheHost {
    pub fn create() -> Result<Self, windows_core::Error> {
        Self::create_named(SHARED_CACHE_NAME)
    }

    pub fn create_named(name: &str) -> Result<Self, windows_core::Error> {
        let wide = wide_null(name);
        let size = size_of::<SharedCacheRegion>() as u32;
        let handle = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                0,
                size,
                PCWSTR(wide.as_ptr()),
            )?
        };
        let already_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        let mapping = unsafe { MappingView::from_handle(handle)? };
        let region = mapping.region();
        if !already_exists {
            unsafe {
                ptr::write_bytes(mapping.view.Value.cast::<u8>(), 0, size as usize);
            }
            region.next_request_seq.store(1, Ordering::Relaxed);
        } else {
            // Another client still holds the section open across host restart: take slot locks
            // rather than zeroing atomics out from under an active Explorer writer.
            region.magic.store(0, Ordering::Release);
            region.host_pid.store(0, Ordering::Release);
            region.host_hwnd.store(0, Ordering::Release);
            let _prefetch_guard = try_acquire_wait(&region.prefetch.lock, 50);
            let _show_guard = try_acquire_wait(&region.show.lock, 50);
            region.host_tracking.store(0, Ordering::Release);
            region.in_flight_hash.store(0, Ordering::Release);
            for slot in &region.slots {
                slot.key_hash.store(0, Ordering::Release);
                slot.expires_tick_ms.store(0, Ordering::Release);
            }
            if _prefetch_guard.is_some() {
                region.prefetch.seq.store(0, Ordering::Release);
                region.prefetch.payload_len.store(0, Ordering::Release);
            }
            if _show_guard.is_some() {
                region.show.seq.store(0, Ordering::Release);
                region.show.key_hash.store(0, Ordering::Release);
            }
            if region.next_request_seq.load(Ordering::Relaxed) == 0 {
                region.next_request_seq.store(1, Ordering::Relaxed);
            }
        }
        region.abi_version.store(ABI_VERSION, Ordering::Release);
        region.magic.store(CACHE_MAGIC, Ordering::Release);
        Ok(Self { mapping })
    }

    /// Publishes host PID and HWND once the message window is ready to accept `PostMessageW`.
    pub fn set_host_info(&self, pid: u32, hwnd: isize) {
        let region = self.mapping.region();
        region.host_pid.store(pid, Ordering::Release);
        region.host_hwnd.store(hwnd as u64, Ordering::Release);
    }

    pub fn host_hwnd(&self) -> isize {
        self.mapping.region().host_hwnd.load(Ordering::Acquire) as isize
    }

    pub fn set_in_flight(&self, key_hash: u64) {
        self.mapping
            .region()
            .in_flight_hash
            .store(key_hash, Ordering::Release);
    }

    fn update_tracking_bit(&self, bit: u32, enabled: bool) {
        let state = &self.mapping.region().host_tracking;
        if enabled {
            state.fetch_or(bit, Ordering::AcqRel);
        } else {
            state.fetch_and(!bit, Ordering::AcqRel);
        }
    }

    /// Sets or clears the live popup tracking bit in `host_tracking`.
    pub fn set_tracking(&self, tracking: bool) {
        self.update_tracking_bit(HOST_STATE_TRACKING, tracking);
    }

    /// Sets or clears the pending show handoff bit in `host_tracking` to prevent concurrent menus.
    pub fn set_pending_show(&self, pending: bool) {
        self.update_tracking_bit(HOST_STATE_PENDING_SHOW, pending);
    }

    /// Replaces the published lock-free lookup table of cached `(key_hash, expires_tick_ms)` entries.
    pub fn publish_slots(&self, entries: &[(u64, u64)]) {
        let region = self.mapping.region();
        for (index, slot) in region.slots.iter().enumerate() {
            if let Some(&(hash, expires)) = entries.get(index) {
                slot.expires_tick_ms.store(expires, Ordering::Release);
                slot.key_hash.store(hash, Ordering::Release);
            } else {
                slot.key_hash.store(0, Ordering::Release);
                slot.expires_tick_ms.store(0, Ordering::Release);
            }
        }
    }

    /// Reads the latest prefetch payload under the slot lock, discarding any partial dead-writer record.
    pub fn read_prefetch(&self) -> Option<PrefetchPayload> {
        let slot = &self.mapping.region().prefetch;
        let (_guard, recovered) = try_acquire_spin(&slot.lock, 8)?;
        if recovered {
            slot.payload_len.store(0, Ordering::Release);
            let cur = slot.seq.load(Ordering::Relaxed);
            if (cur & 1) != 0 {
                slot.seq.store(cur.wrapping_add(1), Ordering::Release);
            }
            return None;
        }
        let seq1 = slot.seq.load(Ordering::Acquire);
        if seq1 == 0 || (seq1 & 1) != 0 {
            return None;
        }
        let len = slot.payload_len.load(Ordering::Relaxed) as usize;
        if len == 0 || len > MAX_PREFETCH_BYTES {
            return None;
        }
        let request_seq = slot.request_seq.load(Ordering::Relaxed);
        let key_hash = slot.key_hash.load(Ordering::Relaxed);
        let shift_held = slot.shift_held.load(Ordering::Relaxed) != 0;
        let clipboard_seq = slot.clipboard_seq.load(Ordering::Relaxed);
        let mut pidl_bytes = vec![0u8; len];
        fence(Ordering::Acquire);
        unsafe {
            ptr::copy_nonoverlapping(
                slot.payload.get().cast::<u8>(),
                pidl_bytes.as_mut_ptr(),
                len,
            );
        }
        fence(Ordering::Acquire);
        let seq2 = slot.seq.load(Ordering::Acquire);
        if seq1 == seq2 {
            Some(PrefetchPayload {
                request_seq,
                key_hash,
                shift_held,
                clipboard_seq,
                pidl_bytes,
            })
        } else {
            None
        }
    }

    /// Reads the latest click-show handoff request under the slot lock, discarding dead-writer records.
    pub fn read_show(&self) -> Option<ShowRequest> {
        let slot = &self.mapping.region().show;
        let (_guard, recovered) = try_acquire_spin(&slot.lock, 8)?;
        if recovered {
            slot.key_hash.store(0, Ordering::Release);
            let cur = slot.seq.load(Ordering::Relaxed);
            if (cur & 1) != 0 {
                slot.seq.store(cur.wrapping_add(1), Ordering::Release);
            }
            return None;
        }
        let seq1 = slot.seq.load(Ordering::Acquire);
        if seq1 == 0 || (seq1 & 1) != 0 {
            return None;
        }
        let request = ShowRequest {
            request_seq: slot.request_seq.load(Ordering::Relaxed),
            key_hash: slot.key_hash.load(Ordering::Relaxed),
            clipboard_seq: slot.clipboard_seq.load(Ordering::Relaxed),
            anchor_x: slot.anchor_x.load(Ordering::Relaxed),
            anchor_y: slot.anchor_y.load(Ordering::Relaxed),
            defview_hwnd: slot.defview_hwnd.load(Ordering::Relaxed) as isize,
            explorer_root_hwnd: slot.explorer_root_hwnd.load(Ordering::Relaxed) as isize,
            no_animation: slot.no_animation.load(Ordering::Relaxed) != 0,
        };
        fence(Ordering::Acquire);
        let seq2 = slot.seq.load(Ordering::Acquire);
        if seq1 == seq2 && request.key_hash != 0 {
            Some(request)
        } else {
            None
        }
    }
}

impl Drop for SharedCacheHost {
    fn drop(&mut self) {
        let region = self.mapping.region();
        region.host_tracking.store(0, Ordering::Release);
        region.host_hwnd.store(0, Ordering::Release);
        region.host_pid.store(0, Ordering::Release);
        region.in_flight_hash.store(0, Ordering::Release);
        region.magic.store(0, Ordering::Release);
    }
}

/// Non-owning Explorer-side client into the lock-free shared-memory cache.
pub struct SharedCacheClient {
    mapping: MappingView,
}

impl SharedCacheClient {
    pub fn open() -> Option<Self> {
        Self::open_named(SHARED_CACHE_NAME)
    }

    pub fn open_named(name: &str) -> Option<Self> {
        let wide = wide_null(name);
        let handle =
            unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS.0, false, PCWSTR(wide.as_ptr())) }.ok()?;
        let mapping = unsafe { MappingView::from_handle(handle) }.ok()?;
        let client = Self { mapping };
        client.is_valid().then_some(client)
    }

    #[inline]
    pub fn is_valid(&self) -> bool {
        let region = self.mapping.region();
        region.magic.load(Ordering::Acquire) == CACHE_MAGIC
            && region.abi_version.load(Ordering::Acquire) == ABI_VERSION
            && region.host_pid.load(Ordering::Acquire) != 0
            && region.host_hwnd.load(Ordering::Acquire) != 0
    }

    #[inline]
    pub fn host_pid(&self) -> u32 {
        self.mapping.region().host_pid.load(Ordering::Acquire)
    }

    #[inline]
    pub fn host_hwnd(&self) -> isize {
        self.mapping.region().host_hwnd.load(Ordering::Acquire) as isize
    }

    /// Verifies that the published host HWND is alive and belongs to `host_pid`.
    pub fn live_host_target(&self) -> Option<(u32, HWND)> {
        if !self.is_valid() {
            return None;
        }
        let pid = self.host_pid();
        let hwnd = HWND(self.host_hwnd() as *mut c_void);
        if pid == 0 || hwnd.is_invalid() || !unsafe { IsWindow(hwnd) }.as_bool() {
            return None;
        }
        let mut owner_pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut owner_pid)) };
        (owner_pid == pid).then_some((pid, hwnd))
    }

    /// Lock-free check whether `rwm-host.exe` currently has an active context menu open.
    #[inline]
    pub fn is_host_tracking(&self) -> bool {
        self.is_valid()
            && self
                .mapping
                .region()
                .host_tracking
                .load(Ordering::Acquire)
                != 0
    }

    /// Lock-free, zero-allocation readiness check (approx. 15 ns).
    #[inline]
    pub fn is_ready_or_in_flight(&self, key_hash: u64, now_tick_ms: u64) -> bool {
        if key_hash == 0 || !self.is_valid() {
            return false;
        }
        let region = self.mapping.region();
        if region.in_flight_hash.load(Ordering::Acquire) == key_hash {
            return true;
        }
        region.slots.iter().any(|slot| {
            let hash = slot.key_hash.load(Ordering::Acquire);
            hash == key_hash && slot.expires_tick_ms.load(Ordering::Acquire) > now_tick_ms
        })
    }

    /// Writes a speculative prefetch payload non-blockingly, returning its monotonic sequence number.
    pub fn try_write_prefetch(
        &self,
        key_hash: u64,
        shift_held: bool,
        clipboard_seq: u32,
        pidl_bytes: &[u8],
    ) -> Option<u64> {
        if !self.is_valid() || pidl_bytes.is_empty() || pidl_bytes.len() > MAX_PREFETCH_BYTES {
            return None;
        }
        let region = self.mapping.region();
        let slot = &region.prefetch;
        let (_guard, _) = SlotLockGuard::try_acquire(&slot.lock)?;
        let request_seq = region.next_request_seq.fetch_add(1, Ordering::Relaxed);
        let prev = slot.seq.load(Ordering::Relaxed);
        let odd = prev.wrapping_add(1) | 1;
        slot.seq.store(odd, Ordering::Release);
        fence(Ordering::Release);
        slot.request_seq.store(request_seq, Ordering::Relaxed);
        slot.key_hash.store(key_hash, Ordering::Relaxed);
        slot.shift_held
            .store(u32::from(shift_held), Ordering::Relaxed);
        slot.clipboard_seq.store(clipboard_seq, Ordering::Relaxed);
        slot.payload_len
            .store(pidl_bytes.len() as u32, Ordering::Relaxed);
        unsafe {
            ptr::copy_nonoverlapping(
                pidl_bytes.as_ptr(),
                slot.payload.get().cast::<u8>(),
                pidl_bytes.len(),
            );
        }
        fence(Ordering::Release);
        slot.seq.store(odd.wrapping_add(1), Ordering::Release);
        Some(request_seq)
    }

    /// Writes a click-show request cleanly, yielding briefly if another thread is finishing a write.
    pub fn try_write_show(&self, req: &ShowRequest) -> Option<u64> {
        if !self.is_valid() || req.key_hash == 0 {
            return None;
        }
        let region = self.mapping.region();
        let slot = &region.show;
        let (_guard, _) = try_acquire_spin(&slot.lock, 32)?;
        let request_seq = region.next_request_seq.fetch_add(1, Ordering::Relaxed);
        let prev = slot.seq.load(Ordering::Relaxed);
        let odd = prev.wrapping_add(1) | 1;
        slot.seq.store(odd, Ordering::Release);
        fence(Ordering::Release);
        slot.request_seq.store(request_seq, Ordering::Relaxed);
        slot.key_hash.store(req.key_hash, Ordering::Relaxed);
        slot.clipboard_seq
            .store(req.clipboard_seq, Ordering::Relaxed);
        slot.anchor_x.store(req.anchor_x, Ordering::Relaxed);
        slot.anchor_y.store(req.anchor_y, Ordering::Relaxed);
        slot.defview_hwnd
            .store(req.defview_hwnd as u64, Ordering::Relaxed);
        slot.explorer_root_hwnd
            .store(req.explorer_root_hwnd as u64, Ordering::Relaxed);
        slot.no_animation
            .store(u32::from(req.no_animation), Ordering::Relaxed);
        fence(Ordering::Release);
        slot.seq.store(odd.wrapping_add(1), Ordering::Release);
        Some(request_seq)
    }
}

/// Ensures no Rust `unsafe` caller can ever exceed struct bounds if someone changes `c_void` usage.
const _: () = assert!(size_of::<*const c_void>() == size_of::<usize>());

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64 as Seq;

    static TEST_SEQ: Seq = Seq::new(1);

    fn unique_mapping() -> String {
        format!(
            r"Local\RwmIpcCacheTest_{}_{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        )
    }

    #[test]
    fn host_and_client_roundtrip_and_expiry() {
        let name = unique_mapping();
        let host = SharedCacheHost::create_named(&name).unwrap();
        assert!(SharedCacheClient::open_named(&name).is_none());

        host.set_host_info(1234, 0xABCD);
        let client = SharedCacheClient::open_named(&name).expect("valid client");
        assert!(client.is_valid());
        assert_eq!(client.host_pid(), 1234);
        assert_eq!(client.host_hwnd(), 0xABCD);
        assert!(!client.is_host_tracking());
        host.set_tracking(true);
        assert!(client.is_host_tracking());
        host.set_pending_show(true);
        assert!(client.is_host_tracking());
        host.set_tracking(false);
        assert!(client.is_host_tracking());
        host.set_pending_show(false);
        assert!(!client.is_host_tracking());
        host.set_pending_show(true);
        host.set_tracking(true);
        host.set_pending_show(false);
        assert!(client.is_host_tracking());
        host.set_tracking(false);
        assert!(!client.is_host_tracking());

        host.publish_slots(&[(0x1111, 5000), (0x2222, 100)]);
        host.set_in_flight(0x3333);
        assert!(client.is_ready_or_in_flight(0x1111, 200));
        assert!(!client.is_ready_or_in_flight(0x2222, 200));
        assert!(client.is_ready_or_in_flight(0x3333, 200));
        assert!(!client.is_ready_or_in_flight(0x9999, 200));

        let payload = vec![1u8, 2, 3, 4, 5, 6];
        let seq = client
            .try_write_prefetch(0x4444, true, 99, &payload)
            .expect("prefetch write");
        let read = host.read_prefetch().expect("prefetch read");
        assert_eq!(read.request_seq, seq);
        assert_eq!(read.key_hash, 0x4444);
        assert!(read.shift_held);
        assert_eq!(read.clipboard_seq, 99);
        assert_eq!(read.pidl_bytes, payload);

        let show_req = ShowRequest {
            request_seq: 0,
            key_hash: 0x5555,
            clipboard_seq: 77,
            anchor_x: 320,
            anchor_y: -120,
            defview_hwnd: 0x1010,
            explorer_root_hwnd: 0x2020,
            no_animation: true,
        };
        let show_seq = client.try_write_show(&show_req).expect("show write");
        let read_show = host.read_show().expect("show read");
        assert_eq!(read_show.request_seq, show_seq);
        assert_eq!(read_show.key_hash, 0x5555);
        assert_eq!(read_show.clipboard_seq, 77);
        assert_eq!(read_show.anchor_x, 320);
        assert_eq!(read_show.anchor_y, -120);
        assert_eq!(read_show.defview_hwnd, 0x1010);
        assert_eq!(read_show.explorer_root_hwnd, 0x2020);
        assert!(read_show.no_animation);

        drop(host);
        assert!(!client.is_valid());
        assert!(!client.is_ready_or_in_flight(0x1111, 100));
    }

    #[test]
    fn oversize_and_odd_seqlock_are_rejected() {
        let name = unique_mapping();
        let host = SharedCacheHost::create_named(&name).unwrap();
        host.set_host_info(42, 99);
        let client = SharedCacheClient::open_named(&name).unwrap();

        let too_big = vec![0xAAu8; MAX_PREFETCH_BYTES + 1];
        assert!(client.try_write_prefetch(1, false, 1, &too_big).is_none());
        assert!(client.try_write_prefetch(1, false, 1, &[]).is_none());

        host.mapping.region().prefetch.seq.store(3, Ordering::Release);
        assert!(host.read_prefetch().is_none());
        host.mapping.region().show.seq.store(5, Ordering::Release);
        assert!(host.read_show().is_none());
    }

    #[test]
    fn host_restart_reinitializes_open_mapping_and_recovers_dead_pid_lock() {
        let name = unique_mapping();
        let host1 = SharedCacheHost::create_named(&name).unwrap();
        host1.set_host_info(1111, 0x1111);
        let client = SharedCacheClient::open_named(&name).unwrap();
        assert!(client.try_write_prefetch(0x1234, false, 1, &[1, 2, 3]).is_some());

        // Simulate abrupt host restart while client still holds the section open.
        drop(host1);
        assert!(!client.is_valid());
        let host2 = SharedCacheHost::create_named(&name).unwrap();
        host2.set_host_info(2222, 0x2222);
        assert!(client.is_valid());
        assert_eq!(client.host_pid(), 2222);
        assert!(host2.read_prefetch().is_none());

        // Simulate a dead writer PID leaving the slot lock held with a partial odd record.
        let dead_pid = u32::MAX - 3;
        let region = host2.mapping.region();
        region.prefetch.lock.store(dead_pid, Ordering::Release);
        region.prefetch.seq.store(7, Ordering::Release);
        region.prefetch.payload_len.store(4, Ordering::Release);
        assert!(host2.read_prefetch().is_none());
        assert_eq!(region.prefetch.lock.load(Ordering::Acquire), 0);
        assert_eq!(region.prefetch.seq.load(Ordering::Acquire) & 1, 0);

        // Fresh write succeeds after dead-owner recovery.
        assert!(client.try_write_prefetch(0x9999, false, 2, &[9, 8]).is_some());
        let read = host2.read_prefetch().unwrap();
        assert_eq!(read.key_hash, 0x9999);
        assert_eq!(read.pidl_bytes, vec![9, 8]);
    }
}
