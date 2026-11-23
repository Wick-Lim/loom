use interprocess::local_socket::tokio::Stream;
use loom_core::model::{
    CausalCapsule, FailureKind, OpKind, RadarLevel, RadarStatusReport, ViewportState,
};
use loom_core::rpc::{
    DropResult, GitPullResult, GitPushResult, McpPreflightResult, RadarSnapshot, RpcErrorCode,
    RpcMethod, RpcPayload, RpcRequest, RpcResponse, ViewportRebaseOutcome, ViewportSummary,
    read_frame, write_frame,
};
#[cfg(windows)]
use loom_core::rpc::{pipe_short_name, read_sock_descriptor};
#[cfg(unix)]
use loom_core::rpc::sock_descriptor_path;
use loom_daemon::capsule::verify_verification_proof;
use loom_daemon::git_sync::LoomGitNote;
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

async fn init_and_spawn(
    repo: &Path,
    colocate_git: bool,
    vp_name: &str,
    intent: &str,
) -> (loom_daemon::RunningDaemon, ViewportSummary) {
    let running = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc(repo, RpcMethod::Init { colocate_git })
        .await
        .unwrap();
    let vp = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: vp_name.into(),
            intent: intent.into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn payload: {other:?}"),
    };
    (running, vp)
}

async fn wait_until<F>(timeout: Duration, mut pred: F)
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if pred() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(pred(), "condition not satisfied within {timeout:?}");
}

