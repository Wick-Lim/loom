use interprocess::local_socket::tokio::Stream;
use loom_core::model::{RadarLevel, RadarStatusReport, SymbolStatus, ViewportState};
use loom_core::rpc::{
    RadarSnapshot, RpcErrorCode, RpcMethod, RpcPayload, RpcRequest, RpcResponse, ViewportSummary,
    read_frame, write_frame,
};
#[cfg(windows)]
use loom_core::rpc::{pipe_short_name, read_sock_descriptor};
#[cfg(unix)]
use loom_core::rpc::sock_descriptor_path;
use loom_daemon::{DaemonRunExit, StartDaemonOutcome, start_daemon};
use std::path::Path;
use std::time::{Duration, Instant};
use tempfile::TempDir;

async fn connect_test_ipc(repo_root: &Path) -> anyhow::Result<Stream> {
    #[cfg(windows)]
    {
        use interprocess::local_socket::{GenericNamespaced, ToNsName, tokio::prelude::*};
        let pipe_short = match read_sock_descriptor(repo_root) {
            Ok(desc) => desc
                .endpoint
                .strip_prefix(r"\\.\pipe\")
                .unwrap_or(&desc.endpoint)
                .to_string(),
            Err(_) => pipe_short_name(repo_root)?,
        };
        let ns_name = pipe_short.as_str().to_ns_name::<GenericNamespaced>()?;
        Ok(Stream::connect(ns_name).await?)
    }
    #[cfg(unix)]
    {
        use interprocess::local_socket::{GenericFilePath, ToFsName, tokio::prelude::*};
        let sock_path = sock_descriptor_path(repo_root);
        let fs_name = sock_path.as_path().to_fs_name::<GenericFilePath>()?;
        Ok(Stream::connect(fs_name).await?)
    }
}

async fn send_rpc(
    repo_root: &Path,
    method: RpcMethod,
) -> Result<RpcPayload, (RpcErrorCode, String)> {
    let mut stream = connect_test_ipc(repo_root)
        .await
        .map_err(|e| (RpcErrorCode::IoError, e.to_string()))?;
    write_frame(&mut stream, &RpcRequest { id: 1, method })
        .await
        .map_err(|e| (RpcErrorCode::IoError, e.to_string()))?;
    let resp: RpcResponse = read_frame(&mut stream)
        .await
        .map_err(|e| (RpcErrorCode::IoError, e.to_string()))?
        .ok_or_else(|| (RpcErrorCode::IoError, "unexpected EOF".to_string()))?;
    resp.result.map_err(|e| (e.code, e.message))
}

async fn spawn_vp(repo: &Path, name: &str, intent: &str) -> ViewportSummary {
    match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: name.into(),
            intent: intent.into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected payload: {other:?}"),
    }
}

async fn poll_snapshot(repo: &Path) -> RadarSnapshot {
    match send_rpc(repo, RpcMethod::RadarSnapshot { viewport_id: None })
        .await
        .unwrap()
    {
        RpcPayload::RadarSnapshot(s) => s,
        other => panic!("unexpected payload: {other:?}"),
    }
}

