use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use e2e_suite::{HarnessError, McpClient, TestRepo};

fn is_pid_running(pid: u32) -> bool {
    #[cfg(windows)]
    {
        let output = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output();
        match output {
            Ok(out) if out.status.success() => {
                let text = String::from_utf8_lossy(&out.stdout);
                let pid_token = format!("\"{pid}\"");
                text.lines().any(|line| line.contains(&pid_token))
            }
            _ => false,
        }
    }
    #[cfg(not(windows))]
    {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

fn force_kill_pid(pid: u32) {
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status();
    }
    #[cfg(not(windows))]
    {
        let _ = Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status();
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !is_pid_running(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !is_pid_running(pid),
        "process {pid} did not exit within 5s after kill"
    );
}

fn collect_files_recursive(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if !dir.is_dir() {
        return files;
    }
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = fs::read_dir(&current)
            .unwrap_or_else(|e| panic!("failed to read dir {}: {e}", current.display()));
        for entry in entries {
            let entry = entry.expect("failed to read dir entry");
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

fn collect_blob_sizes(dir: &Path) -> BTreeMap<PathBuf, u64> {
    let mut map = BTreeMap::new();
    for path in collect_files_recursive(dir) {
        let len = fs::metadata(&path)
            .unwrap_or_else(|e| panic!("failed to stat {}: {e}", path.display()))
            .len();
        map.insert(path, len);
    }
    map
}

fn total_dir_bytes(dir: &Path) -> u64 {
    collect_blob_sizes(dir).values().copied().sum()
}

fn is_idle_timeout_key(key: &str) -> bool {
    let lower = key.to_lowercase();
    lower.contains("idle")
        && (lower.contains("timeout") || lower.contains("sec") || lower.contains("shutdown"))
}

fn find_idle_timeout_value(val: &toml::Value) -> Option<(String, i64)> {
    match val {
        toml::Value::Table(tbl) => {
            for (k, v) in tbl {
                if is_idle_timeout_key(k) {
                    if let Some(n) = v.as_integer() {
                        return Some((k.clone(), n));
                    }
                }
            }
            for v in tbl.values() {
                if let Some(found) = find_idle_timeout_value(v) {
                    return Some(found);
                }
            }
            None
        }
        toml::Value::Array(arr) => {
            for v in arr {
                if let Some(found) = find_idle_timeout_value(v) {
                    return Some(found);
                }
            }
            None
        }
        _ => None,
    }
}

fn update_idle_timeout_in_value(val: &mut toml::Value, secs: i64) -> bool {
    match val {
        toml::Value::Table(tbl) => {
            for (k, v) in tbl.iter_mut() {
                if is_idle_timeout_key(k) && v.is_integer() {
                    let target = if k.to_lowercase().ends_with("_ms") {
                        secs * 1000
                    } else {
                        secs
                    };
                    *v = toml::Value::Integer(target);
                    return true;
                }
            }
            for (_, v) in tbl.iter_mut() {
                if update_idle_timeout_in_value(v, secs) {
                    return true;
                }
            }
            false
        }
        toml::Value::Array(arr) => {
            for v in arr.iter_mut() {
                if update_idle_timeout_in_value(v, secs) {
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}

fn set_idle_timeout_secs(repo: &TestRepo, secs: i64) {
    let raw = repo.read_file(".loom/config.toml").unwrap();
    let mut parsed: toml::Value =
        toml::from_str(&raw).expect(".loom/config.toml should parse as TOML");
    assert!(
        update_idle_timeout_in_value(&mut parsed, secs),
        "failed to locate idle timeout setting in .loom/config.toml:\n{raw}"
    );
    let updated = toml::to_string_pretty(&parsed).expect("serialize updated config.toml");
    repo.write_file(".loom/config.toml", &updated).unwrap();
}

fn mcp_handshake(client: &mut McpClient) -> Value {
    let val = match client.handshake() {
        Ok(v) => v,
        Err(HarnessError::JsonRpc { .. }) => {
            let v = client
                .initialize_with_version("2025-03-26")
                .expect("MCP initialize_with_version(2025-03-26) should succeed");
            client
                .send_initialized()
                .expect("MCP send_initialized should succeed");
            v
        }
        Err(err) => panic!("MCP handshake failed unexpectedly: {err}"),
    };
    assert!(
        val.get("protocolVersion")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty()),
        "MCP initialize result missing protocolVersion: {val}"
    );
    val
}

fn mcp_tool_text(call_res: &Value) -> String {
    assert_ne!(
        call_res.get("isError").and_then(Value::as_bool),
        Some(true),
        "MCP tool returned isError=true: {call_res}"
    );
    let mut out = String::new();
    if let Some(structured) = call_res.get("structuredContent") {
        out.push_str(&structured.to_string());
        out.push('\n');
    }
    if let Some(items) = call_res.get("content").and_then(Value::as_array) {
        for item in items {
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                out.push_str(text);
                out.push('\n');
            }
        }
    }
    out
}

fn generate_varied_rust_payload(seed_init: u64, count: usize) -> String {
    let mut state = seed_init;
    let mut out = String::new();
    for i in 0..count {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.push_str(&format!(
            "pub fn dup_payload_fn_{i}(x: u64) -> u64 {{ x.wrapping_mul(0x{state:016x}) ^ {i} }}\n"
        ));
    }
    out
}

// ============================================================================
// Feature F1: Cargo Workspace & Single Static Binary (5 tests)
// ============================================================================

#[test]
fn tier1_f01_help_and_version_output() {
    let repo = TestRepo::new().unwrap();

    let help = repo.loom(&["--help"]);
    help.assert_success();
    let help_text = help.stdout_str();
    let help_lower = help_text.to_lowercase();
    assert!(!help_text.trim().is_empty(), "--help stdout was empty");
    for cmd in ["init", "list", "status", "log", "undo", "gc"] {
        assert!(
            help_lower.contains(cmd),
            "--help output missing subcommand {cmd:?}: {help_text}"
        );
    }

    let ver = repo.loom(&["--version"]);
    ver.assert_success();
    let ver_text = ver.stdout_str();
    assert!(!ver_text.trim().is_empty(), "--version stdout was empty");
    assert!(
        ver_text.to_lowercase().contains("loom") || ver_text.chars().any(|c| c.is_ascii_digit()),
        "--version output unexpected: {ver_text}"
    );
}

#[test]
fn tier1_f01_static_binary_no_projfs_dll() {
    let bin = e2e_suite::loom_bin();
    assert!(
        bin.is_file(),
        "expected compiled loom binary at {}",
        bin.display()
    );

    let repo = TestRepo::new().unwrap();
    for args in [&["--version"][..], &["init"][..], &["status"][..]] {
        let out = repo.loom(args);
        out.assert_success();
        assert_eq!(out.exit_code(), Some(0));
        // 0xC0000135 == STATUS_DLL_NOT_FOUND (-1073741515i32)
        assert_ne!(out.exit_code(), Some(-1073741515_i32));
    }

    #[cfg(windows)]
    {
        let projfs_dll = Path::new(r"C:\Windows\System32\ProjectedFSLib.dll");
        if !projfs_dll.exists() {
            assert!(repo.exists(".loom/state.redb"));
        }
    }
}

#[test]
fn tier1_f01_subcommand_help_matrix() {
    let repo = TestRepo::new().unwrap();
    let subcommands = ["init", "list", "status", "log", "undo", "gc"];
    for sub in subcommands {
        let out = repo.loom(&[sub, "--help"]);
        out.assert_success();
        let text = out.stdout_str();
        let lower = text.to_lowercase();
        assert!(
            !text.trim().is_empty() && (lower.contains(sub) || lower.contains("usage")),
            "help output for `{sub}` was unexpected: {text}"
        );
    }
}

#[test]
fn tier1_f01_auto_spawn_daemon_via_exe() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();
    repo.loom(&["status"]).assert_success();

    let pid = repo
        .daemon_pid()
        .expect("daemon PID should be recorded after auto-spawn");
    assert!(pid > 0);
    assert!(repo.exists(".loom/daemon.sock"));
    assert!(repo.exists(".loom/daemon.lock"));
    assert!(
        is_pid_running(pid),
        "auto-spawned daemon pid {pid} should be running"
    );
}

#[test]
fn tier1_f01_run_from_nested_subdirectory() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();

    let nested = repo.path("packages/core/src/deep").unwrap();
    fs::create_dir_all(&nested).unwrap();

    repo.loom_in(&nested, &["status"]).assert_success();
    repo.loom_in(&nested, &["list"]).assert_success();

    assert!(
        !nested.join(".loom").exists(),
        "nested subdir should not create a separate .loom/"
    );
    assert!(repo.exists(".loom/state.redb"));
    assert!(repo.daemon_pid().is_some());
}

// ============================================================================
// Feature F2: .loom/ Layout & Config (5 tests)
// ============================================================================

#[test]
fn tier1_f02_init_creates_expected_layout() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();
    repo.loom(&["status"]).assert_success();

    assert!(repo.path(".loom").unwrap().is_dir());
    assert!(repo.path(".loom/config.toml").unwrap().is_file());
    assert!(repo.path(".loom/wal").unwrap().is_dir());
    assert!(repo.path(".loom/cas").unwrap().is_dir());
    assert!(repo.path(".loom/state.redb").unwrap().is_file());
    assert!(repo.path(".loom/viewports").unwrap().is_dir());
    assert!(repo.path(".loom/daemon.sock").unwrap().is_file());
    assert!(repo.path(".loom/daemon.lock").unwrap().is_file());
}

#[test]
fn tier1_f02_init_colocate_git_preserves_git() {
    let repo = TestRepo::new().unwrap();
    repo.write_file("src/lib.rs", "pub fn ok() -> i32 { 1 }\n")
        .unwrap();
    repo.init_git_repo().unwrap();

    let head_before = repo
        .git(&["rev-parse", "HEAD"])
        .assert_success()
        .stdout_str();

    repo.loom(&["init", "--colocate-git"]).assert_success();

    assert!(repo.exists(".git"));
    assert!(repo.exists(".loom"));
    let head_after = repo
        .git(&["rev-parse", "HEAD"])
        .assert_success()
        .stdout_str();
    assert_eq!(head_after, head_before);
    repo.git(&["fsck", "--no-dangling"]).assert_success();

    let porcelain = repo
        .git(&["status", "--porcelain"])
        .assert_success()
        .stdout_str();
    assert!(
        !porcelain.contains(".loom/"),
        "git status should not list .loom/: {porcelain}"
    );
}

#[test]
fn tier1_f02_config_toml_valid_defaults() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();

    let raw = repo.read_file(".loom/config.toml").unwrap();
    let parsed: toml::Value =
        toml::from_str(&raw).expect(".loom/config.toml should be valid TOML");
    assert!(parsed.is_table(), "config.toml root must be a TOML table");
    assert!(
        !parsed.as_table().unwrap().is_empty(),
        "config.toml must not be empty"
    );

    let (key, val) = find_idle_timeout_value(&parsed)
        .unwrap_or_else(|| panic!("expected idle timeout key in config.toml:\n{raw}"));
    assert!(
        val > 0,
        "default idle timeout `{key}` must be positive, got {val}"
    );
}

#[test]
fn tier1_f02_config_toml_custom_override() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();
    let status_before = repo.loom(&["status"]).assert_success().stdout_str();

    set_idle_timeout_secs(&repo, 45);
    if let Some(pid) = repo.daemon_pid() {
        force_kill_pid(pid);
    }

    let status_after = repo.loom(&["status"]).assert_success().stdout_str();
    assert_ne!(
        status_after, status_before,
        "loom status should reflect custom idle timeout override"
    );
    assert!(
        status_after.contains("45000")
            || status_after.contains("45 s")
            || status_after.contains("45s")
            || status_after.contains("45 sec"),
        "expected overridden idle timeout (45000 ms or 45 s) in `loom status`, got:\n{status_after}"
    );
    assert!(
        !status_after.contains("1800000"),
        "loom status still reported default 1800000 ms timeout after override:\n{status_after}"
    );

    let raw_after = repo.read_file(".loom/config.toml").unwrap();
    let parsed_after: toml::Value =
        toml::from_str(&raw_after).expect("modified config.toml must remain valid TOML");
    let (key, val) = find_idle_timeout_value(&parsed_after)
        .expect("idle timeout key missing after daemon start");
    let expected = if key.to_lowercase().ends_with("_ms") {
        45_000
    } else {
        45
    };
    assert_eq!(val, expected);
}

#[test]
fn tier1_f02_gitignore_excludes_loom_dir() {
    let repo = TestRepo::new().unwrap();
    repo.write_file(".gitignore", "target/\n").unwrap();
    repo.init_git_repo().unwrap();

    repo.loom(&["init", "--colocate-git"]).assert_success();
    repo.loom(&["status"]).assert_success();

    let gitignore = repo.read_file(".gitignore").unwrap();
    assert!(
        gitignore.contains("target/"),
        ".gitignore should preserve target/: {gitignore}"
    );

    let exclude_candidates = [".gitignore", ".git/info/exclude", ".loom/.gitignore"];
    let excluded_in_file = exclude_candidates.iter().any(|rel| {
        repo.exists(rel)
            && repo
                .read_file(rel)
                .map(|content| content.contains(".loom"))
                .unwrap_or(false)
    });
    assert!(
        excluded_in_file,
        "expected .loom exclusion in one of .gitignore, .git/info/exclude, or .loom/.gitignore"
    );

    repo.git(&["check-ignore", "-q", ".loom/"]).assert_success();

    let porcelain = repo
        .git(&["status", "--porcelain"])
        .assert_success()
        .stdout_str();
    assert!(
        !porcelain.contains(".loom/"),
        "git status should ignore .loom/: {porcelain}"
    );
}

// ============================================================================
// Feature F3: state.redb 8-Table Schema (5 tests)
// ============================================================================

#[test]
fn tier1_f03_redb_file_created_on_init() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();

    let db = repo.path(".loom/state.redb").unwrap();
    assert!(db.is_file());
    assert!(fs::metadata(&db).unwrap().len() > 0);
}

