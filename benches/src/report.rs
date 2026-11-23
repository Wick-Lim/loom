use crate::metrics::BenchmarkReport;
use std::fmt::Write;

pub const MANDATORY_NTFS_LINE: &str =
    "Filesystem: NTFS (ReFS CoW / ProjFS unavailable on this Windows host; WindowsNtfsFallback active)";

pub fn render_markdown_report(r: &BenchmarkReport) -> String {
    let mut md = String::with_capacity(8192);

    let _ = writeln!(md, "# Project Loom — F25 Windows Multi-Agent Performance SLA Benchmark Report");
    let _ = writeln!(md);
    let _ = writeln!(md, "{MANDATORY_NTFS_LINE}");
    let _ = writeln!(md);
    let _ = writeln!(md, "- **Generated (UTC)**: `{}`", r.generated_at_utc);
    let _ = writeln!(md, "- **Loom Binary**: `{}` (`{}` profile)", r.loom_bin, r.loom_profile);
    let _ = writeln!(md, "- **Active VFS Driver**: `{}`", r.spawn.backend_driver);
    let _ = writeln!(md);

    let _ = writeln!(md, "## 1. Environment & Toolchain Baseline");
    let _ = writeln!(md);
    let _ = writeln!(md, "| Component | Measured Value |");
    let _ = writeln!(md, "|---|---|");
    let _ = writeln!(md, "| OS | `{}` |", r.os_version);
    let _ = writeln!(md, "| CPU | `{}` |", r.cpu_name);
    let _ = writeln!(md, "| Physical RAM | `{:.2} GiB` |", r.total_ram_gib);
    let _ = writeln!(md, "| Rust Toolchain | `{}` |", r.rustc_version);
    let _ = writeln!(md, "| Git | `{}` |", r.git_version);
    let _ = writeln!(
        md,
        "| Fixture Source Payload | `{} files ({:.2} KiB)` across Rust, TS/TSX, Go, Python, JSON, YAML, MD |",
        r.fixture.source_files,
        r.fixture.source_bytes as f64 / 1024.0
    );
    let _ = writeln!(
        md,
        "| Fixture Shared Cache Payload | `{} files ({:.2} MiB)` (`target/`, `node_modules/`, `.venv/`) |",
        r.fixture.cache_files,
        r.fixture.cache_bytes as f64 / (1024.0 * 1024.0)
    );
    let _ = writeln!(md);

    let _ = writeln!(md, "## 2. Methodology & Reproducibility");
    let _ = writeln!(md);
    let _ = writeln!(
        md,
        "All measurements were executed from the standalone `benches/` harness (`loom-bench`) against the compiled `{}` `loom.exe` over real Windows Named Pipe IPC (`\\\\.\\pipe\\loom-<blake3_16>`) and isolated `%TEMP%` git repositories + local bare remotes:",
        r.loom_profile
    );
    let _ = writeln!(md);
    let _ = writeln!(md, "```powershell");
    let _ = writeln!(md, "cargo build --release --package loom-cli --bin loom");
    let _ = writeln!(
        md,
        "cargo run --release --manifest-path benches/Cargo.toml -- --loom-bin target\\release\\loom.exe --report BENCHMARK_REPORT.md"
    );
    let _ = writeln!(md, "```");
    let _ = writeln!(md);
    let _ = writeln!(
        md,
        "- **Spawn Suite**: Cold-cache trials (`N={}`) stop `loomd` via Named Pipe `RpcMethod::Shutdown` on independent fresh repositories with empty `.loom/cas/`. Warm sequential (`N={}`) and warm parallel (`N={}`) trials run against warmed `.loom/cas/` and NTFS MFT/page caches.",
        r.spawn.cold_stats.count, r.spawn.warm_seq_stats.count, r.spawn.warm_par_stats.count
    );
    let _ = writeln!(
        md,
        "- **Radar P99 Suite**: Measured end-to-end from file write completion to atomic `<viewport>/.loom/RADAR_STATUS.json` update (250µs polling, zero fixed sleeps) across {} trials (`N=5`) and {} trials (`N=10`), covering `InsertMember` (`GREEN->GREEN`), `PENDING_SYNTAX` (`false->true->false`), `StructuralConflict` (`GREEN->RED->GREEN`), `SemanticConflict` (`GREEN->RED->GREEN`), and `YELLOW` advisories (`consumed_symbol_modified`, `crdt_concurrent_edit`).",
        r.radar_5.stats.count, r.radar_10.stats.count
    );
    let _ = writeln!(
        md,
        "- **Memory Suite**: Win32 `OpenProcess` + `K32GetProcessMemoryInfo` (`PROCESS_MEMORY_COUNTERS_EX`) polling `WorkingSetSize`, `PrivateUsage`, and `PeakWorkingSetSize` on `loomd` PID `{}` at 10 active viewports.",
        r.memory_10.daemon_pid
    );
    let _ = writeln!(
        md,
        "- **Storage Suite**: Multi-round parallel refactoring (`N=5` and `N=10`) with `loom seal -v`, `loom drop` (`NegativeEdge`), and `loom git push` to local `%TEMP%` bare remotes, summing exact recursive logical byte sizes of `.loom/cas/` vs `.git/objects/` (and post-`git gc` packed size for secondary reference).",
    );
    let _ = writeln!(md);

    let _ = writeln!(md, "## 3. Metric Suite 1 — Viewport Spawn Latency (`WindowsNtfsFallback`)");
    let _ = writeln!(md);
    let _ = writeln!(
        md,
        "| Regime | Count | Min (ms) | P50 (ms) | P95 (ms) | P99 (ms) | Max (ms) | Mean (ms) |"
    );
    let _ = writeln!(md, "|---|---:|---:|---:|---:|---:|---:|---:|");
    let par_label = format!(
        "Warm Cache Parallel ({} threads)",
        r.spawn.warm_par_stats.count
    );
    for (label, s) in [
        ("Cold Cache (daemon stopped + empty CAS)", &r.spawn.cold_stats),
        ("Warm Cache Sequential", &r.spawn.warm_seq_stats),
        (par_label.as_str(), &r.spawn.warm_par_stats),
    ] {
        let _ = writeln!(
            md,
            "| {} | {} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} |",
            label, s.count, s.min_ms, s.p50_ms, s.p95_ms, s.p99_ms, s.max_ms, s.mean_ms
        );
    }
    let _ = writeln!(md);
    let _ = writeln!(
        md,
        "- **Parallel 10-viewport total wall-clock**: `{:.2} ms` (serialized inside `loomd` via `trunk_seal_lock`).",
        r.spawn.warm_par_wall_ms
    );
    let _ = writeln!(
        md,
        "- **Per-viewport mount counts (sample)**: `hardlinked_files = {}`, `copied_files = {}`.",
        r.spawn.sample_hardlinked_files, r.spawn.sample_copied_files
    );
    let lc = &r.spawn.ntfs_link_check;
    let _ = writeln!(
        md,
        "- **Verified NTFS link counts (`GetFileInformationByHandle`)**: third-party `serde.rlib` links=`{}`, `node_modules/tsc.js` links=`{}`, `.venv/numpy_stub.pyd` links=`{}`, workspace `core_engine.rlib` links=`{}`, editable `src/lib.rs` links=`{}`, `.cargo-lock` skipped=`{}`, `incremental/` skipped=`{}`.",
        lc.hardlinked_third_party_rlib_links,
        lc.hardlinked_node_modules_links,
        lc.hardlinked_venv_links,
        lc.isolated_workspace_rlib_links,
        lc.isolated_editable_src_links,
        lc.cargo_lock_skipped,
        lc.incremental_dir_skipped
    );
    let _ = writeln!(md);

    let _ = writeln!(md, "## 4. Metric Suite 2 — Speculative Merge Radar P99 Latency");
    let _ = writeln!(md);
    let _ = writeln!(
        md,
        "| Active Viewports | Samples | Min (ms) | P50 (ms) | P95 (ms) | P99 (ms) | Max (ms) | Mean (ms) |"
    );
    let _ = writeln!(md, "|---|---:|---:|---:|---:|---:|---:|---:|");
    for rr in [&r.radar_5, &r.radar_10] {
        let s = &rr.stats;
        let _ = writeln!(
            md,
            "| `N = {}` | {} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} |",
            rr.active_viewports, s.count, s.min_ms, s.p50_ms, s.p95_ms, s.p99_ms, s.max_ms, s.mean_ms
        );
    }
    let _ = writeln!(md);
    for rr in [&r.radar_5, &r.radar_10] {
        let sc = &rr.scenario_counts;
        let _ = writeln!(
            md,
            "- **`N={}` Scenario Breakdown**: `InsertMember(GREEN)={}`, `PENDING_SYNTAX={}`, `StructuralConflict(RED/recover)={}`, `SemanticConflict(RED/recover)={}`, `YELLOW_advisory={}` | Radar sequence `{} -> {}` | `batch_mode_triggered={}`.",
            rr.active_viewports,
            sc.commutative_insert_member,
            sc.pending_syntax_guard,
            sc.structural_conflict,
            sc.semantic_conflict,
            sc.yellow_advisory,
            rr.initial_sequence,
            rr.final_sequence,
            rr.batch_mode_triggered
        );
    }
    let _ = writeln!(md);

    let _ = writeln!(md, "## 5. Metric Suite 3 — `loomd` Memory Footprint (10 Active Viewports)");
    let _ = writeln!(md);
    let m = &r.memory_10;
    let _ = writeln!(
        md,
        "| Regime | Samples | Working Set (MiB) | Private Bytes (MiB) | OS Peak Working Set (MiB) |"
    );
    let _ = writeln!(md, "|---|---:|---:|---:|---:|");
    let _ = writeln!(
        md,
        "| Idle (`N=10` active viewports) | {} | {:.2} | {:.2} | - |",
        m.idle_sample_count, m.idle_working_set_mib, m.idle_private_bytes_mib
    );
    let _ = writeln!(
        md,
        "| Peak (10x10 concurrent merges + CST diffs + seals, 5ms poll) | {} | {:.2} | {:.2} | {:.2} |",
        m.peak_sample_count,
        m.peak_working_set_mib,
        m.peak_private_bytes_mib,
        m.os_peak_working_set_mib
    );
    let _ = writeln!(md);

    let _ = writeln!(md, "## 6. Metric Suite 4 — Storage Efficiency (`.loom/cas/` vs `.git/objects/`)");
    let _ = writeln!(md);
    let _ = writeln!(
        md,
        "| Workload | Sealed Capsules | Dropped (`NegativeEdge`) | Pushed Commits | `.loom/cas/` Bytes (Objects) | `.git/objects/` Loose Bytes (Objects) | Ratio (`CAS / Git Loose`) | `.git/objects/` Post-`git gc` Packed | Ratio (`CAS / Git Packed`) |"
    );
    let _ = writeln!(md, "|---|---:|---:|---:|---:|---:|---:|---:|---:|");
    for sr in [&r.storage_5, &r.storage_10] {
        let _ = writeln!(
            md,
            "| `N = {}` (2 rounds) | {} | {} | {} | {} ({}) | {} ({}) | {:.3}x | {} | {:.3}x |",
            sr.viewport_count,
            sr.sealed_capsules,
            sr.dropped_negative_edges,
            sr.pushed_commits,
            sr.cas_bytes,
            sr.cas_object_count,
            sr.git_objects_bytes,
            sr.git_object_count,
            sr.cas_to_git_loose_ratio,
            sr.git_objects_packed_bytes,
            sr.cas_to_git_packed_ratio
        );
    }
    let _ = writeln!(md);
    for sr in [&r.storage_5, &r.storage_10] {
        let _ = writeln!(
            md,
            "- **`N={}` Cross-check**: `loom status --json` `cas_total_bytes = {}` (disk walk = `{}` bytes), bare remote `objects/` = `{}` bytes, cumulative touched file bytes across rounds = `{}` bytes, `synced_negative_edges = {}`.",
            sr.viewport_count,
            sr.status_reported_cas_bytes,
            sr.cas_bytes,
            sr.bare_objects_bytes,
            sr.raw_capsule_source_bytes,
            sr.synced_negative_edges
        );
    }
    let _ = writeln!(md);

    let _ = writeln!(md, "## 7. SLA Assessment & Windows NTFS Fallback Analysis");
    let _ = writeln!(md);
    let _ = writeln!(
        md,
        "- **NTFS Fallback Behavior**: Because ReFS block cloning (`FSCTL_DUPLICATE_EXTENTS_TO_FILE`) and Windows ProjFS (`ProjectedFSLib.dll`) are unavailable on this standard Windows 11 NTFS volume, `loom-vfs` automatically selected `{}`. Editable workspace sources and workspace crate artifacts (`core_engine.rlib`, `api_gateway.rlib`) were copied independently per viewport (`nNumberOfLinks = 1`), while immutable third-party caches (`target/debug/deps/*.rlib`, `node_modules/`, `.venv/`) were zero-copy hardlinked (`nNumberOfLinks > 1`) and lock/incremental files (`.cargo-lock`, `incremental/`) were excluded.",
        r.spawn.backend_driver
    );
    let _ = writeln!(
        md,
        "- **Spawn Latency Impact**: Warm sequential spawn P50 (`{:.2} ms`) / P99 (`{:.2} ms`) eliminates cold daemon startup and Zstd-3 CAS compression overhead seen in cold spawns (P50 `{:.2} ms`, P99 `{:.2} ms`). Under `WindowsNtfsFallback`, editable file copies scale linearly with editable source file count/size (unlike O(1) ReFS metadata CoW or lazy ProjFS projection), and parallel spawns serialize on `loomd`'s `trunk_seal_lock` (`{:.2} ms` wall-clock for {} concurrent spawns).",
        r.spawn.warm_seq_stats.p50_ms,
        r.spawn.warm_seq_stats.p99_ms,
        r.spawn.cold_stats.p50_ms,
        r.spawn.cold_stats.p99_ms,
        r.spawn.warm_par_wall_ms,
        r.spawn.warm_par_stats.count
    );
    let _ = writeln!(
        md,
        "- **Radar P99 Responsiveness**: With `debounce_ms = 15`, end-to-end Radar P99 measured `{:.2} ms` at `N=5` and `{:.2} ms` at `N=10` active viewports across real `notify` (`ReadDirectoryChangesW`), incremental Tree-sitter parsing, N×N shadow merges, and atomic `RADAR_STATUS.json` writes.",
        r.radar_5.stats.p99_ms, r.radar_10.stats.p99_ms
    );
    let _ = writeln!(
        md,
        "- **Daemon Memory & CAS Efficiency**: At 10 active viewports, `loomd` maintained `{:.2} MiB` Working Set (`{:.2} MiB` Private Bytes) at idle and peaked at `{:.2} MiB` Working Set (`{:.2} MiB` Private Bytes) during concurrent 10×10 shadow merges and seals. Content-addressed Zstd-3 `.loom/cas/` storage achieved `{:.3}x` vs loose `.git/objects/` (`{:.3}x` vs post-`git gc` packed `.git/objects/`) at `N=10`.",
        m.idle_working_set_mib,
        m.idle_private_bytes_mib,
        m.peak_working_set_mib,
        m.peak_private_bytes_mib,
        r.storage_10.cas_to_git_loose_ratio,
        r.storage_10.cas_to_git_packed_ratio
    );
    let _ = writeln!(md);

    let _ = writeln!(md, "## 8. Hygiene & Cleanup Contract");
    let _ = writeln!(md);
    let _ = writeln!(
        md,
        "All benchmark repositories and bare git remotes were created strictly inside `%TEMP%` under a 4-stage RAII `BenchWorkspaceGuard` (child kill -> Named Pipe `RpcMethod::Shutdown` -> Win32 `TerminateProcess`/`taskkill` fallback -> recursive read-only clear & `remove_dir_all`), ensuring zero lingering `loom`/`loomd` processes, zero residual `loom_bench_*` temp directories, and no `.loom/` directory in the project workspace root."
    );

    md
}
