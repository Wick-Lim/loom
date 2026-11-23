use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use e2e_suite::{wait_for_radar_status, McpClient, TestRepo};

fn setup_repo(files: &[(&str, &str)], colocate_git: bool) -> TestRepo {
    let repo = TestRepo::new().expect("create TestRepo");
    for (rel, content) in files {
        repo.write_file(rel, content)
            .unwrap_or_else(|e| panic!("write {rel}: {e}"));
    }
    if colocate_git {
        repo.init_git_repo().expect("init_git_repo");
        repo.loom(&["init", "--colocate-git", "--json"])
            .assert_success();
    } else {
        repo.loom(&["init", "--json"]).assert_success();
    }
    repo
}

fn setup_git_clone_from_bare(bare: &Path) -> TestRepo {
    let repo = TestRepo::new().expect("create clone TestRepo");
    let bare_str = bare.to_string_lossy().into_owned();
    repo.git(&["clone", &bare_str, "."]).assert_success();
    repo.git(&["config", "user.name", "Loom E2E"])
        .assert_success();
    repo.git(&["config", "user.email", "loom-e2e@example.com"])
        .assert_success();
    repo.git(&["config", "core.autocrlf", "false"])
        .assert_success();
    repo.git(&["checkout", "-B", "main", "origin/main"])
        .assert_success();
    repo
}

fn kill_pid(pid: u32) {
    assert!(pid > 0 && pid != std::process::id(), "refusing to kill invalid pid {pid}");
    #[cfg(windows)]
    {
        let status = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("spawn taskkill");
        let _ = status;
    }
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn is_pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(windows)]
    {
        let out = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .stdin(Stdio::null())
            .output()
            .expect("run tasklist");
        let stdout = String::from_utf8_lossy(&out.stdout);
        stdout.contains(&format!("\"{pid}\""))
    }
    #[cfg(unix)]
    {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

fn wait_until<F>(timeout: Duration, step: Duration, label: &str, mut pred: F)
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + timeout;
    loop {
        if pred() {
            return;
        }
        if Instant::now() >= deadline {
            panic!("timed out after {timeout:?} waiting for: {label}");
        }
        std::thread::sleep(step);
    }
}

fn json_contains_ci(val: &Value, needle: &str) -> bool {
    let needle_lower = needle.to_ascii_lowercase();
    match val {
        Value::String(s) => s.to_ascii_lowercase().contains(&needle_lower),
        Value::Array(arr) => arr.iter().any(|item| json_contains_ci(item, needle)),
        Value::Object(map) => map
            .iter()
            .any(|(k, v)| k.to_ascii_lowercase().contains(&needle_lower) || json_contains_ci(v, needle)),
        _ => false,
    }
}

fn has_pending_syntax(val: &Value) -> bool {
    match val {
        Value::String(s) => {
            s.eq_ignore_ascii_case("PENDING_SYNTAX")
                || s.eq_ignore_ascii_case("PendingSyntax")
                || s.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                    .any(|tok| {
                        tok.eq_ignore_ascii_case("PENDING_SYNTAX")
                            || tok.eq_ignore_ascii_case("PendingSyntax")
                    })
        }
        Value::Array(arr) => arr.iter().any(has_pending_syntax),
        Value::Object(map) => map.iter().any(|(k, v)| {
            if k == "pending_syntax_files" || k == "pending_syntax" {
                match v {
                    Value::Array(arr) if !arr.is_empty() => return true,
                    Value::Bool(true) => return true,
                    Value::Number(n) if n.as_u64().is_some_and(|x| x > 0) => return true,
                    _ => {}
                }
            }
            has_pending_syntax(v)
        }),
        _ => false,
    }
}

fn has_radar_level(val: &Value, target: &str) -> bool {
    match val {
        Value::String(s) => s.eq_ignore_ascii_case(target),
        Value::Array(arr) => arr.iter().any(|item| has_radar_level(item, target)),
        Value::Object(map) => map.iter().any(|(k, v)| {
            if matches!(
                k.as_str(),
                "status" | "level" | "severity" | "radar_level" | "radar_status"
            ) {
                if let Some(s) = v.as_str() {
                    return s.eq_ignore_ascii_case(target)
                        || s.split(|c: char| !c.is_ascii_alphanumeric())
                            .any(|tok| tok.eq_ignore_ascii_case(target));
                }
            }
            has_radar_level(v, target)
        }),
        _ => false,
    }
}

fn has_nonempty_conflicts(val: &Value) -> bool {
    match val {
        Value::Array(arr) => arr.iter().any(has_nonempty_conflicts),
        Value::Object(map) => map.iter().any(|(k, v)| {
            if k == "conflicts" {
                if let Some(arr) = v.as_array() {
                    if !arr.is_empty() {
                        return true;
                    }
                }
            }
            has_nonempty_conflicts(v)
        }),
        _ => false,
    }
}

fn is_radar_clean_green(val: &Value) -> bool {
    has_radar_level(val, "GREEN")
        && !has_radar_level(val, "RED")
        && !has_radar_level(val, "YELLOW")
        && !has_pending_syntax(val)
        && !has_nonempty_conflicts(val)
}

fn text_has_word_ci(text: &str, target: &str) -> bool {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|tok| tok.eq_ignore_ascii_case(target))
}

fn find_viewport_entry<'a>(val: &'a Value, vp_name: &str) -> Option<&'a Value> {
    let matches_name = |item: &Value| {
        item.get("name").and_then(Value::as_str) == Some(vp_name)
            || item.get("viewport_name").and_then(Value::as_str) == Some(vp_name)
            || item.get("id").and_then(Value::as_str) == Some(vp_name)
    };
    if let Some(arr) = val.as_array() {
        return arr.iter().find(|item| matches_name(item));
    }
    if let Some(obj) = val.as_object() {
        for k in ["viewports", "items", "entries"] {
            if let Some(arr) = obj.get(k).and_then(Value::as_array) {
                if let Some(found) = arr.iter().find(|item| matches_name(item)) {
                    return Some(found);
                }
            }
        }
    }
    None
}

fn viewport_has_state(vp_entry: &Value, expected_states: &[&str]) -> bool {
    for field in ["state", "status", "lifecycle_state"] {
        if let Some(s) = vp_entry.get(field).and_then(Value::as_str) {
            if expected_states.iter().any(|exp| s.eq_ignore_ascii_case(exp)) {
                return true;
            }
        }
    }
    false
}

fn extract_op_seq(item: &Value) -> Option<u64> {
    for key in ["op_seq", "seq_id", "seq", "id"] {
        if let Some(n) = item.get(key).and_then(Value::as_u64) {
            return Some(n);
        }
        if let Some(s) = item.get(key).and_then(Value::as_str) {
            if let Ok(n) = s.parse::<u64>() {
                return Some(n);
            }
        }
    }
    None
}

fn mcp_handshake_resilient(client: &mut McpClient) -> Value {
    match client.handshake() {
        Ok(res) => res,
        Err(_) => {
            let res = client
                .initialize_with_version("2025-03-26")
                .expect("MCP initialize fallback 2025-03-26");
            client.send_initialized().expect("MCP send_initialized");
            res
        }
    }
}

fn build_tool_args(
    tools_resp: &Value,
    tool_name: &str,
    candidates: &[(&[&str], Value)],
) -> Value {
    let schema = tools_resp
        .get("tools")
        .and_then(Value::as_array)
        .and_then(|arr| {
            arr.iter()
                .find(|t| t.get("name").and_then(Value::as_str) == Some(tool_name))
        })
        .and_then(|t| t.get("inputSchema"));

    let props = schema
        .and_then(|s| s.get("properties"))
        .and_then(Value::as_object);

    let mut obj = serde_json::Map::new();
    for (keys, val) in candidates {
        let chosen = if let Some(map) = props {
            keys.iter()
                .copied()
                .find(|k| map.contains_key(*k))
        } else {
            None
        };
        if let Some(k) = chosen {
            obj.insert(k.to_string(), val.clone());
        } else if props.is_none() {
            if let Some(first) = keys.first() {
                obj.insert((*first).to_string(), val.clone());
            }
        }
    }

    if let Some(req) = schema.and_then(|s| s.get("required")).and_then(Value::as_array) {
        let missing: Vec<&str> = req
            .iter()
            .filter_map(Value::as_str)
            .filter(|k| !obj.contains_key(*k))
            .collect();
        assert!(
            missing.is_empty(),
            "build_tool_args({tool_name}): failed to populate schema-required fields {missing:?}; schema={schema:?}, built={obj:?}"
        );
    }

    Value::Object(obj)
}

fn mcp_result_text(res: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(sc) = res.get("structuredContent") {
        parts.push(sc.to_string());
    }
    if let Some(content) = res.get("content").and_then(Value::as_array) {
        for item in content {
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                parts.push(text.to_string());
            }
        }
    }
    if parts.is_empty() {
        res.to_string()
    } else {
        parts.join("\n")
    }
}

fn extract_mcp_is_error(res: &Value) -> bool {
    res.get("isError").and_then(Value::as_bool) == Some(true)
}

fn latest_wal_file(repo: &TestRepo) -> PathBuf {
    let wal_dir = repo.path(".loom/wal").expect("wal path");
    let mut files: Vec<PathBuf> = fs::read_dir(&wal_dir)
        .expect("read .loom/wal")
        .filter_map(|e| e.ok().map(|x| x.path()))
        .filter(|p| {
            p.is_file()
                && fs::read(p)
                    .map(|b| b.starts_with(b"LMWA"))
                    .unwrap_or(false)
        })
        .collect();
    files.sort();
    files.pop().expect("at least one LMWA WAL segment file")
}