#[test]
fn tier1_f03_redb_persists_across_daemon_restart() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();
    repo.loom(&["spawn", "-n", "vp-persist", "-m", "survive restart"])
        .assert_success();

    let pid1 = repo.daemon_pid().expect("daemon PID before kill");
    force_kill_pid(pid1);

    let list_out = repo.loom(&["list"]).assert_success().stdout_str();
    assert!(
        list_out.contains("vp-persist"),
        "vp-persist missing after restart: {list_out}"
    );

    let pid2 = repo.daemon_pid().expect("daemon PID after restart");
    assert_ne!(pid1, pid2);
}

#[test]
fn tier1_f03_redb_symbol_index_persistence() {
    let repo = TestRepo::new().unwrap();
    repo.write_file("src/math.rs", "pub fn alpha_base() -> i32 { 1 }\n")
        .unwrap();
    repo.loom(&["init"]).assert_success();

    repo.loom(&["spawn", "-n", "vp-sym", "-m", "add symbol"])
        .assert_success();
    let vp = repo.viewport_dir("vp-sym");
    fs::write(
        vp.join("src/math.rs"),
        "pub fn alpha_base() -> i32 { 1 }\npub fn unique_persist_sym_42() -> i32 { 42 }\n",
    )
    .unwrap();
    repo.loom_in(&vp, &["seal", "-m", "seal sym"])
        .assert_success();

    let pid1 = repo.daemon_pid().expect("daemon PID before kill");
    force_kill_pid(pid1);

    let log_out = repo
        .loom(&["log", "--symbol", "unique_persist_sym_42"])
        .assert_success()
        .stdout_str();
    assert!(
        log_out.contains("seal sym"),
        "symbol index query did not return persisted capsule after restart: {log_out}"
    );
}

