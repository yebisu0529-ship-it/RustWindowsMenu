# RustWindowsMenu (`rwm`) v0.4.1

**English** | [日本語](README.ja.md)

![RustWindowsMenu v0.4.1 0.25x Slow-Mo Comparison](docs/demo.gif)

High-resolution 60 fps MP4: [docs/demo.mp4](docs/demo.mp4)

> `0.25x` slow-motion capture on `Cargo.toml` (27 native OS, MSIX, and COM items; 100 ms right-button hold; `3 -> 2 -> 1 -> CLICK!` countdown): on a cold first click, stock Windows renders its first menu frame in **`389.534 ms`** (`query_ms = 346.143 ms`), whereas `rwm` v0.4.1 with speculative off-thread caching presents all 27 items in **`25.799 ms`** (**`15.1x` faster**; **`3.4x` faster** than warm steady-state Windows at `86.786 ms`).

---

## Why RustWindowsMenu?

Right-clicking a file in Windows 11 File Explorer builds the context menu synchronously on the UI thread, often stalling while COM handlers and MSIX extensions initialize. **RustWindowsMenu (`rwm`)** moves menu construction off Explorer's UI path without stripping out your extensions.

- **100% Native Menu Fidelity:** Keeps every native Windows command, MSIX `IExplorerCommand`, and classic COM shell extension item intact.
- **Off-UI-Thread Speculative Cache (`ABI v3`):** Prebuilds menus on a background STA worker thread (`rwm-host.exe`) during hover dwell, selection changes, or `t=0 ms` right-button press, passing control over lock-free shared memory.
- **Built-in Context Menu Manager GUI (`rwm ui`):** Show or hide individual menu entries, inspect per-handler latency, and disable slow third-party shell extensions directly from a right-click or system tray icon.
- **Non-Destructive & Reversible:** Hooks only Explorer folder views and desktop (`SHELLDLL_DefView`) in-process, never restarts `explorer.exe` automatically, and restores cleanly with `uninstall.bat` or `rwm uninstall`.

---

## Quick Install

### Option A: Prebuilt Release ZIP (Recommended)

