use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use serde_json::{json, Value};

use e2e_suite::{loom_bin, CommandOutput, HarnessError, McpClient, TestRepo};

// ---------------------------------------------------------------------------
// Shared Opaque-Box Helpers
// ---------------------------------------------------------------------------

const STATUS_LIKE_KEYS: &[&str] = &[
    "status",
    "severity",
    "level",
    "state",
    "kind",
    "overall_status",
    "merge_outcome",
    "color",
    "error",
    "errors",
    "statuses",
    "states",
];

const RECOGNIZED_RADAR_SCHEMA_KEYS: &[&str] = &[
    "viewports",
    "matrix",
    "conflicts",
    "pending_syntax",
    "radar",
    "summary",
];

fn write_vp_file(vp: &Path, rel: &str, content: &str) {
    let target = vp.join(rel);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).unwrap_or_else(|e| {
            panic!("failed to create parent dirs for {}: {e}", target.display())
        });
    }
    fs::write(&target, content)
        .unwrap_or_else(|e| panic!("failed to write {}: {e}", target.display()));
}

fn token_matches_ci(s: &str, token: &str) -> bool {
    let norm_token: String = token
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    if norm_token.is_empty() {
        return false;
    }
    let has_sep_in_token = token.contains('_') || token.contains('-');
    let has_mixed_case =
        token.chars().any(|c| c.is_ascii_uppercase()) && token.chars().any(|c| c.is_ascii_lowercase());
    if has_sep_in_token || has_mixed_case {
        let whole_norm: String = s
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        if whole_norm == norm_token {
            return true;
        }
        return s
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
            .any(|w| {
                let wn: String = w
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric())
                    .collect::<String>()
                    .to_ascii_lowercase();
                wn == norm_token
            });
    }
    s.eq_ignore_ascii_case(token)
        || s.split(|c: char| !c.is_ascii_alphanumeric())
            .any(|w| w.eq_ignore_ascii_case(token))
}

fn json_has_token_ci_scoped(val: &Value, token: &str, in_status_key: bool) -> bool {
    match val {
        Value::String(s) => in_status_key && token_matches_ci(s, token),
        Value::Array(arr) => arr
            .iter()
            .any(|v| json_has_token_ci_scoped(v, token, in_status_key)),
        Value::Object(map) => map.iter().any(|(k, v)| {
            let is_status_key = STATUS_LIKE_KEYS
                .iter()
                .any(|sk| k.eq_ignore_ascii_case(sk));
            json_has_token_ci_scoped(v, token, is_status_key)
        }),
        _ => false,
    }
}

fn json_has_token_ci(val: &Value, token: &str) -> bool {
    json_has_token_ci_scoped(val, token, false)
}

fn value_has_meaningful_content(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                u > 0
            } else if let Some(i) = n.as_i64() {
                i > 0
            } else if let Some(f) = n.as_f64() {
                f > 0.0
            } else {
                false
            }
        }
        Value::String(s) => {
            let trimmed = s.trim();
            !trimmed.is_empty()
                && !trimmed.eq_ignore_ascii_case("none")
                && !trimmed.eq_ignore_ascii_case("false")
                && !trimmed.eq_ignore_ascii_case("0")
                && !trimmed.eq_ignore_ascii_case("green")
                && !trimmed.eq_ignore_ascii_case("ok")
                && !trimmed.eq_ignore_ascii_case("clean")
                && !trimmed.eq_ignore_ascii_case("clear")
        }
        Value::Array(arr) => arr.iter().any(value_has_meaningful_content),
        Value::Object(map) => map.values().any(value_has_meaningful_content),
        Value::Null => false,
    }
}

fn json_has_nonempty_key_ci(val: &Value, candidate_keys: &[&str]) -> bool {
    match val {
        Value::Array(arr) => arr
            .iter()
            .any(|v| json_has_nonempty_key_ci(v, candidate_keys)),
        Value::Object(map) => map.iter().any(|(k, v)| {
            let key_hit = candidate_keys.iter().any(|ck| k.eq_ignore_ascii_case(ck));
            (key_hit && value_has_meaningful_content(v))
                || json_has_nonempty_key_ci(v, candidate_keys)
        }),
        _ => false,
    }
}

fn item_is_pending_syntax_entry(item: &Value) -> bool {
    match item {
        Value::String(s) => {
            token_matches_ci(s, "PENDING_SYNTAX")
                || token_matches_ci(s, "PendingSyntax")
                || token_matches_ci(s, "pending-syntax")
        }
        _ => radar_is_pending_syntax(item),
    }
}

fn item_is_yellow_entry(item: &Value) -> bool {
    match item {
        Value::String(s) => token_matches_ci(s, "YELLOW"),
        _ => json_has_token_ci(item, "YELLOW") && !json_has_token_ci(item, "RED"),
    }
}

fn conflict_value_has_red_or_non_pending_entry(val: &Value) -> bool {
    match val {
        Value::Array(items) => items.iter().any(|item| {
            value_has_meaningful_content(item)
                && !item_is_yellow_entry(item)
                && !item_is_pending_syntax_entry(item)
        }),
        Value::Object(map) => {
            if let Some(red_val) = map
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("red"))
                .map(|(_, v)| v)
            {
                return value_has_meaningful_content(red_val);
            }
            if let Some(items_val) = map
                .iter()
                .find(|(k, _)| {
                    k.eq_ignore_ascii_case("items")
                        || k.eq_ignore_ascii_case("entries")
                        || k.eq_ignore_ascii_case("list")
                })
                .map(|(_, v)| v)
            {
                return conflict_value_has_red_or_non_pending_entry(items_val);
            }
            value_has_meaningful_content(val)
                && !item_is_yellow_entry(val)
                && !item_is_pending_syntax_entry(val)
                && !map
                    .iter()
                    .any(|(k, v)| k.eq_ignore_ascii_case("yellow") && value_has_meaningful_content(v))
        }
        _ => {
            value_has_meaningful_content(val)
                && !item_is_yellow_entry(val)
                && !item_is_pending_syntax_entry(val)
        }
    }
}

fn has_non_pending_red_conflict_entry(val: &Value) -> bool {
    match val {
        Value::Array(arr) => arr.iter().any(has_non_pending_red_conflict_entry),
        Value::Object(map) => map.iter().any(|(k, v)| {
            let is_conflict_key =
                k.eq_ignore_ascii_case("conflicts") || k.eq_ignore_ascii_case("shadow_conflicts");
            (is_conflict_key && conflict_value_has_red_or_non_pending_entry(v))
                || has_non_pending_red_conflict_entry(v)
        }),
        _ => false,
    }
}

fn radar_is_pending_syntax(val: &Value) -> bool {
    json_has_token_ci(val, "PENDING_SYNTAX")
        || json_has_token_ci(val, "PendingSyntax")
        || json_has_token_ci(val, "pending-syntax")
        || json_has_nonempty_key_ci(val, &["pending_syntax", "pending_files", "syntax_errors"])
}

fn radar_has_red_conflict(val: &Value) -> bool {
    json_has_token_ci(val, "RED")
        || json_has_token_ci(val, "StructuralConflict")
        || has_non_pending_red_conflict_entry(val)
}

fn radar_has_any_conflict(val: &Value) -> bool {
    json_has_token_ci(val, "RED")
        || json_has_token_ci(val, "YELLOW")
        || json_has_token_ci(val, "StructuralConflict")
        || json_has_token_ci(val, "SemanticConflict")
        || json_has_nonempty_key_ci(val, &["conflicts", "shadow_conflicts"])
}

fn json_has_recognized_radar_schema(val: &Value) -> bool {
    match val {
        Value::Object(map) => map.iter().any(|(k, v)| {
            RECOGNIZED_RADAR_SCHEMA_KEYS
                .iter()
                .any(|rk| k.eq_ignore_ascii_case(rk))
                || json_has_recognized_radar_schema(v)
        }),
        Value::Array(arr) => arr.iter().any(json_has_recognized_radar_schema),
        _ => false,
    }
}

fn radar_has_failure_or_error_state(val: &Value) -> bool {
    if json_has_nonempty_key_ci(val, &["error", "errors", "blocked"]) {
        return true;
    }
    for bad_tok in [
        "ERROR",
        "FAILED",
        "FAIL",
        "CONFLICT",
        "BLOCKED",
        "CRASHED",
        "UNKNOWN",
        "StructuralConflict",
        "SemanticConflict",
    ] {
        if json_has_token_ci(val, bad_tok) {
            return true;
        }
    }
    false
}

fn radar_is_green_and_clean(val: &Value) -> bool {
    if radar_has_any_conflict(val)
        || radar_is_pending_syntax(val)
        || radar_has_failure_or_error_state(val)
    {
        return false;
    }
    let has_clean_token = json_has_token_ci(val, "GREEN")
        || json_has_token_ci(val, "OK")
        || json_has_token_ci(val, "CLEAN")
        || json_has_token_ci(val, "CLEAR");
    has_clean_token || json_has_recognized_radar_schema(val)
}

type RadarStamp = (Option<Vec<u8>>, Option<SystemTime>);