#[test]
fn tier1_f03_redb_negative_index_persistence() {
    let repo = TestRepo::new().unwrap();
    repo.write_file("src/auth.rs", "pub fn auth_lock_fn() -> bool { true }\n")
        .unwrap();
    repo.loom(&["init"]).assert_success();

    repo.loom(&["spawn", "-n", "vp-neg", "-m", "try auth"])
        .assert_success();
    let vp = repo.viewport_dir("vp-neg");
    fs::write(
        vp.join("src/auth.rs"),
        "pub fn auth_lock_fn() -> bool { false }\n",
    )
    .unwrap();
    repo.loom_in(
        &vp,
        &[
            "drop",
            "--reason",
            "deadlock on auth_lock_fn",
            "--kind",
            "Deadlock",
        ],
    )
    .assert_success();

    let pid1 = repo.daemon_pid().expect("daemon PID before kill");
    force_kill_pid(pid1);

    let mut mcp = repo.spawn_mcp_stdio(None).unwrap();
    mcp_handshake(&mut mcp);
    let tools = mcp.tools_list().unwrap();
    let preflight_tool = tools["tools"]
        .as_array()
        .and_then(|arr| {
            arr.iter()
                .find(|t| t.get("name").and_then(Value::as_str) == Some("loom_preflight_symbols"))
        })
        .expect("loom_preflight_symbols should be listed in tools/list");

    let args = if preflight_tool["inputSchema"]["properties"]
        .get("symbol")
        .is_some()
        && preflight_tool["inputSchema"]["properties"]
            .get("symbols")
            .is_none()
    {
        json!({ "symbol": "auth_lock_fn" })
    } else {
        json!({ "symbols": ["auth_lock_fn"] })
    };

    let res = mcp.tools_call("loom_preflight_symbols", args).unwrap();
    let text = mcp_tool_text(&res);
    assert!(
        text.contains("deadlock on auth_lock_fn"),
        "negative_index warning missing after daemon restart: {text}"
    );
}