1. Download `RustWindowsMenu-v0.4.1-x86_64-pc-windows-msvc.zip` from [GitHub Releases](https://github.com/yebisu0529-ship-it/RustWindowsMenu/releases/latest) and verify its SHA-256 checksum against `SHA256SUMS.txt`.
2. Extract the archive and run **`install.bat`** (or `powershell -ExecutionPolicy Bypass -File .\install.ps1`).
   - Your existing `%APPDATA%\RustWindowsMenu\config.toml` (including `hidden_items`) is preserved on upgrade.
   - To uninstall later, run **`uninstall.bat`** (or `rwm uninstall`).

### Option B: Build from Source

Requires the Rust toolchain (`MSVC` target) on Windows 11:

```powershell
cargo build --release --workspace
.\target\release\rwm.exe install
```

Check runtime health and measure your own machine:

```powershell
rwm status
rwm doctor
rwm bench C:\Windows\notepad.exe --runs 5
rwm optimize C:\Windows\notepad.exe
```

Optional: By default, `rwm install` leaves Windows 11's context menu registry setting untouched. To switch Explorer globally to Windows 10 classic menus, pass `--classic` or run:

```powershell
rwm classic enable --restart
```

---

## Key Features

### 1. Native Context Menu Manager GUI (`rwm ui` / `rwm-host.exe --ui`)

Open the manager by clicking **Customize Menu (RWM)...** / **メニューをカスタマイズ (RWM)...** at the bottom of any File Explorer context menu, clicking the `rwm-host.exe` tray icon, or running `rwm ui [PATH] [--background]`:

- **Menu Items Tab:** Toggle individual top-level commands or submenus (`verb:...` / `label:...` saved to `hidden_items` in `config.toml`), search and filter live, preview changes, and apply them immediately with automatic orphan-separator pruning and live cache reload (`RustWindowsMenu.ConfigReload.v3`).
- **Shell Extensions Tab:** Inspect measured query latency (`ms`) per handler with slow third-party handlers (`>= 25 ms`) highlighted, enable or disable handlers individually or in batches, and receive a confirmation prompt before changing built-in Windows handlers.
- **Settings Tab:** Toggle runtime options, inspect host/ABI health, or open and reset `config.toml`.
- Supports Windows 11 Dark/Light mode (`DWMWA_USE_IMMERSIVE_DARK_MODE`), Per-Monitor DPI v2, single-instance focus switching, and a system tray menu (Left-click: open Manager; Right-click: Open Menu Manager, Enable/Disable Context Menu Acceleration, Exit RWM Host).

### 2. Off-UI-Thread Speculative Cache (`rwm-host.exe` + `rwm_hook.dll`)

Inspired by low-overhead Rust desktop utilities such as [`RustWTM`](https://github.com/RustWTM/RustWTM), menu enumeration runs outside Explorer's UI thread:

- **Shared-Memory IPC (`ABI v3`):** Uses `Local\RustWindowsMenu_Cache_v3` (`CACHE_MAGIC = 0x5257_4D33`) with PID-tagged slot locks, dead-owner recovery, seqlock `PrefetchSlot` (16 KB), `ShowSlot`, and an 8-entry `ReadySlot` table (`0.0010 us` / `0 alloc` check).
- **Multi-Signal Prefetch:** Triggers on hover dwell (`120 ms` default via `LVM_HITTEST` / `IFolderView` / `AccessibleObjectFromPoint`), selection change (`80 ms` coalesced), and `WM_RBUTTONDOWN` (`t=0 ms`).
- **Local-Filesystem Safety:** Skips virtual items, UNC paths, and `DRIVE_REMOTE` shares so network drives are never woken up unexpectedly, and falls back to Explorer's native menu on any ambiguity.
- **No Stuck or Duplicate Menus:** Uses a transparent `1x1` layered popup owner (`WS_EX_LAYERED | WS_EX_TOOLWINDOW`, `LWA_ALPHA = 0`) with foreground activation, focus-loss watchdog, scoped non-foreground fallback hooks, and cross-process cancellation (`RustWindowsMenu.CancelMenu.v3` + `host_tracking` bits).

### 3. Slow Shell Extension Optimizer (`rwm optimize`)

Identifies real COM/shell bottlenecks per handler while excluding Microsoft system DLLs (`!is_system_module`). Runs in dry-run mode by default; pass `--apply` only when you want to write HKCU `Shell Extensions\Blocked` entries.

---

## Verified v0.4.1 Benchmarks

All numbers below are measured on Windows 11 with 100% of native, MSIX, and active third-party COM items enabled.

### End-to-End Screen Capture (`demo_capture`, `Cargo.toml`, 100 ms hold, 27 items)

| Mode | Query Time | First Menu Frame After Release | Items | Speedup |
|---|---:|---:|---:|---:|
| Stock Windows (Warm, 2nd+ click) | `59.754 ms` | `86.786 ms` | 27 | `1.0x` |
| Stock Windows (Cold, 1st click / DLL load) | `346.143 ms` | `389.534 ms` | 27 | `1.0x` |
| **RWM v0.4.1 (`--cached`, hover prefetch)** | `362.166 ms` (prebuilt off UI path) | **`25.799 ms`** | **27** | **`3.4x` Warm / `15.1x` Cold** |

On an unhovered click (`--mode rwm`), `demo_capture` finished its background query in `55.316 ms` during the `100 ms` button hold.

### CLI Benchmark (`rwm bench --runs 5`, v0.3.0 baseline)

| Target | Cold Root | Warm Root (p50 / p95) | Warm Query (p50 / p95) | Eager Submenu | 100 ms Hold `t=0 ms` (p50 / p95) |
|---|---:|---:|---:|---:|---:|
| File (`Cargo.toml`, 12 MSIX) | `471.853 ms` | `150.736 / 154.707 ms` | `149.594 / 153.645 ms` | `33.853 ms` | `50.736 / 54.707 ms` |
| Folder (`crates`, 12 MSIX) | `424.078 ms` | `138.249 / 227.754 ms` | `137.033 / 226.723 ms` | `5.555 ms` | `38.249 / 127.754 ms` |
| Background (`--background`, 2 MSIX) | `232.517 ms` | `24.425 / 61.109 ms` | `23.621 / 59.359 ms` | `165.002 ms` | **`0.000 / 0.000 ms`** |

On a warm speculative cache hit, shell query wait drops to **`0.000 ms`** (`IPC handoff 0.0000 ms`).

### Zero-Allocation Hotpath Audit (`hotpath_cost --release`)

Verified with a custom `GlobalAlloc` counter harness in [crates/rwm-core/tests/hotpath_cost.rs](crates/rwm-core/tests/hotpath_cost.rs):

| Operation | Latency | Heap Allocations |
|---|---:|---:|
| `SelectionKey::matches (fast_eq)` | `0.0263 us` | `0.00 allocs` |
| `SelectionKey::cache_key_hash` | `0.0797 us` | `0.00 allocs` |
| `SharedCacheClient::is_ready` | `0.0010 us` | `0.00 allocs` |
| `WarmDllPin::probe_module_bytes` | `13.8445 us` | `0.00 allocs` |
| Map lookup (`FastHasher` vs `SipHash`) | `1.50 ns/op` vs `6.15 ns/op` | `0.00 allocs` |

---

## Release Highlights

- **v0.4.1 (Popup Focus, Single-Menu Fix, IPC & Installer Hardening):** Fixed stuck and duplicate context menus via the `1x1` transparent layered owner, foreground watchdog, scoped non-foreground fallback hooks, and `CancelMenu.v3`. Switched IPC slot locks to PID-tagged ownership with dead-process recovery and safe host-restart reuse, added `ClaimShow` sequence validation and target-verified inline Rename (`Protocol::EditLabel`), protected `config.toml` against silent overwrites on syntax errors, closed open `--ui` windows during upgrade/uninstall, added v0.1 config backup migration (`config.v01.bak.toml`), and enforced strict `current.json` + ancestor reparse-point checks before uninstall deletion.
- **v0.4.0 (Context Menu Manager GUI & Tray Icon):** Added right-click **Customize Menu (RWM)...**, `rwm ui`, 3-tab dark/light native Win32 GUI, orphan separator pruning, handler latency profiling/blocking, and the `rwm-host.exe` system tray icon.
- **v0.3.0 (Speculative Off-Thread Cache & Zero-Alloc Hotpaths):** Added dedicated `rwm-host.exe` Menu STA thread with `LRU + TTL + clipboard-sequence` caching (`CacheCore`), lock-free shared-memory `ABI v3`, hover/selection/`t=0 ms` prefetch, `FastHasher`, `WarmDllPin`, desktop-shell host launch (`IShellDispatch2::ShellExecute`), `rwm optimize`, and polite `Shell_TrayWnd` restart handling.

---

## CLI Reference

```text
rwm install [--classic] [--no-startup] [--no-path] [--no-start]
rwm uninstall [--purge]
rwm start | stop | run
rwm status [--json]
rwm doctor [PATH] [--background] [--json]
rwm inspect [PATH] [--background] [--extended] [--json]
rwm bench [PATH] [--background] [--runs N] [--json]
rwm optimize [PATH] [--background] [--threshold-ms MS] [--apply] [--json]
rwm preview [PATH] [--background] [--extended] [--no-animation] [--raw]
rwm ui [PATH] [--background] [--wait]
rwm handlers list [--json] | block <CLSID> [--name LABEL] | unblock <CLSID> [--force] | reset
rwm classic status [--json] | enable [--restart] | disable [--force] [--restart]
rwm config path | show | init [--force] | reset
```

---

## Configuration (`config.toml`)

User configuration is stored at `%APPDATA%\RustWindowsMenu\config.toml`. See [config.example.toml](config.example.toml) for inline descriptions of every key:

```toml
enabled = true
prebuild_on_rbuttondown = true
arm_delay_ms = 15
no_animation = true
pin_handler_dlls = true
max_pinned_dlls = 32
classic_menu_fallback = false
debug_logging = false
hover_prebuild = true
hover_delay_ms = 120
selection_prebuild = true
host_menu_cache = true
host_cache_entries = 6
host_cache_ttl_ms = 8000
show_customize_entry = true
hidden_items = []
```

---

## Safety & Scope

- Hooks Explorer folder views and desktop (`SHELLDLL_DefView`) in-process without modifying system binaries.
- Never restarts `explorer.exe` automatically during `install` or `uninstall`.
- **Emergency kill switch:** create an empty file at `%LOCALAPPDATA%\RustWindowsMenu\DISABLE_HOOK` to bypass the hook immediately without uninstalling.
- Validates the installation ownership manifest (`current.json`) and rejects reparse points along ancestor paths before uninstall cleanup.
- Falls back cleanly to Explorer's stock context menu if any state or IPC precondition does not match.

## License

Licensed under the [MIT License](LICENSE).
