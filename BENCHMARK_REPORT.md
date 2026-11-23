# Project Loom — F25 Windows Multi-Agent Performance SLA Benchmark Report

Filesystem: NTFS (ReFS CoW / ProjFS unavailable on this Windows host; WindowsNtfsFallback active)

- **Generated (UTC)**: `2026-11-23T01:32:38Z`
- **Loom Binary**: `C:\Users\ijh69\Workspaces\loom\target\release\loom.exe` (`release` profile)
- **Active VFS Driver**: `WindowsNtfsFallback`

## 1. Environment & Toolchain Baseline

| Component | Measured Value |
|---|---|
| OS | `Microsoft Windows [Version 10.0.26200.8875]` |
| CPU | `AMD Ryzen 5 7500F 6-Core Processor` |
| Physical RAM | `31.69 GiB` |
| Rust Toolchain | `rustc 1.97.1 (8bab26f4f 2026-07-14)` |
| Git | `git version 2.43.0.windows.1` |
| Fixture Source Payload | `35 files (7.55 KiB)` across Rust, TS/TSX, Go, Python, JSON, YAML, MD |
| Fixture Shared Cache Payload | `124 files (22.57 MiB)` (`target/`, `node_modules/`, `.venv/`) |

## 2. Methodology & Reproducibility

All measurements were executed from the standalone `benches/` harness (`loom-bench`) against the compiled `release` `loom.exe` over real Windows Named Pipe IPC (`\\.\pipe\loom-<blake3_16>`) and isolated `%TEMP%` git repositories + local bare remotes:

```powershell
cargo build --release --package loom-cli --bin loom
cargo run --release --manifest-path benches/Cargo.toml -- --loom-bin target\release\loom.exe --report BENCHMARK_REPORT.md
```

- **Spawn Suite**: Cold-cache trials (`N=10`) stop `loomd` via Named Pipe `RpcMethod::Shutdown` on independent fresh repositories with empty `.loom/cas/`. Warm sequential (`N=10`) and warm parallel (`N=10`) trials run against warmed `.loom/cas/` and NTFS MFT/page caches.
- **Radar P99 Suite**: Measured end-to-end from file write completion to atomic `<viewport>/.loom/RADAR_STATUS.json` update (250µs polling, zero fixed sleeps) across 105 trials (`N=5`) and 105 trials (`N=10`), covering `InsertMember` (`GREEN->GREEN`), `PENDING_SYNTAX` (`false->true->false`), `StructuralConflict` (`GREEN->RED->GREEN`), `SemanticConflict` (`GREEN->RED->GREEN`), and `YELLOW` advisories (`consumed_symbol_modified`, `crdt_concurrent_edit`).
- **Memory Suite**: Win32 `OpenProcess` + `K32GetProcessMemoryInfo` (`PROCESS_MEMORY_COUNTERS_EX`) polling `WorkingSetSize`, `PrivateUsage`, and `PeakWorkingSetSize` on `loomd` PID `10328` at 10 active viewports.
- **Storage Suite**: Multi-round parallel refactoring (`N=5` and `N=10`) with `loom seal -v`, `loom drop` (`NegativeEdge`), and `loom git push` to local `%TEMP%` bare remotes, summing exact recursive logical byte sizes of `.loom/cas/` vs `.git/objects/` (and post-`git gc` packed size for secondary reference).

## 3. Metric Suite 1 — Viewport Spawn Latency (`WindowsNtfsFallback`)

| Regime | Count | Min (ms) | P50 (ms) | P95 (ms) | P99 (ms) | Max (ms) | Mean (ms) |
|---|---:|---:|---:|---:|---:|---:|---:|
| Cold Cache (daemon stopped + empty CAS) | 10 | 236.28 | 239.50 | 264.81 | 264.81 | 264.81 | 243.55 |
| Warm Cache Sequential | 10 | 120.02 | 121.02 | 239.59 | 239.59 | 239.59 | 134.64 |
| Warm Cache Parallel (10 threads) | 10 | 129.71 | 410.95 | 954.74 | 954.74 | 954.74 | 490.52 |

- **Parallel 10-viewport total wall-clock**: `955.19 ms` (serialized inside `loomd` via `trunk_seal_lock`).
- **Per-viewport mount counts (sample)**: `hardlinked_files = 116`, `copied_files = 38`.
- **Verified NTFS link counts (`GetFileInformationByHandle`)**: third-party `serde.rlib` links=`11`, `node_modules/tsc.js` links=`11`, `.venv/numpy_stub.pyd` links=`11`, workspace `core_engine.rlib` links=`1`, editable `src/lib.rs` links=`1`, `.cargo-lock` skipped=`true`, `incremental/` skipped=`true`.

## 4. Metric Suite 2 — Speculative Merge Radar P99 Latency

| Active Viewports | Samples | Min (ms) | P50 (ms) | P95 (ms) | P99 (ms) | Max (ms) | Mean (ms) |
|---|---:|---:|---:|---:|---:|---:|---:|
| `N = 5` | 105 | 28.63 | 69.17 | 86.38 | 92.58 | 239.92 | 66.75 |
| `N = 10` | 105 | 28.62 | 89.37 | 126.59 | 156.67 | 233.10 | 85.41 |

