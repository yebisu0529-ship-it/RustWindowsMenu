//! Heap-allocation and latency microbenchmark harness modeled on `RustWTM/crates/wtm-sys/tests/tick_cost.rs`.
//! Run full timing breakdown with:
//! `cargo test -p rwm-core --release --test hotpath_cost -- --ignored --nocapture`

#![cfg(windows)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::hash::BuildHasher;
use std::path::Path;
use std::time::Instant;

use rwm_core::fasthash::FastBuildHasher;
use rwm_core::ipc_cache::{now_tick_ms, SharedCacheClient, SharedCacheHost};
use rwm_core::shell::{clipboard_sequence_number, ComStaGuard, SelectionKey};
use rwm_core::warm_pool::WarmDllPin;

struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn measure<T>(what: &str, rounds: u32, mut f: impl FnMut() -> T) -> (f64, f64) {
    drop(f());
    let before = ALLOCATIONS.with(Cell::get);
    let start = Instant::now();
    for _ in 0..rounds {
        drop(std::hint::black_box(f()));
    }
    let micros = start.elapsed().as_secs_f64() * 1e6 / f64::from(rounds);
    let allocs = (ALLOCATIONS.with(Cell::get) - before) as f64 / f64::from(rounds);
    println!("{what:<32} {micros:>10.4} us {allocs:>10.2} allocs");
    (micros, allocs)
}

fn setup_fixtures() -> (SelectionKey, SelectionKey, SharedCacheHost, SharedCacheClient) {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let manifest = repo.join("Cargo.toml");
    let key_a = SelectionKey::from_paths(std::slice::from_ref(&manifest), false, false).unwrap();
    let key_b = SelectionKey::from_paths(&[manifest], false, false).unwrap();
    let name = format!(r"Local\RwmHotpathBench_{}", std::process::id());
    let host = SharedCacheHost::create_named(&name).unwrap();
    host.set_host_info(std::process::id(), 0x1234);
    let clip = clipboard_sequence_number();
    let hash = key_a.cache_key_hash(clip);
    host.publish_slots(&[(hash, now_tick_ms() + 60_000)]);
    let client = SharedCacheClient::open_named(&name).unwrap();
    (key_a, key_b, host, client)
}

#[test]
fn hotpath_operations_make_zero_heap_allocations() {
    let _sta = ComStaGuard::initialize();
    let (key_a, key_b, _host, client) = setup_fixtures();
    let clip = clipboard_sequence_number();
    let hash = key_a.cache_key_hash(clip);
    let now = now_tick_ms();
    let saturated_pin = WarmDllPin::new(0);
    let open_pin = WarmDllPin::new(16);

    let (_, allocs_match) = measure("SelectionKey::matches", 200, || key_a.matches(&key_b));
    let (_, allocs_hash) = measure("SelectionKey::cache_key_hash", 200, || {
        key_a.cache_key_hash(clip)
    });
    let (_, allocs_shm) = measure("SharedCacheClient::ready", 200, || {
        client.is_ready_or_in_flight(hash, now)
    });
    let (_, allocs_skip) = measure("WarmDllPin::should_snapshot", 200, || {
        saturated_pin.should_snapshot()
    });
    let (_, allocs_probe) = measure("WarmDllPin::probe_bytes", 50, || {
        open_pin.probe_module_bytes()
    });

    assert_eq!(allocs_match, 0.0);
    assert_eq!(allocs_hash, 0.0);
    assert_eq!(allocs_shm, 0.0);
    assert_eq!(allocs_skip, 0.0);
    assert_eq!(allocs_probe, 0.0);
}

#[test]
#[ignore = "timing harness"]
fn hotpath_timings() {
    let _sta = ComStaGuard::initialize();
    let (key_a, key_b, _host, client) = setup_fixtures();
    let clip = clipboard_sequence_number();
    let hash = key_a.cache_key_hash(clip);
    let now = now_tick_ms();
    let open_pin = WarmDllPin::new(16);

    measure("SelectionKey::matches (fast_eq)", 50_000, || {
        key_a.matches(&key_b)
    });
    measure("SelectionKey::cache_key_hash", 50_000, || {
        key_a.cache_key_hash(clip)
    });
    measure("SharedCacheClient::is_ready", 100_000, || {
        client.is_ready_or_in_flight(hash, now)
    });
    measure("WarmDllPin::probe_module_bytes", 2_000, || {
        open_pin.probe_module_bytes()
    });

    fn bench_map<S: BuildHasher + Default>() -> f64 {
        let mut map: HashMap<u64, u64, S> = HashMap::default();
        for i in 0..2048u64 {
            map.insert(0xffff_a000_0000_0000u64 + i * 16, i);
        }
        let start = Instant::now();
        let mut sum = 0u64;
        for i in 0..1_000_000u64 {
            let k = 0xffff_a000_0000_0000u64 + ((i * 7919) % 2048) * 16;
            sum = sum.wrapping_add(map[&k]);
        }
        std::hint::black_box(sum);
        start.elapsed().as_secs_f64() * 1e9 / 1_000_000.0
    }
    let sip_ns = bench_map::<std::collections::hash_map::RandomState>();
    let fast_ns = bench_map::<FastBuildHasher>();
    println!("map lookup: SipHash {sip_ns:.2} ns/op, FastHasher {fast_ns:.2} ns/op");
    assert!(fast_ns < sip_ns);
}
