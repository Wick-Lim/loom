pub mod drop;
pub mod git;
pub mod seal;

pub use drop::cmd_drop;
pub use git::{cmd_git_pull, cmd_git_push};
pub use seal::cmd_seal;

use crate::client::{call_rpc, connect_or_autostart, discover_repo_root, subscribe_radar};
use loom_core::model::ViewportState;
use loom_core::rpc::{RadarSnapshot, RpcMethod, RpcPayload};
use loom_daemon::mcp::{McpServerConfig, serve_mcp_http, serve_mcp_stdio};
use std::io::Write;
use std::path::{Path, PathBuf};

fn normalize_path_for_match(p: &Path) -> String {
    let canon = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let raw = canon.to_string_lossy();
    let stripped = if let Some(rest) = raw.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = raw.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        raw.into_owned()
    };
    let mut norm = stripped.replace('\\', "/");
    while norm.len() > 1 && norm.ends_with('/') {
        norm.pop();
    }
    if cfg!(windows) {
        norm.to_lowercase()
    } else {
        norm
    }
}

pub(crate) fn resolve_repo_root(repo: Option<&Path>) -> anyhow::Result<PathBuf> {
    if let Some(r) = repo {
        return discover_repo_root(Some(r), false);
    }
    if let Some(env_repo) = std::env::var_os("LOOM_REPO_ROOT").filter(|s| !s.is_empty()) {
        return discover_repo_root(Some(Path::new(&env_repo)), false);
    }
    discover_repo_root(None, false)
}

pub(crate) async fn resolve_target_viewport(
    root: &Path,
    explicit: Option<String>,
) -> anyhow::Result<String> {
    if let Some(vp) = explicit
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        return Ok(vp);
    }

    if let Ok(env_vp) = std::env::var("LOOM_VIEWPORT") {
        let trimmed = env_vp.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }

    let payload = call_rpc(root, RpcMethod::ListViewports { include_gc: false }).await?;
    let viewports = match payload {
        RpcPayload::Viewports(vps) => vps,
        other => anyhow::bail!("unexpected RPC response: {other:?}"),
    };
    let active: Vec<_> = viewports
        .into_iter()
        .filter(|v| v.state.is_active())
        .collect();

    let candidate_dir = std::env::var_os("LOOM_VIEWPORT_ROOT")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok());

    if let Some(dir) = candidate_dir {
        let dir_norm = normalize_path_for_match(&dir);
        for vp in &active {
            let vp_norm = normalize_path_for_match(&vp.path);
            if dir_norm == vp_norm
                || (dir_norm.starts_with(&vp_norm)
                    && dir_norm.as_bytes().get(vp_norm.len()) == Some(&b'/'))
            {
                return Ok(vp.id.clone());
            }
        }
        let vps_dir_norm = normalize_path_for_match(&root.join(".loom").join("viewports"));
        if let Some(rest) = dir_norm
            .strip_prefix(&vps_dir_norm)
            .and_then(|r| r.strip_prefix('/'))
            && let Some(vp_seg) = rest.split('/').next()
            && !vp_seg.is_empty()
        {
            if let Some(matched) = active.iter().find(|v| {
                if cfg!(windows) {
                    v.name.eq_ignore_ascii_case(vp_seg)
                } else {
                    v.name == vp_seg
                }
            }) {
                return Ok(matched.id.clone());
            }
            return Ok(vp_seg.to_string());
        }
    }

    if active.len() == 1 {
        return Ok(active[0].id.clone());
    }
    if active.is_empty() {
        anyhow::bail!("no active viewports found; pass --viewport <name>");
    }
    let names: Vec<String> = active.iter().map(|v| v.name.clone()).collect();
    anyhow::bail!(
        "multiple active viewports ({}); pass --viewport <name>",
        names.join(", ")
    );
}

pub async fn cmd_init(
    repo: Option<&Path>,
    colocate_git: bool,
    json: bool,
) -> anyhow::Result<()> {
    let root = discover_repo_root(repo, true)?;
    let payload = call_rpc(&root, RpcMethod::Init { colocate_git }).await?;
    match payload {
        RpcPayload::Init(res) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&res)?);
            } else {
                println!(
                    "Initialized Loom repository at {} (id={}, colocate_git={})",
                    res.repo_root.display(),
                    res.repo_id,
                    res.colocate_git
                );
                if let Some(oid) = &res.seeded_head_oid {
                    println!(
                        "Seeded git HEAD {} ({})",
                        oid,
                        res.branch.as_deref().unwrap_or("detached")
                    );
                }
            }
            Ok(())
        }
        other => anyhow::bail!("unexpected RPC response: {other:?}"),
    }
}