async fn wait_snapshot<F>(repo: &Path, timeout: Duration, mut pred: F) -> RadarSnapshot
where
    F: FnMut(&RadarSnapshot) -> bool,
{
    let deadline = Instant::now() + timeout;
    loop {
        let snap = poll_snapshot(repo).await;
        if pred(&snap) {
            return snap;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for radar condition; last snapshot: {snap:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn watcher_ignores_loom_git_target_and_build_dirs() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn base() -> i32 { 1 }\n").unwrap();

    let running = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };

    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();
    let vp = spawn_vp(repo, "v1", "ignore dirs check").await;

    let initial = poll_snapshot(repo).await;
    let seq_before = initial.sequence;

    // Write files inside ignored directories
    for ig in [".loom", ".git", "target", "node_modules", ".venv", "dist", "build"] {
        let d = vp.path.join(ig);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("noise.rs"), "pub fn noisy() {}\n").unwrap();
    }
    tokio::time::sleep(Duration::from_millis(120)).await;
    let after_noise = poll_snapshot(repo).await;
    assert_eq!(
        after_noise.sequence, seq_before,
        "ignored directory writes must not trigger radar re-evaluation"
    );
    assert!(after_noise.viewports[0].dirty_files.is_empty());

    // Write real source file -> picked up quickly
    std::fs::write(
        vp.path.join("src/lib.rs"),
        "pub fn base() -> i32 { 2 }\n",
    )
    .unwrap();
    let after_real = wait_snapshot(repo, Duration::from_secs(3), |s| {
        s.viewports
            .first()
            .is_some_and(|v| v.dirty_files.iter().any(|p| p.ends_with("lib.rs")))
    })
    .await;
    assert!(after_real.sequence > seq_before);

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test]
async fn adaptive_batch_mode_triggers_on_codegen_burst_and_recovers() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn root() {}\n").unwrap();

    let running = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();
    let vp = spawn_vp(repo, "codegen", "bulk codegen").await;

    // Burst write 25 files (< 20 threshold default) rapidly
    for i in 0..25 {
        std::fs::write(
            vp.path.join(format!("src/gen_{i}.rs")),
            format!("pub fn gen_{i}() -> i32 {{ {i} }}\n"),
        )
        .unwrap();
    }

    // Batch mode should activate and increment batch_switches
    let during_or_after = wait_snapshot(repo, Duration::from_secs(4), |s| {
        s.metrics.batch_switches >= 1
    })
    .await;
    assert!(during_or_after.metrics.batch_switches >= 1);
    assert!(during_or_after.metrics.coalesced_events >= 1);

    // After quiet period, batch mode returns to interactive (15ms) and all 25 files are dirty
    let recovered = wait_snapshot(repo, Duration::from_secs(4), |s| {
        !s.metrics.batch_mode
            && s.metrics.effective_debounce_ms == 15
            && s.viewports.first().is_some_and(|v| v.dirty_files.len() >= 25)
    })
    .await;
    assert!(!recovered.batch_mode);
    assert_eq!(recovered.debounce_ms, 15);

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test]
async fn incremental_parse_and_pending_syntax_retains_last_valid_symbols() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("src/lib.rs"),
        "pub fn alpha() -> i32 { 1 }\npub fn beta() -> i32 { 2 }\n",
    )
    .unwrap();

    let running = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();
    let vp1 = spawn_vp(repo, "v1", "edit alpha").await;
    let vp2 = spawn_vp(repo, "v2", "edit beta").await;

    // v2 cleanly modifies beta
    std::fs::write(
        vp2.path.join("src/lib.rs"),
        "pub fn alpha() -> i32 { 1 }\npub fn beta() -> i32 { 99 }\n",
    )
    .unwrap();
    wait_snapshot(repo, Duration::from_secs(3), |s| {
        s.viewports
            .iter()
            .find(|v| v.viewport_id == vp2.id)
            .is_some_and(|v| !v.dirty_files.is_empty())
    })
    .await;

    // v1 starts typing invalid syntax mid-edit
    std::fs::write(
        vp1.path.join("src/lib.rs"),
        "pub fn alpha( {\npub fn beta() -> i32 { 2 }\n",
    )
    .unwrap();

    let snap_pending = wait_snapshot(repo, Duration::from_secs(3), |s| {
        s.viewports
            .iter()
            .find(|v| v.viewport_id == vp1.id)
            .is_some_and(|v| {
                v.pending_syntax_files
                    .iter()
                    .any(|p| p.ends_with("lib.rs"))
            })
    })
    .await;

    let vp1_sum = snap_pending
        .viewports
        .iter()
        .find(|v| v.viewport_id == vp1.id)
        .unwrap();
    // Must NOT emit false RED conflict due to CRDT fallback or false RemoveNode mid-edit
    assert_eq!(vp1_sum.level, RadarLevel::Green);
    assert!(vp1_sum.pending_syntax);

    let disk_pending: RadarStatusReport = serde_json::from_slice(
        &std::fs::read(vp1.path.join(".loom/RADAR_STATUS.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(disk_pending.status, RadarLevel::Green);
    assert!(disk_pending.pending_syntax);
    assert!(
        disk_pending
            .pending_syntax_files
            .iter()
            .any(|p| p.ends_with("lib.rs"))
    );

    // Verify state.redb retains last valid `alpha` symbol with PendingSyntax status
    let provides = running.ctx.db.list_symbol_provides_for_viewport(&vp1.id).unwrap();
    let alpha_entry = provides
        .iter()
        .find(|e| e.symbol_fqn.ends_with("alpha"))
        .expect("retained alpha provide entry");
    assert_eq!(alpha_entry.status, SymbolStatus::PendingSyntax);

    // Fixing syntax clears PENDING_SYNTAX and restores Ready status
    std::fs::write(
        vp1.path.join("src/lib.rs"),
        "pub fn alpha() -> i32 { 42 }\npub fn beta() -> i32 { 2 }\n",
    )
    .unwrap();

    let snap_fixed = wait_snapshot(repo, Duration::from_secs(3), |s| {
        s.viewports
            .iter()
            .find(|v| v.viewport_id == vp1.id)
            .is_some_and(|v| {
                !v.pending_syntax && v.pending_syntax_files.is_empty() && v.level == RadarLevel::Green
            })
    })
    .await;
    assert!(!snap_fixed.viewports[0].pending_syntax);
    assert!(snap_fixed.viewports[0].pending_syntax_files.is_empty());

    let disk_fixed: RadarStatusReport = serde_json::from_slice(
        &std::fs::read(vp1.path.join(".loom/RADAR_STATUS.json")).unwrap(),
    )
    .unwrap();
    assert!(!disk_fixed.pending_syntax);
    assert!(disk_fixed.pending_syntax_files.is_empty());

    let provides_after = running.ctx.db.list_symbol_provides_for_viewport(&vp1.id).unwrap();
    assert_eq!(
        provides_after
            .iter()
            .find(|e| e.symbol_fqn.ends_with("alpha"))
            .unwrap()
            .status,
        SymbolStatus::Ready
    );

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test]
async fn four_language_tags_extraction_rust_ts_go_py() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("src/lib.rs"),
        "pub fn rs_fn() { rs_helper(); }\npub fn rs_helper() {}\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("src/app.ts"),
        "export function tsFn(x: MyType): void { tsHelper(x); }\nexport function tsHelper(v: MyType) {}\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("src/view.tsx"),
        "export function View() { return <div/>; }\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("src/main.go"),
        "package main\nfunc GoFn() { goHelper() }\nfunc goHelper() {}\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("src/mod.py"),
        "def py_fn():\n    return py_helper()\ndef py_helper():\n    return 1\n",
    )
    .unwrap();

    let running = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();
    let vp = spawn_vp(repo, "poly", "4 lang check").await;

    let provides = running.ctx.db.list_symbol_provides_for_viewport(&vp.id).unwrap();
    let consumes = running.ctx.db.list_symbol_consumes_for_viewport(&vp.id).unwrap();

    assert!(provides.iter().any(|p| p.symbol_fqn.ends_with("rs_fn")));
    assert!(provides.iter().any(|p| p.symbol_fqn.ends_with("tsFn")));
    assert!(provides.iter().any(|p| p.symbol_fqn.ends_with("View")));
    assert!(provides.iter().any(|p| p.symbol_fqn.ends_with("GoFn")));
    assert!(provides.iter().any(|p| p.symbol_fqn.ends_with("py_fn")));

    assert!(consumes.iter().any(|c| c.symbol_fqn == "rs_helper"));
    assert!(consumes.iter().any(|c| c.symbol_fqn == "tsHelper"));
    assert!(consumes.iter().any(|c| c.symbol_fqn == "goHelper"));
    assert!(consumes.iter().any(|c| c.symbol_fqn == "py_helper"));

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test]
async fn nxn_shadow_merge_green_yellow_red_and_atomic_radar_status() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("src/lib.rs"),
        "pub fn shared() -> i32 { 1 }\npub fn other() -> i32 { 2 }\n",
    )
    .unwrap();
    std::fs::write(repo.join("src/caller.rs"), "pub fn run() -> i32 { shared() }\n").unwrap();

    let running = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    let vp1 = spawn_vp(repo, "v1", "task 1").await;
    let vp2 = spawn_vp(repo, "v2", "task 2").await;
    let vp3 = spawn_vp(repo, "v3", "task 3").await;

    // 1) Disjoint CST edits in v1 (add fn alpha) and v2 (add fn beta) -> GREEN
    std::fs::write(
        vp1.path.join("src/lib.rs"),
        "pub fn shared() -> i32 { 1 }\npub fn other() -> i32 { 2 }\npub fn alpha() -> i32 { 10 }\n",
    )
    .unwrap();
    std::fs::write(
        vp2.path.join("src/lib.rs"),
        "pub fn shared() -> i32 { 1 }\npub fn other() -> i32 { 2 }\npub fn beta() -> i32 { 20 }\n",
    )
    .unwrap();

    let green_snap = wait_snapshot(repo, Duration::from_secs(3), |s| {
        s.viewports.iter().all(|v| v.level == RadarLevel::Green)
            && s.viewports
                .iter()
                .filter(|v| v.viewport_id == vp1.id || v.viewport_id == vp2.id)
                .all(|v| !v.dirty_files.is_empty())
    })
    .await;
    assert_eq!(green_snap.matrix.len(), 3);

    // Verify atomic RADAR_STATUS.json exists and parses cleanly
    let status_file = vp1.path.join(".loom/RADAR_STATUS.json");
    assert!(status_file.is_file());
    let report: RadarStatusReport =
        serde_json::from_slice(&std::fs::read(&status_file).unwrap()).unwrap();
    assert_eq!(report.status, RadarLevel::Green);

    // 2) v1 modifies body of `shared` (consumed by v3 in src/caller.rs) -> YELLOW
    std::fs::write(
        vp1.path.join("src/lib.rs"),
        "pub fn shared() -> i32 { 100 }\npub fn other() -> i32 { 2 }\npub fn alpha() -> i32 { 10 }\n",
    )
    .unwrap();
    let yellow_snap = wait_snapshot(repo, Duration::from_secs(3), |s| {
        s.viewports
            .iter()
            .find(|v| v.viewport_id == vp1.id)
            .is_some_and(|v| v.level == RadarLevel::Yellow)
    })
    .await;
    assert!(
        yellow_snap
            .viewports
            .iter()
            .find(|v| v.viewport_id == vp3.id)
            .unwrap()
            .level
            == RadarLevel::Yellow
    );

    // 3) v2 also modifies `shared` body incompatibly -> RED
    std::fs::write(
        vp2.path.join("src/lib.rs"),
        "pub fn shared() -> i32 { 200 }\npub fn other() -> i32 { 2 }\npub fn beta() -> i32 { 20 }\n",
    )
    .unwrap();
    let red_snap = wait_snapshot(repo, Duration::from_secs(3), |s| {
        s.viewports
            .iter()
            .find(|v| v.viewport_id == vp1.id)
            .is_some_and(|v| v.level == RadarLevel::Red)
    })
    .await;
    assert!(red_snap.matrix.iter().any(|c| c.level == RadarLevel::Red));

    let red_report: RadarStatusReport =
        serde_json::from_slice(&std::fs::read(&status_file).unwrap()).unwrap();
    assert_eq!(red_report.status, RadarLevel::Red);

    // 4) restore_clean on v2 preserves .loom/RADAR_STATUS.json and clears RED back down
    send_rpc(
        repo,
        RpcMethod::RestoreViewportClean {
            viewport_id: vp2.id.clone(),
        },
    )
    .await
    .unwrap();
    assert!(vp2.path.join(".loom/RADAR_STATUS.json").is_file());
    let after_restore = wait_snapshot(repo, Duration::from_secs(3), |s| {
        s.viewports
            .iter()
            .find(|v| v.viewport_id == vp2.id)
            .is_some_and(|v| v.dirty_files.is_empty() && v.level != RadarLevel::Red)
    })
    .await;
    assert!(!after_restore.matrix.iter().any(|c| c.level == RadarLevel::Red));
    assert_ne!(
        after_restore
            .viewports
            .iter()
            .find(|v| v.viewport_id == vp1.id)
            .unwrap()
            .level,
        RadarLevel::Red
    );

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test]
async fn lifecycle_hooks_startup_recovery_spawn_transition_gc_prune() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn x() -> i32 { 1 }\n").unwrap();

    // First daemon run: init + spawn two viewports + shut down
    let r1 = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();
    let vp1 = spawn_vp(repo, "v1", "first").await;
    let vp2 = spawn_vp(repo, "v2", "second").await;
    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(r1.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);

    // Edit v1 & v2 while daemon is stopped, then restart daemon -> bootstrap_from_db
    std::fs::write(vp1.path.join("src/lib.rs"), "pub fn x() -> i32 { 11 }\n").unwrap();
    std::fs::write(vp2.path.join("src/lib.rs"), "pub fn x() -> i32 { 22 }\n").unwrap();

    let r2 = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };

    let booted = wait_snapshot(repo, Duration::from_secs(3), |s| {
        s.viewports.len() == 2 && s.viewports.iter().all(|v| v.level == RadarLevel::Red)
    })
    .await;
    assert_eq!(booted.viewports.len(), 2);

    // Transition vp2 -> Dropped (unwatches Windows directory handle), then GC --prune-viewports
    send_rpc(
        repo,
        RpcMethod::TransitionViewport {
            viewport_id: vp2.id.clone(),
            target_state: ViewportState::Dropped,
            pid: None,
        },
    )
    .await
    .unwrap();

    let after_drop = wait_snapshot(repo, Duration::from_secs(3), |s| {
        s.viewports.len() == 1 && s.viewports[0].level == RadarLevel::Green
    })
    .await;
    assert_eq!(after_drop.viewports[0].viewport_id, vp1.id);

    // GC prune must succeed on Windows with zero AccessDenied from notify handles
    let gc = match send_rpc(
        repo,
        RpcMethod::Gc {
            prune_viewports: true,
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Gc(g) => g,
        other => panic!("unexpected payload: {other:?}"),
    };
    assert_eq!(gc.pruned_viewport_dirs, 1);
    assert!(!vp2.path.exists());

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(r2.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test]
async fn radar_subscribe_streaming_and_idle_monitor_not_reset_by_passive_broadcasts() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn s() -> i32 { 1 }\n").unwrap();

    let running = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();
    let vp1 = spawn_vp(repo, "v1", "one").await;
    let vp2 = spawn_vp(repo, "v2", "two").await;

    let mut sub_stream = connect_test_ipc(repo).await.unwrap();
    write_frame(
        &mut sub_stream,
        &RpcRequest {
            id: 10,
            method: RpcMethod::RadarSubscribe { viewport_id: None },
        },
    )
    .await
    .unwrap();

    let init_resp: RpcResponse = read_frame(&mut sub_stream).await.unwrap().unwrap();
    assert_eq!(init_resp.id, 10);
    assert!(matches!(init_resp.result, Ok(RpcPayload::RadarSnapshot(_))));

    // Trigger RED conflict across v1 & v2
    std::fs::write(vp1.path.join("src/lib.rs"), "pub fn s() -> i32 { 10 }\n").unwrap();
    std::fs::write(vp2.path.join("src/lib.rs"), "pub fn s() -> i32 { 20 }\n").unwrap();

    let mut got_red_event = false;
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        let remain = deadline.saturating_duration_since(Instant::now());
        if let Ok(Ok(Some(resp))) =
            tokio::time::timeout(remain, read_frame::<_, RpcResponse>(&mut sub_stream)).await
            && let Ok(RpcPayload::RadarEvent(ev)) = resp.result
            && ev
                .escalated_notifications
                .iter()
                .any(|n| n.status == RadarLevel::Red)
        {
            got_red_event = true;
            break;
        }
    }
    assert!(got_red_event, "expected RadarEvent with RED escalation");

    drop(sub_stream);
    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test]
