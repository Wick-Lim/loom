use crate::fixture::{self, FixtureSummary};
use crate::guard::BenchWorkspaceGuard;
use crate::loom_cli::{LoomCli, RadarStatusReport};
use crate::stats::{LatencyStats, median_f64};
use crate::win32::{self, ProcessMemorySample};
use anyhow::Context;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize)]
pub struct NtfsLinkCheck {
    pub hardlinked_third_party_rlib_links: u32,
    pub hardlinked_node_modules_links: u32,
    pub hardlinked_venv_links: u32,
    pub isolated_workspace_rlib_links: u32,
    pub isolated_editable_src_links: u32,
    pub cargo_lock_skipped: bool,
    pub incremental_dir_skipped: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SpawnSuiteMetrics {
    pub backend_driver: String,
    pub cold_stats: LatencyStats,
    pub cold_samples_ms: Vec<f64>,
    pub warm_seq_stats: LatencyStats,
    pub warm_seq_samples_ms: Vec<f64>,
    pub warm_par_stats: LatencyStats,
    pub warm_par_samples_ms: Vec<f64>,
    pub warm_par_wall_ms: f64,
    pub sample_hardlinked_files: u64,
    pub sample_copied_files: u64,
    pub ntfs_link_check: NtfsLinkCheck,
}

#[derive(Debug, Clone, Serialize)]
pub struct RadarScenarioCounts {
    pub commutative_insert_member: usize,
    pub pending_syntax_guard: usize,
    pub structural_conflict: usize,
    pub semantic_conflict: usize,
    pub yellow_advisory: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct RadarRunMetrics {
    pub active_viewports: usize,
    pub stats: LatencyStats,
    pub samples_ms: Vec<f64>,
    pub scenario_counts: RadarScenarioCounts,
    pub initial_sequence: u64,
    pub final_sequence: u64,
    pub batch_mode_triggered: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemorySuiteMetrics {
    pub daemon_pid: u32,
    pub active_viewports: usize,
    pub idle_sample_count: usize,
    pub idle_working_set_mib: f64,
    pub idle_private_bytes_mib: f64,
    pub peak_sample_count: usize,
    pub peak_working_set_mib: f64,
    pub peak_private_bytes_mib: f64,
    pub os_peak_working_set_mib: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct StorageRunMetrics {
    pub viewport_count: usize,
    pub sealed_capsules: usize,
    pub dropped_negative_edges: usize,
    pub pushed_commits: usize,
    pub synced_negative_edges: usize,
    pub cas_object_count: usize,
    pub cas_bytes: u64,
    pub status_reported_cas_bytes: u64,
    pub git_object_count: usize,
    pub git_objects_bytes: u64,
    pub bare_objects_bytes: u64,
    pub git_objects_packed_bytes: u64,
    pub raw_capsule_source_bytes: u64,
    pub cas_to_git_loose_ratio: f64,
    pub cas_to_git_packed_ratio: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct BenchmarkReport {
    pub generated_at_utc: String,
    pub loom_bin: String,
    pub loom_profile: String,
    pub os_version: String,
    pub cpu_name: String,
    pub total_ram_gib: f64,
    pub rustc_version: String,
    pub git_version: String,
    pub filesystem_note: String,
    pub fixture: FixtureSummary,
    pub spawn: SpawnSuiteMetrics,
    pub radar_5: RadarRunMetrics,
    pub radar_10: RadarRunMetrics,
    pub memory_10: MemorySuiteMetrics,
    pub storage_5: StorageRunMetrics,
    pub storage_10: StorageRunMetrics,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn bytes_to_mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn read_radar_status(vp_path: &Path) -> Option<RadarStatusReport> {
    let status_path = vp_path.join(".loom").join("RADAR_STATUS.json");
    let bytes = std::fs::read(&status_path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn wait_until_radar<F>(
    vp_path: &Path,
    timeout: Duration,
    mut predicate: F,
) -> anyhow::Result<(RadarStatusReport, Duration)>
where
    F: FnMut(&RadarStatusReport) -> bool,
{
    let start = Instant::now();
    loop {
        if let Some(report) = read_radar_status(vp_path)
            && predicate(&report)
        {
            return Ok((report, start.elapsed()));
        }
        if start.elapsed() >= timeout {
            let last = read_radar_status(vp_path);
            anyhow::bail!(
                "timed out after {:?} waiting for RADAR_STATUS.json at {} (last={:?})",
                timeout,
                vp_path.display(),
                last
            );
        }
        thread::sleep(Duration::from_micros(250));
    }
}

fn wait_for_updated_timestamp_gt(prev_ms: u64) {
    while now_ms() <= prev_ms {
        thread::sleep(Duration::from_micros(200));
    }
}

fn write_text_file(path: &Path, content: &str) -> anyhow::Result<()> {
    std::fs::write(path, content.as_bytes())
        .with_context(|| format!("write {}", path.display()))
}

fn warmup_repo_cache(cli: &LoomCli, repo_root: &Path) -> anyhow::Result<()> {
    let _ = cli.spawn_timed(repo_root, "vp-warmup", "warmup cas and ntfs cache")?;
    let readme = repo_root
        .join(".loom")
        .join("viewports")
        .join("vp-warmup")
        .join("README.md");
    let mut text = std::fs::read_to_string(&readme)?;
    text.push_str("\nWarmup checkpoint.\n");
    write_text_file(&readme, &text)?;
    let _ = wait_until_radar(
        &repo_root.join(".loom").join("viewports").join("vp-warmup"),
        Duration::from_secs(10),
        |r| !r.pending_syntax,
    )?;
    let _ = cli.seal(repo_root, "vp-warmup", "warmup seal", None)?;
    let _ = cli.gc_prune(repo_root)?;
    Ok(())
}

pub fn measure_spawn_suite(
    cli: &LoomCli,
    guard: &mut BenchWorkspaceGuard,
    quick: bool,
) -> anyhow::Result<(SpawnSuiteMetrics, FixtureSummary)> {
    let cold_count = if quick { 3 } else { 10 };
    let seq_count = if quick { 5 } else { 10 };
    let par_count = if quick { 5 } else { 10 };

    let mut cold_samples = Vec::with_capacity(cold_count);
    let mut sample_driver = String::new();
    let mut sample_hardlinked = 0u64;
    let mut sample_copied = 0u64;
    let mut fixture_summary = None;

    for i in 0..cold_count {
        let tmp = guard.create_temp_dir(&format!("spawn_cold_{i}"))?;
        let fx = fixture::create_fixture_repo(&tmp, false)?;
        if fixture_summary.is_none() {
            fixture_summary = Some(fx.summary.clone());
        }
        guard.register_loom_repo(fx.repo_root.clone());
        let _ = cli.init_colocate_git(&fx.repo_root)?;
        if let Ok(Some(pid)) = crate::ipc::shutdown_daemon(&fx.repo_root, 5000) {
            guard.track_pid(pid);
        }
        let (vp, dur) = cli.spawn_timed(&fx.repo_root, "vp-cold", "cold spawn benchmark")?;
        if let Ok(st) = cli.status(&fx.repo_root) {
            guard.track_pid(st.pid);
        }
        cold_samples.push(dur.as_secs_f64() * 1000.0);
        if sample_driver.is_empty() {
            sample_driver = vp.driver.clone();
            if let Some(m) = &vp.mount {
                sample_hardlinked = m.hardlinked_files;
                sample_copied = m.copied_files;
            }
        }
        guard.shutdown_and_remove_repo(&fx.repo_root, &tmp);
    }

    // Warm sequential spawns
    let tmp_seq = guard.create_temp_dir("spawn_warm_seq")?;
    let fx_seq = fixture::create_fixture_repo(&tmp_seq, false)?;
    guard.register_loom_repo(fx_seq.repo_root.clone());
    let _ = cli.init_colocate_git(&fx_seq.repo_root)?;
    if let Ok(st) = cli.status(&fx_seq.repo_root) {
        guard.track_pid(st.pid);
    }
    warmup_repo_cache(cli, &fx_seq.repo_root)?;

    let mut warm_seq_samples = Vec::with_capacity(seq_count);
    let mut first_vp_path = None;
    for i in 0..seq_count {
        let name = format!("vp-seq-{i:02}");
        let (vp, dur) = cli.spawn_timed(&fx_seq.repo_root, &name, "warm sequential spawn")?;
        warm_seq_samples.push(dur.as_secs_f64() * 1000.0);
        if first_vp_path.is_none() {
            first_vp_path = Some(vp.path);
        }
    }

    let vp0 = first_vp_path.context("missing first warm viewport path")?;
    let ntfs_link_check = NtfsLinkCheck {
        hardlinked_third_party_rlib_links: win32::file_link_info(
            &vp0.join("target/debug/deps/serde-9a1b2c3d.rlib"),
        )?
        .n_links,
        hardlinked_node_modules_links: win32::file_link_info(
            &vp0.join("node_modules/typescript/lib/tsc.js"),
        )?
        .n_links,
        hardlinked_venv_links: win32::file_link_info(
            &vp0.join(".venv/Lib/site-packages/numpy_stub.pyd"),
        )?
        .n_links,
        isolated_workspace_rlib_links: win32::file_link_info(
            &vp0.join("target/debug/deps/core_engine-1111aaaa.rlib"),
        )?
        .n_links,
        isolated_editable_src_links: win32::file_link_info(
            &vp0.join("crates/core_engine/src/lib.rs"),
        )?
        .n_links,
        cargo_lock_skipped: !vp0.join("target/debug/.cargo-lock").exists()
            && !vp0.join("target/.cargo-lock").exists(),
        incremental_dir_skipped: !vp0.join("target/debug/incremental").exists(),
    };
    guard.shutdown_and_remove_repo(&fx_seq.repo_root, &tmp_seq);

    // Warm parallel spawns
    let tmp_par = guard.create_temp_dir("spawn_warm_par")?;
    let fx_par = fixture::create_fixture_repo(&tmp_par, false)?;
    guard.register_loom_repo(fx_par.repo_root.clone());
    let _ = cli.init_colocate_git(&fx_par.repo_root)?;
    if let Ok(st) = cli.status(&fx_par.repo_root) {
        guard.track_pid(st.pid);
    }
    warmup_repo_cache(cli, &fx_par.repo_root)?;

    let barrier = Arc::new(Barrier::new(par_count));
    let wall_start = Instant::now();
    let mut handles = Vec::with_capacity(par_count);
    for i in 0..par_count {
        let cli_c = cli.clone();
        let repo_c = fx_par.repo_root.clone();
        let barrier_c = Arc::clone(&barrier);
        handles.push(thread::spawn(move || -> anyhow::Result<f64> {
            barrier_c.wait();
            let (_vp, dur) =
                cli_c.spawn_timed(&repo_c, &format!("vp-par-{i:02}"), "warm parallel spawn")?;
            Ok(dur.as_secs_f64() * 1000.0)
        }));
    }
    let mut warm_par_samples = Vec::with_capacity(par_count);
    for h in handles {
        let ms = h
            .join()
            .map_err(|_| anyhow::anyhow!("parallel spawn worker panicked"))??;
        warm_par_samples.push(ms);
    }
    let warm_par_wall_ms = wall_start.elapsed().as_secs_f64() * 1000.0;
    guard.shutdown_and_remove_repo(&fx_par.repo_root, &tmp_par);

    let metrics = SpawnSuiteMetrics {
        backend_driver: sample_driver,
        cold_stats: LatencyStats::from_ms(&cold_samples),
        cold_samples_ms: cold_samples,
        warm_seq_stats: LatencyStats::from_ms(&warm_seq_samples),
        warm_seq_samples_ms: warm_seq_samples,
        warm_par_stats: LatencyStats::from_ms(&warm_par_samples),
        warm_par_samples_ms: warm_par_samples,
        warm_par_wall_ms,
        sample_hardlinked_files: sample_hardlinked,
        sample_copied_files: sample_copied,
        ntfs_link_check,
    };

    Ok((
        metrics,
        fixture_summary.context("no cold fixture summary recorded")?,
    ))
}

fn spawn_active_viewports(
    cli: &LoomCli,
    repo_root: &Path,
    count: usize,
) -> anyhow::Result<Vec<(String, PathBuf)>> {
    let mut viewports = Vec::with_capacity(count);
    for i in 1..=count {
        let name = format!("vp-{i:02}");
        let (vp, _) = cli.spawn_timed(repo_root, &name, &format!("active agent {i:02}"))?;
        let _ = wait_until_radar(&vp.path, Duration::from_secs(10), |r| {
            r.status == "GREEN" && !r.pending_syntax
        })?;
        viewports.push((vp.name, vp.path));
    }
    Ok(viewports)
}

#[allow(clippy::too_many_lines)]
pub fn measure_radar_and_optional_memory(
    cli: &LoomCli,
    guard: &mut BenchWorkspaceGuard,
    n_viewports: usize,
    target_trials: usize,
    capture_memory: bool,
) -> anyhow::Result<(RadarRunMetrics, Option<MemorySuiteMetrics>)> {
    let tmp = guard.create_temp_dir(&format!("radar_n{n_viewports}"))?;
    let fx = fixture::create_fixture_repo(&tmp, false)?;
    guard.register_loom_repo(fx.repo_root.clone());
    let _ = cli.init_colocate_git(&fx.repo_root)?;
    let status = cli.status(&fx.repo_root)?;
    guard.track_pid(status.pid);
    let daemon_pid = status.pid;

    let viewports = spawn_active_viewports(cli, &fx.repo_root, n_viewports)?;
    let snap_before = cli.radar_snapshot(&fx.repo_root)?;
    anyhow::ensure!(
        snap_before.viewports.len() == n_viewports,
        "expected {} viewports in radar snapshot, got {}",
        n_viewports,
        snap_before.viewports.len()
    );

    let mut samples_ms = Vec::with_capacity(target_trials + 16);
    let mut counts = RadarScenarioCounts {
        commutative_insert_member: 0,
        pending_syntax_guard: 0,
        structural_conflict: 0,
        semantic_conflict: 0,
        yellow_advisory: 0,
    };

    let mut cycle = 0usize;
    while samples_ms.len() < target_trials {
        cycle += 1;

        // 1. Six commutative InsertMember edits (Rust, TS, Go, Python, Rust route, TS)
        for sub in 0..6 {
            if samples_ms.len() >= target_trials {
                break;
            }
            let vp_idx = (cycle + sub) % n_viewports;
            let vp_path = &viewports[vp_idx].1;
            let prev = read_radar_status(vp_path)
                .context("missing RADAR_STATUS.json before InsertMember")?;
            let (rel, addition) = match sub % 4 {
                0 => (
                    "crates/core_engine/src/lib.rs".to_string(),
                    format!(
                        "\npub fn radar_comm_rs_{cycle}_{sub}(v: u64) -> u64 {{ v + {} }}\n",
                        cycle * 10 + sub
                    ),
                ),
                1 => (
                    "ts/src/billing/formatter.ts".to_string(),
                    format!(
                        "\nexport function radarCommTs_{cycle}_{sub}(v: number): number {{ return v + {}; }}\n",
                        cycle * 10 + sub
                    ),
                ),
                2 => (
                    "go/pkg/scheduler/scheduler.go".to_string(),
                    format!(
                        "\nfunc RadarCommGo_{cycle}_{sub}(v int) int {{\n\treturn v + {}\n}}\n",
                        cycle * 10 + sub
                    ),
                ),
                _ => (
                    "py/pipeline/transforms.py".to_string(),
                    format!(
                        "\ndef radar_comm_py_{cycle}_{sub}(v: int) -> int:\n    return v + {}\n",
                        cycle * 10 + sub
                    ),
                ),
            };
            let full = vp_path.join(&rel);
            let mut src = std::fs::read_to_string(&full)?;
            src.push_str(&addition);

            wait_for_updated_timestamp_gt(prev.updated_at_ms);
            write_text_file(&full, &src)?;
            let (_rep, dur) = wait_until_radar(vp_path, Duration::from_secs(10), |r| {
                r.updated_at_ms > prev.updated_at_ms && !r.pending_syntax
            })?;
            samples_ms.push(dur.as_secs_f64() * 1000.0);
            counts.commutative_insert_member += 1;
        }

        if samples_ms.len() >= target_trials {
            break;
        }

        // 2. PENDING_SYNTAX guard (false -> true -> false)
        {
            let vp_idx = cycle % n_viewports;
            let vp_path = &viewports[vp_idx].1;
            let route_file =
                vp_path.join(format!("crates/api_gateway/src/routes/route_{:02}.rs", vp_idx + 1));
            let orig = std::fs::read_to_string(&route_file)?;
            let broken = format!("{orig}\npub fn broken_radar_probe_{cycle}(\n");

            write_text_file(&route_file, &broken)?;
            let (rep_pending, dur_enter) =
                wait_until_radar(vp_path, Duration::from_secs(10), |r| {
                    r.pending_syntax && !r.pending_syntax_files.is_empty()
                })?;
            anyhow::ensure!(
                rep_pending.status != "RED",
                "PENDING_SYNTAX triggered false-positive RED status"
            );
            samples_ms.push(dur_enter.as_secs_f64() * 1000.0);
            counts.pending_syntax_guard += 1;

            if samples_ms.len() < target_trials {
                write_text_file(&route_file, &orig)?;
                let (_rep_clear, dur_exit) =
                    wait_until_radar(vp_path, Duration::from_secs(10), |r| {
                        !r.pending_syntax && r.pending_syntax_files.is_empty()
                    })?;
                samples_ms.push(dur_exit.as_secs_f64() * 1000.0);
                counts.pending_syntax_guard += 1;
            } else {
                write_text_file(&route_file, &orig)?;
                let _ = wait_until_radar(vp_path, Duration::from_secs(10), |r| !r.pending_syntax)?;
            }
        }

        if samples_ms.len() >= target_trials {
            break;
        }

        // 3. StructuralConflict (GREEN -> RED -> GREEN)
        {
            let p0 = viewports[0].1.join("crates/core_engine/src/lib.rs");
            let p1 = viewports[1].1.join("crates/core_engine/src/lib.rs");
            let orig0 = std::fs::read_to_string(&p0)?;
            let orig1 = std::fs::read_to_string(&p1)?;
            let needle = "        let raw = (weight * self.factor) as i64 + self.bias;\n        raw - penalty";
            let edit0 = orig0.replacen(
                needle,
                &format!(
                    "        let raw = (weight * self.factor) as i64 + self.bias;\n        raw - penalty + {}",
                    1000 + cycle
                ),
                1,
            );
            let edit1 = orig1.replacen(
                needle,
                &format!(
                    "        let raw = (weight * self.factor) as i64 + self.bias;\n        raw - penalty + {}",
                    2000 + cycle
                ),
                1,
            );

            let prev0 = read_radar_status(&viewports[0].1).context("missing vp0 status")?;
            wait_for_updated_timestamp_gt(prev0.updated_at_ms);
            write_text_file(&p0, &edit0)?;
            let _ = wait_until_radar(&viewports[0].1, Duration::from_secs(10), |r| {
                r.updated_at_ms > prev0.updated_at_ms && !r.pending_syntax
            })?;

            write_text_file(&p1, &edit1)?;
            let (_rep_red, dur_red) =
                wait_until_radar(&viewports[1].1, Duration::from_secs(10), |r| {
                    r.status == "RED"
                        && r.conflicts
                            .iter()
                            .any(|c| c.kind == "structural_conflict")
                })?;
            samples_ms.push(dur_red.as_secs_f64() * 1000.0);
            counts.structural_conflict += 1;

            if samples_ms.len() < target_trials {
                write_text_file(&p1, &orig1)?;
                let (_rep_resolved, dur_rec) =
                    wait_until_radar(&viewports[1].1, Duration::from_secs(10), |r| {
                        r.status != "RED"
                            && !r.conflicts
                                .iter()
                                .any(|c| c.kind == "structural_conflict")
                    })?;
                samples_ms.push(dur_rec.as_secs_f64() * 1000.0);
                counts.structural_conflict += 1;
            } else {
                write_text_file(&p1, &orig1)?;
                let _ = wait_until_radar(&viewports[1].1, Duration::from_secs(10), |r| {
                    r.status != "RED"
                        && !r.conflicts
                            .iter()
                            .any(|c| c.kind == "structural_conflict")
                })?;
            }
            let prev0_after = read_radar_status(&viewports[0].1).context("missing vp0 status")?;
            wait_for_updated_timestamp_gt(prev0_after.updated_at_ms);
            write_text_file(&p0, &orig0)?;
            let _ = wait_until_radar(&viewports[0].1, Duration::from_secs(10), |r| {
                r.updated_at_ms > prev0_after.updated_at_ms && r.status == "GREEN"
            })?;
            let _ = wait_until_radar(&viewports[1].1, Duration::from_secs(10), |r| {
                r.status == "GREEN"
            })?;
        }

        if samples_ms.len() >= target_trials {
            break;
        }

        // 4. SemanticConflict (GREEN -> RED -> GREEN)
        {
            let p_consumer = viewports[2].1.join("ts/src/billing/calc.ts");
            let p_producer = viewports[3].1.join("ts/src/billing/formatter.ts");
            let orig_consumer = std::fs::read_to_string(&p_consumer)?;
            let orig_producer = std::fs::read_to_string(&p_producer)?;

            let edit_consumer = orig_consumer.replacen(
                "return `${label}|tax=${tax}`;",
                &format!("return `${{label}}|tax=${{tax}}|c={cycle}`;"),
                1,
            );
            let edit_producer = orig_producer.replacen(
                "export function formatInvoice(spec: InvoiceSpec): string {",
                "export function formatInvoice(spec: InvoiceSpec, locale: string): string {",
                1,
            );

            let prev2 = read_radar_status(&viewports[2].1).context("missing vp2 status")?;
            wait_for_updated_timestamp_gt(prev2.updated_at_ms);
            write_text_file(&p_consumer, &edit_consumer)?;
            let _ = wait_until_radar(&viewports[2].1, Duration::from_secs(10), |r| {
                r.updated_at_ms > prev2.updated_at_ms && !r.pending_syntax
            })?;

            write_text_file(&p_producer, &edit_producer)?;
            let (_rep_red, dur_red) =
                wait_until_radar(&viewports[3].1, Duration::from_secs(10), |r| {
                    r.status == "RED"
                        && r.conflicts
                            .iter()
                            .any(|c| c.kind == "semantic_conflict")
                })?;
            samples_ms.push(dur_red.as_secs_f64() * 1000.0);
            counts.semantic_conflict += 1;

            if samples_ms.len() < target_trials {
                write_text_file(&p_producer, &orig_producer)?;
                let (_rep_rec, dur_rec) =
                    wait_until_radar(&viewports[3].1, Duration::from_secs(10), |r| {
                        r.status != "RED"
                    })?;
                samples_ms.push(dur_rec.as_secs_f64() * 1000.0);
                counts.semantic_conflict += 1;
            } else {
                write_text_file(&p_producer, &orig_producer)?;
                let _ = wait_until_radar(&viewports[3].1, Duration::from_secs(10), |r| {
                    r.status != "RED"
                })?;
            }
            let prev2_after = read_radar_status(&viewports[2].1).context("missing vp2 status")?;
            wait_for_updated_timestamp_gt(prev2_after.updated_at_ms);
            write_text_file(&p_consumer, &orig_consumer)?;
            let _ = wait_until_radar(&viewports[2].1, Duration::from_secs(10), |r| {
                r.updated_at_ms > prev2_after.updated_at_ms && r.status == "GREEN"
            })?;
        }

        if samples_ms.len() >= target_trials {
            break;
        }

        // 5. Advisory YELLOW: consumed_symbol_modified + crdt_concurrent_edit
        {
            let p_py = viewports[4].1.join("py/pipeline/transforms.py");
            let orig_py = std::fs::read_to_string(&p_py)?;
            let edit_py = orig_py.replacen(
                "def normalize_records(rows: list[dict], scale: float) -> list[dict]:",
                "def normalize_records(rows: list[dict], scale: float, strict: bool) -> list[dict]:",
                1,
            );
            write_text_file(&p_py, &edit_py)?;
            let (_rep_yel, dur_yel) =
                wait_until_radar(&viewports[4].1, Duration::from_secs(10), |r| {
                    r.status == "YELLOW"
                        && r.conflicts
                            .iter()
                            .any(|c| c.kind == "consumed_symbol_modified")
                })?;
            samples_ms.push(dur_yel.as_secs_f64() * 1000.0);
            counts.yellow_advisory += 1;

            write_text_file(&p_py, &orig_py)?;
            let _ = wait_until_radar(&viewports[4].1, Duration::from_secs(10), |r| {
                r.status == "GREEN"
            })?;
        }

        if samples_ms.len() >= target_trials {
            break;
        }

        {
            let j0 = viewports[0].1.join("config/services.json");
            let j1 = viewports[1].1.join("config/services.json");
            let orig0 = std::fs::read_to_string(&j0)?;
            let orig1 = std::fs::read_to_string(&j1)?;
            let edit0 = orig0.replacen(
                "\"svc_01\": {\"enabled\": true, \"replicas\": 1}",
                &format!("\"svc_01\": {{\"enabled\": true, \"replicas\": {}}}", 2 + cycle),
                1,
            );
            let edit1 = orig1.replacen(
                "\"svc_02\": {\"enabled\": true, \"replicas\": 1}",
                &format!("\"svc_02\": {{\"enabled\": true, \"replicas\": {}}}", 3 + cycle),
                1,
            );
            let prev0 = read_radar_status(&viewports[0].1).context("missing vp0 status")?;
            wait_for_updated_timestamp_gt(prev0.updated_at_ms);
            write_text_file(&j0, &edit0)?;
            let _ = wait_until_radar(&viewports[0].1, Duration::from_secs(10), |r| {
                r.updated_at_ms > prev0.updated_at_ms
            })?;

            write_text_file(&j1, &edit1)?;
            let (_rep_crdt, dur_crdt) =
                wait_until_radar(&viewports[1].1, Duration::from_secs(10), |r| {
                    r.status == "YELLOW"
                        && r.conflicts
                            .iter()
                            .any(|c| c.kind == "crdt_concurrent_edit")
                })?;
            samples_ms.push(dur_crdt.as_secs_f64() * 1000.0);
            counts.yellow_advisory += 1;

            write_text_file(&j1, &orig1)?;
            let _ = wait_until_radar(&viewports[1].1, Duration::from_secs(10), |r| {
                r.status == "GREEN"
            })?;
            let prev0_after = read_radar_status(&viewports[0].1).context("missing vp0 status")?;
            wait_for_updated_timestamp_gt(prev0_after.updated_at_ms);
            write_text_file(&j0, &orig0)?;
            let _ = wait_until_radar(&viewports[0].1, Duration::from_secs(10), |r| {
                r.updated_at_ms > prev0_after.updated_at_ms && r.status == "GREEN"
            })?;
        }
    }

    let snap_after = cli.radar_snapshot(&fx.repo_root)?;
    anyhow::ensure!(
        snap_after.sequence > snap_before.sequence,
        "expected radar sequence to advance ({} -> {})",
        snap_before.sequence,
        snap_after.sequence
    );

    let memory = if capture_memory {
        Some(measure_memory_on_active_repo(
            cli,
            &fx.repo_root,
            daemon_pid,
            &viewports,
        )?)
    } else {
        None
    };

    guard.shutdown_and_remove_repo(&fx.repo_root, &tmp);

    let radar_metrics = RadarRunMetrics {
        active_viewports: n_viewports,
        stats: LatencyStats::from_ms(&samples_ms),
        samples_ms,
        scenario_counts: counts,
        initial_sequence: snap_before.sequence,
        final_sequence: snap_after.sequence,
        batch_mode_triggered: snap_after.metrics.batch_switches > 0 || snap_after.batch_mode,
    };

    Ok((radar_metrics, memory))
}

fn measure_memory_on_active_repo(
    cli: &LoomCli,
    repo_root: &Path,
    daemon_pid: u32,
    viewports: &[(String, PathBuf)],
) -> anyhow::Result<MemorySuiteMetrics> {
    // Settle into quiet idle window (500ms) with 10 active viewports
    thread::sleep(Duration::from_millis(500));
    let mut idle_ws = Vec::with_capacity(10);
    let mut idle_priv = Vec::with_capacity(10);
    for _ in 0..10 {
        let s = win32::sample_process_memory(daemon_pid)?;
        idle_ws.push(bytes_to_mib(s.working_set_bytes));
        idle_priv.push(bytes_to_mib(s.private_bytes));
        thread::sleep(Duration::from_millis(100));
    }

    // Peak stress: 5ms polling during concurrent 10-viewport edits -> N×N shadow merges -> CST diffs -> seals
    let stop = Arc::new(AtomicBool::new(false));
    let stop_c = Arc::clone(&stop);
    let sampler = thread::spawn(move || -> Vec<ProcessMemorySample> {
        let mut out = Vec::new();
        while !stop_c.load(Ordering::Relaxed) {
            if let Ok(s) = win32::sample_process_memory(daemon_pid) {
                out.push(s);
            }
            thread::sleep(Duration::from_millis(5));
        }
        out
    });

    // Trigger concurrent multi-language edits across all 10 viewports in parallel bursts
    for wave in 0..3 {
        let barrier = Arc::new(Barrier::new(viewports.len()));
        let mut workers = Vec::with_capacity(viewports.len());
        for (idx, (_name, vp_path)) in viewports.iter().enumerate() {
            let b = Arc::clone(&barrier);
            let p = vp_path.clone();
            workers.push(thread::spawn(move || -> anyhow::Result<()> {
                b.wait();
                let rs_path = p.join("crates/core_engine/src/lib.rs");
                let mut rs = std::fs::read_to_string(&rs_path)?;
                rs.push_str(&format!(
                    "\npub fn mem_stress_rs_{wave}_{idx}(x: u64) -> u64 {{ x + {} }}\n",
                    wave * 100 + idx
                ));
                write_text_file(&rs_path, &rs)?;

                let ts_path = p.join("ts/src/billing/formatter.ts");
                let mut ts = std::fs::read_to_string(&ts_path)?;
                ts.push_str(&format!(
                    "\nexport function memStressTs_{wave}_{idx}(x: number): number {{ return x + {}; }}\n",
                    wave * 100 + idx
                ));
                write_text_file(&ts_path, &ts)?;

                let json_path = p.join("config/services.json");
                let json = std::fs::read_to_string(&json_path)?;
                let key = format!("\"svc_{:02}\": {{\"enabled\": true, \"replicas\": ", idx + 1);
                let updated_json = json.replacen(
                    &format!("{key}1}}"),
                    &format!("{key}{}}}", 10 + wave),
                    1,
                );
                write_text_file(&json_path, &updated_json)?;
                Ok(())
            }));
        }
        for w in workers {
            w.join()
                .map_err(|_| anyhow::anyhow!("memory stress worker panicked"))??;
        }
        for (_name, vp_path) in viewports {
            let _ = wait_until_radar(vp_path, Duration::from_secs(10), |r| !r.pending_syntax)?;
        }
    }

    // Seal all 10 viewports sequentially to stress 3-way CST/CRDT merge & CAS compression
    for (idx, (name, _vp_path)) in viewports.iter().enumerate() {
        let _ = cli.seal(
            repo_root,
            name,
            &format!("memory peak stress seal {idx}"),
            Some(&format!("echo peak-ok-{idx}")),
        )?;
    }

    stop.store(true, Ordering::Relaxed);
    let peak_samples = sampler
        .join()
        .map_err(|_| anyhow::anyhow!("memory sampler panicked"))?;

    let peak_ws = peak_samples
        .iter()
        .map(|s| bytes_to_mib(s.working_set_bytes))
        .fold(0.0_f64, f64::max);
    let peak_priv = peak_samples
        .iter()
        .map(|s| bytes_to_mib(s.private_bytes))
        .fold(0.0_f64, f64::max);
    let os_peak_ws = peak_samples
        .iter()
        .map(|s| bytes_to_mib(s.peak_working_set_bytes))
        .fold(0.0_f64, f64::max);

    Ok(MemorySuiteMetrics {
        daemon_pid,
        active_viewports: viewports.len(),
        idle_sample_count: idle_ws.len(),
        idle_working_set_mib: median_f64(&idle_ws),
        idle_private_bytes_mib: median_f64(&idle_priv),
        peak_sample_count: peak_samples.len(),
        peak_working_set_mib: peak_ws,
        peak_private_bytes_mib: peak_priv,
        os_peak_working_set_mib: os_peak_ws,
    })
}

fn dir_logical_bytes_and_count(root: &Path) -> anyhow::Result<(u64, usize)> {
    let mut total_bytes = 0u64;
    let mut file_count = 0usize;
    if !root.exists() {
        return Ok((0, 0));
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let meta = entry.metadata()?;
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() {
                total_bytes += meta.len();
                file_count += 1;
            }
        }
    }
    Ok((total_bytes, file_count))
}

fn apply_convergent_round1_edits(vp_path: &Path, idx: usize) -> anyhow::Result<u64> {
    let mut raw_bytes = 0u64;

    let rs_lib = vp_path.join("crates/core_engine/src/lib.rs");
    let mut rs = std::fs::read_to_string(&rs_lib)?;
    rs.push_str(&format!(
        "\npub fn round1_engine_metric_{idx:02}(v: u64) -> u64 {{\n    v.wrapping_mul({}).wrapping_add({idx})\n}}\n",
        idx + 3
    ));
    write_text_file(&rs_lib, &rs)?;
    raw_bytes += rs.len() as u64;

    let route = vp_path.join(format!(
        "crates/api_gateway/src/routes/route_{:02}.rs",
        idx + 1
    ));
    let route_src = std::fs::read_to_string(&route)?;
    let route_updated = route_src.replacen(
        &format!("load + {}", idx + 1),
        &format!("load + {} + 10", idx + 1),
        1,
    );
    write_text_file(&route, &route_updated)?;
    raw_bytes += route_updated.len() as u64;

    let ts_fmt = vp_path.join("ts/src/billing/formatter.ts");
    let mut ts = std::fs::read_to_string(&ts_fmt)?;
    ts.push_str(&format!(
        "\nexport function formatTag_{idx:02}(id: string): string {{\n  return `tag-{idx:02}:${{id}}`;\n}}\n"
    ));
    write_text_file(&ts_fmt, &ts)?;
    raw_bytes += ts.len() as u64;

    let go_sched = vp_path.join("go/pkg/scheduler/scheduler.go");
    let mut go = std::fs::read_to_string(&go_sched)?;
    go.push_str(&format!(
        "\nfunc ValidateRule{idx:02}(score int) bool {{\n\treturn score >= {idx}\n}}\n"
    ));
    write_text_file(&go_sched, &go)?;
    raw_bytes += go.len() as u64;

    let py_tx = vp_path.join("py/pipeline/transforms.py");
    let mut py = std::fs::read_to_string(&py_tx)?;
    py.push_str(&format!(
        "\ndef transform_step_{idx:02}(val: float) -> float:\n    return val * {}.5\n",
        idx + 1
    ));
    write_text_file(&py_tx, &py)?;
    raw_bytes += py.len() as u64;

    let json_path = vp_path.join("config/services.json");
    let json = std::fs::read_to_string(&json_path)?;
    let svc_old = format!("\"svc_{:02}\": {{\"enabled\": true, \"replicas\": 1}}", idx + 1);
    let svc_new = format!(
        "\"svc_{:02}\": {{\"enabled\": true, \"replicas\": {}}}",
        idx + 1,
        idx + 2
    );
    let json_updated = json.replacen(&svc_old, &svc_new, 1);
    write_text_file(&json_path, &json_updated)?;
    raw_bytes += json_updated.len() as u64;

    let md_path = vp_path.join("CHANGELOG.md");
    let md = std::fs::read_to_string(&md_path)?;
    let md_old = format!("## Agent {:02}\n- Initial baseline entry for agent {:02}.", idx + 1, idx + 1);
    let md_new = format!(
        "## Agent {:02}\n- Initial baseline entry for agent {:02}.\n- Round 1 multi-language refactor completed.",
        idx + 1,
        idx + 1
    );
    let md_updated = md.replacen(&md_old, &md_new, 1);
    write_text_file(&md_path, &md_updated)?;
    raw_bytes += md_updated.len() as u64;

    Ok(raw_bytes)
}

fn apply_convergent_round2_edits(vp_path: &Path, idx: usize) -> anyhow::Result<u64> {
    let mut raw_bytes = 0u64;

    let rs_lib = vp_path.join("crates/core_engine/src/lib.rs");
    let rs = std::fs::read_to_string(&rs_lib)?;
    let rs_old = format!("v.wrapping_mul({}).wrapping_add({idx})", idx + 3);
    let rs_new = format!("v.wrapping_mul({}).wrapping_add({})", idx + 5, idx * 2 + 1);
    let rs_updated = rs.replacen(&rs_old, &rs_new, 1);
    write_text_file(&rs_lib, &rs_updated)?;
    raw_bytes += rs_updated.len() as u64;

    let ts_fmt = vp_path.join("ts/src/billing/formatter.ts");
    let ts = std::fs::read_to_string(&ts_fmt)?;
    let ts_old = format!("return `tag-{idx:02}:${{id}}`;");
    let ts_new = format!("return `v2-tag-{idx:02}:${{id}}`;");
    let ts_updated = ts.replacen(&ts_old, &ts_new, 1);
    write_text_file(&ts_fmt, &ts_updated)?;
    raw_bytes += ts_updated.len() as u64;

    let go_sched = vp_path.join("go/pkg/scheduler/scheduler.go");
    let go = std::fs::read_to_string(&go_sched)?;
    let go_old = format!("return score >= {idx}");
    let go_new = format!("return score >= {}", idx + 10);
    let go_updated = go.replacen(&go_old, &go_new, 1);
    write_text_file(&go_sched, &go_updated)?;
    raw_bytes += go_updated.len() as u64;

    let py_tx = vp_path.join("py/pipeline/transforms.py");
    let py = std::fs::read_to_string(&py_tx)?;
    let py_old = format!("return val * {}.5", idx + 1);
    let py_new = format!("return val * {}.75", idx + 2);
    let py_updated = py.replacen(&py_old, &py_new, 1);
    write_text_file(&py_tx, &py_updated)?;
    raw_bytes += py_updated.len() as u64;

    let readme = vp_path.join("README.md");
    let mut md = std::fs::read_to_string(&readme)?;
    md.push_str(&format!("\n- Round 2 seal note from agent {idx:02}.\n"));
    write_text_file(&readme, &md)?;
    raw_bytes += md.len() as u64;

    Ok(raw_bytes)
}

pub fn measure_storage_suite(
    cli: &LoomCli,
    guard: &mut BenchWorkspaceGuard,
    n_viewports: usize,
) -> anyhow::Result<StorageRunMetrics> {
    let tmp = guard.create_temp_dir(&format!("storage_n{n_viewports}"))?;
    let fx = fixture::create_fixture_repo(&tmp, true)?;
    guard.register_loom_repo(fx.repo_root.clone());
    let _ = cli.init_colocate_git(&fx.repo_root)?;
    let st0 = cli.status(&fx.repo_root)?;
    guard.track_pid(st0.pid);

    let mut raw_capsule_source_bytes = 0u64;
    let mut sealed_capsules = 0usize;
    let mut dropped_negative_edges = 0usize;

    // Round 1: N viewports edit shared & distinct multi-language files + CRDT, then seal
    let mut r1_vps = Vec::with_capacity(n_viewports);
    for i in 0..n_viewports {
        let name = format!("vp-r1-{i:02}");
        let (vp, _) = cli.spawn_timed(&fx.repo_root, &name, &format!("round 1 agent {i:02}"))?;
        let _ = wait_until_radar(&vp.path, Duration::from_secs(10), |r| !r.pending_syntax)?;
        r1_vps.push((vp.name, vp.path));
    }

    let barrier1 = Arc::new(Barrier::new(n_viewports));
    let mut h1 = Vec::with_capacity(n_viewports);
    for (idx, (_name, vp_path)) in r1_vps.iter().enumerate() {
        let b = Arc::clone(&barrier1);
        let p = vp_path.clone();
        h1.push(thread::spawn(move || -> anyhow::Result<u64> {
            b.wait();
            apply_convergent_round1_edits(&p, idx)
        }));
    }
    for h in h1 {
        raw_capsule_source_bytes += h
            .join()
            .map_err(|_| anyhow::anyhow!("round 1 edit worker panicked"))??;
    }

    for (_name, vp_path) in &r1_vps {
        let _ = wait_until_radar(vp_path, Duration::from_secs(10), |r| {
            !r.pending_syntax && r.status != "RED"
        })?;
    }
    for (idx, (name, _vp_path)) in r1_vps.iter().enumerate() {
        let _cap = cli.seal(
            &fx.repo_root,
            name,
            &format!("round 1 refactor agent {idx:02}"),
            Some(&format!("echo verified-r1-{idx:02}")),
        )?;
        sealed_capsules += 1;
    }
    let _ = cli.gc_prune(&fx.repo_root)?;

    // Round 2: N viewports perform ReplaceBody refactors; drop 1 (N=5) or 2 (N=10) as NegativeEdge, seal rest
    let drop_count = if n_viewports >= 10 { 2 } else { 1 };
    let mut r2_vps = Vec::with_capacity(n_viewports);
    for i in 0..n_viewports {
        let name = format!("vp-r2-{i:02}");
        let (vp, _) = cli.spawn_timed(&fx.repo_root, &name, &format!("round 2 agent {i:02}"))?;
        let _ = wait_until_radar(&vp.path, Duration::from_secs(10), |r| !r.pending_syntax)?;
        r2_vps.push((vp.name, vp.path));
    }

    let barrier2 = Arc::new(Barrier::new(n_viewports));
    let mut h2 = Vec::with_capacity(n_viewports);
    for (idx, (_name, vp_path)) in r2_vps.iter().enumerate() {
        let b = Arc::clone(&barrier2);
        let p = vp_path.clone();
        h2.push(thread::spawn(move || -> anyhow::Result<u64> {
            b.wait();
            apply_convergent_round2_edits(&p, idx)
        }));
    }
    for h in h2 {
        raw_capsule_source_bytes += h
            .join()
            .map_err(|_| anyhow::anyhow!("round 2 edit worker panicked"))??;
    }

    for (_name, vp_path) in &r2_vps {
        let _ = wait_until_radar(vp_path, Duration::from_secs(10), |r| !r.pending_syntax)?;
    }

    for (idx, (name, _vp_path)) in r2_vps.iter().enumerate() {
        if idx >= n_viewports - drop_count {
            let _ = cli.drop_viewport(
                &fx.repo_root,
                name,
                "SemanticConflict",
                &format!(
                    "round 2 agent {idx:02} reverted experimental weight multiplier due to downstream mismatch"
                ),
            )?;
            dropped_negative_edges += 1;
        } else {
            let _cap = cli.seal(
                &fx.repo_root,
                name,
                &format!("round 2 refactor agent {idx:02}"),
                Some(&format!("echo verified-r2-{idx:02}")),
            )?;
            sealed_capsules += 1;
        }
    }
    let _ = cli.gc_prune(&fx.repo_root)?;

    let push_res = cli.git_push(&fx.repo_root, "origin", "main")?;
    let status_after = cli.status(&fx.repo_root)?;

    let (cas_bytes, cas_object_count) =
        dir_logical_bytes_and_count(&fx.repo_root.join(".loom").join("cas"))?;
    let (git_objects_bytes, git_object_count) =
        dir_logical_bytes_and_count(&fx.repo_root.join(".git").join("objects"))?;
    let (bare_objects_bytes, _bare_count) =
        dir_logical_bytes_and_count(&fx.bare_remote.join("objects"))?;

    // Secondary context: measure packed `.git/objects/` after `git gc` (after primary loose measurement)
    let _ = win32::run_git(&fx.repo_root, &["gc", "--quiet"])?;
    let (git_objects_packed_bytes, _) =
        dir_logical_bytes_and_count(&fx.repo_root.join(".git").join("objects"))?;

    let cas_to_git_loose_ratio = if git_objects_bytes > 0 {
        cas_bytes as f64 / git_objects_bytes as f64
    } else {
        0.0
    };
    let cas_to_git_packed_ratio = if git_objects_packed_bytes > 0 {
        cas_bytes as f64 / git_objects_packed_bytes as f64
    } else {
        0.0
    };

    guard.shutdown_and_remove_repo(&fx.repo_root, &tmp);

    Ok(StorageRunMetrics {
        viewport_count: n_viewports,
        sealed_capsules,
        dropped_negative_edges,
        pushed_commits: push_res.commits_created,
        synced_negative_edges: push_res.synced_negative_edges,
        cas_object_count,
        cas_bytes,
        status_reported_cas_bytes: status_after.cas_total_bytes,
        git_object_count,
        git_objects_bytes,
        bare_objects_bytes,
        git_objects_packed_bytes,
        raw_capsule_source_bytes,
        cas_to_git_loose_ratio,
        cas_to_git_packed_ratio,
    })
}