fn capture_radar_stamp(vp: &Path) -> RadarStamp {
    let path = vp.join(".loom").join("RADAR_STATUS.json");
    let bytes = fs::read(&path).ok();
    let mtime = fs::metadata(&path).and_then(|m| m.modified()).ok();
    (bytes, mtime)
}

fn wait_for_radar_after_edit<F>(
    repo: &TestRepo,
    vp: &Path,
    timeout: Duration,
    pre_stamp: &RadarStamp,
    min_settle: Duration,
    predicate: F,
) -> e2e_suite::Result<Value>
where
    F: FnMut(&Value) -> bool,
{
    let settle_deadline = Instant::now() + min_settle;
    loop {
        let cur = capture_radar_stamp(vp);
        let changed = match (&pre_stamp.0, &cur.0) {
            (None, Some(_)) => true,
            (Some(prev_bytes), Some(cur_bytes)) if prev_bytes != cur_bytes => true,
            _ => match (pre_stamp.1, cur.1) {
                (None, Some(_)) => true,
                (Some(prev_t), Some(cur_t)) if cur_t != prev_t => true,
                _ => false,
            },
        };
        if changed {
            break;
        }
        let now = Instant::now();
        if now >= settle_deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(10).min(settle_deadline - now));
    }
    repo.wait_for_radar_status(vp, timeout, predicate)
}

fn json_contains_key_or_string(val: &Value, needle: &str) -> bool {
    match val {
        Value::String(s) => s.contains(needle),
        Value::Array(arr) => arr.iter().any(|v| json_contains_key_or_string(v, needle)),
        Value::Object(map) => map
            .iter()
            .any(|(k, v)| k.contains(needle) || json_contains_key_or_string(v, needle)),
        _ => false,
    }
}

fn json_mentions_all(val: &Value, needles: &[&str]) -> bool {
    needles
        .iter()
        .all(|needle| json_contains_key_or_string(val, needle))
}

fn assert_no_rust_panic(stdout: &str, stderr: &str, context: &str) {
    for (label, text) in [("stdout", stdout), ("stderr", stderr)] {
        assert!(
            !text.contains("panicked at")
                && !text.contains("RUST_BACKTRACE")
                && !text.contains("STATUS_DLL_NOT_FOUND")
                && !text.contains("0xC0000135")
                && !text.contains("-1073741515"),
            "{context}: unexpected panic/crash signature in {label}:\nstdout={stdout}\nstderr={stderr}"
        );
    }
}

fn assert_cli_clean_error(out: &CommandOutput, context: &str) {
    out.assert_failure();
    let stdout = out.stdout_str();
    let stderr = out.stderr_str();
    assert_no_rust_panic(&stdout, &stderr, context);
    if let Some(code) = out.exit_code() {
        assert_ne!(code, 101, "{context}: exit code 101 indicates Rust panic");
        assert_ne!(
            code, -1073741515_i32,
            "{context}: exit code 0xC0000135 indicates STATUS_DLL_NOT_FOUND"
        );
    }
    assert!(
        !stderr.trim().is_empty() || !stdout.trim().is_empty(),
        "{context}: expected non-empty error diagnostic"
    );
}

fn assert_mcp_tool_error(res: e2e_suite::Result<Value>, context: &str) {
    match res {
        Err(HarnessError::JsonRpc { code, message, .. }) => {
            assert!(
                code < 0,
                "{context}: expected negative JSON-RPC error code, got {code}"
            );
            assert!(
                !message.trim().is_empty(),
                "{context}: JSON-RPC error message must be non-empty"
            );
        }
        Ok(val) => {
            let is_err = val.get("isError").and_then(Value::as_bool) == Some(true);
            assert!(
                is_err,
                "{context}: expected MCP result.isError == true or JSON-RPC error, got {val}"
            );
        }
        Err(other) => {
            panic!("{context}: expected HarnessError::JsonRpc or Ok(isError=true), got {other:?}")
        }
    }
}

fn assert_mcp_verify_failure_not_schema_error(res: e2e_suite::Result<Value>, context: &str) {
    let err_text = match res {
        Err(HarnessError::JsonRpc {
            code,
            message,
            data,
        }) => {
            assert!(
                code < 0,
                "{context}: expected negative JSON-RPC error code, got {code}"
            );
            assert_ne!(
                code, -32602,
                "{context}: got JSON-RPC -32602 Invalid params instead of verify failure: message={message}, data={data:?}"
            );
            assert!(
                !message.trim().is_empty(),
                "{context}: JSON-RPC error message must be non-empty"
            );
            format!(
                "{} {}",
                message,
                data.as_ref().map(Value::to_string).unwrap_or_default()
            )
        }
        Ok(val) => {
            let is_err = val.get("isError").and_then(Value::as_bool) == Some(true);
            assert!(
                is_err,
                "{context}: expected MCP result.isError == true or JSON-RPC error, got {val}"
            );
            val.to_string()
        }
        Err(other) => {
            panic!("{context}: expected HarnessError::JsonRpc or Ok(isError=true), got {other:?}")
        }
    };

    let lower = err_text.to_ascii_lowercase();
    for bad in [
        "unknown field",
        "missing field",
        "invalid params",
        "invalid type",
    ] {
        assert!(
            !lower.contains(bad),
            "{context}: MCP returned parameter/schema deserialization error ({bad}) instead of verify failure: {err_text}"
        );
    }
}

fn mcp_handshake(client: &mut McpClient) -> Value {
    match client.handshake() {
        Ok(v) => v,
        Err(HarnessError::JsonRpc { .. }) => {
            let v = client
                .initialize_with_version("2025-03-26")
                .expect("MCP initialize fallback 2025-03-26");
            client
                .send_initialized()
                .expect("MCP notifications/initialized after fallback");
            v
        }
        Err(e) => panic!("MCP handshake failed: {e}"),
    }
}

fn find_tool<'a>(tools_list: &'a Value, name: &str) -> Option<&'a Value> {
    tools_list
        .get("tools")
        .and_then(Value::as_array)
        .and_then(|arr| {
            arr.iter()
                .find(|t| t.get("name").and_then(Value::as_str) == Some(name))
        })
}

fn build_seal_args(
    tool_schema: Option<&Value>,
    vp_name: &str,
    verify_cmd: &str,
    summary: &str,
) -> Value {
    let props = tool_schema
        .and_then(|t| t.get("inputSchema"))
        .and_then(|s| s.get("properties"))
        .and_then(Value::as_object);

    let mut obj = serde_json::Map::new();
    if let Some(map) = props {
        let vp_match = ["viewport", "viewport_name", "name", "id"]
            .iter()
            .find(|&&c| map.contains_key(c))
            .copied();
        let verify_match = ["verify_cmd", "verify", "verify_command", "command", "cmd"]
            .iter()
            .find(|&&c| map.contains_key(c))
            .copied();
        let summary_match = ["summary", "message", "intent", "description"]
            .iter()
            .find(|&&c| map.contains_key(c))
            .copied();

        if !map.is_empty() {
            assert!(
                vp_match.is_some(),
                "loom_seal_capsule inputSchema.properties missing recognized viewport field: {map:?}"
            );
            assert!(
                verify_match.is_some(),
                "loom_seal_capsule inputSchema.properties missing recognized verify field: {map:?}"
            );
        }

        if let Some(k) = vp_match {
            obj.insert(k.to_string(), json!(vp_name));
        }
        if let Some(k) = verify_match {
            obj.insert(k.to_string(), json!(verify_cmd));
        }
        if let Some(k) = summary_match {
            obj.insert(k.to_string(), json!(summary));
        }
    } else {
        obj.insert("viewport".to_string(), json!(vp_name));
        obj.insert("verify_cmd".to_string(), json!(verify_cmd));
        obj.insert("summary".to_string(), json!(summary));
    }
    Value::Object(obj)
}

fn mutate_debounce_in_toml_value(val: &mut toml::Value, new_ms: i64) -> bool {
    match val {
        toml::Value::Table(tbl) => {
            let mut changed = false;
            for (k, v) in tbl.iter_mut() {
                if k.to_ascii_lowercase().contains("debounce") && v.is_integer() {
                    *v = toml::Value::Integer(new_ms);
                    changed = true;
                } else if mutate_debounce_in_toml_value(v, new_ms) {
                    changed = true;
                }
            }
            changed
        }
        toml::Value::Array(arr) => {
            let mut changed = false;
            for v in arr.iter_mut() {
                if mutate_debounce_in_toml_value(v, new_ms) {
                    changed = true;
                }
            }
            changed
        }
        _ => false,
    }
}

fn mutate_debounce_in_toml(cfg_raw: &str, new_ms: i64) -> (String, bool) {
    let mut doc: toml::Value = toml::from_str(cfg_raw).expect("parse config.toml");
    let found_existing = mutate_debounce_in_toml_value(&mut doc, new_ms);
    if !found_existing {
        if let Some(root_tbl) = doc.as_table_mut() {
            let radar_tbl = root_tbl
                .entry("radar".to_string())
                .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
            if let Some(rt) = radar_tbl.as_table_mut() {
                rt.insert("debounce_ms".to_string(), toml::Value::Integer(new_ms));
            }
        }
    }
    let updated = toml::to_string_pretty(&doc).expect("serialize config.toml");
    (updated, found_existing)
}