fn has_zstd_cas_blob(repo: &TestRepo) -> bool {
    let cas_dir = repo.path(".loom/cas").expect("cas path");
    fn scan(dir: &Path) -> bool {
        let Ok(entries) = fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                if scan(&p) {
                    return true;
                }
            } else if p.is_file() {
                if let Ok(bytes) = fs::read(&p) {
                    if bytes.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
                        return true;
                    }
                }
            }
        }
        false
    }
    scan(&cas_dir)
}

fn rs_metadata_verify_cmd() -> &'static str {
    "rustc --edition 2021 --crate-type lib --emit=metadata src/lib.rs --out-dir target"
}

fn is_idle_timeout_key(key: &str) -> bool {
    let lower = key.to_lowercase();
    lower.contains("idle")
        && (lower.contains("timeout") || lower.contains("sec") || lower.contains("shutdown"))
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
    let raw = repo.read_file(".loom/config.toml").expect("read config.toml");
    let mut parsed: toml::Value =
        toml::from_str(&raw).expect(".loom/config.toml should parse as TOML");
    assert!(
        update_idle_timeout_in_value(&mut parsed, secs),
        "failed to locate idle timeout setting in .loom/config.toml:\n{raw}"
    );
    let updated = toml::to_string_pretty(&parsed).expect("serialize updated config.toml");
    repo.write_file(".loom/config.toml", &updated)
        .expect("write config.toml");
}

fn extract_items(val: &Value, wrapper_keys: &[&str]) -> Vec<Value> {
    if let Some(arr) = val.as_array() {
        return arr.clone();
    }
    for k in wrapper_keys {
        if let Some(arr) = val.get(*k).and_then(Value::as_array) {
            return arr.clone();
        }
    }
    Vec::new()
}

fn verify_radar_clean_or_green(repo: &TestRepo, vp: &Path) -> Value {
    let st = wait_for_radar_status(vp, Duration::from_secs(10), is_radar_clean_green)
        .unwrap_or_else(|e| panic!("expected clean GREEN radar status for {}: {e}", vp.display()));
    assert!(is_radar_clean_green(&st), "radar not clean GREEN: {st}");
    let _ = repo;
    st
}

// 1. F1 x F6 (Control Flow)
#[test]
fn tier3_f01_f06_cli_auto_spawn_and_single_writer() {
    let repo = setup_repo(&[("crates/app/src/lib.rs", "pub fn init() -> i32 { 1 }\n")], false);

    let root_status = repo.loom(&["status", "--json"]);
    root_status.assert_success();
    let root_json = root_status.stdout_json().expect("status json");
    let pid1 = repo.daemon_pid().expect("daemon pid after status");
    assert_eq!(
        root_json.get("pid").and_then(Value::as_u64),
        Some(u64::from(pid1))
    );

    let sock_raw = repo.read_file(".loom/daemon.sock").expect("read daemon.sock");
    let sock_json: Value = serde_json::from_str(&sock_raw).expect("daemon.sock valid json");
    assert!(sock_json.get("transport").and_then(Value::as_str).is_some());
    assert!(sock_json.get("endpoint").and_then(Value::as_str).is_some());
    assert_eq!(sock_json.get("pid").and_then(Value::as_u64), Some(u64::from(pid1)));
    assert!(sock_json.get("protocol_version").and_then(Value::as_u64).is_some());

    let nested_dir = repo.path("crates/app/src").expect("nested path");
    let nested_status = repo.loom_in(&nested_dir, &["status", "--json"]);
    nested_status.assert_success();
    let nested_json = nested_status.stdout_json().expect("nested status json");
    assert_eq!(nested_json.get("pid").and_then(Value::as_u64), Some(u64::from(pid1)));
    assert_eq!(
        nested_json.get("endpoint").and_then(Value::as_str),
        root_json.get("endpoint").and_then(Value::as_str)
    );
    assert_eq!(
        nested_json.get("endpoint").and_then(Value::as_str),
        sock_json.get("endpoint").and_then(Value::as_str)
    );

    std::thread::scope(|s| {
        for i in 0..8 {
            let repo_ref = &repo;
            s.spawn(move || {
                let vp_name = format!("w-{i}");
                repo_ref
                    .loom(&["spawn", "-n", &vp_name, "-m", "concurrent spawn"])
                    .assert_success();
                repo_ref.loom(&["status", "--json"]).assert_success();
            });
        }
    });

    assert_eq!(repo.daemon_pid(), Some(pid1), "single daemon PID must persist across concurrent spawns");
    let list_out = repo.loom(&["list", "--json"]);
    list_out.assert_success();
    let list_json = list_out.stdout_json().expect("list json");
    for i in 0..8 {
        assert!(
            json_contains_ci(&list_json, &format!("w-{i}")),
            "missing viewport w-{i} in list output: {list_json}"
        );
    }
}

// 2. F2 x F6 (Shared State)
#[test]
fn tier3_f02_f06_config_timeout_controls_idle_shutdown() {
    let repo = setup_repo(&[("src/lib.rs", "pub fn a() -> i32 { 1 }\n")], false);
    let initial_pid = repo.daemon_pid().expect("initial daemon pid");
    set_idle_timeout_secs(&repo, 2);

    kill_pid(initial_pid);
    wait_until(Duration::from_secs(5), Duration::from_millis(50), "initial daemon exit", || {
        !is_pid_alive(initial_pid)
    });

    repo.loom(&["status", "--json"]).assert_success();
    let pid1 = repo.daemon_pid().expect("restarted daemon pid");
    assert_ne!(pid1, initial_pid);

    repo.loom(&["spawn", "-n", "keepalive", "-m", "hold daemon open"])
        .assert_success();
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        is_pid_alive(pid1),
        "daemon must stay alive while active viewport exists"
    );

    let keepalive_dir = repo.viewport_dir("keepalive");
    repo.loom_in(&keepalive_dir, &["drop", "--reason", "done holding"])
        .assert_success();

    wait_until(
        Duration::from_secs(12),
        Duration::from_millis(100),
        "idle daemon auto-exit with 0 active viewports",
        || !is_pid_alive(pid1),
    );

    let list_out = repo.loom(&["list", "--json"]);
    list_out.assert_success();
    let pid2 = repo.daemon_pid().expect("transparently respawned daemon pid");
    assert_ne!(pid2, pid1);
}

// 3. F3 x F5 (Shared State)
#[test]
fn tier3_f03_f05_wal_replay_repopulates_redb_tables() {
    let repo = setup_repo(&[("src/lib.rs", "pub fn alpha() -> i32 { 1 }\n")], false);

    repo.loom(&["spawn", "-n", "vp1", "-m", "add beta"])
        .assert_success();
    let vp1 = repo.viewport_dir("vp1");
    fs::write(
        vp1.join("src/lib.rs"),
        "pub fn alpha() -> i32 { 1 }\npub fn beta() -> i32 { 2 }\n",
    )
    .expect("write vp1 src/lib.rs");
    repo.loom_in(&vp1, &["seal", "-m", "add beta"])
        .assert_success();

    repo.loom(&["spawn", "-n", "vp2", "-m", "feature work"])
        .assert_success();

    let pid1 = repo.daemon_pid().expect("daemon pid before crash");
    kill_pid(pid1);
    wait_until(Duration::from_secs(5), Duration::from_millis(50), "daemon dead", || {
        !is_pid_alive(pid1)
    });

    let wal_file = latest_wal_file(&repo);
    let wal_bytes = fs::read(&wal_file).expect("read WAL segment");
    assert!(wal_bytes.starts_with(b"LMWA"), "WAL segment must start with LMWA magic");

    let mut f = OpenOptions::new()
        .append(true)
        .open(&wal_file)
        .expect("open WAL for torn tail append");
    f.write_all(b"LMWA\x01\x00").expect("append torn tail bytes");
    drop(f);

    let list_out = repo.loom(&["list", "--json"]);
    list_out.assert_success();
    let list_json = list_out.stdout_json().expect("list json");
    let vp2_entry = find_viewport_entry(&list_json, "vp2")
        .unwrap_or_else(|| panic!("expected vp2 in list output: {list_json}"));
    assert!(
        viewport_has_state(vp2_entry, &["Active", "Spawned"]),
        "expected vp2 state Active/Spawned after WAL replay: {vp2_entry}"
    );

    let pid2 = repo.daemon_pid().expect("restarted daemon pid");
    assert_ne!(pid2, pid1);

    let log_out = repo.loom(&["log", "--capsules", "--symbol", "beta", "--json"]);
    log_out.assert_success();
    let log_json = log_out.stdout_json().expect("log json");
    let found_exact =
        json_contains_ci(&log_json, "beta") || json_contains_ci(&log_json, "add beta");
    if !found_exact {
        let all_caps = repo
            .loom(&["log", "--capsules", "--json"])
            .assert_success()
            .stdout_json()
            .expect("all capsules json");
        assert!(
            json_contains_ci(&all_caps, "beta") || json_contains_ci(&all_caps, "add beta"),
            "expected beta capsule in state.redb after WAL replay (exact={log_json}, all={all_caps})"
        );
    }
}