pub async fn cmd_list(repo: Option<&Path>, include_gc: bool, json: bool) -> anyhow::Result<()> {
    let root = discover_repo_root(repo, false)?;
    let payload = call_rpc(&root, RpcMethod::ListViewports { include_gc }).await?;
    match payload {
        RpcPayload::Viewports(vps) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&vps)?);
            } else if vps.is_empty() {
                println!("No viewports found.");
            } else {
                println!(
                    "{:<16} {:<16} {:<12} {:<24} {:<36} INTENT",
                    "ID", "NAME", "STATE", "DRIVER", "PATH"
                );
                for vp in vps {
                    println!(
                        "{:<16} {:<16} {:<12} {:<24} {:<36} {}",
                        vp.id,
                        vp.name,
                        vp.state,
                        vp.driver,
                        vp.path.display(),
                        vp.intent
                    );
                }
            }
            Ok(())
        }
        other => anyhow::bail!("unexpected RPC response: {other:?}"),
    }
}

pub async fn cmd_status(repo: Option<&Path>, json: bool) -> anyhow::Result<()> {
    let root = discover_repo_root(repo, false)?;
    let payload = call_rpc(&root, RpcMethod::Status).await?;
    match payload {
        RpcPayload::Status(st) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&st)?);
            } else {
                println!("Loom Daemon Status");
                println!("  PID:              {}", st.pid);
                println!("  Transport:        {} ({})", st.transport, st.endpoint);
                println!("  Uptime:           {} ms", st.uptime_ms);
                println!("  Idle Timeout:     {} ms", st.idle_timeout_ms);
                println!(
                    "  Idle Remaining:   {}",
                    st.idle_remaining_ms
                        .map_or_else(|| "paused (active viewports)".to_string(), |ms| format!("{ms} ms"))
                );
                println!(
                    "  Viewports:        {} active / {} total",
                    st.active_viewports, st.total_viewports
                );
                if let Some(driver) = st.default_driver {
                    println!("  Default Driver:   {driver}");
                }
                println!("  Latest Op Seq:    {}", st.latest_op_seq);
                println!(
                    "  WAL LSN:          applied={}, checkpoint={}",
                    st.last_applied_lsn, st.last_checkpoint_lsn
                );
                println!(
                    "  CAS Objects:      {} ({} bytes)",
                    st.cas_object_count, st.cas_total_bytes
                );
                println!("  Colocate Git:     {}", st.colocate_git);
                if !st.viewports.is_empty() {
                    println!("  Active/Live Viewports:");
                    for vp in &st.viewports {
                        println!(
                            "    - {} ({}) [{}] driver={} path={} intent={:?}",
                            vp.name,
                            vp.id,
                            vp.state,
                            vp.driver,
                            vp.path.display(),
                            vp.intent
                        );
                    }
                }
            }
            Ok(())
        }
        other => anyhow::bail!("unexpected RPC response: {other:?}"),
    }
}

pub async fn cmd_log(
    repo: Option<&Path>,
    capsules_only: bool,
    symbol: Option<String>,
    limit: Option<usize>,
    json: bool,
) -> anyhow::Result<()> {
    let root = discover_repo_root(repo, false)?;
    let payload = call_rpc(
        &root,
        RpcMethod::Log {
            capsules_only,
            symbol,
            limit,
        },
    )
    .await?;
    match payload {
        RpcPayload::Log(entries) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else if entries.is_empty() {
                println!("No operations recorded.");
            } else {
                for e in entries {
                    let undone_tag = e
                        .undone_by_op
                        .map_or_else(String::new, |u| format!(" [undone by #{u}]"));
                    let cap_tag = match e.capsule_summary.as_deref().map(str::trim) {
                        Some(cs) if !cs.is_empty() && !e.summary.contains(cs) => {
                            format!(" ({cs})")
                        }
                        _ => String::new(),
                    };
                    println!(
                        "#{} (lsn={}) [{}] {}{}{}",
                        e.op_seq, e.lsn, e.kind, e.summary, cap_tag, undone_tag
                    );
                }
            }
            Ok(())
        }
        other => anyhow::bail!("unexpected RPC response: {other:?}"),
    }
}

pub async fn cmd_undo(repo: Option<&Path>, op: Option<u64>, json: bool) -> anyhow::Result<()> {
    let root = discover_repo_root(repo, false)?;
    let payload = call_rpc(&root, RpcMethod::Undo { op_seq: op }).await?;
    match payload {
        RpcPayload::Undo(res) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&res)?);
            } else {
                println!(
                    "Undid op #{} via compensating op #{}: {}",
                    res.undone_op_seq, res.compensating_op_seq, res.summary
                );
            }
            Ok(())
        }
        other => anyhow::bail!("unexpected RPC response: {other:?}"),
    }
}