fn terminate_pid_best_effort(pid: u32) {
    if pid == 0 || pid == std::process::id() {
        return;
    }
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
            .args(["-TERM", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        std::thread::sleep(Duration::from_millis(50));
        let _ = Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn kill_daemon_if_running(repo: &TestRepo) {
    if let Some(pid) = repo.daemon_pid() {
        terminate_pid_best_effort(pid);
        std::thread::sleep(Duration::from_millis(120));
    }
}

struct LocalChildGuard {
    child: Option<Child>,
    threads: Vec<JoinHandle<()>>,
}

impl LocalChildGuard {
    fn new(child: Child) -> Self {
        Self {
            child: Some(child),
            threads: Vec::new(),
        }
    }

    fn push_thread(&mut self, handle: JoinHandle<()>) {
        self.threads.push(handle);
    }

    fn wait_timeout(&mut self, timeout: Duration) -> e2e_suite::Result<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(child) = self.child.as_mut() {
                if let Some(status) = child.try_wait()? {
                    self.child = None;
                    self.join_drains_bounded(Duration::from_millis(150));
                    return Ok(status);
                }
            } else {
                return Err(HarnessError::Protocol(
                    "LocalChildGuard child already reaped".into(),
                ));
            }
            if Instant::now() >= deadline {
                return Err(HarnessError::Timeout {
                    context: "raw child exit".into(),
                    details: "process did not exit within bounded timeout".into(),
                });
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn join_drains_bounded(&mut self, grace: Duration) {
        let deadline = Instant::now() + grace;
        while !self.threads.iter().all(JoinHandle::is_finished) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let remaining = std::mem::take(&mut self.threads);
        for h in remaining {
            if h.is_finished() {
                let _ = h.join();
            } else {
                self.threads.push(h);
            }
        }
    }
}

impl Drop for LocalChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            if let Ok(None) = child.try_wait() {
                terminate_pid_best_effort(child.id());
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        self.join_drains_bounded(Duration::from_millis(150));
    }
}

fn spawn_drain_vec<R: Read + Send + 'static>(
    mut reader: R,
    buf: Arc<Mutex<Vec<u8>>>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut guard = buf.lock().unwrap_or_else(|e| e.into_inner());
                    guard.extend_from_slice(&chunk[..n]);
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// F15: File Watcher & Adaptive Batch Debounce (5 Tests)
// ---------------------------------------------------------------------------

#[test]
fn tier2_f15_bulk_codegen_burst_50_files_converges() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.write_file("src/lib.rs", "pub fn seed() -> i32 { 0 }\n")
        .unwrap();
    repo.loom(&["init"]).assert_success();
    repo.loom(&["spawn", "-n", "vp1", "-m", "bulk codegen burst"])
        .assert_success();

    let vp1 = repo.viewport_dir("vp1");
    repo.wait_for_radar_status(&vp1, Duration::from_secs(10), radar_is_green_and_clean)
        .expect("initial RADAR_STATUS.json should converge");

    let pre_stamp = capture_radar_stamp(&vp1);
    for i in 0..50usize {
        write_vp_file(
            &vp1,
            &format!("src/gen/mod_{i:02}.rs"),
            &format!("pub fn gen_fn_{i}() -> usize {{ {i} }}\n"),
        );
    }

    let status = wait_for_radar_after_edit(
        &repo,
        &vp1,
        Duration::from_secs(15),
        &pre_stamp,
        Duration::from_millis(100),
        radar_is_green_and_clean,
    )
    .expect("radar should converge after 50-file burst");
    assert!(!radar_has_red_conflict(&status));

    let radar_json = repo
        .loom(&["radar", "--json"])
        .assert_success()
        .stdout_json()
        .expect("radar --json valid JSON");
    assert!(!radar_has_red_conflict(&radar_json));
    assert!(!radar_is_pending_syntax(&radar_json));

    repo.loom(&["status"]).assert_success();

    std::thread::sleep(Duration::from_millis(250));
    let settled = repo
        .wait_for_radar_status(&vp1, Duration::from_secs(5), radar_is_green_and_clean)
        .expect("RADAR_STATUS.json must remain stable after burst");
    assert!(radar_is_green_and_clean(&settled));

    repo.loom_in(&vp1, &["seal", "-m", "codegen burst"])
        .assert_success();
    assert!(repo.exists("src/gen/mod_00.rs"));
    assert!(repo.exists("src/gen/mod_49.rs"));
}

#[test]
fn tier2_f15_rapid_overwrite_same_file_20_times() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.write_file(
        "src/lib.rs",
        "pub fn target() -> i32 { 0 }\npub fn disjoint() -> i32 { 0 }\n",
    )
    .unwrap();
    repo.loom(&["init"]).assert_success();
    repo.loom(&["spawn", "-n", "vp1", "-m", "burst writer"])
        .assert_success();
    repo.loom(&["spawn", "-n", "vp2", "-m", "baseline conflict"])
        .assert_success();

    let vp1 = repo.viewport_dir("vp1");
    let vp2 = repo.viewport_dir("vp2");
    repo.wait_for_radar_status(&vp1, Duration::from_secs(10), radar_is_green_and_clean)
        .expect("initial vp1 green");
    repo.wait_for_radar_status(&vp2, Duration::from_secs(10), radar_is_green_and_clean)
        .expect("initial vp2 green");

    let pre_vp2_stamp = capture_radar_stamp(&vp2);
    write_vp_file(
        &vp2,
        "src/lib.rs",
        "pub fn target() -> i32 { 999 }\npub fn disjoint() -> i32 { 0 }\n",
    );
    wait_for_radar_after_edit(
        &repo,
        &vp2,
        Duration::from_secs(10),
        &pre_vp2_stamp,
        Duration::from_millis(90),
        radar_is_green_and_clean,
    )
    .expect("vp2 edit should settle green");

    for i in 0..20usize {
        let target_val = if i == 19 { 42 } else { 0 };
        write_vp_file(
            &vp1,
            "src/lib.rs",
            &format!(
                "pub fn target() -> i32 {{ {target_val} }}\npub fn disjoint() -> i32 {{ {i} }}\n"
            ),
        );
    }

    let status = repo
        .wait_for_radar_status(&vp1, Duration::from_secs(10), radar_has_red_conflict)
        .expect("20th overwrite must converge to RED conflict");
    assert!(radar_has_red_conflict(&status));

    let radar_json = repo
        .loom(&["radar", "--json"])
        .assert_success()
        .stdout_json()
        .expect("radar --json valid JSON");
    assert!(radar_has_red_conflict(&radar_json));

    let disk_content = fs::read_to_string(vp1.join("src/lib.rs")).expect("read vp1 src/lib.rs");
    assert!(disk_content.contains("42") && disk_content.contains("19"));
}

#[test]
fn tier2_f15_edit_after_viewport_dropped_ignored() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.write_file(
        "src/lib.rs",
        "pub fn alpha() -> i32 { 1 }\npub fn beta() -> i32 { 2 }\n",
    )
    .unwrap();
    repo.loom(&["init"]).assert_success();
    repo.loom(&["spawn", "-n", "vp1", "-m", "to drop"])
        .assert_success();
    repo.loom(&["spawn", "-n", "vp2", "-m", "survivor"])
        .assert_success();

    let vp1 = repo.viewport_dir("vp1");
    let vp2 = repo.viewport_dir("vp2");
    repo.wait_for_radar_status(&vp1, Duration::from_secs(10), radar_is_green_and_clean)
        .expect("vp1 initial green");
    repo.wait_for_radar_status(&vp2, Duration::from_secs(10), radar_is_green_and_clean)
        .expect("vp2 initial green");

    write_vp_file(
        &vp2,
        "src/lib.rs",
        "pub fn alpha() -> i32 { 20 }\npub fn beta() -> i32 { 2 }\n",
    );
    write_vp_file(
        &vp1,
        "src/lib.rs",
        "pub fn alpha() -> i32 { 10 }\npub fn beta() -> i32 { 2 }\n",
    );

    repo.wait_for_radar_status(&vp2, Duration::from_secs(10), radar_has_red_conflict)
        .expect("vp2 should detect conflict before vp1 drop");

    let drop_named = repo.loom(&["drop", "-n", "vp1", "--reason", "abandoned branch"]);
    if !drop_named.success() {
        let combined_err = format!(
            "{}\n{}",
            drop_named.stdout_str().to_ascii_lowercase(),
            drop_named.stderr_str().to_ascii_lowercase()
        );
        if combined_err.contains("unexpected")
            || combined_err.contains("unknown")
            || combined_err.contains("unrecognized")
            || combined_err.contains("'-n'")
            || combined_err.contains("\"-n\"")
        {
            repo.loom_in(&vp1, &["drop", "--reason", "abandoned branch"])
                .assert_success();
        } else {
            drop_named.assert_success();
        }
    }

    repo.wait_for_radar_status(&vp2, Duration::from_secs(10), radar_is_green_and_clean)
        .expect("vp2 should return green after vp1 drop");

    // Simulate stale external editor writing into dropped vp1 path
    write_vp_file(
        &vp1,
        "src/lib.rs",
        "pub fn alpha() -> i32 { 777 }\npub fn beta() -> i32 { 2 }\n",
    );
    std::thread::sleep(Duration::from_millis(350));

    let vp2_status = repo
        .wait_for_radar_status(&vp2, Duration::from_secs(5), radar_is_green_and_clean)
        .expect("post-drop write to vp1 must not trigger conflict in vp2");
    assert!(!radar_has_red_conflict(&vp2_status));

    let radar_json = repo
        .loom(&["radar", "--json"])
        .assert_success()
        .stdout_json()
        .expect("radar --json valid");
    assert!(!radar_has_red_conflict(&radar_json));
    repo.loom(&["status"]).assert_success();
}