// 4. F4 x F7 (Data Flow)
#[test]
fn tier3_f04_f07_cas_integrity_verified_by_undo_and_gc() {
    let repo = TestRepo::new().expect("create TestRepo");
    repo.write_file("src/data.rs", "pub const V: i32 = 1;\n")
        .expect("write data.rs");
    let v1_bytes = vec![0xABu8; 8192];
    repo.write_bytes("assets/payload.bin", &v1_bytes)
        .expect("write v1 bin");
    repo.loom(&["init", "--json"]).assert_success();

    repo.loom(&["spawn", "-n", "vp1", "-m", "update v2"])
        .assert_success();
    let vp1 = repo.viewport_dir("vp1");
    fs::write(vp1.join("src/data.rs"), "pub const V: i32 = 2;\n").expect("write v2 data.rs");
    let v2_bytes = vec![0xCDu8; 8192];
    fs::write(vp1.join("assets/payload.bin"), &v2_bytes).expect("write v2 bin");
    repo.loom_in(&vp1, &["seal", "-m", "v2 payload"])
        .assert_success();

    assert_eq!(repo.read_bytes("assets/payload.bin").unwrap(), v2_bytes);
    assert!(
        has_zstd_cas_blob(&repo),
        "expected Zstd-compressed (28 B5 2F FD) blob in .loom/cas/"
    );

    let log_out = repo.loom(&["log", "--json"]);
    log_out.assert_success();
    let log_json = log_out.stdout_json().expect("log json");
    let items = extract_items(&log_json, &["ops", "entries", "log"]);
    let seal_seq = items
        .iter()
        .filter(|item| {
            json_contains_ci(item, "v2 payload")
                || item
                    .get("kind")
                    .and_then(Value::as_str)
                    .is_some_and(|k| k.to_ascii_lowercase().contains("seal"))
        })
        .max_by_key(|item| extract_op_seq(item))
        .and_then(extract_op_seq)
        .expect("find seal op_seq in loom log --json");

    repo.loom(&["spawn", "-n", "vp_drop", "-m", "discard"])
        .assert_success();
    let vp_drop = repo.viewport_dir("vp_drop");
    fs::write(vp_drop.join("src/data.rs"), "pub const V: i32 = 999;\n").unwrap();
    repo.loom_in(&vp_drop, &["drop", "--reason", "bad mutation"])
        .assert_success();

    let undo_out = repo.loom(&["undo", "--op", &seal_seq.to_string(), "--json"]);
    undo_out.assert_success();
    let undo_json = undo_out.stdout_json().expect("undo json");
    assert!(
        json_contains_ci(&undo_json, &seal_seq.to_string()) || json_contains_ci(&undo_json, "undo"),
        "expected undo result referencing op {seal_seq}: {undo_json}"
    );

    let log_after_undo = repo
        .loom(&["log", "--json"])
        .assert_success()
        .stdout_json()
        .expect("log after undo");
    let ops_after = extract_items(&log_after_undo, &["ops", "entries", "log"]);
    assert!(
        ops_after.iter().any(|item| {
            item.get("undone_by_op").and_then(Value::as_u64).is_some()
                || item
                    .get("kind")
                    .and_then(Value::as_str)
                    .is_some_and(|k| k.to_ascii_lowercase().contains("undo"))
        }),
        "expected undone_by_op or Undo op in loom log --json: {log_after_undo}"
    );
    if repo.read_file("src/data.rs").unwrap() == "pub const V: i32 = 1;\n" {
        assert_eq!(repo.read_bytes("assets/payload.bin").unwrap(), v1_bytes);
    }

    repo.loom(&["gc", "--prune-viewports", "--json"])
        .assert_success();
    assert!(!vp_drop.exists(), "dropped viewport dir should be pruned by gc");
    assert!(
        has_zstd_cas_blob(&repo),
        "expected Zstd-compressed CAS blobs retained after gc"
    );

    repo.loom(&["status", "--json"]).assert_success();
    repo.loom(&["log", "--json"]).assert_success();
}

// 5. F7 x F10 (Control Flow)
#[test]
fn tier3_f07_f10_spawn_status_gc_full_lifecycle() {
    let repo = setup_repo(&[("src/lib.rs", "pub fn base() -> i32 { 0 }\n")], false);

    #[cfg(windows)]
    let spawn_seal = repo.loom(&[
        "spawn",
        "-n",
        "vp-seal",
        "-m",
        "feat A",
        "--",
        "cmd",
        "/C",
        "echo",
        "spawned-ok",
    ]);
    #[cfg(not(windows))]
    let spawn_seal = repo.loom(&[
        "spawn",
        "-n",
        "vp-seal",
        "-m",
        "feat A",
        "--",
        "sh",
        "-c",
        "echo spawned-ok",
    ]);
    spawn_seal.assert_success();
    assert!(
        spawn_seal.stdout_str().contains("spawned-ok"),
        "expected wrapped command output, got: {}",
        spawn_seal.stdout_str()
    );

    let vp_seal = repo.viewport_dir("vp-seal");
    fs::write(
        vp_seal.join("src/lib.rs"),
        "pub fn base() -> i32 { 0 }\npub fn feat_a() -> i32 { 1 }\n",
    )
    .unwrap();
    repo.loom_in(&vp_seal, &["seal", "-m", "feat A"])
        .assert_success();

    repo.loom(&["spawn", "-n", "vp-drop", "-m", "dead"])
        .assert_success();
    let vp_drop = repo.viewport_dir("vp-drop");
    repo.loom_in(&vp_drop, &["drop", "--reason", "abandoned"])
        .assert_success();

    repo.loom(&["spawn", "-n", "vp-live", "-m", "keep"])
        .assert_success();
    let vp_live = repo.viewport_dir("vp-live");

    let list_before = repo.loom(&["list", "--include-gc", "--json"]);
    list_before.assert_success();
    let list_json = list_before.stdout_json().expect("list json");
    let seal_entry = find_viewport_entry(&list_json, "vp-seal")
        .unwrap_or_else(|| panic!("missing vp-seal: {list_json}"));
    assert!(viewport_has_state(seal_entry, &["Sealed", "GcPending"]));
    let drop_entry = find_viewport_entry(&list_json, "vp-drop")
        .unwrap_or_else(|| panic!("missing vp-drop: {list_json}"));
    assert!(viewport_has_state(drop_entry, &["Dropped", "GcPending"]));
    let live_entry = find_viewport_entry(&list_json, "vp-live")
        .unwrap_or_else(|| panic!("missing vp-live: {list_json}"));
    assert!(viewport_has_state(live_entry, &["Active", "Spawned"]));

    let status_out = repo.loom(&["status", "--json"]);
    status_out.assert_success();
    let status_json = status_out.stdout_json().expect("status json");
    assert_eq!(status_json.get("active_viewports").and_then(Value::as_u64), Some(1));

    repo.loom(&["gc", "--prune-viewports", "--json"])
        .assert_success();
    assert!(!vp_seal.exists(), "sealed viewport directory must be pruned");
    assert!(!vp_drop.exists(), "dropped viewport directory must be pruned");
    assert!(vp_live.exists(), "active viewport directory must NOT be pruned");

    let capsules = repo.loom(&["log", "--capsules", "--json"]);
    capsules.assert_success();
    assert!(json_contains_ci(&capsules.stdout_json().unwrap(), "feat A"));
}

// 6. F8 x F9 (Shared State)
#[test]
fn tier3_f08_f09_ntfs_fallback_isolates_locks_and_sources() {
    let repo = TestRepo::new().expect("create TestRepo");
    repo.write_file("src/lib.rs", "pub fn trunk_fn() -> i32 { 1 }\n")
        .unwrap();
    repo.write_file("node_modules/pkg/index.js", "module.exports = { v: 1 };\n")
        .unwrap();
    repo.write_file(".venv/Lib/site.py", "VERSION = '1.0'\n")
        .unwrap();
    repo.write_bytes("target/debug/deps/libdep.rlib", &vec![0x5Au8; 4096])
        .unwrap();
    repo.write_bytes("target/debug/app.exe", b"TRUNK_BIN")
        .unwrap();
    repo.write_file(".cargo-lock", "trunk-lock").unwrap();
    repo.loom(&["init", "--json"]).assert_success();

    repo.loom(&["spawn", "-n", "vp1", "-m", "lock holder"])
        .assert_success();
    repo.loom(&["spawn", "-n", "vp2", "-m", "concurrent builder"])
        .assert_success();

    let vp1 = repo.viewport_dir("vp1");
    let vp2 = repo.viewport_dir("vp2");

    #[cfg(windows)]
    let lock_handle = {
        use std::os::windows::fs::OpenOptionsExt;
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(0)
            .open(vp1.join(".cargo-lock"))
            .expect("open exclusive Windows lock on vp1/.cargo-lock")
    };
    #[cfg(not(windows))]
    let lock_handle = {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(vp1.join(".cargo-lock"))
            .expect("open vp1/.cargo-lock")
    };

    repo.loom(&["spawn", "-n", "vp3", "-m", "spawn during lock"])
        .assert_success();
    let vp3 = repo.viewport_dir("vp3");

    fs::write(vp2.join(".cargo-lock"), "vp2-lock").expect("write vp2 .cargo-lock");
    fs::write(vp2.join("target/debug/app.exe"), b"VP2_BIN").expect("write vp2 app.exe");
    fs::write(vp2.join("src/lib.rs"), "pub fn trunk_fn() -> i32 { 222 }\n")
        .expect("write vp2 src/lib.rs");

    for vp in [&vp1, &vp2, &vp3] {
        assert!(vp.join("node_modules/pkg/index.js").is_file());
        assert!(vp.join(".venv/Lib/site.py").is_file());
        assert!(vp.join("target/debug/deps/libdep.rlib").is_file());
    }

    assert_eq!(repo.read_file(".cargo-lock").unwrap(), "trunk-lock");
    assert_eq!(repo.read_bytes("target/debug/app.exe").unwrap(), b"TRUNK_BIN");
    assert_eq!(
        repo.read_file("src/lib.rs").unwrap(),
        "pub fn trunk_fn() -> i32 { 1 }\n"
    );
    assert_eq!(fs::read_to_string(vp2.join(".cargo-lock")).unwrap(), "vp2-lock");

    drop(lock_handle);
}

