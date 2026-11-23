use loom_core::db::StateDb;
use loom_core::rpc::{
    RpcMethod, RpcRequest, RpcResponse, canonical_repo_root, read_frame, write_frame,
};
#[cfg(windows)]
use loom_core::rpc::{pipe_short_name, read_sock_descriptor};
#[cfg(unix)]
use loom_core::rpc::sock_descriptor_path;
use loom_server::{ServerConfig, start_server};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn loom_bin() -> &'static str {
    env!("CARGO_BIN_EXE_loom")
}

fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        p.to_path_buf()
    }
}

struct DaemonGuard {
    repos: Vec<PathBuf>,
}

impl DaemonGuard {
    fn new() -> Self {
        Self { repos: Vec::new() }
    }

    fn track(&mut self, repo: PathBuf) {
        self.repos.push(repo);
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        for repo in &self.repos {
            shutdown_daemon_ipc(repo);
        }
    }
}

fn shutdown_daemon_ipc(repo: &Path) {
    if !repo.join(".loom").exists() {
        return;
    }
    let repo = repo.to_path_buf();
    let _ = std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let stream_res = {
                #[cfg(windows)]
                {
                    use interprocess::local_socket::{GenericNamespaced, ToNsName, tokio::prelude::*};
                    let pipe_short = match read_sock_descriptor(&repo) {
                        Ok(desc) => desc
                            .endpoint
                            .strip_prefix(r"\\.\pipe\")
                            .unwrap_or(&desc.endpoint)
                            .to_string(),
                        Err(_) => pipe_short_name(&repo).unwrap(),
                    };
                    let ns_name = pipe_short.as_str().to_ns_name::<GenericNamespaced>().unwrap();
                    interprocess::local_socket::tokio::Stream::connect(ns_name).await
                }
                #[cfg(unix)]
                {
                    use interprocess::local_socket::{GenericFilePath, ToFsName, tokio::prelude::*};
                    let sock_path = sock_descriptor_path(&repo);
                    let fs_name = sock_path.as_path().to_fs_name::<GenericFilePath>().unwrap();
                    interprocess::local_socket::tokio::Stream::connect(fs_name).await
                }
            };
            if let Ok(mut stream) = stream_res {
                let _ = write_frame(
                    &mut stream,
                    &RpcRequest {
                        id: 1,
                        method: RpcMethod::Shutdown,
                    },
                )
                .await;
                let _ = read_frame::<_, RpcResponse>(&mut stream).await;
            }
        });
        let sock = repo.join(".loom/daemon.sock");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && sock.exists() {
            std::thread::sleep(Duration::from_millis(40));
        }
        std::thread::sleep(Duration::from_millis(60));
    })
    .join();
}

fn run_loom(args: &[&str], env_vars: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(loom_bin());
    cmd.env("LOOM_IDLE_TIMEOUT_MS", "10000")
        .env_remove("LOOM_TOKEN")
        .env_remove("LOOM_REMOTE_TOKEN");
    for (k, v) in env_vars {
        cmd.env(k, v);
    }
    tokio::task::block_in_place(|| cmd.args(args).output().expect("execute loom CLI"))
}

