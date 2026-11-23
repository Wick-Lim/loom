use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Barrier;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use e2e_suite::{agy_bin, loom_bin, wait_for_radar_status, McpClient, TestRepo};

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
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
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
        String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
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

fn extract_agy_json(stdout: &str) -> Option<Value> {
    let trimmed = stdout.trim();
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        if v.is_object() && v.get("status").and_then(Value::as_str).is_some() {
            return Some(v);
        }
    }
    for line in trimmed.lines().rev() {
        let l = line.trim();
        if l.starts_with('{') {
            if let Ok(v) = serde_json::from_str::<Value>(l) {
                if v.is_object() && v.get("status").and_then(Value::as_str).is_some() {
                    return Some(v);
                }
            }
        }
    }
    if let (Some(start), Some(end)) = (trimmed.find('{'), trimmed.rfind('}')) {
        if start < end {
            if let Ok(v) = serde_json::from_str::<Value>(&trimmed[start..=end]) {
                if v.is_object() && v.get("status").and_then(Value::as_str).is_some() {
                    return Some(v);
                }
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
            keys.iter().copied().find(|k| map.contains_key(*k))
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

fn mcp_call_text(res: &Value) -> String {
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

fn rs_metadata_verify_cmd() -> &'static str {
    "rustc --edition 2021 --crate-type lib --emit=metadata src/lib.rs --out-dir target"
}

fn rs_test_verify_cmd(bin_name: &str) -> String {
    if cfg!(windows) {
        format!(
            "rustc --edition 2021 --test src/lib.rs -o target\\{bin_name}.exe && target\\{bin_name}.exe"
        )
    } else {
        format!(
            "rustc --edition 2021 --test src/lib.rs -o target/{bin_name} && ./target/{bin_name}"
        )
    }
}

fn grep_verify_cmd(needle: &str, rel_file: &str) -> String {
    format!("git grep --no-index -q \"{needle}\" {rel_file}")
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
    files.pop().expect("at least one LMWA WAL file")
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

// Scenario 1
#[test]
fn tier4_parallel_polyglot_rust_ts_go_python_refactor() {
    let rs_init = concat!(
        "/// Existing doc\n",
        "#[inline]\n",
        "pub fn compute_tax(v: i64) -> i64 { v * 10 }\n",
        "pub fn base_rate() -> i64 { 5 }\n",
    );
    let ts_init = concat!(
        "/** Fetch user */\n",
        "export function getUser(id: string): string { return id; }\n",
        "export function ping(): string { return \"ok\"; }\n",
    );
    let go_init = concat!(
        "package pkg\n\n",
        "// HandleItem processes an item.\n",
        "func HandleItem(x int) int { return x + 1 }\n",
        "func Health() bool { return true }\n",
    );
    let py_init = concat!(
        "def traced(fn):\n",
        "    return fn\n\n",
        "# Important worker comment\n",
        "@traced\n",
        "def run_job(x: int) -> int:\n",
        "    return x * 2\n\n",
        "def version() -> int:\n",
        "    return 1\n",
    );

    let repo = setup_repo(
        &[
            ("target/.keep", ""),
            ("src/lib.rs", rs_init),
            ("src/service.ts", ts_init),
            ("pkg/handler.go", go_init),
            ("app/worker.py", py_init),
        ],
        true,
    );

    let git_status = repo.git(&["status", "--porcelain"]).assert_success().stdout_str();
    assert!(!git_status.contains(".loom"), ".loom must not appear untracked in git status: {git_status}");

    for (name, intent) in [
        ("vp-rs", "rust tax refactor"),
        ("vp-ts", "ts service refactor"),
        ("vp-go", "go handler refactor"),
        ("vp-py", "py worker refactor"),
    ] {
        repo.loom(&["spawn", "-n", name, "-m", intent])
            .assert_success();
    }

    let vp_rs = repo.viewport_dir("vp-rs");
    let vp_ts = repo.viewport_dir("vp-ts");
    let vp_go = repo.viewport_dir("vp-go");
    let vp_py = repo.viewport_dir("vp-py");

    fs::write(
        vp_rs.join("src/lib.rs"),
        concat!(
            "use std::fmt::Write;\n\n",
            "/// Existing doc\n",
            "#[inline]\n",
            "pub fn compute_tax(v: i64) -> i64 {\n",
            "    let mut s = String::new();\n",
            "    let _ = write!(&mut s, \"{v}\");\n",
            "    v * 12\n",
            "}\n",
            "pub fn base_rate() -> i64 { 5 }\n\n",
            "/// Bonus calc\n",
            "pub fn bonus(v: i64) -> i64 { v + 50 }\n",
        ),
    )
    .unwrap();

    fs::write(
        vp_ts.join("src/service.ts"),
        concat!(
            "// v2 header\n",
            "import type { ID } from \"./types\";\n",
            "/** Fetch user */\n",
            "export function getUser(id: ID): string { return `user:${id}`; }\n",
            "export function ping(): string { return \"ok\"; }\n",
            "/** List users */\n",
            "export function listUsers(): string[] { return []; }\n",
        ),
    )
    .unwrap();

    fs::write(
        vp_go.join("pkg/handler.go"),
        concat!(
            "package pkg\n\n",
            "import \"fmt\"\n\n",
            "// HandleItem processes an item.\n",
            "func HandleItem(x int) int { _ = fmt.Sprint(x); return x + 2 }\n",
            "func Health() bool { return true }\n",
            "// Audit logs event.\n",
            "func Audit() string { return \"ok\" }\n",
        ),
    )
    .unwrap();

    fs::write(
        vp_py.join("app/worker.py"),
        concat!(
            "import os\n\n",
            "def traced(fn):\n",
            "    return fn\n\n",
            "# Important worker comment\n",
            "@traced\n",
            "def run_job(x: int) -> int:\n",
            "    return x * 3 + (0 if os.name else 0)\n\n",
            "def version() -> int:\n",
            "    return 1\n\n",
            "@traced\n",
            "def cleanup() -> bool:\n",
            "    return True\n",
        ),
    )
    .unwrap();

    for vp in [&vp_rs, &vp_ts, &vp_go, &vp_py] {
        wait_for_radar_status(vp, Duration::from_secs(10), is_radar_clean_green)
            .unwrap_or_else(|e| panic!("radar GREEN failed for {}: {e}", vp.display()));
    }

    let radar_snap = repo.loom(&["radar", "--json"]).assert_success().stdout_json().unwrap();
    assert!(
        is_radar_clean_green(&radar_snap),
        "expected clean GREEN radar --json snapshot: {radar_snap}"
    );

    repo.loom_in(&vp_rs, &["seal", "-v", rs_metadata_verify_cmd(), "-m", "rs refactor"])
        .assert_success();
    repo.loom_in(
        &vp_ts,
        &["seal", "-v", &grep_verify_cmd("listUsers", "src/service.ts"), "-m", "ts refactor"],
    )
    .assert_success();
    repo.loom_in(
        &vp_go,
        &["seal", "-v", &grep_verify_cmd("Audit", "pkg/handler.go"), "-m", "go refactor"],
    )
    .assert_success();
    repo.loom_in(
        &vp_py,
        &["seal", "-v", &grep_verify_cmd("cleanup", "app/worker.py"), "-m", "py refactor"],
    )
    .assert_success();

    let rs_final = repo.read_file("src/lib.rs").unwrap();
    assert!(
        rs_final.contains("use std::fmt::Write;")
            && rs_final.contains("/// Existing doc")
            && rs_final.contains("#[inline]")
            && rs_final.contains("v * 12")
            && rs_final.contains("bonus"),
        "unexpected merged src/lib.rs: {rs_final}"
    );
    let ts_final = repo.read_file("src/service.ts").unwrap();
    assert!(
        ts_final.contains("import type { ID } from \"./types\";")
            && ts_final.contains("/** Fetch user */")
            && ts_final.contains("user:${id}")
            && ts_final.contains("listUsers"),
        "unexpected merged src/service.ts: {ts_final}"
    );
    let go_final = repo.read_file("pkg/handler.go").unwrap();
    assert!(
        go_final.contains("import \"fmt\"")
            && go_final.contains("x + 2")
            && go_final.contains("// Audit logs event.")
            && go_final.contains("func Audit()"),
        "unexpected merged pkg/handler.go: {go_final}"
    );
    let py_final = repo.read_file("app/worker.py").unwrap();
    assert!(
        py_final.contains("import os")
            && py_final.contains("# Important worker comment")
            && py_final.contains("@traced")
            && py_final.contains("x * 3")
            && py_final.contains("def cleanup()"),
        "unexpected merged app/worker.py: {py_final}"
    );

    let capsules = repo
        .loom(&["log", "--capsules", "--json"])
        .assert_success()
        .stdout_json()
        .unwrap();
    for summary in ["rs refactor", "ts refactor", "go refactor", "py refactor"] {
        assert!(json_contains_ci(&capsules, summary), "missing capsule {summary}");
    }
}

// Scenario 2
#[test]
fn tier4_agy_live_agent_mcp_workflow() {
    let agy = agy_bin().expect("agy binary must be installed for tier4_agy_live_agent_mcp_workflow");
    let repo = setup_repo(
        &[
            ("target/.keep", ""),
            ("src/lib.rs", "pub fn calc(x: i32) -> i32 { x + 1 }\n"),
        ],
        true,
    );

    let agy_home = tempfile::Builder::new()
        .prefix("loom-agy-home-")
        .tempdir()
        .expect("create agy temp home");
    let agy_home_str = agy_home.path().to_string_lossy().into_owned();

    let real_profile = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    let rustup_home = std::env::var("RUSTUP_HOME").unwrap_or_else(|_| {
        Path::new(&real_profile)
            .join(".rustup")
            .to_string_lossy()
            .into_owned()
    });
    let cargo_home = std::env::var("CARGO_HOME").unwrap_or_else(|_| {
        Path::new(&real_profile)
            .join(".cargo")
            .to_string_lossy()
            .into_owned()
    });

    let loom_exe = loom_bin();
    let loom_exe_str = loom_exe.to_string_lossy().into_owned();

    let add_out = Command::new(&agy)
        .args(["mcp", "add", "loom", &loom_exe_str, "mcp", "serve"])
        .env("USERPROFILE", &agy_home_str)
        .env("HOME", &agy_home_str)
        .env("RUSTUP_HOME", &rustup_home)
        .env("CARGO_HOME", &cargo_home)
        .stdin(Stdio::null())
        .output()
        .expect("run agy mcp add");
    assert!(
        add_out.status.success(),
        "agy mcp add failed: stdout={}, stderr={}",
        String::from_utf8_lossy(&add_out.stdout),
        String::from_utf8_lossy(&add_out.stderr)
    );

    let list_out = Command::new(&agy)
        .args(["mcp", "list"])
        .env("USERPROFILE", &agy_home_str)
        .env("HOME", &agy_home_str)
        .stdin(Stdio::null())
        .output()
        .expect("run agy mcp list");
    assert!(list_out.status.success());
    let list_combined = format!(
        "{}{}",
        String::from_utf8_lossy(&list_out.stdout),
        String::from_utf8_lossy(&list_out.stderr)
    );
    assert!(list_combined.contains("loom"), "agy mcp list missing loom: {list_combined}");

    let mcp_cfg_path = agy_home.path().join(".gemini").join("config").join("mcp_config.json");
    let mcp_cfg: Value = serde_json::from_str(
        &fs::read_to_string(&mcp_cfg_path).expect("read agy mcp_config.json"),
    )
    .expect("valid mcp_config.json");
    assert!(mcp_cfg.get("mcpServers").and_then(|m| m.get("loom")).is_some());

    let agy_str = agy.to_string_lossy().into_owned();
    let vp_agy = repo.viewport_dir("vp-agy");
    let vp_agy_str = vp_agy.to_string_lossy().into_owned();

    let env_vars = [
        ("USERPROFILE", agy_home_str.as_str()),
        ("HOME", agy_home_str.as_str()),
        ("RUSTUP_HOME", rustup_home.as_str()),
        ("CARGO_HOME", cargo_home.as_str()),
    ];
    let spawn_out = repo.loom_with_env(
        &[
            "spawn",
            "-n",
            "vp-agy",
            "-m",
            "agy mcp session",
            "--",
            &agy_str,
            "-p",
            "Reply with ONLY the word OK.",
            "--output-format",
            "json",
            "--print-timeout",
            "15s",
            "--dangerously-skip-permissions",
            "--settings",
            "{}",
            "--add-dir",
            &vp_agy_str,
        ],
        &env_vars,
    );

    let combined_err = spawn_out.stderr_str();
    assert!(
        !combined_err.contains("unknown setting") && !combined_err.contains("unexpected argument"),
        "agy CLI flag error: {combined_err}"
    );
    let agy_json = extract_agy_json(&spawn_out.stdout_str()).unwrap_or_else(|| {
        panic!(
            "expected valid agy JSON output with status field (exit={:?}): stdout={}, stderr={}",
            spawn_out.exit_code(),
            spawn_out.stdout_str(),
            combined_err
        )
    });
    let status_str = agy_json
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        status_str == "SUCCESS" || status_str == "ERROR",
        "unexpected agy JSON status (exit={:?}): {agy_json}",
        spawn_out.exit_code()
    );
    if status_str == "SUCCESS" {
        assert!(
            spawn_out.success(),
            "agy reported SUCCESS JSON but loom spawn exited non-zero ({:?}): stderr={}",
            spawn_out.exit_code(),
            combined_err
        );
    } else {
        assert_ne!(
            spawn_out.exit_code(),
            Some(2),
            "unexpected CLI usage error exit 2 with agy ERROR json: stderr={combined_err}"
        );
    }

    let list_vps = repo.loom(&["list", "--json"]).assert_success().stdout_json().unwrap();
    if find_viewport_entry(&list_vps, "vp-agy").is_none() || !vp_agy.exists() {
        assert_eq!(
            status_str, "ERROR",
            "vp-agy must remain materialized after SUCCESS agy run; list={list_vps}"
        );
        repo.loom(&["spawn", "-n", "vp-agy", "-m", "agy mcp session"])
            .assert_success();
    }
    let list_vps_after = repo.loom(&["list", "--json"]).assert_success().stdout_json().unwrap();
    assert!(
        find_viewport_entry(&list_vps_after, "vp-agy").is_some(),
        "vp-agy missing from loom list: {list_vps_after}"
    );

    // Deterministically execute the full loom mcp serve stdio workflow
    let mut mcp = repo.spawn_mcp_stdio(Some(&vp_agy)).expect("spawn mcp stdio");
    mcp_handshake_resilient(&mut mcp);
    let tools = mcp.tools_list().expect("tools/list");
    for tool_name in [
        "loom_preflight_symbols",
        "loom_radar_poll",
        "loom_record_deadend",
        "loom_seal_capsule",
    ] {
        assert!(json_contains_ci(&tools, tool_name), "missing tool {tool_name}");
    }

    let preflight_args = build_tool_args(
        &tools,
        "loom_preflight_symbols",
        &[
            (&["symbols", "touched_symbols", "symbol_fqns"], json!(["calc"])),
            (&["viewport_id", "viewport", "viewport_name", "vp"], json!("vp-agy")),
        ],
    );
    let pre_res = mcp.tools_call("loom_preflight_symbols", preflight_args).unwrap();
    assert!(!extract_mcp_is_error(&pre_res));

    fs::write(
        vp_agy.join("src/lib.rs"),
        "pub fn calc(x: i32) -> i32 { x + 1 }\npub fn double(x: i32) -> i32 { x * 2 }\n",
    )
    .unwrap();

    let poll_args = build_tool_args(
        &tools,
        "loom_radar_poll",
        &[(&["viewport_id", "viewport", "viewport_name", "vp"], json!("vp-agy"))],
    );
    wait_until(Duration::from_secs(6), Duration::from_millis(100), "MCP radar GREEN", || {
        let res = mcp.tools_call("loom_radar_poll", poll_args.clone()).unwrap();
        text_has_word_ci(&mcp_call_text(&res), "GREEN")
    });

    let seal_args = build_tool_args(
        &tools,
        "loom_seal_capsule",
        &[
            (&["viewport_id", "viewport", "viewport_name", "vp"], json!("vp-agy")),
            (&["verify_cmd", "verify", "command"], json!(rs_metadata_verify_cmd())),
            (&["summary", "message", "intent"], json!("agy mcp sealed")),
        ],
    );
    let seal_res = mcp.tools_call("loom_seal_capsule", seal_args).unwrap();
    assert!(!extract_mcp_is_error(&seal_res), "MCP seal failed: {seal_res}");

    assert!(repo.read_file("src/lib.rs").unwrap().contains("pub fn double"));
    let caps = repo
        .loom(&["log", "--capsules", "--json"])
        .assert_success()
        .stdout_json()
        .unwrap();
    assert!(json_contains_ci(&caps, "agy mcp sealed"));
}

// Scenario 3
#[test]
fn tier4_deadend_memory_guides_second_agent_retry() {
    let init_src = concat!(
        "pub fn authenticate(token: &str) -> bool {\n",
        "    token == \"secret\"\n",
        "}\n\n",
        "#[cfg(test)]\n",
        "mod tests {\n",
        "    #[test]\n",
        "    fn auth_works() {\n",
        "        assert!(super::authenticate(\"secret\"));\n",
        "        assert!(!super::authenticate(\"bad\"));\n",
        "    }\n",
        "}\n",
    );
    let repo = setup_repo(&[("target/.keep", ""), ("src/lib.rs", init_src)], false);
    let test_cmd = rs_test_verify_cmd("auth_unit_test");

    repo.loom(&["spawn", "-n", "vp1", "-m", "attempt 1"])
        .assert_success();
    let vp1 = repo.viewport_dir("vp1");
    fs::write(
        vp1.join("src/lib.rs"),
        init_src.replace("token == \"secret\"", "false"),
    )
    .unwrap();

    repo.loom_in(&vp1, &["seal", "-v", &test_cmd, "-m", "bad mutex"])
        .assert_failure();
    assert_eq!(repo.read_file("src/lib.rs").unwrap(), init_src);

    repo.loom_in(
        &vp1,
        &[
            "drop",
            "--reason",
            "mutex lock inversion on auth_cache",
            "--kind",
            "DesignDeadEnd",
        ],
    )
    .assert_success();

    repo.loom(&["spawn", "-n", "vp2", "-m", "lock-free retry"])
        .assert_success();
    let vp2 = repo.viewport_dir("vp2");

    let mut mcp = repo.spawn_mcp_stdio(Some(&vp2)).expect("spawn mcp stdio");
    mcp_handshake_resilient(&mut mcp);
    let tools = mcp.tools_list().expect("tools/list");
    let pre_args = build_tool_args(
        &tools,
        "loom_preflight_symbols",
        &[
            (&["symbols", "touched_symbols", "symbol_fqns"], json!(["authenticate"])),
            (&["viewport_id", "viewport", "viewport_name", "vp"], json!("vp2")),
        ],
    );
    let pre_res = mcp.tools_call("loom_preflight_symbols", pre_args).unwrap();
    let text = mcp_call_text(&pre_res);
    let text_lc = text.to_ascii_lowercase();
    assert!(
        text.contains("mutex lock inversion on auth_cache")
            && (text_lc.contains("designdeadend") || text_lc.contains("design_dead_end")),
        "expected NegativeEdge DesignDeadEnd warning in preflight: {text}"
    );
    drop(mcp);

    fs::write(
        vp2.join("src/lib.rs"),
        init_src.replace("token == \"secret\"", "!token.is_empty() && token == \"secret\""),
    )
    .unwrap();
    repo.loom_in(&vp2, &["seal", "-v", &test_cmd, "-m", "lock-free auth"])
        .assert_success();

    assert!(repo.read_file("src/lib.rs").unwrap().contains("!token.is_empty()"));
    let caps = repo
        .loom(&["log", "--capsules", "--json"])
        .assert_success()
        .stdout_json()
        .unwrap();
    assert!(json_contains_ci(&caps, "lock-free auth"));
}

// Scenario 4
#[test]
fn tier4_crash_recovery_mid_session_with_torn_wal() {
    let repo = setup_repo(
        &[("target/.keep", ""), ("src/lib.rs", "pub fn f0() -> i32 { 0 }\n")],
        false,
    );

    repo.loom(&["spawn", "-n", "vp1", "-m", "c1"]).assert_success();
    let vp1 = repo.viewport_dir("vp1");
    fs::write(
        vp1.join("src/lib.rs"),
        "pub fn f0() -> i32 { 0 }\npub fn f1() -> i32 { 1 }\n",
    )
    .unwrap();
    repo.loom_in(&vp1, &["seal", "-m", "c1"]).assert_success();

    repo.loom(&["spawn", "-n", "vp2", "-m", "f2"]).assert_success();
    repo.loom(&["spawn", "-n", "vp3", "-m", "f3"]).assert_success();
    let vp2 = repo.viewport_dir("vp2");
    let vp3 = repo.viewport_dir("vp3");

    fs::write(
        vp2.join("src/lib.rs"),
        "pub fn f0() -> i32 { 0 }\npub fn f1() -> i32 { 1 }\npub fn f2() -> i32 { 2 }\n",
    )
    .unwrap();
    fs::write(
        vp3.join("src/lib.rs"),
        "pub fn f0() -> i32 { 0 }\npub fn f1() -> i32 { 1 }\npub fn f3() -> i32 { 3 }\n",
    )
    .unwrap();
    repo.loom(&["status", "--json"]).assert_success();

    let pid = repo.daemon_pid().expect("daemon pid");
    kill_pid(pid);
    wait_until(Duration::from_secs(5), Duration::from_millis(50), "daemon dead", || {
        !is_pid_alive(pid)
    });

    let wal_file = latest_wal_file(&repo);
    assert!(fs::read(&wal_file).unwrap().starts_with(b"LMWA"));
    let mut f = OpenOptions::new().append(true).open(&wal_file).unwrap();
    f.write_all(b"LMWA\x01\x00\x01\x00\xDE\xAD\xBE").unwrap();
    drop(f);

    repo.loom(&["status", "--json"]).assert_success();
    let list_json = repo.loom(&["list", "--json"]).assert_success().stdout_json().unwrap();
    assert!(find_viewport_entry(&list_json, "vp2").is_some());
    assert!(find_viewport_entry(&list_json, "vp3").is_some());
    let caps = repo
        .loom(&["log", "--capsules", "--json"])
        .assert_success()
        .stdout_json()
        .unwrap();
    assert!(json_contains_ci(&caps, "c1"));

    repo.loom_in(&vp2, &["seal", "-v", rs_metadata_verify_cmd(), "-m", "c2"])
        .assert_success();
    repo.loom_in(&vp3, &["seal", "-v", rs_metadata_verify_cmd(), "-m", "c3"])
        .assert_success();

    let trunk = repo.read_file("src/lib.rs").unwrap();
    for sym in ["fn f0()", "fn f1()", "fn f2()", "fn f3()"] {
        assert!(trunk.contains(sym), "missing {sym} in recovered trunk: {trunk}");
    }
}

// Scenario 5
#[test]
fn tier4_bare_git_roundtrip_with_active_viewport_rebase() {
    let repo_a = TestRepo::new().expect("repo_a");
    let bare = repo_a.init_bare_remote().expect("bare remote");
    let bare_str = bare.to_string_lossy().into_owned();

    repo_a
        .write_file(
            "src/lib.rs",
            "pub fn alpha() -> i32 { 1 }\npub fn beta() -> i32 { 2 }\n",
        )
        .unwrap();
    repo_a.init_git_repo().unwrap();
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

    repo_a.loom(&["spawn", "-n", "vp-a1", "-m", "alpha 10"]).assert_success();
    let vp_a1 = repo_a.viewport_dir("vp-a1");
    fs::write(
        vp_a1.join("src/lib.rs"),
        "pub fn alpha() -> i32 { 10 }\npub fn beta() -> i32 { 2 }\n",
    )
    .unwrap();
    repo_a
        .loom_in(
            &vp_a1,
            &[
                "seal",
                "-v",
                &grep_verify_cmd("alpha", "src/lib.rs"),
                "-m",
                "alpha 10",
            ],
        )
        .assert_success();

    repo_a.loom(&["spawn", "-n", "vp-a2", "-m", "add gamma"]).assert_success();
    let vp_a2 = repo_a.viewport_dir("vp-a2");
    fs::write(
        vp_a2.join("src/lib.rs"),
        "pub fn alpha() -> i32 { 10 }\npub fn beta() -> i32 { 2 }\npub fn gamma() -> i32 { 3 }\n",
    )
    .unwrap();
    repo_a
        .loom_in(
            &vp_a2,
            &[
                "seal",
                "-v",
                &grep_verify_cmd("gamma", "src/lib.rs"),
                "-m",
                "add gamma",
            ],
        )
        .assert_success();

    repo_a.loom(&["spawn", "-n", "vp-dead", "-m", "overflow test"]).assert_success();
    let vp_dead = repo_a.viewport_dir("vp-dead");
    fs::write(
        vp_dead.join("src/lib.rs"),
        "pub fn alpha() -> i32 { 999 }\npub fn beta() -> i32 { 2 }\npub fn gamma() -> i32 { 3 }\n",
    )
    .unwrap();
    repo_a
        .loom_in(
            &vp_dead,
            &["drop", "--reason", "overflow in alpha", "--kind", "TestFailure"],
        )
        .assert_success();

    repo_a.loom(&["git", "push", "origin", "main"]).assert_success();

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

    repo_b.loom(&["git", "pull", "origin", "main"]).assert_success();
    assert!(repo_b.read_file("src/lib.rs").unwrap().contains("fn gamma()"));

    let mut mcp_b = repo_b.spawn_mcp_stdio(None).expect("mcp_b");
    mcp_handshake_resilient(&mut mcp_b);
    let tools_b = mcp_b.tools_list().unwrap();
    let pre_args = build_tool_args(
        &tools_b,
        "loom_preflight_symbols",
        &[
            (&["symbols", "touched_symbols", "symbol_fqns"], json!(["alpha"])),
            (&["viewport_id", "viewport", "viewport_name", "vp"], json!("default")),
        ],
    );
    let pre_res = mcp_b.tools_call("loom_preflight_symbols", pre_args).unwrap();
    assert!(mcp_call_text(&pre_res).contains("overflow in alpha"));
    drop(mcp_b);

    repo_b.loom(&["spawn", "-n", "vp-work", "-m", "beta 200"]).assert_success();
    let vp_work = repo_b.viewport_dir("vp-work");
    fs::write(
        vp_work.join("src/lib.rs"),
        "pub fn alpha() -> i32 { 10 }\npub fn beta() -> i32 { 200 }\npub fn gamma() -> i32 { 3 }\n",
    )
    .unwrap();

    let ext = setup_git_clone_from_bare(&bare);
    let header: String = (0..5).map(|i| format!("// ext shift {i}\n")).collect();
    ext.write_file(
        "src/lib.rs",
        &format!(
            "{header}pub fn alpha() -> i32 {{ 111 }}\npub fn beta() -> i32 {{ 2 }}\npub fn gamma() -> i32 {{ 3 }}\n"
        ),
    )
    .unwrap();
    ext.git(&["add", "src/lib.rs"]).assert_success();
    ext.git(&["commit", "-m", "external alpha 111"]).assert_success();
    ext.git(&["push", "origin", "main"]).assert_success();

    repo_b.loom(&["git", "pull", "origin", "main"]).assert_success();
    wait_until(
        Duration::from_secs(8),
        Duration::from_millis(100),
        "vp-work rebased with external alpha 111 and local beta 200",
        || {
            fs::read_to_string(vp_work.join("src/lib.rs"))
                .map(|s| s.contains("111") && s.contains("200") && s.contains("// ext shift 4"))
                .unwrap_or(false)
        },
    );

    repo_b.loom_in(&vp_work, &["seal", "-m", "rebased beta 200"]).assert_success();
    let final_b = repo_b.read_file("src/lib.rs").unwrap();
    assert!(final_b.contains("alpha() -> i32 { 111 }"));
    assert!(final_b.contains("beta() -> i32 { 200 }"));
    assert!(final_b.contains("gamma() -> i32 { 3 }"));
}

// Scenario 6
#[test]
fn tier4_radar_conflict_detection_mcp_push_and_resolution() {
    let repo = setup_repo(
        &[
            ("target/.keep", ""),
            (
                "src/lib.rs",
                "pub fn compute(x: i32) -> i32 { x }\npub fn helper(x: i32) -> i32 { x + 1 }\n",
            ),
        ],
        false,
    );

    repo.loom(&["spawn", "-n", "vp1", "-m", "compute x2"]).assert_success();
    repo.loom(&["spawn", "-n", "vp2", "-m", "compute x3"]).assert_success();
    let vp1 = repo.viewport_dir("vp1");
    let vp2 = repo.viewport_dir("vp2");

    let mut mcp = repo.spawn_mcp_stdio(None).expect("spawn mcp stdio");
    mcp_handshake_resilient(&mut mcp);
    let tools = mcp.tools_list().unwrap();

    fs::write(
        vp1.join("src/lib.rs"),
        "pub fn compute(x: i32) -> i32 { x * 2 }\npub fn helper(x: i32) -> i32 { x + 1 }\n",
    )
    .unwrap();
    fs::write(
        vp2.join("src/lib.rs"),
        "pub fn compute(x: i32) -> i32 { x * 3 }\npub fn helper(x: i32) -> i32 { x + 1 }\n",
    )
    .unwrap();

    wait_for_radar_status(&vp1, Duration::from_secs(8), |v| has_radar_level(v, "RED"))
        .expect("vp1 RED");
    wait_for_radar_status(&vp2, Duration::from_secs(8), |v| has_radar_level(v, "RED"))
        .expect("vp2 RED");

    let notif = mcp
        .read_until_notification("notifications/loom/radar_conflict", Duration::from_secs(8))
        .expect("radar_conflict notification");
    assert!(has_radar_level(&notif, "RED") || json_contains_ci(&notif, "compute"));

    let poll_args = build_tool_args(
        &tools,
        "loom_radar_poll",
        &[(&["viewport_id", "viewport", "viewport_name", "vp"], json!("vp1"))],
    );
    let poll_red = mcp.tools_call("loom_radar_poll", poll_args.clone()).unwrap();
    let poll_red_txt = mcp_call_text(&poll_red);
    assert!(
        text_has_word_ci(&poll_red_txt, "RED") && poll_red_txt.contains("compute"),
        "expected RED conflict on compute in loom_radar_poll: {poll_red_txt}"
    );

    // vp2 pivots to disjoint helper()
    fs::write(
        vp2.join("src/lib.rs"),
        "pub fn compute(x: i32) -> i32 { x }\npub fn helper(x: i32) -> i32 { x + 100 }\n",
    )
    .unwrap();

    wait_for_radar_status(&vp1, Duration::from_secs(8), is_radar_clean_green)
        .expect("vp1 GREEN after pivot");
    wait_for_radar_status(&vp2, Duration::from_secs(8), is_radar_clean_green)
        .expect("vp2 GREEN after pivot");

    repo.loom_in(&vp1, &["seal", "-v", rs_metadata_verify_cmd(), "-m", "compute x2"])
        .assert_success();
    repo.loom_in(&vp2, &["seal", "-v", rs_metadata_verify_cmd(), "-m", "helper +100"])
        .assert_success();

    let trunk = repo.read_file("src/lib.rs").unwrap();
    assert!(trunk.contains("x * 2") && trunk.contains("x + 100"));
}

// Scenario 7
#[test]
fn tier4_mixed_code_crdt_config_and_binary_assets() {
    let repo_a = TestRepo::new().expect("repo_a");
    let bare = repo_a.init_bare_remote().expect("bare remote");
    let bare_str = bare.to_string_lossy().into_owned();

    let icon_v1 = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3, 4];
    repo_a.write_file("target/.keep", "").unwrap();
    repo_a.write_file("src/lib.rs", "pub fn base() -> i32 { 0 }\n").unwrap();
    repo_a
        .write_file("config.json", r#"{"port":8080,"name":"loom","debug":false}"#)
        .unwrap();
    repo_a
        .write_file("deploy.yaml", "replicas: 1\nenv: staging\n")
        .unwrap();
    repo_a
        .write_file("docs/SPEC.md", "# Spec\n\n## Intro\nBase\n\n## API\nPending\n")
        .unwrap();
    repo_a.write_bytes("assets/icon.bin", &icon_v1).unwrap();

    repo_a.init_git_repo().unwrap();
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

    repo_a.loom(&["spawn", "-n", "vp1", "-m", "vp1 mixed"]).assert_success();
    repo_a.loom(&["spawn", "-n", "vp2", "-m", "vp2 mixed"]).assert_success();
    let vp1 = repo_a.viewport_dir("vp1");
    let vp2 = repo_a.viewport_dir("vp2");

    let icon_v2: Vec<u8> = (0u8..=255u8).collect();
    fs::write(
        vp1.join("src/lib.rs"),
        "pub fn base() -> i32 { 0 }\npub fn a() -> i32 { 1 }\n",
    )
    .unwrap();
    fs::write(
        vp1.join("config.json"),
        r#"{"port":9090,"name":"loom","debug":false}"#,
    )
    .unwrap();
    fs::write(vp1.join("deploy.yaml"), "replicas: 3\nenv: staging\n").unwrap();
    fs::write(
        vp1.join("docs/SPEC.md"),
        "# Spec\n\n## Intro\nUpdated intro\n\n## API\nPending\n",
    )
    .unwrap();
    fs::write(vp1.join("assets/icon.bin"), &icon_v2).unwrap();

    fs::write(
        vp2.join("src/lib.rs"),
        "pub fn base() -> i32 { 0 }\npub fn b() -> i32 { 2 }\n",
    )
    .unwrap();
    fs::write(
        vp2.join("config.json"),
        r#"{"port":8080,"name":"loom","debug":true}"#,
    )
    .unwrap();
    fs::write(vp2.join("deploy.yaml"), "replicas: 1\nenv: production\n").unwrap();
    fs::write(
        vp2.join("docs/SPEC.md"),
        "# Spec\n\n## Intro\nBase\n\n## API\nv1 ready\n",
    )
    .unwrap();

    repo_a.loom_in(&vp1, &["seal", "-m", "vp1 mixed"]).assert_success();
    repo_a.loom_in(&vp2, &["seal", "-m", "vp2 mixed"]).assert_success();

    let cfg: Value = serde_json::from_str(&repo_a.read_file("config.json").unwrap()).unwrap();
    assert_eq!(cfg.get("port").and_then(Value::as_i64), Some(9090));
    assert_eq!(cfg.get("debug").and_then(Value::as_bool), Some(true));
    assert_eq!(repo_a.read_bytes("assets/icon.bin").unwrap(), icon_v2);

    let compile = Command::new("rustc")
        .args(["--edition", "2021", "--crate-type", "lib", "--emit=metadata", "src/lib.rs", "--out-dir", "target"])
        .current_dir(repo_a.root())
        .output()
        .unwrap();
    assert!(compile.status.success(), "merged src/lib.rs failed to compile");

    repo_a.loom(&["git", "push", "origin", "main"]).assert_success();
    repo_b.loom(&["git", "pull", "origin", "main"]).assert_success();

    assert_eq!(repo_b.read_bytes("assets/icon.bin").unwrap(), icon_v2);
    let cfg_b: Value = serde_json::from_str(&repo_b.read_file("config.json").unwrap()).unwrap();
    assert_eq!(cfg_b.get("port").and_then(Value::as_i64), Some(9090));
    assert_eq!(cfg_b.get("debug").and_then(Value::as_bool), Some(true));
    let yaml_b = repo_b.read_file("deploy.yaml").unwrap();
    assert!(yaml_b.contains("replicas: 3") && yaml_b.contains("env: production"));
    let md_b = repo_b.read_file("docs/SPEC.md").unwrap();
    assert!(md_b.contains("Updated intro") && md_b.contains("v1 ready"));
    let rs_b = repo_b.read_file("src/lib.rs").unwrap();
    assert!(rs_b.contains("fn a()") && rs_b.contains("fn b()"));
}

// Scenario 8
#[test]
fn tier4_bulk_codegen_burst_and_syntax_error_resilience() {
    let repo = setup_repo(&[("src/lib.rs", "pub fn root() -> i32 { 1 }\n")], false);
    repo.loom(&["spawn", "-n", "vp1", "-m", "bulk codegen"]).assert_success();
    repo.loom(&["spawn", "-n", "vp2", "-m", "peer change"]).assert_success();

    let vp1 = repo.viewport_dir("vp1");
    let vp2 = repo.viewport_dir("vp2");
    fs::create_dir_all(vp1.join("gen")).unwrap();

    for i in 0..30 {
        let content = if i == 5 || i == 15 {
            format!("pub fn broken_{i}( {{\n")
        } else {
            format!("pub fn gen_{i}() -> i32 {{ {i} }}\n")
        };
        fs::write(vp1.join(format!("gen/m{i:02}.rs")), content).unwrap();
    }
    for i in 0..10 {
        let content = if i == 3 {
            format!("export function brokenTs{i}( {{\n")
        } else {
            format!("export function tsGen{i}(): number {{ return {i}; }}\n")
        };
        fs::write(vp1.join(format!("gen/t{i:02}.ts")), content).unwrap();
    }

    let pending = wait_for_radar_status(&vp1, Duration::from_secs(10), has_pending_syntax)
        .expect("expected PENDING_SYNTAX during codegen burst");
    assert!(
        !has_radar_level(&pending, "RED"),
        "false RED during PENDING_SYNTAX: {pending}"
    );

    let radar_json = repo.loom(&["radar", "--json"]).assert_success().stdout_json().unwrap();
    assert!(
        has_pending_syntax(&radar_json),
        "expected PENDING_SYNTAX in loom radar --json: {radar_json}"
    );

    fs::write(vp1.join("gen/m05.rs"), "pub fn fixed_5() -> i32 { 5 }\n").unwrap();
    fs::write(vp1.join("gen/m15.rs"), "pub fn fixed_15() -> i32 { 15 }\n").unwrap();
    fs::write(
        vp1.join("gen/t03.ts"),
        "export function tsGen3(): number { return 3; }\n",
    )
    .unwrap();
    fs::write(
        vp2.join("src/lib.rs"),
        "pub fn root() -> i32 { 1 }\npub fn peer() -> i32 { 2 }\n",
    )
    .unwrap();

    wait_for_radar_status(&vp1, Duration::from_secs(10), is_radar_clean_green)
        .expect("PENDING_SYNTAX should clear to GREEN");

    repo.loom_in(&vp1, &["seal", "-m", "40 generated files"]).assert_success();
    repo.loom_in(&vp2, &["seal", "-m", "peer fn"]).assert_success();

    for i in 0..30 {
        assert!(repo.exists(format!("gen/m{i:02}.rs")));
    }
    for i in 0..10 {
        assert!(repo.exists(format!("gen/t{i:02}.ts")));
    }
    assert!(repo.read_file("src/lib.rs").unwrap().contains("pub fn peer()"));
}

// Scenario 9
#[test]
fn tier4_lock_and_dep_cache_isolation_concurrent_builds() {
    let repo = TestRepo::new().expect("TestRepo");
    repo.write_file("src/lib.rs", "pub fn core() -> i32 { 0 }\n").unwrap();
    repo.write_file("node_modules/pkg/index.js", "exports.v = 1;\n").unwrap();
    repo.write_file(".venv/Lib/site.py", "SITE = 1\n").unwrap();
    repo.write_bytes("target/debug/deps/libdep.rlib", &vec![0x3Cu8; 16 * 1024])
        .unwrap();
    repo.write_bytes("target/debug/app.exe", b"TRUNK_EXE").unwrap();
    repo.write_file(".cargo-lock", "trunk-lock").unwrap();
    repo.loom(&["init", "--json"]).assert_success();

    std::thread::scope(|s| {
        for i in 1..=3 {
            let r = &repo;
            s.spawn(move || {
                r.loom(&["spawn", "-n", &format!("vp{i}"), "-m", &format!("build {i}")])
                    .assert_success();
            });
        }
    });

    let barrier = Barrier::new(3);
    std::thread::scope(|s| {
        for i in 1..=3 {
            let vp = repo.viewport_dir(&format!("vp{i}"));
            let b = &barrier;
            s.spawn(move || {
                #[cfg(windows)]
                let lock_file = {
                    use std::os::windows::fs::OpenOptionsExt;
                    OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create(true)
                        .truncate(false)
                        .share_mode(0)
                        .open(vp.join(".cargo-lock"))
                        .expect("exclusive lock on vp .cargo-lock")
                };
                #[cfg(not(windows))]
                let lock_file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(vp.join(".cargo-lock"))
                    .expect("open vp .cargo-lock");

                // Ensure all 3 viewports hold exclusive locks simultaneously
                b.wait();

                assert!(vp.join("node_modules/pkg/index.js").is_file());
                assert!(vp.join(".venv/Lib/site.py").is_file());
                assert!(vp.join("target/debug/deps/libdep.rlib").is_file());

                fs::write(vp.join("target/debug/app.exe"), format!("EXE_{i}").as_bytes()).unwrap();
                fs::write(
                    vp.join("src/lib.rs"),
                    format!("pub fn core() -> i32 {{ 0 }}\npub fn build_{i}() -> i32 {{ {i} }}\n"),
                )
                .unwrap();

                b.wait();
                drop(lock_file);
            });
        }
    });

    for i in 1..=3 {
        let vp = repo.viewport_dir(&format!("vp{i}"));
        repo.loom_in(&vp, &["seal", "-m", &format!("build_{i}")])
            .assert_success();
    }

    let trunk_rs = repo.read_file("src/lib.rs").unwrap();
    assert!(trunk_rs.contains("build_1") && trunk_rs.contains("build_2") && trunk_rs.contains("build_3"));
    assert_eq!(repo.read_file(".cargo-lock").unwrap(), "trunk-lock");
    assert_eq!(repo.read_bytes("target/debug/app.exe").unwrap(), b"TRUNK_EXE");
}

// Scenario 10
#[test]
fn tier4_undo_gc_and_idle_daemon_shutdown_lifecycle() {
    let repo = setup_repo(
        &[("target/.keep", ""), ("src/lib.rs", "pub fn base() -> i32 { 0 }\n")],
        false,
    );

    let initial_pid = repo.daemon_pid().expect("initial daemon pid");
    set_idle_timeout_secs(&repo, 2);
    kill_pid(initial_pid);
    wait_until(Duration::from_secs(5), Duration::from_millis(50), "initial daemon dead", || {
        !is_pid_alive(initial_pid)
    });

    repo.loom(&["status", "--json"]).assert_success();
    let pid1 = repo.daemon_pid().expect("pid1");

    for (name, spawn_msg, seal_msg, body) in [
        ("vp1", "spawn vp1", "seal s1", "pub fn base() -> i32 { 0 }\npub fn s1() -> i32 { 1 }\n"),
        ("vp2", "spawn vp2", "seal s2", "pub fn base() -> i32 { 0 }\npub fn s1() -> i32 { 1 }\npub fn s2() -> i32 { 2 }\n"),
        ("vp3", "spawn vp3", "seal s3", "pub fn base() -> i32 { 0 }\npub fn s1() -> i32 { 1 }\npub fn s2() -> i32 { 2 }\npub fn s3() -> i32 { 3 }\n"),
    ] {
        repo.loom(&["spawn", "-n", name, "-m", spawn_msg]).assert_success();
        let vp = repo.viewport_dir(name);
        fs::write(vp.join("src/lib.rs"), body).unwrap();
        repo.loom_in(&vp, &["seal", "-m", seal_msg]).assert_success();
    }

    let log_json = repo.loom(&["log", "--json"]).assert_success().stdout_json().unwrap();
    let items = extract_items(&log_json, &["ops", "entries", "log"]);
    let s3_seq = items
        .iter()
        .filter(|item| {
            json_contains_ci(item, "seal s3")
                || (json_contains_ci(item, "vp3")
                    && item
                        .get("kind")
                        .and_then(Value::as_str)
                        .is_some_and(|k| k.to_ascii_lowercase().contains("seal")))
        })
        .max_by_key(|item| extract_op_seq(item))
        .and_then(extract_op_seq)
        .expect("find vp3 seal op_seq");

    repo.loom(&["undo", "--op", &s3_seq.to_string(), "--json"])
        .assert_success();
    let after_undo = repo.read_file("src/lib.rs").unwrap();
    assert!(after_undo.contains("fn s1()") && after_undo.contains("fn s2()"));
    assert!(!after_undo.contains("fn s3()"), "s3 should be reverted: {after_undo}");

    repo.loom(&["gc", "--prune-viewports", "--json"])
        .assert_success();
    for name in ["vp1", "vp2", "vp3"] {
        assert!(!repo.viewport_dir(name).exists(), "{name} should be pruned");
    }

    wait_until(
        Duration::from_secs(12),
        Duration::from_millis(100),
        "idle daemon shutdown after gc",
        || !is_pid_alive(pid1),
    );

    let respawn_log = repo.loom(&["log", "--json"]);
    respawn_log.assert_success();
    let _ = respawn_log.stdout_json().unwrap();
    let pid2 = repo.daemon_pid().expect("respawned daemon pid");
    assert_ne!(pid2, pid1);
}

// Scenario 11
#[test]
fn tier4_semantic_and_structural_conflict_escalation() {
    let init_src = concat!(
        "pub fn process_order(id: u64) -> u64 { id * 2 }\n",
        "pub fn caller() -> u64 { process_order(1) }\n",
    );
    let repo = setup_repo(&[("target/.keep", ""), ("src/lib.rs", init_src)], false);

    repo.loom(&["spawn", "-n", "vp1", "-m", "mutate signature"]).assert_success();
    repo.loom(&["spawn", "-n", "vp2", "-m", "edit caller stale"]).assert_success();
    repo.loom(&["spawn", "-n", "vp3", "-m", "edit body stale"]).assert_success();

    let vp1 = repo.viewport_dir("vp1");
    let vp2 = repo.viewport_dir("vp2");
    let vp3 = repo.viewport_dir("vp3");

    fs::write(
        vp1.join("src/lib.rs"),
        "pub fn process_order(id: u64, priority: u8) -> u64 { id * (priority as u64) }\npub fn caller() -> u64 { process_order(1) }\n",
    )
    .unwrap();
    repo.loom_in(
        &vp1,
        &[
            "seal",
            "-v",
            &grep_verify_cmd("priority", "src/lib.rs"),
            "-m",
            "add priority arg",
        ],
    )
    .assert_success();

    fs::write(
        vp2.join("src/lib.rs"),
        "pub fn process_order(id: u64) -> u64 { id * 2 }\npub fn caller() -> u64 { process_order(1) + 10 }\n",
    )
    .unwrap();
    fs::write(
        vp3.join("src/lib.rs"),
        "pub fn process_order(id: u64) -> u64 { id * 99 }\npub fn caller() -> u64 { process_order(1) }\n",
    )
    .unwrap();

    wait_for_radar_status(&vp2, Duration::from_secs(8), |v| {
        has_radar_level(v, "YELLOW") || has_radar_level(v, "RED")
    })
    .expect("vp2 conflict escalation");
    wait_for_radar_status(&vp3, Duration::from_secs(8), |v| {
        has_radar_level(v, "YELLOW") || has_radar_level(v, "RED")
    })
    .expect("vp3 conflict escalation");

    let radar_json = repo.loom(&["radar", "--json"]).assert_success().stdout_json().unwrap();
    assert!(has_radar_level(&radar_json, "YELLOW") || has_radar_level(&radar_json, "RED"));

    repo.loom_in(&vp2, &["seal", "-m", "stale semantic caller"]).assert_failure();
    repo.loom_in(&vp3, &["seal", "-m", "stale structural body"]).assert_failure();

    let trunk = repo.read_file("src/lib.rs").unwrap();
    assert!(trunk.contains("priority: u8"));
    assert!(!trunk.contains("<<<<<<<"));
}

// Scenario 12
#[test]
fn tier4_multi_round_git_divergence_and_causal_audit() {
    let repo_a = TestRepo::new().expect("repo_a");
    let bare = repo_a.init_bare_remote().expect("bare remote");
    let bare_str = bare.to_string_lossy().into_owned();

    repo_a
        .write_file("src/lib.rs", "pub fn ledger(x: i64) -> i64 { x }\n")
        .unwrap();
    repo_a.init_git_repo().unwrap();
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

    // Round 1
    repo_a.loom(&["spawn", "-n", "vp-a1", "-m", "audit_a"]).assert_success();
    let vp_a1 = repo_a.viewport_dir("vp-a1");
    fs::write(
        vp_a1.join("src/lib.rs"),
        "pub fn ledger(x: i64) -> i64 { x + 1 }\npub fn audit_a() -> bool { true }\n",
    )
    .unwrap();
    repo_a.loom_in(&vp_a1, &["seal", "-m", "capsule audit_a"]).assert_success();

    repo_a.loom(&["spawn", "-n", "vp-a-dead", "-m", "deadend ledger"]).assert_success();
    let vp_dead = repo_a.viewport_dir("vp-a-dead");
    fs::write(
        vp_dead.join("src/lib.rs"),
        "pub fn ledger(x: i64) -> i64 { x * 999999 }\npub fn audit_a() -> bool { true }\n",
    )
    .unwrap();
    repo_a
        .loom_in(
            &vp_dead,
            &["drop", "--reason", "overflow in ledger_v1", "--kind", "TestFailure"],
        )
        .assert_success();

    repo_a.loom(&["git", "push", "origin", "main"]).assert_success();
    repo_b.loom(&["git", "pull", "origin", "main"]).assert_success();
    assert!(repo_b.read_file("src/lib.rs").unwrap().contains("audit_a"));

    // Round 2: external commit + repo_b local seal
    let ext = setup_git_clone_from_bare(&bare);
    ext.write_file(
        "src/lib.rs",
        "// ext comment\npub fn ledger(x: i64) -> i64 { x + 1 }\npub fn audit_a() -> bool { true }\npub fn ext_hook() -> bool { true }\n",
    )
    .unwrap();
    ext.git(&["add", "src/lib.rs"]).assert_success();
    ext.git(&["commit", "-m", "external hook"]).assert_success();
    ext.git(&["push", "origin", "main"]).assert_success();

    repo_b.loom(&["spawn", "-n", "vp-b1", "-m", "audit_b"]).assert_success();
    let vp_b1 = repo_b.viewport_dir("vp-b1");
    fs::write(
        vp_b1.join("src/lib.rs"),
        "pub fn ledger(x: i64) -> i64 { x + 1 }\npub fn audit_a() -> bool { true }\npub fn audit_b() -> bool { true }\n",
    )
    .unwrap();
    repo_b.loom_in(&vp_b1, &["seal", "-m", "capsule audit_b"]).assert_success();

    repo_b.loom(&["git", "pull", "origin", "main"]).assert_success();
    repo_b.loom(&["git", "push", "origin", "main"]).assert_success();

    // Round 3: repo_a pulls and audits
    repo_a.loom(&["git", "pull", "origin", "main"]).assert_success();
    let final_a = repo_a.read_file("src/lib.rs").unwrap();
    assert!(final_a.contains("audit_a"), "missing audit_a: {final_a}");
    assert!(final_a.contains("audit_b"), "missing audit_b: {final_a}");
    assert!(final_a.contains("ext_hook"), "missing ext_hook: {final_a}");

    let mut mcp_a = repo_a.spawn_mcp_stdio(None).expect("mcp_a");
    mcp_handshake_resilient(&mut mcp_a);
    let tools_a = mcp_a.tools_list().unwrap();
    let pre_args = build_tool_args(
        &tools_a,
        "loom_preflight_symbols",
        &[
            (&["symbols", "touched_symbols", "symbol_fqns"], json!(["ledger"])),
            (&["viewport_id", "viewport", "viewport_name", "vp"], json!("default")),
        ],
    );
    let pre_res = mcp_a.tools_call("loom_preflight_symbols", pre_args).unwrap();
    assert!(mcp_call_text(&pre_res).contains("overflow in ledger_v1"));

    let sym_log = repo_a
        .loom(&["log", "--capsules", "--symbol", "ledger", "--json"])
        .assert_success()
        .stdout_json()
        .unwrap();
    let all_caps = repo_a
        .loom(&["log", "--capsules", "--json"])
        .assert_success()
        .stdout_json()
        .unwrap();
    assert!(
        json_contains_ci(&sym_log, "ledger")
            || json_contains_ci(&sym_log, "audit")
            || json_contains_ci(&all_caps, "ledger"),
        "expected ledger capsule in sym_log={sym_log} or all_caps={all_caps}"
    );
    assert!(json_contains_ci(&all_caps, "capsule audit_a"));
    assert!(json_contains_ci(&all_caps, "capsule audit_b"));
}
