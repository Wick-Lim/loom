use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::io::{BufReader, Cursor};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use e2e_suite::{
    read_jsonrpc_frame, resolve_agy_bin_with, resolve_loom_bin_with, wait_for_radar_status,
    write_jsonrpc_frame, HarnessError, McpClient, McpFraming, TestRepo,
};

#[test]
fn tier0_loom_bin_resolution_precedence() {
    let target_dir = Path::new("/repo/target");
    let exe = if cfg!(windows) { "loom.exe" } else { "loom" };
    let debug_bin = target_dir.join("debug").join(exe);
    let release_bin = target_dir.join("release").join(exe);
    let path_dir = Path::new("/custom/bin");
    let path_bin = path_dir.join(exe);
    let path_env = std::env::join_paths([path_dir]).unwrap();

    // 1. LOOM_BIN wins over everything
    let res = resolve_loom_bin_with(
        Some("/override/loom"),
        target_dir,
        Some(&path_env),
        |_| true,
    );
    assert_eq!(res, PathBuf::from("/override/loom"));

    // 2. target/debug wins over target/release & PATH
    let res = resolve_loom_bin_with(None, target_dir, Some(&path_env), |p| {
        p == debug_bin || p == release_bin || p == path_bin
    });
    assert_eq!(res, debug_bin);

    // 3. target/release wins over PATH when debug absent
    let res = resolve_loom_bin_with(None, target_dir, Some(&path_env), |p| {
        p == release_bin || p == path_bin
    });
    assert_eq!(res, release_bin);

    // 4. PATH wins when debug & release absent
    let res = resolve_loom_bin_with(None, target_dir, Some(&path_env), |p| p == path_bin);
    assert_eq!(res, path_bin);

    // 5. Fallback executable name when nothing exists
    let res = resolve_loom_bin_with(None, target_dir, Some(&path_env), |_| false);
    assert_eq!(res, PathBuf::from(exe));
}

#[test]
fn tier0_agy_bin_resolution_precedence() {
    let win_default = Path::new(r"C:\Users\ijh69\AppData\Local\agy\bin\agy.exe");
    let path_dir = Path::new(r"C:\tools\bin");
    let path_agy = path_dir.join(if cfg!(windows) { "agy.exe" } else { "agy" });
    let path_env: OsString = std::env::join_paths([path_dir]).unwrap();

    // 1. AGY_BIN overrides everything
    let res = resolve_agy_bin_with(
        Some(r"C:\custom\agy.exe"),
        Some(win_default),
        Some(&path_env),
        |_| true,
    );
    assert_eq!(res, Some(PathBuf::from(r"C:\custom\agy.exe")));

    // 2. Default Windows path beats PATH
    let res = resolve_agy_bin_with(None, Some(win_default), Some(&path_env), |p| {
        p == win_default || p == path_agy
    });
    assert_eq!(res, Some(win_default.to_path_buf()));

    // 3. PATH beats None
    let res = resolve_agy_bin_with(None, Some(win_default), Some(&path_env), |p| p == path_agy);
    assert_eq!(res, Some(path_agy));

    // 4. None when absent
    let res = resolve_agy_bin_with(None, Some(win_default), Some(&path_env), |_| false);
    assert_eq!(res, None);
}

#[test]
fn tier0_testrepo_file_ops_and_path_traversal_guard() {
    let repo = TestRepo::new().expect("TestRepo::new");
    assert!(repo.root().is_dir());

    repo.write_file("src/nested/mod.rs", "pub fn ok() -> i32 { 42 }\n")
        .unwrap();
    assert!(repo.exists("src/nested/mod.rs"));
    assert_eq!(
        repo.read_file("src/nested/mod.rs").unwrap(),
        "pub fn ok() -> i32 { 42 }\n"
    );

    let bin = [0u8, 0xFF, 0x28, 0xB5, 0x2F, 0xFD, 1, 2, 3];
    repo.write_bytes("assets/blob.bin", &bin).unwrap();
    assert_eq!(repo.read_bytes("assets/blob.bin").unwrap(), bin);

    assert!(matches!(
        repo.path("../escape.txt"),
        Err(HarnessError::InvalidPath(_))
    ));
    let abs = if cfg!(windows) {
        r"C:\Windows\System32\drivers\etc\hosts"
    } else {
        "/etc/passwd"
    };
    assert!(matches!(
        repo.path(abs),
        Err(HarnessError::InvalidPath(_))
    ));
}