#[test]
fn tier2_f15_nonexistent_ignored_dirs_safe() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.write_file("src/lib.rs", "pub fn base() -> i32 { 1 }\n")
        .unwrap();
    assert!(!repo.exists("target"));
    assert!(!repo.exists("node_modules"));
    assert!(!repo.exists(".venv"));

    repo.loom(&["init"]).assert_success();
    repo.loom(&["spawn", "-n", "vp1", "-m", "ignored dirs absent"])
        .assert_success();

    let vp1 = repo.viewport_dir("vp1");
    repo.wait_for_radar_status(&vp1, Duration::from_secs(10), radar_is_green_and_clean)
        .expect("watcher should start cleanly when ignored dirs do not exist");

    // Create ignored dirs dynamically with broken syntax inside them
    write_vp_file(&vp1, "target/debug/junk.rs", "pub fn broken( { @@@ ");
    write_vp_file(
        &vp1,
        "node_modules/pkg/index.ts",
        "export function broken( { ",
    );
    write_vp_file(&vp1, ".venv/lib/site.py", "def broken(:");

    std::thread::sleep(Duration::from_millis(250));
    let pre_stamp = capture_radar_stamp(&vp1);
    write_vp_file(&vp1, "src/lib.rs", "pub fn base() -> i32 { 2 }\n");

    let status = wait_for_radar_after_edit(
        &repo,
        &vp1,
        Duration::from_secs(10),
        &pre_stamp,
        Duration::from_millis(100),
        radar_is_green_and_clean,
    )
    .expect("broken files inside ignored dirs must not poison radar");
    assert!(!radar_is_pending_syntax(&status));

    let radar_json = repo
        .loom(&["radar", "--json"])
        .assert_success()
        .stdout_json()
        .expect("radar --json valid");
    assert!(!radar_is_pending_syntax(&radar_json));
    assert!(!radar_has_any_conflict(&radar_json));
}

#[test]
fn tier2_f15_zero_or_high_debounce_config_boundary() {
    // Subcase A: 0 ms debounce boundary
    {
        let repo = TestRepo::new().expect("TestRepo::new");
        repo.write_file("src/lib.rs", "pub fn base() -> i32 { 1 }\n")
            .unwrap();
        repo.loom(&["init"]).assert_success();

        let cfg_raw = repo.read_file(".loom/config.toml").expect("read config.toml");
        let (updated, _found_existing) = mutate_debounce_in_toml(&cfg_raw, 0);
        repo.write_file(".loom/config.toml", &updated).unwrap();
        kill_daemon_if_running(&repo);

        let spawn_out = repo.loom(&["spawn", "-n", "vp1", "-m", "zero debounce"]);
        if spawn_out.success() {
            let vp1 = repo.viewport_dir("vp1");
            let pre_stamp = capture_radar_stamp(&vp1);
            write_vp_file(&vp1, "src/lib.rs", "pub fn base() -> i32 { 2 }\n");
            wait_for_radar_after_edit(
                &repo,
                &vp1,
                Duration::from_secs(10),
                &pre_stamp,
                Duration::from_millis(80),
                radar_is_green_and_clean,
            )
            .expect("zero debounce should still converge cleanly");
            repo.loom(&["status"]).assert_success();
        } else {
            assert_cli_clean_error(&spawn_out, "zero debounce config boundary");
        }
    }

    // Subcase B: high debounce boundary (500 ms)
    {
        let repo = TestRepo::new().expect("TestRepo::new");
        repo.write_file("src/lib.rs", "pub fn base() -> i32 { 1 }\n")
            .unwrap();
        repo.loom(&["init"]).assert_success();

        let cfg_raw = repo.read_file(".loom/config.toml").expect("read config.toml");
        let (updated, found_existing) = mutate_debounce_in_toml(&cfg_raw, 500);
        repo.write_file(".loom/config.toml", &updated).unwrap();
        kill_daemon_if_running(&repo);

        let spawn_out = repo.loom(&["spawn", "-n", "vp1", "-m", "high debounce"]);
        if !spawn_out.success() {
            if !found_existing {
                assert_cli_clean_error(&spawn_out, "high debounce fallback config boundary");
                return;
            }
            spawn_out.assert_success();
        }

        let vp1 = repo.viewport_dir("vp1");
        repo.wait_for_radar_status(&vp1, Duration::from_secs(10), radar_is_green_and_clean)
            .expect("high debounce initial status should converge");
        let pre_stamp = capture_radar_stamp(&vp1);
        write_vp_file(&vp1, "src/lib.rs", "pub fn base() -> i32 { 99 }\n");
        wait_for_radar_after_edit(
            &repo,
            &vp1,
            Duration::from_secs(10),
            &pre_stamp,
            Duration::from_millis(620),
            radar_is_green_and_clean,
        )
        .expect("high debounce should converge cleanly");
        repo.loom(&["radar", "--json"]).assert_success();
    }
}

// ---------------------------------------------------------------------------
// F16: Incremental Parse & PENDING_SYNTAX Guard (5 Tests)
// ---------------------------------------------------------------------------

#[test]
fn tier2_f16_multiple_syntax_errors_single_file() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.write_file(
        "src/lib.rs",
        "pub fn one() -> i32 { 1 }\npub fn two() -> i32 { 2 }\npub fn three() -> i32 { 3 }\n",
    )
    .unwrap();
    repo.loom(&["init"]).assert_success();
    repo.loom(&["spawn", "-n", "vp1", "-m", "multi syntax errors"])
        .assert_success();

    let vp1 = repo.viewport_dir("vp1");
    repo.wait_for_radar_status(&vp1, Duration::from_secs(10), radar_is_green_and_clean)
        .expect("initial green");

    // 3 simultaneous syntax errors
    write_vp_file(
        &vp1,
        "src/lib.rs",
        "pub fn one( -> i32 { 1 }\npub fn two() -> i32 { let x = ; x }\npub fn three() -> i32 { 3 + }\n",
    );
    repo.wait_for_radar_status(&vp1, Duration::from_secs(10), radar_is_pending_syntax)
        .expect("multiple errors must mark PENDING_SYNTAX");
    repo.loom(&["status"]).assert_success();

    // Fix only one(), leaving two() and three() broken
    write_vp_file(
        &vp1,
        "src/lib.rs",
        "pub fn one() -> i32 { 1 }\npub fn two() -> i32 { let x = ; x }\npub fn three() -> i32 { 3 + }\n",
    );
    std::thread::sleep(Duration::from_millis(200));
    repo.wait_for_radar_status(&vp1, Duration::from_secs(5), radar_is_pending_syntax)
        .expect("partial fix must keep PENDING_SYNTAX");

    // Fix all 3 functions
    write_vp_file(
        &vp1,
        "src/lib.rs",
        "pub fn one() -> i32 { 1 }\npub fn two() -> i32 { 2 }\npub fn three() -> i32 { 3 }\n",
    );
    repo.wait_for_radar_status(&vp1, Duration::from_secs(10), radar_is_green_and_clean)
        .expect("fixing all errors must clear PENDING_SYNTAX");

    let radar_json = repo
        .loom(&["radar", "--json"])
        .assert_success()
        .stdout_json()
        .expect("radar --json valid");
    assert!(!radar_is_pending_syntax(&radar_json));
}

#[test]
fn tier2_f16_missing_token_node_vs_error_node() {
    let repo = TestRepo::new().expect("TestRepo::new");
    let valid_src = "pub fn alpha() -> i32 {\n    let a = 1;\n    a\n}\n";
    repo.write_file("src/lib.rs", valid_src).unwrap();
    repo.loom(&["init"]).assert_success();
    repo.loom(&["spawn", "-n", "vp1", "-m", "missing vs error node"])
        .assert_success();

    let vp1 = repo.viewport_dir("vp1");
    repo.wait_for_radar_status(&vp1, Duration::from_secs(10), radar_is_green_and_clean)
        .expect("initial green");

    // Phase A: Tree-sitter MISSING closing brace `}`
    write_vp_file(&vp1, "src/lib.rs", "pub fn alpha() -> i32 {\n    let a = 1;\n    a\n");
    repo.wait_for_radar_status(&vp1, Duration::from_secs(10), radar_is_pending_syntax)
        .expect("MISSING node must trigger PENDING_SYNTAX");

    write_vp_file(&vp1, "src/lib.rs", valid_src);
    repo.wait_for_radar_status(&vp1, Duration::from_secs(10), radar_is_green_and_clean)
        .expect("restoring closing brace must return green");

    // Phase B: Tree-sitter ERROR node
    write_vp_file(
        &vp1,
        "src/lib.rs",
        "pub fn alpha( @@@ $$$ ) -> i32 { let = ; }\n",
    );
    repo.wait_for_radar_status(&vp1, Duration::from_secs(10), radar_is_pending_syntax)
        .expect("ERROR node must trigger PENDING_SYNTAX");

    write_vp_file(&vp1, "src/lib.rs", valid_src);
    repo.wait_for_radar_status(&vp1, Duration::from_secs(10), radar_is_green_and_clean)
        .expect("restoring after ERROR node must return green");
}

