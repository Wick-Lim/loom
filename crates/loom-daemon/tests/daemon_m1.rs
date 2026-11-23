use interprocess::local_socket::tokio::Stream;
use loom_core::config::LoomConfig;
use loom_core::db::StateDb;
use loom_core::model::{
    CowBackendKind, OpKind, OpLogEntry, RepoMeta, ViewportRecord, ViewportState, now_ns,
};
use loom_core::rpc::{
    RpcMethod, RpcPayload, RpcRequest, RpcResponse, pipe_short_name, read_frame,
    read_sock_descriptor, sock_descriptor_path, write_frame,
};
use loom_core::wal::{
    InverseEffect, StateEffect, WalLog, WalPayload, WalRecordKind,
};
use loom_daemon::{
    DaemonRunExit, IdleMonitor, LockAcquireOutcome, MutationRequest, RepoLock, StartDaemonOutcome,
    WriterActorState, recovery, run_daemon, start_daemon,
};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
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

async fn send_rpc(repo_root: &Path, method: RpcMethod) -> anyhow::Result<RpcPayload> {
    let mut stream = connect_test_ipc(repo_root).await?;
    write_frame(&mut stream, &RpcRequest { id: 1, method }).await?;
    let resp: RpcResponse = read_frame(&mut stream)
        .await?
        .ok_or_else(|| anyhow::anyhow!("unexpected EOF"))?;
    resp.result
        .map_err(|e| anyhow::anyhow!("[{:?}] {}", e.code, e.message))
}

#[tokio::test]
async fn single_daemon_lock_exclusivity() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();

    let lock1 = match RepoLock::try_acquire(repo).unwrap() {
        LockAcquireOutcome::Acquired(l) => l,
        LockAcquireOutcome::AlreadyRunning => panic!("first lock must succeed"),
    };

    // Second lock attempt in same or concurrent context must see AlreadyRunning
    match RepoLock::try_acquire(repo).unwrap() {
        LockAcquireOutcome::AlreadyRunning => {}
        LockAcquireOutcome::Acquired(_) => panic!("second lock must fail while first is held"),
    }

    // Starting run_daemon while lock is held exits cleanly with AlreadyRunning
    let exit = run_daemon(repo, Some(200)).await.unwrap();
    assert_eq!(exit, DaemonRunExit::AlreadyRunning);

    drop(lock1);

    // After releasing lock, acquiring succeeds again
    match RepoLock::try_acquire(repo).unwrap() {
        LockAcquireOutcome::Acquired(_) => {}
        LockAcquireOutcome::AlreadyRunning => panic!("lock must succeed after release"),
    }
}

#[tokio::test]
async fn writer_actor_serializes_concurrent_mutations() {
    let tmp = TempDir::new().unwrap();
    let loom_dir = tmp.path().join(".loom");
    std::fs::create_dir_all(&loom_dir).unwrap();

    let db = Arc::new(StateDb::open_or_create(&loom_dir.join("state.redb")).unwrap());
    db.init_repo_meta(&RepoMeta {
        schema_version: 1,
        repo_id: "test-repo".into(),
        colocate_git: false,
        next_lsn: 1,
        next_op_seq: 1,
        next_edge_seq: 1,
        last_applied_lsn: 0,
        last_checkpoint_lsn: 0,
        lamport_clock: 0,
        head_capsule_id: None,
        created_at_ns: now_ns(),
        updated_at_ns: now_ns(),
    })
    .unwrap();

    let (wal, _) = WalLog::open_and_repair(loom_dir.join("wal"), 1024 * 1024).unwrap();
    let idle = Arc::new(IdleMonitor::new(Duration::from_secs(60), 0));
    let (handle, thread) = WriterActorState::spawn(Arc::clone(&db), wal, 64, idle);

    let mut tasks = Vec::new();
    for i in 0..32u64 {
        let h = handle.clone();
        tasks.push(tokio::spawn(async move {
            let vp = ViewportRecord {
                id: format!("vp-{i}"),
                name: format!("agent-{i}"),
                intent: "concurrent write".into(),
                agent_id: "test".into(),
                state: ViewportState::Sealed,
                path: loom_dir_placeholder(i),
                driver: CowBackendKind::WindowsNtfsFallback,
                created_at_ns: now_ns(),
                updated_at_ns: now_ns(),
                ..ViewportRecord::default()
            };
            h.mutate(MutationRequest {
                kind: OpKind::ViewportSpawn,
                viewport_id: Some(vp.id.clone()),
                target_id: Some(vp.id.clone()),
                capsule_id: None,
                touched_symbols: vec![],
                summary: format!("spawn {i}"),
                effect: StateEffect::UpsertViewport(vp),
            })
            .await
            .unwrap()
        }));
    }

    let mut results = Vec::new();
    for t in tasks {
        results.push(t.await.unwrap());
    }
    results.sort();

    for (idx, (lsn, op_seq)) in results.iter().enumerate() {
        let expected = (idx + 1) as u64;
        assert_eq!(*lsn, expected);
        assert_eq!(*op_seq, expected);
    }

    let ops = db.list_ops(false, None, None).unwrap();
    assert_eq!(ops.len(), 32);
    assert_eq!(db.list_viewports(true).unwrap().len(), 32);

    handle.checkpoint_and_stop().await.unwrap();
    drop(handle);
    thread.join().unwrap();

    // Verify WAL contains all 32 mutations + checkpoint
    let (_, scan) = WalLog::open_and_repair(loom_dir.join("wal"), 1024 * 1024).unwrap();
    assert_eq!(scan.records.len(), 33);
}