#[test]
fn tier1_f03_redb_git_map_and_capsules_persistence() {
    let repo = TestRepo::new().unwrap();
    repo.write_file("src/lib.rs", "pub fn g() -> i32 { 1 }\n")
        .unwrap();
    repo.init_git_repo().unwrap();

    let bare = repo.init_bare_remote().unwrap();
    let bare_str = bare.to_string_lossy().into_owned();
    repo.git(&["remote", "add", "origin", &bare_str])
        .assert_success();
    repo.git(&["push", "-u", "origin", "main"])
        .assert_success();

    repo.loom(&["init", "--colocate-git"]).assert_success();
    repo.loom(&["spawn", "-n", "vp-gmap", "-m", "git map"])
        .assert_success();
    let vp = repo.viewport_dir("vp-gmap");
    fs::write(vp.join("src/lib.rs"), "pub fn g() -> i32 { 2 }\n").unwrap();
    repo.loom_in(&vp, &["seal", "-m", "capsule gmap"])
        .assert_success();

    repo.loom(&["git", "push", "origin", "main"])
        .assert_success();
    let head_first = repo
        .git_in(&bare, &["rev-parse", "main"])
        .assert_success()
        .stdout_str();

    let pid1 = repo.daemon_pid().expect("daemon PID before kill");
    force_kill_pid(pid1);

    let cap_log = repo
        .loom(&["log", "--capsules"])
        .assert_success()
        .stdout_str();
    assert!(
        cap_log.contains("capsule gmap"),
        "capsules missing after daemon restart: {cap_log}"
    );

    repo.loom(&["git", "push", "origin", "main"])
        .assert_success();
    let head_second = repo
        .git_in(&bare, &["rev-parse", "main"])
        .assert_success()
        .stdout_str();
    assert_eq!(
        head_first, head_second,
        "git_map should prevent duplicate commits on bare remote after restart"
    );
}

// ============================================================================
// Feature F4: CAS Store (BLAKE3 + Zstd-3) (5 tests)
// ============================================================================

#[test]
fn tier1_f04_cas_stores_compressed_blobs() {
    let repo = TestRepo::new().unwrap();
    repo.write_file("src/rep.rs", "pub fn base() {}\n").unwrap();
    repo.loom(&["init"]).assert_success();

    repo.loom(&["spawn", "-n", "vp-cas", "-m", "store blob"])
        .assert_success();
    let cas_dir = repo.path(".loom/cas").unwrap();
    let blobs_before = collect_blob_sizes(&cas_dir);

    let vp = repo.viewport_dir("vp-cas");
    let big_rep = "pub fn rep_item() -> i32 { let x = 12345; x }\n".repeat(220);
    assert!(big_rep.len() > 8192);
    fs::write(vp.join("src/rep.rs"), &big_rep).unwrap();
    repo.loom_in(&vp, &["seal", "-m", "compress"])
        .assert_success();

    let blobs_after = collect_files_recursive(&cas_dir);
    assert!(!blobs_after.is_empty(), "expected blobs in .loom/cas/");

    let zstd_magic = [0x28_u8, 0xB5, 0x2F, 0xFD];
    let mut new_blobs = Vec::new();
    for blob in &blobs_after {
        let bytes = fs::read(blob).unwrap();
        assert!(
            bytes.len() >= 4 && bytes[..4] == zstd_magic,
            "CAS blob {} missing Zstd magic",
            blob.display()
        );
        if !blobs_before.contains_key(blob) {
            new_blobs.push((blob.clone(), bytes.len()));
        }
    }
    assert!(
        !new_blobs.is_empty(),
        "expected at least one new CAS blob after sealing big_rep"
    );
    assert!(
        new_blobs
            .iter()
            .all(|(_, len)| *len < big_rep.len() && *len < 2048),
        "expected newly added compressed CAS blobs for big_rep (uncompressed={} bytes) to be < 2048 bytes, got: {:?}",
        big_rep.len(),
        new_blobs
    );
}