#[test]
fn tier2_f16_syntax_error_with_concurrent_valid_vp() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.write_file(
        "src/lib.rs",
        "pub fn shared_fn() -> i32 { 1 }\npub fn disjoint_fn() -> i32 { 2 }\n",
    )
    .unwrap();
    repo.loom(&["init"]).assert_success();
    repo.loom(&["spawn", "-n", "vp1", "-m", "broken syntax vp"])
        .assert_success();
    repo.loom(&["spawn", "-n", "vp2", "-m", "valid concurrent vp"])
        .assert_success();

    let vp1 = repo.viewport_dir("vp1");
    let vp2 = repo.viewport_dir("vp2");
    repo.wait_for_radar_status(&vp1, Duration::from_secs(10), radar_is_green_and_clean)
        .expect("vp1 initial green");
    repo.wait_for_radar_status(&vp2, Duration::from_secs(10), radar_is_green_and_clean)
        .expect("vp2 initial green");

    let pre_vp2_stamp = capture_radar_stamp(&vp2);
    write_vp_file(
        &vp2,
        "src/lib.rs",
        "pub fn shared_fn() -> i32 { 100 }\npub fn disjoint_fn() -> i32 { 2 }\n",
    );
    wait_for_radar_after_edit(
        &repo,
        &vp2,
        Duration::from_secs(10),
        &pre_vp2_stamp,
        Duration::from_millis(90),
        radar_is_green_and_clean,
    )
    .expect("vp2 valid edit settles green");

    // Introduce broken syntax in vp1
    write_vp_file(
        &vp1,
        "src/lib.rs",
        "pub fn shared_fn() -> i32 { 1 }\npub fn disjoint_fn() -> i32 { 2 +\n",
    );
    repo.wait_for_radar_status(&vp1, Duration::from_secs(10), radar_is_pending_syntax)
        .expect("vp1 broken syntax must mark PENDING_SYNTAX");

    let vp2_status = repo
        .wait_for_radar_status(&vp2, Duration::from_secs(5), |v| !radar_has_red_conflict(v))
        .expect("vp2 must not suffer false-positive RED conflict");
    assert!(!radar_has_red_conflict(&vp2_status));

    let radar_during = repo
        .loom(&["radar", "--json"])
        .assert_success()
        .stdout_json()
        .expect("radar --json valid");
    assert!(!radar_has_red_conflict(&radar_during));

    // Complete vp1 edit validly on disjoint_fn
    write_vp_file(
        &vp1,
        "src/lib.rs",
        "pub fn shared_fn() -> i32 { 1 }\npub fn disjoint_fn() -> i32 { 200 }\n",
    );
    repo.wait_for_radar_status(&vp1, Duration::from_secs(10), radar_is_green_and_clean)
        .expect("vp1 should converge green once syntax is completed cleanly");
    repo.wait_for_radar_status(&vp2, Duration::from_secs(5), radar_is_green_and_clean)
        .expect("vp2 remains green");
}

#[test]
fn tier2_f16_unclosed_string_literal_or_comment() {
    let repo = TestRepo::new().expect("TestRepo::new");
    let valid_src = "pub fn greet() -> &'static str { \"hello\" }\npub fn count() -> usize { 1 }\n";
    repo.write_file("src/lib.rs", valid_src).unwrap();
    repo.loom(&["init"]).assert_success();
    repo.loom(&["spawn", "-n", "vp1", "-m", "unclosed literal or comment"])
        .assert_success();

    let vp1 = repo.viewport_dir("vp1");
    repo.wait_for_radar_status(&vp1, Duration::from_secs(10), radar_is_green_and_clean)
        .expect("initial green");

    // Phase A: Unclosed string literal
    write_vp_file(
        &vp1,
        "src/lib.rs",
        "pub fn greet() -> &'static str { \"hello\npub fn count() -> usize { 1 }\n",
    );
    repo.wait_for_radar_status(&vp1, Duration::from_secs(10), radar_is_pending_syntax)
        .expect("unclosed string literal must mark PENDING_SYNTAX");
    repo.loom(&["status"]).assert_success();

    write_vp_file(&vp1, "src/lib.rs", valid_src);
    repo.wait_for_radar_status(&vp1, Duration::from_secs(10), radar_is_green_and_clean)
        .expect("closing string literal clears PENDING_SYNTAX");

    // Phase B: Unclosed block comment
    write_vp_file(
        &vp1,
        "src/lib.rs",
        "pub fn greet() -> &'static str { \"hello\" }\n/* unclosed block comment\npub fn count() -> usize { 1 }\n",
    );
    std::thread::sleep(Duration::from_millis(200));
    let status = repo
        .wait_for_radar_status(&vp1, Duration::from_secs(10), |v| {
            radar_is_pending_syntax(v) || radar_is_green_and_clean(v)
        })
        .expect("unclosed block comment must not crash or emit false RED");
    assert!(!radar_has_red_conflict(&status));
    repo.loom(&["status"]).assert_success();

    write_vp_file(
        &vp1,
        "src/lib.rs",
        "pub fn greet() -> &'static str { \"hello\" }\n/* closed block comment */\npub fn count() -> usize { 1 }\n",
    );
    repo.wait_for_radar_status(&vp1, Duration::from_secs(10), radar_is_green_and_clean)
        .expect("closed comment converges green");
}

#[test]
fn tier2_f16_empty_after_truncating_valid_file() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.write_file(
        "src/lib.rs",
        "pub fn existing_api() -> i32 { 42 }\npub fn helper() -> i32 { existing_api() }\n",
    )
    .unwrap();
    repo.loom(&["init"]).assert_success();
    repo.loom(&["spawn", "-n", "vp1", "-m", "truncate to empty"])
        .assert_success();

    let vp1 = repo.viewport_dir("vp1");
    repo.wait_for_radar_status(&vp1, Duration::from_secs(10), radar_is_green_and_clean)
        .expect("initial green");

    // Truncate file to 0 bytes
    let pre_truncate_stamp = capture_radar_stamp(&vp1);
    write_vp_file(&vp1, "src/lib.rs", "");

    let empty_status = wait_for_radar_after_edit(
        &repo,
        &vp1,
        Duration::from_secs(10),
        &pre_truncate_stamp,
        Duration::from_millis(150),
        radar_is_green_and_clean,
    )
    .expect("0-byte truncated file must remain clean without crash or PENDING_SYNTAX");
    assert!(!radar_has_any_conflict(&empty_status));

    repo.loom(&["radar", "--json"]).assert_success();
    repo.loom(&["status"]).assert_success();

    // Restore valid non-empty code
    let pre_restore_stamp = capture_radar_stamp(&vp1);
    write_vp_file(&vp1, "src/lib.rs", "pub fn existing_api() -> i32 { 43 }\n");
    wait_for_radar_after_edit(
        &repo,
        &vp1,
        Duration::from_secs(10),
        &pre_restore_stamp,
        Duration::from_millis(100),
        radar_is_green_and_clean,
    )
    .expect("rewriting valid code after truncation must stay green");
}

// ---------------------------------------------------------------------------
// F17: N×N Shadow Merge & Dual Delivery (5 Tests)
// ---------------------------------------------------------------------------

#[test]
fn tier2_f17_single_viewport_never_self_conflicts() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.write_file(
        "src/lib.rs",
        "pub fn alpha() -> i32 { 1 }\npub fn beta() -> i32 { 2 }\n",
    )
    .unwrap();
    repo.loom(&["init"]).assert_success();
    repo.loom(&["spawn", "-n", "vp1", "-m", "solo viewport"])
        .assert_success();

    let vp1 = repo.viewport_dir("vp1");
    repo.wait_for_radar_status(&vp1, Duration::from_secs(10), radar_is_green_and_clean)
        .expect("initial green");

    write_vp_file(
        &vp1,
        "src/lib.rs",
        "pub fn alpha() -> i32 { 10 }\npub fn beta() -> i32 { 2 }\n",
    );
    std::thread::sleep(Duration::from_millis(40));
    let pre_stamp = capture_radar_stamp(&vp1);
    write_vp_file(
        &vp1,
        "src/lib.rs",
        "pub fn alpha() -> i32 { 20 }\npub fn beta() -> i32 { 2 }\npub fn gamma() -> i32 { 3 }\n",
    );

    let status = wait_for_radar_after_edit(
        &repo,
        &vp1,
        Duration::from_secs(10),
        &pre_stamp,
        Duration::from_millis(100),
        radar_is_green_and_clean,
    )
    .expect("single viewport must never self-conflict");
    assert!(!radar_has_any_conflict(&status));

    let radar_json = repo
        .loom(&["radar", "--json"])
        .assert_success()
        .stdout_json()
        .expect("radar --json valid");
    assert!(!radar_has_any_conflict(&radar_json));
}