async fn idle_monitor_expires_with_zero_active_viewports_while_radar_subscribe_receives_passive_broadcasts() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn s() -> i32 { 1 }\n").unwrap();

    let running = match start_daemon(repo, Some(300)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();
    let vp1 = spawn_vp(repo, "v1", "one").await;

    let mut sub_stream = connect_test_ipc(repo).await.unwrap();
    write_frame(
        &mut sub_stream,
        &RpcRequest {
            id: 42,
            method: RpcMethod::RadarSubscribe { viewport_id: None },
        },
    )
    .await
    .unwrap();

    let init_resp: RpcResponse = read_frame(&mut sub_stream).await.unwrap().unwrap();
    assert_eq!(init_resp.id, 42);
    assert!(matches!(init_resp.result, Ok(RpcPayload::RadarSnapshot(_))));

    // Drop the only active viewport -> active_viewports becomes 0 and Radar broadcasts a passive RadarEvent.
    send_rpc(
        repo,
        RpcMethod::TransitionViewport {
            viewport_id: vp1.id.clone(),
            target_state: ViewportState::Dropped,
            pid: None,
        },
    )
    .await
    .unwrap();

    // Receive the passive RadarEvent while keeping `sub_stream` open and idle.
    let passive_resp = tokio::time::timeout(
        Duration::from_secs(2),
        read_frame::<_, RpcResponse>(&mut sub_stream),
    )
    .await
    .expect("timed out waiting for passive broadcast")
    .unwrap()
    .expect("expected passive RadarEvent frame");
    assert!(matches!(passive_resp.result, Ok(RpcPayload::RadarEvent(_))));

    // Even with `sub_stream` still connected and having received passive broadcasts,
    // `IdleMonitor` must expire once `active_viewports == 0`.
    let exit = tokio::time::timeout(Duration::from_secs(3), running.wait())
        .await
        .expect("daemon did not expire on idle timeout while RadarSubscribe client was open")
        .unwrap();
    assert_eq!(exit, DaemonRunExit::IdleTimeout);

    // `sub_stream` must observe a clean EOF (`Ok(None)`) when the daemon shuts down on IdleTimeout.
    let eof = tokio::time::timeout(
        Duration::from_secs(2),
        read_frame::<_, RpcResponse>(&mut sub_stream),
    )
    .await
    .expect("timed out waiting for RadarSubscribe EOF after IdleTimeout")
    .expect("read_frame after IdleTimeout should not error");
    assert!(
        eof.is_none(),
        "expected clean EOF on RadarSubscribe after IdleTimeout, got {eof:?}"
    );

    // Immediately restart `start_daemon` even with `sub_stream` still in scope to prove
    // `StateDb` lock and server pipe/socket handles were fully released before `wait()` returned.
    let restarted = match start_daemon(repo, Some(500)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => {
            panic!("daemon should restart immediately after IdleTimeout")
        }
    };
    assert!(matches!(
        send_rpc(repo, RpcMethod::Ping).await.unwrap(),
        RpcPayload::Pong { .. }
    ));
    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(
        restarted.wait().await.unwrap(),
        DaemonRunExit::ShutdownRequested
    );
}

#[tokio::test]
async fn idle_monitor_closes_idle_keepalive_ipc_connection_and_allows_immediate_restart() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn s() -> i32 { 1 }\n").unwrap();

    let running = match start_daemon(repo, Some(300)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    let mut keepalive_stream = connect_test_ipc(repo).await.unwrap();
    write_frame(
        &mut keepalive_stream,
        &RpcRequest {
            id: 7,
            method: RpcMethod::Ping,
        },
    )
    .await
    .unwrap();
    let pong: RpcResponse = read_frame(&mut keepalive_stream).await.unwrap().unwrap();
    assert_eq!(pong.id, 7);
    assert!(matches!(pong.result, Ok(RpcPayload::Pong { .. })));

    // Leave `keepalive_stream` open and idle; daemon must expire on IdleTimeout.
    let exit = tokio::time::timeout(Duration::from_secs(3), running.wait())
        .await
        .expect("daemon did not expire on idle timeout while keep-alive IPC client was open")
        .unwrap();
    assert_eq!(exit, DaemonRunExit::IdleTimeout);

    let eof = tokio::time::timeout(
        Duration::from_secs(2),
        read_frame::<_, RpcResponse>(&mut keepalive_stream),
    )
    .await
    .expect("timed out waiting for keep-alive IPC EOF after IdleTimeout")
    .expect("read_frame on keep-alive IPC after IdleTimeout should not error");
    assert!(eof.is_none(), "expected clean EOF, got {eof:?}");

    let restarted = match start_daemon(repo, Some(500)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => {
            panic!("daemon should restart immediately after IdleTimeout")
        }
    };
    assert!(matches!(
        send_rpc(repo, RpcMethod::Ping).await.unwrap(),
        RpcPayload::Pong { .. }
    ));
    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(
        restarted.wait().await.unwrap(),
        DaemonRunExit::ShutdownRequested
    );
}