#[test]
fn tier0_init_git_repo_and_bare_remote() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.init_git_repo().expect("init_git_repo");

    let branch = repo.git(&["rev-parse", "--abbrev-ref", "HEAD"]);
    branch.assert_success();
    assert_eq!(branch.stdout_str().trim(), "main");

    let status = repo.git(&["status", "--porcelain"]);
    status.assert_success();
    assert!(status.stdout_str().trim().is_empty());

    let bare = repo.init_bare_remote().expect("init_bare_remote");
    assert!(bare.is_dir());
    // Bare remote must NOT live inside repo.root()
    assert!(!bare.starts_with(repo.root()));

    let bare_str = bare.to_string_lossy().into_owned();
    repo.git(&["remote", "add", "origin", &bare_str])
        .assert_success();
    repo.git(&["push", "-u", "origin", "main"])
        .assert_success();

    let remote_log = repo.git_in(&bare, &["log", "--oneline", "main"]);
    remote_log.assert_success();
    assert!(remote_log.stdout_str().contains("initial commit"));
}

#[test]
fn tier0_jsonrpc_dual_framing_roundtrip() {
    let mut buf = Vec::new();
    let msg1 = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"});
    let msg2 = json!({"jsonrpc": "2.0", "method": "notifications/loom/radar_conflict", "params": {"symbol": "한글_심볼"}});

    write_jsonrpc_frame(&mut buf, McpFraming::NewlineDelimited, &msg1).unwrap();
    write_jsonrpc_frame(&mut buf, McpFraming::ContentLength, &msg2).unwrap();

    // Also prepend an extra header before Content-Length for a 3rd message
    let msg3 = json!({"jsonrpc": "2.0", "id": 2, "result": {"ok": true}});
    let msg3_bytes = serde_json::to_vec(&msg3).unwrap();
    buf.extend_from_slice(
        format!(
            "Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: {}\r\n\r\n",
            msg3_bytes.len()
        )
        .as_bytes(),
    );
    buf.extend_from_slice(&msg3_bytes);

    let mut cursor = Cursor::new(buf);
    let (f1, v1) = read_jsonrpc_frame(&mut cursor).unwrap().unwrap();
    assert_eq!(f1, McpFraming::NewlineDelimited);
    assert_eq!(v1, msg1);

    let (f2, v2) = read_jsonrpc_frame(&mut cursor).unwrap().unwrap();
    assert_eq!(f2, McpFraming::ContentLength);
    assert_eq!(v2, msg2);

    let (f3, v3) = read_jsonrpc_frame(&mut cursor).unwrap().unwrap();
    assert_eq!(f3, McpFraming::ContentLength);
    assert_eq!(v3, msg3);

    assert!(read_jsonrpc_frame(&mut cursor).unwrap().is_none());
}

#[test]
fn tier0_mcp_client_interleaved_notifications_and_timeout() {
    for framing in [McpFraming::NewlineDelimited, McpFraming::ContentLength] {
        let (c2s_read, c2s_write) = std::io::pipe().unwrap();
        let (s2c_read, mut s2c_write) = std::io::pipe().unwrap();

        let server = std::thread::spawn(move || {
            let mut reader = BufReader::new(c2s_read);
            while let Ok(Some((_kind, msg))) = read_jsonrpc_frame(&mut reader) {
                let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
                let id = msg.get("id").cloned();
                match method {
                    "initialize" => {
                        let resp = json!({
                            "jsonrpc": "2.0",
                            "id": id.unwrap(),
                            "result": {"protocolVersion": "2024-11-05", "capabilities": {}}
                        });
                        write_jsonrpc_frame(&mut s2c_write, framing, &resp).unwrap();
                    }
                    "notifications/initialized" => {}
                    "tools/list" => {
                        let resp = json!({
                            "jsonrpc": "2.0",
                            "id": id.unwrap(),
                            "result": {"tools": [{"name": "loom_radar_poll"}]}
                        });
                        write_jsonrpc_frame(&mut s2c_write, framing, &resp).unwrap();
                    }
                    "tools/call" => {
                        // Send interleaved push notification BEFORE response
                        let notif = json!({
                            "jsonrpc": "2.0",
                            "method": "notifications/loom/radar_conflict",
                            "params": {"severity": "RED", "symbol": "foo"}
                        });
                        write_jsonrpc_frame(&mut s2c_write, framing, &notif).unwrap();

                        let resp = json!({
                            "jsonrpc": "2.0",
                            "id": id.unwrap(),
                            "result": {"content": [{"type": "text", "text": "ok"}]}
                        });
                        write_jsonrpc_frame(&mut s2c_write, framing, &resp).unwrap();
                    }
                    _ => break,
                }
            }
        });

        let mut client = McpClient::from_reader_writer(
            Box::new(BufReader::new(s2c_read)),
            Box::new(c2s_write),
            framing,
        )
        .with_timeout(Duration::from_secs(3));

        let init = client.handshake().unwrap();
        assert_eq!(init["protocolVersion"], "2024-11-05");

        let tools = client.tools_list().unwrap();
        assert_eq!(tools["tools"][0]["name"], "loom_radar_poll");

        // Interleaved notification arrives during tools_call and must be buffered
        let call = client.tools_call("loom_radar_poll", json!({})).unwrap();
        assert_eq!(call["content"][0]["text"], "ok");

        let notif = client
            .read_until_notification("notifications/loom/radar_conflict", Duration::from_secs(1))
            .unwrap();
        assert_eq!(notif["params"]["severity"], "RED");

        // Short timeout when no further notifications arrive
        let err = client
            .read_notification_matching(Duration::from_millis(80), |_| true)
            .unwrap_err();
        assert!(matches!(err, HarnessError::Timeout { .. }));

        drop(client);
        server.join().unwrap();
    }
}

