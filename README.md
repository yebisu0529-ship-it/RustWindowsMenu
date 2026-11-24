# RustWindowsMenu (`rwm`) v0.4.0

[English](#english) | [日本語](#japanese)

![RustWindowsMenu v0.3.0 Side-by-Side Comparison](docs/demo.gif)

High-res 60fps MP4: [docs/demo.mp4](docs/demo.mp4)

> Side-by-side capture on a real file (`Cargo.toml`, 30 OS/MSIX/COM menu items, 100 ms right-button hold): Stock Windows renders its first menu frame in `176.274 ms` (`query_ms = 142.388 ms`), while `rwm` v0.3.0 with speculative off-thread cache presents all 30 items in `19.092 ms` after release (`9.2x` faster perceived response).

---

<a id="english"></a>

## Overview (English)

**RustWindowsMenu (`rwm`)** eliminates perceived Windows 11 File Explorer context menu lag while keeping 100% of your native Windows, MSIX (`IExplorerCommand`), and classic COM shell extension items intact.

Inspired by low-overhead Rust desktop utilities such as [`RustWTM`](https://github.com/RustWTM/RustWTM), **v0.3.0** moves context menu querying off Explorer's UI path into a dedicated background STA worker inside `rwm-host.exe` and communicates over lock-free shared memory (`ABI v3`).

### Verified v0.3.0 Benchmarks

All numbers below are measured on Windows 11 with 100% of native, MSIX, and third-party COM items active.

#### End-to-End Capture (`demo_capture`, `Cargo.toml`, 100 ms hold, 30 items)

| Mode | Query Time | First Menu Frame After Release | Items | Speedup |
|---|---:|---:|---:|---:|
| Stock Windows (`IContextMenu`) | `142.388 ms` | `176.274 ms` | 30 | 1.0x |
| **RWM v0.3.0 (`--cached`)** | `438.753 ms` (pre-built off UI path) | **`19.092 ms`** | **30** | **`9.2x`** |

#### CLI Benchmark (`rwm bench --runs 5`)

| Target | Cold Root | Warm Root (p50 / p95) | Warm Query (p50 / p95) | Eager Submenu | 100 ms Hold `t=0 ms` (p50 / p95) |
|---|---:|---:|---:|---:|---:|
| File (`Cargo.toml`, 12 MSIX) | `471.853 ms` | `150.736 / 154.707 ms` | `149.594 / 153.645 ms` | `33.853 ms` | `50.736 / 54.707 ms` |
| Folder (`crates`, 12 MSIX) | `424.078 ms` | `138.249 / 227.754 ms` | `137.033 / 226.723 ms` | `5.555 ms` | `38.249 / 127.754 ms` |
| Background (`--background`, 2 MSIX) | `232.517 ms` | `24.425 / 61.109 ms` | `23.621 / 59.359 ms` | `165.002 ms` | **`0.000 / 0.000 ms`** |

On a warm speculative cache hit, shell query wait drops to **`0.000 ms`** (`IPC handoff 0.0000 ms`).

#### Zero-Allocation Hotpath Audit (`hotpath_cost --release`)

Verified with a custom `GlobalAlloc` counter harness in [hotpath_cost.rs](crates/rwm-core/tests/hotpath_cost.rs):

| Operation | Latency | Heap Allocations |
|---|---:|---:|
| `SelectionKey::matches (fast_eq)` | `0.0263 us` | `0.00 allocs` |
| `SelectionKey::cache_key_hash` | `0.0797 us` | `0.00 allocs` |
| `SharedCacheClient::is_ready` | `0.0010 us` | `0.00 allocs` |
| `WarmDllPin::probe_module_bytes` | `13.8445 us` | `0.00 allocs` |
| Map lookup (`FastHasher` vs `SipHash`) | `1.50 ns/op` vs `6.15 ns/op` | `0.00 allocs` |

### What's New in v0.4.0

1. **Right-Click Launcher + Native Context Menu Manager GUI (`rwm ui` / `rwm-host.exe --ui`)**:
   - Click **"Customize Menu (RWM)..."** / **"メニューをカスタマイズ (RWM)..."** at the bottom of any File Explorer right-click menu, click the `rwm-host.exe` tray icon, or run `rwm ui [PATH] [--background]`.
   - **Tab 1 (Menu Items)**: Check/uncheck individual top-level commands or submenus (`verb:...` / `label:...` stored in `hidden_items` in `config.toml`), search/filter live, prune orphan separators cleanly, test via `Preview Menu`, and notify the cached STA worker immediately (`RustWindowsMenu.ConfigReload.v3`).
   - **Tab 2 (Shell Extensions)**: Shows measured query latency (ms) per handler, highlights slow third-party handlers (`>= 25 ms`), warns before touching Microsoft system DLLs, and lets you toggle or batch-disable slow handlers in one click.
   - **Tab 3 (Settings)**: Toggle RWM feature flags, inspect host/ABI status, or open/reset `config.toml`.
   - Automatic Windows 11 Dark/Light theme styling (`DWMWA_USE_IMMERSIVE_DARK_MODE`), Per-Monitor DPI v2, and single-instance focus switching.
2. **System Tray Icon (`rwm-host.exe`)**:
   - Left-click opens the Context Menu Manager directly; right-click offers Pause/Resume, Open Manager, or Exit Host.

### What Changed in v0.3.0 (Included in v0.4.0)

1. **Off-UI-Thread Speculative Cache (`rwm-host.exe` Menu STA Thread)**:
   - Dedicated STA worker thread with `SetThreadDpiAwarenessContext`, dark-mode menu support, hidden `WS_POPUP | WS_EX_TOOLWINDOW` popup owner, and an LRU + TTL + clipboard-sequence cache (`CacheCore`).
   - Renders cached menus directly from the background host with zero shell query wait on hit, preserves inline rename (`Protocol::EditLabel`), restores foreground focus cleanly, and calls `OleFlushClipboard()` on shutdown.
2. **Lock-Free Shared Memory IPC (`ABI v3`)**:
   - `Local\RustWindowsMenu_Cache_v3` (`CACHE_MAGIC = 0x5257_4D33`) with seqlock `PrefetchSlot` (16 KB), `ShowSlot`, and an 8-entry `ReadySlot` table for zero-allocation ready checks (`0.0010 us`).
3. **Hover, Selection, and `t=0 ms` Speculative Prefetch (`rwm-hook.dll`)**:
   - Speculative prefetch triggers on hover dwell (`120 ms` default via `LVM_HITTEST` / `IFolderView` / `AccessibleObjectFromPoint`), selection change (`80 ms` coalesced timer), and `WM_RBUTTONDOWN` (`t=0 ms`).
   - Rejects virtual, UNC, and `DRIVE_REMOTE` paths from speculative prefetch so network shares are never woken unexpectedly.
   - Falls back cleanly to Explorer's native menu on any ambiguity.
4. **Zero-Allocation Hotpaths & Release Optimization**:
   - Custom 128-bit folded-multiply `FastHasher` / `FastMap` / `FastSet` (`1.50 ns/op`), reusable buffer workspaces in `WarmDllPin`, and fat LTO (`lto = "fat"`, `codegen-units = 1`, `opt-level = 3`, `strip = true`).
5. **Slow Shell Extension Optimizer (`rwm optimize`)**:
   - Measures real query latency per handler, skips Microsoft system DLLs (`!is_system_module`), and reports third-party bottlenecks in dry-run mode first. Use `--apply` only if you want to write HKCU `Shell Extensions\Blocked` entries.
   - Example dry-run on `Cargo.toml`: identified `DaemonShellExtImageLite Class` (`79.017 ms`, `dtshl64.dll`), saving an estimated `79.017 ms` (`143.906 ms` -> `64.889 ms`).
6. **Desktop-Shell Decoupled Host Launch & Polite Explorer Restart**:
   - `launch_via_desktop_shell` starts `rwm-host.exe` via `IShellWindows::FindWindowSW(SWC_DESKTOP)` -> `IShellDispatch2::ShellExecute` so the tray host stays resident outside CLI job objects.
   - `rwm classic enable/disable --restart` messages `Shell_TrayWnd` (`WM_USER + 436`) politely before falling back to process termination.

### Quick Start

Build from source or install locally:

```powershell
cargo build --release --workspace
.\target\release\rwm.exe install
```

Check runtime health and benchmark your current environment:

```powershell
rwm status
rwm doctor
rwm bench C:\Windows\notepad.exe --runs 5
rwm optimize C:\Windows\notepad.exe
```

Optional: enable classic Windows 10 right-click menus globally (off by default during `rwm install` unless `--classic` is passed):

```powershell
rwm classic enable --restart
```

### CLI Reference

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

### Configuration (`config.toml`)

See [config.example.toml](config.example.toml):

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

<a id="japanese"></a>

## 概要 (日本語)

**RustWindowsMenu (`rwm`)** は、Windows 標準メニュー、MSIX (`IExplorerCommand`)、従来の COM シェル拡張項目をすべて保持したまま、Windows 11 エクスプローラーの右クリックメニュー体感遅延を劇的に削減するツールです。

[`RustWTM`](https://github.com/RustWTM/RustWTM) のような超低オーバーヘッドな Rust 製ユーティリティの設計思想を取り入れ、**v0.3.0** ではメニュー構築をエクスプローラーの UI スレッドから `rwm-host.exe` の専用バックグラウンド STA スレッドへ逃がし、ロックフリー共有メモリ (`ABI v3`) で連携します。

### 実測ベンチマーク (v0.3.0)

OS 標準項目、MSIX パッケージ動詞 (12 個)、サードパーティ COM シェル拡張を 100% 有効にした状態の実環境計測です。

#### エンドツーエンド画面収録 (`demo_capture`, `Cargo.toml`, 100 ms 長押し, 30 項目)

| モード | クエリ時間 | ボタン離上後の初回メニュー表示 | 表示項目数 | 高速化倍率 |
|---|---:|---:|---:|---:|
| Windows 標準 (`IContextMenu`) | `142.388 ms` | `176.274 ms` | 30 | 1.0x |
| **RWM v0.3.0 (`--cached`)** | `438.753 ms` (離上前にバックグラウンド構築) | **`19.092 ms`** | **30** | **`9.2x`** |

#### CLI ベンチマーク (`rwm bench --runs 5`)

| 対象 | Cold Root | Warm Root (p50 / p95) | Warm Query (p50 / p95) | Eager Submenu | 100 ms 保持時 `t=0 ms` (p50 / p95) |
|---|---:|---:|---:|---:|---:|
| ファイル (`Cargo.toml`, 12 MSIX) | `471.853 ms` | `150.736 / 154.707 ms` | `149.594 / 153.645 ms` | `33.853 ms` | `50.736 / 54.707 ms` |
| フォルダ (`crates`, 12 MSIX) | `424.078 ms` | `138.249 / 227.754 ms` | `137.033 / 226.723 ms` | `5.555 ms` | `38.249 / 127.754 ms` |
| フォルダ背景 (`--background`, 2 MSIX) | `232.517 ms` | `24.425 / 61.109 ms` | `23.621 / 59.359 ms` | `165.002 ms` | **`0.000 / 0.000 ms`** |

投機キャッシュヒット時、右クリック時のシェルクエリ待機時間は **`0.000 ms`** (`IPC handoff 0.0000 ms`) です。

#### ゼロアロケーション Hotpath 計測 (`hotpath_cost --release`)

| 処理 | 所要時間 | ヒープ確保 |
|---|---:|---:|
| `SelectionKey::matches (fast_eq)` | `0.0263 us` | `0.00 allocs` |
| `SelectionKey::cache_key_hash` | `0.0797 us` | `0.00 allocs` |
| `SharedCacheClient::is_ready` | `0.0010 us` | `0.00 allocs` |
| `WarmDllPin::probe_module_bytes` | `13.8445 us` | `0.00 allocs` |
| Map 参照 (`FastHasher` vs `SipHash`) | `1.50 ns/op` vs `6.15 ns/op` | `0.00 allocs` |

### v0.4.0 の新機能 (右クリック管理 GUI・項目フィルタ・トレイアイコン)

1. **右クリックから直接開ける管理画面 (`rwm ui` / `rwm-host.exe --ui`)**:
   - ファイルやフォルダの右クリックメニュー最下部の **「メニューをカスタマイズ (RWM)...」**、`rwm-host.exe` のタスクトレイアイコン、または `rwm ui [PATH] [--background]` から開けます。
   - **タブ 1 (表示するメニュー項目)**: コマンド・サブメニュー単位でチェックボックスから表示/非表示を切り替え (`config.toml` の `hidden_items`)、残った不要な区切り線を自動整理し、投機キャッシュへ即時反映します。
   - **タブ 2 (シェル拡張・高速化)**: ハンドラごとの実測クエリ時間 (ms) を表示し、遅延の原因となるサードパーティ拡張を 1 クリックまたは一括で無効化/再有効化できます (Microsoft 標準 DLL は確認ダイアログ付き)。
   - **タブ 3 (動作設定)**: RWM の各種設定トグル、`rwm-host` 稼働状態の確認、`config.toml` の表示・初期化を行えます。
   - Windows 11 ダーク/ライトテーマ自動追従、Per-Monitor DPI v2、単一インスタンス対応。
2. **タスクトレイアイコン (`rwm-host.exe`)**:
   - 左クリックで管理画面をすぐに開き、右クリックから一時停止/再開・終了を操作できます。

### v0.3.0 からの高速化基盤 (v0.4.0 に内蔵)

1. **専用 Menu STA スレッドによるオフスレッド投機キャッシュ (`rwm-host.exe`)**:
   - `SetThreadDpiAwarenessContext`、ダークモードメニュー対応、非表示 `WS_POPUP | WS_EX_TOOLWINDOW` オーナー、LRU + TTL + クリップボード変更検知キャッシュ (`CacheCore`) を独立 STA スレッドに実装。
   - キャッシュヒット時は即座にネイティブメニューを表示し、名前の変更 (`Protocol::EditLabel`) やフォーカス復帰、`OleFlushClipboard()` も安全に処理します。
2. **ロックフリー共有メモリ IPC (`ABI v3`)**:
   - `Local\RustWindowsMenu_Cache_v3` (`CACHE_MAGIC = 0x5257_4D33`) 上に Seqlock 付きの `PrefetchSlot` (16 KB)、`ShowSlot`、8 スロットの `ReadySlot` を配置し、`0.0010 us` / `0 alloc` でヒット判定を行います。
3. **ホバー・選択変更・`t=0 ms` 右クリック事前構築 (`rwm-hook.dll`)**:
   - ホバー静止 (`120 ms` 既定)、選択変更 (`80 ms` コアレス)、右ボタン押下 (`t=0 ms`) で自動プリフェッチを実行。仮想パス・UNC・`DRIVE_REMOTE` は事前アクセス対象外としてネットワーク共有の不要なウェイクを防止し、不確実な状況ではエクスプローラー標準メニューへフォールバックします。
4. **ゼロアロケーション Hotpath と Release 最適化**:
   - 128-bit 乗算ハッシュ `FastHasher` (`1.50 ns/op`)、`WarmDllPin` のバッファ再利用、`lto = "fat"` + `codegen-units = 1` を採用。
5. **遅延拡張診断コマンド `rwm optimize`**:
   - 実クエリ遅延をハンドラ単位で計測し、Microsoft 製システム DLL を除外して遅いサードパーティ拡張を抽出します (既定は dry-run、適用時のみ `--apply`)。
   - 例 (`Cargo.toml` dry-run): `DaemonShellExtImageLite Class` (`79.017 ms`, `dtshl64.dll`) を特定し、推定短縮 `~79.017 ms` (`143.906 ms` -> `64.889 ms`) を算出。
6. **デスクトップシェル経由の常駐ホスト起動**:
   - `IShellWindows::FindWindowSW(SWC_DESKTOP)` -> `IShellDispatch2::ShellExecute` 経由で `rwm-host.exe` を起動し、CLI プロセスの Job Object 終了後も常駐を維持します。

### 基本的な使い方

```powershell
cargo build --release --workspace
.\target\release\rwm.exe install

rwm status
rwm doctor
rwm bench M:\Tools\RustWindowsMenu\Cargo.toml --runs 5
rwm optimize M:\Tools\RustWindowsMenu\Cargo.toml
```

既定の `rwm install` はレジストリの Classic Menu 切替を行いません (`--no-classic` 相当)。Windows 10 スタイルのクラシックメニューを有効にする場合のみ `--classic` または `rwm classic enable --restart` を使用してください。

## Safety & Scope

- Hooks Explorer folder views and desktop (`CDefView`) in-process without modifying system binaries.
- Never restarts `explorer.exe` automatically during `install` or `uninstall`.
- Verifies an ownership manifest (`install-manifest.json`) and rejects reparse points before uninstall cleanup.
- Falls back immediately to Explorer's stock context menu if any state or IPC precondition does not match.