fn init_git_with_initial_commit(dir: &Path, files: &[(&str, &str)]) -> gix::ObjectId {
    let mut repo = gix::init(dir).unwrap();
    {
        let mut cfg = repo.config_snapshot_mut();
        cfg.append_config(
            [
                "user.name=Test User",
                "user.email=test@example.com",
            ],
            gix::config::Source::Api,
        )
        .unwrap();
        cfg.commit().unwrap();
    }
    let empty = gix::ObjectId::empty_tree(repo.object_hash());
    let mut editor = repo.edit_tree(empty).unwrap();
    for (rel, content) in files {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, content).unwrap();
        let blob_id = repo.write_blob(content.as_bytes()).unwrap().detach();
        editor
            .upsert(*rel, gix::objs::tree::EntryKind::Blob, blob_id)
            .unwrap();
    }
    let tree_id = editor.write().unwrap().detach();
    let oid = repo
        .commit(
            "refs/heads/main",
            "initial commit",
            tree_id,
            std::iter::empty::<gix::ObjectId>(),
        )
        .unwrap()
        .detach();
    // Point HEAD at refs/heads/main
    std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    let mut idx = repo.index_from_tree(&tree_id).unwrap();
    idx.write(Default::default()).unwrap();
    oid
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn f20_unstaged_wal_capture_restart_seal_and_proof_mac() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn base() -> i32 { 1 }\n").unwrap();

    let (running, vp) = init_and_spawn(repo, false, "agent-f20", "implement calc").await;

    // 1. Spawning must emit ZERO spurious AstPatches
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(running.ctx.db.list_patches().unwrap().len(), 0);
    assert!(
        running
            .ctx
            .db
            .get_viewport(&vp.id)
            .unwrap()
            .unwrap()
            .patch_ids
            .is_empty()
    );

    // 2. Save edit #1 -> automatically captured into WAL + TABLE_PATCHES without git add
    let vp_lib = vp.path.join("src/lib.rs");
    std::fs::write(
        &vp_lib,
        "pub fn base() -> i32 { 1 }\npub struct Calc;\nimpl Calc { pub fn add(a: i32, b: i32) -> i32 { a + b } }\n",
    )
    .unwrap();

    wait_until(Duration::from_secs(5), || {
        running
            .ctx
            .db
            .get_viewport(&vp.id)
            .unwrap()
            .is_some_and(|r| !r.patch_ids.is_empty())
    })
    .await;

    let vp_after_1 = running.ctx.db.get_viewport(&vp.id).unwrap().unwrap();
    assert_eq!(vp_after_1.patch_ids.len(), 1);
    let rec1 = running
        .ctx
        .db
        .get_patch(&vp_after_1.patch_ids[0])
        .unwrap()
        .unwrap();
    assert!(rec1.lsn > 0);
    assert!(rec1.patch.parents.is_empty());

    // 3. Transient syntax error must NOT emit spurious TextCrdtFallback WAL patch
    std::fs::write(&vp_lib, "pub fn base() -> i32 { 1 }\npub fn broken( { \n").unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(
        running
            .ctx
            .db
            .get_viewport(&vp.id)
            .unwrap()
            .unwrap()
            .patch_ids
            .len(),
        1,
        "PENDING_SYNTAX must suppress transient broken-syntax WAL patch"
    );

    // 4. Fixing syntax emits incremental AstPatch #2 chained to patch #1
    std::fs::write(
        &vp_lib,
        "pub fn base() -> i32 { 1 }\npub struct Calc;\nimpl Calc { pub fn add(a: i32, b: i32) -> i32 { a + b } pub fn sub(a: i32, b: i32) -> i32 { a - b } }\n",
    )
    .unwrap();
    wait_until(Duration::from_secs(5), || {
        running
            .ctx
            .db
            .get_viewport(&vp.id)
            .unwrap()
            .is_some_and(|r| r.patch_ids.len() >= 2)
    })
    .await;
    let vp_after_2 = running.ctx.db.get_viewport(&vp.id).unwrap().unwrap();
    let rec2 = running
        .ctx
        .db
        .get_patch(&vp_after_2.patch_ids[1])
        .unwrap()
        .unwrap();
    assert_eq!(rec2.patch.parents, vec![rec1.patch.patch_hash]);
    assert!(rec2.patch.lamport_ts > rec1.patch.lamport_ts);

    // 5. Daemon restart must NOT emit duplicate patches, and preserves chain across offline edit
    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(
        running.wait().await.unwrap(),
        DaemonRunExit::ShutdownRequested
    );

    // Offline edit #3 while daemon is down
    std::fs::write(vp.path.join("src/extra.rs"), "pub fn extra() -> bool { true }\n").unwrap();

    let running2 = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should restart"),
    };

    wait_until(Duration::from_secs(5), || {
        running2
            .ctx
            .db
            .get_viewport(&vp.id)
            .unwrap()
            .is_some_and(|r| r.patch_ids.len() >= 3)
    })
    .await;
    let vp_after_3 = running2.ctx.db.get_viewport(&vp.id).unwrap().unwrap();
    assert_eq!(vp_after_3.patch_ids.len(), 3);
    let rec3 = running2
        .ctx
        .db
        .get_patch(&vp_after_3.patch_ids[2])
        .unwrap()
        .unwrap();
    assert_eq!(rec3.patch.parents, vec![rec2.patch.patch_hash]);

    // 6. Failing verify_cmd rolls back Verifying -> Active and leaves trunk untouched
    let fail_err = send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp.id.clone(),
            verify_cmd: Some("exit 1".into()),
            summary: Some("should fail".into()),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(fail_err.0, RpcErrorCode::InvalidState);
    assert_eq!(
        running2.ctx.db.get_viewport(&vp.id).unwrap().unwrap().state,
        ViewportState::Active
    );
    assert!(!repo.join("src/extra.rs").exists());

    // 7. Immediate un-debounced write right before seal is flushed into WAL chain + verified MAC
    std::fs::write(vp.path.join("src/un_debounced.rs"), "pub fn quick() -> i32 { 7 }\n").unwrap();
    let capsule = match send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp.id.clone(),
            verify_cmd: Some("echo verified-ok".into()),
            summary: Some("implement Calc and extras".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::CapsuleSeal(c) => c,
        other => panic!("expected CapsuleSeal, got {other:?}"),
    };

    assert_eq!(capsule.patch_ids.len(), 4);
    assert!(repo.join("src/extra.rs").is_file());
    assert!(repo.join("src/un_debounced.rs").is_file());

    let repo_id = running2.ctx.db.get_repo_meta().unwrap().unwrap().repo_id;
    assert!(verify_verification_proof(&repo_id, &capsule));
    let mut tampered = capsule.clone();
    tampered.patch_ids.pop();
    assert!(!verify_verification_proof(&repo_id, &tampered));
    let mut tampered_digest = capsule.clone();
    tampered_digest
        .verification_proof
        .as_mut()
        .unwrap()
        .duration_ms += 999;
    assert!(!verify_verification_proof(&repo_id, &tampered_digest));

    // 8. loom log --capsules --symbol sub matches FQN symbol
    let log_caps = match send_rpc(
        repo,
        RpcMethod::Log {
            capsules_only: true,
            symbol: Some("sub".into()),
            limit: None,
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Log(entries) => entries,
        other => panic!("unexpected log payload: {other:?}"),
    };
    assert_eq!(log_caps.len(), 1);
    assert_eq!(log_caps[0].kind, OpKind::CapsuleSeal);

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(
        running2.wait().await.unwrap(),
        DaemonRunExit::ShutdownRequested
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn f21_drop_viewport_validation_symbols_restore_preflight_and_wal_replay() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("src/lib.rs"),
        "pub fn compute(x: i32) -> i32 { x + 1 }\n",
    )
    .unwrap();
    std::fs::write(repo.join("config.json"), "{\"mode\": \"safe\"}\n").unwrap();

    let (running, vp1) = init_and_spawn(repo, false, "vp-drop-1", "try risky lock").await;
    let vp2 = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "vp-peer".into(),
            intent: "peer edit".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn payload: {other:?}"),
    };

    // 1. Validation: empty reason, 501-char Unicode reason, invalid kind rejected
    let err_empty = send_rpc(
        repo,
        RpcMethod::DropViewport {
            viewport_id: vp1.id.clone(),
            reason: "   ".into(),
            kind: None,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err_empty.0, RpcErrorCode::InvalidRequest);

    let err_501 = send_rpc(
        repo,
        RpcMethod::DropViewport {
            viewport_id: vp1.id.clone(),
            reason: "가".repeat(501),
            kind: Some("compile_error".into()),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err_501.0, RpcErrorCode::InvalidRequest);

    let err_kind = send_rpc(
        repo,
        RpcMethod::DropViewport {
            viewport_id: vp1.id.clone(),
            reason: "bad kind".into(),
            kind: Some("nonexistent_kind".into()),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err_kind.0, RpcErrorCode::InvalidRequest);

    // 2. Write syntax-broken Rust file (CompileError) + non-code JSON edit in vp1
    std::fs::write(
        vp1.path.join("src/lib.rs"),
        "pub fn compute(x: i32) -> i32 { broken_expr( }\n",
    )
    .unwrap();
    std::fs::write(vp1.path.join("config.json"), "{\"mode\": \"broken\"}\n").unwrap();

    // 3. Drop with exact 500 Unicode chars + normalized FailureKind ("deadend" -> DesignDeadEnd, or "compile-error" -> CompileError)
    let ok_500 = "한".repeat(500);
    let drop_res: DropResult = match send_rpc(
        repo,
        RpcMethod::DropViewport {
            viewport_id: vp1.id.clone(),
            reason: ok_500.clone(),
            kind: Some("compile-error".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Drop(r) => r,
        other => panic!("unexpected drop payload: {other:?}"),
    };

    assert_eq!(drop_res.viewport.state, ViewportState::Dropped);
    assert_eq!(drop_res.negative_edge.failure_kind, FailureKind::CompileError);
    assert_eq!(drop_res.negative_edge.lesson_summary, ok_500);
    assert_eq!(drop_res.negative_edge.source, "loom_drop");
    assert!(
        drop_res
            .negative_edge
            .touched_symbols
            .iter()
            .any(|s| s == "compute"),
        "expected DeclUnit fallback to capture `compute`: {:?}",
        drop_res.negative_edge.touched_symbols
    );
    assert!(
        drop_res
            .negative_edge
            .touched_symbols
            .iter()
            .any(|s| s == "file:config.json"),
        "expected non-code file fallback `file:config.json`: {:?}",
        drop_res.negative_edge.touched_symbols
    );
    assert!(!drop_res.negative_edge.patch_hashes.is_empty());

    // 4. Clean restore + RADAR_STATUS.json cleared + symbol tables cleared
    assert_eq!(
        std::fs::read_to_string(vp1.path.join("src/lib.rs")).unwrap(),
        "pub fn compute(x: i32) -> i32 { x + 1 }\n"
    );
    assert!(!vp1.path.join(".loom/RADAR_STATUS.json").exists());
    assert!(
        running
            .ctx
            .db
            .list_symbol_provides_for_viewport(&vp1.id)
            .unwrap()
            .is_empty()
    );
    assert_eq!(running.ctx.idle_monitor.active_viewports(), 1);

    // 5. Cross-viewport preflight_symbols surfaces the NegativeEdge warning immediately
    let preflight: McpPreflightResult = match send_rpc(
        repo,
        RpcMethod::McpPreflightEx {
            viewport_id: Some(vp2.id.clone()),
            symbols: vec!["compute".into()],
            files: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::McpPreflight(p) => p,
        other => panic!("unexpected preflight: {other:?}"),
    };
    assert_eq!(preflight.negative_warnings.len(), 1);
    assert_eq!(
        preflight.negative_warnings[0].edge_id,
        drop_res.negative_edge.edge_id
    );

    // 6. Drop vp2 with "deadend" -> IdleMonitor drops to 0, then verify WAL replay after restart
    let drop2 = match send_rpc(
        repo,
        RpcMethod::DropViewport {
            viewport_id: vp2.id.clone(),
            reason: "dead end on peer".into(),
            kind: Some("deadend".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Drop(r) => r,
        other => panic!("unexpected drop payload: {other:?}"),
    };
    assert_eq!(drop2.negative_edge.failure_kind, FailureKind::DesignDeadEnd);
    assert_eq!(running.ctx.idle_monitor.active_viewports(), 0);

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(
        running.wait().await.unwrap(),
        DaemonRunExit::ShutdownRequested
    );

    // Restart daemon and verify WAL/StateDb preserved both NegativeEdges & Dropped states
    let running2 = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should restart"),
    };
    assert_eq!(
        running2.ctx.db.get_viewport(&vp1.id).unwrap().unwrap().state,
        ViewportState::Dropped
    );
    let active_and_dropped = match send_rpc(repo, RpcMethod::ListViewports { include_gc: false })
        .await
        .unwrap()
    {
        RpcPayload::Viewports(v) => v,
        other => panic!("unexpected list payload: {other:?}"),
    };
    assert_eq!(active_and_dropped.len(), 2);
    assert!(
        active_and_dropped
            .iter()
            .all(|v| v.state == ViewportState::Dropped)
    );
    let edges = running2.ctx.db.list_all_negative_edges().unwrap();
    assert_eq!(edges.len(), 2);

    // Explicit GC transitions Dropped -> GcPending and prunes directories
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
    assert_eq!(
        running2.ctx.db.get_viewport(&vp1.id).unwrap().unwrap().state,
        ViewportState::GcPending
    );
    assert!(!vp1.path.exists());
    assert!(!vp2.path.exists());

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(
        running2.wait().await.unwrap(),
        DaemonRunExit::ShutdownRequested
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn f22_gix_push_pull_roundtrip_external_commit_and_active_viewport_rebase() {
    let tmp = TempDir::new().unwrap();
    let bare_dir = tmp.path().join("remote.git");
    let repo_a = tmp.path().join("repo_a");
    let repo_b = tmp.path().join("repo_b");
    std::fs::create_dir_all(&repo_a).unwrap();
    std::fs::create_dir_all(&repo_b).unwrap();

    let _bare = gix::init_bare(&bare_dir).unwrap();
    init_git_with_initial_commit(
        &repo_a,
        &[("src/lib.rs", "pub fn shared() -> i32 { 1 }\npub fn keep() -> i32 { 2 }\n")],
    );
    let _repo_b_git = gix::init(&repo_b).unwrap();
    std::fs::write(repo_b.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

    let (running_a, vp_seal) =
        init_and_spawn(&repo_a, true, "vp-seal", "update shared").await;

    // Reject external non-local remote URLs
    let remote_err = send_rpc(
        &repo_a,
        RpcMethod::GitPush {
            remote: Some("https://github.com/example/forbidden.git".into()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(remote_err.0, RpcErrorCode::InvalidState);

    std::fs::write(
        vp_seal.path.join("src/lib.rs"),
        "pub fn shared() -> i32 { 42 }\npub fn keep() -> i32 { 2 }\n",
    )
    .unwrap();
    let sealed_cap: CausalCapsule = match send_rpc(
        &repo_a,
        RpcMethod::SealCapsule {
            viewport_id: vp_seal.id.clone(),
            verify_cmd: Some("echo ok".into()),
            summary: Some("set shared to 42".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::CapsuleSeal(c) => c,
        other => panic!("unexpected seal payload: {other:?}"),
    };
    let repo_a_id = running_a.ctx.db.get_repo_meta().unwrap().unwrap().repo_id;

    let vp_drop = match send_rpc(
        &repo_a,
        RpcMethod::SpawnViewport {
            name: "vp-drop".into(),
            intent: "bad cache".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn payload: {other:?}"),
    };
    std::fs::write(
        vp_drop.path.join("src/lib.rs"),
        "pub fn shared() -> i32 { 99 }\npub fn keep() -> i32 { 2 }\n",
    )
    .unwrap();
    let drop_res = match send_rpc(
        &repo_a,
        RpcMethod::DropViewport {
            viewport_id: vp_drop.id.clone(),
            reason: "avoid changing shared to 99".into(),
            kind: Some("DesignDeadEnd".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Drop(r) => r,
        other => panic!("unexpected drop payload: {other:?}"),
    };

    let push_res: GitPushResult = match send_rpc(
        &repo_a,
        RpcMethod::GitPush {
            remote: Some(bare_dir.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::GitPush(r) => r,
        other => panic!("unexpected push payload: {other:?}"),
    };
    assert_eq!(push_res.commits_created, 1);
    assert_eq!(push_res.pushed_capsule_ids, vec![sealed_cap.capsule_id.clone()]);
    assert_eq!(push_res.synced_negative_edges, 1);

    // Inspect bare repo directly via gix: verify trailers and refs/notes/loom
    {
        let bare = gix::open(&bare_dir).unwrap();
        let tip_oid = bare
            .try_find_reference("refs/heads/main")
            .unwrap()
            .unwrap()
            .peel_to_id()
            .unwrap()
            .detach();
        let commit = bare.find_commit(tip_oid).unwrap();
        let msg = String::from_utf8_lossy(commit.message_raw_sloppy());
        assert!(msg.contains(&format!("Loom-Capsule-Id: {}", sealed_cap.capsule_id)));
        assert!(msg.contains("Loom-Agent: cli"));
        assert!(msg.contains("Loom-Verified: passed (echo ok)"));

        let mut notes = bare.notes().unwrap().with_refs(["refs/notes/loom"]).unwrap();
        let note_list = notes.get(tip_oid).unwrap();
        assert_eq!(note_list.len(), 1);
        let parsed: LoomGitNote = serde_json::from_slice(&note_list[0].blob.data).unwrap();
        assert_eq!(parsed.capsule.as_ref().unwrap().capsule_id, sealed_cap.capsule_id);
        assert_eq!(parsed.negative_edges.len(), 1);
        assert_eq!(parsed.negative_edges[0].edge_id, drop_res.negative_edge.edge_id);
    }

    // Pull in Repo B -> lossless restoration of CausalCapsule, VerificationProof, NegativeEdge, trunk files
    let running_b = match start_daemon(&repo_b, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon B should start"),
    };
    send_rpc(&repo_b, RpcMethod::Init { colocate_git: true })
        .await
        .unwrap();

    let pull1: GitPullResult = match send_rpc(
        &repo_b,
        RpcMethod::GitPull {
            remote: Some(bare_dir.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::GitPull(r) => r,
        other => panic!("unexpected pull payload: {other:?}"),
    };
    assert!(pull1.updated_trunk);
    assert_eq!(pull1.restored_capsule_ids, vec![sealed_cap.capsule_id.clone()]);
    assert_eq!(pull1.restored_negative_edges, 1);
    assert_eq!(
        std::fs::read_to_string(repo_b.join("src/lib.rs")).unwrap(),
        "pub fn shared() -> i32 { 42 }\npub fn keep() -> i32 { 2 }\n"
    );
    let restored_cap = running_b
        .ctx
        .db
        .get_capsule(&sealed_cap.capsule_id)
        .unwrap()
        .unwrap();
    assert!(verify_verification_proof(&repo_a_id, &restored_cap));

    let preflight_b: McpPreflightResult = match send_rpc(
        &repo_b,
        RpcMethod::McpPreflightEx {
            viewport_id: None,
            symbols: vec!["shared".into()],
            files: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::McpPreflight(p) => p,
        other => panic!("unexpected preflight: {other:?}"),
    };
    assert_eq!(preflight_b.negative_warnings.len(), 1);
    assert_eq!(
        preflight_b.negative_warnings[0].lesson_summary,
        "avoid changing shared to 99"
    );

    // Now test external non-Loom commit absorption + background rebase (1 clean viewport + 1 conflicting viewport)
    let vp_clean = match send_rpc(
        &repo_b,
        RpcMethod::SpawnViewport {
            name: "vp-clean".into(),
            intent: "add helper".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn: {other:?}"),
    };
    let vp_conflict = match send_rpc(
        &repo_b,
        RpcMethod::SpawnViewport {
            name: "vp-conflict".into(),
            intent: "conflicting shared edit".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn: {other:?}"),
    };

    // vp_clean adds a non-overlapping function `vp1_helper`
    std::fs::write(
        vp_clean.path.join("src/lib.rs"),
        "pub fn shared() -> i32 { 42 }\npub fn keep() -> i32 { 2 }\npub fn vp1_helper() -> i32 { 7 }\n",
    )
    .unwrap();
    // vp_conflict modifies `shared` directly
    std::fs::write(
        vp_conflict.path.join("src/lib.rs"),
        "pub fn shared() -> i32 { 999 }\npub fn keep() -> i32 { 2 }\n",
    )
    .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Create an external non-Loom commit directly in the bare repo modifying `shared` and adding `src/ext.rs`
    let ext_oid_hex = {
        let mut bare = gix::open(&bare_dir).unwrap();
        {
            let mut cfg = bare.config_snapshot_mut();
            cfg.append_config(
                [
                    "user.name=External Dev",
                    "user.email=ext@example.com",
                ],
                gix::config::Source::Api,
            )
            .unwrap();
            cfg.commit().unwrap();
        }
        let parent_id = bare
            .try_find_reference("refs/heads/main")
            .unwrap()
            .unwrap()
            .peel_to_id()
            .unwrap()
            .detach();
        let parent_tree = bare.find_commit(parent_id).unwrap().tree_id().unwrap().detach();
        let mut editor = bare.edit_tree(parent_tree).unwrap();
        let new_lib = bare
            .write_blob(b"pub fn shared() -> i32 { 100 }\npub fn keep() -> i32 { 2 }\n")
            .unwrap()
            .detach();
        let ext_rs = bare
            .write_blob(b"pub fn external() -> i32 { 5 }\n")
            .unwrap()
            .detach();
        editor
            .upsert("src/lib.rs", gix::objs::tree::EntryKind::Blob, new_lib)
            .unwrap();
        editor
            .upsert("src/ext.rs", gix::objs::tree::EntryKind::Blob, ext_rs)
            .unwrap();
        let new_tree = editor.write().unwrap().detach();
        let cid = bare
            .commit(
                "refs/heads/main",
                "external refactor of shared",
                new_tree,
                [parent_id],
            )
            .unwrap()
            .detach();
        cid.to_hex().to_string()
    };

    let pull2: GitPullResult = match send_rpc(
        &repo_b,
        RpcMethod::GitPull {
            remote: Some(bare_dir.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::GitPull(r) => r,
        other => panic!("unexpected pull2 payload: {other:?}"),
    };

    assert!(pull2.updated_trunk);
    assert_eq!(pull2.pulled_commit_oids, vec![ext_oid_hex.clone()]);
    assert_eq!(pull2.synthesized_capsule_ids.len(), 1);
    assert!(repo_b.join("src/ext.rs").is_file());

    // Verify synthesized external capsule and patches
    let ext_cap = running_b
        .ctx
        .db
        .get_capsule(&pull2.synthesized_capsule_ids[0])
        .unwrap()
        .unwrap();
    assert_eq!(ext_cap.git_commit_oid.as_deref(), Some(ext_oid_hex.as_str()));
    assert_eq!(ext_cap.patch_ids.len(), 2);

    // `loom log` / `list_ops` must include the absorbed external commit summary
    let ext_logs = match send_rpc(
        &repo_b,
        RpcMethod::Log {
            capsules_only: true,
            symbol: Some("external".into()),
            limit: None,
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Log(entries) => entries,
        other => panic!("unexpected log payload: {other:?}"),
    };
    assert!(
        ext_logs
            .iter()
            .any(|e| e.kind == OpKind::GitPull && e.summary.contains("external refactor of shared")),
        "expected GitPull op log to contain external commit summary: {ext_logs:?}"
    );

    // Check rebase outcomes: vp_clean -> Clean, vp_conflict -> Conflict (RED, untouched)
    let clean_status = pull2
        .rebased_viewports
        .iter()
        .find(|s| s.viewport_id == vp_clean.id)
        .unwrap();
    assert_eq!(clean_status.outcome, ViewportRebaseOutcome::Clean);
    assert!(vp_clean.path.join("src/ext.rs").is_file());
    let clean_lib = std::fs::read_to_string(vp_clean.path.join("src/lib.rs")).unwrap();
    assert!(clean_lib.contains("100"));
    assert!(clean_lib.contains("vp1_helper"));

    let conflict_status = pull2
        .rebased_viewports
        .iter()
        .find(|s| s.viewport_id == vp_conflict.id)
        .unwrap();
    assert_eq!(conflict_status.outcome, ViewportRebaseOutcome::Conflict);
    assert!(!conflict_status.conflicts.is_empty());
    // Uncommitted conflicting edit must NOT be clobbered
    assert_eq!(
        std::fs::read_to_string(vp_conflict.path.join("src/lib.rs")).unwrap(),
        "pub fn shared() -> i32 { 999 }\npub fn keep() -> i32 { 2 }\n"
    );

    // RADAR_STATUS.json and RadarSnapshot must report RED rebase conflict
    let radar_report: RadarStatusReport = serde_json::from_slice(
        &std::fs::read(vp_conflict.path.join(".loom/RADAR_STATUS.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(radar_report.status, RadarLevel::Red);
    assert!(
        radar_report
            .conflicts
            .iter()
            .any(|c| c.kind == "rebase_conflict" && c.level == RadarLevel::Red)
    );

    let snap: RadarSnapshot = match send_rpc(
        &repo_b,
        RpcMethod::RadarSnapshot {
            viewport_id: Some(vp_conflict.id.clone()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::RadarSnapshot(s) => s,
        other => panic!("unexpected radar snapshot: {other:?}"),
    };
    assert_eq!(snap.viewports[0].level, RadarLevel::Red);

    send_rpc(&repo_a, RpcMethod::Shutdown).await.unwrap();
    send_rpc(&repo_b, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(
        running_a.wait().await.unwrap(),
        DaemonRunExit::ShutdownRequested
    );
    assert_eq!(
        running_b.wait().await.unwrap(),
        DaemonRunExit::ShutdownRequested
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn m5_offline_viewport_deletion_captured_on_daemon_restart() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn stay() -> i32 { 1 }\n").unwrap();
    std::fs::write(repo.join("src/obsolete.rs"), "pub fn gone() -> i32 { 2 }\n").unwrap();

    let (running, vp) = init_and_spawn(repo, false, "vp-offline-del", "delete offline").await;

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(
        running.wait().await.unwrap(),
        DaemonRunExit::ShutdownRequested
    );

    // Delete `src/obsolete.rs` while loomd is shut down
    std::fs::remove_file(vp.path.join("src/obsolete.rs")).unwrap();

    let running2 = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should restart"),
    };

    wait_until(Duration::from_secs(5), || {
        running2
            .ctx
            .db
            .get_viewport(&vp.id)
            .unwrap()
            .is_some_and(|r| !r.patch_ids.is_empty())
    })
    .await;

    let vp_rec = running2.ctx.db.get_viewport(&vp.id).unwrap().unwrap();
    assert_eq!(vp_rec.patch_ids.len(), 1);
    let patch_rec = running2
        .ctx
        .db
        .get_patch(&vp_rec.patch_ids[0])
        .unwrap()
        .unwrap();
    assert!(patch_rec.patch.cas_after.is_none());
    assert!(patch_rec.patch.file_path.ends_with("obsolete.rs"));

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(
        running2.wait().await.unwrap(),
        DaemonRunExit::ShutdownRequested
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn m5_unpushed_capsule_with_remote_ahead_conflict_aborts_without_clobber() {
    let tmp = TempDir::new().unwrap();
    let bare_dir = tmp.path().join("remote.git");
    let repo_a = tmp.path().join("repo_a");
    std::fs::create_dir_all(&repo_a).unwrap();

    let _bare = gix::init_bare(&bare_dir).unwrap();
    init_git_with_initial_commit(
        &repo_a,
        &[("src/lib.rs", "pub fn shared() -> i32 { 1 }\npub fn other() -> i32 { 2 }\n")],
    );

    let (running_a, vp_seed) =
        init_and_spawn(&repo_a, true, "vp-seed", "initial push").await;
    std::fs::write(
        vp_seed.path.join("src/lib.rs"),
        "pub fn shared() -> i32 { 10 }\npub fn other() -> i32 { 2 }\n",
    )
    .unwrap();
    send_rpc(
        &repo_a,
        RpcMethod::SealCapsule {
            viewport_id: vp_seed.id.clone(),
            verify_cmd: None,
            summary: Some("seed bare".into()),
        },
    )
    .await
    .unwrap();
    send_rpc(
        &repo_a,
        RpcMethod::GitPush {
            remote: Some(bare_dir.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap();

    let head_before = gix::open(&repo_a).unwrap().head_id().unwrap().detach();

    // Advance bare repo with conflicting edit to `shared`
    {
        let mut bare = gix::open(&bare_dir).unwrap();
        {
            let mut cfg = bare.config_snapshot_mut();
            cfg.append_config(
                ["user.name=Remote", "user.email=remote@example.com"],
                gix::config::Source::Api,
            )
            .unwrap();
            cfg.commit().unwrap();
        }
        let parent_id = bare
            .try_find_reference("refs/heads/main")
            .unwrap()
            .unwrap()
            .peel_to_id()
            .unwrap()
            .detach();
        let parent_tree = bare.find_commit(parent_id).unwrap().tree_id().unwrap().detach();
        let mut ed = bare.edit_tree(parent_tree).unwrap();
        let blob = bare
            .write_blob(b"pub fn shared() -> i32 { 555 }\npub fn other() -> i32 { 2 }\n")
            .unwrap()
            .detach();
        ed.upsert("src/lib.rs", gix::objs::tree::EntryKind::Blob, blob)
            .unwrap();
        let tree_id = ed.write().unwrap().detach();
        bare.commit("refs/heads/main", "remote conflict on shared", tree_id, [parent_id])
            .unwrap();
    }

    // Seal unpushed capsule in repo_a modifying `shared` differently
    let vp_local = match send_rpc(
        &repo_a,
        RpcMethod::SpawnViewport {
            name: "vp-local".into(),
            intent: "conflicting shared".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn: {other:?}"),
    };
    std::fs::write(
        vp_local.path.join("src/lib.rs"),
        "pub fn shared() -> i32 { 777 }\npub fn other() -> i32 { 2 }\n",
    )
    .unwrap();
    send_rpc(
        &repo_a,
        RpcMethod::SealCapsule {
            viewport_id: vp_local.id.clone(),
            verify_cmd: None,
            summary: Some("local sets shared=777".into()),
        },
    )
    .await
    .unwrap();

    // `git pull` must return Conflict and leave trunk, refs/heads/main, and capsules untouched!
    let pull_err = send_rpc(
        &repo_a,
        RpcMethod::GitPull {
            remote: Some(bare_dir.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(pull_err.0, RpcErrorCode::Conflict);
    assert_eq!(
        std::fs::read_to_string(repo_a.join("src/lib.rs")).unwrap(),
        "pub fn shared() -> i32 { 777 }\npub fn other() -> i32 { 2 }\n"
    );
    assert_eq!(
        gix::open(&repo_a).unwrap().head_id().unwrap().detach(),
        head_before
    );

    send_rpc(&repo_a, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(
        running_a.wait().await.unwrap(),
        DaemonRunExit::ShutdownRequested
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn m5_git_pull_nonlinear_merge_commit_skips_pre_loom_ancestors() {
    let tmp = TempDir::new().unwrap();
    let bare_dir = tmp.path().join("remote.git");
    let repo_b = tmp.path().join("repo_b");
    std::fs::create_dir_all(&repo_b).unwrap();

    // Initialize repo_b with pre-Loom history: C_root -> C_main, plus side branch S off C_root
    let (c_root, c_main, s_commit, merge_commit) = {
        let r = gix::init(&repo_b).unwrap();
        let sig = gix::actor::SignatureRef {
            name: "Dev".into(),
            email: "dev@example.com".into(),
            time: "1700000000 +0000",
        };
        let empty = gix::ObjectId::empty_tree(r.object_hash());

        let b_root = r.write_blob(b"pub fn root_fn() -> i32 { 1 }\n").unwrap().detach();
        let mut ed_root = r.edit_tree(empty).unwrap();
        ed_root
            .upsert("src/lib.rs", gix::objs::tree::EntryKind::Blob, b_root)
            .unwrap();
        let t_root = ed_root.write().unwrap().detach();
        let c_root = r
            .commit_as(
                sig,
                sig,
                "refs/heads/main",
                "pre-loom root",
                t_root,
                std::iter::empty::<gix::ObjectId>(),
            )
            .unwrap()
            .detach();

        let b_main = r
            .write_blob(b"pub fn root_fn() -> i32 { 1 }\npub fn main_fn() -> i32 { 2 }\n")
            .unwrap()
            .detach();
        let mut ed_main = r.edit_tree(t_root).unwrap();
        ed_main
            .upsert("src/lib.rs", gix::objs::tree::EntryKind::Blob, b_main)
            .unwrap();
        let t_main = ed_main.write().unwrap().detach();
        let c_main = r
            .commit_as(sig, sig, "refs/heads/main", "pre-loom main", t_main, [c_root])
            .unwrap()
            .detach();
        std::fs::write(repo_b.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(repo_b.join("src")).unwrap();
        std::fs::write(
            repo_b.join("src/lib.rs"),
            "pub fn root_fn() -> i32 { 1 }\npub fn main_fn() -> i32 { 2 }\n",
        )
        .unwrap();
        let mut idx = r.index_from_tree(&t_main).unwrap();
        idx.write(Default::default()).unwrap();

        // Create side branch commit S off c_root, and merge commit M(c_main, S) directly in bare_dir
        let bare = gix::init_bare(&bare_dir).unwrap();

        // Copy objects into bare with identical deterministic signature
        let b_r = bare.write_blob(b"pub fn root_fn() -> i32 { 1 }\n").unwrap().detach();
        let mut be_r = bare.edit_tree(empty).unwrap();
        be_r.upsert("src/lib.rs", gix::objs::tree::EntryKind::Blob, b_r).unwrap();
        let bt_r = be_r.write().unwrap().detach();
        let bc_root = bare
            .commit_as(
                sig,
                sig,
                "refs/heads/main",
                "pre-loom root",
                bt_r,
                std::iter::empty::<gix::ObjectId>(),
            )
            .unwrap()
            .detach();
        let b_m = bare
            .write_blob(b"pub fn root_fn() -> i32 { 1 }\npub fn main_fn() -> i32 { 2 }\n")
            .unwrap()
            .detach();
        let mut be_m = bare.edit_tree(bt_r).unwrap();
        be_m.upsert("src/lib.rs", gix::objs::tree::EntryKind::Blob, b_m).unwrap();
        let bt_m = be_m.write().unwrap().detach();
        let bc_main = bare
            .commit_as(sig, sig, "refs/heads/main", "pre-loom main", bt_m, [bc_root])
            .unwrap()
            .detach();
        assert_eq!(bc_root, c_root);
        assert_eq!(bc_main, c_main);

        // Side commit S off bc_root
        let b_side = bare.write_blob(b"pub fn side_fn() -> i32 { 3 }\n").unwrap().detach();
        let mut be_s = bare.edit_tree(bt_r).unwrap();
        be_s.upsert("src/side.rs", gix::objs::tree::EntryKind::Blob, b_side)
            .unwrap();
        let bt_s = be_s.write().unwrap().detach();
        let s_commit = bare
            .commit_as(
                sig,
                sig,
                "refs/heads/side",
                "side branch feature",
                bt_s,
                [bc_root],
            )
            .unwrap()
            .detach();

        // Merge commit M with parents [bc_main, s_commit]
        let mut be_merge = bare.edit_tree(bt_m).unwrap();
        be_merge
            .upsert("src/side.rs", gix::objs::tree::EntryKind::Blob, b_side)
            .unwrap();
        let bt_merge = be_merge.write().unwrap().detach();
        let merge_commit = bare
            .commit_as(
                sig,
                sig,
                "refs/heads/main",
                "merge side into main",
                bt_merge,
                [bc_main, s_commit],
            )
            .unwrap()
            .detach();

        (c_root, c_main, s_commit, merge_commit)
    };

    let running_b = match start_daemon(&repo_b, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon B should start"),
    };
    send_rpc(&repo_b, RpcMethod::Init { colocate_git: true })
        .await
        .unwrap();

    let pull: GitPullResult = match send_rpc(
        &repo_b,
        RpcMethod::GitPull {
            remote: Some(bare_dir.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::GitPull(r) => r,
        other => panic!("unexpected pull payload: {other:?}"),
    };

    let pulled = pull.pulled_commit_oids;
    let c_root_hex = c_root.to_hex().to_string();
    let c_main_hex = c_main.to_hex().to_string();
    let s_hex = s_commit.to_hex().to_string();
    let m_hex = merge_commit.to_hex().to_string();
    assert!(!pulled.contains(&c_root_hex), "must not traverse pre-loom root: {pulled:?}");
    assert!(!pulled.contains(&c_main_hex), "must not traverse old_local_tip: {pulled:?}");
    assert!(pulled.contains(&s_hex), "expected side commit: {pulled:?}");
    assert!(pulled.contains(&m_hex), "expected merge commit: {pulled:?}");
    assert!(repo_b.join("src/side.rs").is_file());

    send_rpc(&repo_b, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(
        running_b.wait().await.unwrap(),
        DaemonRunExit::ShutdownRequested
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capsule_seal_undo_restores_trunk_head_capsule_and_dropped_state() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::create_dir_all(repo.join("assets")).unwrap();
    let v1_rs = "pub fn compute() -> i32 { 1 }\n";
    let v1_bin = vec![1u8, 2, 3];
    std::fs::write(repo.join("src/lib.rs"), v1_rs).unwrap();
    std::fs::write(repo.join("assets/data.bin"), &v1_bin).unwrap();

    let (running, vp1) = init_and_spawn(repo, false, "vp1", "seal 1").await;

    // Seal 1: modifies src/lib.rs and creates src/added.rs
    let v2_rs = "pub fn compute() -> i32 { 2 }\n";
    std::fs::write(vp1.path.join("src/lib.rs"), v2_rs).unwrap();
    std::fs::write(vp1.path.join("src/added.rs"), "pub fn added() {}\n").unwrap();
    let cap1 = match send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp1.id.clone(),
            verify_cmd: None,
            summary: Some("seal 1".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::CapsuleSeal(c) => c,
        other => panic!("unexpected seal payload: {other:?}"),
    };
    let cap1_op_seq = running
        .ctx
        .db
        .list_ops(true, None, Some(1))
        .unwrap()
        .pop()
        .unwrap()
        .op_seq;
    assert_eq!(
        running.ctx.db.get_repo_meta().unwrap().unwrap().head_capsule_id.as_deref(),
        Some(cap1.capsule_id.as_str())
    );

    // Seal 2: modifies src/lib.rs and assets/data.bin
    let vp2 = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "vp2".into(),
            intent: "seal 2".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn payload: {other:?}"),
    };
    let v3_rs = "pub fn compute() -> i32 { 3 }\n";
    let v2_bin = vec![9u8, 9, 9, 9];
    std::fs::write(vp2.path.join("src/lib.rs"), v3_rs).unwrap();
    std::fs::write(vp2.path.join("assets/data.bin"), &v2_bin).unwrap();
    let cap2 = match send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp2.id.clone(),
            verify_cmd: None,
            summary: Some("seal 2".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::CapsuleSeal(c) => c,
        other => panic!("unexpected seal payload: {other:?}"),
    };
    assert_eq!(
        running.ctx.db.get_repo_meta().unwrap().unwrap().head_capsule_id.as_deref(),
        Some(cap2.capsule_id.as_str())
    );

    // Undo latest op (seal 2) -> reverts trunk to v2, restores head_capsule_id -> cap1, sets vp2 -> Dropped
    match send_rpc(repo, RpcMethod::Undo { op_seq: None })
        .await
        .unwrap()
    {
        RpcPayload::Undo(_) => {}
        other => panic!("unexpected undo payload: {other:?}"),
    }
    assert_eq!(std::fs::read_to_string(repo.join("src/lib.rs")).unwrap(), v2_rs);
    assert_eq!(std::fs::read(repo.join("assets/data.bin")).unwrap(), v1_bin);
    assert!(repo.join("src/added.rs").is_file());
    assert_eq!(
        running.ctx.db.get_repo_meta().unwrap().unwrap().head_capsule_id.as_deref(),
        Some(cap1.capsule_id.as_str())
    );
    assert_eq!(
        running.ctx.db.get_viewport(&vp2.id).unwrap().unwrap().state,
        ViewportState::Dropped
    );
    assert_eq!(running.ctx.idle_monitor.active_viewports(), 0);

    // GC with prune_viewports must prune BOTH vp1 (Sealed) and vp2 (Dropped), while retaining cap1 pre-seal CAS blobs
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
    assert_eq!(gc_res.pruned_viewport_dirs, 2);
    assert!(!vp1.path.exists());
    assert!(!vp2.path.exists());

    // Undo cap1 by op_seq even AFTER GC pruned vp1 -> reverts src/lib.rs -> v1, deletes src/added.rs, clears head_capsule_id
    match send_rpc(
        repo,
        RpcMethod::Undo {
            op_seq: Some(cap1_op_seq),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Undo(u) => assert_eq!(u.undone_op_seq, cap1_op_seq),
        other => panic!("unexpected undo payload: {other:?}"),
    }
    assert_eq!(std::fs::read_to_string(repo.join("src/lib.rs")).unwrap(), v1_rs);
    assert!(!repo.join("src/added.rs").exists());
    assert!(running.ctx.db.get_repo_meta().unwrap().unwrap().head_capsule_id.is_none());

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(
        running.wait().await.unwrap(),
        DaemonRunExit::ShutdownRequested
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capsule_seal_undo_rejects_corrupt_and_missing_cas_without_mutating_trunk() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("assets")).unwrap();
    let v1_bin = vec![10u8, 20, 30, 40];
    let v2_bin = vec![99u8, 88, 77, 66];
    std::fs::write(repo.join("assets/payload.bin"), &v1_bin).unwrap();

    let (running, vp1) = init_and_spawn(repo, false, "vp1", "binary seal").await;
    std::fs::write(vp1.path.join("assets/payload.bin"), &v2_bin).unwrap();
    let cap1 = match send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp1.id.clone(),
            verify_cmd: None,
            summary: Some("binary update".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::CapsuleSeal(c) => c,
        other => panic!("unexpected seal payload: {other:?}"),
    };

    // Corrupt all CAS objects on disk
    let cas_dir = repo.join(".loom/cas");
    for entry in std::fs::read_dir(&cas_dir).unwrap() {
        let entry = entry.unwrap();
        if entry.path().is_dir() {
            for blob in std::fs::read_dir(entry.path()).unwrap() {
                let blob = blob.unwrap();
                if blob.path().is_file() {
                    std::fs::write(blob.path(), b"corrupted-cas-bytes").unwrap();
                }
            }
        }
    }

    let corrupt_err = send_rpc(repo, RpcMethod::Undo { op_seq: None })
        .await
        .unwrap_err();
    assert_eq!(corrupt_err.0, RpcErrorCode::IoError);
    assert_eq!(std::fs::read(repo.join("assets/payload.bin")).unwrap(), v2_bin);
    assert_eq!(
        running.ctx.db.get_repo_meta().unwrap().unwrap().head_capsule_id.as_deref(),
        Some(cap1.capsule_id.as_str())
    );

    // Delete all CAS objects on disk
    for entry in std::fs::read_dir(&cas_dir).unwrap() {
        let entry = entry.unwrap();
        if entry.path().is_dir() {
            std::fs::remove_dir_all(entry.path()).unwrap();
        }
    }

    let missing_err = send_rpc(repo, RpcMethod::Undo { op_seq: None })
        .await
        .unwrap_err();
    assert_eq!(missing_err.0, RpcErrorCode::IoError);
    assert_eq!(std::fs::read(repo.join("assets/payload.bin")).unwrap(), v2_bin);
    assert_eq!(
        running.ctx.db.get_repo_meta().unwrap().unwrap().head_capsule_id.as_deref(),
        Some(cap1.capsule_id.as_str())
    );

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(
        running.wait().await.unwrap(),
        DaemonRunExit::ShutdownRequested
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn f22_git_pull_multi_commit_log_includes_all_absorbed_summaries() {
    let tmp = TempDir::new().unwrap();
    let bare_dir = tmp.path().join("remote.git");
    let repo_a = tmp.path().join("repo_a");
    let repo_b = tmp.path().join("repo_b");
    std::fs::create_dir_all(&repo_a).unwrap();
    std::fs::create_dir_all(&repo_b).unwrap();

    let _bare = gix::init_bare(&bare_dir).unwrap();
    init_git_with_initial_commit(&repo_a, &[("src/lib.rs", "pub fn seed() -> i32 { 1 }\n")]);
    let _repo_b_git = gix::init(&repo_b).unwrap();
    std::fs::write(repo_b.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

    let (running_a, vp_a) = init_and_spawn(&repo_a, true, "vp-a", "seed push").await;
    std::fs::write(vp_a.path.join("src/lib.rs"), "pub fn seed() -> i32 { 2 }\n").unwrap();
    let _ = send_rpc(
        &repo_a,
        RpcMethod::SealCapsule {
            viewport_id: vp_a.id.clone(),
            verify_cmd: None,
            summary: Some("seed v2".into()),
        },
    )
    .await
    .unwrap();
    let _ = send_rpc(
        &repo_a,
        RpcMethod::GitPush {
            remote: Some(bare_dir.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap();

    // Repo B pulls initial commit
    let running_b = match start_daemon(&repo_b, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon B should start"),
    };
    send_rpc(&repo_b, RpcMethod::Init { colocate_git: true })
        .await
        .unwrap();
    let _ = send_rpc(
        &repo_b,
        RpcMethod::GitPull {
            remote: Some(bare_dir.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap();

    // Add two consecutive external commits directly to bare_dir (first has leading blank line)
    {
        let mut bare = gix::open(&bare_dir).unwrap();
        {
            let mut cfg = bare.config_snapshot_mut();
            cfg.append_config(
                ["user.name=External Dev", "user.email=ext@example.com"],
                gix::config::Source::Api,
            )
            .unwrap();
            cfg.commit().unwrap();
        }
        let parent1 = bare
            .try_find_reference("refs/heads/main")
            .unwrap()
            .unwrap()
            .peel_to_id()
            .unwrap()
            .detach();
        let tree1_base = bare.find_commit(parent1).unwrap().tree_id().unwrap().detach();
        let mut ed1 = bare.edit_tree(tree1_base).unwrap();
        let b1 = bare
            .write_blob(b"pub fn ext_one() -> i32 { 10 }\n")
            .unwrap()
            .detach();
        ed1.upsert("src/ext_one.rs", gix::objs::tree::EntryKind::Blob, b1)
            .unwrap();
        let t1 = ed1.write().unwrap().detach();
        let c1 = bare
            .commit(
                "refs/heads/main",
                "\n  ext: first commit\n\nextra details",
                t1,
                [parent1],
            )
            .unwrap()
            .detach();

        let mut ed2 = bare.edit_tree(t1).unwrap();
        let b2 = bare
            .write_blob(b"pub fn ext_two() -> i32 { 20 }\n")
            .unwrap()
            .detach();
        ed2.upsert("src/ext_two.rs", gix::objs::tree::EntryKind::Blob, b2)
            .unwrap();
        let t2 = ed2.write().unwrap().detach();
        let _c2 = bare
            .commit("refs/heads/main", "ext: second commit", t2, [c1])
            .unwrap()
            .detach();
    }

    let pull_multi: GitPullResult = match send_rpc(
        &repo_b,
        RpcMethod::GitPull {
            remote: Some(bare_dir.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::GitPull(r) => r,
        other => panic!("unexpected pull payload: {other:?}"),
    };
    assert_eq!(pull_multi.pulled_commit_oids.len(), 2);
    assert_eq!(pull_multi.synthesized_capsule_ids.len(), 2);

    let logs = match send_rpc(
        &repo_b,
        RpcMethod::Log {
            capsules_only: false,
            symbol: None,
            limit: None,
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Log(entries) => entries,
        other => panic!("unexpected log payload: {other:?}"),
    };
    let latest_pull = logs
        .iter()
        .find(|e| e.kind == OpKind::GitPull)
        .expect("latest GitPull entry");
    assert!(
        latest_pull.summary.contains("ext: first commit")
            && latest_pull.summary.contains("ext: second commit"),
        "expected both absorbed commit summaries in GitPull log summary: {}",
        latest_pull.summary
    );

    let sym_logs = match send_rpc(
        &repo_b,
        RpcMethod::Log {
            capsules_only: true,
            symbol: Some("ext_one".into()),
            limit: None,
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Log(entries) => entries,
        other => panic!("unexpected symbol log payload: {other:?}"),
    };
    assert_eq!(sym_logs.len(), 1);
    assert!(sym_logs[0].summary.contains("ext: first commit"));

    send_rpc(&repo_a, RpcMethod::Shutdown).await.unwrap();
    send_rpc(&repo_b, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(
        running_a.wait().await.unwrap(),
        DaemonRunExit::ShutdownRequested
    );
    assert_eq!(
        running_b.wait().await.unwrap(),
        DaemonRunExit::ShutdownRequested
    );
}

#[tokio::test]
async fn seal_rejects_empty_and_whitespace_yaml_leaving_trunk_intact_and_viewport_active() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let orig_yaml = "service:\n  port: 8080\n";
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("config.yaml"), orig_yaml).unwrap();

    let (running, vp) = init_and_spawn(&repo, false, "yaml-bad", "truncate yaml").await;

    // 1. 0-byte config.yaml
    std::fs::write(vp.path.join("config.yaml"), b"").unwrap();
    let (code_empty, msg_empty) = send_rpc(
        &repo,
        RpcMethod::SealCapsule {
            viewport_id: vp.id.clone(),
            verify_cmd: None,
            summary: Some("empty yaml".into()),
        },
    )
    .await
    .expect_err("0-byte config.yaml must be rejected by seal");
    assert_eq!(code_empty, RpcErrorCode::Conflict, "unexpected error: {msg_empty}");
    assert_eq!(std::fs::read_to_string(repo.join("config.yaml")).unwrap(), orig_yaml);

    let list_after_empty = match send_rpc(&repo, RpcMethod::ListViewports { include_gc: false })
        .await
        .unwrap()
    {
        RpcPayload::Viewports(v) => v,
        other => panic!("unexpected list payload: {other:?}"),
    };
    assert_eq!(list_after_empty.len(), 1);
    assert_eq!(list_after_empty[0].state, ViewportState::Active);

    // 2. Whitespace-only config.yaml
    std::fs::write(vp.path.join("config.yaml"), "   \n  \n").unwrap();
    let (code_ws, msg_ws) = send_rpc(
        &repo,
        RpcMethod::SealCapsule {
            viewport_id: vp.id.clone(),
            verify_cmd: None,
            summary: Some("whitespace yaml".into()),
        },
    )
    .await
    .expect_err("whitespace-only config.yaml must be rejected by seal");
    assert_eq!(code_ws, RpcErrorCode::Conflict, "unexpected error: {msg_ws}");
    assert_eq!(std::fs::read_to_string(repo.join("config.yaml")).unwrap(), orig_yaml);

    let list_after_ws = match send_rpc(&repo, RpcMethod::ListViewports { include_gc: false })
        .await
        .unwrap()
    {
        RpcPayload::Viewports(v) => v,
        other => panic!("unexpected list payload: {other:?}"),
    };
    assert_eq!(list_after_ws.len(), 1);
    assert_eq!(list_after_ws[0].state, ViewportState::Active);

    send_rpc(&repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seal_verify_cmd_preserves_quoted_args_and_spaces() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn base() -> i32 { 1 }\n").unwrap();

    let (running, vp) = init_and_spawn(repo, false, "vp-quoted", "quoted verify cmd").await;

    std::fs::create_dir_all(vp.path.join("sub dir")).unwrap();
    std::fs::write(
        vp.path.join("sub dir/note file.txt"),
        "hello world priority\n",
    )
    .unwrap();
    std::fs::write(
        vp.path.join("src/lib.rs"),
        "pub fn base() -> i32 { 2 }\n",
    )
    .unwrap();

    let capsule = match send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp.id.clone(),
            verify_cmd: Some(
                r#"git grep --no-index -q "hello world" "sub dir/note file.txt""#.into(),
            ),
            summary: Some("quoted args and spaced path".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::CapsuleSeal(c) => c,
        other => panic!("expected CapsuleSeal, got {other:?}"),
    };

    let proof = capsule
        .verification_proof
        .as_ref()
        .expect("verification proof must be present");
    assert!(proof.passed);
    assert_eq!(proof.exit_code, 0);
    let repo_id = running.ctx.db.get_repo_meta().unwrap().unwrap().repo_id;
    assert!(verify_verification_proof(&repo_id, &capsule));
    assert_eq!(
        std::fs::read_to_string(repo.join("sub dir/note file.txt")).unwrap(),
        "hello world priority\n"
    );

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seal_verify_cmd_supports_and_chain_and_pipe() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn init() {}\n").unwrap();

    let (running, vp1) = init_and_spawn(repo, false, "vp-and-chain", "and chain verify").await;

    std::fs::write(
        vp1.path.join("src/lib.rs"),
        "pub fn first_fn() {}\npub fn second_fn() {}\n",
    )
    .unwrap();

    let cap1 = match send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp1.id.clone(),
            verify_cmd: Some(
                r#"git grep --no-index -q "first_fn" src/lib.rs && git grep --no-index -q "second_fn" src/lib.rs"#
                    .into(),
            ),
            summary: Some("and-chain verify".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::CapsuleSeal(c) => c,
        other => panic!("expected CapsuleSeal, got {other:?}"),
    };
    let proof1 = cap1.verification_proof.as_ref().unwrap();
    assert!(proof1.passed);
    assert_eq!(proof1.exit_code, 0);

    let vp2 = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "vp-pipe".into(),
            intent: "pipe verify".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn payload: {other:?}"),
    };
    std::fs::write(
        vp2.path.join("src/extra.rs"),
        "pub fn third_fn() -> i32 { 3 }\n",
    )
    .unwrap();

    #[cfg(windows)]
    let pipe_cmd = r#"echo "pipe_token_ok" | findstr "pipe_token_ok""#.to_string();
    #[cfg(unix)]
    let pipe_cmd = r#"echo "pipe_token_ok" | grep -q "pipe_token_ok""#.to_string();

    let cap2 = match send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp2.id.clone(),
            verify_cmd: Some(pipe_cmd),
            summary: Some("pipe verify".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::CapsuleSeal(c) => c,
        other => panic!("expected CapsuleSeal, got {other:?}"),
    };
    let proof2 = cap2.verification_proof.as_ref().unwrap();
    assert!(proof2.passed);
    assert_eq!(proof2.exit_code, 0);
    let repo_id = running.ctx.db.get_repo_meta().unwrap().unwrap().repo_id;
    assert!(verify_verification_proof(&repo_id, &cap1));
    assert!(verify_verification_proof(&repo_id, &cap2));

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seal_verify_cmd_quoted_failing_command_rolls_back_to_active() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let orig = "pub fn base() -> i32 { 1 }\n";
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), orig).unwrap();

    let (running, vp) = init_and_spawn(repo, false, "vp-quoted-fail", "failing quoted cmd").await;

    std::fs::write(
        vp.path.join("src/lib.rs"),
        "pub fn present_token() -> i32 { 42 }\n",
    )
    .unwrap();

    let err_missing = send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp.id.clone(),
            verify_cmd: Some(
                r#"git grep --no-index -q "nonexistent token" src/lib.rs"#.into(),
            ),
            summary: Some("should fail missing quoted token".into()),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err_missing.0, RpcErrorCode::InvalidState);
    assert_eq!(
        running.ctx.db.get_viewport(&vp.id).unwrap().unwrap().state,
        ViewportState::Active
    );
    assert_eq!(std::fs::read_to_string(repo.join("src/lib.rs")).unwrap(), orig);

    let err_chain = send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp.id.clone(),
            verify_cmd: Some(
                r#"git grep --no-index -q "present_token" src/lib.rs && exit 7"#.into(),
            ),
            summary: Some("should fail second command in chain".into()),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err_chain.0, RpcErrorCode::InvalidState);
    assert!(
        err_chain.1.contains("exit 7"),
        "expected exit 7 in error message, got: {}",
        err_chain.1
    );
    assert_eq!(
        running.ctx.db.get_viewport(&vp.id).unwrap().unwrap().state,
        ViewportState::Active
    );
    assert_eq!(std::fs::read_to_string(repo.join("src/lib.rs")).unwrap(), orig);

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn undo_viewport_spawn_and_drop_keeps_disk_and_radar_in_sync() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn base() -> i32 { 1 }\n").unwrap();

    let (running, vp1) = init_and_spawn(repo, false, "vp1", "undo spawn").await;
    assert!(vp1.path.is_dir());

    // Undo ViewportTransition + ViewportSpawn
    send_rpc(repo, RpcMethod::Undo { op_seq: None }).await.unwrap();
    send_rpc(repo, RpcMethod::Undo { op_seq: None }).await.unwrap();

    assert!(running.ctx.db.get_viewport(&vp1.id).unwrap().is_none());
    assert!(!vp1.path.exists(), "undone ViewportSpawn directory must be removed");
    let snap1 = match send_rpc(repo, RpcMethod::RadarSnapshot { viewport_id: None }).await.unwrap() {
        RpcPayload::RadarSnapshot(s) => s,
        other => panic!("{other:?}"),
    };
    assert!(!snap1.viewports.iter().any(|v| v.viewport_id == vp1.id));

    // Respawning same name succeeds cleanly
    let vp1_re = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "vp1".into(),
            intent: "respawn".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("{other:?}"),
    };

    // Drop vp1_re, then undo DropViewport -> must re-register in Radar
    send_rpc(
        repo,
        RpcMethod::DropViewport {
            viewport_id: vp1_re.id.clone(),
            reason: "temp dead end".into(),
            kind: None,
        },
    )
    .await
    .unwrap();
    send_rpc(repo, RpcMethod::Undo { op_seq: None }).await.unwrap();

    assert_eq!(
        running.ctx.db.get_viewport(&vp1_re.id).unwrap().unwrap().state,
        ViewportState::Active
    );
    let snap2 = match send_rpc(repo, RpcMethod::RadarSnapshot { viewport_id: None }).await.unwrap() {
        RpcPayload::RadarSnapshot(s) => s,
        other => panic!("{other:?}"),
    };
    assert!(
        snap2.viewports.iter().any(|v| v.viewport_id == vp1_re.id),
        "undone DropViewport must re-register Active viewport in Radar"
    );

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_push_failure_on_bare_ref_update_does_not_mutate_local_refs() {
    let tmp = TempDir::new().unwrap();
    let bare_dir = tmp.path().join("remote.git");
    let repo_a = tmp.path().join("repo_a");
    std::fs::create_dir_all(&repo_a).unwrap();

    let _bare = gix::init_bare(&bare_dir).unwrap();
    init_git_with_initial_commit(&repo_a, &[("src/lib.rs", "pub fn v() -> i32 { 1 }\n")]);

    let (running_a, vp) = init_and_spawn(&repo_a, true, "vp-push", "edit").await;
    std::fs::write(vp.path.join("src/lib.rs"), "pub fn v() -> i32 { 2 }\n").unwrap();
    let cap = match send_rpc(
        &repo_a,
        RpcMethod::SealCapsule {
            viewport_id: vp.id.clone(),
            verify_cmd: None,
            summary: Some("v2".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::CapsuleSeal(c) => c,
        other => panic!("{other:?}"),
    };

    let before_oid = gix::open(&repo_a).unwrap().head_id().unwrap().detach();
    std::fs::create_dir_all(bare_dir.join("refs/heads")).unwrap();
    std::fs::write(bare_dir.join("refs/heads/main.lock"), b"locked").unwrap();

    let err = send_rpc(
        &repo_a,
        RpcMethod::GitPush {
            remote: Some(bare_dir.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err.0, RpcErrorCode::Conflict);

    let after_oid = gix::open(&repo_a).unwrap().head_id().unwrap().detach();
    assert_eq!(
        before_oid, after_oid,
        "local refs/heads/main must not advance when bare_repo update fails"
    );
    assert!(
        running_a
            .ctx
            .db
            .get_capsule(&cap.capsule_id)
            .unwrap()
            .unwrap()
            .git_commit_oid
            .is_none()
    );

    let _ = std::fs::remove_file(bare_dir.join("refs/heads/main.lock"));
    send_rpc(&repo_a, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running_a.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn undo_after_gc_prune_cannot_resurrect_missing_viewport() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn v() -> i32 { 1 }\n").unwrap();

    let (daemon, vp) = init_and_spawn(repo, false, "vp-gc-undo", "drop then gc").await;
    std::fs::write(vp.path.join("src/lib.rs"), "pub fn v() -> i32 { 2 }\n").unwrap();

    send_rpc(
        repo,
        RpcMethod::DropViewport {
            viewport_id: vp.id.clone(),
            reason: "dead end".into(),
            kind: None,
        },
    )
    .await
    .unwrap();

    let ops = match send_rpc(
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
        RpcPayload::Log(entries) => entries,
        other => panic!("unexpected log payload: {other:?}"),
    };
    let drop_op_seq = ops
        .iter()
        .find(|e| e.kind == OpKind::ViewportDrop)
        .expect("ViewportDrop op must exist")
        .op_seq;

    send_rpc(repo, RpcMethod::Gc { prune_viewports: true })
        .await
        .unwrap();
    assert!(!vp.path.exists());

    // Neither `loom undo` (latest) nor `loom undo --op <drop_seq>` may resurrect `vp` as Active
    let undo_latest_err = send_rpc(repo, RpcMethod::Undo { op_seq: None })
        .await
        .unwrap_err();
    assert_eq!(undo_latest_err.0, RpcErrorCode::InvalidState);

    let undo_drop_err = send_rpc(
        repo,
        RpcMethod::Undo {
            op_seq: Some(drop_op_seq),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(undo_drop_err.0, RpcErrorCode::InvalidState);

    let vps = match send_rpc(repo, RpcMethod::ListViewports { include_gc: true })
        .await
        .unwrap()
    {
        RpcPayload::Viewports(v) => v,
        other => panic!("unexpected viewports payload: {other:?}"),
    };
    let rec = vps.into_iter().find(|v| v.id == vp.id).unwrap();
    assert_eq!(rec.state, ViewportState::GcPending);

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    let _ = daemon.wait().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn out_of_order_undo_of_older_capsule_seal_is_rejected_without_clobbering_trunk() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn v0() -> i32 { 0 }\n").unwrap();

    let running = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    let spawn = |name: &str| {
        let name = name.to_string();
        async move {
            match send_rpc(
                repo,
                RpcMethod::SpawnViewport {
                    name: name.clone(),
                    intent: name,
                    cmd: vec![],
                },
            )
            .await
            .unwrap()
            {
                RpcPayload::Viewport(v) => v,
                other => panic!("unexpected spawn payload: {other:?}"),
            }
        }
    };

    let vp1 = spawn("vp1").await;
    std::fs::write(vp1.path.join("src/lib.rs"), "pub fn v0() -> i32 { 10 }\n").unwrap();
    let cap1 = match send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp1.id.clone(),
            verify_cmd: None,
            summary: Some("cap1".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::CapsuleSeal(c) => c,
        other => panic!("unexpected seal payload: {other:?}"),
    };

    let vp2 = spawn("vp2").await;
    std::fs::write(vp2.path.join("src/lib.rs"), "pub fn v0() -> i32 { 20 }\n").unwrap();
    let _cap2 = send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp2.id.clone(),
            verify_cmd: None,
            summary: Some("cap2".into()),
        },
    )
    .await
    .unwrap();

    let ops = match send_rpc(
        repo,
        RpcMethod::Log {
            capsules_only: true,
            symbol: None,
            limit: None,
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Log(entries) => entries,
        other => panic!("unexpected log payload: {other:?}"),
    };
    let cap1_op_seq = ops
        .iter()
        .find(|e| e.target_id.as_deref() == Some(cap1.capsule_id.as_str()))
        .expect("cap1 seal op must exist")
        .op_seq;

    let err = send_rpc(
        repo,
        RpcMethod::Undo {
            op_seq: Some(cap1_op_seq),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err.0, RpcErrorCode::Conflict);
    assert_eq!(
        std::fs::read_to_string(repo.join("src/lib.rs")).unwrap(),
        "pub fn v0() -> i32 { 20 }\n"
    );

    // Also verify failed seal verification revert cannot leave viewport stuck in Verifying on undo
    let vp3 = spawn("vp3").await;
    std::fs::write(vp3.path.join("src/lib.rs"), "pub fn v0() -> i32 { 30 }\n").unwrap();
    let fail_err = send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp3.id.clone(),
            verify_cmd: Some("exit 7".into()),
            summary: Some("fail verify".into()),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(fail_err.0, RpcErrorCode::InvalidState);
    assert_eq!(
        running.ctx.db.get_viewport(&vp3.id).unwrap().unwrap().state,
        ViewportState::Active
    );
    send_rpc(repo, RpcMethod::Undo { op_seq: None }).await.unwrap();
    assert_ne!(
        running.ctx.db.get_viewport(&vp3.id).unwrap().unwrap().state,
        ViewportState::Verifying
    );

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    let _ = running.wait().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn undo_rejects_git_push_and_already_pushed_capsule_seal() {
    let tmp = TempDir::new().unwrap();
    let bare_dir = tmp.path().join("remote.git");
    let repo_dir = tmp.path().join("repo");
    std::fs::create_dir_all(&repo_dir).unwrap();

    let _bare = gix::init_bare(&bare_dir).unwrap();
    init_git_with_initial_commit(&repo_dir, &[("src/lib.rs", "pub fn v() -> i32 { 1 }\n")]);

    let (running, vp1) = init_and_spawn(&repo_dir, true, "vp1", "v2").await;
    std::fs::write(vp1.path.join("src/lib.rs"), "pub fn v() -> i32 { 2 }\n").unwrap();
    let cap1 = match send_rpc(
        &repo_dir,
        RpcMethod::SealCapsule {
            viewport_id: vp1.id.clone(),
            verify_cmd: None,
            summary: Some("cap1 v2".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::CapsuleSeal(c) => c,
        other => panic!("unexpected seal payload: {other:?}"),
    };

    let push1 = match send_rpc(
        &repo_dir,
        RpcMethod::GitPush {
            remote: Some(bare_dir.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::GitPush(p) => p,
        other => panic!("unexpected push payload: {other:?}"),
    };
    assert_eq!(push1.commits_created, 1);

    let ops = match send_rpc(
        &repo_dir,
        RpcMethod::Log {
            capsules_only: false,
            symbol: None,
            limit: None,
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Log(entries) => entries,
        other => panic!("unexpected log payload: {other:?}"),
    };
    let seal_op = ops
        .iter()
        .find(|e| e.kind == OpKind::CapsuleSeal)
        .expect("CapsuleSeal op must exist")
        .op_seq;
    let push_op = ops
        .iter()
        .find(|e| e.kind == OpKind::GitPush)
        .expect("GitPush op must exist")
        .op_seq;

    // 1. GitPush itself must be non-undoable
    let undo_push_err = send_rpc(
        &repo_dir,
        RpcMethod::Undo {
            op_seq: Some(push_op),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(undo_push_err.0, RpcErrorCode::InvalidState);

    // 2. Pushed CapsuleSeal cannot be undone (neither via latest nor explicit op_seq)
    let undo_seal_err = send_rpc(
        &repo_dir,
        RpcMethod::Undo {
            op_seq: Some(seal_op),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(undo_seal_err.0, RpcErrorCode::Conflict);

    assert_eq!(
        std::fs::read_to_string(repo_dir.join("src/lib.rs")).unwrap(),
        "pub fn v() -> i32 { 2 }\n"
    );
    assert!(
        running
            .ctx
            .db
            .get_capsule(&cap1.capsule_id)
            .unwrap()
            .is_some()
    );

    send_rpc(&repo_dir, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(
        running.wait().await.unwrap(),
        DaemonRunExit::ShutdownRequested
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn out_of_order_undo_cannot_resurrect_sealed_or_dropped_viewport_or_orphan_capsule() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn base() -> i32 { 1 }\n").unwrap();

    let (running, vp1) = init_and_spawn(repo, false, "vp1", "seal lifecycle").await;
    std::fs::write(vp1.path.join("src/lib.rs"), "pub fn base() -> i32 { 2 }\n").unwrap();
    let cap1 = match send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp1.id.clone(),
            verify_cmd: None,
            summary: Some("seal vp1".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::CapsuleSeal(c) => c,
        other => panic!("unexpected seal payload: {other:?}"),
    };

    let ops = match send_rpc(
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
        RpcPayload::Log(entries) => entries,
        other => panic!("unexpected log payload: {other:?}"),
    };
    let vp1_spawn_op = ops
        .iter()
        .find(|e| e.kind == OpKind::ViewportSpawn && e.viewport_id.as_deref() == Some(vp1.id.as_str()))
        .unwrap()
        .op_seq;
    let vp1_trans_op = ops
        .iter()
        .filter(|e| e.kind == OpKind::ViewportTransition && e.viewport_id.as_deref() == Some(vp1.id.as_str()))
        .map(|e| e.op_seq)
        .min()
        .unwrap();

    // Neither ViewportSpawn nor ViewportTransition for a Sealed viewport may be undone out of order
    let err_spawn = send_rpc(repo, RpcMethod::Undo { op_seq: Some(vp1_spawn_op) })
        .await
        .unwrap_err();
    assert!(matches!(err_spawn.0, RpcErrorCode::InvalidState | RpcErrorCode::Conflict));

    let err_trans = send_rpc(repo, RpcMethod::Undo { op_seq: Some(vp1_trans_op) })
        .await
        .unwrap_err();
    assert!(matches!(err_trans.0, RpcErrorCode::InvalidState | RpcErrorCode::Conflict));

    assert_eq!(
        running.ctx.db.get_viewport(&vp1.id).unwrap().unwrap().state,
        ViewportState::Sealed
    );
    assert!(running.ctx.db.get_capsule(&cap1.capsule_id).unwrap().is_some());

    // Spawn vp2, record an AstPatch via Radar, drop vp2 -> AstPatch undo must not resurrect Dropped vp2
    let vp2 = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "vp2".into(),
            intent: "drop lifecycle".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn payload: {other:?}"),
    };
    std::fs::write(vp2.path.join("src/lib.rs"), "pub fn base() -> i32 { 99 }\n").unwrap();
    wait_until(Duration::from_secs(3), || {
        running
            .ctx
            .db
            .list_ops(false, None, None)
            .unwrap_or_default()
            .iter()
            .any(|e| e.kind == OpKind::AstPatch && e.viewport_id.as_deref() == Some(vp2.id.as_str()))
    })
    .await;

    send_rpc(
        repo,
        RpcMethod::DropViewport {
            viewport_id: vp2.id.clone(),
            reason: "dead end".into(),
            kind: None,
        },
    )
    .await
    .unwrap();

    let ops2 = match send_rpc(
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
        RpcPayload::Log(entries) => entries,
        other => panic!("unexpected log payload: {other:?}"),
    };
    let vp2_ast_op = ops2
        .iter()
        .find(|e| e.kind == OpKind::AstPatch && e.viewport_id.as_deref() == Some(vp2.id.as_str()))
        .unwrap()
        .op_seq;

    let err_ast = send_rpc(repo, RpcMethod::Undo { op_seq: Some(vp2_ast_op) })
        .await
        .unwrap_err();
    assert!(matches!(err_ast.0, RpcErrorCode::InvalidState | RpcErrorCode::Conflict));
    assert_eq!(
        running.ctx.db.get_viewport(&vp2.id).unwrap().unwrap().state,
        ViewportState::Dropped
    );

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(
        running.wait().await.unwrap(),
        DaemonRunExit::ShutdownRequested
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_gc_does_not_delete_inflight_spawn_or_seal_cas_blobs() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn init() -> i32 { 0 }\n").unwrap();

    let running = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    let cas = loom_core::cas::CasStore::new(repo.join(".loom/cas"), 3).unwrap();

    for iter in 0..10 {
        std::fs::write(
            repo.join("src/lib.rs"),
            format!("pub fn iter_{iter}() -> i32 {{ {iter} }}\n"),
        )
        .unwrap();

        let repo_spawn = repo.to_path_buf();
        let spawn_task = tokio::spawn(async move {
            send_rpc(
                &repo_spawn,
                RpcMethod::SpawnViewport {
                    name: format!("vp_gc_{iter}"),
                    intent: "concurrent gc spawn".into(),
                    cmd: vec![],
                },
            )
            .await
        });
        let repo_gc = repo.to_path_buf();
        let gc_task = tokio::spawn(async move {
            for _ in 0..6 {
                let _ = send_rpc(&repo_gc, RpcMethod::Gc { prune_viewports: false }).await;
            }
        });

        let (spawn_res, _) = tokio::join!(spawn_task, gc_task);
        let vp = match spawn_res.unwrap().unwrap() {
            RpcPayload::Viewport(v) => v,
            other => panic!("unexpected spawn payload: {other:?}"),
        };
        let vp_rec = running.ctx.db.get_viewport(&vp.id).unwrap().unwrap();
        let base_snap = vp_rec.base_snapshot_cas.expect("base_snapshot_cas must exist");
        assert!(
            cas.contains(&base_snap),
            "base_snapshot_cas {base_snap} deleted by concurrent GC at iter={iter}"
        );

        std::fs::write(
            vp.path.join(format!("src/new_{iter}.rs")),
            format!("pub fn unique_{iter}() -> i32 {{ {iter} }}\n"),
        )
        .unwrap();

        let repo_seal = repo.to_path_buf();
        let vp_id = vp.id.clone();
        let seal_task = tokio::spawn(async move {
            send_rpc(
                &repo_seal,
                RpcMethod::SealCapsule {
                    viewport_id: vp_id,
                    verify_cmd: Some(format!("echo unique_proof_{iter}")),
                    summary: Some(format!("seal_{iter}")),
                },
            )
            .await
        });
        let repo_gc2 = repo.to_path_buf();
        let gc_task2 = tokio::spawn(async move {
            for _ in 0..6 {
                let _ = send_rpc(&repo_gc2, RpcMethod::Gc { prune_viewports: false }).await;
            }
        });

        let (seal_res, _) = tokio::join!(seal_task, gc_task2);
        let cap = match seal_res.unwrap().unwrap() {
            RpcPayload::CapsuleSeal(c) => c,
            other => panic!("unexpected seal payload: {other:?}"),
        };
        if let Some(ph) = cap.proof_cas_hash {
            assert!(
                cas.contains(&ph),
                "proof_cas_hash {ph} deleted by concurrent GC at iter={iter}"
            );
        }
        if let Some(sh) = cap.snapshot_cas_hash {
            assert!(
                cas.contains(&sh),
                "snapshot_cas_hash {sh} deleted by concurrent GC at iter={iter}"
            );
        }
    }

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(
        running.wait().await.unwrap(),
        DaemonRunExit::ShutdownRequested
    );
}

fn write_vp_file_atomic(vp_root: &Path, rel: &str, content: &str) {
    use std::io::Write;

    let loom_dir = vp_root.join(".loom");
    std::fs::create_dir_all(&loom_dir).unwrap();
    let dst = vp_root.join(rel);
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let mut tmp = tempfile::NamedTempFile::new_in(&loom_dir).unwrap();
    tmp.write_all(content.as_bytes()).unwrap();
    tmp.as_file().sync_data().unwrap();
    for attempt in 0..8 {
        match tmp.persist(&dst) {
            Ok(_) => return,
            Err(err) => {
                let is_sharing_violation = err.error.kind() == std::io::ErrorKind::PermissionDenied
                    || matches!(err.error.raw_os_error(), Some(5) | Some(32));
                if is_sharing_violation && attempt < 7 {
                    tmp = err.file;
                    std::thread::sleep(Duration::from_millis(5));
                } else {
                    panic!("atomic persist failed for {}: {}", dst.display(), err.error);
                }
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn out_of_order_ast_patch_and_transition_undo_rejected_preserving_patch_chain() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/a.rs"), "pub fn a() -> i32 { 1 }\n").unwrap();
    std::fs::write(repo.join("src/b.rs"), "pub fn b() -> i32 { 2 }\n").unwrap();

    let (running, vp) = init_and_spawn(repo, false, "vp-ooo-ast", "ooo ast patch").await;

    write_vp_file_atomic(&vp.path, "src/a.rs", "pub fn a() -> i32 { 10 }\n");
    running.ctx.radar.flush_viewport(&vp.id).await;
    write_vp_file_atomic(&vp.path, "src/b.rs", "pub fn b() -> i32 { 20 }\n");
    running.ctx.radar.flush_viewport(&vp.id).await;

    let ast_ops: Vec<_> = running
        .ctx
        .db
        .list_ops(false, None, None)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == OpKind::AstPatch && e.viewport_id.as_deref() == Some(vp.id.as_str()))
        .collect();
    assert!(
        ast_ops.len() >= 2,
        "expected at least 2 AstPatch ops, got {}",
        ast_ops.len()
    );
    let first_ast_op = ast_ops.last().unwrap().op_seq;
    let second_ast_op = ast_ops[0].op_seq;

    // 1. Out-of-order undo of first AstPatch must be rejected with Conflict
    let err = send_rpc(
        repo,
        RpcMethod::Undo {
            op_seq: Some(first_ast_op),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err.0, RpcErrorCode::Conflict);
    let vp_now = running.ctx.db.get_viewport(&vp.id).unwrap().unwrap();
    assert_eq!(vp_now.patch_ids.len(), ast_ops.len());
    for pid in &vp_now.patch_ids {
        assert!(running.ctx.db.get_patch(pid).unwrap().is_some());
    }

    // 2. Active ViewportTransition (`RestoreViewportClean`) followed by another AstPatch:
    //    out-of-order undo of the Active transition must be rejected with Conflict.
    send_rpc(
        repo,
        RpcMethod::RestoreViewportClean {
            viewport_id: vp.id.clone(),
        },
    )
    .await
    .unwrap();
    let restore_clean_op = running
        .ctx
        .db
        .list_ops(false, None, None)
        .unwrap()
        .into_iter()
        .find(|e| {
            e.kind == OpKind::ViewportTransition
                && e.viewport_id.as_deref() == Some(vp.id.as_str())
                && e.summary.contains("Refresh clean base snapshot")
        })
        .expect("RestoreViewportClean ViewportTransition op must exist")
        .op_seq;

    write_vp_file_atomic(&vp.path, "src/a.rs", "pub fn a() -> i32 { 30 }\n");
    running.ctx.radar.flush_viewport(&vp.id).await;

    let err_restore_clean = send_rpc(
        repo,
        RpcMethod::Undo {
            op_seq: Some(restore_clean_op),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err_restore_clean.0, RpcErrorCode::Conflict);

    // 3. NegativeEdgeRecord(restore_clean=true) followed by AstPatch: out-of-order undo rejected.
    send_rpc(
        repo,
        RpcMethod::McpRecordDeadendEx {
            viewport_id: vp.id.clone(),
            touched_symbols: vec!["a".into()],
            kind: "DesignDeadEnd".into(),
            lesson_summary: "bad idea".into(),
            restore_clean: true,
        },
    )
    .await
    .unwrap();
    let neg_clean_op = running
        .ctx
        .db
        .list_ops(false, None, None)
        .unwrap()
        .into_iter()
        .find(|e| {
            e.kind == OpKind::NegativeEdgeRecord
                && e.viewport_id.as_deref() == Some(vp.id.as_str())
        })
        .expect("NegativeEdgeRecord(restore_clean=true) must exist")
        .op_seq;

    write_vp_file_atomic(&vp.path, "src/b.rs", "pub fn b() -> i32 { 40 }\n");
    running.ctx.radar.flush_viewport(&vp.id).await;

    let err_neg_clean = send_rpc(
        repo,
        RpcMethod::Undo {
            op_seq: Some(neg_clean_op),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err_neg_clean.0, RpcErrorCode::Conflict);

    // 4. NegativeEdgeRecord(restore_clean=false) has empty restore_vps and remains independently undoable
    //    even before an earlier AstPatch? Wait: record it before a new AstPatch, then undo it independently.
    send_rpc(
        repo,
        RpcMethod::McpRecordDeadend {
            viewport_id: vp.id.clone(),
            touched_symbols: vec!["b".into()],
            kind: "Other".into(),
            lesson_summary: "independent note".into(),
        },
    )
    .await
    .unwrap();
    let neg_note_op = running
        .ctx
        .db
        .list_ops(false, None, None)
        .unwrap()
        .into_iter()
        .find(|e| {
            e.kind == OpKind::NegativeEdgeRecord
                && e.viewport_id.as_deref() == Some(vp.id.as_str())
                && e.summary.contains("independent note")
        })
        .expect("NegativeEdgeRecord(restore_clean=false) must exist")
        .op_seq;

    write_vp_file_atomic(&vp.path, "src/a.rs", "pub fn a() -> i32 { 50 }\n");
    running.ctx.radar.flush_viewport(&vp.id).await;

    send_rpc(
        repo,
        RpcMethod::Undo {
            op_seq: Some(neg_note_op),
        },
    )
    .await
    .unwrap();

    // 5. Reverse-order undo of the remaining latest ops succeeds cleanly without dangling patch_ids
    while let Ok(RpcPayload::Undo(_)) = send_rpc(repo, RpcMethod::Undo { op_seq: None }).await {
        let cur = running.ctx.db.get_viewport(&vp.id).unwrap();
        if let Some(v) = cur {
            for pid in &v.patch_ids {
                assert!(
                    running.ctx.db.get_patch(pid).unwrap().is_some(),
                    "dangling patch_id {pid} after undo"
                );
            }
        } else {
            break;
        }
    }
    let _ = (second_ast_op, first_ast_op);

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(
        running.wait().await.unwrap(),
        DaemonRunExit::ShutdownRequested
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn undo_blocked_while_verifying_and_failed_seal_skips_internal_verifying_op() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn v() -> i32 { 1 }\n").unwrap();

    let (running, vp) = init_and_spawn(repo, false, "vp-verify-undo", "verify undo race").await;
    write_vp_file_atomic(&vp.path, "src/lib.rs", "pub fn v() -> i32 { 2 }\n");
    running.ctx.radar.flush_viewport(&vp.id).await;

    let ast_op_seq = running
        .ctx
        .db
        .list_ops(false, None, None)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == OpKind::AstPatch && e.viewport_id.as_deref() == Some(vp.id.as_str()))
        .expect("AstPatch op must exist")
        .op_seq;

    // 1. Failed seal --verify must not leave Active->Verifying as latest undoable op
    let fail_err = send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp.id.clone(),
            verify_cmd: Some("exit 1".into()),
            summary: Some("fail".into()),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(fail_err.0, RpcErrorCode::InvalidState);

    let undo_after_fail = match send_rpc(repo, RpcMethod::Undo { op_seq: None }).await.unwrap() {
        RpcPayload::Undo(u) => u,
        other => panic!("unexpected undo payload: {other:?}"),
    };
    assert_eq!(
        undo_after_fail.undone_op_seq, ast_op_seq,
        "undo after failed seal must undo the user's AstPatch, not internal Active->Verifying"
    );

    // 2. Concurrent undo while seal_capsule is in Verifying must fail with Conflict and not abort seal
    write_vp_file_atomic(&vp.path, "src/lib.rs", "pub fn v() -> i32 { 3 }\n");
    running.ctx.radar.flush_viewport(&vp.id).await;

    #[cfg(windows)]
    let slow_cmd = "ping -n 2 127.0.0.1 > NUL".to_string();
    #[cfg(unix)]
    let slow_cmd = "sleep 0.5".to_string();

    let repo_seal = repo.to_path_buf();
    let vp_id_seal = vp.id.clone();
    let seal_task = tokio::spawn(async move {
        send_rpc(
            &repo_seal,
            RpcMethod::SealCapsule {
                viewport_id: vp_id_seal,
                verify_cmd: Some(slow_cmd),
                summary: Some("concurrent seal".into()),
            },
        )
        .await
    });

    wait_until(Duration::from_secs(3), || {
        running
            .ctx
            .db
            .get_viewport(&vp.id)
            .ok()
            .flatten()
            .is_some_and(|v| v.state == ViewportState::Verifying)
    })
    .await;

    let undo_during_verify = send_rpc(repo, RpcMethod::Undo { op_seq: None })
        .await
        .unwrap_err();
    assert_eq!(undo_during_verify.0, RpcErrorCode::Conflict);

    let seal_res = seal_task.await.unwrap().unwrap();
    assert!(matches!(seal_res, RpcPayload::CapsuleSeal(_)));

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(
        running.wait().await.unwrap(),
        DaemonRunExit::ShutdownRequested
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sequential_undo_after_capsule_seal_undo_reaches_earlier_capsule_without_wedging() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn v() -> i32 { 0 }\n").unwrap();

    let (running, vp1) = init_and_spawn(repo, false, "vp1", "cap1").await;
    write_vp_file_atomic(&vp1.path, "src/lib.rs", "pub fn v() -> i32 { 1 }\n");
    running.ctx.radar.flush_viewport(&vp1.id).await;
    send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp1.id.clone(),
            verify_cmd: None,
            summary: Some("cap1".into()),
        },
    )
    .await
    .unwrap();

    let vp2 = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "vp2".into(),
            intent: "cap2".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("{other:?}"),
    };
    write_vp_file_atomic(&vp2.path, "src/lib.rs", "pub fn v() -> i32 { 2 }\n");
    running.ctx.radar.flush_viewport(&vp2.id).await;
    send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp2.id.clone(),
            verify_cmd: None,
            summary: Some("cap2".into()),
        },
    )
    .await
    .unwrap();

    // First undo(None) undoes cap2
    send_rpc(repo, RpcMethod::Undo { op_seq: None }).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.join("src/lib.rs")).unwrap(),
        "pub fn v() -> i32 { 1 }\n"
    );

    // Second undo(None) must undo cap1 rather than wedging on vp2's pre-seal ops
    send_rpc(repo, RpcMethod::Undo { op_seq: None }).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.join("src/lib.rs")).unwrap(),
        "pub fn v() -> i32 { 0 }\n"
    );
    assert!(
        running
            .ctx
            .db
            .get_repo_meta()
            .unwrap()
            .unwrap()
            .head_capsule_id
            .is_none()
    );

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(
        running.wait().await.unwrap(),
        DaemonRunExit::ShutdownRequested
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn undo_capsule_seal_rejects_external_trunk_modification_without_clobber() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn v() -> i32 { 1 }\n").unwrap();
    std::fs::write(repo.join("src/unrelated.rs"), "pub fn u() -> i32 { 1 }\n").unwrap();

    let (running, vp1) = init_and_spawn(repo, false, "vp1", "edit lib").await;
    write_vp_file_atomic(&vp1.path, "src/lib.rs", "pub fn v() -> i32 { 2 }\n");
    write_vp_file_atomic(&vp1.path, "src/added.rs", "pub fn added() -> i32 { 10 }\n");
    running.ctx.radar.flush_viewport(&vp1.id).await;

    let cap1 = match send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp1.id.clone(),
            verify_cmd: None,
            summary: Some("cap1".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::CapsuleSeal(c) => c,
        other => panic!("{other:?}"),
    };

    // External trunk edits on files touched by cap1 must block undo with Conflict
    std::fs::write(repo.join("src/lib.rs"), "pub fn v() -> i32 { 999 }\n").unwrap();
    std::fs::write(repo.join("src/added.rs"), "pub fn added() -> i32 { 888 }\n").unwrap();
    std::fs::write(repo.join("src/unrelated.rs"), "pub fn u() -> i32 { 777 }\n").unwrap();

    let err = send_rpc(repo, RpcMethod::Undo { op_seq: None })
        .await
        .unwrap_err();
    assert_eq!(err.0, RpcErrorCode::Conflict);
    assert_eq!(
        std::fs::read_to_string(repo.join("src/lib.rs")).unwrap(),
        "pub fn v() -> i32 { 999 }\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("src/added.rs")).unwrap(),
        "pub fn added() -> i32 { 888 }\n"
    );
    assert!(running.ctx.db.get_capsule(&cap1.capsule_id).unwrap().is_some());

    // Restoring touched files back to post-seal state allows undo while preserving unrelated external edits
    std::fs::write(repo.join("src/lib.rs"), "pub fn v() -> i32 { 2 }\n").unwrap();
    std::fs::write(repo.join("src/added.rs"), "pub fn added() -> i32 { 10 }\n").unwrap();
    send_rpc(repo, RpcMethod::Undo { op_seq: None }).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.join("src/lib.rs")).unwrap(),
        "pub fn v() -> i32 { 1 }\n"
    );
    assert!(!repo.join("src/added.rs").exists());
    assert_eq!(
        std::fs::read_to_string(repo.join("src/unrelated.rs")).unwrap(),
        "pub fn u() -> i32 { 777 }\n"
    );

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(
        running.wait().await.unwrap(),
        DaemonRunExit::ShutdownRequested
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn undo_after_git_pull_rebase_cannot_revert_viewport_base_snapshot() {
    let tmp = TempDir::new().unwrap();
    let bare = tmp.path().join("bare.git");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let _bare = gix::init_bare(&bare).unwrap();
    init_git_with_initial_commit(
        &repo,
        &[(
            "src/lib.rs",
            "pub fn f1() -> i32 { 1 }\npub fn f2() -> i32 { 2 }\n",
        )],
    );

    let (running, vp) = init_and_spawn(&repo, true, "vp-active", "edit f1").await;
    let vp_base_before = running
        .ctx
        .db
        .get_viewport(&vp.id)
        .unwrap()
        .unwrap()
        .base_snapshot_cas;

    let pre_pull_ops = running.ctx.db.list_ops(false, None, None).unwrap();
    let vp_spawn_op = pre_pull_ops
        .iter()
        .find(|e| e.kind == OpKind::ViewportSpawn && e.viewport_id.as_deref() == Some(vp.id.as_str()))
        .expect("ViewportSpawn op must exist")
        .op_seq;
    let vp_trans_op = pre_pull_ops
        .iter()
        .find(|e| {
            e.kind == OpKind::ViewportTransition
                && e.viewport_id.as_deref() == Some(vp.id.as_str())
        })
        .expect("ViewportTransition op must exist")
        .op_seq;

    send_rpc(
        &repo,
        RpcMethod::GitPush {
            remote: Some(bare.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap();

    // Advance remote bare repo with f2 = 200
    {
        let mut bare_rw = gix::open(&bare).unwrap();
        {
            let mut cfg = bare_rw.config_snapshot_mut();
            cfg.append_config(
                ["user.name=Remote", "user.email=remote@example.com"],
                gix::config::Source::Api,
            )
            .unwrap();
            cfg.commit().unwrap();
        }
        let pid = bare_rw
            .try_find_reference("refs/heads/main")
            .unwrap()
            .unwrap()
            .peel_to_id()
            .unwrap()
            .detach();
        let ptree = bare_rw.find_commit(pid).unwrap().tree_id().unwrap().detach();
        let mut ed = bare_rw.edit_tree(ptree).unwrap();
        let blob = bare_rw
            .write_blob(b"pub fn f1() -> i32 { 1 }\npub fn f2() -> i32 { 200 }\n")
            .unwrap()
            .detach();
        ed.upsert("src/lib.rs", gix::objs::tree::EntryKind::Blob, blob)
            .unwrap();
        let tree = ed.write().unwrap().detach();
        bare_rw
            .commit("refs/heads/main", "remote f2=200", tree, [pid])
            .unwrap();
    }

    // Pull updates trunk and cleanly rebases active `vp`
    let pull = match send_rpc(
        &repo,
        RpcMethod::GitPull {
            remote: Some(bare.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::GitPull(p) => p,
        other => panic!("unexpected pull payload: {other:?}"),
    };
    assert_eq!(pull.rebased_viewports.len(), 1);
    assert_eq!(
        pull.rebased_viewports[0].outcome,
        loom_core::rpc::ViewportRebaseOutcome::Clean
    );
    let vp_base_after = running
        .ctx
        .db
        .get_viewport(&vp.id)
        .unwrap()
        .unwrap()
        .base_snapshot_cas;
    assert_ne!(vp_base_before, vp_base_after);

    // 1. Immediate `loom undo` (latest) and explicit `undo --op` on pre-pull ops must NOT revert `vp.base_snapshot_cas`
    let undo_err = send_rpc(&repo, RpcMethod::Undo { op_seq: None })
        .await
        .unwrap_err();
    assert_eq!(undo_err.0, RpcErrorCode::NotFound);

    let err_spawn = send_rpc(
        &repo,
        RpcMethod::Undo {
            op_seq: Some(vp_spawn_op),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err_spawn.0, RpcErrorCode::Conflict);

    let err_trans = send_rpc(
        &repo,
        RpcMethod::Undo {
            op_seq: Some(vp_trans_op),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err_trans.0, RpcErrorCode::Conflict);

    assert_eq!(
        running
            .ctx
            .db
            .get_viewport(&vp.id)
            .unwrap()
            .unwrap()
            .base_snapshot_cas,
        vp_base_after
    );

    // 2. Post-pull AstPatch on the rebased viewport is undoable, and subsequent `undo(None)` stops at the GitPull barrier
    write_vp_file_atomic(
        &vp.path,
        "src/lib.rs",
        "pub fn f1() -> i32 { 10 }\npub fn f2() -> i32 { 200 }\n",
    );
    running.ctx.radar.flush_viewport(&vp.id).await;
    let post_pull_ast_op = running
        .ctx
        .db
        .list_ops(false, None, None)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == OpKind::AstPatch && e.viewport_id.as_deref() == Some(vp.id.as_str()))
        .expect("post-pull AstPatch op must exist")
        .op_seq;

    let undo_post_ast = match send_rpc(&repo, RpcMethod::Undo { op_seq: None })
        .await
        .unwrap()
    {
        RpcPayload::Undo(u) => u,
        other => panic!("unexpected undo payload: {other:?}"),
    };
    assert_eq!(undo_post_ast.undone_op_seq, post_pull_ast_op);

    let undo_after_post_ast_err = send_rpc(&repo, RpcMethod::Undo { op_seq: None })
        .await
        .unwrap_err();
    assert_eq!(undo_after_post_ast_err.0, RpcErrorCode::NotFound);
    assert_eq!(
        running
            .ctx
            .db
            .get_viewport(&vp.id)
            .unwrap()
            .unwrap()
            .base_snapshot_cas,
        vp_base_after
    );

    // 3. Editing pulled function f2 (200 -> 201) and sealing must succeed without false StructuralConflict
    write_vp_file_atomic(
        &vp.path,
        "src/lib.rs",
        "pub fn f1() -> i32 { 1 }\npub fn f2() -> i32 { 201 }\n",
    );
    running.ctx.radar.flush_viewport(&vp.id).await;
    let cap = match send_rpc(
        &repo,
        RpcMethod::SealCapsule {
            viewport_id: vp.id.clone(),
            verify_cmd: None,
            summary: Some("edit f2 post-pull".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::CapsuleSeal(c) => c,
        other => panic!("unexpected seal payload: {other:?}"),
    };
    assert!(cap.touched_files.iter().any(|p| p.ends_with("lib.rs")));
    assert_eq!(
        std::fs::read_to_string(repo.join("src/lib.rs")).unwrap(),
        "pub fn f1() -> i32 { 1 }\npub fn f2() -> i32 { 201 }\n"
    );

    send_rpc(&repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(
        running.wait().await.unwrap(),
        DaemonRunExit::ShutdownRequested
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn undo_after_git_pull_seal_git_push_skips_pushed_capsule_without_wedging() {
    let tmp = TempDir::new().unwrap();
    let bare = tmp.path().join("bare.git");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let _bare = gix::init_bare(&bare).unwrap();
    init_git_with_initial_commit(
        &repo,
        &[
            ("src/a.rs", "pub fn a() -> i32 { 1 }\n"),
            ("src/b.rs", "pub fn b() -> i32 { 1 }\n"),
            ("src/c.rs", "pub fn c() -> i32 { 1 }\n"),
        ],
    );

    let (running, vp1) = init_and_spawn(&repo, true, "vp1", "edit a").await;
    send_rpc(
        &repo,
        RpcMethod::GitPush {
            remote: Some(bare.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap();

    // Advance remote bare on src/c.rs (c = 200)
    {
        let mut bare_rw = gix::open(&bare).unwrap();
        {
            let mut cfg = bare_rw.config_snapshot_mut();
            cfg.append_config(
                ["user.name=Remote", "user.email=remote@example.com"],
                gix::config::Source::Api,
            )
            .unwrap();
            cfg.commit().unwrap();
        }
        let pid = bare_rw
            .try_find_reference("refs/heads/main")
            .unwrap()
            .unwrap()
            .peel_to_id()
            .unwrap()
            .detach();
        let ptree = bare_rw.find_commit(pid).unwrap().tree_id().unwrap().detach();
        let mut ed = bare_rw.edit_tree(ptree).unwrap();
        let blob = bare_rw
            .write_blob(b"pub fn c() -> i32 { 200 }\n")
            .unwrap()
            .detach();
        ed.upsert("src/c.rs", gix::objs::tree::EntryKind::Blob, blob)
            .unwrap();
        let tree = ed.write().unwrap().detach();
        bare_rw
            .commit("refs/heads/main", "remote c=200", tree, [pid])
            .unwrap();
    }

    // 1. git_pull (rebases vp1)
    send_rpc(
        &repo,
        RpcMethod::GitPull {
            remote: Some(bare.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap();

    // 2. Active peer vp2 records an AstPatch prior to vp1 seal + git_push
    let vp2 = match send_rpc(
        &repo,
        RpcMethod::SpawnViewport {
            name: "vp2".into(),
            intent: "edit b".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("{other:?}"),
    };
    write_vp_file_atomic(&vp2.path, "src/b.rs", "pub fn b() -> i32 { 55 }\n");
    running.ctx.radar.flush_viewport(&vp2.id).await;
    let vp2_ast_seq = running
        .ctx
        .db
        .list_ops(false, None, None)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == OpKind::AstPatch && e.viewport_id.as_deref() == Some(vp2.id.as_str()))
        .expect("vp2 AstPatch op must exist")
        .op_seq;

    // 3. Seal vp1 and git_push
    write_vp_file_atomic(&vp1.path, "src/a.rs", "pub fn a() -> i32 { 99 }\n");
    running.ctx.radar.flush_viewport(&vp1.id).await;
    send_rpc(
        &repo,
        RpcMethod::SealCapsule {
            viewport_id: vp1.id.clone(),
            verify_cmd: None,
            summary: Some("seal a=99".into()),
        },
    )
    .await
    .unwrap();
    let seal_seq = running
        .ctx
        .db
        .list_ops(true, None, Some(1))
        .unwrap()
        .pop()
        .unwrap()
        .op_seq;

    send_rpc(
        &repo,
        RpcMethod::GitPush {
            remote: Some(bare.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap();

    // Explicit `undo --op <seal_seq>` must still return Conflict
    assert_eq!(
        send_rpc(&repo, RpcMethod::Undo { op_seq: Some(seal_seq) })
            .await
            .unwrap_err()
            .0,
        RpcErrorCode::Conflict
    );

    // `undo(None)` must skip the pushed CapsuleSeal and undo vp2's earlier AstPatch,
    // then vp2's transition/spawn, then hit NotFound at the GitPull barrier!
    let undone_ast = match send_rpc(&repo, RpcMethod::Undo { op_seq: None }).await.unwrap() {
        RpcPayload::Undo(u) => u,
        other => panic!("{other:?}"),
    };
    assert_eq!(undone_ast.undone_op_seq, vp2_ast_seq);
    send_rpc(&repo, RpcMethod::Undo { op_seq: None }).await.unwrap();
    send_rpc(&repo, RpcMethod::Undo { op_seq: None }).await.unwrap();
    assert!(running.ctx.db.get_viewport(&vp2.id).unwrap().is_none());

    let no_more = send_rpc(&repo, RpcMethod::Undo { op_seq: None })
        .await
        .unwrap_err();
    assert_eq!(no_more.0, RpcErrorCode::NotFound);

    // 4. Verify `seal -> git_pull` with unrebased active peer viewport skips `seal`
    //    and reaches the peer viewport's op without wedging!
    let vp3 = match send_rpc(
        &repo,
        RpcMethod::SpawnViewport {
            name: "vp3".into(),
            intent: "edit c conflict".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("{other:?}"),
    };
    let vp4 = match send_rpc(
        &repo,
        RpcMethod::SpawnViewport {
            name: "vp4".into(),
            intent: "edit a unpushed".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("{other:?}"),
    };

    // vp3 modifies src/c.rs (which remote will also modify so vp3 stays unrebased after git_pull)
    write_vp_file_atomic(&vp3.path, "src/c.rs", "pub fn c() -> i32 { 777 }\n");
    running.ctx.radar.flush_viewport(&vp3.id).await;
    let vp3_ast_seq = running
        .ctx
        .db
        .list_ops(false, None, None)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == OpKind::AstPatch && e.viewport_id.as_deref() == Some(vp3.id.as_str()))
        .expect("vp3 AstPatch op must exist")
        .op_seq;

    // vp4 modifies src/a.rs and seals (unpushed CapsuleSeal)
    write_vp_file_atomic(&vp4.path, "src/a.rs", "pub fn a() -> i32 { 123 }\n");
    running.ctx.radar.flush_viewport(&vp4.id).await;
    send_rpc(
        &repo,
        RpcMethod::SealCapsule {
            viewport_id: vp4.id.clone(),
            verify_cmd: None,
            summary: Some("seal a=123".into()),
        },
    )
    .await
    .unwrap();
    let seal2_seq = running
        .ctx
        .db
        .list_ops(true, None, Some(1))
        .unwrap()
        .pop()
        .unwrap()
        .op_seq;

    // Advance remote bare on src/c.rs (c = 300) so git_pull leaves vp3 unrebased
    {
        let mut bare_rw = gix::open(&bare).unwrap();
        {
            let mut cfg = bare_rw.config_snapshot_mut();
            cfg.append_config(
                ["user.name=Remote", "user.email=remote@example.com"],
                gix::config::Source::Api,
            )
            .unwrap();
            cfg.commit().unwrap();
        }
        let pid = bare_rw
            .try_find_reference("refs/heads/main")
            .unwrap()
            .unwrap()
            .peel_to_id()
            .unwrap()
            .detach();
        let ptree = bare_rw.find_commit(pid).unwrap().tree_id().unwrap().detach();
        let mut ed = bare_rw.edit_tree(ptree).unwrap();
        let blob = bare_rw
            .write_blob(b"pub fn c() -> i32 { 300 }\n")
            .unwrap()
            .detach();
        ed.upsert("src/c.rs", gix::objs::tree::EntryKind::Blob, blob)
            .unwrap();
        let tree = ed.write().unwrap().detach();
        bare_rw
            .commit("refs/heads/main", "remote c=300", tree, [pid])
            .unwrap();
    }

    let pull2 = match send_rpc(
        &repo,
        RpcMethod::GitPull {
            remote: Some(bare.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::GitPull(p) => p,
        other => panic!("unexpected pull payload: {other:?}"),
    };
    assert_eq!(pull2.rebased_viewports.len(), 1);
    assert_eq!(
        pull2.rebased_viewports[0].outcome,
        ViewportRebaseOutcome::Conflict
    );

    // Explicit `undo --op <seal2_seq>` must still return Conflict due to later GitPull trunk op
    assert_eq!(
        send_rpc(&repo, RpcMethod::Undo { op_seq: Some(seal2_seq) })
            .await
            .unwrap_err()
            .0,
        RpcErrorCode::Conflict
    );

    // `undo(None)` must skip the GitPull-barriered unpushed CapsuleSeal and undo vp3's AstPatch
    let undone_vp3_ast = match send_rpc(&repo, RpcMethod::Undo { op_seq: None }).await.unwrap() {
        RpcPayload::Undo(u) => u,
        other => panic!("{other:?}"),
    };
    assert_eq!(undone_vp3_ast.undone_op_seq, vp3_ast_seq);

    send_rpc(&repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(
        running.wait().await.unwrap(),
        DaemonRunExit::ShutdownRequested
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn undo_after_viewport_drop_and_gc_skips_dropped_viewport_without_wedging() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn base() -> i32 { 1 }\n").unwrap();

    let (running, vp_peer) = init_and_spawn(&repo, false, "vp-peer", "peer work").await;
    write_vp_file_atomic(&vp_peer.path, "src/lib.rs", "pub fn base() -> i32 { 2 }\n");
    running.ctx.radar.flush_viewport(&vp_peer.id).await;
    let peer_ast_seq = running
        .ctx
        .db
        .list_ops(false, None, None)
        .unwrap()
        .into_iter()
        .find(|e| {
            e.kind == OpKind::AstPatch && e.viewport_id.as_deref() == Some(vp_peer.id.as_str())
        })
        .expect("vp-peer AstPatch must exist")
        .op_seq;

    let vp_dead = match send_rpc(
        &repo,
        RpcMethod::SpawnViewport {
            name: "vp-dead".into(),
            intent: "deadend attempt".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("{other:?}"),
    };
    write_vp_file_atomic(&vp_dead.path, "src/lib.rs", "pub fn broken() {}\n");
    running.ctx.radar.flush_viewport(&vp_dead.id).await;

    send_rpc(
        &repo,
        RpcMethod::DropViewport {
            viewport_id: vp_dead.id.clone(),
            reason: "deadend approach".into(),
            kind: Some("DesignDeadEnd".into()),
        },
    )
    .await
    .unwrap();
    let drop_seq = running
        .ctx
        .db
        .list_ops(false, None, None)
        .unwrap()
        .into_iter()
        .find(|e| {
            e.kind == OpKind::ViewportDrop && e.viewport_id.as_deref() == Some(vp_dead.id.as_str())
        })
        .expect("ViewportDrop op must exist")
        .op_seq;

    send_rpc(
        &repo,
        RpcMethod::Gc {
            prune_viewports: true,
        },
    )
    .await
    .unwrap();

    // Explicit undo --op <drop_seq> still fails with InvalidState
    assert_eq!(
        send_rpc(
            &repo,
            RpcMethod::Undo {
                op_seq: Some(drop_seq)
            }
        )
        .await
        .unwrap_err()
        .0,
        RpcErrorCode::InvalidState
    );

    // undo(None) must skip the GcPending ViewportDrop and undo vp-peer's AstPatch
    let undone = match send_rpc(&repo, RpcMethod::Undo { op_seq: None })
        .await
        .unwrap()
    {
        RpcPayload::Undo(u) => u,
        other => panic!("{other:?}"),
    };
    assert_eq!(undone.undone_op_seq, peer_ast_seq);

    // Subsequent undo(None) unwinds vp-peer's ViewportTransition and ViewportSpawn cleanly
    let undone_trans = match send_rpc(&repo, RpcMethod::Undo { op_seq: None })
        .await
        .unwrap()
    {
        RpcPayload::Undo(u) => u,
        other => panic!("{other:?}"),
    };
    assert_ne!(undone_trans.undone_op_seq, drop_seq);

    let undone_spawn = match send_rpc(&repo, RpcMethod::Undo { op_seq: None })
        .await
        .unwrap()
    {
        RpcPayload::Undo(u) => u,
        other => panic!("{other:?}"),
    };
    assert_ne!(undone_spawn.undone_op_seq, drop_seq);
    assert!(running.ctx.db.get_viewport(&vp_peer.id).unwrap().is_none());

    // Once vp-peer is fully undone and only vp-dead's GcPending ViewportDrop remains,
    // first_blocked fallback surfaces InvalidState without resurrecting vp-dead
    assert_eq!(
        send_rpc(&repo, RpcMethod::Undo { op_seq: None })
            .await
            .unwrap_err()
            .0,
        RpcErrorCode::InvalidState
    );

    send_rpc(&repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(
        running.wait().await.unwrap(),
        DaemonRunExit::ShutdownRequested
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_preserves_restore_clean_inverse_base_snapshot_until_undone() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn v() -> i32 { 1 }\n").unwrap();

    let (running, vp) = init_and_spawn(&repo, false, "vp1", "restore clean gc").await;
    let s1 = running
        .ctx
        .db
        .get_viewport(&vp.id)
        .unwrap()
        .unwrap()
        .base_snapshot_cas
        .expect("initial base_snapshot_cas S1 must exist");

    // Advance trunk directly (no CapsuleSeal holding S1) so RestoreViewportClean captures S2 != S1.
    std::fs::write(repo.join("src/extra.rs"), "pub fn extra() -> i32 { 77 }\n").unwrap();
    write_vp_file_atomic(&vp.path, "src/lib.rs", "pub fn v() -> i32 { 2 }\n");
    running.ctx.radar.flush_viewport(&vp.id).await;

    send_rpc(
        &repo,
        RpcMethod::RestoreViewportClean {
            viewport_id: vp.id.clone(),
        },
    )
    .await
    .unwrap();
    let s2 = running
        .ctx
        .db
        .get_viewport(&vp.id)
        .unwrap()
        .unwrap()
        .base_snapshot_cas
        .expect("S2 must exist");
    assert_ne!(s1, s2);

    // GC must NOT delete S1 while RestoreViewportClean op is still un-undone on a live viewport.
    send_rpc(
        &repo,
        RpcMethod::Gc {
            prune_viewports: false,
        },
    )
    .await
    .unwrap();
    let cas = loom_core::cas::CasStore::new(repo.join(".loom/cas"), 3).unwrap();
    assert!(
        cas.contains(&s1),
        "GC must preserve S1 referenced by RestoreViewport inverse"
    );

    // Undoing RestoreViewportClean restores base_snapshot_cas = S1, and SealCapsule must succeed.
    send_rpc(&repo, RpcMethod::Undo { op_seq: None })
        .await
        .unwrap();
    assert_eq!(
        running
            .ctx
            .db
            .get_viewport(&vp.id)
            .unwrap()
            .unwrap()
            .base_snapshot_cas,
        Some(s1)
    );
    write_vp_file_atomic(&vp.path, "src/lib.rs", "pub fn v() -> i32 { 3 }\n");
    let cap = send_rpc(
        &repo,
        RpcMethod::SealCapsule {
            viewport_id: vp.id.clone(),
            verify_cmd: None,
            summary: Some("seal after undo restore_clean".into()),
        },
    )
    .await
    .unwrap();
    assert!(matches!(cap, RpcPayload::CapsuleSeal(_)));

    send_rpc(&repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(
        running.wait().await.unwrap(),
        DaemonRunExit::ShutdownRequested
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seal_and_undo_replace_file_with_directory_and_directory_with_file() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join("src/pkg")).unwrap();
    std::fs::write(repo.join("src/item"), "pub fn item_file() -> i32 { 1 }\n").unwrap();
    std::fs::write(repo.join("src/pkg/mod.rs"), "pub fn pkg_mod() -> i32 { 2 }\n").unwrap();

    let (running, vp1) = init_and_spawn(&repo, false, "vp-f2d", "file to dir").await;

    // Case A: Replace file `src/item` with directory `src/item/mod.rs`
    std::fs::remove_file(vp1.path.join("src/item")).unwrap();
    std::fs::create_dir_all(vp1.path.join("src/item")).unwrap();
    std::fs::write(
        vp1.path.join("src/item/mod.rs"),
        "pub fn item_dir() -> i32 { 10 }\n",
    )
    .unwrap();

    send_rpc(
        &repo,
        RpcMethod::SealCapsule {
            viewport_id: vp1.id.clone(),
            verify_cmd: None,
            summary: Some("replace file with dir".into()),
        },
    )
    .await
    .unwrap();
    assert!(repo.join("src/item").is_dir());
    assert_eq!(
        std::fs::read_to_string(repo.join("src/item/mod.rs")).unwrap(),
        "pub fn item_dir() -> i32 { 10 }\n"
    );

    // Case B: Replace directory `src/pkg/mod.rs` with regular file `src/pkg`
    let vp2 = match send_rpc(
        &repo,
        RpcMethod::SpawnViewport {
            name: "vp-d2f".into(),
            intent: "dir to file".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn payload: {other:?}"),
    };
    std::fs::remove_dir_all(vp2.path.join("src/pkg")).unwrap();
    std::fs::write(vp2.path.join("src/pkg"), "pub fn pkg_file() -> i32 { 20 }\n").unwrap();

    send_rpc(
        &repo,
        RpcMethod::SealCapsule {
            viewport_id: vp2.id.clone(),
            verify_cmd: None,
            summary: Some("replace dir with file".into()),
        },
    )
    .await
    .unwrap();
    assert!(repo.join("src/pkg").is_file());
    assert_eq!(
        std::fs::read_to_string(repo.join("src/pkg")).unwrap(),
        "pub fn pkg_file() -> i32 { 20 }\n"
    );

    // Undo Case B (restores directory `src/pkg/mod.rs` and deletes file `src/pkg`)
    send_rpc(&repo, RpcMethod::Undo { op_seq: None })
        .await
        .unwrap();
    assert!(repo.join("src/pkg").is_dir());
    assert_eq!(
        std::fs::read_to_string(repo.join("src/pkg/mod.rs")).unwrap(),
        "pub fn pkg_mod() -> i32 { 2 }\n"
    );

    // Undo Case A (restores file `src/item` and deletes `src/item/mod.rs` + empty `src/item/`)
    send_rpc(&repo, RpcMethod::Undo { op_seq: None })
        .await
        .unwrap();
    assert!(repo.join("src/item").is_file());
    assert_eq!(
        std::fs::read_to_string(repo.join("src/item")).unwrap(),
        "pub fn item_file() -> i32 { 1 }\n"
    );

    // Case C: Seal conflict when another viewport added `src/pkg/keep.rs` on trunk
    // while `vp3` attempts to replace `src/pkg` with a regular file.
    let vp_peer = match send_rpc(
        &repo,
        RpcMethod::SpawnViewport {
            name: "vp-peer".into(),
            intent: "add keep.rs".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn payload: {other:?}"),
    };
    let vp3 = match send_rpc(
        &repo,
        RpcMethod::SpawnViewport {
            name: "vp-conflict-d2f".into(),
            intent: "replace pkg dir".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn payload: {other:?}"),
    };

    std::fs::write(
        vp_peer.path.join("src/pkg/keep.rs"),
        "pub fn keep() -> i32 { 7 }\n",
    )
    .unwrap();
    send_rpc(
        &repo,
        RpcMethod::SealCapsule {
            viewport_id: vp_peer.id.clone(),
            verify_cmd: None,
            summary: Some("add src/pkg/keep.rs".into()),
        },
    )
    .await
    .unwrap();

    std::fs::remove_dir_all(vp3.path.join("src/pkg")).unwrap();
    std::fs::write(vp3.path.join("src/pkg"), "pub fn clash() -> i32 { 9 }\n").unwrap();
    let seal_err = send_rpc(
        &repo,
        RpcMethod::SealCapsule {
            viewport_id: vp3.id.clone(),
            verify_cmd: None,
            summary: Some("conflict dir vs file".into()),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(seal_err.0, RpcErrorCode::Conflict);
    assert!(repo.join("src/pkg/keep.rs").is_file());

    send_rpc(&repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(
        running.wait().await.unwrap(),
        DaemonRunExit::ShutdownRequested
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_pull_file_vs_dir_replacement_rebase_conflict_and_ref_rollback() {
    let tmp = TempDir::new().unwrap();
    let bare_dir = tmp.path().join("remote.git");
    let repo_a = tmp.path().join("repo_a");
    let repo_b = tmp.path().join("repo_b");
    let repo_c = tmp.path().join("repo_c");
    std::fs::create_dir_all(&repo_a).unwrap();
    std::fs::create_dir_all(&repo_b).unwrap();
    std::fs::create_dir_all(&repo_c).unwrap();

    let _bare = gix::init_bare(&bare_dir).unwrap();
    let init_files = [
        ("src/item", "pub fn item_v1() -> i32 { 1 }\n"),
        ("src/pkg/mod.rs", "pub fn pkg_v1() -> i32 { 2 }\n"),
    ];
    init_git_with_initial_commit(&repo_a, &init_files);
    let _rb = gix::init(&repo_b).unwrap();
    std::fs::write(repo_b.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    let _rc = gix::init(&repo_c).unwrap();
    std::fs::write(repo_c.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

    // Ensure repo_a, repo_b, and repo_c share the exact seed commit via bare remote
    let (running_a, vp_seed) = init_and_spawn(&repo_a, true, "vp-seed", "seed bare").await;
    std::fs::write(
        vp_seed.path.join("src/item"),
        "pub fn item_v1() -> i32 { 1 }\n",
    )
    .unwrap();
    std::fs::write(
        vp_seed.path.join("src/seed.rs"),
        "pub fn seed() -> i32 { 0 }\n",
    )
    .unwrap();
    send_rpc(
        &repo_a,
        RpcMethod::SealCapsule {
            viewport_id: vp_seed.id.clone(),
            verify_cmd: None,
            summary: Some("seed commit".into()),
        },
    )
    .await
    .unwrap();
    send_rpc(
        &repo_a,
        RpcMethod::GitPush {
            remote: Some(bare_dir.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap();

    let bare_tip = gix::open(&bare_dir)
        .unwrap()
        .try_find_reference("refs/heads/main")
        .unwrap()
        .unwrap()
        .peel_to_id()
        .unwrap()
        .detach();

    let running_b = match start_daemon(&repo_b, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("repo_b daemon should start"),
    };
    send_rpc(&repo_b, RpcMethod::Init { colocate_git: true })
        .await
        .unwrap();
    send_rpc(
        &repo_b,
        RpcMethod::GitPull {
            remote: Some(bare_dir.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap();
    let vp_b_clean = match send_rpc(
        &repo_b,
        RpcMethod::SpawnViewport {
            name: "vp-b-clean".into(),
            intent: "clean rebase".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn payload: {other:?}"),
    };
    let vp_b_conflict = match send_rpc(
        &repo_b,
        RpcMethod::SpawnViewport {
            name: "vp-b-conflict".into(),
            intent: "path conflict rebase".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn payload: {other:?}"),
    };
    // `vp_b_conflict` creates a blocking file `src/cdir` and a non-empty dir `src/cfile/extra.rs`
    std::fs::write(vp_b_conflict.path.join("src/cdir"), "blocking file").unwrap();
    std::fs::create_dir_all(vp_b_conflict.path.join("src/cfile")).unwrap();
    std::fs::write(
        vp_b_conflict.path.join("src/cfile/extra.rs"),
        "pub fn keep_me() {}\n",
    )
    .unwrap();

    let running_c = match start_daemon(&repo_c, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("repo_c daemon should start"),
    };
    send_rpc(&repo_c, RpcMethod::Init { colocate_git: true })
        .await
        .unwrap();
    send_rpc(
        &repo_c,
        RpcMethod::GitPull {
            remote: Some(bare_dir.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap();
    // Place an untracked blocking file `src/cdir` on repo_c's trunk to force trunk sync Conflict
    std::fs::write(repo_c.join("src/cdir"), "untracked trunk blocker").unwrap();

    // On repo_a: replace `src/item` -> `src/item/mod.rs`, `src/pkg/mod.rs` -> `src/pkg`,
    // and add `src/cdir/mod.rs` + `src/cfile`.
    let vp_a = match send_rpc(
        &repo_a,
        RpcMethod::SpawnViewport {
            name: "vp-a-swap".into(),
            intent: "file-dir swaps".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn payload: {other:?}"),
    };
    std::fs::remove_file(vp_a.path.join("src/item")).unwrap();
    std::fs::create_dir_all(vp_a.path.join("src/item")).unwrap();
    std::fs::write(
        vp_a.path.join("src/item/mod.rs"),
        "pub fn item_v2() -> i32 { 11 }\n",
    )
    .unwrap();
    std::fs::remove_dir_all(vp_a.path.join("src/pkg")).unwrap();
    std::fs::write(
        vp_a.path.join("src/pkg"),
        "pub fn pkg_v2() -> i32 { 22 }\n",
    )
    .unwrap();
    std::fs::create_dir_all(vp_a.path.join("src/cdir")).unwrap();
    std::fs::write(
        vp_a.path.join("src/cdir/mod.rs"),
        "pub fn cdir() -> i32 { 33 }\n",
    )
    .unwrap();
    std::fs::write(
        vp_a.path.join("src/cfile"),
        "pub fn cfile() -> i32 { 44 }\n",
    )
    .unwrap();

    send_rpc(
        &repo_a,
        RpcMethod::SealCapsule {
            viewport_id: vp_a.id.clone(),
            verify_cmd: None,
            summary: Some("swap files and dirs".into()),
        },
    )
    .await
    .unwrap();
    send_rpc(
        &repo_a,
        RpcMethod::GitPush {
            remote: Some(bare_dir.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap();

    // 1. repo_b pulls: trunk + vp_b_clean swap cleanly; vp_b_conflict reports Conflict without data loss
    let pull_b = match send_rpc(
        &repo_b,
        RpcMethod::GitPull {
            remote: Some(bare_dir.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::GitPull(res) => res,
        other => panic!("unexpected git pull payload: {other:?}"),
    };
    assert!(pull_b.updated_trunk);
    assert!(repo_b.join("src/item/mod.rs").is_file());
    assert!(repo_b.join("src/pkg").is_file());
    assert!(vp_b_clean.path.join("src/item/mod.rs").is_file());
    assert!(vp_b_clean.path.join("src/pkg").is_file());

    let clean_st = pull_b
        .rebased_viewports
        .iter()
        .find(|v| v.viewport_id == vp_b_clean.id)
        .unwrap();
    assert_eq!(clean_st.outcome, ViewportRebaseOutcome::Clean);

    let conflict_st = pull_b
        .rebased_viewports
        .iter()
        .find(|v| v.viewport_id == vp_b_conflict.id)
        .unwrap();
    assert_eq!(conflict_st.outcome, ViewportRebaseOutcome::Conflict);
    assert!(!conflict_st.conflicts.is_empty());
    assert_eq!(
        std::fs::read_to_string(vp_b_conflict.path.join("src/cdir")).unwrap(),
        "blocking file"
    );
    assert_eq!(
        std::fs::read_to_string(vp_b_conflict.path.join("src/cfile/extra.rs")).unwrap(),
        "pub fn keep_me() {}\n"
    );

    // 2. repo_c pulls with untracked trunk blocker `src/cdir`:
    //    must fail with Conflict AND must NOT advance refs/heads/main or record a new GitPull.
    let pull_ops_before = running_c
        .ctx
        .db
        .list_ops(false, None, None)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == OpKind::GitPull)
        .count();
    let pull_c_err = send_rpc(
        &repo_c,
        RpcMethod::GitPull {
            remote: Some(bare_dir.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(pull_c_err.0, RpcErrorCode::Conflict);
    assert_eq!(
        gix::open(&repo_c).unwrap().head_id().unwrap().detach(),
        bare_tip,
        "refs/heads/main must stay rolled back to bare_tip when trunk sync fails"
    );
    assert!(repo_c.join("src/item").is_file());
    assert!(repo_c.join("src/pkg/mod.rs").is_file());
    assert_eq!(
        running_c
            .ctx
            .db
            .list_ops(false, None, None)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == OpKind::GitPull)
            .count(),
        pull_ops_before
    );

    // Removing the untracked blocker allows retrying `git pull` to succeed completely
    std::fs::remove_file(repo_c.join("src/cdir")).unwrap();
    let pull_c_ok = match send_rpc(
        &repo_c,
        RpcMethod::GitPull {
            remote: Some(bare_dir.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::GitPull(res) => res,
        other => panic!("unexpected git pull payload: {other:?}"),
    };
    assert!(pull_c_ok.updated_trunk);
    assert!(repo_c.join("src/cdir/mod.rs").is_file());
    assert!(repo_c.join("src/cfile").is_file());

    send_rpc(&repo_a, RpcMethod::Shutdown).await.unwrap();
    send_rpc(&repo_b, RpcMethod::Shutdown).await.unwrap();
    send_rpc(&repo_c, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(
        running_a.wait().await.unwrap(),
        DaemonRunExit::ShutdownRequested
    );
    assert_eq!(
        running_b.wait().await.unwrap(),
        DaemonRunExit::ShutdownRequested
    );
    assert_eq!(
        running_c.wait().await.unwrap(),
        DaemonRunExit::ShutdownRequested
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_seal_capsule_single_line_ts_interface_and_go_struct_prepend() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::create_dir_all(repo.join("pkg")).unwrap();
    std::fs::write(
        repo.join("src/codec.ts"),
        "export interface Codec { decode(raw: string): string; }\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("pkg/s.go"),
        "package pkg\n\ntype S struct { A int }\n",
    )
    .unwrap();

    let (running, vp) = init_and_spawn(&repo, false, "vp-single-line", "prepend members").await;
    write_vp_file_atomic(
        &vp.path,
        "src/codec.ts",
        "export interface Codec {\n    decode(raw: Uint8Array): Uint8Array;\n    decode(raw: string): string;\n}\n",
    );
    write_vp_file_atomic(
        &vp.path,
        "pkg/s.go",
        "package pkg\n\ntype S struct {\n\tX int\n\tA int\n}\n",
    );

    let seal_res = send_rpc(
        &repo,
        RpcMethod::SealCapsule {
            viewport_id: vp.id.clone(),
            verify_cmd: None,
            summary: Some("single-line TS/Go prepend".into()),
        },
    )
    .await
    .unwrap();
    assert!(matches!(seal_res, RpcPayload::CapsuleSeal(_)));

    let sealed_ts = std::fs::read_to_string(repo.join("src/codec.ts")).unwrap();
    assert!(sealed_ts.contains("decode(raw: Uint8Array): Uint8Array;"));
    assert!(sealed_ts.contains("decode(raw: string): string;"));
    assert!(
        !loom_core::cst::parse_source(
            loom_core::model::LanguageKind::TypeScript,
            &sealed_ts,
            None
        )
        .unwrap()
        .tree
        .root_node()
        .has_error()
    );

    let sealed_go = std::fs::read_to_string(repo.join("pkg/s.go")).unwrap();
    assert!(sealed_go.contains("X int"));
    assert!(sealed_go.contains("A int"));
    assert!(
        !loom_core::cst::parse_source(loom_core::model::LanguageKind::Go, &sealed_go, None)
            .unwrap()
            .tree
            .root_node()
            .has_error()
    );

    send_rpc(&repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(
        running.wait().await.unwrap(),
        DaemonRunExit::ShutdownRequested
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_seal_capsule_python_single_line_class_and_ellipsis() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("one.py"), "class One: x = 1\n").unwrap();
    std::fs::write(repo.join("empty.py"), "class Empty: ...\n").unwrap();
    std::fs::write(repo.join("doc.py"), "class Doc: \"\"\"doc\"\"\"; x = 1\n").unwrap();
    std::fs::write(repo.join("two.py"), "class Two: x = 1; y = 2\n").unwrap();

    let (running, vp) = init_and_spawn(&repo, false, "vp-py-single", "python single line").await;
    write_vp_file_atomic(
        &vp.path,
        "one.py",
        "class One:\n    def m(self):\n        return 1\n    x = 1\n",
    );
    write_vp_file_atomic(
        &vp.path,
        "empty.py",
        "class Empty:\n    def m(self):\n        pass\n",
    );
    write_vp_file_atomic(
        &vp.path,
        "doc.py",
        "class Doc:\n    \"\"\"doc\"\"\"\n    def m(self): pass\n    x = 1\n",
    );
    write_vp_file_atomic(
        &vp.path,
        "two.py",
        "class Two:\n    def m(self): pass\n    x = 1; y = 2\n",
    );

    let seal_res = send_rpc(
        &repo,
        RpcMethod::SealCapsule {
            viewport_id: vp.id.clone(),
            verify_cmd: None,
            summary: Some("python single-line class prepend & ellipsis".into()),
        },
    )
    .await
    .unwrap();
    assert!(matches!(seal_res, RpcPayload::CapsuleSeal(_)));

    let s_one = std::fs::read_to_string(repo.join("one.py")).unwrap();
    assert_eq!(s_one, "class One:\n    def m(self):\n        return 1\n    x = 1\n");

    let s_empty = std::fs::read_to_string(repo.join("empty.py")).unwrap();
    assert_eq!(s_empty, "class Empty:\n    def m(self):\n        pass\n");

    let s_doc = std::fs::read_to_string(repo.join("doc.py")).unwrap();
    assert_eq!(
        s_doc,
        "class Doc:\n    \"\"\"doc\"\"\"\n    def m(self): pass\n    x = 1\n"
    );

    let s_two = std::fs::read_to_string(repo.join("two.py")).unwrap();
    assert_eq!(
        s_two,
        "class Two:\n    def m(self): pass\n    x = 1; y = 2\n"
    );

    for (name, content) in [
        ("one.py", &s_one),
        ("empty.py", &s_empty),
        ("doc.py", &s_doc),
        ("two.py", &s_two),
    ] {
        assert!(
            !loom_core::cst::parse_source(loom_core::model::LanguageKind::Python, content, None)
                .unwrap()
                .tree
                .root_node()
                .has_error(),
            "post-seal Python CST has errors in {name}:\n{content}"
        );
    }

    send_rpc(&repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(
        running.wait().await.unwrap(),
        DaemonRunExit::ShutdownRequested
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_pull_external_commit_over_unpushed_capsule_has_no_dag_cycle() {
    let tmp = TempDir::new().unwrap();
    let bare_dir = tmp.path().join("remote.git");
    let repo_a = tmp.path().join("repo_a");
    let repo_b = tmp.path().join("repo_b");
    std::fs::create_dir_all(&repo_a).unwrap();
    std::fs::create_dir_all(&repo_b).unwrap();

    let _bare = gix::init_bare(&bare_dir).unwrap();
    init_git_with_initial_commit(&repo_a, &[("src/base.rs", "pub fn base() -> i32 { 1 }\n")]);

    // 1. repo_a seals an initial seed capsule and pushes it to bare remote
    let (running_a, vp_seed) = init_and_spawn(&repo_a, true, "vp-seed", "seed").await;
    std::fs::write(
        vp_seed.path.join("src/seed.rs"),
        "pub fn seed() -> i32 { 2 }\n",
    )
    .unwrap();
    let cap_seed = match send_rpc(
        &repo_a,
        RpcMethod::SealCapsule {
            viewport_id: vp_seed.id.clone(),
            verify_cmd: None,
            summary: Some("seed cap".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::CapsuleSeal(c) => c,
        other => panic!("unexpected seal: {other:?}"),
    };
    send_rpc(
        &repo_a,
        RpcMethod::GitPush {
            remote: Some(bare_dir.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap();

    // 2. repo_a seals TWO unpushed local capsules (`cap_local_1`, `cap_local_2`)
    let vp1 = match send_rpc(
        &repo_a,
        RpcMethod::SpawnViewport {
            name: "vp-local-1".into(),
            intent: "local 1".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn: {other:?}"),
    };
    std::fs::write(
        vp1.path.join("src/local1.rs"),
        "pub fn local1() -> i32 { 10 }\n",
    )
    .unwrap();
    let cap_local_1 = match send_rpc(
        &repo_a,
        RpcMethod::SealCapsule {
            viewport_id: vp1.id.clone(),
            verify_cmd: None,
            summary: Some("local cap 1".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::CapsuleSeal(c) => c,
        other => panic!("unexpected seal: {other:?}"),
    };

    let vp2 = match send_rpc(
        &repo_a,
        RpcMethod::SpawnViewport {
            name: "vp-local-2".into(),
            intent: "local 2".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn: {other:?}"),
    };
    std::fs::write(
        vp2.path.join("src/local2.rs"),
        "pub fn local2() -> i32 { 20 }\n",
    )
    .unwrap();
    let cap_local_2 = match send_rpc(
        &repo_a,
        RpcMethod::SealCapsule {
            viewport_id: vp2.id.clone(),
            verify_cmd: None,
            summary: Some("local cap 2".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::CapsuleSeal(c) => c,
        other => panic!("unexpected seal: {other:?}"),
    };

    // 3. External plain Git commit pushed directly to bare_dir (no Loom note)
    {
        let bare = gix::open(&bare_dir).unwrap();
        let parent_id = bare
            .try_find_reference("refs/heads/main")
            .unwrap()
            .unwrap()
            .peel_to_id()
            .unwrap()
            .detach();
        let parent_tree = bare.find_commit(parent_id).unwrap().tree_id().unwrap().detach();
        let mut ed = bare.edit_tree(parent_tree).unwrap();
        let blob = bare.write_blob(b"pub fn external() -> i32 { 99 }\n").unwrap().detach();
        ed.upsert("src/external.rs", gix::objs::tree::EntryKind::Blob, blob)
            .unwrap();
        let tree_id = ed.write().unwrap().detach();
        bare.commit("refs/heads/main", "external commit", tree_id, [parent_id])
            .unwrap();
    }

    // 4. repo_a runs git_pull -> must synthesize cap-ext-* without any parent_capsule_ids cycle
    let pull_res = match send_rpc(
        &repo_a,
        RpcMethod::GitPull {
            remote: Some(bare_dir.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::GitPull(r) => r,
        other => panic!("unexpected pull: {other:?}"),
    };
    assert_eq!(pull_res.synthesized_capsule_ids.len(), 1);
    let ext_cap_id = &pull_res.synthesized_capsule_ids[0];

    let capsules = running_a.ctx.db.list_capsules().unwrap();
    let by_id: std::collections::HashMap<String, CausalCapsule> = capsules
        .iter()
        .cloned()
        .map(|c| (c.capsule_id.clone(), c))
        .collect();

    // Assert zero cycles across ALL capsules via full parent graph DFS
    fn assert_acyclic(
        id: &str,
        by_id: &std::collections::HashMap<String, CausalCapsule>,
        visiting: &mut std::collections::HashSet<String>,
        visited: &mut std::collections::HashSet<String>,
    ) {
        if visited.contains(id) {
            return;
        }
        assert!(
            visiting.insert(id.to_string()),
            "cycle detected in parent_capsule_ids involving {id}: {by_id:?}"
        );
        if let Some(cap) = by_id.get(id) {
            for p in &cap.parent_capsule_ids {
                assert_acyclic(p, by_id, visiting, visited);
            }
        }
        visiting.remove(id);
        visited.insert(id.to_string());
    }
    let mut visited = std::collections::HashSet::new();
    for c in &capsules {
        let mut visiting = std::collections::HashSet::new();
        assert_acyclic(&c.capsule_id, &by_id, &mut visiting, &mut visited);
    }

    let ext_cap = by_id.get(ext_cap_id).unwrap();
    assert_eq!(ext_cap.parent_capsule_ids, vec![cap_seed.capsule_id.clone()]);
    let local1_after = by_id.get(&cap_local_1.capsule_id).unwrap();
    assert_eq!(local1_after.parent_capsule_ids, vec![ext_cap_id.clone()]);
    let local2_after = by_id.get(&cap_local_2.capsule_id).unwrap();
    assert_eq!(local2_after.parent_capsule_ids, vec![cap_local_1.capsule_id.clone()]);

    // 5. Also verify zero-pushed-capsule repo_b: unpushed local capsule + external commit -> acyclic
    let bare_b = tmp.path().join("remote_b.git");
    let _bb = gix::init_bare(&bare_b).unwrap();
    init_git_with_initial_commit(&repo_b, &[("src/init.rs", "pub fn init() -> i32 { 0 }\n")]);
    {
        // Push initial commit to bare_b using gix so bare_b has the exact base commit
        let r_b = gix::open(&repo_b).unwrap();
        let b_init_oid = r_b.head_id().unwrap().detach();
        let b_init_commit = r_b.find_commit(b_init_oid).unwrap();
        let sig = b_init_commit.author().unwrap();
        let bb_repo = gix::open(&bare_b).unwrap();
        let empty = gix::ObjectId::empty_tree(bb_repo.object_hash());
        let mut ed0 = bb_repo.edit_tree(empty).unwrap();
        let bl0 = bb_repo.write_blob(b"pub fn init() -> i32 { 0 }\n").unwrap().detach();
        ed0.upsert("src/init.rs", gix::objs::tree::EntryKind::Blob, bl0).unwrap();
        let t0 = ed0.write().unwrap().detach();
        let c0 = bb_repo.commit_as(sig, sig, "refs/heads/main", "initial commit", t0, std::iter::empty::<gix::ObjectId>()).unwrap().detach();
        assert_eq!(c0, b_init_oid);
        let mut ed1 = bb_repo.edit_tree(t0).unwrap();
        let bl1 = bb_repo.write_blob(b"pub fn peer_ext() -> i32 { 5 }\n").unwrap().detach();
        ed1.upsert("src/peer_ext.rs", gix::objs::tree::EntryKind::Blob, bl1).unwrap();
        let t1 = ed1.write().unwrap().detach();
        bb_repo.commit("refs/heads/main", "peer external", t1, [c0]).unwrap();
    }

    let (running_b, vp_b) = init_and_spawn(&repo_b, true, "vp-b", "unpushed only").await;
    std::fs::write(vp_b.path.join("src/only_local.rs"), "pub fn ol() -> i32 { 7 }\n").unwrap();
    let cap_b_local = match send_rpc(
        &repo_b,
        RpcMethod::SealCapsule {
            viewport_id: vp_b.id.clone(),
            verify_cmd: None,
            summary: Some("only local".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::CapsuleSeal(c) => c,
        other => panic!("unexpected seal: {other:?}"),
    };
    let pull_b = match send_rpc(
        &repo_b,
        RpcMethod::GitPull {
            remote: Some(bare_b.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::GitPull(r) => r,
        other => panic!("unexpected pull: {other:?}"),
    };
    assert_eq!(pull_b.synthesized_capsule_ids.len(), 1);
    let b_ext_id = &pull_b.synthesized_capsule_ids[0];
    let b_caps = running_b.ctx.db.list_capsules().unwrap();
    let b_by_id: std::collections::HashMap<String, CausalCapsule> = b_caps
        .iter()
        .cloned()
        .map(|c| (c.capsule_id.clone(), c))
        .collect();
    let mut b_visited = std::collections::HashSet::new();
    for c in &b_caps {
        let mut b_visiting = std::collections::HashSet::new();
        assert_acyclic(&c.capsule_id, &b_by_id, &mut b_visiting, &mut b_visited);
    }
    assert!(b_by_id[b_ext_id].parent_capsule_ids.is_empty());
    assert_eq!(b_by_id[&cap_b_local.capsule_id].parent_capsule_ids, vec![b_ext_id.clone()]);

    send_rpc(&repo_a, RpcMethod::Shutdown).await.unwrap();
    send_rpc(&repo_b, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running_a.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
    assert_eq!(running_b.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ntfs_case_variant_file_dir_replacement_seal_and_undo() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join("src/Pkg")).unwrap();
    std::fs::write(repo.join("src/Foo"), "pub fn foo_file() -> i32 { 1 }\n").unwrap();
    std::fs::write(repo.join("src/Pkg/mod.rs"), "pub fn pkg_mod() -> i32 { 2 }\n").unwrap();

    let (running, vp1) = init_and_spawn(&repo, false, "vp-case-f2d", "case f2d").await;

    // 1. Replace `src/Foo` file with `src/foo/bar.rs`
    std::fs::remove_file(vp1.path.join("src/Foo")).unwrap();
    std::fs::create_dir_all(vp1.path.join("src/foo")).unwrap();
    std::fs::write(
        vp1.path.join("src/foo/bar.rs"),
        "pub fn foo_bar() -> i32 { 10 }\n",
    )
    .unwrap();
    send_rpc(
        &repo,
        RpcMethod::SealCapsule {
            viewport_id: vp1.id.clone(),
            verify_cmd: None,
            summary: Some("case-variant file to dir".into()),
        },
    )
    .await
    .unwrap();
    assert!(repo.join("src/foo/bar.rs").is_file());
    assert_eq!(
        std::fs::read_to_string(repo.join("src/foo/bar.rs")).unwrap(),
        "pub fn foo_bar() -> i32 { 10 }\n"
    );

    // 2. Replace `src/Pkg/mod.rs` directory with `src/pkg` file
    let vp2 = match send_rpc(
        &repo,
        RpcMethod::SpawnViewport {
            name: "vp-case-d2f".into(),
            intent: "case d2f".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn: {other:?}"),
    };
    std::fs::remove_dir_all(vp2.path.join("src/Pkg")).unwrap();
    std::fs::write(vp2.path.join("src/pkg"), "pub fn pkg_file() -> i32 { 20 }\n").unwrap();
    send_rpc(
        &repo,
        RpcMethod::SealCapsule {
            viewport_id: vp2.id.clone(),
            verify_cmd: None,
            summary: Some("case-variant dir to file".into()),
        },
    )
    .await
    .unwrap();
    assert!(repo.join("src/pkg").is_file());
    assert_eq!(
        std::fs::read_to_string(repo.join("src/pkg")).unwrap(),
        "pub fn pkg_file() -> i32 { 20 }\n"
    );

    // 3. Undo both seals cleanly
    send_rpc(&repo, RpcMethod::Undo { op_seq: None }).await.unwrap();
    assert!(repo.join("src/Pkg/mod.rs").is_file());
    assert_eq!(
        std::fs::read_to_string(repo.join("src/Pkg/mod.rs")).unwrap(),
        "pub fn pkg_mod() -> i32 { 2 }\n"
    );

    send_rpc(&repo, RpcMethod::Undo { op_seq: None }).await.unwrap();
    assert!(repo.join("src/Foo").is_file());
    assert_eq!(
        std::fs::read_to_string(repo.join("src/Foo")).unwrap(),
        "pub fn foo_file() -> i32 { 1 }\n"
    );

    send_rpc(&repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(
        running.wait().await.unwrap(),
        DaemonRunExit::ShutdownRequested
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_seal_capsule_python_single_line_class_append_two_viewports() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(
        repo.join("app.py"),
        "class One: x = 1\n\ndef other():\n    return 0\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("doc_two.py"),
        "class Doc: \"\"\"doc\"\"\"; x = 1\n\nclass Two: a = 1; b = 2\n",
    )
    .unwrap();

    let (running, vp1) = init_and_spawn(&repo, false, "vp-py-app1", "edit other").await;
    let vp2 = match send_rpc(
        &repo,
        RpcMethod::SpawnViewport {
            name: "vp-py-app2".into(),
            intent: "expand class One and append".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn: {other:?}"),
    };

    // vp1 edits other() in app.py and appends tail() after Two::b in doc_two.py
    write_vp_file_atomic(
        &vp1.path,
        "app.py",
        "class One: x = 1\n\ndef other():\n    return 1\n",
    );
    write_vp_file_atomic(
        &vp1.path,
        "doc_two.py",
        "class Doc: \"\"\"doc\"\"\"; x = 1\n\nclass Two:\n    a = 1\n    b = 2\n    def tail(self):\n        return 4\n",
    );
    let seal1 = send_rpc(
        &repo,
        RpcMethod::SealCapsule {
            viewport_id: vp1.id.clone(),
            verify_cmd: None,
            summary: Some("vp1 edit other and Two tail".into()),
        },
    )
    .await
    .unwrap();
    assert!(matches!(seal1, RpcPayload::CapsuleSeal(_)));

    // vp2 expands class One and appends def n(self) after x = 1, and appends def m(self) after Doc::x
    write_vp_file_atomic(
        &vp2.path,
        "app.py",
        "class One:\n    x = 1\n    def n(self):\n        return 2\n\ndef other():\n    return 0\n",
    );
    write_vp_file_atomic(
        &vp2.path,
        "doc_two.py",
        "class Doc:\n    \"\"\"doc\"\"\"\n    x = 1\n    def m(self):\n        return 3\n\nclass Two: a = 1; b = 2\n",
    );
    let seal2 = send_rpc(
        &repo,
        RpcMethod::SealCapsule {
            viewport_id: vp2.id.clone(),
            verify_cmd: None,
            summary: Some("vp2 expand One and Doc append".into()),
        },
    )
    .await
    .unwrap();
    assert!(matches!(seal2, RpcPayload::CapsuleSeal(_)));

    let merged_app = std::fs::read_to_string(repo.join("app.py")).unwrap();
    assert_eq!(
        merged_app,
        "class One:\n    x = 1\n\n    def n(self):\n        return 2\n\ndef other():\n    return 1\n"
    );

    let merged_doc_two = std::fs::read_to_string(repo.join("doc_two.py")).unwrap();
    assert_eq!(
        merged_doc_two,
        "class Doc:\n    \"\"\"doc\"\"\"\n    x = 1\n\n    def m(self):\n        return 3\n\nclass Two:\n    a = 1\n    b = 2\n\n    def tail(self):\n        return 4\n"
    );

    for (name, content) in [("app.py", &merged_app), ("doc_two.py", &merged_doc_two)] {
        assert!(
            !loom_core::cst::parse_source(loom_core::model::LanguageKind::Python, content, None)
                .unwrap()
                .tree
                .root_node()
                .has_error(),
            "post-seal Python CST has errors in {name}:\n{content}"
        );
    }

    send_rpc(&repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(
        running.wait().await.unwrap(),
        DaemonRunExit::ShutdownRequested
    );
}

fn dir_entry_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_pull_case_only_rename_rebase_preserves_file_and_exact_casing() {
    let tmp = TempDir::new().unwrap();
    let bare_dir = tmp.path().join("remote.git");
    let repo_a = tmp.path().join("repo_a");
    let repo_b = tmp.path().join("repo_b");
    std::fs::create_dir_all(&repo_a).unwrap();
    std::fs::create_dir_all(&repo_b).unwrap();

    let _bare = gix::init_bare(&bare_dir).unwrap();
    init_git_with_initial_commit(
        &repo_a,
        &[
            ("src/Foo.rs", "pub fn foo() -> i32 { 1 }\n"),
            ("src/Keep.rs", "pub fn keep() -> i32 { 1 }\n"),
        ],
    );
    let _rb = gix::init(&repo_b).unwrap();
    std::fs::write(repo_b.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

    // Seed bare repo via repo_a
    let (running_a, vp_seed) = init_and_spawn(&repo_a, true, "vp-seed", "seed").await;
    std::fs::write(vp_seed.path.join("src/seed.rs"), "pub fn seed() -> i32 { 0 }\n").unwrap();
    send_rpc(
        &repo_a,
        RpcMethod::SealCapsule {
            viewport_id: vp_seed.id.clone(),
            verify_cmd: None,
            summary: Some("seed".into()),
        },
    )
    .await
    .unwrap();
    send_rpc(
        &repo_a,
        RpcMethod::GitPush {
            remote: Some(bare_dir.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap();

    let running_b = match start_daemon(&repo_b, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("repo_b daemon should start"),
    };
    send_rpc(&repo_b, RpcMethod::Init { colocate_git: true })
        .await
        .unwrap();
    send_rpc(
        &repo_b,
        RpcMethod::GitPull {
            remote: Some(bare_dir.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap();

    let vp_b = match send_rpc(
        &repo_b,
        RpcMethod::SpawnViewport {
            name: "vp-b".into(),
            intent: "unrelated work".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn: {other:?}"),
    };
    std::fs::write(vp_b.path.join("src/other.rs"), "pub fn other() -> i32 { 7 }\n").unwrap();

    // Part A: repo_a performs same-content case-only rename `src/Foo.rs` -> `src/foo.rs`
    let vp_a1 = match send_rpc(
        &repo_a,
        RpcMethod::SpawnViewport {
            name: "vp-a1".into(),
            intent: "case rename Foo".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn: {other:?}"),
    };
    let tmp_foo = vp_a1.path.join("src/foo_tmp.rs");
    std::fs::rename(vp_a1.path.join("src/Foo.rs"), &tmp_foo).unwrap();
    std::fs::rename(&tmp_foo, vp_a1.path.join("src/foo.rs")).unwrap();
    send_rpc(
        &repo_a,
        RpcMethod::SealCapsule {
            viewport_id: vp_a1.id.clone(),
            verify_cmd: None,
            summary: Some("rename Foo -> foo".into()),
        },
    )
    .await
    .unwrap();
    send_rpc(
        &repo_a,
        RpcMethod::GitPush {
            remote: Some(bare_dir.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap();

    let pull_a = match send_rpc(
        &repo_b,
        RpcMethod::GitPull {
            remote: Some(bare_dir.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::GitPull(p) => p,
        other => panic!("unexpected pull: {other:?}"),
    };
    assert_eq!(pull_a.rebased_viewports.len(), 1);
    assert_eq!(
        pull_a.rebased_viewports[0].outcome,
        loom_core::rpc::ViewportRebaseOutcome::Clean,
        "same-content case-only rename rebase must be Clean: {:?}",
        pull_a.rebased_viewports[0]
    );
    let vp_src_names_a = dir_entry_names(&vp_b.path.join("src"));
    assert!(
        vp_src_names_a.iter().any(|n| n == "foo.rs") && !vp_src_names_a.iter().any(|n| n == "Foo.rs"),
        "vp_b must have exact `foo.rs` casing without silent deletion: {vp_src_names_a:?}"
    );
    assert_eq!(
        std::fs::read_to_string(vp_b.path.join("src/foo.rs")).unwrap(),
        "pub fn foo() -> i32 { 1 }\n"
    );

    // Part B: repo_a performs modified-content case-only rename `src/Keep.rs` -> `src/keep.rs`
    let vp_a2 = match send_rpc(
        &repo_a,
        RpcMethod::SpawnViewport {
            name: "vp-a2".into(),
            intent: "case rename + edit Keep".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn: {other:?}"),
    };
    std::fs::remove_file(vp_a2.path.join("src/Keep.rs")).unwrap();
    std::fs::write(
        vp_a2.path.join("src/keep.rs"),
        "pub fn keep() -> i32 { 999 }\n",
    )
    .unwrap();
    send_rpc(
        &repo_a,
        RpcMethod::SealCapsule {
            viewport_id: vp_a2.id.clone(),
            verify_cmd: None,
            summary: Some("rename Keep -> keep and edit".into()),
        },
    )
    .await
    .unwrap();
    send_rpc(
        &repo_a,
        RpcMethod::GitPush {
            remote: Some(bare_dir.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap();

    let pull_b = match send_rpc(
        &repo_b,
        RpcMethod::GitPull {
            remote: Some(bare_dir.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::GitPull(p) => p,
        other => panic!("unexpected pull: {other:?}"),
    };
    assert_eq!(pull_b.rebased_viewports.len(), 1);
    assert_eq!(
        pull_b.rebased_viewports[0].outcome,
        loom_core::rpc::ViewportRebaseOutcome::Clean,
        "modified-content case-only rename rebase must be Clean: {:?}",
        pull_b.rebased_viewports[0]
    );
    let vp_src_names_b = dir_entry_names(&vp_b.path.join("src"));
    assert!(
        vp_src_names_b.iter().any(|n| n == "keep.rs") && !vp_src_names_b.iter().any(|n| n == "Keep.rs"),
        "vp_b must have exact `keep.rs` casing: {vp_src_names_b:?}"
    );
    assert_eq!(
        std::fs::read_to_string(vp_b.path.join("src/keep.rs")).unwrap(),
        "pub fn keep() -> i32 { 999 }\n"
    );

    send_rpc(&repo_a, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running_a.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
    send_rpc(&repo_b, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running_b.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seal_and_undo_with_case_only_rename_in_viewport_and_trunk() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/Bar.rs"), "pub fn bar() -> i32 { 1 }\n").unwrap();
    std::fs::write(repo.join("src/Foo.rs"), "pub fn foo() -> i32 { 1 }\n").unwrap();
    std::fs::write(repo.join("src/other.rs"), "pub fn other() -> i32 { 1 }\n").unwrap();

    let (running, vp_ren) = init_and_spawn(&repo, false, "vp-ren", "viewport case rename").await;
    let vp_other = match send_rpc(
        &repo,
        RpcMethod::SpawnViewport {
            name: "vp-other".into(),
            intent: "edit other while trunk has case rename".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn: {other:?}"),
    };

    // 1. Viewport performs same-content case-only rename `src/Bar.rs` -> `src/bar.rs` and seals
    let tmp_bar = vp_ren.path.join("src/bar_tmp.rs");
    std::fs::rename(vp_ren.path.join("src/Bar.rs"), &tmp_bar).unwrap();
    std::fs::rename(&tmp_bar, vp_ren.path.join("src/bar.rs")).unwrap();
    let seal1 = send_rpc(
        &repo,
        RpcMethod::SealCapsule {
            viewport_id: vp_ren.id.clone(),
            verify_cmd: None,
            summary: Some("vp case rename Bar -> bar".into()),
        },
    )
    .await
    .unwrap();
    assert!(matches!(seal1, RpcPayload::CapsuleSeal(_)));
    let trunk_names_1 = dir_entry_names(&repo.join("src"));
    assert!(
        trunk_names_1.iter().any(|n| n == "bar.rs") && !trunk_names_1.iter().any(|n| n == "Bar.rs"),
        "trunk must have exact `bar.rs` casing after seal: {trunk_names_1:?}"
    );

    // 2. Trunk performs case-only rename `src/Foo.rs` -> `src/foo.rs` while vp_other edits `src/other.rs`
    let tmp_foo = repo.join("src/foo_tmp.rs");
    std::fs::rename(repo.join("src/Foo.rs"), &tmp_foo).unwrap();
    std::fs::rename(&tmp_foo, repo.join("src/foo.rs")).unwrap();
    write_vp_file_atomic(&vp_other.path, "src/other.rs", "pub fn other() -> i32 { 22 }\n");
    let seal2 = send_rpc(
        &repo,
        RpcMethod::SealCapsule {
            viewport_id: vp_other.id.clone(),
            verify_cmd: None,
            summary: Some("edit other while trunk renamed Foo -> foo".into()),
        },
    )
    .await
    .unwrap();
    assert!(matches!(seal2, RpcPayload::CapsuleSeal(_)));
    let trunk_names_2 = dir_entry_names(&repo.join("src"));
    assert!(
        trunk_names_2.iter().any(|n| n == "foo.rs") && !trunk_names_2.iter().any(|n| n == "Foo.rs"),
        "trunk must retain exact `foo.rs` casing after vp_other seal: {trunk_names_2:?}"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("src/other.rs")).unwrap(),
        "pub fn other() -> i32 { 22 }\n"
    );

    // 3. Undo seal2 then seal1 restores `src/Bar.rs` exact casing on trunk
    send_rpc(&repo, RpcMethod::Undo { op_seq: None }).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.join("src/other.rs")).unwrap(),
        "pub fn other() -> i32 { 1 }\n"
    );
    send_rpc(&repo, RpcMethod::Undo { op_seq: None }).await.unwrap();
    let trunk_names_3 = dir_entry_names(&repo.join("src"));
    assert!(
        trunk_names_3.iter().any(|n| n == "Bar.rs") && !trunk_names_3.iter().any(|n| n == "bar.rs"),
        "undoing seal1 must restore exact `Bar.rs` casing on trunk: {trunk_names_3:?}"
    );

    send_rpc(&repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn modified_content_case_only_rename_seal_and_undo_with_conflict_guard() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    let orig_body = "pub fn bar() -> i32 { 1 }\n";
    let new_body = "pub fn bar() -> i32 { 42 }\n";
    let ext_body = "pub fn bar() -> i32 { 99 }\n";
    std::fs::write(repo.join("src/Bar.rs"), orig_body).unwrap();

    let (running, vp1) = init_and_spawn(&repo, false, "vp-mod-case", "modified case rename").await;

    // 1. Viewport performs modified-content case-only rename `src/Bar.rs` -> `src/bar.rs` and seals
    let tmp_bar = vp1.path.join("src/bar_tmp.rs");
    std::fs::rename(vp1.path.join("src/Bar.rs"), &tmp_bar).unwrap();
    std::fs::rename(&tmp_bar, vp1.path.join("src/bar.rs")).unwrap();
    write_vp_file_atomic(&vp1.path, "src/bar.rs", new_body);

    let seal = send_rpc(
        &repo,
        RpcMethod::SealCapsule {
            viewport_id: vp1.id.clone(),
            verify_cmd: None,
            summary: Some("modified case-only rename Bar -> bar".into()),
        },
    )
    .await
    .unwrap();
    assert!(matches!(seal, RpcPayload::CapsuleSeal(_)));

    let trunk_after_seal = dir_entry_names(&repo.join("src"));
    assert!(
        trunk_after_seal.iter().any(|n| n == "bar.rs")
            && !trunk_after_seal.iter().any(|n| n == "Bar.rs"),
        "trunk must have exact `bar.rs` casing after seal: {trunk_after_seal:?}"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("src/bar.rs")).unwrap(),
        new_body
    );

    // 2. Genuine external modification on `src/bar.rs` must reject Undo with Conflict
    std::fs::write(repo.join("src/bar.rs"), ext_body).unwrap();
    let undo_conflict = send_rpc(&repo, RpcMethod::Undo { op_seq: None })
        .await
        .expect_err("external modification on src/bar.rs must fail Undo with Conflict");
    assert_eq!(undo_conflict.0, RpcErrorCode::Conflict);
    assert_eq!(
        std::fs::read_to_string(repo.join("src/bar.rs")).unwrap(),
        ext_body
    );

    // 3. External unmanaged case change `src/bar.rs` -> `src/BAR.rs` must also reject Undo with Conflict
    std::fs::write(repo.join("src/bar.rs"), new_body).unwrap();
    let tmp_upper = repo.join("src/bar_upper_tmp.rs");
    std::fs::rename(repo.join("src/bar.rs"), &tmp_upper).unwrap();
    std::fs::rename(&tmp_upper, repo.join("src/BAR.rs")).unwrap();
    let undo_case_conflict = send_rpc(&repo, RpcMethod::Undo { op_seq: None })
        .await
        .expect_err("external case change to src/BAR.rs must fail Undo with Conflict");
    assert_eq!(undo_case_conflict.0, RpcErrorCode::Conflict);
    let trunk_during_upper = dir_entry_names(&repo.join("src"));
    assert!(
        trunk_during_upper.iter().any(|n| n == "BAR.rs"),
        "conflicted Undo must not clobber trunk `BAR.rs`: {trunk_during_upper:?}"
    );

    // 4. Restore trunk to exact post-seal state (`src/bar.rs` with `new_body`), then Undo succeeds
    let tmp_restore = repo.join("src/bar_restore_tmp.rs");
    std::fs::rename(repo.join("src/BAR.rs"), &tmp_restore).unwrap();
    std::fs::rename(&tmp_restore, repo.join("src/bar.rs")).unwrap();

    send_rpc(&repo, RpcMethod::Undo { op_seq: None })
        .await
        .expect("clean modified-content case-only rename Undo must succeed");
    let trunk_after_undo = dir_entry_names(&repo.join("src"));
    assert!(
        trunk_after_undo.iter().any(|n| n == "Bar.rs")
            && !trunk_after_undo.iter().any(|n| n == "bar.rs"),
        "Undo must restore exact `Bar.rs` casing on trunk: {trunk_after_undo:?}"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("src/Bar.rs")).unwrap(),
        orig_body
    );

    send_rpc(&repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drop_viewport_with_case_only_rename_records_old_casing_deletion_patch() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/Foo.rs"), "pub fn foo() -> i32 { 1 }\n").unwrap();

    let (running, vp_drop) = init_and_spawn(&repo, false, "vp-drop-case", "drop case rename").await;

    let tmp_foo = vp_drop.path.join("src/foo_tmp.rs");
    std::fs::rename(vp_drop.path.join("src/Foo.rs"), &tmp_foo).unwrap();
    std::fs::rename(&tmp_foo, vp_drop.path.join("src/foo.rs")).unwrap();
    std::fs::write(vp_drop.path.join("src/foo.rs"), "pub fn foo() -> i32 { 77 }\n").unwrap();

    let drop_res = match send_rpc(
        &repo,
        RpcMethod::DropViewport {
            viewport_id: vp_drop.id.clone(),
            reason: "abandoned case-only rename".into(),
            kind: None,
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Drop(r) => r,
        other => panic!("unexpected drop payload: {other:?}"),
    };
    assert!(
        drop_res
            .negative_edge
            .touched_symbols
            .iter()
            .any(|s| s == "foo"),
        "touched_symbols must include `foo`: {:?}",
        drop_res.negative_edge.touched_symbols
    );

    let stored_vp = running
        .ctx
        .db
        .get_viewport(&vp_drop.id)
        .unwrap()
        .expect("dropped viewport must exist in db");
    let patches: Vec<_> = stored_vp
        .patch_ids
        .iter()
        .filter_map(|pid| running.ctx.db.get_patch(pid).ok().flatten())
        .collect();
    assert!(
        patches
            .iter()
            .any(|r| r.patch.file_path == std::path::Path::new("src/Foo.rs")
                && r.patch.cas_after.is_none()),
        "dropped viewport must record deletion patch on old casing `src/Foo.rs`: {patches:?}"
    );
    assert!(
        patches
            .iter()
            .any(|r| r.patch.file_path == std::path::Path::new("src/foo.rs")
                && r.patch.cas_after.is_some()),
        "dropped viewport must record patch on new casing `src/foo.rs`: {patches:?}"
    );
    for rec in &patches {
        assert!(
            drop_res
                .negative_edge
                .patch_hashes
                .contains(&rec.patch.patch_hash),
            "negative_edge.patch_hashes must include patch {}: {:?}",
            rec.patch_id,
            drop_res.negative_edge.patch_hashes
        );
    }

    send_rpc(&repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_pull_aborts_on_ntfs_case_colliding_unpushed_file() {
    let tmp = TempDir::new().unwrap();
    let bare = tmp.path().join("bare.git");
    let repo_a = tmp.path().join("repo_a");
    let repo_b = tmp.path().join("repo_b");
    std::fs::create_dir_all(&repo_a).unwrap();
    std::fs::create_dir_all(&repo_b).unwrap();
    let _bare = gix::init_bare(&bare).unwrap();

    init_git_with_initial_commit(&repo_a, &[("src/base.rs", "pub fn base() -> i32 { 1 }\n")]);

    let (running_a, vp_a) = init_and_spawn(&repo_a, true, "vp-a", "push lowercase newfile").await;
    // Push common initial commit first so repo_b can pull the shared ancestor
    send_rpc(
        &repo_a,
        RpcMethod::GitPush {
            remote: Some(bare.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap();

    // Initialize repo_b and pull the shared initial commit
    let _ = gix::init(&repo_b).unwrap();
    let (running_b, vp_b) = init_and_spawn(&repo_b, true, "vp-b", "add PascalCase NewFile").await;
    send_rpc(
        &repo_b,
        RpcMethod::GitPull {
            remote: Some(bare.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap();

    // Now repo_a seals and pushes lowercase `src/newfile.rs`
    write_vp_file_atomic(&vp_a.path, "src/newfile.rs", "pub fn remote_a() -> i32 { 10 }\n");
    send_rpc(
        &repo_a,
        RpcMethod::SealCapsule {
            viewport_id: vp_a.id.clone(),
            verify_cmd: None,
            summary: Some("add lowercase newfile.rs".into()),
        },
    )
    .await
    .unwrap();
    send_rpc(
        &repo_a,
        RpcMethod::GitPush {
            remote: Some(bare.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap();
    send_rpc(&repo_a, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running_a.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);

    // repo_b seals unpushed PascalCase `src/NewFile.rs`
    write_vp_file_atomic(&vp_b.path, "src/NewFile.rs", "pub fn local_b() -> i32 { 20 }\n");
    send_rpc(
        &repo_b,
        RpcMethod::SealCapsule {
            viewport_id: vp_b.id.clone(),
            verify_cmd: None,
            summary: Some("add NewFile.rs unpushed".into()),
        },
    )
    .await
    .unwrap();

    let pull_err = send_rpc(
        &repo_b,
        RpcMethod::GitPull {
            remote: Some(bare.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .expect_err("NTFS case-colliding newfile.rs vs NewFile.rs must abort GitPull with Conflict");
    assert_eq!(pull_err.0, RpcErrorCode::Conflict);
    assert!(
        pull_err.1.contains("case-colliding"),
        "expected case-colliding conflict message, got: {}",
        pull_err.1
    );

    // Local unpushed file and disk casing must remain intact (not clobbered by remote_a)
    let names = dir_entry_names(&repo_b.join("src"));
    assert!(
        names.iter().any(|n| n == "NewFile.rs") && !names.iter().any(|n| n == "newfile.rs"),
        "aborted pull must preserve local `NewFile.rs` casing: {names:?}"
    );
    assert_eq!(
        std::fs::read_to_string(repo_b.join("src/NewFile.rs")).unwrap(),
        "pub fn local_b() -> i32 { 20 }\n"
    );

    send_rpc(&repo_b, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running_b.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_pull_case_only_rename_rebases_active_caller_cleanly_and_detects_sig_break() {
    let tmp = TempDir::new().unwrap();
    let bare = tmp.path().join("bare.git");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let _bare = gix::init_bare(&bare).unwrap();
    init_git_with_initial_commit(
        &repo,
        &[
            ("src/Foo.rs", "pub fn helper_fn() -> i32 { 1 }\n"),
            ("src/caller.rs", "pub fn call() -> i32 { helper_fn() }\n"),
        ],
    );

    let (running, vp_active) = init_and_spawn(&repo, true, "vp-caller", "edit caller").await;
    send_rpc(
        &repo,
        RpcMethod::GitPush {
            remote: Some(bare.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap();

    // Active viewport edits src/caller.rs while still calling helper_fn()
    write_vp_file_atomic(
        &vp_active.path,
        "src/caller.rs",
        "pub fn call() -> i32 { helper_fn() + 10 }\n",
    );
    running.ctx.radar.flush_viewport(&vp_active.id).await;

    // Also seal an unpushed capsule on another viewport editing src/caller_unpushed.rs (or caller2)
    // to exercise `merge_3way_trees` during the same remote rename!
    let vp_seal = match send_rpc(
        &repo,
        RpcMethod::SpawnViewport {
            name: "vp-seal".into(),
            intent: "add caller2".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn payload: {other:?}"),
    };
    write_vp_file_atomic(
        &vp_seal.path,
        "src/caller2.rs",
        "pub fn call2() -> i32 { helper_fn() + 20 }\n",
    );
    send_rpc(
        &repo,
        RpcMethod::SealCapsule {
            viewport_id: vp_seal.id.clone(),
            verify_cmd: None,
            summary: Some("unpushed caller2".into()),
        },
    )
    .await
    .unwrap();

    // 1. Remote renames `src/Foo.rs` -> `src/foo.rs` with identical `helper_fn` signature
    {
        let mut bare_rw = gix::open(&bare).unwrap();
        {
            let mut cfg = bare_rw.config_snapshot_mut();
            cfg.append_config(
                ["user.name=Remote", "user.email=remote@example.com"],
                gix::config::Source::Api,
            )
            .unwrap();
            cfg.commit().unwrap();
        }
        let pid = bare_rw
            .try_find_reference("refs/heads/main")
            .unwrap()
            .unwrap()
            .peel_to_id()
            .unwrap()
            .detach();
        let ptree = bare_rw.find_commit(pid).unwrap().tree_id().unwrap().detach();
        let mut ed = bare_rw.edit_tree(ptree).unwrap();
        ed.remove("src/Foo.rs").unwrap();
        let blob = bare_rw
            .write_blob(b"pub fn helper_fn() -> i32 { 1 }\n")
            .unwrap()
            .detach();
        ed.upsert("src/foo.rs", gix::objs::tree::EntryKind::Blob, blob)
            .unwrap();
        let tree = ed.write().unwrap().detach();
        bare_rw
            .commit("refs/heads/main", "rename Foo.rs to foo.rs", tree, [pid])
            .unwrap();
    }

    let pull1 = match send_rpc(
        &repo,
        RpcMethod::GitPull {
            remote: Some(bare.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::GitPull(p) => p,
        other => panic!("unexpected pull payload: {other:?}"),
    };
    assert!(pull1.updated_trunk);
    assert_eq!(pull1.rebased_viewports.len(), 1);
    assert_eq!(
        pull1.rebased_viewports[0].outcome,
        ViewportRebaseOutcome::Clean,
        "remote case-only rename with identical symbol must not cause false SemanticConflict: {:?}",
        pull1.rebased_viewports[0].conflicts
    );
    assert!(pull1.rebased_viewports[0].conflicts.is_empty());

    // Verify exact casing `src/foo.rs` on both trunk and rebased active viewport
    let trunk_names = dir_entry_names(&repo.join("src"));
    assert!(trunk_names.iter().any(|n| n == "foo.rs") && !trunk_names.iter().any(|n| n == "Foo.rs"));
    let vp_names = dir_entry_names(&vp_active.path.join("src"));
    assert!(vp_names.iter().any(|n| n == "foo.rs") && !vp_names.iter().any(|n| n == "Foo.rs"));

    // 2. Remote renames `src/foo.rs` -> `src/bar.rs` AND incompatibly changes `helper_fn` arity!
    //    First, `merge_3way_trees` against unpushed capsule `caller2.rs` aborts pull with Conflict (`SemanticConflict`).
    {
        let mut bare_rw = gix::open(&bare).unwrap();
        {
            let mut cfg = bare_rw.config_snapshot_mut();
            cfg.append_config(
                ["user.name=Remote", "user.email=remote@example.com"],
                gix::config::Source::Api,
            )
            .unwrap();
            cfg.commit().unwrap();
        }
        let pid = bare_rw
            .try_find_reference("refs/heads/main")
            .unwrap()
            .unwrap()
            .peel_to_id()
            .unwrap()
            .detach();
        let ptree = bare_rw.find_commit(pid).unwrap().tree_id().unwrap().detach();
        let mut ed = bare_rw.edit_tree(ptree).unwrap();
        ed.remove("src/foo.rs").unwrap();
        let blob = bare_rw
            .write_blob(b"pub fn helper_fn(x: i32) -> i32 { x }\n")
            .unwrap()
            .detach();
        ed.upsert("src/bar.rs", gix::objs::tree::EntryKind::Blob, blob)
            .unwrap();
        let tree = ed.write().unwrap().detach();
        bare_rw
            .commit("refs/heads/main", "rename foo.rs to bar.rs with breaking arity", tree, [pid])
            .unwrap();
    }

    let pull2_err = send_rpc(
        &repo,
        RpcMethod::GitPull {
            remote: Some(bare.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .expect_err("rename + breaking arity against unpushed capsule caller2.rs must abort pull with Conflict");
    assert_eq!(pull2_err.0, RpcErrorCode::Conflict);
    assert!(
        pull2_err.1.contains("helper_fn"),
        "expected helper_fn SemanticConflict in pull2 abort, got: {}",
        pull2_err.1
    );

    send_rpc(&repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);

    // 3. Without unpushed capsules, active viewport rebase (`compute_viewport_rebase_plan`)
    //    must also catch rename + incompatible signature change as `ViewportRebaseOutcome::Conflict`.
    let bare2 = tmp.path().join("bare2.git");
    let repo2 = tmp.path().join("repo2");
    std::fs::create_dir_all(&repo2).unwrap();
    let _bare2 = gix::init_bare(&bare2).unwrap();
    init_git_with_initial_commit(
        &repo2,
        &[
            ("src/Foo.rs", "pub fn helper_fn() -> i32 { 1 }\n"),
            ("src/caller.rs", "pub fn call() -> i32 { helper_fn() }\n"),
        ],
    );
    let (running2, vp2) = init_and_spawn(&repo2, true, "vp2-caller", "edit caller").await;
    send_rpc(
        &repo2,
        RpcMethod::GitPush {
            remote: Some(bare2.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap();
    write_vp_file_atomic(
        &vp2.path,
        "src/caller.rs",
        "pub fn call() -> i32 { helper_fn() + 10 }\n",
    );
    running2.ctx.radar.flush_viewport(&vp2.id).await;

    {
        let mut bare_rw = gix::open(&bare2).unwrap();
        {
            let mut cfg = bare_rw.config_snapshot_mut();
            cfg.append_config(
                ["user.name=Remote", "user.email=remote@example.com"],
                gix::config::Source::Api,
            )
            .unwrap();
            cfg.commit().unwrap();
        }
        let pid = bare_rw
            .try_find_reference("refs/heads/main")
            .unwrap()
            .unwrap()
            .peel_to_id()
            .unwrap()
            .detach();
        let ptree = bare_rw.find_commit(pid).unwrap().tree_id().unwrap().detach();
        let mut ed = bare_rw.edit_tree(ptree).unwrap();
        ed.remove("src/Foo.rs").unwrap();
        let blob = bare_rw
            .write_blob(b"pub fn helper_fn(x: i32) -> i32 { x }\n")
            .unwrap()
            .detach();
        ed.upsert("src/bar.rs", gix::objs::tree::EntryKind::Blob, blob)
            .unwrap();
        let tree = ed.write().unwrap().detach();
        bare_rw
            .commit("refs/heads/main", "rename Foo.rs to bar.rs with breaking arity", tree, [pid])
            .unwrap();
    }

    let pull3 = match send_rpc(
        &repo2,
        RpcMethod::GitPull {
            remote: Some(bare2.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::GitPull(p) => p,
        other => panic!("unexpected pull payload: {other:?}"),
    };
    assert_eq!(pull3.rebased_viewports.len(), 1);
    assert_eq!(
        pull3.rebased_viewports[0].outcome,
        ViewportRebaseOutcome::Conflict,
        "rename + incompatible signature change must still emit SemanticConflict on active rebase: {:?}",
        pull3.rebased_viewports[0]
    );
    assert!(
        pull3.rebased_viewports[0]
            .conflicts
            .iter()
            .any(|c| c.symbol_fqn.as_deref() == Some("helper_fn")),
        "expected SemanticConflict on helper_fn, got {:?}",
        pull3.rebased_viewports[0].conflicts
    );

    send_rpc(&repo2, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running2.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[cfg(windows)]
fn rename_with_windows_retry(from: &Path, to: &Path) {
    let mut attempts = 0usize;
    loop {
        match std::fs::rename(from, to) {
            Ok(()) => return,
            Err(err)
                if err.kind() == std::io::ErrorKind::PermissionDenied && attempts < 25 =>
            {
                attempts += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => panic!(
                "rename `{}` -> `{}` failed after {attempts} retries: {err}",
                from.display(),
                to.display()
            ),
        }
    }
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seal_rejects_windows_case_collision_and_preserves_legitimate_case_rename() {
    // 1. Concurrent create `src/Helper.rs` vs `src/helper.rs`
    {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo1");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/lib.rs"), "pub fn root() -> i32 { 0 }\n").unwrap();

        let (running, vp1) = init_and_spawn(&repo, false, "vp1", "create helper").await;
        let vp2 = match send_rpc(
            &repo,
            RpcMethod::SpawnViewport {
                name: "vp2".into(),
                intent: "create Helper".into(),
                cmd: vec![],
            },
        )
        .await
        .unwrap()
        {
            RpcPayload::Viewport(v) => v,
            other => panic!("unexpected spawn payload: {other:?}"),
        };

        std::fs::write(vp2.path.join("src/Helper.rs"), "pub fn from_vp2() -> i32 { 2 }\n").unwrap();
        send_rpc(
            &repo,
            RpcMethod::SealCapsule {
                viewport_id: vp2.id.clone(),
                verify_cmd: None,
                summary: Some("add Helper.rs".into()),
            },
        )
        .await
        .expect("vp2 seal of Helper.rs must succeed");

        let capsules_before = running.ctx.db.list_capsules().unwrap().len();
        std::fs::write(vp1.path.join("src/helper.rs"), "pub fn from_vp1() -> i32 { 1 }\n").unwrap();
        let seal1_err = send_rpc(
            &repo,
            RpcMethod::SealCapsule {
                viewport_id: vp1.id.clone(),
                verify_cmd: None,
                summary: Some("add helper.rs".into()),
            },
        )
        .await
        .expect_err("vp1 seal of case-colliding src/helper.rs must fail with Conflict");
        assert_eq!(seal1_err.0, RpcErrorCode::Conflict);

        let trunk_names = dir_entry_names(&repo.join("src"));
        assert!(
            trunk_names.iter().any(|n| n == "Helper.rs")
                && !trunk_names.iter().any(|n| n == "helper.rs"),
            "trunk must preserve exact Helper.rs casing: {trunk_names:?}"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("src/Helper.rs")).unwrap(),
            "pub fn from_vp2() -> i32 { 2 }\n"
        );
        assert_eq!(running.ctx.db.list_capsules().unwrap().len(), capsules_before);
        assert_eq!(
            running.ctx.db.get_viewport(&vp1.id).unwrap().unwrap().state,
            ViewportState::Active
        );

        send_rpc(&repo, RpcMethod::Shutdown).await.unwrap();
        assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
    }

    // 2. Concurrent rename `src/old.rs -> src/Target.rs` vs create `src/target.rs`
    {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo2");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/old.rs"), "pub fn moved_fn() -> i32 { 42 }\n").unwrap();

        let (running, vp1) = init_and_spawn(&repo, false, "vp1", "create target").await;
        let vp2 = match send_rpc(
            &repo,
            RpcMethod::SpawnViewport {
                name: "vp2".into(),
                intent: "rename old to Target".into(),
                cmd: vec![],
            },
        )
        .await
        .unwrap()
        {
            RpcPayload::Viewport(v) => v,
            other => panic!("unexpected spawn payload: {other:?}"),
        };

        rename_with_windows_retry(&vp2.path.join("src/old.rs"), &vp2.path.join("src/Target.rs"));
        send_rpc(
            &repo,
            RpcMethod::SealCapsule {
                viewport_id: vp2.id.clone(),
                verify_cmd: None,
                summary: Some("rename to Target.rs".into()),
            },
        )
        .await
        .expect("vp2 rename seal must succeed");

        std::fs::write(vp1.path.join("src/target.rs"), "pub fn unrelated() -> i32 { 99 }\n")
            .unwrap();
        let seal1_err = send_rpc(
            &repo,
            RpcMethod::SealCapsule {
                viewport_id: vp1.id.clone(),
                verify_cmd: None,
                summary: Some("create target.rs".into()),
            },
        )
        .await
        .expect_err("vp1 create target.rs colliding with renamed Target.rs must fail with Conflict");
        assert_eq!(seal1_err.0, RpcErrorCode::Conflict);
        let trunk_names = dir_entry_names(&repo.join("src"));
        assert!(trunk_names.iter().any(|n| n == "Target.rs"));
        assert_eq!(
            std::fs::read_to_string(repo.join("src/Target.rs")).unwrap(),
            "pub fn moved_fn() -> i32 { 42 }\n"
        );

        send_rpc(&repo, RpcMethod::Shutdown).await.unwrap();
        assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
    }

    // 3. Parent directory casing collision (`Alt/a.rs` vs `alt/b.rs`), legitimate case-only file rename,
    //    and legitimate case-only directory rename when no differently-cased files remain.
    {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo3");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/helper.rs"), "pub fn helper() -> i32 { 7 }\n").unwrap();

        let (running, vp_rename) = init_and_spawn(&repo, false, "vp-rename", "case rename file").await;
        // Legitimate case-only file rename: src/helper.rs -> src/Helper.rs
        let tmp_p = vp_rename.path.join("src/helper_tmp.rs");
        rename_with_windows_retry(&vp_rename.path.join("src/helper.rs"), &tmp_p);
        rename_with_windows_retry(&tmp_p, &vp_rename.path.join("src/Helper.rs"));
        send_rpc(
            &repo,
            RpcMethod::SealCapsule {
                viewport_id: vp_rename.id.clone(),
                verify_cmd: None,
                summary: Some("case-only rename file".into()),
            },
        )
        .await
        .expect("legitimate case-only file rename must succeed");
        let trunk_names = dir_entry_names(&repo.join("src"));
        assert!(
            trunk_names.iter().any(|n| n == "Helper.rs")
                && !trunk_names.iter().any(|n| n == "helper.rs"),
            "legitimate case-only file rename must update trunk casing: {trunk_names:?}"
        );

        // Parent dir collision: vp_a seals Alt/a.rs, vp_b tries to seal alt/b.rs -> Conflict
        let vp_a = match send_rpc(
            &repo,
            RpcMethod::SpawnViewport {
                name: "vp-a".into(),
                intent: "add Alt/a.rs".into(),
                cmd: vec![],
            },
        )
        .await
        .unwrap()
        {
            RpcPayload::Viewport(v) => v,
            other => panic!("unexpected spawn payload: {other:?}"),
        };
        let vp_b = match send_rpc(
            &repo,
            RpcMethod::SpawnViewport {
                name: "vp-b".into(),
                intent: "add alt/b.rs".into(),
                cmd: vec![],
            },
        )
        .await
        .unwrap()
        {
            RpcPayload::Viewport(v) => v,
            other => panic!("unexpected spawn payload: {other:?}"),
        };

        std::fs::create_dir_all(vp_a.path.join("Alt")).unwrap();
        std::fs::write(vp_a.path.join("Alt/a.rs"), "pub fn a() -> i32 { 1 }\n").unwrap();
        send_rpc(
            &repo,
            RpcMethod::SealCapsule {
                viewport_id: vp_a.id.clone(),
                verify_cmd: None,
                summary: Some("add Alt/a.rs".into()),
            },
        )
        .await
        .expect("vp_a seal must succeed");

        std::fs::create_dir_all(vp_b.path.join("alt")).unwrap();
        std::fs::write(vp_b.path.join("alt/b.rs"), "pub fn b() -> i32 { 2 }\n").unwrap();
        let dir_err = send_rpc(
            &repo,
            RpcMethod::SealCapsule {
                viewport_id: vp_b.id.clone(),
                verify_cmd: None,
                summary: Some("add alt/b.rs".into()),
            },
        )
        .await
        .expect_err("parent directory casing collision Alt vs alt must fail with Conflict");
        assert_eq!(dir_err.0, RpcErrorCode::Conflict);
        let root_names = dir_entry_names(&repo);
        assert!(root_names.iter().any(|n| n == "Alt") && !root_names.iter().any(|n| n == "alt"));

        // Legitimate case-only directory rename when no other files remain in Alt/
        let vp_dir_rename = match send_rpc(
            &repo,
            RpcMethod::SpawnViewport {
                name: "vp-dir-rename".into(),
                intent: "rename Alt/a.rs -> alt/a.rs".into(),
                cmd: vec![],
            },
        )
        .await
        .unwrap()
        {
            RpcPayload::Viewport(v) => v,
            other => panic!("unexpected spawn payload: {other:?}"),
        };
        let tmp_dir = vp_dir_rename.path.join("Alt_tmp");
        rename_with_windows_retry(&vp_dir_rename.path.join("Alt"), &tmp_dir);
        rename_with_windows_retry(&tmp_dir, &vp_dir_rename.path.join("alt"));
        send_rpc(
            &repo,
            RpcMethod::SealCapsule {
                viewport_id: vp_dir_rename.id.clone(),
                verify_cmd: None,
                summary: Some("case-only dir rename".into()),
            },
        )
        .await
        .expect("legitimate case-only directory rename must succeed");
        let root_after = dir_entry_names(&repo);
        assert!(
            root_after.iter().any(|n| n == "alt") && !root_after.iter().any(|n| n == "Alt"),
            "legitimate case-only dir rename must update trunk dir casing: {root_after:?}"
        );

        send_rpc(&repo, RpcMethod::Shutdown).await.unwrap();
        assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
    }
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_pull_without_unpushed_capsules_rejects_case_colliding_remote_tree() {
    let tmp = TempDir::new().unwrap();
    let bare = tmp.path().join("bare.git");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let _bare = gix::init_bare(&bare).unwrap();

    init_git_with_initial_commit(&repo, &[("src/base.rs", "pub fn base() -> i32 { 1 }\n")]);
    let (running, _vp_unused) = init_and_spawn(&repo, true, "vp-unused", "idle").await;

    send_rpc(
        &repo,
        RpcMethod::GitPush {
            remote: Some(bare.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap();

    let local_repo = gix::open(&repo).unwrap();
    let orig_head = local_repo
        .try_find_reference("refs/heads/main")
        .unwrap()
        .unwrap()
        .peel_to_id()
        .unwrap()
        .detach();
    let orig_capsules_len = running.ctx.db.list_capsules().unwrap().len();
    let orig_git_map_len = running.ctx.db.list_git_maps().unwrap().len();

    // Craft a bare-repo commit containing BOTH `src/Foo.rs` and `src/foo.rs`
    {
        let mut bare_rw = gix::open(&bare).unwrap();
        {
            let mut cfg = bare_rw.config_snapshot_mut();
            cfg.append_config(
                ["user.name=Remote", "user.email=remote@example.com"],
                gix::config::Source::Api,
            )
            .unwrap();
            cfg.commit().unwrap();
        }
        let pid = bare_rw
            .try_find_reference("refs/heads/main")
            .unwrap()
            .unwrap()
            .peel_to_id()
            .unwrap()
            .detach();
        let ptree = bare_rw.find_commit(pid).unwrap().tree_id().unwrap().detach();
        let mut ed = bare_rw.edit_tree(ptree).unwrap();
        let b1 = bare_rw
            .write_blob(b"pub fn upper() -> i32 { 10 }\n")
            .unwrap()
            .detach();
        let b2 = bare_rw
            .write_blob(b"pub fn lower() -> i32 { 20 }\n")
            .unwrap()
            .detach();
        ed.upsert("src/Foo.rs", gix::objs::tree::EntryKind::Blob, b1)
            .unwrap();
        ed.upsert("src/foo.rs", gix::objs::tree::EntryKind::Blob, b2)
            .unwrap();
        let tree = ed.write().unwrap().detach();
        bare_rw
            .commit("refs/heads/main", "add colliding Foo.rs and foo.rs", tree, [pid])
            .unwrap();
    }

    let pull_err = send_rpc(
        &repo,
        RpcMethod::GitPull {
            remote: Some(bare.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .expect_err("GitPull of remote commit with both src/Foo.rs and src/foo.rs must fail with Conflict");
    assert_eq!(pull_err.0, RpcErrorCode::Conflict);

    // Local refs, trunk worktree, and StateDb must remain completely untouched
    let head_after = gix::open(&repo)
        .unwrap()
        .try_find_reference("refs/heads/main")
        .unwrap()
        .unwrap()
        .peel_to_id()
        .unwrap()
        .detach();
    assert_eq!(head_after, orig_head, "local branch ref must not advance on Conflict");
    let trunk_names = dir_entry_names(&repo.join("src"));
    assert_eq!(trunk_names, vec!["base.rs".to_string()]);
    assert_eq!(running.ctx.db.list_capsules().unwrap().len(), orig_capsules_len);
    assert_eq!(running.ctx.db.list_git_maps().unwrap().len(), orig_git_map_len);

    // Now replace that colliding remote tip with a legitimate case-only rename (`src/base.rs -> src/Base.rs`)
    // and verify GitPull without unpushed capsules succeeds cleanly.
    {
        let mut bare_rw = gix::open(&bare).unwrap();
        {
            let mut cfg = bare_rw.config_snapshot_mut();
            cfg.append_config(
                ["user.name=Remote", "user.email=remote@example.com"],
                gix::config::Source::Api,
            )
            .unwrap();
            cfg.commit().unwrap();
        }
        let pid = bare_rw
            .try_find_reference("refs/heads/main")
            .unwrap()
            .unwrap()
            .peel_to_id()
            .unwrap()
            .detach();
        let base_tree = bare_rw.find_commit(orig_head).unwrap().tree_id().unwrap().detach();
        let mut ed = bare_rw.edit_tree(base_tree).unwrap();
        ed.remove("src/base.rs").unwrap();
        let b_ren = bare_rw
            .write_blob(b"pub fn base() -> i32 { 1 }\n")
            .unwrap()
            .detach();
        ed.upsert("src/Base.rs", gix::objs::tree::EntryKind::Blob, b_ren)
            .unwrap();
        let tree = ed.write().unwrap().detach();
        bare_rw
            .edit_reference(gix::refs::transaction::RefEdit::update(
                gix::refs::FullName::try_from("refs/heads/main").unwrap(),
                orig_head,
                gix::refs::transaction::PreviousValue::Any,
                "reset main to orig_head",
            ))
            .unwrap();
        bare_rw
            .commit("refs/heads/main", "case-only rename base.rs -> Base.rs", tree, [orig_head])
            .unwrap();
        let _ = pid;
    }

    let pull_ok = match send_rpc(
        &repo,
        RpcMethod::GitPull {
            remote: Some(bare.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .expect("legitimate remote case-only rename must pull cleanly")
    {
        RpcPayload::GitPull(p) => p,
        other => panic!("unexpected pull payload: {other:?}"),
    };
    assert!(pull_ok.updated_trunk);
    let trunk_names_ok = dir_entry_names(&repo.join("src"));
    assert_eq!(trunk_names_ok, vec!["Base.rs".to_string()]);

    send_rpc(&repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_pull_rebase_detects_parent_dir_casing_conflict_and_allows_pure_dir_rename() {
    let tmp = TempDir::new().unwrap();
    let bare = tmp.path().join("bare.git");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let _bare = gix::init_bare(&bare).unwrap();

    init_git_with_initial_commit(
        &repo,
        &[
            ("src/a.rs", "pub fn a() -> i32 { 1 }\n"),
            ("keep/k.rs", "pub fn k() -> i32 { 2 }\n"),
        ],
    );
    let (running, vp_conflict) =
        init_and_spawn(&repo, true, "vp-conflict", "adds src/c.rs").await;
    let vp_clean = match send_rpc(
        &repo,
        RpcMethod::SpawnViewport {
            name: "vp-clean".into(),
            intent: "edits keep/k.rs only".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn payload: {other:?}"),
    };

    send_rpc(
        &repo,
        RpcMethod::GitPush {
            remote: Some(bare.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap();

    // vp_conflict retains lowercase `src/` by adding `src/c.rs`.
    write_vp_file_atomic(&vp_conflict.path, "src/c.rs", "pub fn c() -> i32 { 3 }\n");
    running.ctx.radar.flush_viewport(&vp_conflict.id).await;
    let vp_conflict_base_before = running
        .ctx
        .db
        .get_viewport(&vp_conflict.id)
        .unwrap()
        .unwrap()
        .base_snapshot_cas;

    // vp_clean only modifies `keep/k.rs`, leaving no retained files in `src/`.
    write_vp_file_atomic(&vp_clean.path, "keep/k.rs", "pub fn k() -> i32 { 20 }\n");
    running.ctx.radar.flush_viewport(&vp_clean.id).await;

    // Remote removes `src/a.rs` and adds `Src/b.rs` (capital `S`).
    {
        let mut bare_rw = gix::open(&bare).unwrap();
        {
            let mut cfg = bare_rw.config_snapshot_mut();
            cfg.append_config(
                ["user.name=Remote", "user.email=remote@example.com"],
                gix::config::Source::Api,
            )
            .unwrap();
            cfg.commit().unwrap();
        }
        let pid = bare_rw
            .try_find_reference("refs/heads/main")
            .unwrap()
            .unwrap()
            .peel_to_id()
            .unwrap()
            .detach();
        let ptree = bare_rw.find_commit(pid).unwrap().tree_id().unwrap().detach();
        let mut ed = bare_rw.edit_tree(ptree).unwrap();
        ed.remove("src/a.rs").unwrap();
        let blob = bare_rw
            .write_blob(b"pub fn b() -> i32 { 4 }\n")
            .unwrap()
            .detach();
        ed.upsert("Src/b.rs", gix::objs::tree::EntryKind::Blob, blob)
            .unwrap();
        let tree = ed.write().unwrap().detach();
        bare_rw
            .commit("refs/heads/main", "move src/a.rs to Src/b.rs", tree, [pid])
            .unwrap();
    }

    let pull_res = match send_rpc(
        &repo,
        RpcMethod::GitPull {
            remote: Some(bare.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::GitPull(p) => p,
        other => panic!("unexpected pull payload: {other:?}"),
    };
    assert!(pull_res.updated_trunk);
    let trunk_root_names = dir_entry_names(&repo);
    assert!(
        trunk_root_names.iter().any(|n| n == "Src")
            && !trunk_root_names.iter().any(|n| n == "src"),
        "trunk must have Src/ casing: {trunk_root_names:?}"
    );

    // vp_conflict must be flagged as Conflict with file_path = Some("Src/b.rs") and must not drift
    let st_conflict = pull_res
        .rebased_viewports
        .iter()
        .find(|s| s.viewport_id == vp_conflict.id)
        .expect("vp_conflict status present");
    assert_eq!(st_conflict.outcome, ViewportRebaseOutcome::Conflict);
    assert!(
        st_conflict
            .conflicts
            .iter()
            .any(|c| c.file_path.as_deref() == Some(Path::new("Src/b.rs"))),
        "expected conflict with file_path Some(Src/b.rs), got {:?}",
        st_conflict.conflicts
    );
    let vp_conflict_base_after = running
        .ctx
        .db
        .get_viewport(&vp_conflict.id)
        .unwrap()
        .unwrap()
        .base_snapshot_cas;
    assert_eq!(
        vp_conflict_base_after, vp_conflict_base_before,
        "conflicted rebase must NOT advance base_snapshot_cas"
    );
    assert!(!vp_conflict.path.join("src/b.rs").exists());
    assert!(vp_conflict.path.join("src/c.rs").exists());

    // vp_clean had no retained files in `src/`, so pure directory rename rebases Clean with `Src/`
    let st_clean = pull_res
        .rebased_viewports
        .iter()
        .find(|s| s.viewport_id == vp_clean.id)
        .expect("vp_clean status present");
    assert_eq!(
        st_clean.outcome,
        ViewportRebaseOutcome::Clean,
        "pure dir rename in vp_clean must succeed: {:?}",
        st_clean.conflicts
    );
    let vp_clean_root_names = dir_entry_names(&vp_clean.path);
    assert!(
        vp_clean_root_names.iter().any(|n| n == "Src")
            && !vp_clean_root_names.iter().any(|n| n == "src"),
        "vp_clean must adopt Src/ casing: {vp_clean_root_names:?}"
    );

    // Sealing vp_clean must NOT revert trunk's `Src/` back to `src/`
    send_rpc(
        &repo,
        RpcMethod::SealCapsule {
            viewport_id: vp_clean.id.clone(),
            verify_cmd: None,
            summary: Some("seal vp_clean".into()),
        },
    )
    .await
    .expect("seal vp_clean must succeed");
    let trunk_root_after_seal = dir_entry_names(&repo);
    assert!(
        trunk_root_after_seal.iter().any(|n| n == "Src")
            && !trunk_root_after_seal.iter().any(|n| n == "src"),
        "trunk Src/ casing must remain intact after vp_clean seal: {trunk_root_after_seal:?}"
    );

    send_rpc(&repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn undo_destructive_drop_clears_stale_patches_and_prevents_phantom_attribution() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn base() -> i32 { 1 }\n").unwrap();

    let (running, vp1) = init_and_spawn(repo, false, "vp-drop-undo-a", "case A").await;

    // Also advance trunk via a peer viewport so drop's restore_clean picks up newer trunk
    let vp_peer = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "vp-peer".into(),
            intent: "advance trunk".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn payload: {other:?}"),
    };
    write_vp_file_atomic(
        &vp_peer.path,
        "src/trunk_added.rs",
        "pub fn trunk_added() -> i32 { 42 }\n",
    );
    send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp_peer.id.clone(),
            verify_cmd: None,
            summary: Some("advance trunk".into()),
        },
    )
    .await
    .unwrap();

    // Case A: modify file -> Radar flush -> drop_viewport -> undo_op -> verify clean DB/Radar & zero phantom attribution on immediate seal
    write_vp_file_atomic(&vp1.path, "src/lib.rs", "pub fn bad_idea() -> i32 { 99 }\n");
    running.ctx.radar.flush_viewport(&vp1.id).await;
    let vp_before_drop = running.ctx.db.get_viewport(&vp1.id).unwrap().unwrap();
    assert!(
        !vp_before_drop.patch_ids.is_empty(),
        "expected flushed AstPatch before drop"
    );

    send_rpc(
        repo,
        RpcMethod::DropViewport {
            viewport_id: vp1.id.clone(),
            reason: "dead end".into(),
            kind: Some("DesignDeadEnd".into()),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(vp1.path.join("src/lib.rs")).unwrap(),
        "pub fn base() -> i32 { 1 }\n"
    );
    assert_eq!(
        std::fs::read_to_string(vp1.path.join("src/trunk_added.rs")).unwrap(),
        "pub fn trunk_added() -> i32 { 42 }\n"
    );

    let patch_count_before_undo = running.ctx.db.list_patches().unwrap().len();

    send_rpc(repo, RpcMethod::Undo { op_seq: None })
        .await
        .unwrap();

    let vp_after_undo = running.ctx.db.get_viewport(&vp1.id).unwrap().unwrap();
    assert_eq!(vp_after_undo.state, ViewportState::Active);
    assert!(
        vp_after_undo.patch_ids.is_empty(),
        "undoing ViewportDrop must clear stale patch_ids, got {:?}",
        vp_after_undo.patch_ids
    );
    assert!(vp_after_undo.head_cas.is_none());
    assert_eq!(
        running.ctx.db.list_patches().unwrap().len(),
        patch_count_before_undo,
        "undoing ViewportDrop must not emit phantom reverse AstPatch"
    );

    // Flushing Radar again must still produce zero patches
    running.ctx.radar.flush_viewport(&vp1.id).await;
    let vp_after_flush = running.ctx.db.get_viewport(&vp1.id).unwrap().unwrap();
    assert!(vp_after_flush.patch_ids.is_empty());

    // Immediate seal_capsule on the clean unmodified viewport must contain ZERO phantom files, symbols, or patches
    let noop_cap = match send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp1.id.clone(),
            verify_cmd: None,
            summary: Some("noop seal after undo".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::CapsuleSeal(c) => c,
        other => panic!("unexpected seal payload: {other:?}"),
    };
    assert!(
        noop_cap.touched_files.is_empty(),
        "immediate seal after drop->undo must not attribute any files, got {:?}",
        noop_cap.touched_files
    );
    assert!(
        noop_cap.touched_symbols.is_empty(),
        "immediate seal after drop->undo must not attribute dead-end symbols, got {:?}",
        noop_cap.touched_symbols
    );
    assert!(
        noop_cap.patch_ids.is_empty(),
        "immediate seal after drop->undo must not attach phantom patches, got {:?}",
        noop_cap.patch_ids
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("src/lib.rs")).unwrap(),
        "pub fn base() -> i32 { 1 }\n"
    );

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn undo_drop_and_edit_unrelated_file_seals_without_deadend_provenance_pollution() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn base() -> i32 { 1 }\n").unwrap();

    let (running, vp1) = init_and_spawn(repo, false, "vp-drop-undo-b", "case B").await;

    // Case B: modify src/lib.rs -> Radar flush -> drop_viewport -> undo_op -> edit src/other.rs -> seal
    write_vp_file_atomic(&vp1.path, "src/lib.rs", "pub fn bad_idea() -> i32 { 99 }\n");
    running.ctx.radar.flush_viewport(&vp1.id).await;

    send_rpc(
        repo,
        RpcMethod::DropViewport {
            viewport_id: vp1.id.clone(),
            reason: "abandoned lib edit".into(),
            kind: Some("DesignDeadEnd".into()),
        },
    )
    .await
    .unwrap();

    send_rpc(repo, RpcMethod::Undo { op_seq: None })
        .await
        .unwrap();

    write_vp_file_atomic(&vp1.path, "src/other.rs", "pub fn other() -> i32 { 7 }\n");
    running.ctx.radar.flush_viewport(&vp1.id).await;

    let capsule = match send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp1.id.clone(),
            verify_cmd: Some("echo ok".into()),
            summary: Some("valid other edit".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::CapsuleSeal(c) => c,
        other => panic!("unexpected seal payload: {other:?}"),
    };

    assert_eq!(
        capsule.touched_files,
        vec![std::path::PathBuf::from("src/other.rs")],
        "touched_files must contain ONLY src/other.rs, got {:?}",
        capsule.touched_files
    );
    assert_eq!(
        capsule.touched_symbols,
        vec!["other".to_string()],
        "touched_symbols must contain ONLY other, got {:?}",
        capsule.touched_symbols
    );
    assert_eq!(
        capsule.patch_ids.len(),
        1,
        "capsule.patch_ids must contain only the src/other.rs patch, got {:?}",
        capsule.patch_ids
    );
    let rec = running
        .ctx
        .db
        .get_patch(&capsule.patch_ids[0])
        .unwrap()
        .unwrap();
    assert_eq!(
        rec.patch.file_path,
        std::path::PathBuf::from("src/other.rs")
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("src/lib.rs")).unwrap(),
        "pub fn base() -> i32 { 1 }\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("src/other.rs")).unwrap(),
        "pub fn other() -> i32 { 7 }\n"
    );

    // Undoing the post-recovery seal must also unwind trunk cleanly
    send_rpc(repo, RpcMethod::Undo { op_seq: None })
        .await
        .unwrap();
    assert!(!repo.join("src/other.rs").exists());

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn undo_record_deadend_restore_clean_vs_preserve_dirty_edits() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn base() -> i32 { 1 }\n").unwrap();

    let (running, vp1) = init_and_spawn(repo, false, "vp-deadend-undo", "case C").await;

    // Part 1 (restore_clean=true): modify src/lib.rs -> flush -> record_deadend(true) -> undo -> edit src/other.rs -> seal
    write_vp_file_atomic(&vp1.path, "src/lib.rs", "pub fn dead_two() -> i32 { 55 }\n");
    running.ctx.radar.flush_viewport(&vp1.id).await;

    send_rpc(
        repo,
        RpcMethod::McpRecordDeadendEx {
            viewport_id: vp1.id.clone(),
            touched_symbols: vec![],
            kind: "DesignDeadEnd".into(),
            lesson_summary: "dead two".into(),
            restore_clean: true,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(vp1.path.join("src/lib.rs")).unwrap(),
        "pub fn base() -> i32 { 1 }\n"
    );

    send_rpc(repo, RpcMethod::Undo { op_seq: None })
        .await
        .unwrap();

    let vp1_after_undo = running.ctx.db.get_viewport(&vp1.id).unwrap().unwrap();
    assert_eq!(vp1_after_undo.state, ViewportState::Active);
    assert!(
        vp1_after_undo.patch_ids.is_empty(),
        "undoing restore_clean=true deadend must clear stale patch_ids: {:?}",
        vp1_after_undo.patch_ids
    );
    assert!(vp1_after_undo.head_cas.is_none());

    write_vp_file_atomic(&vp1.path, "src/other.rs", "pub fn other() -> i32 { 8 }\n");
    running.ctx.radar.flush_viewport(&vp1.id).await;

    let cap1 = match send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp1.id.clone(),
            verify_cmd: None,
            summary: Some("clean seal after deadend undo".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::CapsuleSeal(c) => c,
        other => panic!("unexpected seal payload: {other:?}"),
    };
    assert_eq!(cap1.touched_files, vec![std::path::PathBuf::from("src/other.rs")]);
    assert_eq!(cap1.touched_symbols, vec!["other".to_string()]);
    assert_eq!(cap1.patch_ids.len(), 1);
    let cap1_patch = running.ctx.db.get_patch(&cap1.patch_ids[0]).unwrap().unwrap();
    assert_eq!(
        cap1_patch.patch.file_path,
        std::path::PathBuf::from("src/other.rs")
    );

    // Part 2 (contrast restore_clean=false): dirty edits and patch_ids are preserved across record_deadend & undo
    let vp2 = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "vp-keep-dirty".into(),
            intent: "contrast restore_clean=false".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn payload: {other:?}"),
    };
    write_vp_file_atomic(&vp2.path, "src/lib.rs", "pub fn kept_fn() -> i32 { 77 }\n");
    running.ctx.radar.flush_viewport(&vp2.id).await;
    let patches_before_note = running
        .ctx
        .db
        .get_viewport(&vp2.id)
        .unwrap()
        .unwrap()
        .patch_ids
        .clone();
    assert!(!patches_before_note.is_empty());

    send_rpc(
        repo,
        RpcMethod::McpRecordDeadend {
            viewport_id: vp2.id.clone(),
            touched_symbols: vec!["kept_fn".into()],
            kind: "Other".into(),
            lesson_summary: "note without clean restore".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(vp2.path.join("src/lib.rs")).unwrap(),
        "pub fn kept_fn() -> i32 { 77 }\n"
    );

    send_rpc(repo, RpcMethod::Undo { op_seq: None })
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(vp2.path.join("src/lib.rs")).unwrap(),
        "pub fn kept_fn() -> i32 { 77 }\n"
    );
    assert_eq!(
        running.ctx.db.get_viewport(&vp2.id).unwrap().unwrap().patch_ids,
        patches_before_note
    );

    let cap2 = match send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp2.id.clone(),
            verify_cmd: None,
            summary: Some("seal preserved dirty edits".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::CapsuleSeal(c) => c,
        other => panic!("unexpected seal payload: {other:?}"),
    };
    assert_eq!(cap2.touched_files, vec![std::path::PathBuf::from("src/lib.rs")]);
    assert!(cap2.touched_symbols.iter().any(|s| s == "kept_fn"));
    assert_eq!(
        std::fs::read_to_string(repo.join("src/lib.rs")).unwrap(),
        "pub fn kept_fn() -> i32 { 77 }\n"
    );

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seal_excludes_manually_reverted_file_provenance() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn base() -> i32 { 1 }\n").unwrap();

    let (running, vp) = init_and_spawn(repo, false, "vp-manual-revert", "filter reverted").await;

    // Edit src/lib.rs, flush to WAL, then manually revert src/lib.rs back to base
    write_vp_file_atomic(&vp.path, "src/lib.rs", "pub fn reverted_fn() -> i32 { 99 }\n");
    running.ctx.radar.flush_viewport(&vp.id).await;
    write_vp_file_atomic(&vp.path, "src/lib.rs", "pub fn base() -> i32 { 1 }\n");
    running.ctx.radar.flush_viewport(&vp.id).await;

    // Edit unrelated src/other.rs and seal
    write_vp_file_atomic(&vp.path, "src/other.rs", "pub fn other() -> i32 { 3 }\n");
    running.ctx.radar.flush_viewport(&vp.id).await;

    let cap = match send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp.id.clone(),
            verify_cmd: None,
            summary: Some("only other.rs changed".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::CapsuleSeal(c) => c,
        other => panic!("unexpected seal payload: {other:?}"),
    };
    assert_eq!(cap.touched_files, vec![std::path::PathBuf::from("src/other.rs")]);
    assert_eq!(cap.touched_symbols, vec!["other".to_string()]);
    assert_eq!(cap.patch_ids.len(), 1);

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn undo_restore_viewport_clean_aligns_db_disk_radar_and_prevents_resurrection() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn base() -> i32 { 1 }\n").unwrap();

    let (running, vp1) = init_and_spawn(repo, false, "vp1", "restore clean undo alignment").await;
    let s1 = running
        .ctx
        .db
        .get_viewport(&vp1.id)
        .unwrap()
        .unwrap()
        .base_snapshot_cas
        .expect("initial base_snapshot_cas S1 must exist");

    // Peer adds src/peer_feature.rs and seals -> trunk advances to S2.
    let vp_peer = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "vp-peer".into(),
            intent: "add peer feature".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn payload: {other:?}"),
    };
    write_vp_file_atomic(
        &vp_peer.path,
        "src/peer_feature.rs",
        "pub fn peer_fn() -> i32 { 42 }\n",
    );
    running.ctx.radar.flush_viewport(&vp_peer.id).await;
    send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp_peer.id.clone(),
            verify_cmd: None,
            summary: Some("add peer_feature".into()),
        },
    )
    .await
    .unwrap();

    // vp1 modifies src/lib.rs, flushes, and runs RestoreViewportClean -> disk/DB/Radar move to S2.
    write_vp_file_atomic(&vp1.path, "src/lib.rs", "pub fn base() -> i32 { 99 }\n");
    running.ctx.radar.flush_viewport(&vp1.id).await;
    send_rpc(
        repo,
        RpcMethod::RestoreViewportClean {
            viewport_id: vp1.id.clone(),
        },
    )
    .await
    .unwrap();
    assert!(vp1.path.join("src/peer_feature.rs").exists());
    let s2 = running
        .ctx
        .db
        .get_viewport(&vp1.id)
        .unwrap()
        .unwrap()
        .base_snapshot_cas
        .expect("S2 must exist");
    assert_ne!(s1, s2);

    // Undo RestoreViewportClean -> DB, Disk, and Radar must ALL align back to S1.
    send_rpc(repo, RpcMethod::Undo { op_seq: None })
        .await
        .unwrap();

    let vp1_after_undo = running.ctx.db.get_viewport(&vp1.id).unwrap().unwrap();
    assert_eq!(vp1_after_undo.base_snapshot_cas, Some(s1));
    assert!(vp1_after_undo.patch_ids.is_empty());
    assert!(vp1_after_undo.head_cas.is_none());
    assert!(
        !vp1.path.join("src/peer_feature.rs").exists(),
        "undoing RestoreViewportClean must remove S2-only src/peer_feature.rs from vp1 disk"
    );
    assert_eq!(
        std::fs::read_to_string(vp1.path.join("src/lib.rs")).unwrap(),
        "pub fn base() -> i32 { 1 }\n"
    );

    let snap_after_undo =
        match send_rpc(repo, RpcMethod::RadarSnapshot { viewport_id: None })
            .await
            .unwrap()
        {
        RpcPayload::RadarSnapshot(s) => s,
        other => panic!("unexpected radar payload: {other:?}"),
    };
    let vp1_radar = snap_after_undo
        .viewports
        .iter()
        .find(|v| v.viewport_id == vp1.id)
        .expect("vp1 must remain registered in radar");
    assert!(
        vp1_radar.dirty_files.is_empty(),
        "radar dirty_files must be empty after RestoreViewportClean undo: {:?}",
        vp1_radar.dirty_files
    );

    // A deleter viewport removes src/peer_feature.rs from trunk.
    let vp_deleter = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "vp-deleter".into(),
            intent: "delete peer feature".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn payload: {other:?}"),
    };
    std::fs::remove_file(vp_deleter.path.join("src/peer_feature.rs")).unwrap();
    running.ctx.radar.flush_viewport(&vp_deleter.id).await;
    send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp_deleter.id.clone(),
            verify_cmd: None,
            summary: Some("delete peer_feature".into()),
        },
    )
    .await
    .unwrap();
    assert!(!repo.join("src/peer_feature.rs").exists());

    // vp1 creates only src/unrelated.rs and seals -> must NOT resurrect src/peer_feature.rs or misattribute peer_fn.
    write_vp_file_atomic(
        &vp1.path,
        "src/unrelated.rs",
        "pub fn unrelated() -> i32 { 7 }\n",
    );
    running.ctx.radar.flush_viewport(&vp1.id).await;

    let cap = match send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp1.id.clone(),
            verify_cmd: None,
            summary: Some("seal unrelated only".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::CapsuleSeal(c) => c,
        other => panic!("unexpected seal payload: {other:?}"),
    };
    assert_eq!(
        cap.touched_files,
        vec![std::path::PathBuf::from("src/unrelated.rs")],
        "capsule must not misattribute peer_feature.rs: {:?}",
        cap.touched_files
    );
    assert_eq!(
        cap.touched_symbols,
        vec!["unrelated".to_string()],
        "capsule must not misattribute peer_fn: {:?}",
        cap.touched_symbols
    );
    assert!(
        !repo.join("src/peer_feature.rs").exists(),
        "sealing vp1 after RestoreViewportClean undo must NOT resurrect deleted trunk file"
    );
    assert!(repo.join("src/unrelated.rs").exists());

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sequential_seal_capsule_preserves_added_go_and_python_imports() {
    use loom_core::cst::parse_source;
    use loom_core::model::LanguageKind;

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("app")).unwrap();
    std::fs::create_dir_all(repo.join("pkg")).unwrap();
    std::fs::write(repo.join("app/main.py"), "def run():\n    return 1\n").unwrap();
    std::fs::write(
        repo.join("pkg/main.go"),
        "package pkg\n\nfunc Run() int {\n\treturn 1\n}\n",
    )
    .unwrap();

    let StartDaemonOutcome::Started(running) = start_daemon(repo, Some(60_000)).await.unwrap()
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
            intent: "edit run".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn: {other:?}"),
    };
    let vp2 = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "vp2".into(),
            intent: "add imports and callers".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn: {other:?}"),
    };

    // vp1 modifies run() / Run() and seals first
    write_vp_file_atomic(&vp1.path, "app/main.py", "def run():\n    return 10\n");
    write_vp_file_atomic(
        &vp1.path,
        "pkg/main.go",
        "package pkg\n\nfunc Run() int {\n\treturn 10\n}\n",
    );
    running.ctx.radar.flush_viewport(&vp1.id).await;
    send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp1.id.clone(),
            verify_cmd: None,
            summary: Some("update run".into()),
        },
    )
    .await
    .unwrap();

    // vp2 adds imports + new functions and seals against updated trunk
    write_vp_file_atomic(
        &vp2.path,
        "app/main.py",
        "import os\n\ndef run():\n    return 1\n\ndef env():\n    return os.getcwd()\n",
    );
    write_vp_file_atomic(
        &vp2.path,
        "pkg/main.go",
        "package pkg\n\nimport \"fmt\"\n\nfunc Run() int {\n\treturn 1\n}\n\nfunc Print() {\n\tfmt.Println(1)\n}\n",
    );
    running.ctx.radar.flush_viewport(&vp2.id).await;
    send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp2.id.clone(),
            verify_cmd: None,
            summary: Some("add imports and helpers".into()),
        },
    )
    .await
    .unwrap();

    let py_trunk = std::fs::read_to_string(repo.join("app/main.py")).unwrap();
    let go_trunk = std::fs::read_to_string(repo.join("pkg/main.go")).unwrap();
    assert!(
        py_trunk.contains("import os") && py_trunk.contains("return 10") && py_trunk.contains("def env():"),
        "trunk app/main.py dropped import or edit:\n{py_trunk}"
    );
    assert!(
        go_trunk.starts_with("package pkg")
            && go_trunk.contains("import \"fmt\"")
            && go_trunk.contains("return 10")
            && go_trunk.contains("func Print()"),
        "trunk pkg/main.go dropped import or edit:\n{go_trunk}"
    );
    assert!(
        !parse_source(LanguageKind::Python, &py_trunk, None)
            .unwrap()
            .tree
            .root_node()
            .has_error(),
        "trunk app/main.py has syntax error:\n{py_trunk}"
    );
    assert!(
        !parse_source(LanguageKind::Go, &go_trunk, None)
            .unwrap()
            .tree
            .root_node()
            .has_error(),
        "trunk pkg/main.go has syntax error:\n{go_trunk}"
    );

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}


#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn undo_after_undone_viewport_drop_blocks_pre_clean_rewind_and_preserves_attribution() {
    let tmp = TempDir::new().unwrap();
    let bare = tmp.path().join("bare.git");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    let _bare = gix::init_bare(&bare).unwrap();
    init_git_with_initial_commit(&repo, &[("src/base.rs", "pub fn base_fn() -> i32 { 1 }\n")]);

    let (running, vp1) = init_and_spawn(&repo, true, "vp1", "vp1 drop barrier").await;
    let s1 = running
        .ctx
        .db
        .get_viewport(&vp1.id)
        .unwrap()
        .unwrap()
        .base_snapshot_cas
        .expect("S1 must exist");

    // Advance trunk via peer vp2 to S2 and push so vp2's CapsuleSeal is pushed
    let vp2 = match send_rpc(
        &repo,
        RpcMethod::SpawnViewport {
            name: "vp2".into(),
            intent: "add peer".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn: {other:?}"),
    };
    write_vp_file_atomic(&vp2.path, "src/peer.rs", "pub fn peer_fn() -> i32 { 2 }\n");
    running.ctx.radar.flush_viewport(&vp2.id).await;
    send_rpc(
        &repo,
        RpcMethod::SealCapsule {
            viewport_id: vp2.id.clone(),
            verify_cmd: None,
            summary: Some("peer seal".into()),
        },
    )
    .await
    .unwrap();
    send_rpc(
        &repo,
        RpcMethod::GitPush {
            remote: Some(bare.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap();

    // Record >=2 pre-clean AstPatches on vp1
    write_vp_file_atomic(&vp1.path, "src/base.rs", "pub fn base_fn() -> i32 { 10 }\n");
    running.ctx.radar.flush_viewport(&vp1.id).await;
    write_vp_file_atomic(&vp1.path, "src/extra.rs", "pub fn extra_fn() -> i32 { 20 }\n");
    running.ctx.radar.flush_viewport(&vp1.id).await;

    let pre_clean_patch_seqs: Vec<u64> = running
        .ctx
        .db
        .list_ops(false, None, None)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == OpKind::AstPatch && e.viewport_id.as_deref() == Some(vp1.id.as_str()))
        .map(|e| e.op_seq)
        .collect();
    assert!(pre_clean_patch_seqs.len() >= 2);
    let latest_pre_clean_patch_seq = pre_clean_patch_seqs[0];

    let pre_clean_trans_seq = running
        .ctx
        .db
        .list_ops(false, None, None)
        .unwrap()
        .into_iter()
        .find(|e| {
            e.kind == OpKind::ViewportTransition
                && e.viewport_id.as_deref() == Some(vp1.id.as_str())
        })
        .expect("Activate ViewportTransition must exist")
        .op_seq;

    // Drop vp1 (wipes disk to S2) -> Undo #1 (restores Active at S2)
    send_rpc(
        &repo,
        RpcMethod::DropViewport {
            viewport_id: vp1.id.clone(),
            reason: "dead end".into(),
            kind: Some("DesignDeadEnd".into()),
        },
    )
    .await
    .unwrap();
    let s2 = running
        .ctx
        .db
        .get_viewport(&vp1.id)
        .unwrap()
        .unwrap()
        .base_snapshot_cas
        .expect("S2 must exist");
    assert_ne!(s1, s2);

    send_rpc(&repo, RpcMethod::Undo { op_seq: None }).await.unwrap();

    // Undo #2: both explicit (`--op <pre_clean_patch_seq>` / `--op <pre_clean_trans_seq>`) and `op_seq: None`
    // must NOT rewind base_snapshot_cas or patch_ids
    let err_patch = send_rpc(
        &repo,
        RpcMethod::Undo {
            op_seq: Some(latest_pre_clean_patch_seq),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err_patch.0, RpcErrorCode::Conflict);

    let err_trans = send_rpc(
        &repo,
        RpcMethod::Undo {
            op_seq: Some(pre_clean_trans_seq),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err_trans.0, RpcErrorCode::Conflict);

    let err_latest = send_rpc(&repo, RpcMethod::Undo { op_seq: None })
        .await
        .unwrap_err();
    assert_eq!(err_latest.0, RpcErrorCode::NotFound);

    let vp1_now = running.ctx.db.get_viewport(&vp1.id).unwrap().unwrap();
    assert_eq!(vp1_now.base_snapshot_cas, Some(s2));
    assert!(
        vp1_now.patch_ids.is_empty(),
        "patch_ids must remain empty: {:?}",
        vp1_now.patch_ids
    );

    // Edit only src/vp1_only.rs and seal -> must attribute ONLY src/vp1_only.rs (no peer.rs/peer_fn)
    write_vp_file_atomic(&vp1.path, "src/vp1_only.rs", "pub fn vp1_only() -> i32 { 7 }\n");
    running.ctx.radar.flush_viewport(&vp1.id).await;
    let cap = match send_rpc(
        &repo,
        RpcMethod::SealCapsule {
            viewport_id: vp1.id.clone(),
            verify_cmd: None,
            summary: Some("vp1 only".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::CapsuleSeal(c) => c,
        other => panic!("unexpected seal: {other:?}"),
    };
    assert_eq!(
        cap.touched_files,
        vec![std::path::PathBuf::from("src/vp1_only.rs")]
    );
    assert_eq!(cap.touched_symbols, vec!["vp1_only".to_string()]);

    send_rpc(&repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn undo_after_undone_deadend_restore_clean_prevents_base_rewind_and_trunk_resurrection() {
    let tmp = TempDir::new().unwrap();
    let bare = tmp.path().join("bare.git");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    let _bare = gix::init_bare(&bare).unwrap();
    init_git_with_initial_commit(&repo, &[("src/base.rs", "pub fn base_fn() -> i32 { 1 }\n")]);

    let (running, vp1) = init_and_spawn(&repo, true, "vp1", "deadend barrier").await;
    let vp2 = match send_rpc(
        &repo,
        RpcMethod::SpawnViewport {
            name: "vp2".into(),
            intent: "peer".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn: {other:?}"),
    };
    write_vp_file_atomic(&vp2.path, "src/peer.rs", "pub fn peer_fn() -> i32 { 2 }\n");
    running.ctx.radar.flush_viewport(&vp2.id).await;
    send_rpc(
        &repo,
        RpcMethod::SealCapsule {
            viewport_id: vp2.id.clone(),
            verify_cmd: None,
            summary: Some("peer seal".into()),
        },
    )
    .await
    .unwrap();
    send_rpc(
        &repo,
        RpcMethod::GitPush {
            remote: Some(bare.to_string_lossy().into_owned()),
            branch: Some("main".into()),
        },
    )
    .await
    .unwrap();

    write_vp_file_atomic(&vp1.path, "src/base.rs", "pub fn base_fn() -> i32 { 11 }\n");
    running.ctx.radar.flush_viewport(&vp1.id).await;
    let patch_seq = running
        .ctx
        .db
        .list_ops(false, None, None)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == OpKind::AstPatch && e.viewport_id.as_deref() == Some(vp1.id.as_str()))
        .unwrap()
        .op_seq;

    send_rpc(
        &repo,
        RpcMethod::McpRecordDeadendEx {
            viewport_id: vp1.id.clone(),
            touched_symbols: vec![],
            kind: "DesignDeadEnd".into(),
            lesson_summary: "deadend clean".into(),
            restore_clean: true,
        },
    )
    .await
    .unwrap();
    let s2 = running
        .ctx
        .db
        .get_viewport(&vp1.id)
        .unwrap()
        .unwrap()
        .base_snapshot_cas
        .unwrap();

    // Undo #1: undoes NegativeEdgeRecord(restore_clean=true)
    send_rpc(&repo, RpcMethod::Undo { op_seq: None }).await.unwrap();

    // Undo #2 (explicit & latest): must NOT rewind vp1.base_snapshot_cas
    let err_explicit = send_rpc(
        &repo,
        RpcMethod::Undo {
            op_seq: Some(patch_seq),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err_explicit.0, RpcErrorCode::Conflict);

    let err_latest = send_rpc(&repo, RpcMethod::Undo { op_seq: None })
        .await
        .unwrap_err();
    assert_eq!(err_latest.0, RpcErrorCode::NotFound);

    let vp1_now = running.ctx.db.get_viewport(&vp1.id).unwrap().unwrap();
    assert_eq!(vp1_now.base_snapshot_cas, Some(s2));
    assert!(vp1_now.patch_ids.is_empty());

    // Delete src/peer.rs from trunk via vp3 and seal
    let vp3 = match send_rpc(
        &repo,
        RpcMethod::SpawnViewport {
            name: "vp3".into(),
            intent: "delete peer".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn: {other:?}"),
    };
    std::fs::remove_file(vp3.path.join("src/peer.rs")).unwrap();
    running.ctx.radar.flush_viewport(&vp3.id).await;
    send_rpc(
        &repo,
        RpcMethod::SealCapsule {
            viewport_id: vp3.id.clone(),
            verify_cmd: None,
            summary: Some("remove peer".into()),
        },
    )
    .await
    .unwrap();
    assert!(!repo.join("src/peer.rs").exists());

    // Seal vp1 with only src/vp1_only.rs -> must NOT resurrect deleted src/peer.rs onto trunk
    write_vp_file_atomic(&vp1.path, "src/vp1_only.rs", "pub fn vp1_only() -> i32 { 8 }\n");
    running.ctx.radar.flush_viewport(&vp1.id).await;
    let cap1 = match send_rpc(
        &repo,
        RpcMethod::SealCapsule {
            viewport_id: vp1.id.clone(),
            verify_cmd: None,
            summary: Some("seal vp1_only".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::CapsuleSeal(c) => c,
        other => panic!("unexpected seal: {other:?}"),
    };
    assert_eq!(
        cap1.touched_files,
        vec![std::path::PathBuf::from("src/vp1_only.rs")]
    );
    assert!(
        !repo.join("src/peer.rs").exists(),
        "sealing vp1 must NOT resurrect deleted trunk file src/peer.rs"
    );

    send_rpc(&repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn undo_after_undone_restore_clean_blocks_patch_ids_rewind_and_negative_edge_pollution() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/a.rs"), "pub fn fn_a() -> i32 { 1 }\n").unwrap();
    std::fs::write(repo.join("src/b.rs"), "pub fn fn_b() -> i32 { 2 }\n").unwrap();

    let (running, vp1) = init_and_spawn(repo, false, "vp1", "restore clean multi-patch").await;
    let s1 = running
        .ctx
        .db
        .get_viewport(&vp1.id)
        .unwrap()
        .unwrap()
        .base_snapshot_cas
        .expect("S1 must exist");

    // Record >= 2 pre-clean patches in vp1
    write_vp_file_atomic(&vp1.path, "src/a.rs", "pub fn fn_a() -> i32 { 10 }\n");
    running.ctx.radar.flush_viewport(&vp1.id).await;
    write_vp_file_atomic(&vp1.path, "src/b.rs", "pub fn fn_b() -> i32 { 20 }\n");
    running.ctx.radar.flush_viewport(&vp1.id).await;

    let patch_seqs: Vec<u64> = running
        .ctx
        .db
        .list_ops(false, None, None)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == OpKind::AstPatch && e.viewport_id.as_deref() == Some(vp1.id.as_str()))
        .map(|e| e.op_seq)
        .collect();
    assert!(patch_seqs.len() >= 2);
    let latest_pre_clean_patch_seq = patch_seqs[0];

    // RestoreViewportClean -> Undo #1 (rolls back to clean S1 with empty patch_ids)
    send_rpc(
        repo,
        RpcMethod::RestoreViewportClean {
            viewport_id: vp1.id.clone(),
        },
    )
    .await
    .unwrap();
    send_rpc(repo, RpcMethod::Undo { op_seq: None }).await.unwrap();

    let vp1_after_undo1 = running.ctx.db.get_viewport(&vp1.id).unwrap().unwrap();
    assert_eq!(vp1_after_undo1.base_snapshot_cas, Some(s1));
    assert!(vp1_after_undo1.patch_ids.is_empty());

    // Undo #2 (explicit and latest) must NOT rewind patch_ids
    let err_explicit = send_rpc(
        repo,
        RpcMethod::Undo {
            op_seq: Some(latest_pre_clean_patch_seq),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err_explicit.0, RpcErrorCode::Conflict);

    let err_latest = send_rpc(repo, RpcMethod::Undo { op_seq: None })
        .await
        .unwrap_err();
    assert_eq!(err_latest.0, RpcErrorCode::NotFound);

    let vp1_after_undo2 = running.ctx.db.get_viewport(&vp1.id).unwrap().unwrap();
    assert!(
        vp1_after_undo2.patch_ids.is_empty(),
        "patch_ids must remain empty after RestoreViewportClean->Undo->Undo: {:?}",
        vp1_after_undo2.patch_ids
    );

    // Edit only src/c.rs and drop -> NegativeEdge must contain ONLY fn_c (no phantom fn_a/fn_b)
    write_vp_file_atomic(&vp1.path, "src/c.rs", "pub fn fn_c() -> i32 { 3 }\n");
    running.ctx.radar.flush_viewport(&vp1.id).await;
    let drop_res = match send_rpc(
        repo,
        RpcMethod::DropViewport {
            viewport_id: vp1.id.clone(),
            reason: "only fn_c failed".into(),
            kind: Some("DesignDeadEnd".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Drop(d) => d,
        other => panic!("unexpected drop: {other:?}"),
    };
    assert_eq!(
        drop_res.negative_edge.touched_symbols,
        vec!["fn_c".to_string()]
    );

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn non_destructive_deadend_allows_pre_patch_undo_and_clean_barrier_does_not_wedge_peer_or_trunk() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/a.rs"), "pub fn fn_a() -> i32 { 1 }\n").unwrap();
    std::fs::write(repo.join("src/b.rs"), "pub fn fn_b() -> i32 { 2 }\n").unwrap();
    std::fs::write(repo.join("src/t.rs"), "pub fn fn_t() -> i32 { 3 }\n").unwrap();

    // Part 1: McpRecordDeadendEx(restore_clean=false) -> Undo still allows undoing pre-deadend AstPatch
    let (running, vp1) = init_and_spawn(repo, false, "vp1", "non-destructive deadend").await;
    write_vp_file_atomic(&vp1.path, "src/a.rs", "pub fn fn_a() -> i32 { 11 }\n");
    running.ctx.radar.flush_viewport(&vp1.id).await;
    let vp1_patch_seq = running
        .ctx
        .db
        .list_ops(false, None, None)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == OpKind::AstPatch && e.viewport_id.as_deref() == Some(vp1.id.as_str()))
        .unwrap()
        .op_seq;

    send_rpc(
        repo,
        RpcMethod::McpRecordDeadendEx {
            viewport_id: vp1.id.clone(),
            touched_symbols: vec!["fn_a".into()],
            kind: "Other".into(),
            lesson_summary: "keep dirty".into(),
            restore_clean: false,
        },
    )
    .await
    .unwrap();

    // Undo #1 undoes NegativeEdgeRecord(restore_clean=false)
    send_rpc(repo, RpcMethod::Undo { op_seq: None }).await.unwrap();
    // Undo #2 MUST still undo the pre-deadend AstPatch!
    let undo2 = match send_rpc(repo, RpcMethod::Undo { op_seq: None }).await.unwrap() {
        RpcPayload::Undo(u) => u,
        other => panic!("unexpected undo: {other:?}"),
    };
    assert_eq!(undo2.undone_op_seq, vp1_patch_seq);

    // Part 2: Seal a trunk capsule via vp0, then clean vp2 via DropViewport + Undo.
    // Subsequent Undo(None) calls must skip vp2's pre-clean ops without wedging,
    // reaching both trunk CapsuleSeal (vp0) AND peer vp1's ViewportTransition/ViewportSpawn!
    let vp0 = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "vp0".into(),
            intent: "trunk seal".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn: {other:?}"),
    };
    write_vp_file_atomic(&vp0.path, "src/t.rs", "pub fn fn_t() -> i32 { 33 }\n");
    running.ctx.radar.flush_viewport(&vp0.id).await;
    send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp0.id.clone(),
            verify_cmd: None,
            summary: Some("trunk seal".into()),
        },
    )
    .await
    .unwrap();

    let vp2 = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "vp2".into(),
            intent: "vp2 cleaned".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("unexpected spawn: {other:?}"),
    };
    write_vp_file_atomic(&vp2.path, "src/b.rs", "pub fn fn_b() -> i32 { 22 }\n");
    running.ctx.radar.flush_viewport(&vp2.id).await;
    send_rpc(
        repo,
        RpcMethod::DropViewport {
            viewport_id: vp2.id.clone(),
            reason: "drop vp2".into(),
            kind: Some("DesignDeadEnd".into()),
        },
    )
    .await
    .unwrap();
    // Undo DropViewport on vp2 -> vp2 now has an undone destructive-clean barrier
    send_rpc(repo, RpcMethod::Undo { op_seq: None }).await.unwrap();

    // Next Undo(None) must skip vp2's pre-clean ops and reach vp0's trunk CapsuleSeal!
    let seal_undo = match send_rpc(repo, RpcMethod::Undo { op_seq: None }).await.unwrap() {
        RpcPayload::Undo(u) => u,
        other => panic!("unexpected undo: {other:?}"),
    };
    let seal_op = running
        .ctx
        .db
        .get_op(seal_undo.undone_op_seq)
        .unwrap()
        .unwrap();
    assert_eq!(seal_op.kind, OpKind::CapsuleSeal);
    assert_eq!(
        std::fs::read_to_string(repo.join("src/t.rs")).unwrap(),
        "pub fn fn_t() -> i32 { 3 }\n"
    );

    // Next Undo(None) calls must skip vp0's pre-seal (Dropped) & vp2's pre-clean ops,
    // cleanly unwinding vp1's ViewportTransition and ViewportSpawn!
    let trans_undo = match send_rpc(repo, RpcMethod::Undo { op_seq: None }).await.unwrap() {
        RpcPayload::Undo(u) => u,
        other => panic!("unexpected undo: {other:?}"),
    };
    let trans_op = running
        .ctx
        .db
        .get_op(trans_undo.undone_op_seq)
        .unwrap()
        .unwrap();
    assert_eq!(trans_op.kind, OpKind::ViewportTransition);
    assert_eq!(trans_op.viewport_id.as_deref(), Some(vp1.id.as_str()));

    let spawn_undo = match send_rpc(repo, RpcMethod::Undo { op_seq: None }).await.unwrap() {
        RpcPayload::Undo(u) => u,
        other => panic!("unexpected undo: {other:?}"),
    };
    let spawn_op = running
        .ctx
        .db
        .get_op(spawn_undo.undone_op_seq)
        .unwrap()
        .unwrap();
    assert_eq!(spawn_op.kind, OpKind::ViewportSpawn);
    assert_eq!(spawn_op.viewport_id.as_deref(), Some(vp1.id.as_str()));
    assert!(running.ctx.db.get_viewport(&vp1.id).unwrap().is_none());

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}
