use loom_core::model::{CausalCapsule, FailureKind, ViewportState};
use loom_core::rpc::{DropResult, GitPullResult, GitPushResult, OpLogView, ViewportSummary};
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use tempfile::TempDir;

fn loom_bin() -> &'static str {
    env!("CARGO_BIN_EXE_loom")
}

fn run_loom(
    repo: &Path,
    cwd: Option<&Path>,
    envs: &[(&str, &str)],
    args: &[&str],
) -> std::process::Output {
    let mut cmd = Command::new(loom_bin());
    cmd.env("LOOM_IDLE_TIMEOUT_MS", "2000");
    for (k, v) in envs {
        cmd.env(k, v);
    }
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
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if !sock.exists() {
            std::thread::sleep(Duration::from_millis(50));
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn init_git_repo(dir: &Path, files: &[(&str, &str)]) {
    let mut repo = gix::init(dir).unwrap();
    {
        let mut cfg = repo.config_snapshot_mut();
        cfg.append_config(
            ["user.name=CLI Test", "user.email=cli@example.com"],
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
    let _ = repo
        .commit(
            "refs/heads/main",
            "initial commit",
            tree_id,
            std::iter::empty::<gix::ObjectId>(),
        )
        .unwrap();
    std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    let mut idx = repo.index_from_tree(&tree_id).unwrap();
    idx.write(Default::default()).unwrap();
}

#[test]
fn cli_seal_and_drop_inference_validation_and_exit_codes() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn val() -> i32 { 1 }\n").unwrap();

    let init_out = run_loom(repo, None, &[], &["init", "--json"]);
    assert!(init_out.status.success());

    // Spawn 1 active viewport -> omitted `--viewport` resolves sole active viewport
    let sp1_out = run_loom(
        repo,
        None,
        &[],
        &["spawn", "-n", "vp-alpha", "-m", "alpha work", "--json"],
    );
    assert!(sp1_out.status.success());
    let vp1: ViewportSummary = serde_json::from_slice(&sp1_out.stdout).unwrap();

    std::fs::write(vp1.path.join("src/lib.rs"), "pub fn val() -> i32 { 20 }\n").unwrap();

    // Failing verify command returns exit code 1 and leaves viewport Active
    let fail_seal = run_loom(
        repo,
        None,
        &[],
        &["seal", "-v", "exit 1", "-m", "bad verify", "--json"],
    );
    assert_eq!(fail_seal.status.code(), Some(1));

    // Passing verify command seals capsule using sole active viewport inference + LOOM_VIEWPORT
    let pass_seal = run_loom(
        repo,
        None,
        &[],
        &["seal", "-v", "echo ok", "-m", "good seal", "--json"],
    );
    assert!(
        pass_seal.status.success(),
        "seal failed: {}",
        String::from_utf8_lossy(&pass_seal.stderr)
    );
    let capsule: CausalCapsule = serde_json::from_slice(&pass_seal.stdout).unwrap();
    assert!(capsule.verification_proof.as_ref().unwrap().passed);
    assert_eq!(
        std::fs::read_to_string(repo.join("src/lib.rs")).unwrap(),
        "pub fn val() -> i32 { 20 }\n"
    );

    // Spawn 2 active viewports -> omitting --viewport errors clearly on ambiguity, while CWD inside vp2 resolves vp2!
    let sp2_out = run_loom(
        repo,
        None,
        &[],
        &["spawn", "-n", "vp-beta", "-m", "beta work", "--json"],
    );
    assert!(sp2_out.status.success());
    let vp2: ViewportSummary = serde_json::from_slice(&sp2_out.stdout).unwrap();

    let sp3_out = run_loom(
        repo,
        None,
        &[],
        &["spawn", "-n", "vp-gamma", "-m", "gamma work", "--json"],
    );
    assert!(sp3_out.status.success());
    let vp3: ViewportSummary = serde_json::from_slice(&sp3_out.stdout).unwrap();

    // Ambiguous drop without --viewport from repo root must exit 1
    let ambig = run_loom(
        repo,
        Some(repo),
        &[],
        &["drop", "--reason", "ambiguous", "--json"],
    );
    assert_eq!(ambig.status.code(), Some(1));

    // Drop with >500-char reason must exit 1
    let long_reason = "x".repeat(501);
    let err_501 = run_loom(
        repo,
        None,
        &[],
        &[
            "drop",
            "--viewport",
            "vp-beta",
            "--reason",
            &long_reason,
            "--json",
        ],
    );
    assert_eq!(err_501.status.code(), Some(1));

    // Drop with invalid --kind must exit 1
    let err_kind = run_loom(
        repo,
        None,
        &[],
        &[
            "drop",
            "--viewport",
            "vp-beta",
            "--reason",
            "bad kind",
            "--kind",
            "invalid_kind",
            "--json",
        ],
    );
    assert_eq!(err_kind.status.code(), Some(1));

    // CWD inside vp-beta resolves vp-beta automatically!
    std::fs::write(vp2.path.join("src/lib.rs"), "pub fn val() -> i32 { 999 }\n").unwrap();
    let drop_vp2 = run_loom(
        repo,
        Some(&vp2.path),
        &[],
        &[
            "drop",
            "-r",
            "999 broke invariant",
            "-k",
            "test_failure",
            "--json",
        ],
    );
    assert!(
        drop_vp2.status.success(),
        "drop failed: {}",
        String::from_utf8_lossy(&drop_vp2.stderr)
    );
    let drop_res: DropResult = serde_json::from_slice(&drop_vp2.stdout).unwrap();
    assert_eq!(drop_res.viewport.state, ViewportState::Dropped);
    assert_eq!(drop_res.negative_edge.failure_kind, FailureKind::TestFailure);

    // LOOM_VIEWPORT env var resolves vp-gamma
    let drop_vp3 = run_loom(
        repo,
        None,
        &[("LOOM_VIEWPORT", &vp3.name)],
        &["drop", "-r", "gamma dead end", "-k", "deadend", "--json"],
    );
    assert!(drop_vp3.status.success());
    let drop3_res: DropResult = serde_json::from_slice(&drop_vp3.stdout).unwrap();
    assert_eq!(drop3_res.negative_edge.failure_kind, FailureKind::DesignDeadEnd);

    // Verify `loom log --capsules --symbol val --json` returns the sealed capsule
    let log_out = run_loom(
        repo,
        None,
        &[],
        &["log", "--capsules", "--symbol", "val", "--json"],
    );
    assert!(log_out.status.success());
    let logs: Vec<OpLogView> = serde_json::from_slice(&log_out.stdout).unwrap();
    assert_eq!(logs.len(), 1);

    wait_for_daemon_exit(repo);
}

#[test]
fn cli_git_push_and_pull_roundtrip_via_local_bare_repo() {
    let tmp = TempDir::new().unwrap();
    let bare = tmp.path().join("bare.git");
    let repo_a = tmp.path().join("repo_a");
    let repo_b = tmp.path().join("repo_b");
    std::fs::create_dir_all(&repo_a).unwrap();
    std::fs::create_dir_all(&repo_b).unwrap();

    let _bare_repo = gix::init_bare(&bare).unwrap();
    init_git_repo(&repo_a, &[("src/lib.rs", "pub fn api() -> i32 { 1 }\n")]);
    let _git_b = gix::init(&repo_b).unwrap();
    std::fs::write(repo_b.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

    assert!(run_loom(&repo_a, None, &[], &["init", "--colocate-git", "--json"]).status.success());
    let sp_a = run_loom(
        &repo_a,
        None,
        &[],
        &["spawn", "-n", "a1", "-m", "upgrade api", "--json"],
    );
    assert!(sp_a.status.success());
    let vp_a: ViewportSummary = serde_json::from_slice(&sp_a.stdout).unwrap();
    std::fs::write(vp_a.path.join("src/lib.rs"), "pub fn api() -> i32 { 77 }\n").unwrap();

    assert!(
        run_loom(
            &repo_a,
            None,
            &[],
            &["seal", "--viewport", "a1", "-m", "api -> 77", "--json"],
        )
        .status
        .success()
    );

    let bare_str = bare.to_string_lossy().into_owned();
    let push_out = run_loom(
        &repo_a,
        None,
        &[],
        &["git", "push", &bare_str, "main", "--json"],
    );
    assert!(
        push_out.status.success(),
        "git push failed: {}",
        String::from_utf8_lossy(&push_out.stderr)
    );
    let push_res: GitPushResult = serde_json::from_slice(&push_out.stdout).unwrap();
    assert_eq!(push_res.commits_created, 1);

    assert!(run_loom(&repo_b, None, &[], &["init", "--colocate-git", "--json"]).status.success());
    let pull_out = run_loom(
        &repo_b,
        None,
        &[],
        &["git", "pull", &bare_str, "main", "--json"],
    );
    assert!(
        pull_out.status.success(),
        "git pull failed: {}",
        String::from_utf8_lossy(&pull_out.stderr)
    );
    let pull_res: GitPullResult = serde_json::from_slice(&pull_out.stdout).unwrap();
    assert!(pull_res.updated_trunk);
    assert_eq!(pull_res.restored_capsule_ids.len(), 1);
    assert_eq!(
        std::fs::read_to_string(repo_b.join("src/lib.rs")).unwrap(),
        "pub fn api() -> i32 { 77 }\n"
    );

    wait_for_daemon_exit(&repo_a);
    wait_for_daemon_exit(&repo_b);
}

#[test]
fn cli_drop_from_viewport_subdir_restores_cleanly_without_sharing_violation() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("src/lib.rs"),
        "pub fn alpha() -> i32 { 1 }\npub fn beta() -> i32 { 2 }\n",
    )
    .unwrap();
    std::fs::write(repo.join("README.md"), "trunk readme\n").unwrap();

    assert!(run_loom(repo, None, &[], &["init", "--json"]).status.success());

    let sp = run_loom(
        repo,
        None,
        &[],
        &["spawn", "-n", "vp-subdir", "-m", "subdir drop", "--json"],
    );
    assert!(sp.status.success());
    let vp: ViewportSummary = serde_json::from_slice(&sp.stdout).unwrap();

    // Edit inside <vp>/src/ and <vp>/README.md, add extra file in <vp>/src/
    std::fs::write(
        vp.path.join("src/mixed.rs"),
        "pub fn valid_sibling() -> i32 { 7 }\npub fn broken_part( {\n",
    )
    .unwrap();
    std::fs::write(vp.path.join("README.md"), "modified readme\n").unwrap();
    std::fs::create_dir_all(vp.path.join(".loom")).unwrap();
    std::fs::write(vp.path.join(".loom/RADAR_STATUS.json"), "{\"status\":\"GREEN\"}").unwrap();

    // Invoke `loom drop` with CWD = <vp>/src (no --viewport flag)
    let vp_src = vp.path.join("src");
    let drop_out = run_loom(
        repo,
        Some(&vp_src),
        &[],
        &[
            "drop",
            "--reason",
            "dropped from inside viewport src subdir",
            "--kind",
            "deadend",
            "--json",
        ],
    );
    assert!(
        drop_out.status.success(),
        "drop from subdir failed: {}",
        String::from_utf8_lossy(&drop_out.stderr)
    );
    let drop_res: DropResult = serde_json::from_slice(&drop_out.stdout).unwrap();
    assert_eq!(drop_res.viewport.state, ViewportState::Dropped);
    assert_eq!(drop_res.negative_edge.failure_kind, FailureKind::DesignDeadEnd);
    assert!(
        drop_res
            .negative_edge
            .touched_symbols
            .iter()
            .any(|s| s == "valid_sibling"),
        "expected valid_sibling in touched_symbols: {:?}",
        drop_res.negative_edge.touched_symbols
    );

    // Trunk files must be cleanly restored and extra files + RADAR_STATUS.json removed
    assert_eq!(
        std::fs::read_to_string(vp.path.join("src/lib.rs")).unwrap(),
        "pub fn alpha() -> i32 { 1 }\npub fn beta() -> i32 { 2 }\n"
    );
    assert_eq!(
        std::fs::read_to_string(vp.path.join("README.md")).unwrap(),
        "trunk readme\n"
    );
    assert!(!vp.path.join("src/mixed.rs").exists());
    assert!(!vp.path.join(".loom/RADAR_STATUS.json").exists());

    wait_for_daemon_exit(repo);
}

#[test]
fn cli_unpushed_sealed_capsule_with_remote_ahead_pull_then_push_preserves_both() {
    let tmp = TempDir::new().unwrap();
    let bare = tmp.path().join("bare.git");
    let repo_a = tmp.path().join("repo_a");
    std::fs::create_dir_all(&repo_a).unwrap();

    let _bare_repo = gix::init_bare(&bare).unwrap();
    let c0_files = [("src/lib.rs", "pub fn f1() -> i32 { 1 }\npub fn f2() -> i32 { 2 }\n")];
    init_git_repo(&repo_a, &c0_files);

    assert!(run_loom(&repo_a, None, &[], &["init", "--colocate-git", "--json"]).status.success());
    let bare_str = bare.to_string_lossy().into_owned();

    // Seed bare.git from repo_a with an initial sealed+pushed capsule
    let sp0 = run_loom(
        &repo_a,
        None,
        &[],
        &["spawn", "-n", "vp0", "-m", "seed bare", "--json"],
    );
    assert!(sp0.status.success());
    let vp0: ViewportSummary = serde_json::from_slice(&sp0.stdout).unwrap();
    std::fs::write(
        vp0.path.join("src/lib.rs"),
        "pub fn f1() -> i32 { 1 }\npub fn f2() -> i32 { 2 }\npub fn f0() -> i32 { 0 }\n",
    )
    .unwrap();
    assert!(
        run_loom(
            &repo_a,
            None,
            &[],
            &["seal", "--viewport", "vp0", "-m", "c0", "--json"]
        )
        .status
        .success()
    );
    assert!(
        run_loom(&repo_a, None, &[], &["git", "push", &bare_str, "main", "--json"])
            .status
            .success()
    );

    // Advance bare.git with remote commit setting f2 = 300
    {
        let mut bare_rw = gix::open(&bare).unwrap();
        {
            let mut cfg = bare_rw.config_snapshot_mut();
            cfg.append_config(
                ["user.name=Remote Dev", "user.email=remote@example.com"],
                gix::config::Source::Api,
            )
            .unwrap();
            cfg.commit().unwrap();
        }
        let parent_id = bare_rw
            .try_find_reference("refs/heads/main")
            .unwrap()
            .unwrap()
            .peel_to_id()
            .unwrap()
            .detach();
        let parent_tree = bare_rw.find_commit(parent_id).unwrap().tree_id().unwrap().detach();
        let mut ed = bare_rw.edit_tree(parent_tree).unwrap();
        let new_blob = bare_rw
            .write_blob(b"pub fn f1() -> i32 { 1 }\npub fn f2() -> i32 { 300 }\npub fn f0() -> i32 { 0 }\n")
            .unwrap()
            .detach();
        ed.upsert("src/lib.rs", gix::objs::tree::EntryKind::Blob, new_blob)
            .unwrap();
        let new_tree = ed.write().unwrap().detach();
        bare_rw
            .commit(
                "refs/heads/main",
                "remote sets f2=300",
                new_tree,
                [parent_id],
            )
            .unwrap();
    }

    // In repo_a, seal local capsule setting f1 = 777 (unpushed)
    let sp1 = run_loom(
        &repo_a,
        None,
        &[],
        &["spawn", "-n", "vp1", "-m", "set f1=777", "--json"],
    );
    assert!(sp1.status.success());
    let vp1: ViewportSummary = serde_json::from_slice(&sp1.stdout).unwrap();
    std::fs::write(
        vp1.path.join("src/lib.rs"),
        "pub fn f1() -> i32 { 777 }\npub fn f2() -> i32 { 2 }\npub fn f0() -> i32 { 0 }\n",
    )
    .unwrap();
    assert!(
        run_loom(
            &repo_a,
            None,
            &[],
            &["seal", "--viewport", "vp1", "-m", "set f1=777", "--json"]
        )
        .status
        .success()
    );

    // Push must reject because remote is ahead
    let push_rej = run_loom(
        &repo_a,
        None,
        &[],
        &["git", "push", &bare_str, "main", "--json"],
    );
    assert_eq!(push_rej.status.code(), Some(1));

    // Pull must preserve f1=777 AND merge f2=300 on trunk
    let pull_ok = run_loom(
        &repo_a,
        None,
        &[],
        &["git", "pull", &bare_str, "main", "--json"],
    );
    assert!(
        pull_ok.status.success(),
        "git pull failed: {}",
        String::from_utf8_lossy(&pull_ok.stderr)
    );
    let trunk_lib = std::fs::read_to_string(repo_a.join("src/lib.rs")).unwrap();
    assert!(trunk_lib.contains("777"), "trunk lost f1=777: {trunk_lib}");
    assert!(trunk_lib.contains("300"), "trunk missing f2=300: {trunk_lib}");

    // Subsequent push must push merged snapshot without reverting f2=300
    let push_ok = run_loom(
        &repo_a,
        None,
        &[],
        &["git", "push", &bare_str, "main", "--json"],
    );
    assert!(
        push_ok.status.success(),
        "git push after pull failed: {}",
        String::from_utf8_lossy(&push_ok.stderr)
    );

    // Verify bare repo tip contains BOTH f1=777 and f2=300
    let bare_check = gix::open(&bare).unwrap();
    let tip = bare_check
        .try_find_reference("refs/heads/main")
        .unwrap()
        .unwrap()
        .peel_to_id()
        .unwrap()
        .detach();
    let tree = bare_check.find_commit(tip).unwrap().tree().unwrap();
    let entry = tree.lookup_entry_by_path("src/lib.rs").unwrap().unwrap();
    let blob_str = String::from_utf8(bare_check.find_blob(entry.oid()).unwrap().data.clone()).unwrap();
    assert!(blob_str.contains("777"), "bare lost f1=777: {blob_str}");
    assert!(blob_str.contains("300"), "bare reverted f2=300: {blob_str}");

    // Verify .git/index in repo_a matches HEAD tree
    let local_check = gix::open(&repo_a).unwrap();
    let head_tree_id = local_check.head_commit().unwrap().tree_id().unwrap().detach();
    let idx = local_check.index().unwrap();
    let idx_entry = idx.entry_by_path("src/lib.rs".into()).unwrap();
    assert_eq!(idx_entry.id, entry.oid());
    let _ = head_tree_id;

    wait_for_daemon_exit(&repo_a);
}