#[test]
fn tier1_f04_cas_content_address_deduplication() {
    let repo = TestRepo::new().unwrap();
    let payload_1 = generate_varied_rust_payload(0x1357_9BDF_2468_ACE0, 180);
    let payload_2 = generate_varied_rust_payload(0xFEDC_BA98_7654_3210, 180);
    assert!(payload_1.len() > 8192);
    assert!(payload_2.len() > 8192);
    assert_ne!(payload_1, payload_2);

    repo.write_file("src/a.rs", "pub fn seed() -> i32 { 0 }\n")
        .unwrap();
    repo.loom(&["init"]).assert_success();

    // Seal payload_1 once in vp-d1/src/b.rs
    repo.loom(&["spawn", "-n", "vp-d1", "-m", "d1"])
        .assert_success();
    let vp1 = repo.viewport_dir("vp-d1");
    fs::write(vp1.join("src/b.rs"), &payload_1).unwrap();
    repo.loom_in(&vp1, &["seal", "-m", "seal d1"])
        .assert_success();
    let cas_dir = repo.path(".loom/cas").unwrap();
    let blobs_1 = collect_blob_sizes(&cas_dir);
    let bytes_1: u64 = blobs_1.values().copied().sum();

    // Seal identical payload_1 in vp-d2/src/c.rs -> content blob must be deduplicated
    repo.loom(&["spawn", "-n", "vp-d2", "-m", "d2"])
        .assert_success();
    let vp2 = repo.viewport_dir("vp-d2");
    fs::write(vp2.join("src/c.rs"), &payload_1).unwrap();
    repo.loom_in(&vp2, &["seal", "-m", "seal d2"])
        .assert_success();
    let blobs_2 = collect_blob_sizes(&cas_dir);
    let bytes_2: u64 = blobs_2.values().copied().sum();
    for existing_path in blobs_1.keys() {
        assert!(
            blobs_2.contains_key(existing_path),
            "existing CAS blob disappeared: {}",
            existing_path.display()
        );
    }
    let max_new_dup_blob = blobs_2
        .iter()
        .filter(|(p, _)| !blobs_1.contains_key(*p))
        .map(|(_, &sz)| sz)
        .max()
        .unwrap_or(0);
    let delta_dup_bytes = bytes_2.saturating_sub(bytes_1);

    // Seal distinct payload_2 of similar size in vp-d3/src/d.rs as a baseline
    repo.loom(&["spawn", "-n", "vp-d3", "-m", "d3"])
        .assert_success();
    let vp3 = repo.viewport_dir("vp-d3");
    fs::write(vp3.join("src/d.rs"), &payload_2).unwrap();
    repo.loom_in(&vp3, &["seal", "-m", "seal d3"])
        .assert_success();
    let blobs_3 = collect_blob_sizes(&cas_dir);
    let bytes_3: u64 = blobs_3.values().copied().sum();
    let max_new_unique_blob = blobs_3
        .iter()
        .filter(|(p, _)| !blobs_2.contains_key(*p))
        .map(|(_, &sz)| sz)
        .max()
        .unwrap_or(0);
    let delta_unique_bytes = bytes_3.saturating_sub(bytes_2);

    assert!(
        max_new_unique_blob >= 1024,
        "expected unique 8+ KiB payload to produce a >=1 KiB compressed CAS blob, got {max_new_unique_blob}"
    );
    assert!(
        max_new_dup_blob < max_new_unique_blob / 2,
        "identical payload created a content-sized blob ({max_new_dup_blob} vs unique {max_new_unique_blob})"
    );
    assert!(
        delta_dup_bytes < delta_unique_bytes,
        "duplicate payload CAS growth ({delta_dup_bytes}) was not smaller than unique payload ({delta_unique_bytes})"
    );
    assert_eq!(repo.read_file("src/b.rs").unwrap(), payload_1);
    assert_eq!(repo.read_file("src/c.rs").unwrap(), payload_1);
    assert_eq!(repo.read_file("src/d.rs").unwrap(), payload_2);
}

#[test]
fn tier1_f04_cas_roundtrip_exact_bytes() {
    let repo = TestRepo::new().unwrap();
    let v1 = "// 한글 UTF-8 αβγ\npub fn exact() -> i32 { 1 }\n";
    let v2 = "// 수정된 UTF-8 Ω\npub fn exact() -> i32 { 2 }\n";
    repo.write_file("src/exact.rs", v1).unwrap();
    repo.loom(&["init"]).assert_success();

    repo.loom(&["spawn", "-n", "vp-rt", "-m", "rt"])
        .assert_success();
    let vp = repo.viewport_dir("vp-rt");
    fs::write(vp.join("src/exact.rs"), v2).unwrap();
    repo.loom_in(&vp, &["seal", "-m", "v2"]).assert_success();
    assert_eq!(repo.read_file("src/exact.rs").unwrap(), v2);

    repo.loom(&["undo"]).assert_success();
    assert_eq!(repo.read_file("src/exact.rs").unwrap(), v1);
}

#[test]
fn tier1_f04_cas_large_file_chunking() {
    let repo = TestRepo::new().unwrap();
    repo.write_bytes("assets/big.bin", b"init").unwrap();
    repo.loom(&["init"]).assert_success();

    let bin_v2: Vec<u8> = (0..1_572_864_usize)
        .map(|i| (i.wrapping_mul(31) ^ (i >> 8)) as u8)
        .collect();

    repo.loom(&["spawn", "-n", "vp-big", "-m", "chunking"])
        .assert_success();
    let vp = repo.viewport_dir("vp-big");
    fs::write(vp.join("assets/big.bin"), &bin_v2).unwrap();
    repo.loom_in(&vp, &["seal", "-m", "big bin"])
        .assert_success();

    repo.loom(&["spawn", "-n", "vp-check", "-m", "verify big"])
        .assert_success();

    let blobs = collect_files_recursive(&repo.path(".loom/cas").unwrap());
    assert!(!blobs.is_empty());
    let zstd_magic = [0x28_u8, 0xB5, 0x2F, 0xFD];
    assert!(blobs.iter().any(|p| {
        fs::read(p)
            .map(|b| b.len() >= 4 && b[..4] == zstd_magic)
            .unwrap_or(false)
    }));

    assert_eq!(repo.read_bytes("assets/big.bin").unwrap(), bin_v2);
    assert_eq!(
        fs::read(repo.viewport_dir("vp-check").join("assets/big.bin")).unwrap(),
        bin_v2
    );
}