// 7. F10 x F20 (Control Flow)
#[test]
fn tier3_f10_f20_spawn_verify_seal_with_command() {
    let repo = setup_repo(
        &[
            ("target/.keep", ""),
            ("src/lib.rs", "pub fn add(a: i32, b: i32) -> i32 { a + b }\n"),
        ],
        false,
    );

    #[cfg(windows)]
    let spawn_out = repo.loom(&[
        "spawn",
        "-n",
        "vp-cmd",
        "-m",
        "wrapped edit",
        "--",
        "cmd",
        "/C",
        "echo pub fn add() { bad_symbol(); } > src\\lib.rs",
    ]);
    #[cfg(not(windows))]
    let spawn_out = repo.loom(&[
        "spawn",
        "-n",
        "vp-cmd",
        "-m",
        "wrapped edit",
        "--",
        "sh",
        "-c",
        "printf 'pub fn add() { bad_symbol(); }\\n' > src/lib.rs",
    ]);
    spawn_out.assert_success();

    let vp_cmd = repo.viewport_dir("vp-cmd");
    let bad_seal = repo.loom_in(
        &vp_cmd,
        &["seal", "-v", rs_metadata_verify_cmd(), "-m", "try 1"],
    );
    bad_seal.assert_failure();
    assert_eq!(
        repo.read_file("src/lib.rs").unwrap(),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n"
    );

    let list_mid = repo.loom(&["list", "--json"]).assert_success().stdout_json().unwrap();
    let vp_entry = find_viewport_entry(&list_mid, "vp-cmd")
        .unwrap_or_else(|| panic!("vp-cmd present after failed seal: {list_mid}"));
    assert!(
        !viewport_has_state(vp_entry, &["Sealed"]),
        "vp-cmd must not be Sealed after failed verify: {vp_entry}"
    );

    fs::write(
        vp_cmd.join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b + 1 }\n",
    )
    .unwrap();
    repo.loom_in(
        &vp_cmd,
        &["seal", "-v", rs_metadata_verify_cmd(), "-m", "verified calc"],
    )
    .assert_success();

    assert!(repo.read_file("src/lib.rs").unwrap().contains("a + b + 1"));
    let caps = repo
        .loom(&["log", "--capsules", "--json"])
        .assert_success()
        .stdout_json()
        .unwrap();
    assert!(json_contains_ci(&caps, "verified calc"));
}

// 8. F11 x F12 (Data Flow)
#[test]
fn tier3_f11_f12_trivia_attached_across_line_shift_merge() {
    let rs_init = concat!(
        "/// Alpha doc\n",
        "#[inline]\n",
        "pub fn alpha() -> i32 { 1 }\n\n",
        "/// Beta doc\n",
        "#[must_use]\n",
        "pub fn beta() -> i32 { 2 }\n",
    );
    let py_init = concat!(
        "def cached(fn):\n",
        "    return fn\n\n",
        "# py doc\n",
        "@cached\n",
        "def compute(x: int) -> int:\n",
        "    return x + 1\n",
    );
    let repo = setup_repo(&[("src/lib.rs", rs_init), ("src/py_mod.py", py_init)], false);

    repo.loom(&["spawn", "-n", "vp1", "-m", "top header shift"])
        .assert_success();
    repo.loom(&["spawn", "-n", "vp2", "-m", "beta and compute update"])
        .assert_success();

    let vp1 = repo.viewport_dir("vp1");
    let vp2 = repo.viewport_dir("vp2");

    let rs_header: String = (0..20).map(|i| format!("// header line {i}\n")).collect();
    let py_header: String = (0..20).map(|i| format!("# header line {i}\n")).collect();

    fs::write(
        vp1.join("src/lib.rs"),
        format!(
            "{rs_header}/// Alpha doc\n#[inline]\npub fn alpha() -> i32 {{ 11 }}\n\n/// Beta doc\n#[must_use]\npub fn beta() -> i32 {{ 2 }}\n"
        ),
    )
    .unwrap();
    fs::write(
        vp1.join("src/py_mod.py"),
        format!("{py_header}{py_init}"),
    )
    .unwrap();

    fs::write(
        vp2.join("src/lib.rs"),
        concat!(
            "/// Alpha doc\n",
            "#[inline]\n",
            "pub fn alpha() -> i32 { 1 }\n\n",
            "/// Updated beta doc\n",
            "#[must_use]\n",
            "pub fn beta() -> i32 { 22 }\n",
        ),
    )
    .unwrap();
    fs::write(
        vp2.join("src/py_mod.py"),
        concat!(
            "def cached(fn):\n",
            "    return fn\n\n",
            "# py doc\n",
            "@cached\n",
            "def compute(x: int) -> int:\n",
            "    return x + 99\n",
        ),
    )
    .unwrap();

    repo.loom_in(&vp1, &["seal", "-m", "vp1 shifts"])
        .assert_success();
    repo.loom_in(&vp2, &["seal", "-m", "vp2 body and trivia"])
        .assert_success();

    let merged_rs = repo.read_file("src/lib.rs").unwrap();
    assert!(merged_rs.contains("// header line 19"));
    assert!(merged_rs.contains("pub fn alpha() -> i32 { 11 }"));
    assert!(merged_rs.contains("/// Updated beta doc"));
    assert!(merged_rs.contains("#[must_use]"));
    assert!(merged_rs.contains("pub fn beta() -> i32 { 22 }"));
    assert!(!merged_rs.contains("<<<<<<<"));

    let merged_py = repo.read_file("src/py_mod.py").unwrap();
    assert!(merged_py.contains("# header line 19"));
    assert!(merged_py.contains("@cached"));
    assert!(merged_py.contains("return x + 99"));
}

// 9. F11 x F13 (Data Flow)
#[test]
fn tier3_f11_f13_4lang_commutative_insert_member() {
    let rs_base = "pub struct Svc;\n\nimpl Svc {\n    pub fn ping(&self) -> i32 { 1 }\n}\n";
    let ts_base = "export class Service {\n  ping(): number { return 1; }\n}\n";
    let go_base = "package pkg\n\nfunc Ping() int {\n\treturn 1\n}\n";
    let py_base = "class Service:\n    @staticmethod\n    def ping() -> int:\n        return 1\n";

    let repo = setup_repo(
        &[
            ("src/lib.rs", rs_base),
            ("src/service.ts", ts_base),
            ("pkg/service.go", go_base),
            ("app/service.py", py_base),
        ],
        false,
    );

    repo.loom(&["spawn", "-n", "vp1", "-m", "insert alpha members"])
        .assert_success();
    repo.loom(&["spawn", "-n", "vp2", "-m", "insert beta members"])
        .assert_success();

    let vp1 = repo.viewport_dir("vp1");
    let vp2 = repo.viewport_dir("vp2");

    fs::write(
        vp1.join("src/lib.rs"),
        "pub struct Svc;\n\nimpl Svc {\n    pub fn ping(&self) -> i32 { 1 }\n\n    /// Alpha helper\n    #[inline]\n    pub fn alpha(&self) -> i32 { 10 }\n}\n",
    )
    .unwrap();
    fs::write(
        vp1.join("src/service.ts"),
        "export class Service {\n  ping(): number { return 1; }\n  /** Alpha JSDoc */\n  alpha(): number { return 10; }\n}\n",
    )
    .unwrap();
    fs::write(
        vp1.join("pkg/service.go"),
        "package pkg\n\nfunc Ping() int {\n\treturn 1\n}\n\n// Alpha Go doc\nfunc Alpha() int {\n\treturn 10\n}\n",
    )
    .unwrap();
    fs::write(
        vp1.join("app/service.py"),
        "class Service:\n    @staticmethod\n    def ping() -> int:\n        return 1\n\n    # Alpha py doc\n    @staticmethod\n    def alpha() -> int:\n        return 10\n",
    )
    .unwrap();

    fs::write(
        vp2.join("src/lib.rs"),
        "pub struct Svc;\n\nimpl Svc {\n    pub fn ping(&self) -> i32 { 1 }\n\n    /// Beta helper\n    #[inline]\n    pub fn beta(&self) -> i32 { 20 }\n}\n",
    )
    .unwrap();
    fs::write(
        vp2.join("src/service.ts"),
        "export class Service {\n  ping(): number { return 1; }\n  /** Beta JSDoc */\n  beta(): number { return 20; }\n}\n",
    )
    .unwrap();
    fs::write(
        vp2.join("pkg/service.go"),
        "package pkg\n\nfunc Ping() int {\n\treturn 1\n}\n\n// Beta Go doc\nfunc Beta() int {\n\treturn 20\n}\n",
    )
    .unwrap();
    fs::write(
        vp2.join("app/service.py"),
        "class Service:\n    @staticmethod\n    def ping() -> int:\n        return 1\n\n    # Beta py doc\n    @staticmethod\n    def beta() -> int:\n        return 20\n",
    )
    .unwrap();

    repo.loom_in(&vp1, &["seal", "-m", "insert alpha"])
        .assert_success();
    repo.loom_in(&vp2, &["seal", "-m", "insert beta"])
        .assert_success();

    let rs_merged = repo.read_file("src/lib.rs").unwrap();
    assert!(rs_merged.contains("/// Alpha helper") && rs_merged.contains("fn alpha"));
    assert!(rs_merged.contains("/// Beta helper") && rs_merged.contains("fn beta"));

    let ts_merged = repo.read_file("src/service.ts").unwrap();
    assert!(ts_merged.contains("/** Alpha JSDoc */") && ts_merged.contains("alpha()"));
    assert!(ts_merged.contains("/** Beta JSDoc */") && ts_merged.contains("beta()"));

    let go_merged = repo.read_file("pkg/service.go").unwrap();
    assert!(go_merged.contains("// Alpha Go doc") && go_merged.contains("func Alpha()"));
    assert!(go_merged.contains("// Beta Go doc") && go_merged.contains("func Beta()"));

    let py_merged = repo.read_file("app/service.py").unwrap();
    assert!(py_merged.contains("# Alpha py doc") && py_merged.contains("def alpha()"));
    assert!(py_merged.contains("# Beta py doc") && py_merged.contains("def beta()"));
}

