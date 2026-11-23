use loom_core::model::{CowBackendKind, ViewportState};
use loom_core::rpc::{
    DaemonStatus, RpcMethod, RpcPayload, RpcRequest, RpcResponse, ViewportSummary, read_frame,
    write_frame,
};
#[cfg(windows)]
use loom_core::rpc::{pipe_short_name, read_sock_descriptor};
#[cfg(unix)]
use loom_core::rpc::sock_descriptor_path;
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use tempfile::TempDir;

fn loom_bin() -> &'static str {
    env!("CARGO_BIN_EXE_loom")
}

fn run_loom(repo: &Path, args: &[&str], idle_ms: u64) -> std::process::Output {
    Command::new(loom_bin())
        .env("LOOM_IDLE_TIMEOUT_MS", idle_ms.to_string())
        .arg("--repo")
        .arg(repo)
        .args(args)
        .output()
        .expect("execute loom CLI")
}

fn wait_for_daemon_exit(repo: &Path) {
    let sock = repo.join(".loom/daemon.sock");
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while std::time::Instant::now() < deadline {
        if !sock.exists() {
            std::thread::sleep(Duration::from_millis(50));
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn shutdown_daemon_ipc(repo: &Path) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let stream_res = {
            #[cfg(windows)]
            {
                use interprocess::local_socket::{GenericNamespaced, ToNsName, tokio::prelude::*};
                let pipe_short = match read_sock_descriptor(repo) {
                    Ok(desc) => desc
                        .endpoint
                        .strip_prefix(r"\\.\pipe\")
                        .unwrap_or(&desc.endpoint)
                        .to_string(),
                    Err(_) => pipe_short_name(repo).unwrap(),
                };
                let ns_name = pipe_short.as_str().to_ns_name::<GenericNamespaced>().unwrap();
                interprocess::local_socket::tokio::Stream::connect(ns_name).await
            }
            #[cfg(unix)]
            {
                use interprocess::local_socket::{GenericFilePath, ToFsName, tokio::prelude::*};
                let sock_path = sock_descriptor_path(repo);
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
    wait_for_daemon_exit(repo);
}

#[test]
fn cli_spawn_without_cmd_prints_json_and_human_and_isolates_viewports() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::create_dir_all(repo.join("target/debug/deps")).unwrap();
    std::fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn trunk() -> i32 { 1 }\n").unwrap();
    std::fs::write(repo.join("target/.cargo-lock"), "trunk-lock").unwrap();
    std::fs::write(repo.join("target/debug/deps/serde-1234.rlib"), "rlib").unwrap();

    let init_out = run_loom(repo, &["init", "--json"], 2000);
    assert!(
        init_out.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&init_out.stderr)
    );

    // Spawn v1 with --json
    let spawn1 = run_loom(
        repo,
        &["spawn", "-n", "v1", "-m", "refactor parser", "--json"],
        2000,
    );
    assert!(
        spawn1.status.success(),
        "spawn v1 failed: {}",
        String::from_utf8_lossy(&spawn1.stderr)
    );
    let vp1: ViewportSummary = serde_json::from_slice(&spawn1.stdout).unwrap();
    assert_eq!(vp1.name, "v1");
    assert_eq!(vp1.intent, "refactor parser");
    assert_eq!(vp1.state, ViewportState::Active);
    assert!(vp1.path.join("src/lib.rs").is_file());
    assert!(!vp1.path.join("target/.cargo-lock").exists());
    assert!(vp1.path.join("target/debug/deps/serde-1234.rlib").is_file());
    #[cfg(target_os = "windows")]
    assert_eq!(vp1.driver, CowBackendKind::WindowsNtfsFallback);

    // Spawn v2 in human-readable mode
    let spawn2 = run_loom(repo, &["spawn", "-n", "v2", "-m", "add tests"], 2000);
    assert!(
        spawn2.status.success(),
        "spawn v2 failed: {}",
        String::from_utf8_lossy(&spawn2.stderr)
    );
    let stdout2 = String::from_utf8_lossy(&spawn2.stdout);
    assert!(stdout2.contains("Spawned viewport `v2`"));

    // Mutate v1's src/lib.rs -> trunk & v2 stay isolated
    std::fs::write(vp1.path.join("src/lib.rs"), "pub fn trunk() -> i32 { 99 }\n").unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.join("src/lib.rs")).unwrap(),
        "pub fn trunk() -> i32 { 1 }\n"
    );
    let vp2_path = repo.join(".loom/viewports/v2");
    assert_eq!(
        std::fs::read_to_string(vp2_path.join("src/lib.rs")).unwrap(),
        "pub fn trunk() -> i32 { 1 }\n"
    );

    // `loom list` and `loom status` report live viewports
    let list_out = run_loom(repo, &["list", "--json"], 2000);
    assert!(list_out.status.success());
    let listed: Vec<ViewportSummary> = serde_json::from_slice(&list_out.stdout).unwrap();
    assert_eq!(listed.len(), 2);

    let status_out = run_loom(repo, &["status", "--json"], 2000);
    assert!(status_out.status.success());
    let st: DaemonStatus = serde_json::from_slice(&status_out.stdout).unwrap();
    assert_eq!(st.active_viewports, 2);
    assert_eq!(st.viewports.len(), 2);
    assert!(st.default_driver.is_some());
    assert!(st.idle_remaining_ms.is_none());

    // Seal both viewports via IPC and verify daemon auto-exits via idle timeout
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let mut stream = {
            #[cfg(windows)]
            {
                use interprocess::local_socket::{GenericNamespaced, ToNsName, tokio::prelude::*};
                let desc = read_sock_descriptor(repo).unwrap();
                let short = desc
                    .endpoint
                    .strip_prefix(r"\\.\pipe\")
                    .unwrap_or(&desc.endpoint)
                    .to_string();
                let ns_name = short.as_str().to_ns_name::<GenericNamespaced>().unwrap();
                interprocess::local_socket::tokio::Stream::connect(ns_name)
                    .await
                    .unwrap()
            }
            #[cfg(unix)]
            {
                use interprocess::local_socket::{GenericFilePath, ToFsName, tokio::prelude::*};
                let sock_path = sock_descriptor_path(repo);
                let fs_name = sock_path.as_path().to_fs_name::<GenericFilePath>().unwrap();
                interprocess::local_socket::tokio::Stream::connect(fs_name)
                    .await
                    .unwrap()
            }
        };
        for vp in &listed {
            write_frame(
                &mut stream,
                &RpcRequest {
                    id: 1,
                    method: RpcMethod::TransitionViewport {
                        viewport_id: vp.id.clone(),
                        target_state: ViewportState::Sealed,
                        pid: None,
                    },
                },
            )
            .await
            .unwrap();
            let resp: RpcResponse = read_frame(&mut stream).await.unwrap().unwrap();
            assert!(matches!(resp.result, Ok(RpcPayload::Viewport(_))));
        }
    });

    wait_for_daemon_exit(repo);
    assert!(!repo.join(".loom/daemon.sock").exists());
}

