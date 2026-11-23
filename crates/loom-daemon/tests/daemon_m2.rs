use interprocess::local_socket::tokio::Stream;
use loom_core::model::{CowBackendKind, OpKind, RadarLevel, RadarStatusReport, ViewportState};
use loom_core::rpc::{
    RpcErrorCode, RpcMethod, RpcPayload, RpcRequest, RpcResponse, read_frame, write_frame,
};
#[cfg(windows)]
use loom_core::rpc::{pipe_short_name, read_sock_descriptor};
#[cfg(unix)]
use loom_core::rpc::sock_descriptor_path;
use loom_daemon::{DaemonRunExit, StartDaemonOutcome, start_daemon};
use std::path::Path;
use std::time::Duration;
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

async fn send_rpc_raw(
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

#[tokio::test]
async fn spawn_viewport_persists_spawned_then_active_and_blocks_idle_exit() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn trunk() -> i32 { 1 }\n").unwrap();

    let running = match start_daemon(repo, Some(150)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };

    let init = send_rpc_raw(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();
    assert!(matches!(init, RpcPayload::Init(_)));

    let spawned = match send_rpc_raw(
        repo,
        RpcMethod::SpawnViewport {
            name: "agent-a".into(),
            intent: "implement feature".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected payload: {other:?}"),
    };

    assert_eq!(spawned.name, "agent-a");
    assert_eq!(spawned.state, ViewportState::Active);
    assert!(spawned.mount.is_some());
    assert!(spawned.path.join("src/lib.rs").is_file());
    #[cfg(target_os = "windows")]
    assert_eq!(spawned.driver, CowBackendKind::WindowsNtfsFallback);

    // Status exposes default_driver and active viewports
    let status = match send_rpc_raw(repo, RpcMethod::Status).await.unwrap() {
        RpcPayload::Status(s) => s,
        other => panic!("unexpected payload: {other:?}"),
    };
    assert_eq!(status.active_viewports, 1);
    assert!(status.default_driver.is_some());
    assert_eq!(status.viewports.len(), 1);
    assert_eq!(status.viewports[0].name, "agent-a");
    assert!(status.idle_remaining_ms.is_none());

    // Log contains ViewportSpawn + ViewportTransition
    let logs = match send_rpc_raw(
        repo,
        RpcMethod::Log {
            capsules_only: false,
            symbol: None,
            limit: None,
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Log(l) => l,
        other => panic!("unexpected payload: {other:?}"),
    };
    assert!(logs.iter().any(|o| o.kind == OpKind::ViewportSpawn));
    assert!(logs.iter().any(|o| o.kind == OpKind::ViewportTransition));

    // Wait well past 150ms idle timeout -> IdleMonitor must hold daemon alive
    tokio::time::sleep(Duration::from_millis(450)).await;
    let pong = send_rpc_raw(repo, RpcMethod::Ping).await.unwrap();
    assert!(matches!(pong, RpcPayload::Pong { .. }));

    // Transition Active -> Verifying -> Sealed
    let verifying = match send_rpc_raw(
        repo,
        RpcMethod::TransitionViewport {
            viewport_id: spawned.id.clone(),
            target_state: ViewportState::Verifying,
            pid: Some(9876),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected payload: {other:?}"),
    };
    assert_eq!(verifying.state, ViewportState::Verifying);
    assert_eq!(verifying.pid, Some(9876));

    let sealed = match send_rpc_raw(
        repo,
        RpcMethod::TransitionViewport {
            viewport_id: spawned.id,
            target_state: ViewportState::Sealed,
            pid: None,
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected payload: {other:?}"),
    };
    assert_eq!(sealed.state, ViewportState::Sealed);

    let exit = tokio::time::timeout(Duration::from_millis(1500), running.wait())
        .await
        .expect("daemon must exit via idle timeout once all viewports are Sealed")
        .unwrap();
    assert_eq!(exit, DaemonRunExit::IdleTimeout);
}

#[tokio::test]
async fn spawn_viewport_rejects_duplicate_active_name_and_path_traversal() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/main.rs"), "fn main() {}\n").unwrap();

    let running = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc_raw(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    for bad_name in ["", "   ", "has space", ".", "..", ".hidden", "../escape", "a/b", "a\\b", "CON", "aux.txt", "bad:name"] {
        let err = send_rpc_raw(
            repo,
            RpcMethod::SpawnViewport {
                name: bad_name.into(),
                intent: "bad".into(),
                cmd: vec![],
            },
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.0,
            RpcErrorCode::InvalidRequest,
            "expected InvalidRequest for `{bad_name}`, got {err:?}"
        );
    }

    // Concurrent parallel spawn race for the same name -> exactly 1 succeeds, 1 gets Conflict
    let r1 = repo.to_path_buf();
    let r2 = repo.to_path_buf();
    let (res1, res2) = tokio::join!(
        send_rpc_raw(
            &r1,
            RpcMethod::SpawnViewport {
                name: "race-vp".into(),
                intent: "first".into(),
                cmd: vec![],
            }
        ),
        send_rpc_raw(
            &r2,
            RpcMethod::SpawnViewport {
                name: "race-vp".into(),
                intent: "second".into(),
                cmd: vec![],
            }
        )
    );
    let successes = [&res1, &res2].iter().filter(|r| r.is_ok()).count();
    let conflicts = [&res1, &res2]
        .iter()
        .filter(|r| matches!(r, Err((RpcErrorCode::Conflict, _))))
        .count();
    assert_eq!(successes, 1, "res1={res1:?}, res2={res2:?}");
    assert_eq!(conflicts, 1, "res1={res1:?}, res2={res2:?}");

    send_rpc_raw(repo, RpcMethod::Shutdown).await.unwrap();
    let exit = tokio::time::timeout(Duration::from_millis(2000), running.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(exit, DaemonRunExit::ShutdownRequested);
}

#[tokio::test]
#[allow(clippy::permissions_set_readonly_false)]
async fn spawn_allows_name_reuse_after_gc_prune_without_deleting_active_dir_and_restore_clean() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::create_dir_all(repo.join("target/debug/deps")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn v() -> i32 { 1 }").unwrap();
    std::fs::write(repo.join("target/.cargo-lock"), "skip-lock").unwrap();
    std::fs::write(repo.join("target/debug/deps/serde-111.rlib"), "rlib").unwrap();

    let running = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc_raw(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    // 1. Spawn "work", test restore_clean preserving .loom/RADAR_STATUS.json, mark readonly, Drop & GC prune
    let vp1 = match send_rpc_raw(
        repo,
        RpcMethod::SpawnViewport {
            name: "work".into(),
            intent: "v1".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected payload: {other:?}"),
    };
    assert!(!vp1.path.join("target/.cargo-lock").exists());
    assert!(vp1.path.join("target/debug/deps/serde-111.rlib").is_file());

    std::fs::create_dir_all(vp1.path.join(".loom")).unwrap();
    std::fs::write(vp1.path.join(".loom/PRESERVE_TEST.json"), "{\"ok\":true}").unwrap();
    std::fs::write(vp1.path.join("src/lib.rs"), "mutated").unwrap();
    std::fs::write(vp1.path.join("src/extra.rs"), "untracked").unwrap();

    send_rpc_raw(
        repo,
        RpcMethod::RestoreViewportClean {
            viewport_id: vp1.id.clone(),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(vp1.path.join("src/lib.rs")).unwrap(),
        "pub fn v() -> i32 { 1 }"
    );
    assert!(!vp1.path.join("src/extra.rs").exists());
    assert_eq!(
        std::fs::read_to_string(vp1.path.join(".loom/PRESERVE_TEST.json")).unwrap(),
        "{\"ok\":true}"
    );
    let radar_report: RadarStatusReport = serde_json::from_slice(
        &std::fs::read(vp1.path.join(".loom/RADAR_STATUS.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(radar_report.status, RadarLevel::Green);
    assert!(!radar_report.pending_syntax);

    // Mark a file readonly to verify GC teardown_viewport removes it cleanly
    let mut perms = std::fs::metadata(vp1.path.join("src/lib.rs"))
        .unwrap()
        .permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(vp1.path.join("src/lib.rs"), perms).unwrap();

    send_rpc_raw(
        repo,
        RpcMethod::TransitionViewport {
            viewport_id: vp1.id.clone(),
            target_state: ViewportState::Dropped,
            pid: None,
        },
    )
    .await
    .unwrap();

    let gc1 = match send_rpc_raw(
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
    assert_eq!(gc1.transitioned_viewports, 1);
    assert_eq!(gc1.pruned_viewport_dirs, 1);
    assert_eq!(gc1.pruned_viewport_ids, vec![vp1.id.clone()]);
    assert_eq!(gc1.pruned_viewport_names, vec!["work".to_string()]);
    assert!(!vp1.path.exists());

    // GcPending record remains in state.redb
    let list_gc = match send_rpc_raw(repo, RpcMethod::ListViewports { include_gc: true })
        .await
        .unwrap()
    {
        RpcPayload::Viewports(v) => v,
        other => panic!("unexpected payload: {other:?}"),
    };
    assert_eq!(list_gc.len(), 1);
    assert_eq!(list_gc[0].state, ViewportState::GcPending);

    // 2. Reuse name "work" -> spawns new Active viewport at the same .loom/viewports/work path
    let vp2 = match send_rpc_raw(
        repo,
        RpcMethod::SpawnViewport {
            name: "work".into(),
            intent: "v2".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected payload: {other:?}"),
    };
    assert_ne!(vp2.id, vp1.id);
    assert_eq!(vp2.path, vp1.path);
    assert!(vp2.path.join("src/lib.rs").is_file());

    // 3. Running GC prune again MUST NOT delete vp2's active directory!
    let gc2 = match send_rpc_raw(
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
    assert_eq!(gc2.pruned_viewport_dirs, 0);
    assert!(
        vp2.path.join("src/lib.rs").is_file(),
        "active viewport dir sharing reused name must NOT be deleted by stale GcPending record"
    );

    // 4. Double name reuse across a second Seal -> gc --prune-viewports cycle:
    //    vp1 already has pruned_at_ns set, so pruning vp2 must report ONLY vp2.id and vp2.name
    send_rpc_raw(
        repo,
        RpcMethod::TransitionViewport {
            viewport_id: vp2.id.clone(),
            target_state: ViewportState::Sealed,
            pid: None,
        },
    )
    .await
    .unwrap();

    let gc3 = match send_rpc_raw(
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
    assert_eq!(gc3.transitioned_viewports, 1);
    assert_eq!(gc3.pruned_viewport_dirs, 1);
    assert_eq!(gc3.pruned_viewport_ids, vec![vp2.id.clone()]);
    assert_eq!(gc3.pruned_viewport_names, vec!["work".to_string()]);
    assert!(!vp2.path.exists());

    // Both GcPending records remain in state.redb, and resolve/list_viewports keeps them
    let list_gc2 = match send_rpc_raw(repo, RpcMethod::ListViewports { include_gc: true })
        .await
        .unwrap()
    {
        RpcPayload::Viewports(v) => v,
        other => panic!("unexpected payload: {other:?}"),
    };
    assert_eq!(list_gc2.len(), 2);
    assert!(list_gc2.iter().all(|v| v.state == ViewportState::GcPending));

    // A subsequent GC is a clean no-op for viewport pruning
    let gc4 = match send_rpc_raw(
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
    assert_eq!(gc4.pruned_viewport_dirs, 0);
    assert!(gc4.pruned_viewport_ids.is_empty());

    send_rpc_raw(repo, RpcMethod::Shutdown).await.unwrap();
    let exit = tokio::time::timeout(Duration::from_millis(2000), running.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(exit, DaemonRunExit::ShutdownRequested);
}

#[tokio::test]
async fn gc_prune_prefers_newest_gc_pending_even_when_legacy_stale_has_smaller_id() {
    use loom_core::db::StateDb;
    use loom_core::model::{ViewportMount, ViewportRecord};

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn x() {}\n").unwrap();

    let running = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc_raw(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    // Seed a legacy stale GcPending record with smaller UUID ("vp-000-stale") and pruned_at_ns = None
    let shared_vp_path = repo.join(".loom/viewports/work");
    std::fs::create_dir_all(&shared_vp_path).unwrap();
    std::fs::write(shared_vp_path.join("marker.txt"), "new").unwrap();

    // Shut down temporarily to seed directly in StateDb, then restart daemon
    send_rpc_raw(repo, RpcMethod::Shutdown).await.unwrap();
    let exit = tokio::time::timeout(Duration::from_millis(2000), running.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(exit, DaemonRunExit::ShutdownRequested);

    {
        let db = StateDb::open_or_create(&repo.join(".loom/state.redb")).unwrap();
        db.upsert_viewport(&ViewportRecord {
            id: "vp-000-stale".into(),
            name: "work".into(),
            state: ViewportState::GcPending,
            path: shared_vp_path.clone(),
            driver: CowBackendKind::GenericCopyFallback,
            mount: None,
            created_at_ns: 100,
            updated_at_ns: 100,
            pruned_at_ns: None,
            ..ViewportRecord::default()
        })
        .unwrap();
        db.upsert_viewport(&ViewportRecord {
            id: "vp-zzz-newer".into(),
            name: "work".into(),
            state: ViewportState::Sealed,
            path: shared_vp_path.clone(),
            driver: CowBackendKind::WindowsNtfsFallback,
            mount: Some(ViewportMount {
                backend: CowBackendKind::WindowsNtfsFallback,
                viewport_root: shared_vp_path.clone(),
                mount_point: None,
                upper_dir: None,
                work_dir: None,
                hardlinked_files: 0,
                copied_files: 1,
                shared_cache_dirs: vec!["target".into()],
            }),
            created_at_ns: 200,
            updated_at_ns: 200,
            pruned_at_ns: None,
            ..ViewportRecord::default()
        })
        .unwrap();
    }

    let running2 = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };

    let gc = match send_rpc_raw(
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
    assert_eq!(gc.transitioned_viewports, 1);
    assert_eq!(gc.pruned_viewport_dirs, 1);
    assert_eq!(gc.pruned_viewport_ids, vec!["vp-zzz-newer".to_string()]);
    assert!(!shared_vp_path.exists());

    // Confirm both records now have pruned_at_ns set when checked via StateDb after shutdown
    send_rpc_raw(repo, RpcMethod::Shutdown).await.unwrap();
    let exit2 = tokio::time::timeout(Duration::from_millis(2000), running2.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(exit2, DaemonRunExit::ShutdownRequested);

    let db = StateDb::open_or_create(&repo.join(".loom/state.redb")).unwrap();
    let stale = db.get_viewport("vp-000-stale").unwrap().unwrap();
    let newer = db.get_viewport("vp-zzz-newer").unwrap().unwrap();
    assert!(stale.pruned_at_ns.is_some());
    assert!(newer.pruned_at_ns.is_some());
    assert!(newer.mount.is_none());
}

#[tokio::test]
async fn rollback_failed_spawn_cleans_up_without_affecting_peers_or_undo() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn ok() {}\n").unwrap();

    let running = match start_daemon(repo, Some(250)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc_raw(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    // Create an unrelated Sealed viewport and an Active peer viewport
    let vp_sealed = match send_rpc_raw(
        repo,
        RpcMethod::SpawnViewport {
            name: "vp-sealed".into(),
            intent: "keep sealed".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected payload: {other:?}"),
    };
    send_rpc_raw(
        repo,
        RpcMethod::TransitionViewport {
            viewport_id: vp_sealed.id.clone(),
            target_state: ViewportState::Sealed,
            pid: None,
        },
    )
    .await
    .unwrap();

    let vp_peer = match send_rpc_raw(
        repo,
        RpcMethod::SpawnViewport {
            name: "vp-peer".into(),
            intent: "peer active".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected payload: {other:?}"),
    };

    // Spawn vp-fail and roll it back via RollbackFailedSpawn
    let vp_fail = match send_rpc_raw(
        repo,
        RpcMethod::SpawnViewport {
            name: "vp-fail".into(),
            intent: "will fail".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected payload: {other:?}"),
    };
    assert!(vp_fail.path.is_dir());

    let rolled = match send_rpc_raw(
        repo,
        RpcMethod::RollbackFailedSpawn {
            viewport_id: vp_fail.id.clone(),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected payload: {other:?}"),
    };
    assert_eq!(rolled.id, vp_fail.id);
    assert_eq!(rolled.state, ViewportState::GcPending);
    assert!(!vp_fail.path.exists());

    // List (include_gc=false) and Status exclude vp-fail, while preserving vp-sealed and vp-peer
    let list = match send_rpc_raw(repo, RpcMethod::ListViewports { include_gc: false })
        .await
        .unwrap()
    {
        RpcPayload::Viewports(v) => v,
        other => panic!("unexpected payload: {other:?}"),
    };
    assert!(!list.iter().any(|v| v.id == vp_fail.id));
    assert!(
        list.iter()
            .any(|v| v.id == vp_sealed.id && v.state == ViewportState::Sealed)
    );
    assert!(
        list.iter()
            .any(|v| v.id == vp_peer.id && v.state == ViewportState::Active)
    );
    assert!(vp_sealed.path.is_dir());
    assert!(vp_peer.path.is_dir());

    let status = match send_rpc_raw(repo, RpcMethod::Status).await.unwrap() {
        RpcPayload::Status(s) => s,
        other => panic!("unexpected payload: {other:?}"),
    };
    assert_eq!(status.active_viewports, 1);
    assert!(!status.viewports.iter().any(|v| v.id == vp_fail.id));

    // Explicit undo on vp-fail ops must fail and latest_undoable_op must skip vp-fail to undo vp-peer
    let db_fail_rec = running.ctx.db.get_viewport(&vp_fail.id).unwrap().unwrap();
    assert_eq!(db_fail_rec.state, ViewportState::GcPending);
    assert!(db_fail_rec.pruned_at_ns.is_some());
    assert!(db_fail_rec.mount.is_none());

    // Undo(None) skips vp-fail's GcPending ops and undoes vp-peer's Activate then Spawn
    let undo1 = match send_rpc_raw(repo, RpcMethod::Undo { op_seq: None })
        .await
        .unwrap()
    {
        RpcPayload::Undo(u) => u,
        other => panic!("unexpected payload: {other:?}"),
    };
    let undo2 = match send_rpc_raw(repo, RpcMethod::Undo { op_seq: None })
        .await
        .unwrap()
    {
        RpcPayload::Undo(u) => u,
        other => panic!("unexpected payload: {other:?}"),
    };
    assert_ne!(undo1.undone_op_seq, undo2.undone_op_seq);
    assert!(running.ctx.db.get_viewport(&vp_peer.id).unwrap().is_none());

    // Further Undo(None) must fail with NotFound (must NOT resurrect vp-fail)
    let err = send_rpc_raw(repo, RpcMethod::Undo { op_seq: None })
        .await
        .unwrap_err();
    assert_eq!(err.0, RpcErrorCode::NotFound);

    let list_after_undo = match send_rpc_raw(repo, RpcMethod::ListViewports { include_gc: false })
        .await
        .unwrap()
    {
        RpcPayload::Viewports(v) => v,
        other => panic!("unexpected payload: {other:?}"),
    };
    assert!(!list_after_undo.iter().any(|v| v.id == vp_fail.id));
    assert!(!vp_fail.path.exists());

    // IdleMonitor active viewport count is now 0 -> daemon exits on idle timeout
    let exit = tokio::time::timeout(Duration::from_millis(1500), running.wait())
        .await
        .expect("daemon must exit via idle timeout once active viewports == 0")
        .unwrap();
    assert_eq!(exit, DaemonRunExit::IdleTimeout);
}