// 10. F12 x F17 (Data Flow)
#[test]
fn tier3_f12_f17_anchor_invariant_prevents_false_radar_red() {
    let ts_init = concat!(
        "export function helper(): number { return 1; }\n",
        "export function target(): number { return 2; }\n",
    );
    let rs_init = concat!(
        "pub fn top() -> i32 { 1 }\n",
        "pub fn bottom() -> i32 { 2 }\n",
    );
    let repo = setup_repo(&[("src/mod.ts", ts_init), ("src/worker.rs", rs_init)], false);

    repo.loom(&["spawn", "-n", "vp1", "-m", "prepend exports"])
        .assert_success();
    repo.loom(&["spawn", "-n", "vp2", "-m", "edit target & bottom"])
        .assert_success();

    let vp1 = repo.viewport_dir("vp1");
    let vp2 = repo.viewport_dir("vp2");

    let ts_top: String = (0..15)
        .map(|i| format!("export const PAD_{i}: number = {i};\n"))
        .collect();
    let rs_top: String = (0..15)
        .map(|i| format!("pub fn pad_{i}() -> i32 {{ {i} }}\n"))
        .collect();

    fs::write(vp1.join("src/mod.ts"), format!("{ts_top}{ts_init}")).unwrap();
    fs::write(vp1.join("src/worker.rs"), format!("{rs_top}{rs_init}")).unwrap();

    fs::write(
        vp2.join("src/mod.ts"),
        "export function helper(): number { return 1; }\nexport function target(): number { return 200; }\n",
    )
    .unwrap();
    fs::write(
        vp2.join("src/worker.rs"),
        "pub fn top() -> i32 { 1 }\npub fn bottom() -> i32 { 200 }\n",
    )
    .unwrap();

    verify_radar_clean_or_green(&repo, &vp1);
    verify_radar_clean_or_green(&repo, &vp2);

    repo.loom_in(&vp1, &["seal", "-m", "vp1 prepend"])
        .assert_success();
    repo.loom_in(&vp2, &["seal", "-m", "vp2 bottom"])
        .assert_success();

    let ts_merged = repo.read_file("src/mod.ts").unwrap();
    assert!(ts_merged.contains("PAD_14") && ts_merged.contains("return 200;"));
    let rs_merged = repo.read_file("src/worker.rs").unwrap();
    assert!(rs_merged.contains("pad_14") && rs_merged.contains("bottom() -> i32 { 200 }"));
}