#[test]
fn tier0_wait_for_radar_status_success_and_timeout() {
    let repo = TestRepo::new().expect("TestRepo::new");
    let vp = repo.viewport_dir("vp1");
    let loom_dir = vp.join(".loom");
    fs::create_dir_all(&loom_dir).unwrap();

    // Spawn writer simulating atomic temp + rename updates
    let loom_dir_clone = loom_dir.clone();
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(40));
        let tmp = loom_dir_clone.join("RADAR_STATUS.json.tmp");
        let target = loom_dir_clone.join("RADAR_STATUS.json");
        fs::write(&tmp, r#"{"status":"YELLOW"}"#).unwrap();
        fs::rename(&tmp, &target).unwrap();

        std::thread::sleep(Duration::from_millis(60));
        fs::write(&tmp, r#"{"status":"RED","symbol":"alpha"}"#).unwrap();
        fs::rename(&tmp, &target).unwrap();
    });

    let val = wait_for_radar_status(&vp, Duration::from_secs(2), |v| {
        v.get("status").and_then(Value::as_str) == Some("RED")
    })
    .unwrap();
    assert_eq!(val["symbol"], "alpha");
    writer.join().unwrap();

    let err = wait_for_radar_status(&vp, Duration::from_millis(80), |v| {
        v.get("status").and_then(Value::as_str) == Some("GREEN")
    })
    .unwrap_err();
    assert!(matches!(err, HarnessError::Timeout { .. }));
    let msg = err.to_string();
    assert!(msg.contains("RED"));
}

#[cfg(windows)]
#[test]
fn tier0_drop_cleans_up_tempdir_and_daemon_sock_pid() {
    let repo = TestRepo::new().expect("TestRepo::new");
    let root = repo.root().to_path_buf();
    let loom_dir = root.join(".loom");
    fs::create_dir_all(&loom_dir).unwrap();
    let locked_file = loom_dir.join("locked.tmp");

    // Spawn a real child process holding an open write handle on .loom/locked.tmp
    let script = format!(
        "$fs = [System.IO.File]::Open('{}', [System.IO.FileMode]::OpenOrCreate, [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::None); Start-Sleep -Seconds 30",
        locked_file.display()
    );
    let mut child = Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn powershell lock holder");

    // Wait briefly until locked.tmp is created and held by the child
    let start = std::time::Instant::now();
    while !locked_file.exists() && start.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(locked_file.exists(), "child failed to create locked.tmp");

    let pid = child.id();
    let descriptor = json!({
        "transport": "named_pipe",
        "endpoint": r"\\.\pipe\loom-tier0-test",
        "pid": pid,
        "protocol_version": 1
    });
    fs::write(
        loom_dir.join("daemon.sock"),
        serde_json::to_vec(&descriptor).unwrap(),
    )
    .unwrap();

    // Drop TestRepo without manually killing child — TestRepo::drop must terminate PID and remove TempDir
    drop(repo);
    let _ = child.wait();

    assert!(
        !root.exists(),
        "TestRepo::drop failed to remove locked TempDir at {}",
        root.display()
    );
}

#[test]
fn tier0_loom_bin_and_agy_bin_smoke_resolvable() {
    // Ensure public functions compile, run cleanly, and never panic
    let loom = e2e_suite::loom_bin();
    assert!(!loom.as_os_str().is_empty());

    let agy = e2e_suite::agy_bin();
    if let Some(p) = agy {
        assert!(p.is_file());
    }

    let mut seen = HashSet::new();
    for _ in 0..3 {
        let r = TestRepo::new().unwrap();
        assert!(seen.insert(r.root().to_path_buf()));
    }
}

