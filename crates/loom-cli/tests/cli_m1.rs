use loom_core::cas::CasStore;
use loom_core::db::StateDb;
use loom_core::model::{
    CausalCapsule, CowBackendKind, OpKind, ViewportRecord, ViewportState, now_ns,
};
use loom_core::rpc::{DaemonStatus, GcResult, InitResult, OpLogView, UndoResult, ViewportSummary};
use loom_core::wal::{StateEffect, WalLog};
use loom_daemon::{IdleMonitor, MutationRequest, WriterActorState};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

fn loom_bin() -> &'static str {
    env!("CARGO_BIN_EXE_loom")
}

fn run_loom(repo: &Path, cwd: Option<&Path>, args: &[&str], idle_ms: u64) -> std::process::Output {
    let mut cmd = Command::new(loom_bin());
    cmd.env("LOOM_IDLE_TIMEOUT_MS", idle_ms.to_string());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    } else {
        cmd.arg("--repo").arg(repo);
    }
    cmd.args(args);
    cmd.output().expect("execute loom CLI")
}

fn wait_for_daemon_exit(repo: &Path) {
    let sock = repo.join(".loom/daemon.sock");
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while std::time::Instant::now() < deadline {
        if !sock.exists() {
            // Give OS a tiny moment to finalize lock handle drop
            std::thread::sleep(Duration::from_millis(50));
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn piped_command_output_does_not_block_on_daemon_lifetime() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();

    let started = std::time::Instant::now();
    let init_out = Command::new(loom_bin())
        .env("LOOM_IDLE_TIMEOUT_MS", "5000")
        .arg("--repo")
        .arg(repo)
        .args(["init", "--json"])
        .output()
        .expect("execute piped loom init");
    let elapsed = started.elapsed();

    assert!(
        init_out.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&init_out.stderr)
    );
    assert!(
        elapsed < Duration::from_millis(1500),
        "piped `loom init --json` blocked on daemon lifetime (elapsed={elapsed:?})"
    );
    let init_res: InitResult = serde_json::from_slice(&init_out.stdout).unwrap();
    assert!(!init_res.already_initialized);

    // Daemon must remain alive in the background
    assert!(
        repo.join(".loom/daemon.sock").exists(),
        "daemon.sock disappeared prematurely"
    );
    let status_out = Command::new(loom_bin())
        .env("LOOM_IDLE_TIMEOUT_MS", "5000")
        .arg("--repo")
        .arg(repo)
        .args(["status", "--json"])
        .output()
        .expect("execute piped loom status");
    assert!(
        status_out.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&status_out.stderr)
    );
    let status: DaemonStatus = serde_json::from_slice(&status_out.stdout).unwrap();
    assert!(status.pid != 0);
    assert_eq!(status.idle_timeout_ms, 5000);
    assert!(status.idle_remaining_ms.is_some());

    wait_for_daemon_exit(repo);
}

#[test]
fn init_colocate_git_persists_config_and_does_not_precreate_on_failure() {
    // 1. Failing --colocate-git on non-git dir must NOT pre-create config.toml
    {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let out = run_loom(repo, None, &["init", "--colocate-git", "--json"], 800);
        assert!(!out.status.success());
        assert!(
            !repo.join(".loom/config.toml").exists(),
            "config.toml must not be pre-created before handle_init validates --colocate-git"
        );
        wait_for_daemon_exit(repo);
    }

    // 2. Succeeding --colocate-git must persist colocate_git = true in config.toml
    {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

        let out = run_loom(repo, None, &["init", "--colocate-git", "--json"], 800);
        assert!(
            out.status.success(),
            "colocate-git init failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let cfg = loom_core::config::LoomConfig::load(&repo.join(".loom/config.toml")).unwrap();
        assert!(cfg.colocate_git);
        wait_for_daemon_exit(repo);
    }
}

#[test]
fn concurrent_autostart_converges_to_single_daemon() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();

    let init_out = run_loom(repo, None, &["init", "--json"], 1500);
    assert!(
        init_out.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&init_out.stderr)
    );

    // Wait for initial daemon to exit so we specifically race 5 concurrent auto-starts
    wait_for_daemon_exit(repo);

    let repo_arc = Arc::new(repo.to_path_buf());
    let mut handles = Vec::new();
    for _ in 0..5 {
        let r = Arc::clone(&repo_arc);
        handles.push(std::thread::spawn(move || {
            let out = run_loom(&r, None, &["status", "--json"], 1500);
            assert!(
                out.status.success(),
                "status failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let status: DaemonStatus =
                serde_json::from_slice(&out.stdout).expect("parse DaemonStatus JSON");
            status.pid
        }));
    }

    let pids: Vec<u32> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert!(pids.iter().all(|&p| p == pids[0] && p != 0));

    wait_for_daemon_exit(repo);
}