#[test]
fn tier1_f04_cas_gc_retains_referenced_blobs() {
    let repo = TestRepo::new().unwrap();
    repo.write_file("src/keep.rs", "pub fn k() -> i32 { 1 }\n")
        .unwrap();
    repo.loom(&["init"]).assert_success();

    repo.loom(&["spawn", "-n", "vp-k", "-m", "keep"])
        .assert_success();
    let vp_k = repo.viewport_dir("vp-k");
    fs::write(vp_k.join("src/keep.rs"), "pub fn k() -> i32 { 10 }\n").unwrap();
    repo.loom_in(&vp_k, &["seal", "-m", "keep cap"])
        .assert_success();

    repo.loom(&["spawn", "-n", "vp-x", "-m", "drop"])
        .assert_success();
    let vp_x = repo.viewport_dir("vp-x");
    fs::write(
        vp_x.join("src/trash.rs"),
        "pub fn trash() -> i32 { 999 }\n".repeat(120),
    )
    .unwrap();
    repo.loom_in(&vp_x, &["drop", "--reason", "unused"])
        .assert_success();

    repo.loom(&["gc", "--prune-viewports"]).assert_success();
    repo.loom(&["spawn", "-n", "vp-after", "-m", "after gc"])
        .assert_success();

    let trunk_keep = repo.read_file("src/keep.rs").unwrap();
    assert!(trunk_keep.contains("10"));
    assert_eq!(
        fs::read_to_string(repo.viewport_dir("vp-after").join("src/keep.rs")).unwrap(),
        trunk_keep
    );
    let cap_log = repo
        .loom(&["log", "--capsules"])
        .assert_success()
        .stdout_str();
    assert!(cap_log.contains("keep cap"));
}

// ============================================================================
// Feature F5: WAL & Crash Recovery (5 tests)
// ============================================================================

#[test]
fn tier1_f05_wal_segment_lmwa_header() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();
    repo.loom(&["spawn", "-n", "vp-wal", "-m", "wal header"])
        .assert_success();

    let files = collect_files_recursive(&repo.path(".loom/wal").unwrap());
    assert!(!files.is_empty(), "expected WAL segment files in .loom/wal/");
    let has_lmwa = files.iter().any(|p| {
        fs::read(p)
            .map(|b| b.len() >= 4 && &b[..4] == b"LMWA")
            .unwrap_or(false)
    });
    assert!(has_lmwa, "expected at least one WAL segment starting with LMWA");
}

#[test]
fn tier1_f05_wal_appends_on_mutations() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();
    repo.loom(&["status"]).assert_success();

    let wal_dir = repo.path(".loom/wal").unwrap();
    let wal_before = total_dir_bytes(&wal_dir);

    repo.loom(&["spawn", "-n", "vp-w1", "-m", "m1"])
        .assert_success();
    let vp = repo.viewport_dir("vp-w1");
    fs::create_dir_all(vp.join("src")).unwrap();
    fs::write(vp.join("src/a.rs"), "pub fn wal_a() -> i32 { 7 }\n").unwrap();
    repo.loom_in(&vp, &["seal", "-m", "s1"]).assert_success();

    let wal_after = total_dir_bytes(&wal_dir);
    assert!(
        wal_after > wal_before,
        "WAL bytes should grow on mutations (before={wal_before}, after={wal_after})"
    );
}

#[test]
fn tier1_f05_wal_replay_after_clean_checkpoint() {
    let repo = TestRepo::new().unwrap();
    repo.write_file("src/lib.rs", "pub fn c() -> i32 { 0 }\n")
        .unwrap();
    repo.loom(&["init"]).assert_success();

    repo.loom(&["spawn", "-n", "vp-cp", "-m", "cp"])
        .assert_success();
    let vp = repo.viewport_dir("vp-cp");
    fs::write(vp.join("src/lib.rs"), "pub fn c() -> i32 { 1 }\n").unwrap();
    repo.loom_in(&vp, &["seal", "-m", "checkpointed"])
        .assert_success();
    repo.loom(&["gc", "--prune-viewports"]).assert_success();

    if let Some(pid) = repo.daemon_pid() {
        force_kill_pid(pid);
    }

    let cap_log = repo
        .loom(&["log", "--capsules"])
        .assert_success()
        .stdout_str();
    assert!(cap_log.contains("checkpointed"));
    assert!(repo.read_file("src/lib.rs").unwrap().contains('1'));
}

#[test]
fn tier1_f05_wal_crash_recovery_after_sigkill() {
    let repo = TestRepo::new().unwrap();
    repo.write_file("src/lib.rs", "pub fn k() -> i32 { 0 }\n")
        .unwrap();
    repo.loom(&["init"]).assert_success();

    repo.loom(&["spawn", "-n", "vp-1", "-m", "seal before kill"])
        .assert_success();
    let vp1 = repo.viewport_dir("vp-1");
    fs::write(vp1.join("src/lib.rs"), "pub fn k() -> i32 { 5 }\n").unwrap();
    repo.loom_in(&vp1, &["seal", "-m", "before kill"])
        .assert_success();

    repo.loom(&["spawn", "-n", "vp-2", "-m", "active during kill"])
        .assert_success();

    let pid1 = repo.daemon_pid().expect("daemon PID before kill");
    force_kill_pid(pid1);

    let list_out = repo.loom(&["list"]).assert_success().stdout_str();
    assert!(
        list_out.contains("vp-2"),
        "active viewport lost across crash recovery: {list_out}"
    );

    let log_out = repo
        .loom(&["log", "--capsules"])
        .assert_success()
        .stdout_str();
    assert!(
        log_out.contains("before kill"),
        "sealed capsule lost across crash recovery: {log_out}"
    );

    let pid2 = repo.daemon_pid().expect("daemon PID after recovery");
    assert_ne!(pid1, pid2);
    assert!(is_pid_running(pid2));
}