fn loom_dir_placeholder(i: u64) -> std::path::PathBuf {
    std::path::PathBuf::from(format!(".loom/viewports/vp-{i}"))
}

#[tokio::test]
async fn idle_monitor_exits_at_zero_active_viewports() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();

    let started = Instant::now();
    let exit = run_daemon(repo, Some(200)).await.unwrap();
    let elapsed = started.elapsed();

    assert_eq!(exit, DaemonRunExit::IdleTimeout);
    assert!(
        elapsed < Duration::from_millis(1500),
        "idle shutdown took too long: {elapsed:?}"
    );
    assert!(!sock_descriptor_path(repo).exists());

    // Lock must be released
    assert!(matches!(
        RepoLock::try_acquire(repo).unwrap(),
        LockAcquireOutcome::Acquired(_)
    ));
}

#[tokio::test]
async fn idle_monitor_does_not_exit_with_active_viewport() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let loom_dir = repo.join(".loom");
    std::fs::create_dir_all(&loom_dir).unwrap();

    // Seed an active viewport in state.redb before daemon starts
    {
        let db = StateDb::open_or_create(&loom_dir.join("state.redb")).unwrap();
        db.init_repo_meta(&RepoMeta {
            repo_id: "r1".into(),
            ..RepoMeta::default()
        })
        .unwrap();
        db.upsert_viewport(&ViewportRecord {
            id: "vp-active".into(),
            name: "work".into(),
            state: ViewportState::Active,
            driver: CowBackendKind::WindowsNtfsFallback,
            ..ViewportRecord::default()
        })
        .unwrap();
    }

    let running = match start_daemon(repo, Some(150)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };

    // Wait well past 150ms idle timeout -> must still respond to Ping RPC
    tokio::time::sleep(Duration::from_millis(450)).await;
    let pong = send_rpc(repo, RpcMethod::Ping).await.unwrap();
    assert!(matches!(pong, RpcPayload::Pong { .. }));

    // Transition viewport to Sealed via WriterActor
    running
        .ctx
        .writer
        .mutate(MutationRequest {
            kind: OpKind::ViewportTransition,
            viewport_id: Some("vp-active".into()),
            target_id: Some("vp-active".into()),
            capsule_id: None,
            touched_symbols: vec![],
            summary: "seal viewport".into(),
            effect: StateEffect::UpsertViewport(ViewportRecord {
                id: "vp-active".into(),
                name: "work".into(),
                state: ViewportState::Sealed,
                driver: CowBackendKind::WindowsNtfsFallback,
                ..ViewportRecord::default()
            }),
        })
        .await
        .unwrap();

    let wait_res = tokio::time::timeout(Duration::from_millis(1500), running.wait())
        .await
        .expect("daemon must exit once active_viewports == 0")
        .unwrap();
    assert_eq!(wait_res, DaemonRunExit::IdleTimeout);
}

#[tokio::test]
async fn idle_monitor_resets_on_rpc_activity() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();

    let running = match start_daemon(repo, Some(300)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };

    // Ping every 120ms for ~600ms total -> stays alive past 300ms timeout
    for _ in 0..5 {
        tokio::time::sleep(Duration::from_millis(120)).await;
        let pong = send_rpc(repo, RpcMethod::Ping).await.unwrap();
        assert!(matches!(pong, RpcPayload::Pong { .. }));
    }

    // Stop sending RPCs -> exits after ~300ms idle gap
    let exit = tokio::time::timeout(Duration::from_millis(1500), running.wait())
        .await
        .expect("daemon should exit after idle gap")
        .unwrap();
    assert_eq!(exit, DaemonRunExit::IdleTimeout);
}