#[test]
fn cli_init_colocate_git_and_all_r1_commands() {
    // 1. Unborn git repository test (pure Rust .git fixture, no git.exe)
    {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

        let out = run_loom(repo, None, &["init", "--colocate-git", "--json"], 800);
        assert!(
            out.status.success(),
            "unborn init failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let init_res: InitResult = serde_json::from_slice(&out.stdout).unwrap();
        assert!(init_res.colocate_git);
        assert_eq!(init_res.seeded_head_oid, None);
        assert_eq!(init_res.branch.as_deref(), Some("main"));

        let exclude = std::fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
        assert!(exclude.lines().any(|l| l.trim() == ".loom/"));

        wait_for_daemon_exit(repo);
    }

    // 2. Committed git repository + full R1 commands + viewport-safe root discovery
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let fake_oid = "a".repeat(40);
    std::fs::create_dir_all(repo.join(".git/refs/heads")).unwrap();
    std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(repo.join(".git/refs/heads/main"), format!("{fake_oid}\n")).unwrap();

    let init_out = run_loom(repo, None, &["init", "--colocate-git", "--json"], 800);
    assert!(
        init_out.status.success(),
        "committed init failed: {}",
        String::from_utf8_lossy(&init_out.stderr)
    );
    let init_res: InitResult = serde_json::from_slice(&init_out.stdout).unwrap();
    assert_eq!(init_res.seeded_head_oid.as_deref(), Some(fake_oid.as_str()));

    // Wait for daemon to exit so we can seed test viewports/capsules/CAS blobs cleanly
    wait_for_daemon_exit(repo);

    let cas_keep_hash;
    let cas_orphan_hash;
    {
        let loom_dir = repo.join(".loom");
        let cas = CasStore::open_default(loom_dir.join("cas")).unwrap();
        cas_keep_hash = cas.put_bytes(b"referenced-blob").unwrap();
        cas_orphan_hash = cas.put_bytes(b"unreferenced-blob").unwrap();

        let db = Arc::new(StateDb::open_or_create(&loom_dir.join("state.redb")).unwrap());
        assert_eq!(
            db.get_git_map(&format!("oid:{fake_oid}"))
                .unwrap()
                .unwrap()
                .git_commit_oid,
            fake_oid
        );

        let (wal, _) = WalLog::open_and_repair(loom_dir.join("wal"), 16 * 1024 * 1024).unwrap();
        let idle = Arc::new(IdleMonitor::new(Duration::from_secs(60), 0));
        let (writer, thread) = WriterActorState::spawn(Arc::clone(&db), wal, 64, idle);

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            // Op 2: Sealed viewport with referenced CAS blob & on-disk dir
            let vp_sealed_dir = loom_dir.join("viewports/vp-sealed");
            std::fs::create_dir_all(&vp_sealed_dir).unwrap();
            std::fs::write(vp_sealed_dir.join("file.txt"), "hello").unwrap();

            // Create nested .loom/RADAR_STATUS.json inside viewport to test viewport-safe discovery
            let vp_active_dir = loom_dir.join("viewports/vp-active");
            std::fs::create_dir_all(vp_active_dir.join(".loom")).unwrap();
            std::fs::write(vp_active_dir.join(".loom/RADAR_STATUS.json"), "{}").unwrap();

            writer
                .mutate(MutationRequest {
                    kind: OpKind::ViewportSpawn,
                    viewport_id: Some("vp-sealed".into()),
                    target_id: Some("vp-sealed".into()),
                    capsule_id: None,
                    touched_symbols: vec![],
                    summary: "spawn sealed vp".into(),
                    effect: StateEffect::UpsertViewport(ViewportRecord {
                        id: "vp-sealed".into(),
                        name: "sealed-vp".into(),
                        intent: "done work".into(),
                        state: ViewportState::Sealed,
                        path: vp_sealed_dir,
                        driver: CowBackendKind::WindowsNtfsFallback,
                        base_snapshot_cas: Some(cas_keep_hash),
                        created_at_ns: now_ns(),
                        updated_at_ns: now_ns(),
                        ..ViewportRecord::default()
                    }),
                })
                .await
                .unwrap();

            // Op 3: CapsuleSeal touching crate::foo
            writer
                .mutate(MutationRequest {
                    kind: OpKind::CapsuleSeal,
                    viewport_id: Some("vp-sealed".into()),
                    target_id: Some("cap-1".into()),
                    capsule_id: Some("cap-1".into()),
                    touched_symbols: vec!["crate::foo".into()],
                    summary: "seal capsule cap-1".into(),
                    effect: StateEffect::UpsertCapsule(CausalCapsule {
                        capsule_id: "cap-1".into(),
                        viewport_id: "vp-sealed".into(),
                        agent_id: "agy".into(),
                        summary: "implemented foo".into(),
                        touched_symbols: vec!["crate::foo".into()],
                        snapshot_cas_hash: Some(cas_keep_hash),
                        created_at_ns: now_ns(),
                        ..CausalCapsule::default()
                    }),
                })
                .await
                .unwrap();

            // Op 4: Dropped viewport (will be undone by `loom undo`)
            writer
                .mutate(MutationRequest {
                    kind: OpKind::ViewportSpawn,
                    viewport_id: Some("vp-temp".into()),
                    target_id: Some("vp-temp".into()),
                    capsule_id: None,
                    touched_symbols: vec!["crate::bar".into()],
                    summary: "spawn temp vp".into(),
                    effect: StateEffect::UpsertViewport(ViewportRecord {
                        id: "vp-temp".into(),
                        name: "temp-vp".into(),
                        intent: "temp".into(),
                        state: ViewportState::Dropped,
                        path: loom_dir.join("viewports/vp-temp"),
                        driver: CowBackendKind::WindowsNtfsFallback,
                        created_at_ns: now_ns(),
                        updated_at_ns: now_ns(),
                        ..ViewportRecord::default()
                    }),
                })
                .await
                .unwrap();

            // Op 5: Another viewport (will be undone explicitly by `loom undo --op 5`)
            writer
                .mutate(MutationRequest {
                    kind: OpKind::ViewportSpawn,
                    viewport_id: Some("vp-op5".into()),
                    target_id: Some("vp-op5".into()),
                    capsule_id: None,
                    touched_symbols: vec![],
                    summary: "spawn op5 vp".into(),
                    effect: StateEffect::UpsertViewport(ViewportRecord {
                        id: "vp-op5".into(),
                        name: "op5-vp".into(),
                        intent: "op5".into(),
                        state: ViewportState::Dropped,
                        path: loom_dir.join("viewports/vp-op5"),
                        driver: CowBackendKind::WindowsNtfsFallback,
                        created_at_ns: now_ns(),
                        updated_at_ns: now_ns(),
                        ..ViewportRecord::default()
                    }),
                })
                .await
                .unwrap();

            writer.checkpoint_and_stop().await.unwrap();
        });
        drop(writer);
        thread.join().unwrap();
    }

    // Test viewport-safe walk-up discovery from inside <repo>/.loom/viewports/vp-active
    let vp_active_dir = repo.join(".loom/viewports/vp-active");
    let status_out = run_loom(repo, Some(&vp_active_dir), &["status", "--json"], 800);
    assert!(
        status_out.status.success(),
        "status from nested viewport failed: {}",
        String::from_utf8_lossy(&status_out.stderr)
    );
    let status: DaemonStatus = serde_json::from_slice(&status_out.stdout).unwrap();
    assert!(status.colocate_git);
    assert_eq!(status.cas_object_count, 2);

    // Test `loom list --json`
    let list_out = run_loom(repo, None, &["list", "--json"], 800);
    assert!(list_out.status.success());
    let vps: Vec<ViewportSummary> = serde_json::from_slice(&list_out.stdout).unwrap();
    assert_eq!(vps.len(), 3);

    // Test `loom log --capsules --symbol crate::foo --limit 1 --json`
    let log_out = run_loom(
        repo,
        None,
        &[
            "log",
            "--capsules",
            "--symbol",
            "crate::foo",
            "--limit",
            "1",
            "--json",
        ],
        800,
    );
    assert!(log_out.status.success());
    let logs: Vec<OpLogView> = serde_json::from_slice(&log_out.stdout).unwrap();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].kind, OpKind::CapsuleSeal);
    assert_eq!(logs[0].capsule_summary.as_deref(), Some("implemented foo"));

    // Test `loom undo --op 4 --json` (undoes specific op #4 -> removes vp-temp)
    let undo_op_out = run_loom(repo, None, &["undo", "--op", "4", "--json"], 800);
    assert!(
        undo_op_out.status.success(),
        "undo --op 4 failed: {}",
        String::from_utf8_lossy(&undo_op_out.stderr)
    );
    let undo_op_res: UndoResult = serde_json::from_slice(&undo_op_out.stdout).unwrap();
    assert_eq!(undo_op_res.undone_op_seq, 4);

    // Test `loom undo --json` (undoes latest undoable op #5 -> removes vp-op5)
    let undo_out = run_loom(repo, None, &["undo", "--json"], 800);
    assert!(
        undo_out.status.success(),
        "undo failed: {}",
        String::from_utf8_lossy(&undo_out.stderr)
    );
    let undo_res: UndoResult = serde_json::from_slice(&undo_out.stdout).unwrap();
    assert_eq!(undo_res.undone_op_seq, 5);

    let list_after_undo = run_loom(repo, None, &["list", "--json"], 800);
    let vps_after: Vec<ViewportSummary> = serde_json::from_slice(&list_after_undo.stdout).unwrap();
    assert_eq!(vps_after.len(), 1);
    assert_eq!(vps_after[0].id, "vp-sealed");

    // Test `loom gc --prune-viewports --json`
    let gc_out = run_loom(repo, None, &["gc", "--prune-viewports", "--json"], 800);
    assert!(
        gc_out.status.success(),
        "gc failed: {}",
        String::from_utf8_lossy(&gc_out.stderr)
    );
    let gc_res: GcResult = serde_json::from_slice(&gc_out.stdout).unwrap();
    assert_eq!(gc_res.transitioned_viewports, 1);
    assert_eq!(gc_res.pruned_viewport_dirs, 1);
    assert_eq!(gc_res.removed_cas_blobs, 1);

    // Referenced CAS blob from capsule remains; orphan blob was swept; vp-sealed hidden unless --include-gc
    let cas = CasStore::open_default(repo.join(".loom/cas")).unwrap();
    assert!(cas.contains(&cas_keep_hash));
    assert!(!cas.contains(&cas_orphan_hash));
    assert!(!repo.join(".loom/viewports/vp-sealed").exists());

    let list_no_gc: Vec<ViewportSummary> =
        serde_json::from_slice(&run_loom(repo, None, &["list", "--json"], 800).stdout).unwrap();
    assert_eq!(list_no_gc.len(), 0);

    let list_with_gc: Vec<ViewportSummary> = serde_json::from_slice(
        &run_loom(repo, None, &["list", "--include-gc", "--json"], 800).stdout,
    )
    .unwrap();
    assert_eq!(list_with_gc.len(), 1);
    assert_eq!(list_with_gc[0].state, ViewportState::GcPending);

    wait_for_daemon_exit(repo);
}

#[test]
fn non_idempotent_spawn_does_not_fail_on_idle_shutdown_boundary() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn x() {}\n").unwrap();

    assert!(run_loom(repo, None, &["init", "--json"], 150).status.success());

    for i in 0..10u64 {
        let _ = run_loom(repo, None, &["status", "--json"], 150);
        let delay_ms = 135 + (i % 6) * 5;
        std::thread::sleep(Duration::from_millis(delay_ms));
        let sp = run_loom(
            repo,
            None,
            &["spawn", "-n", &format!("vp-{i}"), "-m", "idle race", "--json"],
            150,
        );
        assert!(
            sp.status.success(),
            "spawn iter {i} (delay={delay_ms}ms) failed on idle boundary: {}",
            String::from_utf8_lossy(&sp.stderr)
        );
        let drop_out = run_loom(
            repo,
            None,
            &[
                "drop",
                "--viewport",
                &format!("vp-{i}"),
                "-r",
                "cleanup",
                "--json",
            ],
            150,
        );
        assert!(
            drop_out.status.success(),
            "drop iter {i} failed: {}",
            String::from_utf8_lossy(&drop_out.stderr)
        );
    }

    wait_for_daemon_exit(repo);
}
