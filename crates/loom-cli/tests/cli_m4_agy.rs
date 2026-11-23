use loom_core::rpc::{
    RpcMethod, RpcPayload, RpcRequest, RpcResponse, canonical_repo_root, read_frame, write_frame,
};
#[cfg(windows)]
use loom_core::rpc::{pipe_short_name, read_sock_descriptor};
#[cfg(unix)]
use loom_core::rpc::sock_descriptor_path;
use std::path::{Path, PathBuf};
use std::process::Command;
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

struct AgyMcpCleanupGuard {
    server_name: String,
}

impl Drop for AgyMcpCleanupGuard {
    fn drop(&mut self) {
        let _ = Command::new("agy")
            .args(["mcp", "remove", &self.server_name])
            .output();
    }
}

struct DaemonCleanupGuard {
    repo_root: PathBuf,
}

impl Drop for DaemonCleanupGuard {
    fn drop(&mut self) {
        shutdown_daemon_ipc(&self.repo_root);
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
    let sock = repo.join(".loom/daemon.sock");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && sock.exists() {
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn live_agy_cli_spawn_and_mcp_serve_integration() {
    // Verify `agy` binary is present on this host
    let ver = Command::new("agy")
        .arg("--version")
        .output()
        .expect("execute agy --version");
    assert!(
        ver.status.success(),
        "agy --version failed: {}",
        String::from_utf8_lossy(&ver.stderr)
    );

    let tmp = TempDir::new().unwrap();
    let repo = strip_verbatim(&canonical_repo_root(tmp.path()).unwrap());
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn greet() -> i32 { 1 }\n").unwrap();

    let _daemon_guard = DaemonCleanupGuard {
        repo_root: repo.clone(),
    };

    // 1. Initialize Loom repo & spawn a peer viewport
    let init_out = Command::new(loom_bin())
        .env("LOOM_IDLE_TIMEOUT_MS", "10000")
        .arg("--repo")
        .arg(&repo)
        .args(["init", "--json"])
        .output()
        .unwrap();
    assert!(
        init_out.status.success(),
        "loom init failed: {}",
        String::from_utf8_lossy(&init_out.stderr)
    );

    let peer_out = Command::new(loom_bin())
        .env("LOOM_IDLE_TIMEOUT_MS", "10000")
        .arg("--repo")
        .arg(&repo)
        .args(["spawn", "-n", "peer-vp", "-m", "peer smoke", "--json"])
        .output()
        .unwrap();
    assert!(
        peer_out.status.success(),
        "loom spawn peer-vp failed: {}",
        String::from_utf8_lossy(&peer_out.stderr)
    );

    // Seed a NegativeEdge on `greet` via IPC so preflight returns rich data
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let mut stream = {
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
                interprocess::local_socket::tokio::Stream::connect(ns_name)
                    .await
                    .unwrap()
            }
            #[cfg(unix)]
            {
                use interprocess::local_socket::{GenericFilePath, ToFsName, tokio::prelude::*};
                let sock_path = sock_descriptor_path(&repo);
                let fs_name = sock_path.as_path().to_fs_name::<GenericFilePath>().unwrap();
                interprocess::local_socket::tokio::Stream::connect(fs_name)
                    .await
                    .unwrap()
            }
        };
        write_frame(
            &mut stream,
            &RpcRequest {
                id: 1,
                method: RpcMethod::McpRecordDeadend {
                    viewport_id: "peer-vp".into(),
                    touched_symbols: vec!["greet".into()],
                    kind: "CompileError".into(),
                    lesson_summary: "keep greet() returning i32".into(),
                },
            },
        )
        .await
        .unwrap();
        let resp: RpcResponse = read_frame(&mut stream).await.unwrap().unwrap();
        assert!(matches!(resp.result, Ok(RpcPayload::McpRecordDeadend(_))));
    });

    // 2. Register temporary MCP server in `agy` with RAII Drop cleanup
    let unique_name = format!("loom_m4_test_{}", std::process::id());
    let _agy_guard = AgyMcpCleanupGuard {
        server_name: unique_name.clone(),
    };

    let loom_exe = strip_verbatim(Path::new(loom_bin()));
    let env_repo = format!("LOOM_REPO_ROOT={}", repo.display());
    let add_out = Command::new("agy")
        .args([
            "mcp",
            "add",
            "-t",
            "stdio",
            "-e",
            &env_repo,
            "-e",
            "LOOM_VIEWPORT=agy-agent",
            &unique_name,
            "--",
            &loom_exe.to_string_lossy(),
            "--repo",
            &repo.to_string_lossy(),
            "mcp",
            "serve",
            "--viewport",
            "agy-agent",
        ])
        .output()
        .expect("execute agy mcp add");
    assert!(
        add_out.status.success(),
        "agy mcp add failed: stdout={} stderr={}",
        String::from_utf8_lossy(&add_out.stdout),
        String::from_utf8_lossy(&add_out.stderr)
    );

    let list_out = Command::new("agy").args(["mcp", "list"]).output().unwrap();
    let list_text = format!(
        "{}{}",
        String::from_utf8_lossy(&list_out.stdout),
        String::from_utf8_lossy(&list_out.stderr)
    );
    assert!(
        list_text.contains(&unique_name),
        "agy mcp list should show {unique_name}, got: {list_text}"
    );

    // 3. Run `loom spawn -n agy-agent ... -- agy -p ...`
    let spawn_out = Command::new(loom_bin())
        .env("LOOM_IDLE_TIMEOUT_MS", "10000")
        .arg("--repo")
        .arg(&repo)
        .args([
            "spawn",
            "-n",
            "agy-agent",
            "-m",
            "agy mcp smoke",
            "--",
            "agy",
            "-p",
            "Call the MCP tool loom_radar_poll and loom_preflight_symbols with symbols [\"greet\"], then respond with a concise JSON object summarizing the status and negative warning.",
            "--dangerously-skip-permissions",
            "--disable-slash-commands",
            "--output-format",
            "json",
            "--print-timeout",
            "60s",
            "--add-dir",
            &repo.to_string_lossy(),
        ])
        .output()
        .expect("execute loom spawn -- agy");

    let stdout_str = String::from_utf8_lossy(&spawn_out.stdout);
    let stderr_str = String::from_utf8_lossy(&spawn_out.stderr);
    assert!(
        spawn_out.status.success(),
        "loom spawn -- agy failed (code {:?}):\nstdout: {stdout_str}\nstderr: {stderr_str}",
        spawn_out.status.code()
    );
    assert!(
        !stdout_str.trim().is_empty(),
        "expected non-empty JSON output from agy; stderr: {stderr_str}"
    );
    let parsed_json: serde_json::Value = serde_json::from_str(stdout_str.trim())
        .unwrap_or_else(|e| panic!("agy output was not valid JSON ({e}): {stdout_str}"));
    assert!(parsed_json.is_object() || parsed_json.is_array());

    // 4. Explicitly drop guard, daemon, and temp repo, then allow watchdog to exit cleanly
    drop(_agy_guard);
    let list_after = Command::new("agy").args(["mcp", "list"]).output().unwrap();
    let after_text = format!(
        "{}{}",
        String::from_utf8_lossy(&list_after.stdout),
        String::from_utf8_lossy(&list_after.stderr)
    );
    assert!(
        !after_text.contains(&unique_name),
        "agy mcp remove did not clean up {unique_name}: {after_text}"
    );
    drop(_daemon_guard);
    drop(tmp);
    std::thread::sleep(Duration::from_millis(350));
}