pub async fn cmd_gc(
    repo: Option<&Path>,
    prune_viewports: bool,
    json: bool,
) -> anyhow::Result<()> {
    let root = discover_repo_root(repo, false)?;
    let payload = call_rpc(&root, RpcMethod::Gc { prune_viewports }).await?;
    match payload {
        RpcPayload::Gc(res) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&res)?);
            } else {
                println!(
                    "GC complete: transitioned_viewports={}, pruned_dirs={}, removed_cas_blobs={} ({} bytes), compacted_wal_segments={}",
                    res.transitioned_viewports,
                    res.pruned_viewport_dirs,
                    res.removed_cas_blobs,
                    res.reclaimed_cas_bytes,
                    res.compacted_wal_segments
                );
                if !res.pruned_viewport_names.is_empty() {
                    println!("  Pruned viewports: {}", res.pruned_viewport_names.join(", "));
                }
            }
            Ok(())
        }
        other => anyhow::bail!("unexpected RPC response: {other:?}"),
    }
}

#[cfg(windows)]
fn resolve_command_program(program: &str) -> std::ffi::OsString {
    use std::ffi::OsString;
    let p = Path::new(program);
    if p.extension().is_some() {
        return OsString::from(program);
    }
    let pathext_raw =
        std::env::var_os("PATHEXT").unwrap_or_else(|| OsString::from(".COM;.EXE;.BAT;.CMD"));
    let exts: Vec<String> = pathext_raw
        .to_string_lossy()
        .split(';')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if program.contains('\\') || program.contains('/') {
        for ext in &exts {
            let candidate = PathBuf::from(format!("{program}{ext}"));
            if candidate.is_file() {
                return candidate.into_os_string();
            }
        }
        return OsString::from(program);
    }
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            for ext in &exts {
                let candidate = dir.join(format!("{program}{ext}"));
                if candidate.is_file() {
                    return candidate.into_os_string();
                }
            }
        }
    }
    OsString::from(program)
}

#[cfg(not(windows))]
fn resolve_command_program(program: &str) -> std::ffi::OsString {
    std::ffi::OsString::from(program)
}

async fn cleanup_failed_spawn(root: &Path, vp: &loom_core::rpc::ViewportSummary) {
    if call_rpc(
        root,
        RpcMethod::RollbackFailedSpawn {
            viewport_id: vp.id.clone(),
        },
    )
    .await
    .is_err()
    {
        let vp_path = vp.path.clone();
        let driver = vp.driver;
        let mount = vp.mount.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let drv = loom_vfs::driver_for_backend(driver, &[]);
            let _ = drv.teardown_viewport(&vp_path, mount.as_ref());
        })
        .await;
    }
}

pub async fn cmd_spawn(
    repo: Option<&Path>,
    name: String,
    intent: String,
    cmd: Vec<String>,
    json: bool,
) -> anyhow::Result<i32> {
    let root = discover_repo_root(repo, false)?;
    let payload = call_rpc(
        &root,
        RpcMethod::SpawnViewport {
            name,
            intent,
            cmd: cmd.clone(),
        },
    )
    .await?;
    let vp = match payload {
        RpcPayload::Viewport(vp) => vp,
        other => anyhow::bail!("unexpected RPC response: {other:?}"),
    };

    if cmd.is_empty() {
        if json {
            println!("{}", serde_json::to_string_pretty(&vp)?);
        } else {
            println!("Spawned viewport `{}` ({})", vp.name, vp.id);
            println!("  State:  {}", vp.state);
            println!("  Driver: {}", vp.driver);
            println!("  Path:   {}", vp.path.display());
            println!("  Intent: {}", vp.intent);
        }
        return Ok(0);
    }

    let program = resolve_command_program(&cmd[0]);
    let mut child = match std::process::Command::new(&program)
        .args(&cmd[1..])
        .current_dir(&vp.path)
        .env("LOOM_VIEWPORT", &vp.name)
        .env("LOOM_VIEWPORT_ROOT", &vp.path)
        .env("LOOM_REPO_ROOT", &root)
        .spawn()
    {
        Ok(c) => c,
        Err(err) => {
            cleanup_failed_spawn(&root, &vp).await;
            return Err(err.into());
        }
    };

    let child_pid = child.id();
    let _ = call_rpc(
        &root,
        RpcMethod::TransitionViewport {
            viewport_id: vp.id.clone(),
            target_state: ViewportState::Active,
            pid: Some(child_pid),
        },
    )
    .await;

    let status = tokio::task::spawn_blocking(move || child.wait()).await??;
    Ok(status.code().unwrap_or(1))
}