#[tokio::test]
async fn wal_crash_recovery_replays_uncommitted_and_truncates_torn_tail() {
    let tmp = TempDir::new().unwrap();
    let loom_dir = tmp.path().join(".loom");
    std::fs::create_dir_all(&loom_dir).unwrap();
    let config = LoomConfig::default();

    let vp1 = ViewportRecord {
        id: "vp-1".into(),
        name: "one".into(),
        state: ViewportState::Active,
        driver: CowBackendKind::WindowsNtfsFallback,
        ..ViewportRecord::default()
    };
    let vp2 = ViewportRecord {
        id: "vp-2".into(),
        name: "two".into(),
        state: ViewportState::Active,
        driver: CowBackendKind::WindowsNtfsFallback,
        ..ViewportRecord::default()
    };

    // Commit 2 ops (init + vp1) via WriterActor
    {
        let db = Arc::new(StateDb::open_or_create(&loom_dir.join("state.redb")).unwrap());
        let (wal, _) =
            WalLog::open_and_repair(loom_dir.join("wal"), config.wal_segment_max_bytes).unwrap();
        let idle = Arc::new(IdleMonitor::new(Duration::from_secs(60), 0));
        let (handle, thread) = WriterActorState::spawn(Arc::clone(&db), wal, 64, idle);

        let (tx, rx) = tokio::sync::oneshot::channel();
        handle
            .send(loom_daemon::WriterCommand::InitRepo {
                colocate_git: false,
                seeded_head_oid: None,
                branch: None,
                reply: tx,
            })
            .await
            .unwrap();
        rx.await.unwrap().unwrap();

        handle
            .mutate(MutationRequest {
                kind: OpKind::ViewportSpawn,
                viewport_id: Some("vp-1".into()),
                target_id: Some("vp-1".into()),
                capsule_id: None,
                touched_symbols: vec![],
                summary: "spawn vp1".into(),
                effect: StateEffect::UpsertViewport(vp1.clone()),
            })
            .await
            .unwrap();

        // Don't call checkpoint_and_stop; drop handle and join cleanly
        drop(handle);
        thread.join().unwrap();
    }

    // Now simulate crash: append 2 uncommitted WAL frames directly to wal/ (vp2 spawn + undo vp1) + torn tail
    let wal_dir = loom_dir.join("wal");
    let seg_path;
    {
        let (mut wal, _) = WalLog::open_and_repair(&wal_dir, config.wal_segment_max_bytes).unwrap();
        assert_eq!(wal.last_lsn(), 2);

        let inv_vp2 = loom_core::db::encode_value(&InverseEffect::DeleteViewport("vp-2".into())).unwrap();
        wal.append(
            WalRecordKind::Mutation,
            3,
            now_ns(),
            &WalPayload::Op {
                op: OpLogEntry {
                    op_seq: 3,
                    lsn: 3,
                    kind: OpKind::ViewportSpawn,
                    viewport_id: Some("vp-2".into()),
                    target_id: Some("vp-2".into()),
                    summary: "spawn vp2".into(),
                    inverse_payload: Some(inv_vp2),
                    timestamp_ns: now_ns(),
                    ..OpLogEntry::default()
                },
                effect: StateEffect::UpsertViewport(vp2.clone()),
            },
        )
        .unwrap();

        wal.append(
            WalRecordKind::Undo,
            4,
            now_ns(),
            &WalPayload::Undo {
                undo_op: OpLogEntry {
                    op_seq: 4,
                    lsn: 4,
                    kind: OpKind::Undo,
                    viewport_id: Some("vp-1".into()),
                    target_id: Some("2".into()),
                    summary: "undo vp1".into(),
                    timestamp_ns: now_ns(),
                    ..OpLogEntry::default()
                },
                target_op_seq: 2,
                inverse: InverseEffect::DeleteViewport("vp-1".into()),
            },
        )
        .unwrap();

        seg_path = wal.segment_paths().last().unwrap().clone();
    }

    // Inject torn trailing bytes
    {
        let mut f = OpenOptions::new().append(true).open(&seg_path).unwrap();
        f.write_all(b"LMWA\x01\x00\x01\x00torn_crash").unwrap();
        f.sync_all().unwrap();
    }

    // Run recovery
    let outcome = recovery::recover(&loom_dir, &config).unwrap();
    assert_eq!(outcome.replayed_count, 2);
    assert!(outcome.truncated_tail_bytes > 0);

    let meta = outcome.db.get_repo_meta().unwrap().unwrap();
    assert_eq!(meta.last_applied_lsn, 5); // 4 replayed + 1 recovery checkpoint
    assert_eq!(meta.last_checkpoint_lsn, 5);
    assert_eq!(meta.lamport_clock, 4); // 1 (init) + 1 (vp1) + 1 (vp2 replay) + 1 (undo replay)
    assert!(outcome.db.get_viewport("vp-1").unwrap().is_none()); // undone!
    assert!(outcome.db.get_viewport("vp-2").unwrap().is_some()); // replayed!
    assert_eq!(outcome.db.get_op(2).unwrap().unwrap().undone_by_op, Some(4));

    drop(outcome);

    // Second recovery pass must be an exact idempotent no-op
    let outcome2 = recovery::recover(&loom_dir, &config).unwrap();
    assert_eq!(outcome2.replayed_count, 0);
    assert_eq!(outcome2.truncated_tail_bytes, 0);
    assert_eq!(
        outcome2.db.get_repo_meta().unwrap().unwrap().lamport_clock,
        4
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_recovery_reverts_stranded_verifying_viewport_and_restores_lifecycle_and_idle() {
    use loom_daemon::WriterCommand;
    use tokio::sync::oneshot;

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn base() -> i32 { 1 }\n").unwrap();

    // 1. Start daemon with short idle timeout, init repo, spawn vp1 & vp2, and leave both in Verifying.
    let StartDaemonOutcome::Started(running1) =
        start_daemon(repo, Some(300)).await.unwrap()
    else {
        panic!("expected daemon start");
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    let vp1 = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "vp1".into(),
            intent: "seal after crash".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn payload: {other:?}"),
    };
    let vp2 = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "vp2".into(),
            intent: "drop after crash".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn payload: {other:?}"),
    };

    std::fs::write(vp1.path.join("src/lib.rs"), "pub fn base() -> i32 { 42 }\n").unwrap();
    running1.ctx.radar.flush_viewport(&vp1.id).await;

    for vid in [&vp1.id, &vp2.id] {
        let (tx, rx) = oneshot::channel();
        running1
            .ctx
            .writer
            .send(WriterCommand::ClaimViewportForSeal {
                viewport_id: vid.clone(),
                reply: tx,
            })
            .await
            .unwrap();
        let claimed = rx.await.unwrap().unwrap();
        assert_eq!(claimed.state, ViewportState::Verifying);
    }

    // Simulate daemon shutdown while both viewports are persisted in Verifying.
    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running1.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);

    // 2. Restart daemon: startup recovery must transition both stranded Verifying viewports -> Active.
    let StartDaemonOutcome::Started(running2) =
        start_daemon(repo, Some(300)).await.unwrap()
    else {
        panic!("expected daemon restart");
    };

    let vps = match send_rpc(repo, RpcMethod::ListViewports { include_gc: true })
        .await
        .unwrap()
    {
        RpcPayload::Viewports(v) => v,
        other => panic!("unexpected list payload: {other:?}"),
    };
    assert_eq!(vps.len(), 2);
    for vp in &vps {
        assert_eq!(
            vp.state,
            ViewportState::Active,
            "recovered viewport {} must be Active, got {}",
            vp.name,
            vp.state
        );
    }

    // 3. Seal vp1, drop vp2, and verify undo/gc + IdleMonitor all work cleanly afterwards.
    let seal = send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp1.id.clone(),
            verify_cmd: None,
            summary: Some("seal after crash recovery".into()),
        },
    )
    .await
    .unwrap();
    assert!(matches!(seal, RpcPayload::CapsuleSeal(_)));
    assert_eq!(
        std::fs::read_to_string(repo.join("src/lib.rs")).unwrap(),
        "pub fn base() -> i32 { 42 }\n"
    );

    // Undo seal -> restores trunk to 1 and vp1 -> Dropped, and verify drop/gc on vp2.
    let undo_res = send_rpc(repo, RpcMethod::Undo { op_seq: None })
        .await
        .unwrap();
    assert!(matches!(undo_res, RpcPayload::Undo(_)));
    assert_eq!(
        std::fs::read_to_string(repo.join("src/lib.rs")).unwrap(),
        "pub fn base() -> i32 { 1 }\n"
    );

    let drop_res = send_rpc(
        repo,
        RpcMethod::DropViewport {
            viewport_id: vp2.id.clone(),
            reason: "clean drop after crash recovery".into(),
            kind: None,
        },
    )
    .await
    .unwrap();
    assert!(matches!(drop_res, RpcPayload::Drop(_)));

    let gc_res = match send_rpc(
        repo,
        RpcMethod::Gc {
            prune_viewports: true,
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Gc(g) => g,
        other => panic!("unexpected gc payload: {other:?}"),
    };
    assert_eq!(gc_res.transitioned_viewports, 2);
    assert_eq!(gc_res.pruned_viewport_dirs, 2);

    // With 0 active viewports remaining, IdleMonitor must now fire normally.
    let exit = tokio::time::timeout(Duration::from_secs(5), running2.wait())
        .await
        .expect("IdleMonitor must fire after recovered viewports complete")
        .unwrap();
    assert_eq!(exit, DaemonRunExit::IdleTimeout);
}