#[test]
fn cli_spawn_with_cmd_sets_cwd_env_and_forwards_exit_code() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn ok() {}\n").unwrap();

    let init_out = run_loom(repo, &["init", "--json"], 5000);
    assert!(init_out.status.success());

    #[cfg(windows)]
    let out = run_loom(
        repo,
        &[
            "spawn",
            "-n",
            "runner",
            "-m",
            "exec child",
            "--",
            "cmd",
            "/C",
            "echo VP=%LOOM_VIEWPORT% ROOT=%LOOM_VIEWPORT_ROOT% REPO=%LOOM_REPO_ROOT% ARG=--json && cd && exit /b 42",
        ],
        5000,
    );

    #[cfg(unix)]
    let out = run_loom(
        repo,
        &[
            "spawn",
            "-n",
            "runner",
            "-m",
            "exec child",
            "--",
            "sh",
            "-c",
            "echo VP=$LOOM_VIEWPORT ROOT=$LOOM_VIEWPORT_ROOT REPO=$LOOM_REPO_ROOT ARG=--json && pwd && exit 42",
        ],
        5000,
    );

    assert_eq!(
        out.status.code(),
        Some(42),
        "expected exit code 42, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let expected_vp_root = std::fs::canonicalize(repo.join(".loom/viewports/runner")).unwrap();
    let expected_repo_root = std::fs::canonicalize(repo).unwrap();
    assert!(stdout.contains("VP=runner"), "stdout={stdout}");
    assert!(stdout.contains("ARG=--json"), "stdout={stdout}");
    assert!(
        stdout
            .to_lowercase()
            .contains(&expected_vp_root.to_string_lossy().to_lowercase()),
        "stdout={stdout}, expected_vp_root={}",
        expected_vp_root.display()
    );
    assert!(
        stdout
            .to_lowercase()
            .contains(&expected_repo_root.to_string_lossy().to_lowercase()),
        "stdout={stdout}, expected_repo_root={}",
        expected_repo_root.display()
    );

    // Verify child PID was recorded on the viewport
    let list_out = run_loom(repo, &["list", "--json"], 5000);
    assert!(list_out.status.success());
    let listed: Vec<ViewportSummary> = serde_json::from_slice(&list_out.stdout).unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].pid.is_some());

    shutdown_daemon_ipc(repo);
}