#[test]
fn tier1_f05_wal_multiple_sequential_replays() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();

    for i in 1..=3 {
        let name = format!("vp-seq-{i}");
        let intent = format!("round {i}");
        repo.loom(&["spawn", "-n", &name, "-m", &intent])
            .assert_success();
        let pid = repo.daemon_pid().expect("daemon PID");
        force_kill_pid(pid);
        repo.loom(&["list"]).assert_success();
    }

    let final_list = repo.loom(&["list"]).assert_success().stdout_str();
    for name in ["vp-seq-1", "vp-seq-2", "vp-seq-3"] {
        assert!(
            final_list.contains(name),
            "missing {name} after sequential crash replays: {final_list}"
        );
    }
}

// ============================================================================
// Feature F6: Single-Writer loomd & Idle Shutdown (5 tests)
// ============================================================================

#[test]
fn tier1_f06_single_daemon_per_repo() {
    let repo1 = TestRepo::new().unwrap();
    let repo2 = TestRepo::new().unwrap();

    repo1.loom(&["init"]).assert_success();
    repo1.loom(&["status"]).assert_success();
    let p1 = repo1.daemon_pid().expect("repo1 daemon PID");

    repo1.loom(&["list"]).assert_success();
    repo1.loom(&["log"]).assert_success();
    repo1.loom(&["status"]).assert_success();
    let p1_again = repo1.daemon_pid().expect("repo1 daemon PID again");

    repo2.loom(&["init"]).assert_success();
    repo2.loom(&["status"]).assert_success();
    let p2 = repo2.daemon_pid().expect("repo2 daemon PID");

    assert_eq!(p1, p1_again);
    assert_ne!(p1, p2);
    assert!(is_pid_running(p1));
    assert!(is_pid_running(p2));
}

#[test]
fn tier1_f06_daemon_sock_windows_pipe_descriptor() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();
    repo.loom(&["status"]).assert_success();

    let raw = repo.read_file(".loom/daemon.sock").unwrap();
    let val: Value =
        serde_json::from_str(&raw).expect(".loom/daemon.sock should be valid JSON descriptor");
    let expected_pid = repo.daemon_pid().expect("daemon PID") as u64;

    assert_eq!(val["pid"].as_u64(), Some(expected_pid));
    assert!(val["protocol_version"].as_u64().is_some_and(|v| v >= 1));
    assert!(val["transport"].as_str().is_some_and(|s| !s.is_empty()));

    let endpoint = val["endpoint"]
        .as_str()
        .expect("daemon.sock endpoint must be a string");
    assert!(!endpoint.is_empty());
    #[cfg(windows)]
    assert!(
        endpoint.starts_with(r"\\.\pipe\loom-"),
        "expected Windows named pipe endpoint, got {endpoint}"
    );
}

#[test]
fn tier1_f06_concurrent_cli_serialization() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();

    std::thread::scope(|s| {
        let mut handles = Vec::new();
        for i in 0..6 {
            let repo_ref = &repo;
            handles.push(s.spawn(move || {
                let name = format!("vp-c-{i}");
                repo_ref
                    .loom(&["spawn", "-n", &name, "-m", "conc"])
                    .assert_success();
            }));
        }
        for h in handles {
            h.join().expect("concurrent CLI worker thread panicked");
        }
    });

    let list_out = repo.loom(&["list"]).assert_success().stdout_str();
    for i in 0..6 {
        let name = format!("vp-c-{i}");
        assert!(
            list_out.contains(&name),
            "concurrently spawned {name} missing from list: {list_out}"
        );
    }
}

#[test]
fn tier1_f06_idle_shutdown_with_zero_viewports() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();
    set_idle_timeout_secs(&repo, 2);
    if let Some(pid) = repo.daemon_pid() {
        force_kill_pid(pid);
    }

    repo.loom(&["status"]).assert_success();
    let pid1 = repo.daemon_pid().expect("daemon PID after status");
    assert!(is_pid_running(pid1));

    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline && is_pid_running(pid1) {
        std::thread::sleep(Duration::from_millis(250));
    }
    assert!(
        !is_pid_running(pid1),
        "daemon pid {pid1} did not auto-exit after 2s idle timeout with 0 viewports"
    );

    repo.loom(&["status"]).assert_success();
    let pid2 = repo.daemon_pid().expect("respawned daemon PID");
    assert_ne!(pid1, pid2);
    assert!(is_pid_running(pid2));
}

#[test]
fn tier1_f06_no_idle_shutdown_with_active_viewport() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();
    set_idle_timeout_secs(&repo, 2);
    if let Some(pid) = repo.daemon_pid() {
        force_kill_pid(pid);
    }

    repo.loom(&["spawn", "-n", "vp-hold", "-m", "keep alive"])
        .assert_success();
    let pid1 = repo.daemon_pid().expect("daemon PID");

    std::thread::sleep(Duration::from_secs(4));

    assert!(
        is_pid_running(pid1),
        "daemon pid {pid1} should remain alive while active viewport exists"
    );
    let list_out = repo.loom(&["list"]).assert_success().stdout_str();
    assert!(list_out.contains("vp-hold"));
    assert_eq!(repo.daemon_pid(), Some(pid1));
}

// ============================================================================
// Feature F7: R1 CLI Commands (5 tests)
// ============================================================================

#[test]
fn tier1_f07_list_and_status_empty_repo() {
    let repo = TestRepo::new().unwrap();
    repo.loom(&["init"]).assert_success();

    let list_out = repo.loom(&["list"]);
    list_out.assert_success();
    let status_out = repo.loom(&["status"]);
    status_out.assert_success();

    assert!(!list_out.stdout_str().trim().is_empty());
    assert!(!status_out.stdout_str().trim().is_empty());
}