- **`N=5` Scenario Breakdown**: `InsertMember(GREEN)=48`, `PENDING_SYNTAX=15`, `StructuralConflict(RED/recover)=14`, `SemanticConflict(RED/recover)=14`, `YELLOW_advisory=14` | Radar sequence `6 -> 168` | `batch_mode_triggered=true`.
- **`N=10` Scenario Breakdown**: `InsertMember(GREEN)=48`, `PENDING_SYNTAX=15`, `StructuralConflict(RED/recover)=14`, `SemanticConflict(RED/recover)=14`, `YELLOW_advisory=14` | Radar sequence `11 -> 173` | `batch_mode_triggered=true`.

## 5. Metric Suite 3 — `loomd` Memory Footprint (10 Active Viewports)

| Regime | Samples | Working Set (MiB) | Private Bytes (MiB) | OS Peak Working Set (MiB) |
|---|---:|---:|---:|---:|
| Idle (`N=10` active viewports) | 10 | 30.45 | 14.85 | - |
| Peak (10x10 concurrent merges + CST diffs + seals, 5ms poll) | 1322 | 32.18 | 15.82 | 32.18 |

## 6. Metric Suite 4 — Storage Efficiency (`.loom/cas/` vs `.git/objects/`)

| Workload | Sealed Capsules | Dropped (`NegativeEdge`) | Pushed Commits | `.loom/cas/` Bytes (Objects) | `.git/objects/` Loose Bytes (Objects) | Ratio (`CAS / Git Loose`) | `.git/objects/` Post-`git gc` Packed | Ratio (`CAS / Git Packed`) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `N = 5` (2 rounds) | 9 | 1 | 9 | 50935 (153) | 58079 (273) | 0.877x | 40969 | 1.243x |
| `N = 10` (2 rounds) | 18 | 2 | 18 | 101578 (281) | 116219 (492) | 0.874x | 70838 | 1.434x |

- **`N=5` Cross-check**: `loom status --json` `cas_total_bytes = 50935` (disk walk = `50935` bytes), bare remote `objects/` = `58079` bytes, cumulative touched file bytes across rounds = `37680` bytes, `synced_negative_edges = 1`.
- **`N=10` Cross-check**: `loom status --json` `cas_total_bytes = 101578` (disk walk = `101578` bytes), bare remote `objects/` = `116219` bytes, cumulative touched file bytes across rounds = `90115` bytes, `synced_negative_edges = 2`.

## 7. SLA Assessment & Windows NTFS Fallback Analysis

- **NTFS Fallback Behavior**: Because ReFS block cloning (`FSCTL_DUPLICATE_EXTENTS_TO_FILE`) and Windows ProjFS (`ProjectedFSLib.dll`) are unavailable on this standard Windows 11 NTFS volume, `loom-vfs` automatically selected `WindowsNtfsFallback`. Editable workspace sources and workspace crate artifacts (`core_engine.rlib`, `api_gateway.rlib`) were copied independently per viewport (`nNumberOfLinks = 1`), while immutable third-party caches (`target/debug/deps/*.rlib`, `node_modules/`, `.venv/`) were zero-copy hardlinked (`nNumberOfLinks > 1`) and lock/incremental files (`.cargo-lock`, `incremental/`) were excluded.
- **Spawn Latency Impact**: Warm sequential spawn P50 (`121.02 ms`) / P99 (`239.59 ms`) eliminates cold daemon startup and Zstd-3 CAS compression overhead seen in cold spawns (P50 `239.50 ms`, P99 `264.81 ms`). Under `WindowsNtfsFallback`, editable file copies scale linearly with editable source file count/size (unlike O(1) ReFS metadata CoW or lazy ProjFS projection), and parallel spawns serialize on `loomd`'s `trunk_seal_lock` (`955.19 ms` wall-clock for 10 concurrent spawns).
- **Radar P99 Responsiveness**: With `debounce_ms = 15`, end-to-end Radar P99 measured `92.58 ms` at `N=5` and `156.67 ms` at `N=10` active viewports across real `notify` (`ReadDirectoryChangesW`), incremental Tree-sitter parsing, N×N shadow merges, and atomic `RADAR_STATUS.json` writes.
- **Daemon Memory & CAS Efficiency**: At 10 active viewports, `loomd` maintained `30.45 MiB` Working Set (`14.85 MiB` Private Bytes) at idle and peaked at `32.18 MiB` Working Set (`15.82 MiB` Private Bytes) during concurrent 10×10 shadow merges and seals. Content-addressed Zstd-3 `.loom/cas/` storage achieved `0.874x` vs loose `.git/objects/` (`1.434x` vs post-`git gc` packed `.git/objects/`) at `N=10`.

## 8. Hygiene & Cleanup Contract

All benchmark repositories and bare git remotes were created strictly inside `%TEMP%` under a 4-stage RAII `BenchWorkspaceGuard` (child kill -> Named Pipe `RpcMethod::Shutdown` -> Win32 `TerminateProcess`/`taskkill` fallback -> recursive read-only clear & `remove_dir_all`), ensuring zero lingering `loom`/`loomd` processes, zero residual `loom_bench_*` temp directories, and no `.loom/` directory in the project workspace root.