#[test]
fn cli_spawn_missing_cmd_cleans_up_viewport_and_windows_cmd_shim_executes() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn ok() {}\n").unwrap();

    assert!(run_loom(&repo, &["init", "--json"], 2000).status.success());

    // Create unrelated Sealed & Dropped viewports to ensure failed-spawn cleanup
    // never runs global GC or touches unrelated viewports.
    assert!(
        run_loom(
            &repo,
            &["spawn", "-n", "vp-keep-sealed", "-m", "keep sealed", "--json"],
            2000,
        )
        .status
        .success()
    );
    assert!(
        run_loom(
            &repo,
            &["seal", "--viewport", "vp-keep-sealed", "-m", "seal keep", "--json"],
            2000,
        )
        .status
        .success()
    );
    assert!(
        run_loom(
            &repo,
            &["spawn", "-n", "vp-keep-dropped", "-m", "keep dropped", "--json"],
            2000,
        )
        .status
        .success()
    );
    assert!(
        run_loom(
            &repo,
            &[
                "drop",
                "--viewport",
                "vp-keep-dropped",
                "--reason",
                "keep dropped for unrelated check",
                "--json",
            ],
            2000,
        )
        .status
        .success()
    );

    // 1. Missing command must fail AND not leave an orphaned/dangling viewport in list, status, or disk
    let missing = run_loom(
        &repo,
        &[
            "spawn",
            "-n",
            "vp-missing",
            "-m",
            "missing",
            "--",
            "definitely_missing_bin_xyz",
        ],
        2000,
    );
    assert!(!missing.status.success());
    let list_after_missing = run_loom(&repo, &["list", "--json"], 2000);
    assert!(list_after_missing.status.success());
    let vps: Vec<ViewportSummary> = serde_json::from_slice(&list_after_missing.stdout).unwrap();
    assert!(
        !vps.iter().any(|v| v.name == "vp-missing"),
        "failed spawn must not leave a dangling entry in loom list: {vps:?}"
    );
    assert!(
        vps.iter()
            .any(|v| v.name == "vp-keep-sealed" && v.state == ViewportState::Sealed),
        "unrelated Sealed viewport must remain unaffected: {vps:?}"
    );
    assert!(
        vps.iter()
            .any(|v| v.name == "vp-keep-dropped" && v.state == ViewportState::Dropped),
        "unrelated Dropped viewport must remain unaffected: {vps:?}"
    );
    assert!(
        !repo.join(".loom/viewports/vp-missing").exists(),
        "failed spawn must clean up viewport directory"
    );
    assert!(
        repo.join(".loom/viewports/vp-keep-sealed").is_dir(),
        "unrelated Sealed viewport dir must remain on disk"
    );
    assert!(
        repo.join(".loom/viewports/vp-keep-dropped").is_dir(),
        "unrelated Dropped viewport dir must remain on disk"
    );

    let status_after_missing = run_loom(&repo, &["status", "--json"], 2000);
    assert!(status_after_missing.status.success());
    let st: DaemonStatus = serde_json::from_slice(&status_after_missing.stdout).unwrap();
    assert_eq!(st.active_viewports, 0);
    assert!(
        !st.viewports.iter().any(|v| v.name == "vp-missing"),
        "failed spawn must not leave a dangling entry in loom status: {:?}",
        st.viewports
    );

    // Re-spawning with the same name after failed spawn must succeed cleanly
    let respawn = run_loom(
        &repo,
        &["spawn", "-n", "vp-missing", "-m", "retry ok", "--json"],
        2000,
    );
    assert!(
        respawn.status.success(),
        "re-spawn after failed spawn failed: {}",
        String::from_utf8_lossy(&respawn.stderr)
    );
    let respawn_vp: ViewportSummary = serde_json::from_slice(&respawn.stdout).unwrap();
    assert_eq!(respawn_vp.name, "vp-missing");
    assert_eq!(respawn_vp.state, ViewportState::Active);
    assert!(repo.join(".loom/viewports/vp-missing").is_dir());

    // 2. Windows .cmd shim without extension on PATH must resolve and run
    #[cfg(windows)]
    {
        std::fs::write(
            bin_dir.join("loom_shim.cmd"),
            "@echo off\r\necho SHIM_OK %*\r\n",
        )
        .unwrap();
        let old_path = std::env::var_os("PATH").unwrap_or_default();
        let new_path = format!("{};{}", bin_dir.display(), old_path.to_string_lossy());
        let out = Command::new(loom_bin())
            .env("LOOM_IDLE_TIMEOUT_MS", "2000")
            .env("PATH", &new_path)
            .arg("--repo")
            .arg(&repo)
            .args([
                "spawn",
                "-n",
                "vp-shim",
                "-m",
                "shim",
                "--",
                "loom_shim",
                "hello world",
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "bare .cmd shim failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("SHIM_OK"), "stdout={stdout}");
    }

    shutdown_daemon_ipc(&repo);
}