// 11. F13 x F14 (Data Flow)
#[test]
fn tier3_f13_f14_mixed_code_and_crdt_single_seal() {
    let repo = setup_repo(
        &[
            (
                "src/lib.rs",
                "pub fn left() -> i32 { 1 }\npub fn right() -> i32 { 2 }\n",
            ),
            ("config.json", r#"{"port":8080,"debug":false,"name":"loom"}"#),
            ("deploy.yaml", "replicas: 1\nenv: staging\n"),
            ("README.md", "# Spec\n\n## A\ninit\n\n## B\ninit\n"),
        ],
        false,
    );

    repo.loom(&["spawn", "-n", "vp1", "-m", "left + port + replicas"])
        .assert_success();
    repo.loom(&["spawn", "-n", "vp2", "-m", "right + debug + env"])
        .assert_success();

    let vp1 = repo.viewport_dir("vp1");
    let vp2 = repo.viewport_dir("vp2");

    fs::write(
        vp1.join("src/lib.rs"),
        "pub fn left() -> i32 { 10 }\npub fn right() -> i32 { 2 }\n",
    )
    .unwrap();
    fs::write(
        vp1.join("config.json"),
        r#"{"port":9090,"debug":false,"name":"loom"}"#,
    )
    .unwrap();
    fs::write(vp1.join("deploy.yaml"), "replicas: 3\nenv: staging\n").unwrap();
    fs::write(
        vp1.join("README.md"),
        "# Spec\n\n## A\nupdated by vp1\n\n## B\ninit\n",
    )
    .unwrap();

    fs::write(
        vp2.join("src/lib.rs"),
        "pub fn left() -> i32 { 1 }\npub fn right() -> i32 { 20 }\n",
    )
    .unwrap();
    fs::write(
        vp2.join("config.json"),
        r#"{"port":8080,"debug":true,"name":"loom"}"#,
    )
    .unwrap();
    fs::write(vp2.join("deploy.yaml"), "replicas: 1\nenv: production\n").unwrap();
    fs::write(
        vp2.join("README.md"),
        "# Spec\n\n## A\ninit\n\n## B\nupdated by vp2\n",
    )
    .unwrap();

    repo.loom_in(&vp1, &["seal", "-m", "vp1 mixed"])
        .assert_success();
    repo.loom_in(&vp2, &["seal", "-m", "vp2 mixed"])
        .assert_success();

    let rs = repo.read_file("src/lib.rs").unwrap();
    assert!(rs.contains("10") && rs.contains("20"));

    let cfg: Value = serde_json::from_str(&repo.read_file("config.json").unwrap())
        .expect("merged config.json must be valid JSON");
    assert_eq!(cfg.get("port").and_then(Value::as_i64), Some(9090));
    assert_eq!(cfg.get("debug").and_then(Value::as_bool), Some(true));

    let yaml = repo.read_file("deploy.yaml").unwrap();
    assert!(yaml.contains("replicas: 3") && yaml.contains("env: production"));

    let md = repo.read_file("README.md").unwrap();
    assert!(md.contains("updated by vp1") && md.contains("updated by vp2"));
}

// 12. F13 x F20 (Control Flow)
#[test]
fn tier3_f13_f20_semantic_conflict_surfaces_on_seal() {
    let repo = setup_repo(
        &[(
            "src/lib.rs",
            "pub fn compute(x: i32) -> i32 { x }\npub fn caller() -> i32 { compute(5) }\n",
        )],
        false,
    );

    repo.loom(&["spawn", "-n", "vp1", "-m", "change compute arity"])
        .assert_success();
    repo.loom(&["spawn", "-n", "vp2", "-m", "edit caller with stale arity"])
        .assert_success();

    let vp1 = repo.viewport_dir("vp1");
    let vp2 = repo.viewport_dir("vp2");

    fs::write(
        vp1.join("src/lib.rs"),
        "pub fn compute(x: i32, scale: i32) -> i32 { x * scale }\npub fn caller() -> i32 { compute(5) }\n",
    )
    .unwrap();
    repo.loom_in(&vp1, &["seal", "-m", "update compute signature"])
        .assert_success();

    fs::write(
        vp2.join("src/lib.rs"),
        "pub fn compute(x: i32) -> i32 { x }\npub fn caller() -> i32 { compute(42) }\n",
    )
    .unwrap();
    let stale_seal = repo.loom_in(&vp2, &["seal", "-m", "stale caller"]);
    stale_seal.assert_failure();

    let combined_diag = format!("{} {}", stale_seal.stdout_str(), stale_seal.stderr_str());
    assert!(
        combined_diag.to_ascii_lowercase().contains("compute")
            || combined_diag.to_ascii_lowercase().contains("conflict"),
        "expected semantic conflict diagnostics referencing compute: {combined_diag}"
    );

    let trunk = repo.read_file("src/lib.rs").unwrap();
    assert!(trunk.contains("scale: i32"));
    assert!(!trunk.contains("<<<<<<<"));
}

// 13. F14 x F22 (Data Flow)
#[test]
fn tier3_f14_f22_binary_cas_and_crdt_git_roundtrip() {
    let repo_a = TestRepo::new().expect("repo_a");
    let bare = repo_a.init_bare_remote().expect("bare remote");
    let bare_str = bare.to_string_lossy().into_owned();

    let bin_v1: Vec<u8> = (0..(64 * 1024)).map(|i| (i % 251) as u8).collect();
    repo_a
        .write_file("settings.json", r#"{"theme":"light","retries":1}"#)
        .unwrap();
    repo_a
        .write_file("NOTES.md", "# Notes\n\n## Alpha\nold\n\n## Beta\nold\n")
        .unwrap();
    repo_a.write_bytes("assets/model.bin", &bin_v1).unwrap();
    repo_a.init_git_repo().expect("init_git_repo");
    repo_a
        .git(&["remote", "add", "origin", &bare_str])
        .assert_success();
    repo_a.git(&["push", "-u", "origin", "main"]).assert_success();
    repo_a
        .loom(&["init", "--colocate-git", "--json"])
        .assert_success();

    let repo_b = setup_git_clone_from_bare(&bare);
    repo_b
        .loom(&["init", "--colocate-git", "--json"])
        .assert_success();

    repo_a
        .loom(&["spawn", "-n", "vp1", "-m", "bin + theme"])
        .assert_success();
    repo_a
        .loom(&["spawn", "-n", "vp2", "-m", "retries + beta"])
        .assert_success();
    let vp1 = repo_a.viewport_dir("vp1");
    let vp2 = repo_a.viewport_dir("vp2");

    let bin_v2: Vec<u8> = (0..(128 * 1024)).map(|i| ((i * 7 + 3) % 256) as u8).collect();
    fs::write(vp1.join("assets/model.bin"), &bin_v2).unwrap();
    fs::write(
        vp1.join("settings.json"),
        r#"{"theme":"dark","retries":1}"#,
    )
    .unwrap();
    fs::write(
        vp1.join("NOTES.md"),
        "# Notes\n\n## Alpha\nnew alpha\n\n## Beta\nold\n",
    )
    .unwrap();

    fs::write(
        vp2.join("settings.json"),
        r#"{"theme":"light","retries":5}"#,
    )
    .unwrap();
    fs::write(
        vp2.join("NOTES.md"),
        "# Notes\n\n## Alpha\nold\n\n## Beta\nnew beta\n",
    )
    .unwrap();

    repo_a
        .loom_in(
            &vp1,
            &[
                "seal",
                "-v",
                "git grep --no-index -q dark settings.json",
                "-m",
                "bin and theme",
            ],
        )
        .assert_success();
    repo_a
        .loom_in(
            &vp2,
            &[
                "seal",
                "-v",
                "git grep --no-index -q retries settings.json",
                "-m",
                "retries and beta",
            ],
        )
        .assert_success();

    repo_a
        .loom(&["git", "push", "origin", "main"])
        .assert_success();

    let bare_log = repo_a
        .git_in(&bare, &["log", "--format=%B", "main"])
        .assert_success()
        .stdout_str();
    assert!(bare_log.contains("Loom-Capsule-Id:"));
    assert!(bare_log.contains("Loom-Agent:"));
    assert!(bare_log.contains("Loom-Verified:"));

    let bare_notes = repo_a
        .git_in(&bare, &["notes", "--ref=loom", "list"])
        .assert_success()
        .stdout_str();
    assert!(!bare_notes.trim().is_empty());

    repo_b
        .loom(&["git", "pull", "origin", "main"])
        .assert_success();

    assert_eq!(repo_b.read_bytes("assets/model.bin").unwrap(), bin_v2);
    let settings_b: Value = serde_json::from_str(&repo_b.read_file("settings.json").unwrap())
        .expect("settings.json valid json in repo_b");
    assert_eq!(settings_b.get("theme").and_then(Value::as_str), Some("dark"));
    assert_eq!(settings_b.get("retries").and_then(Value::as_i64), Some(5));
    let notes_b = repo_b.read_file("NOTES.md").unwrap();
    assert!(notes_b.contains("new alpha") && notes_b.contains("new beta"));
}

// 14. F15 x F16 (Control Flow)
#[test]
fn tier3_f15_f16_burst_codegen_with_transient_syntax_error() {
    let repo = setup_repo(&[("src/lib.rs", "pub fn base() -> i32 { 1 }\n")], false);

    repo.loom(&["spawn", "-n", "vp1", "-m", "codegen burst"])
        .assert_success();
    repo.loom(&["spawn", "-n", "vp2", "-m", "valid peer"])
        .assert_success();

    let vp1 = repo.viewport_dir("vp1");
    let vp2 = repo.viewport_dir("vp2");
    fs::create_dir_all(vp1.join("gen")).unwrap();

    for i in 0..35 {
        let p = vp1.join(format!("gen/m{i:02}.rs"));
        if i == 5 || i == 15 || i == 25 {
            fs::write(p, format!("pub fn broken_{i}( {{\n")).unwrap();
        } else {
            fs::write(p, format!("pub fn gen_{i}() -> i32 {{ {i} }}\n")).unwrap();
        }
    }
    fs::write(
        vp2.join("src/lib.rs"),
        "pub fn base() -> i32 { 1 }\npub fn peer() -> i32 { 2 }\n",
    )
    .unwrap();

    let pending = wait_for_radar_status(&vp1, Duration::from_secs(8), has_pending_syntax)
        .expect("PENDING_SYNTAX should appear during broken burst");
    assert!(
        !has_radar_level(&pending, "RED"),
        "PENDING_SYNTAX guard must prevent false RED conflict: {pending}"
    );

    for i in [5, 15, 25] {
        fs::write(
            vp1.join(format!("gen/m{i:02}.rs")),
            format!("pub fn fixed_{i}() -> i32 {{ {i} }}\n"),
        )
        .unwrap();
    }

    wait_for_radar_status(&vp1, Duration::from_secs(8), is_radar_clean_green)
        .expect("PENDING_SYNTAX should clear to GREEN once repaired");
}

// 15. F15 x F17 (Data Flow)
#[test]
fn tier3_f15_f17_debounce_to_radar_status_and_mcp_push() {
    let repo = setup_repo(
        &[("src/lib.rs", "pub fn shared_op(x: i32) -> i32 { x }\n")],
        false,
    );

    repo.loom(&["spawn", "-n", "vp1", "-m", "edit shared_op 1"])
        .assert_success();
    repo.loom(&["spawn", "-n", "vp2", "-m", "edit shared_op 2"])
        .assert_success();

    let vp1 = repo.viewport_dir("vp1");
    let vp2 = repo.viewport_dir("vp2");

    let mut mcp = repo.spawn_mcp_stdio(Some(&vp1)).expect("spawn mcp stdio");
    mcp_handshake_resilient(&mut mcp);

    // Ignored build artifact write must not cause spurious conflict
    fs::create_dir_all(vp1.join("target")).unwrap();
    fs::write(vp1.join("target/noise.o"), b"ignored build noise").unwrap();

    for k in 1..=3 {
        fs::write(
            vp1.join("src/lib.rs"),
            format!("pub fn shared_op(x: i32) -> i32 {{ x + {k} }}\n"),
        )
        .unwrap();
    }
    fs::write(
        vp2.join("src/lib.rs"),
        "pub fn shared_op(x: i32) -> i32 { x * 9 }\n",
    )
    .unwrap();

    let radar_val = wait_for_radar_status(&vp1, Duration::from_secs(8), |v| {
        has_radar_level(v, "RED")
    })
    .expect("Radar must report RED on conflicting shared_op edits");
    assert!(has_radar_level(&radar_val, "RED"));

    let notif = mcp
        .read_until_notification("notifications/loom/radar_conflict", Duration::from_secs(8))
        .expect("MCP push notification notifications/loom/radar_conflict");
    assert!(
        has_radar_level(&notif, "RED") || json_contains_ci(&notif, "shared_op"),
        "unexpected radar_conflict notification payload: {notif}"
    );
}

// 16. F16 x F19 (Data Flow)
#[test]
fn tier3_f16_f19_pending_syntax_visible_in_radar_json() {
    let repo = setup_repo(
        &[("src/app.ts", "export function run(): number { return 1; }\n")],
        false,
    );
    repo.loom(&["spawn", "-n", "vp1", "-m", "ts edit"])
        .assert_success();
    let vp1 = repo.viewport_dir("vp1");

    fs::write(vp1.join("src/app.ts"), "export function run(: number {\n").unwrap();
    wait_for_radar_status(&vp1, Duration::from_secs(8), has_pending_syntax)
        .expect("RADAR_STATUS.json must mark PENDING_SYNTAX");

    let radar_broken = repo
        .loom(&["radar", "--json"])
        .assert_success()
        .stdout_json()
        .expect("radar --json valid json");
    assert!(
        has_pending_syntax(&radar_broken),
        "loom radar --json must surface PENDING_SYNTAX: {radar_broken}"
    );

    fs::write(
        vp1.join("src/app.ts"),
        "export function run(): number { return 42; }\n",
    )
    .unwrap();
    wait_for_radar_status(&vp1, Duration::from_secs(8), is_radar_clean_green)
        .expect("RADAR_STATUS.json must clear PENDING_SYNTAX");

    let radar_fixed = repo
        .loom(&["radar", "--json"])
        .assert_success()
        .stdout_json()
        .expect("radar --json fixed valid json");
    assert!(
        is_radar_clean_green(&radar_fixed),
        "expected clean GREEN radar --json after fix: {radar_fixed}"
    );
}

// 17. F17 x F18 (Data Flow)
#[test]
fn tier3_f17_f18_radar_conflict_drives_mcp_poll_and_push() {
    let repo = setup_repo(&[("src/engine.rs", "pub fn step(v: i32) -> i32 { v }\n")], false);
    repo.loom(&["spawn", "-n", "vp1", "-m", "step 1"])
        .assert_success();
    repo.loom(&["spawn", "-n", "vp2", "-m", "step 2"])
        .assert_success();

    let vp1 = repo.viewport_dir("vp1");
    let vp2 = repo.viewport_dir("vp2");

    let mut mcp = repo.spawn_mcp_stdio(Some(&vp1)).expect("spawn mcp stdio");
    mcp_handshake_resilient(&mut mcp);
    let tools = mcp.tools_list().expect("tools/list");
    for tool_name in [
        "loom_preflight_symbols",
        "loom_radar_poll",
        "loom_record_deadend",
        "loom_seal_capsule",
    ] {
        assert!(
            json_contains_ci(&tools, tool_name),
            "missing MCP tool {tool_name} in {tools}"
        );
    }

    let poll_args = build_tool_args(
        &tools,
        "loom_radar_poll",
        &[(&["viewport_id", "viewport", "viewport_name", "vp"], json!("vp1"))],
    );
    let clean_poll = mcp.tools_call("loom_radar_poll", poll_args.clone()).unwrap();
    assert!(!extract_mcp_is_error(&clean_poll));
    assert!(text_has_word_ci(&mcp_result_text(&clean_poll), "GREEN"));

    fs::write(vp1.join("src/engine.rs"), "pub fn step(v: i32) -> i32 { v + 10 }\n").unwrap();
    fs::write(vp2.join("src/engine.rs"), "pub fn step(v: i32) -> i32 { v + 20 }\n").unwrap();

    let notif = mcp
        .read_until_notification("notifications/loom/radar_conflict", Duration::from_secs(8))
        .expect("conflict notification");
    assert!(has_radar_level(&notif, "RED") || json_contains_ci(&notif, "step"));

    let conflict_poll = mcp.tools_call("loom_radar_poll", poll_args).unwrap();
    let poll_text = mcp_result_text(&conflict_poll);
    assert!(
        text_has_word_ci(&poll_text, "RED") && poll_text.contains("step"),
        "expected RED conflict on step in loom_radar_poll: {poll_text}"
    );
}

// 18. F18 x F20 (Control Flow)
#[test]
fn tier3_f18_f20_mcp_seal_capsule_and_cli_log_parity() {
    let repo = setup_repo(
        &[
            ("target/.keep", ""),
            ("src/lib.rs", "pub fn greet() -> i32 { 1 }\n"),
        ],
        false,
    );
    repo.loom(&["spawn", "-n", "vp1", "-m", "mcp greet"])
        .assert_success();
    let vp1 = repo.viewport_dir("vp1");
    fs::write(vp1.join("src/lib.rs"), "pub fn greet() -> i32 { 7 }\n").unwrap();

    let mut mcp = repo.spawn_mcp_stdio(Some(&vp1)).expect("spawn mcp stdio");
    mcp_handshake_resilient(&mut mcp);
    let tools = mcp.tools_list().expect("tools/list");

    let seal_args = build_tool_args(
        &tools,
        "loom_seal_capsule",
        &[
            (&["viewport_id", "viewport", "viewport_name", "vp"], json!("vp1")),
            (&["verify_cmd", "verify", "command"], json!(rs_metadata_verify_cmd())),
            (&["summary", "message", "intent"], json!("mcp sealed greet")),
        ],
    );
    let seal_res = mcp
        .tools_call("loom_seal_capsule", seal_args)
        .expect("loom_seal_capsule");
    assert!(
        !extract_mcp_is_error(&seal_res),
        "loom_seal_capsule returned error: {seal_res}"
    );

    assert!(repo.read_file("src/lib.rs").unwrap().contains("7"));
    let list_json = repo
        .loom(&["list", "--include-gc", "--json"])
        .assert_success()
        .stdout_json()
        .unwrap();
    let vp1_entry = find_viewport_entry(&list_json, "vp1")
        .unwrap_or_else(|| panic!("expected vp1 in list --include-gc: {list_json}"));
    assert!(
        viewport_has_state(vp1_entry, &["Sealed", "GcPending"]),
        "expected vp1 Sealed after loom_seal_capsule: {vp1_entry}"
    );

    let log_json = repo
        .loom(&["log", "--capsules", "--json"])
        .assert_success()
        .stdout_json()
        .unwrap();
    assert!(json_contains_ci(&log_json, "mcp sealed greet"));
}

// 19. F18 x F21 (Data Flow)
#[test]
fn tier3_f18_f21_mcp_record_deadend_to_preflight() {
    let repo = setup_repo(
        &[(
            "src/auth.rs",
            "pub fn verify_token(t: &str) -> bool { !t.is_empty() }\n",
        )],
        false,
    );
    repo.loom(&["spawn", "-n", "vp1", "-m", "try lock"])
        .assert_success();
    repo.loom(&["spawn", "-n", "vp2", "-m", "retry safely"])
        .assert_success();

    let vp1 = repo.viewport_dir("vp1");
    let vp2 = repo.viewport_dir("vp2");
    fs::write(
        vp1.join("src/auth.rs"),
        "pub fn verify_token(_t: &str) -> bool { false }\n",
    )
    .unwrap();

    let mut mcp = repo.spawn_mcp_stdio(Some(&vp1)).expect("spawn mcp stdio");
    mcp_handshake_resilient(&mut mcp);
    let tools = mcp.tools_list().expect("tools/list");

    let deadend_args = build_tool_args(
        &tools,
        "loom_record_deadend",
        &[
            (&["viewport_id", "viewport", "viewport_name", "vp"], json!("vp1")),
            (&["touched_symbols", "symbols", "symbol_fqns"], json!(["verify_token"])),
            (&["kind", "failure_kind"], json!("DesignDeadEnd")),
            (
                &["lesson_summary", "reason", "lesson", "summary"],
                json!("recursive mutex deadlock on token cache"),
            ),
        ],
    );
    let dead_res = mcp
        .tools_call("loom_record_deadend", deadend_args)
        .expect("loom_record_deadend");
    assert!(!extract_mcp_is_error(&dead_res), "record_deadend failed: {dead_res}");

    if vp1.join("src/auth.rs").exists() {
        assert_eq!(
            fs::read_to_string(vp1.join("src/auth.rs")).unwrap(),
            "pub fn verify_token(t: &str) -> bool { !t.is_empty() }\n"
        );
    }

    let preflight_args = build_tool_args(
        &tools,
        "loom_preflight_symbols",
        &[
            (&["symbols", "touched_symbols", "symbol_fqns"], json!(["verify_token"])),
            (&["viewport_id", "viewport", "viewport_name", "vp"], json!("vp2")),
        ],
    );
    drop(mcp);

    let mut mcp2 = repo.spawn_mcp_stdio(Some(&vp2)).expect("spawn mcp2 stdio");
    mcp_handshake_resilient(&mut mcp2);
    let preflight_res = mcp2
        .tools_call("loom_preflight_symbols", preflight_args)
        .expect("loom_preflight_symbols");
    let text = mcp_result_text(&preflight_res);
    assert!(
        text.contains("recursive mutex deadlock on token cache"),
        "expected NegativeEdge warning in preflight output: {text}"
    );
}

// 20. F19 x F20 (Control Flow)
#[test]
fn tier3_f19_f20_radar_clears_after_seal_or_drop() {
    let repo = setup_repo(&[("src/math.rs", "pub fn calc(x: i32) -> i32 { x }\n")], false);
    repo.loom(&["spawn", "-n", "vp1", "-m", "calc 1"])
        .assert_success();
    repo.loom(&["spawn", "-n", "vp2", "-m", "calc 2"])
        .assert_success();

    let vp1 = repo.viewport_dir("vp1");
    let vp2 = repo.viewport_dir("vp2");
    fs::write(vp1.join("src/math.rs"), "pub fn calc(x: i32) -> i32 { x + 1 }\n").unwrap();
    fs::write(vp2.join("src/math.rs"), "pub fn calc(x: i32) -> i32 { x + 2 }\n").unwrap();

    wait_for_radar_status(&vp1, Duration::from_secs(8), |v| has_radar_level(v, "RED"))
        .expect("vp1 radar RED");

    let r1 = repo.loom(&["radar", "--json"]).assert_success().stdout_json().unwrap();
    assert!(has_radar_level(&r1, "RED"));

    repo.loom_in(
        &vp2,
        &["drop", "--reason", "yield to vp1", "--kind", "SemanticConflict"],
    )
    .assert_success();

    verify_radar_clean_or_green(&repo, &vp1);
    let r2 = repo.loom(&["radar", "--json"]).assert_success().stdout_json().unwrap();
    assert!(
        is_radar_clean_green(&r2),
        "expected clean GREEN radar --json after drop: {r2}"
    );

    repo.loom_in(&vp1, &["seal", "-m", "final calc"])
        .assert_success();
    let r3 = repo.loom(&["radar", "--json"]).assert_success().stdout_json().unwrap();
    assert!(
        !has_radar_level(&r3, "RED") && !has_radar_level(&r3, "YELLOW"),
        "expected no RED/YELLOW after seal: {r3}"
    );
}

// 21. F3 x F21 (Shared State)
#[test]
fn tier3_f03_f21_negative_edge_survives_daemon_restart() {
    let repo = setup_repo(
        &[("src/cache.rs", "pub fn evict_lru() -> usize { 0 }\n")],
        false,
    );
    repo.loom(&["spawn", "-n", "vp1", "-m", "try evict"])
        .assert_success();
    let vp1 = repo.viewport_dir("vp1");
    fs::write(vp1.join("src/cache.rs"), "pub fn evict_lru() -> usize { 1 }\n").unwrap();

    repo.loom_in(
        &vp1,
        &[
            "drop",
            "--reason",
            "evicting while pinned panics",
            "--kind",
            "DesignDeadEnd",
        ],
    )
    .assert_success();

    let pid1 = repo.daemon_pid().expect("pid1");
    kill_pid(pid1);
    wait_until(Duration::from_secs(5), Duration::from_millis(50), "daemon exit", || {
        !is_pid_alive(pid1)
    });

    repo.loom(&["spawn", "-n", "vp2", "-m", "retry evict"])
        .assert_success();
    let pid2 = repo.daemon_pid().expect("pid2");
    assert_ne!(pid2, pid1);

    let vp2 = repo.viewport_dir("vp2");
    let mut mcp = repo.spawn_mcp_stdio(Some(&vp2)).expect("spawn mcp");
    mcp_handshake_resilient(&mut mcp);
    let tools = mcp.tools_list().expect("tools/list");
    let args = build_tool_args(
        &tools,
        "loom_preflight_symbols",
        &[
            (&["symbols", "touched_symbols", "symbol_fqns"], json!(["evict_lru"])),
            (&["viewport_id", "viewport", "viewport_name", "vp"], json!("vp2")),
        ],
    );
    let res = mcp.tools_call("loom_preflight_symbols", args).unwrap();
    let text = mcp_result_text(&res);
    assert!(
        text.contains("evicting while pinned panics"),
        "NegativeEdge must survive daemon restart via state.redb: {text}"
    );
}

// 22. F21 x F22 (Data Flow)
#[test]
fn tier3_f21_f22_negative_edge_replicated_via_git_notes() {
    let repo_a = TestRepo::new().expect("repo_a");
    let bare = repo_a.init_bare_remote().expect("bare");
    let bare_str = bare.to_string_lossy().into_owned();

    repo_a
        .write_file("src/parser.rs", "pub fn parse_hdr() -> bool { true }\n")
        .unwrap();
    repo_a.init_git_repo().expect("init_git_repo");
    repo_a
        .git(&["remote", "add", "origin", &bare_str])
        .assert_success();
    repo_a.git(&["push", "-u", "origin", "main"]).assert_success();
    repo_a
        .loom(&["init", "--colocate-git", "--json"])
        .assert_success();

    let repo_b = setup_git_clone_from_bare(&bare);
    repo_b
        .loom(&["init", "--colocate-git", "--json"])
        .assert_success();

    repo_a
        .loom(&["spawn", "-n", "vp_ok", "-m", "baseline helper"])
        .assert_success();
    let vp_ok = repo_a.viewport_dir("vp_ok");
    fs::write(
        vp_ok.join("src/parser.rs"),
        "pub fn parse_hdr() -> bool { true }\npub fn ok_helper() -> bool { true }\n",
    )
    .unwrap();
    repo_a
        .loom_in(&vp_ok, &["seal", "-m", "baseline capsule"])
        .assert_success();

    repo_a
        .loom(&["spawn", "-n", "vp_fail", "-m", "recursive attempt"])
        .assert_success();
    let vp_fail = repo_a.viewport_dir("vp_fail");
    fs::write(
        vp_fail.join("src/parser.rs"),
        "pub fn parse_hdr() -> bool { false }\npub fn ok_helper() -> bool { true }\n",
    )
    .unwrap();
    repo_a
        .loom_in(
            &vp_fail,
            &[
                "drop",
                "--reason",
                "unbounded recursion on malformed header",
                "--kind",
                "PerformanceRegression",
            ],
        )
        .assert_success();

    repo_a
        .loom(&["git", "push", "origin", "main"])
        .assert_success();

    let refs = repo_a
        .git_in(&bare, &["show-ref"])
        .assert_success()
        .stdout_str();
    assert!(refs.contains("refs/notes/loom"));

    repo_b
        .loom(&["git", "pull", "origin", "main"])
        .assert_success();

    let mut mcp = repo_b.spawn_mcp_stdio(None).expect("repo_b mcp");
    mcp_handshake_resilient(&mut mcp);
    let tools = mcp.tools_list().expect("tools/list");
    let args = build_tool_args(
        &tools,
        "loom_preflight_symbols",
        &[
            (&["symbols", "touched_symbols", "symbol_fqns"], json!(["parse_hdr"])),
            (&["viewport_id", "viewport", "viewport_name", "vp"], json!("default")),
        ],
    );
    let res = mcp.tools_call("loom_preflight_symbols", args).unwrap();
    let text = mcp_result_text(&res);
    assert!(
        text.contains("unbounded recursion on malformed header"),
        "repo_b preflight must surface NegativeEdge replicated via refs/notes/loom: {text}"
    );
}

// 23. F10 x F22 (Control Flow)
#[test]
fn tier3_f10_f22_active_viewport_rebased_on_git_pull() {
    let repo_a = TestRepo::new().expect("repo_a");
    let bare = repo_a.init_bare_remote().expect("bare");
    let bare_str = bare.to_string_lossy().into_owned();

    repo_a
        .write_file(
            "src/lib.rs",
            "pub fn alpha() -> i32 { 1 }\npub fn beta() -> i32 { 2 }\n",
        )
        .unwrap();
    repo_a.init_git_repo().expect("init_git_repo");
    repo_a
        .git(&["remote", "add", "origin", &bare_str])
        .assert_success();
    repo_a.git(&["push", "-u", "origin", "main"]).assert_success();
    repo_a
        .loom(&["init", "--colocate-git", "--json"])
        .assert_success();

    repo_a
        .loom(&["spawn", "-n", "vp_active", "-m", "edit beta"])
        .assert_success();
    let vp_active = repo_a.viewport_dir("vp_active");
    fs::write(
        vp_active.join("src/lib.rs"),
        "pub fn alpha() -> i32 { 1 }\npub fn beta() -> i32 { 20 }\n",
    )
    .unwrap();

    let ext = setup_git_clone_from_bare(&bare);
    let ext_header: String = (0..10).map(|i| format!("// ext header {i}\n")).collect();
    ext.write_file(
        "src/lib.rs",
        &format!("{ext_header}pub fn alpha() -> i32 {{ 100 }}\npub fn beta() -> i32 {{ 2 }}\n"),
    )
    .unwrap();
    ext.git(&["add", "src/lib.rs"]).assert_success();
    ext.git(&["commit", "-m", "external alpha update"])
        .assert_success();
    ext.git(&["push", "origin", "main"]).assert_success();

    repo_a
        .loom(&["git", "pull", "origin", "main"])
        .assert_success();

    assert!(repo_a.read_file("src/lib.rs").unwrap().contains("100"));
    wait_until(
        Duration::from_secs(8),
        Duration::from_millis(100),
        "vp_active rebased with external alpha and local beta",
        || {
            fs::read_to_string(vp_active.join("src/lib.rs"))
                .map(|s| s.contains("100") && s.contains("20") && s.contains("// ext header 9"))
                .unwrap_or(false)
        },
    );

    repo_a
        .loom_in(&vp_active, &["seal", "-m", "rebased beta"])
        .assert_success();

    let trunk = repo_a.read_file("src/lib.rs").unwrap();
    assert!(trunk.contains("alpha() -> i32 { 100 }"));
    assert!(trunk.contains("beta() -> i32 { 20 }"));
    assert!(trunk.contains("// ext header 9"));
}

// 24. F5 x F20 (Shared State)
#[test]
fn tier3_f05_f20_crash_during_unstaged_patches_then_seal() {
    let repo = setup_repo(
        &[
            ("target/.keep", ""),
            (
                "src/lib.rs",
                "pub fn one() -> i32 { 1 }\npub fn two() -> i32 { 2 }\n",
            ),
        ],
        false,
    );

    repo.loom(&["spawn", "-n", "vp1", "-m", "unstaged patch chain"])
        .assert_success();
    let vp1 = repo.viewport_dir("vp1");

    let ts0 = wait_for_radar_status(&vp1, Duration::from_secs(5), is_radar_clean_green)
        .ok()
        .and_then(|v| v.get("updated_at_ms").and_then(Value::as_u64))
        .unwrap_or(0);

    fs::write(
        vp1.join("src/lib.rs"),
        "pub fn one() -> i32 { 11 }\npub fn two() -> i32 { 2 }\n",
    )
    .unwrap();
    let st1 = wait_for_radar_status(&vp1, Duration::from_secs(10), |v| {
        is_radar_clean_green(v)
            && v.get("updated_at_ms").and_then(Value::as_u64).unwrap_or(0) > ts0
    })
    .expect("expected radar update after write 1");
    let ts1 = st1
        .get("updated_at_ms")
        .and_then(Value::as_u64)
        .unwrap_or(ts0);

    fs::write(
        vp1.join("src/lib.rs"),
        "pub fn one() -> i32 { 11 }\npub fn two() -> i32 { 22 }\n",
    )
    .unwrap();
    wait_for_radar_status(&vp1, Duration::from_secs(10), |v| {
        is_radar_clean_green(v)
            && v.get("updated_at_ms").and_then(Value::as_u64).unwrap_or(0) > ts1
    })
    .expect("expected radar update after write 2 before kill");

    let pid1 = repo.daemon_pid().expect("daemon pid before crash");
    kill_pid(pid1);
    wait_until(Duration::from_secs(5), Duration::from_millis(50), "daemon dead", || {
        !is_pid_alive(pid1)
    });

    repo.loom_in(
        &vp1,
        &[
            "seal",
            "-v",
            rs_metadata_verify_cmd(),
            "-m",
            "recovered seal",
        ],
    )
    .assert_success();

    let trunk = repo.read_file("src/lib.rs").unwrap();
    assert!(trunk.contains("one() -> i32 { 11 }"));
    assert!(trunk.contains("two() -> i32 { 22 }"));

    let caps = repo
        .loom(&["log", "--capsules", "--json"])
        .assert_success()
        .stdout_json()
        .unwrap();
    assert!(json_contains_ci(&caps, "recovered seal"));
}
