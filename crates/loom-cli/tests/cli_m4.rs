use loom_core::model::RadarLevel;
use loom_core::rpc::{
    RadarSnapshot, RpcMethod, RpcRequest, RpcResponse, ViewportSummary, read_frame, write_frame,
};
#[cfg(windows)]
use loom_core::rpc::{pipe_short_name, read_sock_descriptor};
#[cfg(unix)]
use loom_core::rpc::sock_descriptor_path;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
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
fn cli_radar_snapshot_json_human_and_watch_ndjson() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn val() -> i32 { 1 }\n").unwrap();

    assert!(run_loom(repo, &["init", "--json"], 5000).status.success());

    let sp1 = run_loom(repo, &["spawn", "-n", "v1", "-m", "one", "--json"], 5000);
    assert!(sp1.status.success());
    let vp1: ViewportSummary = serde_json::from_slice(&sp1.stdout).unwrap();

    let sp2 = run_loom(repo, &["spawn", "-n", "v2", "-m", "two", "--json"], 5000);
    assert!(sp2.status.success());
    let vp2: ViewportSummary = serde_json::from_slice(&sp2.stdout).unwrap();

    // 1. One-shot `loom radar --json`
    let json_out = run_loom(repo, &["radar", "--json"], 5000);
    assert!(json_out.status.success());
    let snap: RadarSnapshot = serde_json::from_slice(&json_out.stdout).unwrap();
    assert_eq!(snap.viewports.len(), 2);
    assert!(snap.viewports.iter().all(|v| v.level == RadarLevel::Green));

    // 2. One-shot human `loom radar`
    let human_out = run_loom(repo, &["radar"], 5000);
    assert!(human_out.status.success());
    let human_str = String::from_utf8_lossy(&human_out.stdout);
    assert!(human_str.contains("Speculative Merge Radar"));
    assert!(human_str.contains("GREEN"));

    // 3. Streaming `loom radar --watch --json` NDJSON
    let mut child = Command::new(loom_bin())
        .env("LOOM_IDLE_TIMEOUT_MS", "5000")
        .arg("--repo")
        .arg(repo)
        .args(["radar", "--watch", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn loom radar --watch --json");

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut first_line = String::new();
    reader.read_line(&mut first_line).unwrap();
    let init_snap: RadarSnapshot = serde_json::from_str(&first_line).unwrap();
    assert_eq!(init_snap.viewports.len(), 2);

    // Edit both viewports incompatibly -> NDJSON stream emits RED update
    std::fs::write(vp1.path.join("src/lib.rs"), "pub fn val() -> i32 { 10 }\n").unwrap();
    std::fs::write(vp2.path.join("src/lib.rs"), "pub fn val() -> i32 { 20 }\n").unwrap();

    let mut saw_red = false;
    for _ in 0..10 {
        let mut next_line = String::new();
        if reader.read_line(&mut next_line).unwrap() == 0 {
            break;
        }
        let upd: RadarSnapshot = serde_json::from_str(&next_line).unwrap();
        if upd.viewports.iter().any(|v| v.level == RadarLevel::Red) {
            saw_red = true;
            break;
        }
    }

    let _ = child.kill();
    let _ = child.wait();
    assert!(saw_red, "expected NDJSON stream to emit RED conflict");

    shutdown_daemon_ipc(repo);
}

#[test]
fn radar_watch_json_exits_cleanly_on_stdout_broken_pipe() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let init = run_loom(repo, &["init"], 15_000);
    assert!(init.status.success());

    let mut child = Command::new(loom_bin())
        .env("LOOM_IDLE_TIMEOUT_MS", "15000")
        .arg("--repo")
        .arg(repo)
        .args(["--json", "radar", "--watch"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut first_line = String::new();
    reader.read_line(&mut first_line).unwrap();
    assert!(!first_line.trim().is_empty());
    drop(reader); // Close pipe early

    let spawn = run_loom(repo, &["spawn", "-n", "v1", "-m", "trigger"], 15_000);
    assert!(spawn.status.success());

    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "loom radar --watch --json must exit cleanly on BrokenPipe, got {:?}, stderr={stderr}",
        output.status.code()
    );
    assert!(!stderr.contains("panicked"), "unexpected panic: {stderr}");

    shutdown_daemon_ipc(repo);
}