#[test]
fn tier2_f17_three_way_conflict_n3_matrix() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.write_file(
        "src/lib.rs",
        "pub fn shared_fn() -> i32 { 1 }\npub fn isolated_fn() -> i32 { 2 }\n",
    )
    .unwrap();
    repo.loom(&["init"]).assert_success();
    repo.loom(&["spawn", "-n", "vp1", "-m", "n3 edit 1"])
        .assert_success();
    repo.loom(&["spawn", "-n", "vp2", "-m", "n3 edit 2"])
        .assert_success();
    repo.loom(&["spawn", "-n", "vp3", "-m", "n3 edit 3"])
        .assert_success();

    let vp1 = repo.viewport_dir("vp1");
    let vp2 = repo.viewport_dir("vp2");
    let vp3 = repo.viewport_dir("vp3");
    for vp in [&vp1, &vp2, &vp3] {
        repo.wait_for_radar_status(vp, Duration::from_secs(10), radar_is_green_and_clean)
            .expect("initial green");
    }

    write_vp_file(
        &vp1,
        "src/lib.rs",
        "pub fn shared_fn() -> i32 { 10 }\npub fn isolated_fn() -> i32 { 2 }\n",
    );
    write_vp_file(
        &vp2,
        "src/lib.rs",
        "pub fn shared_fn() -> i32 { 20 }\npub fn isolated_fn() -> i32 { 2 }\n",
    );
    write_vp_file(
        &vp3,
        "src/lib.rs",
        "pub fn shared_fn() -> i32 { 30 }\npub fn isolated_fn() -> i32 { 2 }\n",
    );

    for vp in [&vp1, &vp2, &vp3] {
        repo.wait_for_radar_status(vp, Duration::from_secs(10), radar_has_red_conflict)
            .expect("3-way collision must report RED across all 3 viewports");
    }

    let radar_json = repo
        .loom(&["radar", "--json"])
        .assert_success()
        .stdout_json()
        .expect("radar --json valid");
    assert!(radar_has_red_conflict(&radar_json));
    assert!(
        json_mentions_all(&radar_json, &["vp1", "vp2", "vp3"]),
        "expected N=3 radar output to mention vp1, vp2, vp3; got {radar_json}"
    );
}

#[test]
fn tier2_f17_seal_clears_conflict_when_resolved() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.write_file(
        "src/lib.rs",
        "pub fn alpha() -> i32 { 1 }\npub fn gamma() -> i32 { 2 }\n",
    )
    .unwrap();
    repo.loom(&["init"]).assert_success();
    repo.loom(&["spawn", "-n", "vp1", "-m", "resolver"])
        .assert_success();
    repo.loom(&["spawn", "-n", "vp2", "-m", "keeper"])
        .assert_success();

    let vp1 = repo.viewport_dir("vp1");
    let vp2 = repo.viewport_dir("vp2");
    repo.wait_for_radar_status(&vp1, Duration::from_secs(10), radar_is_green_and_clean)
        .expect("vp1 initial green");
    repo.wait_for_radar_status(&vp2, Duration::from_secs(10), radar_is_green_and_clean)
        .expect("vp2 initial green");

    write_vp_file(
        &vp1,
        "src/lib.rs",
        "pub fn alpha() -> i32 { 10 }\npub fn gamma() -> i32 { 2 }\n",
    );
    write_vp_file(
        &vp2,
        "src/lib.rs",
        "pub fn alpha() -> i32 { 20 }\npub fn gamma() -> i32 { 2 }\n",
    );

    repo.wait_for_radar_status(&vp1, Duration::from_secs(10), radar_has_red_conflict)
        .expect("vp1 should report RED");
    repo.wait_for_radar_status(&vp2, Duration::from_secs(10), radar_has_red_conflict)
        .expect("vp2 should report RED");

    // vp1 resolves conflict by reverting alpha() and editing disjoint gamma()
    write_vp_file(
        &vp1,
        "src/lib.rs",
        "pub fn alpha() -> i32 { 1 }\npub fn gamma() -> i32 { 30 }\n",
    );
    repo.loom_in(&vp1, &["seal", "-m", "resolved gamma"])
        .assert_success();

    let trunk = repo.read_file("src/lib.rs").expect("read trunk src/lib.rs");
    assert!(trunk.contains("30"), "trunk should contain sealed gamma edit");

    repo.wait_for_radar_status(&vp2, Duration::from_secs(10), radar_is_green_and_clean)
        .expect("vp2 conflict must clear once vp1 resolves and seals");

    let radar_json = repo
        .loom(&["radar", "--json"])
        .assert_success()
        .stdout_json()
        .expect("radar --json valid");
    assert!(!radar_has_any_conflict(&radar_json));

    let vp2_disk = fs::read_to_string(vp2.join("src/lib.rs")).expect("read vp2 src/lib.rs");
    assert!(vp2_disk.contains("20"));
}

#[test]
fn tier2_f17_radar_status_json_present_immediately_on_spawn() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.write_file("src/lib.rs", "pub fn ok() -> i32 { 1 }\n")
        .unwrap();
    repo.loom(&["init"]).assert_success();
    repo.loom(&["spawn", "-n", "vp1", "-m", "immediate radar file"])
        .assert_success();

    // Immediately verify presence without sleeping or triggering file edits
    let status_path = repo
        .viewport_dir("vp1")
        .join(".loom")
        .join("RADAR_STATUS.json");
    assert!(
        status_path.is_file(),
        "RADAR_STATUS.json must exist immediately after `loom spawn` returns: {}",
        status_path.display()
    );

    let raw = fs::read_to_string(&status_path).expect("read RADAR_STATUS.json immediately");
    let val: Value = serde_json::from_str(&raw).expect("RADAR_STATUS.json must be valid JSON");
    assert!(
        radar_is_green_and_clean(&val),
        "initial RADAR_STATUS.json should be clean/green, got {val}"
    );
}