fn assert_ok(out: &Output, label: &str) -> serde_json::Value {
    assert!(
        out.status.success(),
        "{label} failed (code={:?}):\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "{label} invalid JSON ({e}):\n{}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

fn noop_verify_cmd() -> &'static str {
    if cfg!(windows) {
        "cmd /C exit 0"
    } else {
        "true"
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_push_clone_preserves_capsules_proof_negative_edges_and_redacts_tokens() {
    let server_dir = TempDir::new().unwrap();
    let handle = start_server(ServerConfig {
        bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        data_dir: server_dir.path().to_path_buf(),
        tokens: vec!["secret-token-alpha".to_string()],
        allow_unauthenticated: false,
        allow_auto_create: true,
        ..ServerConfig::default()
    })
    .await
    .unwrap();
    let remote_url = format!("http://{}/wick-lim/loom-native", handle.local_addr);

    let mut guard = DaemonGuard::new();

    let tmp_a = TempDir::new().unwrap();
    let repo_a = strip_verbatim(&canonical_repo_root(tmp_a.path()).unwrap());
    guard.track(repo_a.clone());
    std::fs::create_dir_all(repo_a.join("src")).unwrap();
    std::fs::write(
        repo_a.join("src/lib.rs"),
        "pub fn greet() -> i32 {\n    1\n}\n",
    )
    .unwrap();

    let repo_a_str = repo_a.to_string_lossy().into_owned();
    assert_ok(&run_loom(&["--repo", &repo_a_str, "init", "--json"], &[]), "init repo_a");

    let spawn_feat = assert_ok(
        &run_loom(
            &[
                "--repo",
                &repo_a_str,
                "spawn",
                "-n",
                "feat",
                "-m",
                "implement farewell",
                "--json",
            ],
            &[],
        ),
        "spawn feat",
    );
    let vp_feat_path = PathBuf::from(spawn_feat["path"].as_str().unwrap());
    std::fs::write(
        vp_feat_path.join("src/lib.rs"),
        "pub fn greet() -> i32 {\n    1\n}\n\npub fn farewell() -> i32 {\n    2\n}\n",
    )
    .unwrap();
    let seal_out = assert_ok(
        &run_loom(
            &[
                "--repo",
                &repo_a_str,
                "seal",
                "--viewport",
                "feat",
                "-v",
                noop_verify_cmd(),
                "-m",
                "add farewell function",
                "--json",
            ],
            &[],
        ),
        "seal feat",
    );
    let sealed_cap_id = seal_out["capsule_id"].as_str().unwrap().to_string();

    assert_ok(
        &run_loom(
            &[
                "--repo",
                &repo_a_str,
                "spawn",
                "-n",
                "bad-exp",
                "-m",
                "test lock approach",
                "--json",
            ],
            &[],
        ),
        "spawn bad-exp",
    );
    assert_ok(
        &run_loom(
            &[
                "--repo",
                &repo_a_str,
                "drop",
                "--viewport",
                "bad-exp",
                "-r",
                "avoid mutex across await in greet",
                "-k",
                "Deadlock",
                "--json",
            ],
            &[],
        ),
        "drop bad-exp",
    );

    assert_ok(
        &run_loom(
            &[
                "--repo",
                &repo_a_str,
                "remote",
                "add",
                "origin",
                &remote_url,
                "--token",
                "secret-token-alpha",
                "--json",
            ],
            &[],
        ),
        "remote add",
    );

    let remote_list = run_loom(&["--repo", &repo_a_str, "remote", "list", "--json"], &[]);
    let list_json = assert_ok(&remote_list, "remote list");
    let list_str = String::from_utf8_lossy(&remote_list.stdout);
    assert!(!list_str.contains("secret-token-alpha"), "raw token leaked: {list_str}");
    assert_eq!(list_json[0]["name"], "origin");
    assert_eq!(list_json[0]["has_token"], true);

    let push_res = assert_ok(
        &run_loom(&["--repo", &repo_a_str, "push", "origin", "main", "--json"], &[]),
        "push repo_a",
    );
    assert_eq!(push_res["head_capsule_id"], sealed_cap_id);
    assert!(!push_res["pushed_capsule_ids"].as_array().unwrap().is_empty());
    assert_eq!(push_res["pushed_negative_edges"], 1);
    assert!(push_res["uploaded_cas_blobs"].as_u64().unwrap() >= 1);

    // Clone into fresh destination using token env var
    let clone_parent = TempDir::new().unwrap();
    let clone_dest = clone_parent.path().join("cloned-loom");
    let clone_dest_str = clone_dest.to_string_lossy().into_owned();
    guard.track(clone_dest.clone());

    let clone_res = assert_ok(
        &run_loom(
            &[
                "clone",
                &remote_url,
                &clone_dest_str,
                "--token-env",
                "TEST_CLONE_TOK",
                "--json",
            ],
            &[("TEST_CLONE_TOK", "secret-token-alpha")],
        ),
        "clone native remote",
    );
    assert_eq!(clone_res["head_capsule_id"], sealed_cap_id);
    assert_eq!(clone_res["restored_negative_edges"], 1);
    assert!(!clone_dest.join(".git").exists(), "native clone should not require .git");
    let cloned_lib = std::fs::read_to_string(clone_dest.join("src/lib.rs")).unwrap();
    assert!(cloned_lib.contains("pub fn farewell() -> i32"));

    // Verify capsule, VerificationProof, and NegativeEdge are intact in cloned repo
    let log_caps = assert_ok(
        &run_loom(
            &["--repo", &clone_dest_str, "log", "--capsules", "--json"],
            &[],
        ),
        "cloned log capsules",
    );
    let caps_arr = log_caps.as_array().unwrap();
    assert!(caps_arr.iter().any(|c| {
        c["target_id"] == sealed_cap_id && c["capsule_summary"] == "add farewell function"
    }));

    shutdown_daemon_ipc(&clone_dest);
    let cloned_db = {
        let db_path = clone_dest.join(".loom/state.redb");
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match StateDb::open_or_create(&db_path) {
                Ok(db) => break db,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(40));
                }
                Err(e) => panic!("failed to open cloned state.redb: {e}"),
            }
        }
    };
    let cloned_cap = cloned_db
        .get_capsule(&sealed_cap_id)
        .unwrap()
        .expect("cloned repo should contain sealed capsule");
    assert_eq!(cloned_cap.summary, "add farewell function");
    assert!(
        cloned_cap
            .verification_proof
            .as_ref()
            .expect("verification proof preserved")
            .passed
    );
    if let Some(proof_hash) = cloned_cap.proof_cas_hash {
        let cas = loom_core::cas::CasStore::new(clone_dest.join(".loom/cas"), 3).unwrap();
        assert!(cas.contains(&proof_hash), "proof CAS blob missing in clone");
    }
    let cloned_edges = cloned_db.list_all_negative_edges().unwrap();
    assert_eq!(cloned_edges.len(), 1);
    assert_eq!(
        cloned_edges[0].lesson_summary,
        "avoid mutex across await in greet"
    );
    drop(cloned_db);

    drop(guard);
    handle.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_pull_concurrent_edits_merges_and_rebases_active_viewport() {
    let server_dir = TempDir::new().unwrap();
    let handle = start_server(ServerConfig {
        bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        data_dir: server_dir.path().to_path_buf(),
        tokens: vec!["tok-rebase".to_string()],
        allow_unauthenticated: false,
        allow_auto_create: true,
        ..ServerConfig::default()
    })
    .await
    .unwrap();
    let remote_url = format!("http://{}/rebase-proj", handle.local_addr);

    let mut guard = DaemonGuard::new();

    // Repo A initializes trunk and pushes
    let tmp_a = TempDir::new().unwrap();
    let repo_a = strip_verbatim(&canonical_repo_root(tmp_a.path()).unwrap());
    guard.track(repo_a.clone());
    std::fs::create_dir_all(repo_a.join("src")).unwrap();
    std::fs::write(
        repo_a.join("src/lib.rs"),
        "pub fn alpha() -> i32 {\n    1\n}\n\npub fn beta() -> i32 {\n    2\n}\n",
    )
    .unwrap();

    let repo_a_str = repo_a.to_string_lossy().into_owned();
    assert_ok(&run_loom(&["--repo", &repo_a_str, "init", "--json"], &[]), "init a");
    assert_ok(
        &run_loom(
            &[
                "--repo",
                &repo_a_str,
                "remote",
                "add",
                "origin",
                &remote_url,
                "--token",
                "tok-rebase",
                "--json",
            ],
            &[],
        ),
        "remote add a",
    );
    assert_ok(
        &run_loom(&["--repo", &repo_a_str, "push", "origin", "main", "--json"], &[]),
        "push initial",
    );

    // Repo B clones
    let tmp_b = TempDir::new().unwrap();
    let repo_b = tmp_b.path().join("b");
    let repo_b_str = repo_b.to_string_lossy().into_owned();
    guard.track(repo_b.clone());
    assert_ok(
        &run_loom(
            &["clone", &remote_url, &repo_b_str, "--token", "tok-rebase", "--json"],
            &[],
        ),
        "clone b",
    );

    // Repo A spawns active viewport vp-a and edits alpha() -> 10
    let vp_a = assert_ok(
        &run_loom(
            &[
                "--repo",
                &repo_a_str,
                "spawn",
                "-n",
                "vp-a",
                "-m",
                "update alpha",
                "--json",
            ],
            &[],
        ),
        "spawn vp-a",
    );
    let vp_a_path = PathBuf::from(vp_a["path"].as_str().unwrap());
    std::fs::write(
        vp_a_path.join("src/lib.rs"),
        "pub fn alpha() -> i32 {\n    10\n}\n\npub fn beta() -> i32 {\n    2\n}\n",
    )
    .unwrap();

    // Repo B spawns vp-b, adds gamma(), seals, and pushes
    let vp_b = assert_ok(
        &run_loom(
            &[
                "--repo",
                &repo_b_str,
                "spawn",
                "-n",
                "vp-b",
                "-m",
                "add gamma",
                "--json",
            ],
            &[],
        ),
        "spawn vp-b",
    );
    let vp_b_path = PathBuf::from(vp_b["path"].as_str().unwrap());
    std::fs::write(
        vp_b_path.join("src/lib.rs"),
        "pub fn alpha() -> i32 {\n    1\n}\n\npub fn beta() -> i32 {\n    2\n}\n\npub fn gamma() -> i32 {\n    3\n}\n",
    )
    .unwrap();
    assert_ok(
        &run_loom(
            &[
                "--repo",
                &repo_b_str,
                "seal",
                "--viewport",
                "vp-b",
                "-m",
                "add gamma",
                "--json",
            ],
            &[],
        ),
        "seal vp-b",
    );
    assert_ok(
        &run_loom(&["--repo", &repo_b_str, "push", "origin", "main", "--json"], &[]),
        "push b",
    );

    // Repo A pulls -> trunk updates with gamma(), active vp-a auto-rebases preserving alpha()=10 + gamma()=3
    let pull_a = assert_ok(
        &run_loom(&["--repo", &repo_a_str, "pull", "origin", "main", "--json"], &[]),
        "pull a",
    );
    assert_eq!(pull_a["updated_trunk"], true);
    let rebased = pull_a["rebased_viewports"].as_array().unwrap();
    assert_eq!(rebased.len(), 1);
    assert_eq!(rebased[0]["outcome"], "clean");

    let trunk_a = std::fs::read_to_string(repo_a.join("src/lib.rs")).unwrap();
    assert!(trunk_a.contains("pub fn gamma() -> i32"));
    let vp_a_src = std::fs::read_to_string(vp_a_path.join("src/lib.rs")).unwrap();
    assert!(vp_a_src.contains("10"), "vp-a edit lost: {vp_a_src}");
    assert!(vp_a_src.contains("pub fn gamma() -> i32"), "vp-a rebase missed gamma: {vp_a_src}");

    drop(guard);
    handle.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_non_fast_forward_and_auth_failures_handled_cleanly() {
    let server_dir = TempDir::new().unwrap();
    let handle = start_server(ServerConfig {
        bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        data_dir: server_dir.path().to_path_buf(),
        tokens: vec!["valid-token".to_string()],
        allow_unauthenticated: false,
        allow_auto_create: true,
        ..ServerConfig::default()
    })
    .await
    .unwrap();
    let remote_url = format!("http://{}/nff-proj", handle.local_addr);

    let mut guard = DaemonGuard::new();

    let tmp_a = TempDir::new().unwrap();
    let repo_a = strip_verbatim(&canonical_repo_root(tmp_a.path()).unwrap());
    guard.track(repo_a.clone());
    std::fs::create_dir_all(repo_a.join("src")).unwrap();
    std::fs::write(repo_a.join("src/a.rs"), "pub fn a() -> i32 { 1 }\n").unwrap();
    let repo_a_str = repo_a.to_string_lossy().into_owned();

    assert_ok(&run_loom(&["--repo", &repo_a_str, "init", "--json"], &[]), "init a");
    assert_ok(
        &run_loom(
            &["--repo", &repo_a_str, "remote", "add", "origin", &remote_url, "--json"],
            &[],
        ),
        "remote add a",
    );

    // Unauthorized push fails
    let unauth_push = run_loom(&["--repo", &repo_a_str, "push", "origin", "main"], &[]);
    assert!(!unauth_push.status.success());

    // Wrong token clone fails and cleans up target dir
    let bad_clone_parent = TempDir::new().unwrap();
    let bad_clone_dest = bad_clone_parent.path().join("should-clean");
    let bad_clone = run_loom(
        &[
            "clone",
            &remote_url,
            &bad_clone_dest.to_string_lossy(),
            "--token",
            "wrong-token",
        ],
        &[],
    );
    assert!(!bad_clone.status.success());
    assert!(!bad_clone_dest.exists());

    // Push initial file with valid token via --token
    assert_ok(
        &run_loom(
            &[
                "--repo",
                &repo_a_str,
                "push",
                "origin",
                "main",
                "--token",
                "valid-token",
                "--json",
            ],
            &[],
        ),
        "initial push",
    );

    // Clone into B, add src/b.rs, seal, and push
    let tmp_b = TempDir::new().unwrap();
    let repo_b = tmp_b.path().join("b");
    let repo_b_str = repo_b.to_string_lossy().into_owned();
    guard.track(repo_b.clone());
    assert_ok(
        &run_loom(
            &["clone", &remote_url, &repo_b_str, "--token", "valid-token", "--json"],
            &[],
        ),
        "clone b",
    );
    let vp_b = assert_ok(
        &run_loom(
            &["--repo", &repo_b_str, "spawn", "-n", "b-work", "-m", "add b", "--json"],
            &[],
        ),
        "spawn b-work",
    );
    std::fs::write(
        PathBuf::from(vp_b["path"].as_str().unwrap()).join("src/b.rs"),
        "pub fn b() -> i32 { 2 }\n",
    )
    .unwrap();
    assert_ok(
        &run_loom(
            &["--repo", &repo_b_str, "seal", "--viewport", "b-work", "-m", "seal b", "--json"],
            &[],
        ),
        "seal b-work",
    );
    assert_ok(
        &run_loom(&["--repo", &repo_b_str, "push", "origin", "main", "--json"], &[]),
        "push b",
    );

    // Repo A seals disjoint src/c.rs and tries to push without pulling -> non-fast-forward rejection
    let vp_a = assert_ok(
        &run_loom(
            &["--repo", &repo_a_str, "spawn", "-n", "a-work", "-m", "add c", "--json"],
            &[],
        ),
        "spawn a-work",
    );
    std::fs::write(
        PathBuf::from(vp_a["path"].as_str().unwrap()).join("src/c.rs"),
        "pub fn c() -> i32 { 3 }\n",
    )
    .unwrap();
    assert_ok(
        &run_loom(
            &["--repo", &repo_a_str, "seal", "--viewport", "a-work", "-m", "seal c", "--json"],
            &[],
        ),
        "seal a-work",
    );

    let stale_push = run_loom(
        &[
            "--repo",
            &repo_a_str,
            "push",
            "origin",
            "main",
            "--token",
            "valid-token",
        ],
        &[],
    );
    assert!(!stale_push.status.success(), "expected non-fast-forward push rejection");
    let stderr = String::from_utf8_lossy(&stale_push.stderr);
    assert!(
        stderr.contains("non-fast-forward") || stderr.contains("409"),
        "unexpected error message: {stderr}"
    );

    // Repo A pulls -> 3-way clean merge -> push succeeds -> B pulls src/c.rs
    assert_ok(
        &run_loom(
            &[
                "--repo",
                &repo_a_str,
                "pull",
                "origin",
                "main",
                "--token",
                "valid-token",
                "--json",
            ],
            &[],
        ),
        "pull merge a",
    );
    assert!(repo_a.join("src/b.rs").is_file());
    assert!(repo_a.join("src/c.rs").is_file());

    assert_ok(
        &run_loom(
            &[
                "--repo",
                &repo_a_str,
                "push",
                "origin",
                "main",
                "--token",
                "valid-token",
                "--json",
            ],
            &[],
        ),
        "push after merge",
    );

    assert_ok(
        &run_loom(&["--repo", &repo_b_str, "pull", "origin", "main", "--json"], &[]),
        "pull b final",
    );
    assert!(repo_b.join("src/c.rs").is_file());

    drop(guard);
    handle.shutdown().await.unwrap();
}