#[tokio::test]
async fn radar_status_json_refreshes_updated_at_ms_on_clean_green_edit() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn alpha() -> i32 { 1 }\n").unwrap();

    let running = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();
    let vp = spawn_vp(repo, "vp-green", "clean green edit").await;

    let status_path = vp.path.join(".loom/RADAR_STATUS.json");
    let deadline = Instant::now() + Duration::from_secs(3);
    let initial: RadarStatusReport = loop {
        if let Ok(bytes) = std::fs::read(&status_path)
            && let Ok(parsed) = serde_json::from_slice::<RadarStatusReport>(&bytes)
        {
            break parsed;
        }
        assert!(Instant::now() < deadline, "initial RADAR_STATUS.json missing");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(initial.status, RadarLevel::Green);
    assert!(!initial.pending_syntax);

    tokio::time::sleep(Duration::from_millis(35)).await;
    std::fs::write(
        vp.path.join("src/lib.rs"),
        "pub fn alpha() -> i32 { 2 }\n",
    )
    .unwrap();

    let refresh_deadline = Instant::now() + Duration::from_secs(3);
    let refreshed: RadarStatusReport = loop {
        if let Ok(bytes) = std::fs::read(&status_path)
            && let Ok(parsed) = serde_json::from_slice::<RadarStatusReport>(&bytes)
            && parsed.updated_at_ms > initial.updated_at_ms
        {
            break parsed;
        }
        assert!(
            Instant::now() < refresh_deadline,
            "RADAR_STATUS.json updated_at_ms was not refreshed after clean GREEN edit (initial={})",
            initial.updated_at_ms
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert_eq!(refreshed.status, RadarLevel::Green);
    assert!(!refreshed.pending_syntax);

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(
        running.wait().await.unwrap(),
        DaemonRunExit::ShutdownRequested
    );
}

#[tokio::test]
async fn radar_does_not_emit_false_conflict_on_untouched_viewport_after_peer_seal() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("src/lib.rs"),
        "pub fn alpha() -> i32 { 1 }\npub fn beta() -> i32 { 2 }\n",
    )
    .unwrap();

    let running = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    let vp1 = spawn_vp(repo, "vp1", "edit alpha").await;
    let vp2 = spawn_vp(repo, "vp2", "untouched or disjoint peer").await;

    std::fs::write(
        vp1.path.join("src/lib.rs"),
        "pub fn alpha() -> i32 { 10 }\npub fn beta() -> i32 { 2 }\n",
    )
    .unwrap();
    send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp1.id.clone(),
            verify_cmd: None,
            summary: Some("alpha=10".into()),
        },
    )
    .await
    .unwrap();

    let vp3 = spawn_vp(repo, "vp3", "edit alpha after seal").await;
    std::fs::write(
        vp3.path.join("src/lib.rs"),
        "pub fn alpha() -> i32 { 20 }\npub fn beta() -> i32 { 2 }\n",
    )
    .unwrap();

    let snap = wait_snapshot(repo, Duration::from_secs(3), |s| {
        s.viewports
            .iter()
            .find(|v| v.viewport_id == vp3.id)
            .is_some_and(|v| v.dirty_files.iter().any(|p| p.ends_with("lib.rs")))
    })
    .await;

    let vp2_sum = snap.viewports.iter().find(|v| v.viewport_id == vp2.id).unwrap();
    let vp3_sum = snap.viewports.iter().find(|v| v.viewport_id == vp3.id).unwrap();
    assert!(
        vp2_sum.dirty_files.is_empty(),
        "untouched vp2 must not be marked dirty after vp1 seals: {:?}",
        vp2_sum.dirty_files
    );
    assert_eq!(vp2_sum.level, RadarLevel::Green);
    assert_eq!(vp3_sum.level, RadarLevel::Green);

    // If vp2 edits only disjoint beta(), vp2 stays GREEN and vp3 stays GREEN (no false blame on vp2)
    std::fs::write(
        vp2.path.join("src/lib.rs"),
        "pub fn alpha() -> i32 { 1 }\npub fn beta() -> i32 { 33 }\n",
    )
    .unwrap();
    running.ctx.radar.flush_viewport(&vp2.id).await;
    let snap2 = poll_snapshot(repo).await;
    let vp2_sum2 = snap2.viewports.iter().find(|v| v.viewport_id == vp2.id).unwrap();
    let vp3_sum2 = snap2.viewports.iter().find(|v| v.viewport_id == vp3.id).unwrap();
    assert_eq!(vp2_sum2.level, RadarLevel::Green, "disjoint vp2={vp2_sum2:?}");
    assert_eq!(vp3_sum2.level, RadarLevel::Green, "vp3={vp3_sum2:?}");

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_active_viewport_stale_trunk_conflict_detected_as_red() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().to_path_buf();
    std::fs::create_dir_all(repo.join(".loom")).unwrap();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("src/lib.rs"),
        "pub fn a() -> i32 {\n    1\n}\n\npub fn b() -> i32 {\n    2\n}\n",
    )
    .unwrap();

    let mut cfg = loom_core::config::LoomConfig::default();
    cfg.radar.debounce_ms = 15;
    cfg.save(&repo.join(".loom").join("config.toml")).unwrap();

    let running = match start_daemon(&repo, Some(60_000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("unexpected AlreadyRunning"),
    };
    let ctx = std::sync::Arc::clone(&running.ctx);
    let _ = send_rpc(&repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    let vp1 = spawn_vp(&repo, "vp1", "edit a").await;
    let vp2 = spawn_vp(&repo, "vp2", "seal a").await;

    std::fs::write(
        vp2.path.join("src/lib.rs"),
        "pub fn a() -> i32 {\n    22\n}\n\npub fn b() -> i32 {\n    2\n}\n",
    )
    .unwrap();
    let _ = send_rpc(
        &repo,
        RpcMethod::SealCapsule {
            viewport_id: vp2.id.clone(),
            verify_cmd: None,
            summary: Some("advance trunk".into()),
        },
    )
    .await
    .unwrap();

    // Untouched vp1 must NOT falsely report src/lib.rs as dirty
    let RpcPayload::RadarSnapshot(before_snap) = send_rpc(
        &repo,
        RpcMethod::RadarSnapshot {
            viewport_id: Some(vp1.id.clone()),
        },
    )
    .await
    .unwrap() else {
        panic!("expected snapshot");
    };
    assert!(
        before_snap.viewports[0].dirty_files.is_empty(),
        "untouched vp1 must not have phantom dirty_files: {:?}",
        before_snap.viewports[0].dirty_files
    );

    // Sole active viewport vp1 edits a() conflicting with advanced trunk
    std::fs::write(
        vp1.path.join("src/lib.rs"),
        "pub fn a() -> i32 {\n    99\n}\n\npub fn b() -> i32 {\n    2\n}\n",
    )
    .unwrap();
    ctx.radar.flush_viewport(&vp1.id).await;

    let RpcPayload::RadarSnapshot(after_snap) = send_rpc(
        &repo,
        RpcMethod::RadarSnapshot {
            viewport_id: Some(vp1.id.clone()),
        },
    )
    .await
    .unwrap() else {
        panic!("expected snapshot");
    };
    assert_eq!(after_snap.viewports[0].level, RadarLevel::Red);
    assert!(
        after_snap.viewports[0]
            .conflicts
            .iter()
            .any(|c| c.peer_viewport_id == "__trunk__" && c.peer_viewport_name == "trunk"),
        "expected trunk conflict item for sole active stale viewport, got {:?}",
        after_snap.viewports[0].conflicts
    );

    let _ = send_rpc(&repo, RpcMethod::Shutdown).await;
    let _ = running.wait().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn radar_restore_clean_and_divergent_trunk_delete_do_not_emit_false_red_on_disjoint_peer() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("src/lib.rs"),
        "pub fn old_fn() -> i32 { 1 }\n\npub fn keep_fn() -> i32 { 2 }\n",
    )
    .unwrap();

    let StartDaemonOutcome::Started(running) = start_daemon(repo, Some(60_000)).await.unwrap()
    else {
        panic!("expected daemon start");
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    let vp_a = spawn_vp(repo, "vp-a", "restored peer").await;
    let vp_b = spawn_vp(repo, "vp-b", "older peer").await;
    let vp_c = spawn_vp(repo, "vp-c", "trunk updater").await;

    std::fs::write(
        vp_c.path.join("src/lib.rs"),
        "pub fn keep_fn() -> i32 { 2 }\n\npub fn added_on_trunk() -> i32 { 3 }\n",
    )
    .unwrap();
    send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp_c.id.clone(),
            verify_cmd: None,
            summary: Some("remove old_fn, add added_on_trunk".into()),
        },
    )
    .await
    .unwrap();

    // Part 1: restore_clean on vp_a must bump base_op_seq so disjoint edits stay GREEN
    send_rpc(
        repo,
        RpcMethod::RestoreViewportClean {
            viewport_id: vp_a.id.clone(),
        },
    )
    .await
    .unwrap();

    std::fs::write(
        vp_a.path.join("src/lib.rs"),
        "pub fn keep_fn() -> i32 { 2 }\n\npub fn added_on_trunk() -> i32 { 300 }\n",
    )
    .unwrap();
    std::fs::write(
        vp_b.path.join("src/lib.rs"),
        "pub fn old_fn() -> i32 { 100 }\n\npub fn keep_fn() -> i32 { 2 }\n",
    )
    .unwrap();
    running.ctx.radar.flush_viewport(&vp_a.id).await;
    running.ctx.radar.flush_viewport(&vp_b.id).await;

    let snap = poll_snapshot(repo).await;
    let sum_a = snap.viewports.iter().find(|v| v.viewport_id == vp_a.id).unwrap();
    assert_eq!(
        sum_a.level,
        RadarLevel::Green,
        "up-to-date vp_a editing disjoint added_on_trunk must be GREEN, got {:?}",
        sum_a.conflicts
    );

    // Part 2: vp_a (newer base, no old_fn) edits keep_fn while older vp_b edits deleted old_fn
    // -> vp_a must stay GREEN (single-base retry with vp_b's base succeeds cleanly)
    std::fs::write(
        vp_a.path.join("src/lib.rs"),
        "pub fn keep_fn() -> i32 { 200 }\n\npub fn added_on_trunk() -> i32 { 3 }\n",
    )
    .unwrap();
    running.ctx.radar.flush_viewport(&vp_a.id).await;
    let snap2 = poll_snapshot(repo).await;
    let sum_a2 = snap2.viewports.iter().find(|v| v.viewport_id == vp_a.id).unwrap();
    assert_eq!(
        sum_a2.level,
        RadarLevel::Green,
        "up-to-date vp_a editing disjoint keep_fn must remain GREEN despite older vp_b editing deleted old_fn, got {:?}",
        sum_a2.conflicts
    );

    let _ = send_rpc(repo, RpcMethod::Shutdown).await;
    let _ = running.wait().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn radar_clears_pending_syntax_and_dirty_files_on_subdir_delete_and_rename() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn ok() -> i32 { 1 }\n").unwrap();

    let StartDaemonOutcome::Started(running) = start_daemon(repo, Some(60_000)).await.unwrap()
    else {
        panic!("expected daemon start");
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    let vp = spawn_vp(repo, "vp-dir", "subdir delete/rename").await;
    std::fs::create_dir_all(vp.path.join("src/sub")).unwrap();
    std::fs::write(
        vp.path.join("src/sub/wip.rs"),
        "pub fn broken( -> i32 { let x = ;;; }\n",
    )
    .unwrap();

    let mut saw_pending = false;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let snap = poll_snapshot(repo).await;
        if snap.viewports.iter().any(|v| v.viewport_id == vp.id && v.pending_syntax) {
            saw_pending = true;
            break;
        }
    }
    assert!(saw_pending, "expected PENDING_SYNTAX before subdir rename/delete");

    // Rename subdir while file has syntax error -> old path must clear, new path must be scanned
    std::fs::rename(vp.path.join("src/sub"), vp.path.join("src/sub2")).unwrap();
    let snap_ren = wait_snapshot(repo, Duration::from_secs(5), |snap| {
        snap.viewports.iter().any(|v| {
            v.viewport_id == vp.id
                && !v
                    .pending_syntax_files
                    .iter()
                    .any(|p| p.starts_with("src/sub") && !p.starts_with("src/sub2"))
                && !v
                    .dirty_files
                    .iter()
                    .any(|p| p.starts_with("src/sub") && !p.starts_with("src/sub2"))
        })
    })
    .await;
    let v_ren = snap_ren.viewports.iter().find(|v| v.viewport_id == vp.id).unwrap();
    assert!(
        !v_ren
            .pending_syntax_files
            .iter()
            .any(|p| p.starts_with("src/sub") && !p.starts_with("src/sub2")),
        "stale old_dir path remained after rename: {:?}",
        v_ren.pending_syntax_files
    );

    // Delete renamed subdir -> PENDING_SYNTAX and dirty_files must clear completely
    std::fs::remove_dir_all(vp.path.join("src/sub2")).unwrap();
    let snap_del = wait_snapshot(repo, Duration::from_secs(5), |snap| {
        snap.viewports.iter().any(|v| {
            v.viewport_id == vp.id
                && !v.pending_syntax
                && v.pending_syntax_files.is_empty()
                && v.dirty_files.is_empty()
        })
    })
    .await;
    let v_del = snap_del.viewports.iter().find(|v| v.viewport_id == vp.id).unwrap();
    assert!(
        !v_del.pending_syntax && v_del.pending_syntax_files.is_empty(),
        "PENDING_SYNTAX must clear after remove_dir_all: {:?}",
        v_del.pending_syntax_files
    );

    let _ = send_rpc(repo, RpcMethod::Shutdown).await;
    let _ = running.wait().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn radar_clears_pending_syntax_and_detects_delete_conflict_when_file_replaced_by_dir() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/modfile.rs"), "pub fn target() -> i32 { 1 }\n").unwrap();

    let StartDaemonOutcome::Started(running) = start_daemon(repo, Some(60_000)).await.unwrap()
    else {
        panic!("expected daemon start");
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    let vp_a = spawn_vp(repo, "vp-a", "file to dir").await;
    let vp_b = spawn_vp(repo, "vp-b", "edit target").await;

    // Put syntax error in a new file src/sub.rs -> PENDING_SYNTAX
    std::fs::write(
        vp_a.path.join("src/sub.rs"),
        "pub fn broken( -> i32 { let x = ;;; }\n",
    )
    .unwrap();
    running.ctx.radar.flush_viewport(&vp_a.id).await;
    let s1 = poll_snapshot(repo).await;
    let a1 = s1.viewports.iter().find(|v| v.viewport_id == vp_a.id).unwrap();
    assert!(a1.pending_syntax_files.contains(&std::path::PathBuf::from("src/sub.rs")));

    // Replace src/sub.rs with directory src/sub.rs/child.rs, and replace trunk file src/modfile.rs with dir
    std::fs::remove_file(vp_a.path.join("src/sub.rs")).unwrap();
    std::fs::create_dir_all(vp_a.path.join("src/sub.rs")).unwrap();
    std::fs::write(vp_a.path.join("src/sub.rs/child.rs"), "pub fn child() -> i32 { 1 }\n").unwrap();

    std::fs::remove_file(vp_a.path.join("src/modfile.rs")).unwrap();
    std::fs::create_dir_all(vp_a.path.join("src/modfile.rs")).unwrap();
    std::fs::write(vp_a.path.join("src/modfile.rs/leaf.rs"), "pub fn leaf() -> i32 { 2 }\n").unwrap();

    std::fs::write(vp_b.path.join("src/modfile.rs"), "pub fn target() -> i32 { 99 }\n").unwrap();

    running.ctx.radar.flush_viewport(&vp_a.id).await;
    running.ctx.radar.flush_viewport(&vp_b.id).await;
    let s2 = poll_snapshot(repo).await;
    let a2 = s2.viewports.iter().find(|v| v.viewport_id == vp_a.id).unwrap();
    let b2 = s2.viewports.iter().find(|v| v.viewport_id == vp_b.id).unwrap();

    assert!(
        !a2.pending_syntax && !a2.pending_syntax_files.contains(&std::path::PathBuf::from("src/sub.rs")),
        "stale PENDING_SYNTAX remained after file replaced by dir: {:?}",
        a2.pending_syntax_files
    );
    assert_eq!(
        b2.level,
        RadarLevel::Red,
        "expected RED delete-vs-edit conflict when peer replaced src/modfile.rs with dir: a={a2:?}, b={b2:?}"
    );

    let _ = send_rpc(repo, RpcMethod::Shutdown).await;
    let _ = running.wait().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn radar_divergent_bases_reports_red_for_json_crdt_and_rename_conflicts() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("src/lib.rs"),
        "pub fn old_fn() -> i32 { 1 }\n\npub fn keep_fn() -> i32 { 2 }\n",
    )
    .unwrap();
    std::fs::write(repo.join("src/rename_me.rs"), "pub fn ren() -> i32 { 7 }\n").unwrap();
    std::fs::write(repo.join("config.json"), "{\n  \"a\": 1\n}\n").unwrap();

    let StartDaemonOutcome::Started(running) = start_daemon(repo, Some(60_000)).await.unwrap()
    else {
        panic!("expected daemon start");
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    let vp_old = spawn_vp(repo, "vp-old", "older base").await;
    let vp_seal = spawn_vp(repo, "vp-seal", "advance trunk").await;
    std::fs::write(
        vp_seal.path.join("src/lib.rs"),
        "pub fn keep_fn() -> i32 { 2 }\n\npub fn added_on_trunk() -> i32 { 3 }\n",
    )
    .unwrap();
    send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp_seal.id.clone(),
            verify_cmd: None,
            summary: Some("advance trunk".into()),
        },
    )
    .await
    .unwrap();

    let vp_new = spawn_vp(repo, "vp-new", "newer base").await;
    std::fs::write(
        vp_old.path.join("src/lib.rs"),
        "pub fn old_fn() -> i32 { 10 }\n\npub fn keep_fn() -> i32 { 2 }\n",
    )
    .unwrap();
    std::fs::write(
        vp_new.path.join("src/lib.rs"),
        "pub fn keep_fn() -> i32 { 2 }\n\npub fn added_on_trunk() -> i32 { 30 }\n",
    )
    .unwrap();
    std::fs::write(vp_old.path.join("config.json"), "{\n  \"a\": 1,\n  \"k\": 10\n}\n").unwrap();
    std::fs::write(vp_new.path.join("config.json"), "{\n  \"a\": 1,\n  \"k\": 20\n}\n").unwrap();
    std::fs::rename(vp_old.path.join("src/rename_me.rs"), vp_old.path.join("src/x.rs")).unwrap();
    std::fs::rename(vp_new.path.join("src/rename_me.rs"), vp_new.path.join("src/y.rs")).unwrap();

    running.ctx.radar.flush_viewport(&vp_old.id).await;
    running.ctx.radar.flush_viewport(&vp_new.id).await;
    let snap = poll_snapshot(repo).await;
    let sum_new = snap.viewports.iter().find(|v| v.viewport_id == vp_new.id).unwrap();

    assert!(
        sum_new.conflicts.iter().any(|c| {
            c.level == RadarLevel::Red
                && c.file_path.as_ref().is_some_and(|p| p.ends_with("config.json"))
        }),
        "expected RED JSON CRDT conflict under divergent bases: {:?}",
        sum_new.conflicts
    );
    assert!(
        sum_new.conflicts.iter().any(|c| {
            c.level == RadarLevel::Red && c.message.contains("RenameOrMoveFile")
        }),
        "expected RED RenameOrMoveFile conflict under divergent bases: {:?}",
        sum_new.conflicts
    );

    let _ = send_rpc(repo, RpcMethod::Shutdown).await;
    let _ = running.wait().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn radar_divergent_disjoint_json_is_yellow_and_convergent_json_delete_has_no_red() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::write(repo.join("config.json"), "{\n  \"a\": 1,\n  \"b\": 2\n}\n").unwrap();
    std::fs::write(repo.join("del.json"), "{\n  \"x\": 1\n}\n").unwrap();

    let StartDaemonOutcome::Started(running) = start_daemon(repo, Some(60_000)).await.unwrap()
    else {
        panic!("expected daemon start");
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    let vp1 = spawn_vp(repo, "vp1", "older json base").await;
    let vp_other = spawn_vp(repo, "vp-other", "advance config.json").await;
    std::fs::write(
        vp_other.path.join("config.json"),
        "{\n  \"a\": 1,\n  \"b\": 2,\n  \"c\": 3\n}\n",
    )
    .unwrap();
    send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp_other.id.clone(),
            verify_cmd: None,
            summary: Some("add c".into()),
        },
    )
    .await
    .unwrap();

    let vp2 = spawn_vp(repo, "vp2", "newer json base").await;

    // 1. Disjoint JSON edits under divergent bases -> Yellow (crdt_concurrent_edit), NOT Red
    std::fs::write(
        vp1.path.join("config.json"),
        "{\n  \"a\": 10,\n  \"b\": 2\n}\n",
    )
    .unwrap();
    std::fs::write(
        vp2.path.join("config.json"),
        "{\n  \"a\": 1,\n  \"b\": 20,\n  \"c\": 3\n}\n",
    )
    .unwrap();

    running.ctx.radar.flush_viewport(&vp1.id).await;
    running.ctx.radar.flush_viewport(&vp2.id).await;

    let snap1 = poll_snapshot(repo).await;
    let sum1 = snap1.viewports.iter().find(|v| v.viewport_id == vp1.id).unwrap();
    let sum2 = snap1.viewports.iter().find(|v| v.viewport_id == vp2.id).unwrap();
    assert_eq!(
        sum1.level,
        RadarLevel::Yellow,
        "divergent disjoint JSON must be advisory YELLOW for vp1, got {:?}",
        sum1.conflicts
    );
    assert_eq!(
        sum2.level,
        RadarLevel::Yellow,
        "divergent disjoint JSON must be advisory YELLOW for vp2, got {:?}",
        sum2.conflicts
    );
    assert!(
        sum1.conflicts
            .iter()
            .any(|c| c.level == RadarLevel::Yellow && c.kind == "crdt_concurrent_edit")
    );
    assert!(
        !sum1.conflicts.iter().any(|c| c.level == RadarLevel::Red)
            && !sum2.conflicts.iter().any(|c| c.level == RadarLevel::Red)
    );

    // 2. Convergent deletion of del.json in both vp1 and vp2 -> no Red structural_conflict
    std::fs::remove_file(vp1.path.join("del.json")).unwrap();
    std::fs::remove_file(vp2.path.join("del.json")).unwrap();
    running.ctx.radar.flush_viewport(&vp1.id).await;
    running.ctx.radar.flush_viewport(&vp2.id).await;

    let snap2 = poll_snapshot(repo).await;
    let sum1_after = snap2.viewports.iter().find(|v| v.viewport_id == vp1.id).unwrap();
    let sum2_after = snap2.viewports.iter().find(|v| v.viewport_id == vp2.id).unwrap();
    assert!(
        !sum1_after
            .conflicts
            .iter()
            .any(|c| c.level == RadarLevel::Red && c.file_path.as_ref().is_some_and(|p| p.ends_with("del.json"))),
        "convergent JSON delete must not emit RED on vp1: {:?}",
        sum1_after.conflicts
    );
    assert!(
        !sum2_after
            .conflicts
            .iter()
            .any(|c| c.level == RadarLevel::Red && c.file_path.as_ref().is_some_and(|p| p.ends_with("del.json"))),
        "convergent JSON delete must not emit RED on vp2: {:?}",
        sum2_after.conflicts
    );
    assert_eq!(sum1_after.level, RadarLevel::Yellow);
    assert_eq!(sum2_after.level, RadarLevel::Yellow);

    let _ = send_rpc(repo, RpcMethod::Shutdown).await;
    let _ = running.wait().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn radar_preserves_deleted_file_patches_across_peer_seal_and_daemon_restart() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/foo.rs"), "pub fn foo() -> i32 { 1 }\n").unwrap();
    std::fs::write(repo.join("src/bar.rs"), "pub fn bar() -> i32 { 1 }\n").unwrap();

    let StartDaemonOutcome::Started(running) = start_daemon(repo, Some(60_000)).await.unwrap()
    else {
        panic!("expected daemon start");
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    let vp1 = spawn_vp(repo, "vp1", "delete foo").await;
    let vp2 = spawn_vp(repo, "vp2", "seal bar").await;
    let vp3 = spawn_vp(repo, "vp3", "edit foo").await;

    // vp1 deletes src/foo.rs
    std::fs::remove_file(vp1.path.join("src/foo.rs")).unwrap();
    running.ctx.radar.flush_viewport(&vp1.id).await;
    let snap_pre = poll_snapshot(repo).await;
    let v1_pre = snap_pre.viewports.iter().find(|v| v.viewport_id == vp1.id).unwrap();
    assert!(
        v1_pre.dirty_files.iter().any(|p| p.ends_with("foo.rs")),
        "vp1 should record deleted src/foo.rs before peer seal: {:?}",
        v1_pre.dirty_files
    );

    // vp2 edits and seals unrelated src/bar.rs -> triggers RefreshTrunkAndReeval
    std::fs::write(vp2.path.join("src/bar.rs"), "pub fn bar() -> i32 { 2 }\n").unwrap();
    running.ctx.radar.flush_viewport(&vp2.id).await;
    send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp2.id.clone(),
            verify_cmd: None,
            summary: Some("seal bar".into()),
        },
    )
    .await
    .unwrap();

    // vp3 edits src/foo.rs -> must report RED delete-vs-modify against vp1
    std::fs::write(vp3.path.join("src/foo.rs"), "pub fn foo() -> i32 { 99 }\n").unwrap();
    running.ctx.radar.flush_viewport(&vp3.id).await;

    let snap_after_seal = poll_snapshot(repo).await;
    let v1_seal = snap_after_seal.viewports.iter().find(|v| v.viewport_id == vp1.id).unwrap();
    let v3_seal = snap_after_seal.viewports.iter().find(|v| v.viewport_id == vp3.id).unwrap();
    assert!(
        v1_seal.dirty_files.iter().any(|p| p.ends_with("foo.rs")),
        "vp1 lost deleted-file patch after peer seal: {v1_seal:?}"
    );
    assert_eq!(
        v1_seal.level,
        RadarLevel::Red,
        "vp1 should be RED after peer seal + vp3 edit: {v1_seal:?}"
    );
    assert_eq!(
        v3_seal.level,
        RadarLevel::Red,
        "vp3 should be RED after peer seal + vp1 delete: {v3_seal:?}"
    );

    // Restart daemon (bootstrap_from_db -> register_viewport_inner)
    let _ = send_rpc(repo, RpcMethod::Shutdown).await;
    let _ = running.wait().await;

    let StartDaemonOutcome::Started(running2) = start_daemon(repo, Some(60_000)).await.unwrap()
    else {
        panic!("expected daemon restart");
    };
    let snap_restarted = wait_snapshot(repo, Duration::from_secs(5), |s| {
        s.viewports.iter().any(|v| v.viewport_id == vp1.id)
            && s.viewports.iter().any(|v| v.viewport_id == vp3.id)
    })
    .await;
    let v1_boot = snap_restarted.viewports.iter().find(|v| v.viewport_id == vp1.id).unwrap();
    let v3_boot = snap_restarted.viewports.iter().find(|v| v.viewport_id == vp3.id).unwrap();
    assert!(
        v1_boot.dirty_files.iter().any(|p| p.ends_with("foo.rs")),
        "vp1 lost deleted-file patch after daemon restart: {v1_boot:?}"
    );
    assert_eq!(
        v1_boot.level,
        RadarLevel::Red,
        "vp1 should remain RED after daemon restart: {v1_boot:?}"
    );
    assert_eq!(
        v3_boot.level,
        RadarLevel::Red,
        "vp3 should remain RED after daemon restart: {v3_boot:?}"
    );

    let _ = send_rpc(repo, RpcMethod::Shutdown).await;
    let _ = running2.wait().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn radar_handles_case_only_rename_without_ghosting_old_casing() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/Foo.rs"), "pub fn foo() -> i32 { 1 }\n").unwrap();

    let StartDaemonOutcome::Started(running) = start_daemon(repo, Some(60_000)).await.unwrap()
    else {
        panic!("expected daemon start");
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    let vp1 = spawn_vp(repo, "vp1", "case-only rename").await;
    let vp2 = spawn_vp(repo, "vp2", "edit original").await;

    // vp1 renames src/Foo.rs -> src/foo.rs (via temp name for NTFS safety)
    let tmp_name = vp1.path.join("src/foo_tmp.rs");
    std::fs::rename(vp1.path.join("src/Foo.rs"), &tmp_name).unwrap();
    std::fs::rename(&tmp_name, vp1.path.join("src/foo.rs")).unwrap();

    // vp2 edits src/Foo.rs -> commutes cleanly with vp1's case-only rename
    std::fs::write(vp2.path.join("src/Foo.rs"), "pub fn foo() -> i32 { 42 }\n").unwrap();

    running.ctx.radar.flush_viewport(&vp1.id).await;
    running.ctx.radar.flush_viewport(&vp2.id).await;

    let snap = poll_snapshot(repo).await;
    let v1 = snap.viewports.iter().find(|v| v.viewport_id == vp1.id).unwrap();
    let v2 = snap.viewports.iter().find(|v| v.viewport_id == vp2.id).unwrap();
    assert!(
        v1.dirty_files.iter().any(|p| p.ends_with("Foo.rs"))
            && v1.dirty_files.iter().any(|p| p.ends_with("foo.rs")),
        "vp1 must detect deletion of Foo.rs and creation of foo.rs on NTFS: {:?}",
        v1.dirty_files
    );
    assert_eq!(
        v1.level,
        RadarLevel::Green,
        "vp1 case-only rename vs vp2 non-conflicting edit commutes cleanly: {v1:?}"
    );
    assert_eq!(v2.level, RadarLevel::Green);

    // Now vp2 deletes src/Foo.rs -> must report RED rename-vs-delete conflict
    std::fs::remove_file(vp2.path.join("src/Foo.rs")).unwrap();
    running.ctx.radar.flush_viewport(&vp2.id).await;
    let snap2 = poll_snapshot(repo).await;
    let v1_red = snap2.viewports.iter().find(|v| v.viewport_id == vp1.id).unwrap();
    let v2_red = snap2.viewports.iter().find(|v| v.viewport_id == vp2.id).unwrap();
    assert_eq!(
        v1_red.level,
        RadarLevel::Red,
        "vp1 case-only rename vs vp2 delete must report RED: {v1_red:?}"
    );
    assert_eq!(
        v2_red.level,
        RadarLevel::Red,
        "vp2 delete vs vp1 case-only rename must report RED: {v2_red:?}"
    );

    let _ = send_rpc(repo, RpcMethod::Shutdown).await;
    let _ = running.wait().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn radar_detects_delete_vs_modify_on_code_files_with_top_level_insert_and_comment_only() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/foo.rs"), "pub fn a() -> i32 { 1 }\n").unwrap();
    std::fs::write(repo.join("src/empty.rs"), "// comment only\n").unwrap();
    std::fs::write(repo.join("src/both_del.rs"), "pub fn z() -> i32 { 9 }\n").unwrap();

    let StartDaemonOutcome::Started(running) = start_daemon(repo, Some(60_000)).await.unwrap()
    else {
        panic!("expected daemon start");
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    let vp1 = spawn_vp(repo, "vp1", "delete files").await;
    let vp2 = spawn_vp(repo, "vp2", "modify files").await;

    // vp1 deletes all three files
    std::fs::remove_file(vp1.path.join("src/foo.rs")).unwrap();
    std::fs::remove_file(vp1.path.join("src/empty.rs")).unwrap();
    std::fs::remove_file(vp1.path.join("src/both_del.rs")).unwrap();

    // vp2 adds a new top-level member to src/foo.rs, edits comment in src/empty.rs, and convergently deletes src/both_del.rs
    std::fs::write(
        vp2.path.join("src/foo.rs"),
        "pub fn a() -> i32 { 1 }\n\npub fn b() -> i32 { 2 }\n",
    )
    .unwrap();
    std::fs::write(
        vp2.path.join("src/empty.rs"),
        "// comment only\n// extra comment\n",
    )
    .unwrap();
    std::fs::remove_file(vp2.path.join("src/both_del.rs")).unwrap();

    running.ctx.radar.flush_viewport(&vp1.id).await;
    running.ctx.radar.flush_viewport(&vp2.id).await;

    let snap_peer = poll_snapshot(repo).await;
    for vid in [&vp1.id, &vp2.id] {
        let sum = snap_peer.viewports.iter().find(|v| &v.viewport_id == vid).unwrap();
        assert_eq!(sum.level, RadarLevel::Red, "expected RED in peer phase: {sum:?}");
        assert!(
            sum.conflicts.iter().any(|c| {
                c.level == RadarLevel::Red
                    && c.kind == "structural_conflict"
                    && c.file_path.as_ref().is_some_and(|p| p.ends_with("foo.rs"))
                    && c.message.contains("delete-vs-modify")
            }),
            "expected delete-vs-modify RED on src/foo.rs: {:?}",
            sum.conflicts
        );
        assert!(
            sum.conflicts.iter().any(|c| {
                c.level == RadarLevel::Red
                    && c.kind == "structural_conflict"
                    && c.file_path.as_ref().is_some_and(|p| p.ends_with("empty.rs"))
                    && c.message.contains("delete-vs-modify")
            }),
            "expected delete-vs-modify RED on src/empty.rs: {:?}",
            sum.conflicts
        );
        assert!(
            !sum.conflicts.iter().any(|c| {
                c.file_path.as_ref().is_some_and(|p| p.ends_with("both_del.rs"))
            }),
            "convergent deletion on src/both_del.rs must not conflict: {:?}",
            sum.conflicts
        );
    }

    // Seal vp1 (deletes files on trunk) -> trunk-vs-viewport must still flag RED on vp2 for foo.rs & empty.rs
    send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp1.id.clone(),
            verify_cmd: None,
            summary: Some("delete files on trunk".into()),
        },
    )
    .await
    .unwrap();

    let snap_trunk = wait_snapshot(repo, Duration::from_secs(5), |s| {
        s.viewports
            .iter()
            .find(|v| v.viewport_id == vp2.id)
            .is_some_and(|v| {
                v.conflicts.iter().any(|c| c.peer_viewport_name == "trunk" && c.level == RadarLevel::Red)
            })
    })
    .await;
    let v2_trunk = snap_trunk.viewports.iter().find(|v| v.viewport_id == vp2.id).unwrap();
    assert_eq!(v2_trunk.level, RadarLevel::Red);
    assert!(
        v2_trunk.conflicts.iter().any(|c| {
            c.peer_viewport_name == "trunk"
                && c.level == RadarLevel::Red
                && c.file_path.as_ref().is_some_and(|p| p.ends_with("foo.rs"))
        }),
        "expected trunk delete-vs-modify RED on src/foo.rs: {:?}",
        v2_trunk.conflicts
    );
    assert!(
        v2_trunk.conflicts.iter().any(|c| {
            c.peer_viewport_name == "trunk"
                && c.level == RadarLevel::Red
                && c.file_path.as_ref().is_some_and(|p| p.ends_with("empty.rs"))
        }),
        "expected trunk delete-vs-modify RED on src/empty.rs: {:?}",
        v2_trunk.conflicts
    );
    assert!(
        !v2_trunk.conflicts.iter().any(|c| {
            c.file_path.as_ref().is_some_and(|p| p.ends_with("both_del.rs"))
        }),
        "convergent trunk deletion on src/both_del.rs must remain conflict-free: {:?}",
        v2_trunk.conflicts
    );

    let _ = send_rpc(repo, RpcMethod::Shutdown).await;
    let _ = running.wait().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn radar_trunk_vs_viewport_detects_delete_vs_modify_on_detached_banner_comment_only() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("src/lib.rs"),
        "// BANNER v1\n\npub fn keep() -> i32 { 1 }\n",
    )
    .unwrap();

    let StartDaemonOutcome::Started(running) = start_daemon(repo, Some(60_000)).await.unwrap()
    else {
        panic!("expected daemon start");
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    let vp1 = spawn_vp(repo, "vp1", "edit banner comment only").await;
    let vp2 = spawn_vp(repo, "vp2", "delete lib.rs").await;

    // vp1 edits only the detached banner comment (0 AST ops -> file_patches is empty)
    std::fs::write(
        vp1.path.join("src/lib.rs"),
        "// BANNER v2\n\npub fn keep() -> i32 { 1 }\n",
    )
    .unwrap();
    // vp2 deletes src/lib.rs
    std::fs::remove_file(vp2.path.join("src/lib.rs")).unwrap();

    running.ctx.radar.flush_viewport(&vp1.id).await;
    running.ctx.radar.flush_viewport(&vp2.id).await;

    // Peer phase: vp1 must be RED with delete-vs-modify
    let snap_peer = poll_snapshot(repo).await;
    let vp1_peer = snap_peer.viewports.iter().find(|v| v.viewport_id == vp1.id).unwrap();
    assert_eq!(vp1_peer.level, RadarLevel::Red, "expected RED in peer phase: {vp1_peer:?}");
    assert!(
        vp1_peer.conflicts.iter().any(|c| {
            c.level == RadarLevel::Red
                && c.kind == "structural_conflict"
                && c.file_path.as_ref().is_some_and(|p| p.ends_with("lib.rs"))
                && c.message.contains("delete-vs-modify")
        }),
        "expected peer delete-vs-modify RED on src/lib.rs: {:?}",
        vp1_peer.conflicts
    );

    // Seal vp2 (deletes src/lib.rs on trunk)
    send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp2.id.clone(),
            verify_cmd: None,
            summary: Some("delete lib.rs on trunk".into()),
        },
    )
    .await
    .unwrap();

    // Trunk-vs-viewport phase: vp1 must STILL report RED delete-vs-modify (never flip to false GREEN)
    let snap_trunk = wait_snapshot(repo, Duration::from_secs(5), |s| {
        s.viewports
            .iter()
            .find(|v| v.viewport_id == vp1.id)
            .is_some_and(|v| {
                v.conflicts
                    .iter()
                    .any(|c| c.peer_viewport_name == "trunk" && c.level == RadarLevel::Red)
            })
    })
    .await;
    let vp1_trunk = snap_trunk.viewports.iter().find(|v| v.viewport_id == vp1.id).unwrap();
    assert_eq!(vp1_trunk.level, RadarLevel::Red);
    assert!(
        vp1_trunk.conflicts.iter().any(|c| {
            c.peer_viewport_name == "trunk"
                && c.level == RadarLevel::Red
                && c.kind == "structural_conflict"
                && c.file_path.as_ref().is_some_and(|p| p.ends_with("lib.rs"))
                && c.message.contains("delete-vs-modify")
        }),
        "expected trunk delete-vs-modify RED on src/lib.rs: {:?}",
        vp1_trunk.conflicts
    );

    // Sealing vp1 must also fail with Conflict, matching Radar RED
    let seal_err = send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp1.id.clone(),
            verify_cmd: None,
            summary: Some("banner edit".into()),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(seal_err.0, RpcErrorCode::Conflict);

    let _ = send_rpc(repo, RpcMethod::Shutdown).await;
    let _ = running.wait().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn radar_yellow_consumed_symbol_modified_for_ts_static_and_python_property() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::create_dir_all(repo.join("app")).unwrap();
    std::fs::write(
        repo.join("src/user.ts"),
        "export class User {\n    static parse(raw: string): User {\n        return new User(raw);\n    }\n}\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("src/caller.ts"),
        "import { User } from './user';\nexport function run(): User {\n    return User.parse('x');\n}\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("app/prop.py"),
        "class Temp:\n    @property\n    def c(self):\n        return self._c\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("app/caller.py"),
        "from prop import Temp\ndef use(t: Temp):\n    return t.c()\n",
    )
    .unwrap();

    let StartDaemonOutcome::Started(running) = start_daemon(repo, Some(60_000)).await.unwrap()
    else {
        panic!("expected daemon start");
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    let vp1 = spawn_vp(repo, "vp1", "edit static parse and property getter").await;
    let vp2 = spawn_vp(repo, "vp2", "edit callers").await;

    // vp1 modifies User::static:parse and Temp::get:c
    std::fs::write(
        vp1.path.join("src/user.ts"),
        "export class User {\n    static parse(raw: string): User {\n        return new User(raw.trim());\n    }\n}\n",
    )
    .unwrap();
    std::fs::write(
        vp1.path.join("app/prop.py"),
        "class Temp:\n    @property\n    def c(self):\n        return float(self._c)\n",
    )
    .unwrap();

    // vp2 modifies consumers in src/caller.ts and app/caller.py
    std::fs::write(
        vp2.path.join("src/caller.ts"),
        "import { User } from './user';\nexport function run(): User {\n    return User.parse('y');\n}\n",
    )
    .unwrap();
    std::fs::write(
        vp2.path.join("app/caller.py"),
        "from prop import Temp\ndef use(t: Temp):\n    return t.c() + 1\n",
    )
    .unwrap();

    running.ctx.radar.flush_viewport(&vp1.id).await;
    running.ctx.radar.flush_viewport(&vp2.id).await;

    // Phase 1: evaluate_viewport_pair must emit RadarLevel::Yellow (consumed_symbol_modified)
    let snap_peer = poll_snapshot(repo).await;
    let v1_peer = snap_peer
        .viewports
        .iter()
        .find(|v| v.viewport_id == vp1.id)
        .unwrap();
    let v2_peer = snap_peer
        .viewports
        .iter()
        .find(|v| v.viewport_id == vp2.id)
        .unwrap();
    assert_eq!(
        v2_peer.level,
        RadarLevel::Yellow,
        "expected vp2 YELLOW for consumed static/property symbol, got {v2_peer:?}"
    );
    assert!(
        v2_peer.conflicts.iter().any(|c| {
            c.level == RadarLevel::Yellow
                && c.kind == "consumed_symbol_modified"
                && c.symbol_fqn.as_deref() == Some("User::static:parse")
        }),
        "expected consumed_symbol_modified for User::static:parse on vp2: {:?}",
        v2_peer.conflicts
    );
    assert!(
        v2_peer.conflicts.iter().any(|c| {
            c.level == RadarLevel::Yellow
                && c.kind == "consumed_symbol_modified"
                && c.symbol_fqn.as_deref() == Some("Temp::get:c")
        }),
        "expected consumed_symbol_modified for Temp::get:c on vp2: {:?}",
        v2_peer.conflicts
    );
    assert_eq!(
        v1_peer.level,
        RadarLevel::Yellow,
        "expected vp1 YELLOW for modified symbol consumed by vp2, got {v1_peer:?}"
    );
    assert!(
        v1_peer.conflicts.iter().any(|c| {
            c.level == RadarLevel::Yellow
                && c.kind == "consumed_symbol_modified"
                && c.symbol_fqn.as_deref() == Some("User::static:parse")
        }),
        "expected consumed_symbol_modified for User::static:parse on vp1: {:?}",
        v1_peer.conflicts
    );

    // Phase 2: seal vp1 to trunk -> evaluate_trunk_vs_viewport must also emit YELLOW on vp2
    send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp1.id.clone(),
            verify_cmd: None,
            summary: Some("seal static parse and property".into()),
        },
    )
    .await
    .unwrap();

    let snap_trunk = wait_snapshot(repo, Duration::from_secs(5), |s| {
        s.viewports
            .iter()
            .find(|v| v.viewport_id == vp2.id)
            .is_some_and(|v| {
                v.conflicts.iter().any(|c| {
                    c.peer_viewport_name == "trunk"
                        && c.level == RadarLevel::Yellow
                        && c.kind == "consumed_symbol_modified"
                        && c.symbol_fqn.as_deref() == Some("User::static:parse")
                })
            })
    })
    .await;
    let v2_trunk = snap_trunk
        .viewports
        .iter()
        .find(|v| v.viewport_id == vp2.id)
        .unwrap();
    assert_eq!(v2_trunk.level, RadarLevel::Yellow);
    assert!(
        v2_trunk.conflicts.iter().any(|c| {
            c.peer_viewport_name == "trunk"
                && c.level == RadarLevel::Yellow
                && c.kind == "consumed_symbol_modified"
                && c.symbol_fqn.as_deref() == Some("User::static:parse")
        }),
        "expected trunk consumed_symbol_modified for User::static:parse: {:?}",
        v2_trunk.conflicts
    );
    assert!(
        v2_trunk.conflicts.iter().any(|c| {
            c.peer_viewport_name == "trunk"
                && c.level == RadarLevel::Yellow
                && c.kind == "consumed_symbol_modified"
                && c.symbol_fqn.as_deref() == Some("Temp::get:c")
        }),
        "expected trunk consumed_symbol_modified for Temp::get:c: {:?}",
        v2_trunk.conflicts
    );

    let _ = send_rpc(repo, RpcMethod::Shutdown).await;
    let _ = running.wait().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn radar_yellow_and_preflight_match_rust_impl_methods_via_type_method_call() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("src/foo.rs"),
        concat!(
            "pub struct Foo;\n",
            "impl Foo {\n    pub fn bar(x: i32) -> i32 { x + 1 }\n}\n",
            "impl Foo {\n    pub fn baz(x: i32) -> i32 { x + 2 }\n}\n",
        ),
    )
    .unwrap();
    std::fs::write(
        repo.join("src/caller.rs"),
        "use crate::foo::Foo;\npub fn run() -> i32 { Foo::bar(1) + Foo::baz(2) }\n",
    )
    .unwrap();

    let StartDaemonOutcome::Started(running) = start_daemon(repo, Some(60_000)).await.unwrap()
    else {
        panic!("expected daemon start");
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    let vp1 = spawn_vp(repo, "vp1", "edit impl Foo::bar and impl#2::baz").await;
    let vp2 = spawn_vp(repo, "vp2", "edit caller").await;

    std::fs::write(
        vp1.path.join("src/foo.rs"),
        concat!(
            "pub struct Foo;\n",
            "impl Foo {\n    pub fn bar(x: i32) -> i32 { x + 42 }\n}\n",
            "impl Foo {\n    pub fn baz(x: i32) -> i32 { x + 99 }\n}\n",
        ),
    )
    .unwrap();
    std::fs::write(
        vp2.path.join("src/caller.rs"),
        "use crate::foo::Foo;\npub fn run() -> i32 { Foo::bar(10) + Foo::baz(20) }\n",
    )
    .unwrap();

    running.ctx.radar.flush_viewport(&vp1.id).await;
    running.ctx.radar.flush_viewport(&vp2.id).await;

    let snap = poll_snapshot(repo).await;
    let v2 = snap
        .viewports
        .iter()
        .find(|v| v.viewport_id == vp2.id)
        .unwrap();
    assert_eq!(
        v2.level,
        RadarLevel::Yellow,
        "expected vp2 YELLOW for consumed Foo::bar / Foo::baz, got {v2:?}"
    );
    assert!(
        v2.conflicts.iter().any(|c| {
            c.level == RadarLevel::Yellow
                && c.kind == "consumed_symbol_modified"
                && c.symbol_fqn.as_deref() == Some("impl<Foo>::bar")
        }),
        "expected consumed_symbol_modified for impl<Foo>::bar: {:?}",
        v2.conflicts
    );
    assert!(
        v2.conflicts.iter().any(|c| {
            c.level == RadarLevel::Yellow
                && c.kind == "consumed_symbol_modified"
                && c.symbol_fqn.as_deref() == Some("impl<Foo>#2::baz")
        }),
        "expected consumed_symbol_modified for impl<Foo>#2::baz: {:?}",
        v2.conflicts
    );

    // Record a deadend on impl<Foo>::bar and verify preflight query with "Foo::bar" surfaces it.
    send_rpc(
        repo,
        RpcMethod::McpRecordDeadend {
            viewport_id: vp1.id.clone(),
            touched_symbols: vec!["impl<Foo>::bar".into()],
            kind: "design_deadend".into(),
            lesson_summary: "bar overflowed".into(),
        },
    )
    .await
    .unwrap();

    let pre = match send_rpc(
        repo,
        RpcMethod::McpPreflightEx {
            viewport_id: Some(vp2.id.clone()),
            symbols: vec!["Foo::bar".into()],
            files: Vec::new(),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::McpPreflight(p) => p,
        other => panic!("unexpected preflight payload: {other:?}"),
    };
    assert!(
        pre.negative_warnings
            .iter()
            .any(|w| w.lesson_summary == "bar overflowed"),
        "expected preflight negative warning for Foo::bar matching impl<Foo>::bar: {:?}",
        pre.negative_warnings
    );

    let _ = send_rpc(repo, RpcMethod::Shutdown).await;
    let _ = running.wait().await;
}