#[test]
fn tier2_f17_concurrent_readers_of_radar_status_json() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.write_file(
        "src/lib.rs",
        "pub fn alpha() -> i32 { 0 }\npub fn beta() -> i32 { 0 }\n",
    )
    .unwrap();
    repo.loom(&["init"]).assert_success();
    repo.loom(&["spawn", "-n", "vp1", "-m", "concurrent readers 1"])
        .assert_success();
    repo.loom(&["spawn", "-n", "vp2", "-m", "concurrent readers 2"])
        .assert_success();

    let vp1 = repo.viewport_dir("vp1");
    let vp2 = repo.viewport_dir("vp2");
    repo.wait_for_radar_status(&vp1, Duration::from_secs(10), radar_is_green_and_clean)
        .expect("vp1 initial green");

    let status_path = vp1.join(".loom").join("RADAR_STATUS.json");
    let stop = Arc::new(AtomicBool::new(false));
    let mut readers = Vec::new();

    for _ in 0..4 {
        let path_clone = status_path.clone();
        let stop_clone = Arc::clone(&stop);
        readers.push(std::thread::spawn(move || {
            let mut valid_reads: usize = 0;
            let mut sharing_violations: usize = 0;
            let mut partial_json_errors: Vec<String> = Vec::new();
            let mut other_io_errors: Vec<String> = Vec::new();

            while !stop_clone.load(Ordering::Acquire) {
                match fs::read_to_string(&path_clone) {
                    Ok(text) => match serde_json::from_str::<Value>(&text) {
                        Ok(_) => valid_reads += 1,
                        Err(e) => partial_json_errors.push(format!("{e}: {text:?}")),
                    },
                    Err(e) => {
                        if matches!(e.raw_os_error(), Some(32) | Some(5)) {
                            sharing_violations += 1;
                        } else if e.kind() != std::io::ErrorKind::NotFound {
                            other_io_errors.push(e.to_string());
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            (
                valid_reads,
                sharing_violations,
                partial_json_errors,
                other_io_errors,
            )
        }));
    }

    for i in 0..16usize {
        write_vp_file(
            &vp1,
            "src/lib.rs",
            &format!("pub fn alpha() -> i32 {{ 0 }}\npub fn beta() -> i32 {{ {i} }}\n"),
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    write_vp_file(
        &vp1,
        "src/lib.rs",
        "pub fn alpha() -> i32 { 111 }\npub fn beta() -> i32 { 16 }\n",
    );
    write_vp_file(
        &vp2,
        "src/lib.rs",
        "pub fn alpha() -> i32 { 222 }\npub fn beta() -> i32 { 0 }\n",
    );

    repo.wait_for_radar_status(&vp1, Duration::from_secs(10), radar_has_red_conflict)
        .expect("radar must still converge to RED under concurrent readers");

    stop.store(true, Ordering::Release);
    let mut total_valid = 0usize;
    for handle in readers {
        let (valid, sharing, partial, other) = handle.join().expect("reader thread join");
        assert_eq!(
            sharing, 0,
            "atomic write must prevent Windows sharing violations"
        );
        assert!(
            partial.is_empty(),
            "readers observed partial/invalid JSON: {partial:?}"
        );
        assert!(
            other.is_empty(),
            "readers observed unexpected I/O errors: {other:?}"
        );
        total_valid += valid;
    }
    assert!(
        total_valid >= 20,
        "expected >= 20 valid concurrent JSON reads, got {total_valid}"
    );
    repo.loom(&["status"]).assert_success();
}

// ---------------------------------------------------------------------------
// F18: `loom mcp serve` (`rmcp`) Boundary & Error Cases (5 Tests)
// ---------------------------------------------------------------------------

#[test]
fn tier2_f18_mcp_unknown_tool_returns_jsonrpc_error() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.loom(&["init"]).assert_success();

    let mut mcp = repo.spawn_mcp_stdio(None).expect("spawn_mcp_stdio");
    mcp_handshake(&mut mcp);

    let res = mcp.tools_call("loom_nonexistent_tool_xyz", json!({}));
    assert_mcp_tool_error(res, "unknown MCP tool");

    let tools = mcp
        .tools_list()
        .expect("server must stay alive after unknown tool error");
    let arr = tools
        .get("tools")
        .and_then(Value::as_array)
        .expect("tools array");
    assert!(!arr.is_empty(), "tools/list must return registered tools");
    assert_no_rust_panic("", &mcp.stderr_output(), "unknown MCP tool");
}

#[test]
fn tier2_f18_mcp_invalid_json_line_handled_cleanly() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.loom(&["init"]).assert_success();

    let bin = loom_bin();
    let mut child = Command::new(&bin)
        .args(["mcp", "serve"])
        .current_dir(repo.root())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn {} mcp serve: {e}", bin.display()));

    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let mut guard = LocalChildGuard::new(child);

    let stderr_buf = Arc::new(Mutex::new(Vec::new()));
    guard.push_thread(spawn_drain_vec(stderr, Arc::clone(&stderr_buf)));

    let (line_tx, line_rx) = mpsc::channel::<Option<String>>();
    let stdout_thread = std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => {
                    let _ = line_tx.send(None);
                    break;
                }
                Ok(_) => {
                    let trimmed = line.trim().to_string();
                    if !trimmed.is_empty() && line_tx.send(Some(trimmed)).is_err() {
                        break;
                    }
                }
            }
        }
    });
    guard.push_thread(stdout_thread);

    // 1. Valid initialize (with 2025-03-26 fallback) + notifications/initialized
    let mut got_init = false;
    for (req_id, proto_ver) in [(1u64, "2024-11-05"), (10u64, "2025-03-26")] {
        let init_req = json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "method": "initialize",
            "params": {
                "protocolVersion": proto_ver,
                "capabilities": {},
                "clientInfo": { "name": "loom-e2e-raw", "version": "0.1.0" }
            }
        });
        writeln!(stdin, "{init_req}").expect("write initialize");
        stdin.flush().expect("flush initialize");

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut retry_fallback = false;
        while Instant::now() < deadline {
            let rem = deadline.saturating_duration_since(Instant::now());
            match line_rx.recv_timeout(rem) {
                Ok(Some(line)) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&line) {
                        if v.get("id").and_then(Value::as_u64) == Some(req_id) {
                            if v.get("result").is_some() {
                                got_init = true;
                            } else if let Some(err) = v.get("error") {
                                if req_id == 1 {
                                    retry_fallback = true;
                                } else {
                                    panic!(
                                        "MCP initialize failed for both 2024-11-05 and 2025-03-26: {err}"
                                    );
                                }
                            } else {
                                panic!("unexpected initialize response for id={req_id}: {v}");
                            }
                            break;
                        }
                    }
                }
                Ok(None) => panic!("MCP server exited unexpectedly during initialize"),
                Err(_) => break,
            }
        }
        if got_init {
            break;
        }
        if !retry_fallback {
            break;
        }
    }
    assert!(got_init, "expected initialize result");

    let notif = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    writeln!(stdin, "{notif}").expect("write initialized");
    stdin.flush().expect("flush initialized");

    // 2. Write malformed non-JSON line
    stdin
        .write_all(b"{broken-json: [1, 2\n")
        .expect("write malformed line");
    stdin.flush().expect("flush malformed line");

    // 3. Observe optional parse error frame for up to 500ms without failing on clean EOF
    let mut saw_eof = false;
    let err_deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < err_deadline {
        let rem = err_deadline.saturating_duration_since(Instant::now());
        match line_rx.recv_timeout(rem) {
            Ok(Some(line)) => {
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    if let Some(err) = v.get("error") {
                        let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
                        assert!(code < 0, "parse error code must be negative, got {v}");
                    }
                }
            }
            Ok(None) => {
                saw_eof = true;
                break;
            }
            Err(_) => break,
        }
    }

    // 4. If server stayed open, verify subsequent tools/list works cleanly
    if !saw_eof {
        let list_req = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        });
        writeln!(stdin, "{list_req}").expect("write tools/list after invalid JSON line");
        stdin.flush().expect("flush tools/list after invalid JSON line");

        let mut got_list_result = false;
        let list_deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < list_deadline {
            let rem = list_deadline.saturating_duration_since(Instant::now());
            match line_rx.recv_timeout(rem) {
                Ok(Some(line)) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&line) {
                        if v.get("id").and_then(Value::as_u64) == Some(2) {
                            assert!(v.get("result").is_some(), "expected result for id=2: {v}");
                            got_list_result = true;
                            break;
                        } else if let Some(err) = v.get("error") {
                            let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
                            assert!(code < 0, "expected negative JSON-RPC error code: {v}");
                        }
                    }
                }
                Ok(None) => {
                    panic!("MCP server closed stdout (EOF) before responding to tools/list id=2")
                }
                Err(_) => panic!("timed out waiting for tools/list or EOF after invalid JSON"),
            }
        }
        assert!(
            got_list_result,
            "expected tools/list id=2 result when server remained open after invalid JSON"
        );
    }

    // 5. Close stdin (EOF) and verify clean bounded exit with zero panics
    drop(stdin);
    let status = guard
        .wait_timeout(Duration::from_secs(3))
        .expect("MCP server should exit cleanly on EOF");
    if let Some(code) = status.code() {
        assert_ne!(code, 101, "MCP server panicked on invalid JSON line");
        assert_ne!(code, -1073741515_i32, "STATUS_DLL_NOT_FOUND");
    }
    if !saw_eof {
        assert!(
            status.success(),
            "MCP server that survived invalid JSON must exit 0 on stdin EOF, got {status:?}"
        );
    }

    let stderr_str = {
        let g = stderr_buf.lock().unwrap_or_else(|e| e.into_inner());
        String::from_utf8_lossy(&g).into_owned()
    };
    assert_no_rust_panic("", &stderr_str, "mcp invalid json line");
}

#[test]
fn tier2_f18_mcp_missing_required_arguments() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.loom(&["init"]).assert_success();

    let mut mcp = repo.spawn_mcp_stdio(None).expect("spawn_mcp_stdio");
    mcp_handshake(&mut mcp);

    let tools = mcp.tools_list().expect("tools/list");
    let tool_arr = tools
        .get("tools")
        .and_then(Value::as_array)
        .expect("tools array");

    let mut tested_count = 0usize;
    for tool in tool_arr {
        let name = tool
            .get("name")
            .and_then(Value::as_str)
            .expect("tool name");
        let has_required = tool
            .get("inputSchema")
            .and_then(|s| s.get("required"))
            .and_then(Value::as_array)
            .is_some_and(|req| !req.is_empty());

        if has_required {
            let res = mcp.tools_call(name, json!({}));
            assert_mcp_tool_error(res, &format!("missing required args for {name}"));
            tested_count += 1;
        }
    }

    assert!(
        tested_count >= 1,
        "expected at least one MCP tool to declare required arguments in inputSchema: {tools}"
    );

    mcp.tools_list()
        .expect("server must stay alive after missing-args errors");
    assert_no_rust_panic("", &mcp.stderr_output(), "mcp missing required arguments");
}

#[test]
fn tier2_f18_mcp_call_before_initialize() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.loom(&["init"]).assert_success();

    // spawn_mcp_stdio does NOT call initialize/handshake automatically
    let mut mcp = repo
        .spawn_mcp_stdio(None)
        .expect("spawn_mcp_stdio")
        .with_timeout(Duration::from_secs(5));

    let res = mcp.tools_call("loom_radar_poll", json!({}));
    match res {
        Err(HarnessError::JsonRpc { code, message, .. }) => {
            assert!(
                code < 0,
                "pre-initialize JSON-RPC error code should be negative, got {code}"
            );
            assert!(!message.trim().is_empty());
        }
        Err(HarnessError::Protocol(_)) => {
            // Clean transport close / EOF on pre-initialize protocol violation is valid
        }
        Ok(val) => panic!("MCP server must not accept tools/call before initialize: {val}"),
        Err(HarnessError::Timeout { context, details }) => {
            panic!("MCP server hung on pre-initialize tools/call: {context}: {details}")
        }
        Err(other) => panic!("unexpected error for pre-initialize tools/call: {other:?}"),
    }

    assert_no_rust_panic("", &mcp.stderr_output(), "mcp call before initialize");
}