fn print_radar_human(snap: &RadarSnapshot) {
    let mode_label = if snap.batch_mode {
        format!("BATCH MODE ({}ms)", snap.debounce_ms)
    } else {
        format!("INTERACTIVE ({}ms)", snap.debounce_ms)
    };
    println!(
        "Speculative Merge Radar (seq={}, mode={}, switches={}, coalesced={})",
        snap.sequence,
        mode_label,
        snap.metrics.batch_switches,
        snap.metrics.coalesced_events
    );
    if snap.viewports.is_empty() {
        println!("  No active viewports.");
        return;
    }
    println!("  Viewports:");
    for vp in &snap.viewports {
        println!(
            "    - [{}] {} ({}) state={} dirty={} pending_syntax={} warnings={}",
            match vp.level {
                loom_core::model::RadarLevel::Green => "GREEN",
                loom_core::model::RadarLevel::Yellow => "YELLOW",
                loom_core::model::RadarLevel::Red => "RED",
            },
            vp.viewport_name,
            vp.viewport_id,
            vp.state,
            vp.dirty_files.len(),
            vp.pending_syntax_files.len(),
            vp.negative_warnings.len()
        );
        for ps in &vp.pending_syntax_files {
            println!("        PENDING_SYNTAX: {}", ps.display());
        }
        for c in &vp.conflicts {
            println!(
                "        conflict with {} [{}]: {}",
                c.peer_viewport_name, c.kind, c.message
            );
        }
    }
    if !snap.matrix.is_empty() {
        println!("  N×N Matrix:");
        for cell in &snap.matrix {
            println!(
                "    {} <-> {}: {:?}",
                cell.viewport_a_name, cell.viewport_b_name, cell.level
            );
        }
    }
}

fn write_stdout_line_ignoring_broken_pipe(line: &str) -> anyhow::Result<bool> {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    for res in [writeln!(lock, "{line}"), lock.flush()] {
        if let Err(err) = res {
            if err.kind() == std::io::ErrorKind::BrokenPipe || err.raw_os_error() == Some(232) {
                return Ok(false);
            }
            return Err(err.into());
        }
    }
    Ok(true)
}

pub async fn cmd_radar(
    repo: Option<&Path>,
    watch: bool,
    viewport: Option<String>,
    json: bool,
) -> anyhow::Result<()> {
    let root = discover_repo_root(repo, false)?;
    if !watch {
        let payload = call_rpc(
            &root,
            RpcMethod::RadarSnapshot {
                viewport_id: viewport,
            },
        )
        .await?;
        match payload {
            RpcPayload::RadarSnapshot(snap) => {
                if json {
                    let line = serde_json::to_string_pretty(&snap)?;
                    let _ = write_stdout_line_ignoring_broken_pipe(&line)?;
                } else {
                    print_radar_human(&snap);
                }
                return Ok(());
            }
            other => anyhow::bail!("unexpected RPC response: {other:?}"),
        }
    }

    if json {
        let (initial, mut sub) = subscribe_radar(&root, viewport).await?;
        let initial_line = serde_json::to_string(&initial)?;
        if !write_stdout_line_ignoring_broken_pipe(&initial_line)? {
            return Ok(());
        }

        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => break,
                res = sub.next_event() => {
                    match res? {
                        Some(snap) => {
                            let line = serde_json::to_string(&snap)?;
                            if !write_stdout_line_ignoring_broken_pipe(&line)? {
                                break;
                            }
                        }
                        None => break,
                    }
                }
            }
        }
        return Ok(());
    }

    crate::tui::run_radar_tui(&root, viewport).await
}

pub async fn cmd_mcp_serve(
    repo: Option<&Path>,
    viewport: Option<String>,
    http: Option<String>,
) -> anyhow::Result<()> {
    let env_repo = std::env::var_os("LOOM_REPO_ROOT")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    let repo_arg = repo.map(Path::to_path_buf).or(env_repo);
    let root = discover_repo_root(repo_arg.as_deref(), false)?;

    let default_viewport = viewport
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var("LOOM_VIEWPORT")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        });

    // Ensure loomd is running before serving MCP requests.
    let _stream = connect_or_autostart(&root).await?;
    drop(_stream);

    let cfg = McpServerConfig {
        repo_root: root,
        default_viewport,
    };

    if let Some(addr) = http {
        serve_mcp_http(&addr, cfg).await
    } else {
        serve_mcp_stdio(cfg).await
    }
}