#[test]
fn tier1_f07_log_shows_ordered_ops() {
    let repo = TestRepo::new().unwrap();
    repo.write_file("src/a.rs", "pub fn a() -> i32 { 1 }\n")
        .unwrap();
    repo.loom(&["init"]).assert_success();

    repo.loom(&["spawn", "-n", "vp-1", "-m", "first"])
        .assert_success();
    repo.loom(&["spawn", "-n", "vp-2", "-m", "second"])
        .assert_success();
    let vp1 = repo.viewport_dir("vp-1");
    fs::write(vp1.join("src/a.rs"), "pub fn a() -> i32 { 2 }\n").unwrap();
    repo.loom_in(&vp1, &["seal", "-m", "third seal"])
        .assert_success();

    let log_out = repo.loom(&["log"]).assert_success().stdout_str();
    let p1_first = log_out
        .find("vp-1")
        .unwrap_or_else(|| panic!("log missing vp-1: {log_out}"));
    let p1_last = log_out
        .rfind("vp-1")
        .unwrap_or_else(|| panic!("log missing vp-1: {log_out}"));
    let p2 = log_out
        .find("vp-2")
        .unwrap_or_else(|| panic!("log missing vp-2: {log_out}"));
    let p3 = log_out
        .find("third seal")
        .unwrap_or_else(|| panic!("log missing third seal: {log_out}"));
    assert!(
        (p1_first < p2 && p2 < p3) || (p3 < p2 && p2 < p1_last),
        "log entries are not monotonically ordered (p1_first={p1_first}, p1_last={p1_last}, p2={p2}, p3={p3}):\n{log_out}"
    );
}

#[test]
fn tier1_f07_log_filter_capsules_and_symbol() {
    let repo = TestRepo::new().unwrap();
    repo.write_file("src/a.rs", "pub fn alpha_only_fn() -> i32 { 1 }\n")
        .unwrap();
    repo.write_file("src/b.rs", "pub fn beta_only_fn() -> i32 { 2 }\n")
        .unwrap();
    repo.loom(&["init"]).assert_success();

    repo.loom(&["spawn", "-n", "vp-a", "-m", "a"])
        .assert_success();
    let vp_a = repo.viewport_dir("vp-a");
    fs::write(vp_a.join("src/a.rs"), "pub fn alpha_only_fn() -> i32 { 10 }\n").unwrap();
    repo.loom_in(&vp_a, &["seal", "-m", "cap alpha"])
        .assert_success();

    repo.loom(&["spawn", "-n", "vp-b", "-m", "b"])
        .assert_success();
    let vp_b = repo.viewport_dir("vp-b");
    fs::write(vp_b.join("src/b.rs"), "pub fn beta_only_fn() -> i32 { 20 }\n").unwrap();
    repo.loom_in(&vp_b, &["seal", "-m", "cap beta"])
        .assert_success();

    let caps_out = repo
        .loom(&["log", "--capsules"])
        .assert_success()
        .stdout_str();
    assert!(caps_out.contains("cap alpha"));
    assert!(caps_out.contains("cap beta"));

    let sym_out = repo
        .loom(&["log", "--symbol", "alpha_only_fn"])
        .assert_success()
        .stdout_str();
    assert!(
        sym_out.contains("cap alpha"),
        "symbol log missing cap alpha match for alpha_only_fn: {sym_out}"
    );
    assert!(
        !sym_out.contains("cap beta"),
        "symbol log should exclude unrelated capsule beta: {sym_out}"
    );
}

#[test]
fn tier1_f07_undo_reverts_last_op() {
    let repo = TestRepo::new().unwrap();
    let orig = "pub fn u() -> i32 { 1 }\n";
    repo.write_file("src/u.rs", orig).unwrap();
    repo.loom(&["init"]).assert_success();

    repo.loom(&["spawn", "-n", "vp-u", "-m", "edit u"])
        .assert_success();
    let vp = repo.viewport_dir("vp-u");
    fs::write(vp.join("src/u.rs"), "pub fn u() -> i32 { 99 }\n").unwrap();
    repo.loom_in(&vp, &["seal", "-m", "bump u"])
        .assert_success();
    assert!(repo.read_file("src/u.rs").unwrap().contains("99"));

    repo.loom(&["undo"]).assert_success();
    assert_eq!(repo.read_file("src/u.rs").unwrap(), orig);

    let log_out = repo.loom(&["log"]).assert_success().stdout_str();
    assert!(
        log_out.to_lowercase().contains("undo") || log_out.contains("bump u"),
        "log should reflect undo or prior op: {log_out}"
    );
}

#[test]
fn tier1_f07_gc_prunes_sealed_and_dropped() {
    let repo = TestRepo::new().unwrap();
    repo.write_file("src/lib.rs", "pub fn f() -> i32 { 1 }\n")
        .unwrap();
    repo.loom(&["init"]).assert_success();

    repo.loom(&["spawn", "-n", "vp-s", "-m", "s"])
        .assert_success();
    let vp_s = repo.viewport_dir("vp-s");
    fs::write(vp_s.join("src/lib.rs"), "pub fn f() -> i32 { 2 }\n").unwrap();
    repo.loom_in(&vp_s, &["seal", "-m", "s"]).assert_success();

    repo.loom(&["spawn", "-n", "vp-d", "-m", "d"])
        .assert_success();
    let vp_d = repo.viewport_dir("vp-d");
    fs::write(vp_d.join("src/lib.rs"), "pub fn f() -> i32 { 3 }\n").unwrap();
    repo.loom_in(&vp_d, &["drop", "--reason", "d"])
        .assert_success();

    repo.loom(&["spawn", "-n", "vp-a", "-m", "a"])
        .assert_success();

    repo.loom(&["gc", "--prune-viewports"]).assert_success();

    assert!(!repo.viewport_dir("vp-s").exists());
    assert!(!repo.viewport_dir("vp-d").exists());
    assert!(repo.viewport_dir("vp-a").exists());
    assert!(repo
        .loom(&["list"])
        .assert_success()
        .stdout_str()
        .contains("vp-a"));
}