#[test]
fn tier2_f18_mcp_seal_capsule_failing_verify_reports_error() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.write_file("src/lib.rs", "pub fn v() -> i32 { 1 }\n")
        .unwrap();
    repo.loom(&["init"]).assert_success();
    repo.loom(&["spawn", "-n", "vp1", "-m", "mcp fail verify"])
        .assert_success();

    let vp1 = repo.viewport_dir("vp1");
    repo.wait_for_radar_status(&vp1, Duration::from_secs(10), radar_is_green_and_clean)
        .expect("vp1 initial green");
    let pre_stamp = capture_radar_stamp(&vp1);
    write_vp_file(&vp1, "src/lib.rs", "pub fn v() -> i32 { 999 }\n");
    wait_for_radar_after_edit(
        &repo,
        &vp1,
        Duration::from_secs(10),
        &pre_stamp,
        Duration::from_millis(100),
        radar_is_green_and_clean,
    )
    .expect("vp1 edit settled");

    let mut mcp = repo.spawn_mcp_stdio(None).expect("spawn_mcp_stdio");
    mcp_handshake(&mut mcp);

    let tools = mcp.tools_list().expect("tools/list");
    let seal_schema = find_tool(&tools, "loom_seal_capsule");
    assert!(
        seal_schema.is_some(),
        "loom_seal_capsule must be advertised by tools/list: {tools}"
    );

    let fail_cmd = if cfg!(windows) {
        "cmd /C exit 1"
    } else {
        "exit 1"
    };
    let args = build_seal_args(seal_schema, "vp1", fail_cmd, "should abort on failing verify");
    let res = mcp.tools_call("loom_seal_capsule", args);
    assert_mcp_verify_failure_not_schema_error(
        res,
        "loom_seal_capsule with failing verify command",
    );

    let trunk_after = repo.read_file("src/lib.rs").expect("read trunk src/lib.rs");
    assert_eq!(
        trunk_after, "pub fn v() -> i32 { 1 }\n",
        "trunk must remain unmodified when verification fails"
    );

    let vp1_after = fs::read_to_string(vp1.join("src/lib.rs")).expect("read vp1 src/lib.rs");
    assert!(
        vp1_after.contains("999"),
        "vp1 edits should remain intact after failed verify"
    );

    mcp.tools_list()
        .expect("MCP server must stay alive after failing verify");
    repo.loom(&["status"]).assert_success();
    assert_no_rust_panic("", &mcp.stderr_output(), "mcp seal failing verify");
}

// ---------------------------------------------------------------------------
// F19: `loom radar` CLI/TUI (`ratatui`) Boundary & Error Cases (5 Tests)
// ---------------------------------------------------------------------------

#[test]
fn tier2_f19_radar_json_with_zero_viewports() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.loom(&["init"]).assert_success();

    let out = repo.loom(&["radar", "--json"]);
    out.assert_success();
    assert_no_rust_panic(
        &out.stdout_str(),
        &out.stderr_str(),
        "radar json zero viewports",
    );

    let val = out.stdout_json().expect("radar --json must emit valid JSON");
    assert!(val.is_object() || val.is_array());
    assert!(!radar_has_any_conflict(&val));
    assert!(!radar_is_pending_syntax(&val));
    if let Some(arr) = val.get("viewports").and_then(Value::as_array) {
        assert!(arr.is_empty(), "expected 0 viewports, got {val}");
    }
}

#[test]
fn tier2_f19_radar_json_outside_repo_errors_cleanly() {
    let repo = TestRepo::new().expect("TestRepo::new");
    let out = repo.loom(&["radar", "--json"]);
    assert_cli_clean_error(&out, "radar --json outside repo");
    assert!(!repo.exists(".loom"));
}

#[test]
fn tier2_f19_radar_invalid_flag_errors_cleanly() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.loom(&["init"]).assert_success();

    let out = repo.loom(&["radar", "--definitely-invalid-flag-xyz"]);
    assert_cli_clean_error(&out, "radar invalid flag");

    let combined = format!(
        "{}\n{}",
        out.stdout_str().to_ascii_lowercase(),
        out.stderr_str().to_ascii_lowercase()
    );
    assert!(
        combined.contains("unexpected")
            || combined.contains("unknown")
            || combined.contains("unrecognized")
            || combined.contains("usage")
            || combined.contains("definitely-invalid-flag-xyz"),
        "expected CLI flag diagnostic, got: {combined}"
    );

    repo.loom(&["radar", "--json"]).assert_success();
}

#[test]
fn tier2_f19_radar_json_includes_pending_syntax_state() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.write_file("src/lib.rs", "pub fn compute(x: i32) -> i32 { x + 1 }\n")
        .unwrap();
    repo.loom(&["init"]).assert_success();
    repo.loom(&["spawn", "-n", "vp1", "-m", "radar json pending syntax"])
        .assert_success();

    let vp1 = repo.viewport_dir("vp1");
    repo.wait_for_radar_status(&vp1, Duration::from_secs(10), radar_is_green_and_clean)
        .expect("initial green");

    write_vp_file(
        &vp1,
        "src/lib.rs",
        "pub fn compute(x: i32) -> i32 { let y = (x + ; }\n",
    );
    repo.wait_for_radar_status(&vp1, Duration::from_secs(10), radar_is_pending_syntax)
        .expect("RADAR_STATUS.json should enter PENDING_SYNTAX");

    let out1 = repo.loom(&["radar", "--json"]);
    out1.assert_success();
    let val1 = out1.stdout_json().expect("radar --json valid during syntax error");
    assert!(
        radar_is_pending_syntax(&val1),
        "loom radar --json must include PENDING_SYNTAX state, got {val1}"
    );
    assert!(!radar_has_red_conflict(&val1));

    // Repair syntax
    write_vp_file(
        &vp1,
        "src/lib.rs",
        "pub fn compute(x: i32) -> i32 { x + 2 }\n",
    );
    repo.wait_for_radar_status(&vp1, Duration::from_secs(10), radar_is_green_and_clean)
        .expect("RADAR_STATUS.json should clear PENDING_SYNTAX");

    let out2 = repo.loom(&["radar", "--json"]);
    out2.assert_success();
    let val2 = out2.stdout_json().expect("radar --json valid after fix");
    assert!(
        !radar_is_pending_syntax(&val2),
        "loom radar --json must clear PENDING_SYNTAX after fix, got {val2}"
    );
}

#[test]
fn tier2_f19_radar_watch_non_tty_eof_clean_exit() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.loom(&["init"]).assert_success();
    repo.loom(&["spawn", "-n", "vp1", "-m", "watch eof test"])
        .assert_success();

    // Variant A: Piped stdin closed immediately (explicit EOF)
    {
        let bin = loom_bin();
        let mut child = Command::new(&bin)
            .args(["radar", "--watch"])
            .current_dir(repo.root())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("failed to spawn {} radar --watch: {e}", bin.display()));

        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let mut guard = LocalChildGuard::new(child);
        let stdout_buf = Arc::new(Mutex::new(Vec::new()));
        let stderr_buf = Arc::new(Mutex::new(Vec::new()));
        guard.push_thread(spawn_drain_vec(stdout, Arc::clone(&stdout_buf)));
        guard.push_thread(spawn_drain_vec(stderr, Arc::clone(&stderr_buf)));

        // Close stdin write end to trigger immediate EOF
        drop(stdin);

        let status = guard
            .wait_timeout(Duration::from_secs(5))
            .expect("loom radar --watch must exit within 5s on piped stdin EOF");

        let stdout_str = {
            let g = stdout_buf.lock().unwrap_or_else(|e| e.into_inner());
            String::from_utf8_lossy(&g).into_owned()
        };
        let stderr_str = {
            let g = stderr_buf.lock().unwrap_or_else(|e| e.into_inner());
            String::from_utf8_lossy(&g).into_owned()
        };
        assert_no_rust_panic(&stdout_str, &stderr_str, "radar --watch piped EOF");
        let code = status.code().expect("exit code present");
        assert_ne!(code, 101, "exit code 101 indicates Rust panic");
        assert_ne!(code, -1073741515_i32, "STATUS_DLL_NOT_FOUND");
        if !status.success() {
            assert!(
                !stderr_str.trim().is_empty() || !stdout_str.trim().is_empty(),
                "non-zero exit on non-TTY EOF must emit diagnostic message"
            );
        }
    }

    // Variant B: Stdio::null() stdin via TestRepo::spawn_loom_bg
    {
        let bg = repo
            .spawn_loom_bg(&["radar", "--watch"], None, &[])
            .expect("spawn_loom_bg radar --watch");
        let status = bg
            .wait_timeout(Duration::from_secs(5))
            .expect("loom radar --watch must exit within 5s on Stdio::null()");
        let stdout_str = bg.stdout_str();
        let stderr_str = bg.stderr_str();
        assert_no_rust_panic(&stdout_str, &stderr_str, "radar --watch Stdio::null");
        let code = status.code().expect("exit code present");
        assert_ne!(code, 101, "exit code 101 indicates Rust panic");
        assert_ne!(code, -1073741515_i32, "STATUS_DLL_NOT_FOUND");
        if !status.success() {
            assert!(
                !stderr_str.trim().is_empty() || !stdout_str.trim().is_empty(),
                "non-zero exit on Stdio::null() must emit diagnostic message"
            );
        }
    }
}
