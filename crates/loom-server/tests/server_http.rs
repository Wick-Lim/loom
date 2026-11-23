use loom_core::cas::CasStore;
use loom_core::model::{
    Blake3Hash, CausalCapsule, FailureKind, NegativeEdge, WorkspaceSnapshot,
};
use loom_core::remote::{
    CasHasRequest, CasHasResponse, RemotePullRequest, RemotePullResponse, RemotePushRequest,
    RemotePushResponse, RemoteRepoInfo,
};
use loom_server::{ServerConfig, start_server};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use tempfile::TempDir;

#[tokio::test]
async fn server_rejects_missing_tokens_without_no_auth() {
    let dir = TempDir::new().unwrap();
    let cfg = ServerConfig {
        bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        data_dir: dir.path().to_path_buf(),
        tokens: Vec::new(),
        allow_unauthenticated: false,
        ..ServerConfig::default()
    };
    assert!(start_server(cfg).await.is_err());
}

#[tokio::test]
async fn server_auth_cas_dedup_push_conflict_and_pull_roundtrip() {
    let server_dir = TempDir::new().unwrap();
    let handle = start_server(ServerConfig {
        bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        data_dir: server_dir.path().to_path_buf(),
        tokens: vec!["top-secret-token".to_string()],
        allow_unauthenticated: false,
        allow_auto_create: true,
        ..ServerConfig::default()
    })
    .await
    .unwrap();

    let base = format!("http://{}", handle.local_addr);
    let client = reqwest::Client::new();

    // Healthz is open without auth
    let health = client.get(format!("{base}/healthz")).send().await.unwrap();
    assert_eq!(health.status(), 200);

    // /v1 routes reject missing or wrong Bearer token
    let unauth = client
        .get(format!("{base}/v1/repos/team/loom/info"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status(), 401);
    let bad_auth = client
        .get(format!("{base}/v1/repos/team/loom/info"))
        .bearer_auth("wrong-token")
        .send()
        .await
        .unwrap();
    assert_eq!(bad_auth.status(), 401);

    // Authorized info reports exists=false before first push
    let info0: RemoteRepoInfo = client
        .get(format!("{base}/v1/repos/team/loom/info"))
        .bearer_auth("top-secret-token")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!info0.exists);

    // Prepare local CAS blobs (file + snapshot manifest)
    let local_dir = TempDir::new().unwrap();
    let local_cas = CasStore::open_default(local_dir.path().join("cas")).unwrap();
    let file_bytes = b"pub fn hello() -> i32 { 42 }\n";
    let file_hash = local_cas.put_bytes(file_bytes).unwrap();
    let mut files = BTreeMap::new();
    files.insert("src/lib.rs".to_string(), file_hash);
    let snap = WorkspaceSnapshot { files };
    let snap_bytes = rmp_serde::to_vec_named(&snap).unwrap();
    let snap_hash = local_cas.put_bytes(&snap_bytes).unwrap();

    let cap1 = CausalCapsule {
        capsule_id: "cap-1".into(),
        viewport_id: "vp-1".into(),
        agent_id: "agy".into(),
        summary: "initial native capsule".into(),
        snapshot_cas_hash: Some(snap_hash),
        created_at_ns: 100,
        ..Default::default()
    };
    let edge1 = NegativeEdge {
        edge_id: "ne-1".into(),
        edge_seq: 1,
        viewport_id: "vp-1".into(),
        touched_symbols: vec!["crate::hello".into()],
        failure_kind: FailureKind::Deadlock,
        lesson_summary: "avoid holding write lock across await".into(),
        source: "loom_drop".into(),
        created_at_ns: 90,
        ..Default::default()
    };

    // Pushing before uploading CAS returns 422
    let push_missing = client
        .post(format!("{base}/v1/repos/team/loom/push"))
        .bearer_auth("top-secret-token")
        .json(&RemotePushRequest {
            branch: "main".into(),
            expected_remote_head: None,
            force: false,
            new_head_capsule_id: Some("cap-1".into()),
            new_head_snapshot_cas: Some(snap_hash),
            capsules: vec![cap1.clone()],
            patches: Vec::new(),
            negative_edges: vec![edge1.clone()],
        })
        .send()
        .await
        .unwrap();
    assert_eq!(push_missing.status(), 422);

    // Corrupt CAS upload returns 400
    let bad_put = client
        .put(format!("{base}/v1/repos/team/loom/cas/{}", file_hash.to_hex()))
        .bearer_auth("top-secret-token")
        .header("content-type", "application/zstd")
        .body(local_cas.get_raw_compressed(&snap_hash).unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(bad_put.status(), 400);

    // Check CAS has -> upload missing zstd blobs -> second cas/has reports none missing
    let has1: CasHasResponse = client
        .post(format!("{base}/v1/repos/team/loom/cas/has"))
        .bearer_auth("top-secret-token")
        .json(&CasHasRequest {
            hashes: vec![file_hash, snap_hash],
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(has1.missing.len(), 2);

    for h in [file_hash, snap_hash] {
        let put = client
            .put(format!("{base}/v1/repos/team/loom/cas/{}", h.to_hex()))
            .bearer_auth("top-secret-token")
            .header("content-type", "application/zstd")
            .body(local_cas.get_raw_compressed(&h).unwrap())
            .send()
            .await
            .unwrap();
        assert_eq!(put.status(), 201);
    }

    let has2: CasHasResponse = client
        .post(format!("{base}/v1/repos/team/loom/cas/has"))
        .bearer_auth("top-secret-token")
        .json(&CasHasRequest {
            hashes: vec![file_hash, snap_hash],
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(has2.missing.is_empty());

    // Push succeeds
    let push_ok: RemotePushResponse = client
        .post(format!("{base}/v1/repos/team/loom/push"))
        .bearer_auth("top-secret-token")
        .json(&RemotePushRequest {
            branch: "main".into(),
            expected_remote_head: None,
            force: false,
            new_head_capsule_id: Some("cap-1".into()),
            new_head_snapshot_cas: Some(snap_hash),
            capsules: vec![cap1.clone()],
            patches: Vec::new(),
            negative_edges: vec![edge1.clone()],
        })
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(push_ok.accepted_capsules, 1);
    assert_eq!(push_ok.accepted_negative_edges, 1);
    assert_eq!(push_ok.head_capsule_id.as_deref(), Some("cap-1"));

    // Non-fast-forward push with stale expected_remote_head returns 409
    let conflict = client
        .post(format!("{base}/v1/repos/team/loom/push"))
        .bearer_auth("top-secret-token")
        .json(&RemotePushRequest {
            branch: "main".into(),
            expected_remote_head: None,
            force: false,
            new_head_capsule_id: Some("cap-2".into()),
            new_head_snapshot_cas: Some(snap_hash),
            capsules: vec![CausalCapsule {
                capsule_id: "cap-2".into(),
                parent_capsule_ids: vec!["cap-1".into()],
                snapshot_cas_hash: Some(snap_hash),
                created_at_ns: 200,
                ..Default::default()
            }],
            patches: Vec::new(),
            negative_edges: Vec::new(),
        })
        .send()
        .await
        .unwrap();
    assert_eq!(conflict.status(), 409);

    // Pull returns capsule, negative edge, and required CAS hashes; GET CAS verifies hash
    let pull1: RemotePullResponse = client
        .post(format!("{base}/v1/repos/team/loom/pull"))
        .bearer_auth("top-secret-token")
        .json(&RemotePullRequest {
            branch: "main".into(),
            known_capsule_ids: Vec::new(),
            known_patch_ids: Vec::new(),
            known_negative_edge_ids: Vec::new(),
            known_cas_hashes: Vec::new(),
        })
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(pull1.capsules.len(), 1);
    assert_eq!(pull1.negative_edges.len(), 1);
    assert_eq!(pull1.required_cas_hashes.len(), 2);

    let clone_cas = CasStore::open_default(TempDir::new().unwrap().path().join("cas")).unwrap();
    for h in pull1.required_cas_hashes {
        let zst_bytes = client
            .get(format!("{base}/v1/repos/team/loom/cas/{}", h.to_hex()))
            .bearer_auth("top-secret-token")
            .header("accept", "application/zstd")
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert!(clone_cas.put_verified_compressed(&h, &zst_bytes).unwrap());
    }
    assert_eq!(clone_cas.get_bytes(&file_hash).unwrap(), file_bytes);

    // Incremental pull with known state returns empty deltas
    let pull2: RemotePullResponse = client
        .post(format!("{base}/v1/repos/team/loom/pull"))
        .bearer_auth("top-secret-token")
        .json(&RemotePullRequest {
            branch: "main".into(),
            known_capsule_ids: vec!["cap-1".into()],
            known_patch_ids: Vec::new(),
            known_negative_edge_ids: vec!["ne-1".into()],
            known_cas_hashes: vec![file_hash, snap_hash],
        })
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(pull2.capsules.is_empty());
    assert!(pull2.negative_edges.is_empty());
    assert!(pull2.required_cas_hashes.is_empty());

    let _ = Blake3Hash::ZERO;
    handle.shutdown().await.unwrap();
}