#[test]
fn tier0_mcp_client_drop_before_pipe_writer_does_not_deadlock() {
    let (s2c_read, s2c_write) = std::io::pipe().unwrap();
    let (_c2s_read, c2s_write) = std::io::pipe().unwrap();

    let client = McpClient::from_reader_writer(
        Box::new(BufReader::new(s2c_read)),
        Box::new(c2s_write),
        McpFraming::NewlineDelimited,
    );

    // Drop client BEFORE s2c_write — must return quickly rather than deadlocking on reader_thread.join()
    let start = Instant::now();
    drop(client);
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(500),
        "McpClient::drop took too long ({elapsed:?}) with open pipe writer"
    );
    drop(s2c_write);
}

#[cfg(windows)]
#[test]
fn tier0_drop_kills_dual_sock_and_pid_files() {
    let repo = TestRepo::new().expect("TestRepo::new");
    let root = repo.root().to_path_buf();
    let loom_dir = root.join(".loom");
    fs::create_dir_all(&loom_dir).unwrap();

    let lock1 = loom_dir.join("dual_lock1.tmp");
    let lock2 = loom_dir.join("dual_lock2.tmp");

    let spawn_locker = |lock_path: &Path| {
        let script = format!(
            "$fs = [System.IO.File]::Open('{}', [System.IO.FileMode]::OpenOrCreate, [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::None); Start-Sleep -Seconds 30",
            lock_path.display()
        );
        Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn powershell lock holder")
    };

    let c1 = spawn_locker(&lock1);
    let c2 = spawn_locker(&lock2);

    let start = Instant::now();
    while (!lock1.exists() || !lock2.exists()) && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(lock1.exists() && lock2.exists(), "lock holders failed to create lock files");

    let descriptor = json!({
        "transport": "named_pipe",
        "endpoint": r"\\.\pipe\loom-tier0-dual",
        "pid": c1.id(),
        "protocol_version": 1
    });
    fs::write(
        loom_dir.join("daemon.sock"),
        serde_json::to_vec(&descriptor).unwrap(),
    )
    .unwrap();
    fs::write(loom_dir.join("daemon.pid"), c2.id().to_string()).unwrap();

    drop(repo);
    for mut c in [c1, c2] {
        let mut exited = false;
        for _ in 0..20 {
            if matches!(c.try_wait(), Ok(Some(_))) {
                exited = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        if !exited {
            let _ = c.kill();
        }
        let _ = c.wait();
        assert!(exited, "child process was not terminated by TestRepo::drop");
    }

    assert!(
        !root.exists(),
        "TestRepo::drop failed to kill both daemon.sock and daemon.pid processes at {}",
        root.display()
    );
}

#[cfg(windows)]
#[test]
fn tier0_loom_in_nested_subdir_kills_ancestor_loomd_on_drop() {
    let repo1 = TestRepo::new().expect("TestRepo::new");
    let second_repo = tempfile::Builder::new()
        .prefix("loom-e2e-ancestor-")
        .tempdir()
        .expect("second tempdir");
    let second_root = second_repo.path().to_path_buf();
    let second_loom = second_root.join(".loom");
    let nested_subdir = second_root.join("packages").join("nested").join("sub");
    fs::create_dir_all(&second_loom).unwrap();
    fs::create_dir_all(&nested_subdir).unwrap();

    let lock_file = second_loom.join("ancestor_lock.tmp");
    let script = format!(
        "$fs = [System.IO.File]::Open('{}', [System.IO.FileMode]::OpenOrCreate, [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::None); Start-Sleep -Seconds 30",
        lock_file.display()
    );
    let mut child = Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn powershell lock holder");

    let start = Instant::now();
    while !lock_file.exists() && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(lock_file.exists(), "lock holder failed to create ancestor_lock.tmp");

    let descriptor = json!({
        "transport": "named_pipe",
        "endpoint": r"\\.\pipe\loom-tier0-ancestor",
        "pid": child.id(),
        "protocol_version": 1
    });
    fs::write(
        second_loom.join("daemon.sock"),
        serde_json::to_vec(&descriptor).unwrap(),
    )
    .unwrap();

    // Even if loom binary doesn't exist yet, try_loom_in_with_env records nested_subdir and its .loom ancestor
    let _ = repo1.try_loom_in_with_env(&nested_subdir, &["--version"], &[]);

    drop(repo1);
    let mut exited = false;
    for _ in 0..20 {
        if matches!(child.try_wait(), Ok(Some(_))) {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    if !exited {
        let _ = child.kill();
    }
    let _ = child.wait();
    assert!(exited, "ancestor daemon was not terminated by TestRepo::drop");

    // Must now be unlocked and cleanly removable
    fs::remove_file(&lock_file).expect("ancestor lock holder should have been terminated on drop");
    second_repo.close().expect("second_repo tempdir should remove cleanly");
}
