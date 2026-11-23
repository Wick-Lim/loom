use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};
use tempfile::TempDir;

use e2e_suite::{
    agy_bin, loom_bin, wait_for_radar_status, HarnessError, McpClient, TestRepo,
};

fn init_loom_repo_with_files(files: &[(&str, &str)]) -> TestRepo {
    let repo = TestRepo::new().expect("create TestRepo");
    for (rel, content) in files {
        repo.write_file(rel, content).expect("write fixture file");
    }
    repo.loom(&["init"]).assert_success();
    repo
}

fn spawn_vp(repo: &TestRepo, name: &str, intent: &str) -> PathBuf {
    repo.loom(&["spawn", "-n", name, "-m", intent])
        .assert_success();
    repo.viewport_dir(name)
}

fn write_vp_file(vp: &Path, rel: &str, content: &str) {
    let full = vp.join(rel);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).expect("create viewport parent dirs");
    }
    fs::write(&full, content).expect("write viewport file");
}

fn key_value_asserts_status(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                u != 0
            } else if let Some(i) = n.as_i64() {
                i != 0
            } else if let Some(f) = n.as_f64() {
                f != 0.0
            } else {
                false
            }
        }
        Value::Array(arr) => !arr.is_empty(),
        _ => false,
    }
}

fn json_has_status(val: &Value, expected_tokens: &[&str]) -> bool {
    match val {
        Value::String(s) => s
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .filter(|tok| !tok.is_empty())
            .any(|tok| {
                expected_tokens
                    .iter()
                    .any(|exp| tok.eq_ignore_ascii_case(exp))
            }),
        Value::Array(items) => items.iter().any(|item| json_has_status(item, expected_tokens)),
        Value::Object(map) => map.iter().any(|(k, v)| {
            (key_value_asserts_status(v)
                && expected_tokens
                    .iter()
                    .any(|exp| k.eq_ignore_ascii_case(exp)))
                || json_has_status(v, expected_tokens)
        }),
        _ => false,
    }
}

fn json_contains_ident(val: &Value, needle: &str) -> bool {
    let needle_norm = needle.replace('\\', "/").to_ascii_lowercase();
    match val {
        Value::String(s) => s
            .replace('\\', "/")
            .to_ascii_lowercase()
            .contains(&needle_norm),
        Value::Array(items) => items.iter().any(|item| json_contains_ident(item, needle)),
        Value::Object(map) => map.iter().any(|(k, v)| {
            k.replace('\\', "/")
                .to_ascii_lowercase()
                .contains(&needle_norm)
                || json_contains_ident(v, needle)
        }),
        _ => false,
    }
}

fn mcp_handshake(client: &mut McpClient) -> Value {
    let res = match client.handshake() {
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
    };
    let proto = res
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        !proto.is_empty(),
        "MCP initialize must return non-empty protocolVersion: {res}"
    );
    res
}

fn mcp_tool_text(call_res: &Value) -> String {
    assert_ne!(
        call_res.get("isError"),
        Some(&Value::Bool(true)),
        "MCP tool call returned isError=true: {call_res}"
    );
    let mut parts = Vec::new();
    if let Some(sc) = call_res.get("structuredContent") {
        parts.push(sc.to_string());
    }
    if let Some(content) = call_res.get("content").and_then(Value::as_array) {
        for item in content {
            if let Some(t) = item.get("text").and_then(Value::as_str) {
                parts.push(t.to_string());
            }
        }
    }
    if parts.is_empty() {
        parts.push(call_res.to_string());
    }
    parts.join("\n")
}

fn find_tool_props<'a>(tools_res: &'a Value, tool_name: &str) -> Option<&'a Map<String, Value>> {
    let tools = tools_res.get("tools")?.as_array()?;
    let tool = tools
        .iter()
        .find(|t| t.get("name").and_then(Value::as_str) == Some(tool_name))?;
    tool.get("inputSchema")?
        .get("properties")?
        .as_object()
}

fn build_schema_args(
    props: Option<&Map<String, Value>>,
    specs: &[(&[&str], Value)],
    fallback: &[(&str, Value)],
) -> Value {
    let mut obj = Map::new();
    if let Some(prop_map) = props {
        for (candidates, val) in specs {
            if let Some(matched_key) = candidates.iter().find(|k| prop_map.contains_key(**k)) {
                let adapted = match (prop_map[*matched_key].get("type").and_then(Value::as_str), val)
                {
                    (Some("string"), Value::Array(arr)) if !arr.is_empty() => arr[0].clone(),
                    (Some("array"), Value::String(s)) => json!([s]),
                    _ => val.clone(),
                };
                obj.insert((*matched_key).to_string(), adapted);
            }
        }
    }
    if obj.is_empty() {
        for (k, v) in fallback {
            obj.insert((*k).to_string(), v.clone());
        }
    }
    Value::Object(obj)
}

struct TuiChildGuard {
    child: Option<Child>,
    drains: Vec<JoinHandle<()>>,
}

impl TuiChildGuard {
    fn spawn_drain<R: Read + Send + 'static>(
        mut reader: R,
        buf: Arc<Mutex<Vec<u8>>>,
    ) -> JoinHandle<()> {
        thread::spawn(move || {
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

    fn terminate_and_wait(&mut self) {
        if let Some(mut child) = self.child.take() {
            if matches!(child.try_wait(), Ok(None)) {
                let pid = child.id();
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
                }
                let _ = child.kill();
            }
            let _ = child.wait();
        }
        let deadline = Instant::now() + Duration::from_millis(200);
        while !self.drains.iter().all(JoinHandle::is_finished) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        for h in self.drains.drain(..) {
            if h.is_finished() {
                let _ = h.join();
            }
        }
    }
}

impl Drop for TuiChildGuard {
    fn drop(&mut self) {
        self.terminate_and_wait();
    }
}

// ============================================================================
// F15 — File Watcher & Adaptive Batch Debounce (5 tests)
// ============================================================================

#[test]
fn tier1_f15_watch_edit_triggers_radar_update() {
    let repo = init_loom_repo_with_files(&[("src/lib.rs", "pub fn alpha() -> i32 { 1 }\n")]);
    let vp1 = spawn_vp(&repo, "vp1", "watch edit");

    let initial = wait_for_radar_status(&vp1, Duration::from_secs(5), |_| true)
        .expect("initial RADAR_STATUS.json");
    let status_file = vp1.join(".loom").join("RADAR_STATUS.json");
    let initial_raw = fs::read_to_string(&status_file).expect("read initial RADAR_STATUS.json");
    let initial_mtime = fs::metadata(&status_file)
        .ok()
        .and_then(|m| m.modified().ok());

    thread::sleep(Duration::from_millis(30));
    write_vp_file(&vp1, "src/lib.rs", "pub fn alpha() -> i32 { 2 }\n");

    let updated = wait_for_radar_status(&vp1, Duration::from_secs(5), |v| {
        let raw_changed = fs::read_to_string(&status_file)
            .map(|r| r != initial_raw)
            .unwrap_or(false);
        let mtime_changed = fs::metadata(&status_file)
            .ok()
            .and_then(|m| m.modified().ok())
            .zip(initial_mtime)
            .map(|(now_m, prev_m)| now_m > prev_m)
            .unwrap_or(false);
        (v != &initial || raw_changed || mtime_changed) && json_has_status(v, &["GREEN"])
    })
    .expect("RADAR_STATUS.json updates after viewport edit");

    assert!(
        json_has_status(&updated, &["GREEN"]),
        "expected GREEN after single-viewport edit, got: {updated}"
    );
}

#[test]
fn tier1_f15_ignores_dot_loom_self_writes() {
    let repo = init_loom_repo_with_files(&[("src/lib.rs", "pub fn clean() -> i32 { 1 }\n")]);
    let vp1 = spawn_vp(&repo, "vp1", "dot-loom self-write isolation");

    wait_for_radar_status(&vp1, Duration::from_secs(5), |v| {
        json_has_status(v, &["GREEN"])
    })
    .expect("initial GREEN RADAR_STATUS.json");

    fs::write(vp1.join(".loom").join("scratch_probe.tmp"), b"noise\n")
        .expect("write probe in vp .loom");
    fs::write(
        repo.root().join(".loom").join("scratch_probe.tmp"),
        b"noise\n",
    )
    .expect("write probe in root .loom");

    thread::sleep(Duration::from_millis(300));

    let vp_status = wait_for_radar_status(&vp1, Duration::from_secs(3), |v| {
        json_has_status(v, &["GREEN"])
    })
    .expect("RADAR_STATUS.json stays GREEN");
    assert!(
        !json_has_status(&vp_status, &["RED", "YELLOW", "PENDING_SYNTAX"]),
        "unexpected non-GREEN status after .loom probe write: {vp_status}"
    );
    assert!(
        !json_contains_ident(&vp_status, "scratch_probe.tmp"),
        ".loom scratch file leaked into RADAR_STATUS.json: {vp_status}"
    );

    let out = repo.loom(&["radar", "--json"]);
    out.assert_success();
    let radar_json = out.stdout_json().expect("parse radar --json");
    assert!(
        !json_contains_ident(&radar_json, "scratch_probe.tmp"),
        ".loom scratch file leaked into loom radar --json: {radar_json}"
    );
}

#[test]
fn tier1_f15_ignores_build_dirs_target_node_modules() {
    let repo = init_loom_repo_with_files(&[
        ("src/lib.rs", "pub fn ok() -> i32 { 1 }\n"),
        ("target/debug/.keep", ""),
        ("node_modules/pkg/.keep", ""),
        (".venv/lib/.keep", ""),
    ]);
    let vp1 = spawn_vp(&repo, "vp1", "ignore build dirs 1");
    let vp2 = spawn_vp(&repo, "vp2", "ignore build dirs 2");

    wait_for_radar_status(&vp1, Duration::from_secs(5), |v| {
        json_has_status(v, &["GREEN"])
    })
    .expect("vp1 initial GREEN");
    wait_for_radar_status(&vp2, Duration::from_secs(5), |v| {
        json_has_status(v, &["GREEN"])
    })
    .expect("vp2 initial GREEN");

    let broken = "pub fn broken( {{{\n";
    for vp in [&vp1, &vp2] {
        write_vp_file(vp, "target/debug/gen.rs", broken);
        write_vp_file(vp, "node_modules/pkg/index.ts", broken);
        write_vp_file(vp, ".venv/lib/mod.py", broken);
    }

    thread::sleep(Duration::from_millis(300));

    for vp in [&vp1, &vp2] {
        let st = wait_for_radar_status(vp, Duration::from_secs(3), |v| {
            json_has_status(v, &["GREEN"])
        })
        .expect("viewport stays GREEN when writing to ignored build dirs");
        assert!(
            !json_has_status(&st, &["RED", "PENDING_SYNTAX"]),
            "ignored build dirs triggered status change: {st}"
        );
    }

    let out = repo.loom(&["radar", "--json"]);
    out.assert_success();
    let radar_json = out.stdout_json().expect("parse radar --json");
    for ignored_name in ["gen.rs", "index.ts", "mod.py"] {
        assert!(
            !json_contains_ident(&radar_json, ignored_name),
            "ignored file `{ignored_name}` appeared in radar --json: {radar_json}"
        );
    }
    assert!(
        !json_has_status(&radar_json, &["PENDING_SYNTAX", "RED"]),
        "ignored build dir broken syntax caused non-GREEN radar: {radar_json}"
    );
}

#[test]
fn tier1_f15_adaptive_batch_mode_on_multi_file_codegen() {
    let repo = init_loom_repo_with_files(&[("src/lib.rs", "pub fn root_fn() -> i32 { 0 }\n")]);
    let vp1 = spawn_vp(&repo, "vp1", "codegen burst");
    let vp2 = spawn_vp(&repo, "vp2", "disjoint edit");

    let initial = wait_for_radar_status(&vp1, Duration::from_secs(5), |v| {
        json_has_status(v, &["GREEN"])
    })
    .expect("vp1 initial GREEN");
    let status_file = vp1.join(".loom").join("RADAR_STATUS.json");
    let initial_raw = fs::read(&status_file).expect("read initial RADAR_STATUS.json");
    let initial_mtime = fs::metadata(&status_file)
        .expect("stat initial RADAR_STATUS.json")
        .modified()
        .expect("initial RADAR_STATUS.json mtime");

    thread::sleep(Duration::from_millis(30));

    for idx in 0..25 {
        let rel = format!("src/gen_{idx:02}.rs");
        let body = format!("pub fn gen_{idx:02}() -> i32 {{ {idx} }}\n");
        write_vp_file(&vp1, &rel, &body);
    }
    write_vp_file(
        &vp2,
        "src/lib.rs",
        "pub fn root_fn() -> i32 { 0 }\n\npub fn vp2_fn() -> i32 { 9 }\n",
    );

    let converged = wait_for_radar_status(&vp1, Duration::from_secs(8), |v| {
        let raw_changed = fs::read(&status_file)
            .map(|r| r != initial_raw)
            .unwrap_or(false);
        let mtime_changed = fs::metadata(&status_file)
            .and_then(|m| m.modified())
            .map(|now_m| now_m > initial_mtime)
            .unwrap_or(false);
        (v != &initial || raw_changed || mtime_changed) && json_has_status(v, &["GREEN"])
    })
    .expect("radar converges to GREEN after 25-file codegen burst");
    assert!(json_has_status(&converged, &["GREEN"]));

    let out = repo.loom(&["radar", "--json"]);
    out.assert_success();
    let radar_json = out.stdout_json().expect("radar --json after burst");
    assert!(
        json_has_status(&radar_json, &["GREEN"]),
        "expected GREEN after burst convergence, got: {radar_json}"
    );
}

#[test]
fn tier1_f15_watch_detects_new_file_creation_and_delete() {
    let repo = init_loom_repo_with_files(&[("src/lib.rs", "pub fn base() -> i32 { 1 }\n")]);
    let vp1 = spawn_vp(&repo, "vp1", "create and delete 1");
    let vp2 = spawn_vp(&repo, "vp2", "create and delete 2");

    wait_for_radar_status(&vp1, Duration::from_secs(5), |v| {
        json_has_status(v, &["GREEN"])
    })
    .expect("vp1 initial GREEN");

    write_vp_file(&vp1, "src/shared_new.rs", "pub fn collide() -> i32 { 1 }\n");
    write_vp_file(&vp2, "src/shared_new.rs", "pub fn collide() -> i32 { 2 }\n");

    let conflict = wait_for_radar_status(&vp1, Duration::from_secs(6), |v| {
        json_has_status(v, &["RED", "YELLOW"])
    })
    .expect("overlapping new file creation triggers RED/YELLOW");
    assert!(json_has_status(&conflict, &["RED", "YELLOW"]));

    fs::remove_file(vp1.join("src").join("shared_new.rs")).expect("delete vp1/src/shared_new.rs");

    let cleared = wait_for_radar_status(&vp1, Duration::from_secs(6), |v| {
        json_has_status(v, &["GREEN"]) && !json_has_status(v, &["RED", "YELLOW"])
    })
    .expect("deleting conflicting file clears radar back to GREEN");
    assert!(json_has_status(&cleared, &["GREEN"]));
}

// ============================================================================
// F16 — Incremental Parse & PENDING_SYNTAX Guard (5 tests)
// ============================================================================

#[test]
fn tier1_f16_rust_syntax_error_marks_pending_syntax() {
    let repo =
        init_loom_repo_with_files(&[("src/lib.rs", "pub fn compute(x: i32) -> i32 { x + 1 }\n")]);
    let vp1 = spawn_vp(&repo, "vp1", "rust syntax error");
    let vp2 = spawn_vp(&repo, "vp2", "rust valid concurrent edit");

    write_vp_file(&vp2, "src/lib.rs", "pub fn compute(x: i32) -> i32 { x + 2 }\n");
    write_vp_file(
        &vp1,
        "src/lib.rs",
        "pub fn compute(x: i32) -> i32 { let a = \n",
    );

    let status = wait_for_radar_status(&vp1, Duration::from_secs(5), |v| {
        json_has_status(v, &["PENDING_SYNTAX"])
    })
    .expect("Rust broken syntax marks PENDING_SYNTAX");
    assert!(
        !json_has_status(&status, &["RED"]),
        "PENDING_SYNTAX must suppress false RED conflict: {status}"
    );
}

#[test]
fn tier1_f16_ts_syntax_error_marks_pending_syntax() {
    let repo = init_loom_repo_with_files(&[(
        "src/app.ts",
        "export function run(n: number): number { return n + 1; }\n",
    )]);
    let vp1 = spawn_vp(&repo, "vp1", "ts syntax error");

    write_vp_file(
        &vp1,
        "src/app.ts",
        "export function run(n: number): number { return (\n",
    );

    let status = wait_for_radar_status(&vp1, Duration::from_secs(5), |v| {
        json_has_status(v, &["PENDING_SYNTAX"])
    })
    .expect("TypeScript broken syntax marks PENDING_SYNTAX");
    assert!(json_has_status(&status, &["PENDING_SYNTAX"]));
}

#[test]
fn tier1_f16_go_syntax_error_marks_pending_syntax() {
    let repo = init_loom_repo_with_files(&[(
        "pkg/calc.go",
        "package pkg\n\nfunc Eval(x int) int {\n\treturn x + 1\n}\n",
    )]);
    let vp1 = spawn_vp(&repo, "vp1", "go syntax error");

    write_vp_file(
        &vp1,
        "pkg/calc.go",
        "package pkg\n\nfunc Eval(x int) int {\n\treturn x +\n",
    );

    let status = wait_for_radar_status(&vp1, Duration::from_secs(5), |v| {
        json_has_status(v, &["PENDING_SYNTAX"])
    })
    .expect("Go broken syntax marks PENDING_SYNTAX");
    assert!(json_has_status(&status, &["PENDING_SYNTAX"]));
}

#[test]
fn tier1_f16_python_syntax_error_marks_pending_syntax() {
    let repo = init_loom_repo_with_files(&[(
        "app/calc.py",
        "def calc(x: int) -> int:\n    return x + 1\n",
    )]);
    let vp1 = spawn_vp(&repo, "vp1", "python syntax error");

    write_vp_file(
        &vp1,
        "app/calc.py",
        "def calc(x: int) -> int:\n    return (\n",
    );

    let status = wait_for_radar_status(&vp1, Duration::from_secs(5), |v| {
        json_has_status(v, &["PENDING_SYNTAX"])
    })
    .expect("Python broken syntax marks PENDING_SYNTAX");
    assert!(json_has_status(&status, &["PENDING_SYNTAX"]));
}

#[test]
fn tier1_f16_fixing_syntax_clears_pending_syntax() {
    let repo =
        init_loom_repo_with_files(&[("src/lib.rs", "pub fn calc(x: i32) -> i32 { x + 1 }\n")]);
    let vp1 = spawn_vp(&repo, "vp1", "fix pending syntax");

    write_vp_file(
        &vp1,
        "src/lib.rs",
        "pub fn calc(x: i32) -> i32 { let v = ; }\n",
    );
    wait_for_radar_status(&vp1, Duration::from_secs(5), |v| {
        json_has_status(v, &["PENDING_SYNTAX"])
    })
    .expect("broken syntax enters PENDING_SYNTAX");

    write_vp_file(
        &vp1,
        "src/lib.rs",
        "pub fn calc(x: i32) -> i32 { let v = x + 9; v }\n",
    );
    let cleared = wait_for_radar_status(&vp1, Duration::from_secs(5), |v| {
        !json_has_status(v, &["PENDING_SYNTAX"]) && json_has_status(v, &["GREEN"])
    })
    .expect("fixing syntax clears PENDING_SYNTAX to GREEN");
    assert!(json_has_status(&cleared, &["GREEN"]));
}

// ============================================================================
// F17 — N×N Shadow Merge & Dual Delivery (5 tests)
// ============================================================================

#[test]
fn tier1_f17_disjoint_viewports_report_green() {
    let repo = init_loom_repo_with_files(&[(
        "src/lib.rs",
        "pub fn left() -> i32 { 1 }\n\npub fn right() -> i32 { 2 }\n",
    )]);
    let vp1 = spawn_vp(&repo, "vp1", "disjoint left");
    let vp2 = spawn_vp(&repo, "vp2", "disjoint right");

    write_vp_file(
        &vp1,
        "src/lib.rs",
        "pub fn left() -> i32 { 10 }\n\npub fn right() -> i32 { 2 }\n",
    );
    write_vp_file(
        &vp2,
        "src/lib.rs",
        "pub fn left() -> i32 { 1 }\n\npub fn right() -> i32 { 20 }\n",
    );

    for vp in [&vp1, &vp2] {
        let st = wait_for_radar_status(vp, Duration::from_secs(5), |v| {
            json_has_status(v, &["GREEN"]) && !json_has_status(v, &["RED", "YELLOW"])
        })
        .expect("disjoint viewport reports GREEN");
        assert!(json_has_status(&st, &["GREEN"]));
    }

    let out = repo.loom(&["radar", "--json"]);
    out.assert_success();
    let radar_json = out.stdout_json().expect("parse radar --json");
    assert!(json_has_status(&radar_json, &["GREEN"]));
    assert!(!json_has_status(&radar_json, &["RED", "YELLOW"]));
}

#[test]
fn tier1_f17_overlapping_body_edits_report_red() {
    let repo = init_loom_repo_with_files(&[(
        "src/lib.rs",
        "pub fn shared_calc(x: i32) -> i32 { x + 1 }\n",
    )]);
    let vp1 = spawn_vp(&repo, "vp1", "overlap 1");
    let vp2 = spawn_vp(&repo, "vp2", "overlap 2");

    write_vp_file(
        &vp1,
        "src/lib.rs",
        "pub fn shared_calc(x: i32) -> i32 { x + 100 }\n",
    );
    write_vp_file(
        &vp2,
        "src/lib.rs",
        "pub fn shared_calc(x: i32) -> i32 { x * 200 }\n",
    );

    let status = wait_for_radar_status(&vp1, Duration::from_secs(6), |v| {
        json_has_status(v, &["RED"])
    })
    .expect("overlapping body edits trigger RED");
    assert!(
        json_contains_ident(&status, "shared_calc") || json_contains_ident(&status, "src/lib.rs"),
        "expected RED status to reference shared_calc or src/lib.rs: {status}"
    );
}

#[test]
fn tier1_f17_signature_impact_reports_yellow_or_red() {
    let repo = init_loom_repo_with_files(&[(
        "src/lib.rs",
        "pub fn process(id: u32) -> u32 { id + 1 }\n\npub fn caller() -> u32 { process(10) }\n",
    )]);
    let vp1 = spawn_vp(&repo, "vp1", "mutate signature");
    let vp2 = spawn_vp(&repo, "vp2", "stale caller");

    write_vp_file(
        &vp1,
        "src/lib.rs",
        "pub fn process(id: u32, weight: u8) -> u32 { id + weight as u32 }\n\npub fn caller() -> u32 { process(10, 1) }\n",
    );
    write_vp_file(
        &vp2,
        "src/lib.rs",
        "pub fn process(id: u32) -> u32 { id + 1 }\n\npub fn caller() -> u32 { process(20) + 1 }\n",
    );

    let status = wait_for_radar_status(&vp1, Duration::from_secs(6), |v| {
        json_has_status(v, &["YELLOW", "RED"])
    })
    .expect("signature mutation vs stale caller escalates to YELLOW or RED");
    assert!(
        !json_has_status(&status, &["GREEN"]),
        "signature impact must not report GREEN: {status}"
    );
    assert!(
        json_contains_ident(&status, "process") || json_contains_ident(&status, "src/lib.rs"),
        "expected process or src/lib.rs in radar status: {status}"
    );
}

#[test]
fn tier1_f17_radar_status_json_atomic_validity() {
    let repo = init_loom_repo_with_files(&[(
        "src/lib.rs",
        "pub fn f1() -> i32 { 1 }\n\npub fn f2() -> i32 { 2 }\n",
    )]);
    let vp1 = spawn_vp(&repo, "vp1", "atomic reader 1");
    let vp2 = spawn_vp(&repo, "vp2", "atomic reader 2");

    wait_for_radar_status(&vp1, Duration::from_secs(5), |_| true)
        .expect("initial RADAR_STATUS.json");

    let status_path = vp1.join(".loom").join("RADAR_STATUS.json");
    let reader_handle = thread::spawn(move || -> (usize, Vec<String>) {
        let mut valid_reads = 0usize;
        let mut parse_errors = Vec::new();
        for _ in 0..100 {
            if let Ok(text) = fs::read_to_string(&status_path) {
                match serde_json::from_str::<Value>(&text) {
                    Ok(_) => valid_reads += 1,
                    Err(e) => parse_errors.push(format!("{e}: {text:?}")),
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
        (valid_reads, parse_errors)
    });

    for round in 0..10 {
        let v1_code = format!(
            "pub fn f1() -> i32 {{ {} }}\n\npub fn f2() -> i32 {{ 2 }}\n",
            round + 10
        );
        let v2_code = format!(
            "pub fn f1() -> i32 {{ 1 }}\n\npub fn f2() -> i32 {{ {} }}\n",
            round + 20
        );
        write_vp_file(&vp1, "src/lib.rs", &v1_code);
        write_vp_file(&vp2, "src/lib.rs", &v2_code);
        thread::sleep(Duration::from_millis(25));
    }

    let (valid_reads, parse_errors) = reader_handle.join().expect("join atomic reader thread");
    assert!(
        valid_reads > 0,
        "expected at least one successful RADAR_STATUS.json read"
    );
    assert!(
        parse_errors.is_empty(),
        "observed non-atomic/truncated RADAR_STATUS.json reads: {parse_errors:?}"
    );
}

#[test]
fn tier1_f17_mcp_push_notification_on_conflict() {
    let repo = init_loom_repo_with_files(&[("src/lib.rs", "pub fn hot_fn(x: i32) -> i32 { x }\n")]);
    let vp1 = spawn_vp(&repo, "vp1", "mcp push 1");
    let vp2 = spawn_vp(&repo, "vp2", "mcp push 2");

    let mut mcp = repo.spawn_mcp_stdio(None).expect("spawn mcp stdio");
    mcp_handshake(&mut mcp);

    wait_for_radar_status(&vp1, Duration::from_secs(5), |_| true)
        .expect("vp1 radar ready");

    write_vp_file(&vp1, "src/lib.rs", "pub fn hot_fn(x: i32) -> i32 { x + 1 }\n");
    write_vp_file(&vp2, "src/lib.rs", "pub fn hot_fn(x: i32) -> i32 { x + 2 }\n");

    let notif = mcp
        .read_until_notification("notifications/loom/radar_conflict", Duration::from_secs(8))
        .expect("receive notifications/loom/radar_conflict");

    assert_eq!(
        notif.get("method").and_then(Value::as_str),
        Some("notifications/loom/radar_conflict")
    );
    let params = notif.get("params").unwrap_or(&notif);
    assert!(
        json_has_status(params, &["RED", "YELLOW"]),
        "notification params must include RED/YELLOW severity: {notif}"
    );
    assert!(
        json_contains_ident(params, "hot_fn") || json_contains_ident(params, "src/lib.rs"),
        "notification params must reference hot_fn or src/lib.rs: {notif}"
    );
}

// ============================================================================
// F18 — loom mcp serve (rmcp) & agy Integration (5 tests)
// ============================================================================

#[test]
fn tier1_f18_mcp_initialize_and_tools_list() {
    let repo = init_loom_repo_with_files(&[("src/lib.rs", "pub fn init_ok() -> bool { true }\n")]);
    let mut mcp = repo.spawn_mcp_stdio(None).expect("spawn mcp stdio");

    let init = mcp_handshake(&mut mcp);
    assert!(
        init.get("serverInfo").is_some(),
        "MCP initialize result missing serverInfo: {init}"
    );
    assert!(
        init.get("capabilities").is_some(),
        "MCP initialize result missing capabilities: {init}"
    );

    let tools_res = mcp.tools_list().expect("tools/list");
    let tools = tools_res
        .get("tools")
        .and_then(Value::as_array)
        .expect("tools/list must return tools array");

    for expected in [
        "loom_preflight_symbols",
        "loom_radar_poll",
        "loom_record_deadend",
        "loom_seal_capsule",
    ] {
        let tool = tools
            .iter()
            .find(|t| t.get("name").and_then(Value::as_str) == Some(expected))
            .unwrap_or_else(|| panic!("missing MCP tool `{expected}` in {tools_res}"));
        assert!(
            tool.get("inputSchema").is_some(),
            "tool `{expected}` missing inputSchema: {tool}"
        );
    }
}

#[test]
fn tier1_f18_mcp_tool_preflight_symbols() {
    let repo = init_loom_repo_with_files(&[(
        "src/lib.rs",
        "pub fn parse_token(s: &str) -> bool { !s.is_empty() }\n",
    )]);
    let vp1 = spawn_vp(&repo, "vp1", "preflight parse_token");
    write_vp_file(
        &vp1,
        "src/lib.rs",
        "pub fn parse_token(s: &str) -> bool { s.len() > 1 }\n",
    );
    wait_for_radar_status(&vp1, Duration::from_secs(5), |_| true)
        .expect("radar sees vp1");

    let mut mcp = repo.spawn_mcp_stdio(None).expect("spawn mcp stdio");
    mcp_handshake(&mut mcp);
    let tools = mcp.tools_list().expect("tools/list");
    let props = find_tool_props(&tools, "loom_preflight_symbols");

    let args = build_schema_args(
        props,
        &[
            (
                &["symbols", "symbol_names", "fqns", "names", "targets", "symbol"],
                json!(["parse_token"]),
            ),
            (&["viewport", "viewport_name", "vp"], json!("vp1")),
        ],
        &[("symbols", json!(["parse_token"]))],
    );

    let res = mcp
        .tools_call("loom_preflight_symbols", args)
        .expect("call loom_preflight_symbols");
    let text = mcp_tool_text(&res);
    let text_lc = text.to_ascii_lowercase();
    assert!(
        text_lc.contains("parse_token") || text_lc.contains("vp1"),
        "expected preflight response to reference parse_token or vp1, got: {text}"
    );
}

#[test]
fn tier1_f18_mcp_tool_radar_poll() {
    let repo = init_loom_repo_with_files(&[("src/lib.rs", "pub fn collide(x: i32) -> i32 { x }\n")]);
    let vp1 = spawn_vp(&repo, "vp1", "poll 1");
    let vp2 = spawn_vp(&repo, "vp2", "poll 2");

    wait_for_radar_status(&vp1, Duration::from_secs(5), |v| {
        json_has_status(v, &["GREEN"])
    })
    .expect("baseline GREEN");

    let mut mcp = repo.spawn_mcp_stdio(None).expect("spawn mcp stdio");
    mcp_handshake(&mut mcp);
    let tools = mcp.tools_list().expect("tools/list");
    let props = find_tool_props(&tools, "loom_radar_poll");
    let poll_args = build_schema_args(
        props,
        &[(&["viewport", "viewport_name", "vp"], json!("vp1"))],
        &[],
    );

    let baseline_res = mcp
        .tools_call("loom_radar_poll", poll_args.clone())
        .expect("baseline loom_radar_poll");
    let baseline_text = mcp_tool_text(&baseline_res);
    assert!(
        !baseline_text.is_empty(),
        "baseline loom_radar_poll returned empty payload"
    );

    write_vp_file(&vp1, "src/lib.rs", "pub fn collide(x: i32) -> i32 { x + 1 }\n");
    write_vp_file(&vp2, "src/lib.rs", "pub fn collide(x: i32) -> i32 { x + 2 }\n");
    wait_for_radar_status(&vp1, Duration::from_secs(6), |v| {
        json_has_status(v, &["RED"])
    })
    .expect("radar reaches RED");

    let conflict_res = mcp
        .tools_call("loom_radar_poll", poll_args)
        .expect("conflict loom_radar_poll");
    let conflict_text = mcp_tool_text(&conflict_res);
    let conflict_val = Value::String(conflict_text.clone());
    assert!(
        json_has_status(&conflict_val, &["RED"]),
        "expected RED in loom_radar_poll response: {conflict_text}"
    );
    assert!(
        conflict_text.contains("collide") || conflict_text.replace('\\', "/").contains("src/lib.rs"),
        "expected collide or src/lib.rs in loom_radar_poll response: {conflict_text}"
    );
}

#[test]
fn tier1_f18_mcp_tool_record_deadend_and_seal_capsule() {
    let repo = init_loom_repo_with_files(&[(
        "src/lib.rs",
        "pub fn risky() -> i32 { 1 }\n\npub fn safe() -> i32 { 2 }\n",
    )]);
    let vp1 = spawn_vp(&repo, "vp1", "record deadend via mcp");
    let vp2 = spawn_vp(&repo, "vp2", "seal capsule via mcp");

    write_vp_file(
        &vp1,
        "src/lib.rs",
        "pub fn risky() -> i32 { 999 }\n\npub fn safe() -> i32 { 2 }\n",
    );
    write_vp_file(
        &vp2,
        "src/lib.rs",
        "pub fn risky() -> i32 { 1 }\n\npub fn safe() -> i32 { 42 }\n",
    );
    wait_for_radar_status(&vp1, Duration::from_secs(5), |_| true)
        .expect("vp1 radar ready");
    wait_for_radar_status(&vp2, Duration::from_secs(5), |_| true)
        .expect("vp2 radar ready");

    let mut mcp = repo.spawn_mcp_stdio(None).expect("spawn mcp stdio");
    mcp_handshake(&mut mcp);
    let tools = mcp.tools_list().expect("tools/list");

    let deadend_props = find_tool_props(&tools, "loom_record_deadend");
    let deadend_args = build_schema_args(
        deadend_props,
        &[
            (&["viewport", "viewport_name", "vp"], json!("vp1")),
            (
                &["touched_symbols", "symbols", "symbol_names", "symbol"],
                json!(["risky"]),
            ),
            (
                &["lesson_summary", "reason", "lesson", "summary", "message"],
                json!("avoid overflow in risky"),
            ),
            (&["failure_kind", "kind"], json!("Deadlock")),
        ],
        &[
            ("viewport", json!("vp1")),
            ("touched_symbols", json!(["risky"])),
            ("lesson_summary", json!("avoid overflow in risky")),
            ("kind", json!("Deadlock")),
        ],
    );
    let deadend_res = mcp
        .tools_call("loom_record_deadend", deadend_args)
        .expect("call loom_record_deadend");
    let _deadend_text = mcp_tool_text(&deadend_res);

    let preflight_props = find_tool_props(&tools, "loom_preflight_symbols");
    let preflight_args = build_schema_args(
        preflight_props,
        &[(
            &["symbols", "symbol_names", "fqns", "names", "targets", "symbol"],
            json!(["risky"]),
        )],
        &[("symbols", json!(["risky"]))],
    );
    let preflight_res = mcp
        .tools_call("loom_preflight_symbols", preflight_args)
        .expect("call loom_preflight_symbols for risky");
    let preflight_text = mcp_tool_text(&preflight_res);
    assert!(
        preflight_text.contains("avoid overflow in risky"),
        "preflight must warn with recorded deadend lesson: {preflight_text}"
    );

    let seal_props = find_tool_props(&tools, "loom_seal_capsule");
    let seal_args = build_schema_args(
        seal_props,
        &[
            (&["viewport", "viewport_name", "vp"], json!("vp2")),
            (
                &["summary", "message", "intent_summary"],
                json!("update safe via mcp"),
            ),
        ],
        &[
            ("viewport", json!("vp2")),
            ("summary", json!("update safe via mcp")),
        ],
    );
    let seal_res = mcp
        .tools_call("loom_seal_capsule", seal_args)
        .expect("call loom_seal_capsule");
    let _seal_text = mcp_tool_text(&seal_res);

    let trunk = repo.read_file("src/lib.rs").expect("read trunk src/lib.rs");
    assert!(
        trunk.contains("42"),
        "expected trunk src/lib.rs to include vp2's sealed edit 42: {trunk}"
    );
    assert!(
        !trunk.contains("999"),
        "dead-end edit 999 must not merge into trunk: {trunk}"
    );

    let log_out = repo.loom(&["log", "--capsules"]);
    log_out.assert_success();
    assert!(
        log_out.stdout_str().contains("update safe via mcp"),
        "expected sealed capsule summary in loom log --capsules: {}",
        log_out.stdout_str()
    );
}

#[test]
fn tier1_f18_agy_cli_mcp_registration_and_spawn_probe() {
    let agy = agy_bin().expect("agy.exe must be installed for F18 integration test");
    let loom = loom_bin();

    // Declare isolated_home before repo so repo drops (and terminates loomd)
    // before isolated_home is removed. Start loomd under the real user env first.
    let isolated_home = TempDir::new().expect("create isolated home TempDir");
    let repo = init_loom_repo_with_files(&[("src/lib.rs", "pub fn agy_probe() -> i32 { 1 }\n")]);
    repo.loom(&["status"]).assert_success();
    let home_dir = isolated_home.path();
    let appdata_dir = home_dir.join("appdata");
    let local_appdata_dir = home_dir.join("localappdata");
    fs::create_dir_all(&appdata_dir).expect("create appdata");
    fs::create_dir_all(&local_appdata_dir).expect("create localappdata");

    let home_str = home_dir.to_string_lossy().into_owned();
    let appdata_str = appdata_dir.to_string_lossy().into_owned();
    let local_str = local_appdata_dir.to_string_lossy().into_owned();
    let agy_str = agy.to_string_lossy().into_owned();
    let loom_str = loom.to_string_lossy().into_owned();

    let run_agy = |args: &[&str]| {
        let mut cmd = Command::new(&agy);
        cmd.args(args)
            .current_dir(repo.root())
            .env("USERPROFILE", &home_str)
            .env("HOME", &home_str)
            .env("APPDATA", &appdata_str)
            .env("LOCALAPPDATA", &local_str)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = cmd.output().expect("spawn agy.exe");
        e2e_suite::CommandOutput::from_output(
            agy.clone(),
            args.iter().map(|s| (*s).to_string()).collect(),
            repo.root().to_path_buf(),
            output,
        )
    };

    run_agy(&["mcp", "add", "loom-e2e", "--", &loom_str, "mcp", "serve"]).assert_success();
    let list_out = run_agy(&["mcp", "list"]);
    list_out.assert_success();
    assert!(
        list_out.stdout_str().contains("loom-e2e"),
        "agy mcp list should show loom-e2e: {}",
        list_out.stdout_str()
    );

    let settings_path = home_dir.join("agy-settings.json");
    fs::write(&settings_path, b"{}\n").expect("write agy-settings.json");
    let settings_str = settings_path.to_string_lossy().into_owned();

    let env_pairs = [
        ("USERPROFILE", home_str.as_str()),
        ("HOME", home_str.as_str()),
        ("APPDATA", appdata_str.as_str()),
        ("LOCALAPPDATA", local_str.as_str()),
    ];

    let probe_help = repo.loom_with_env(
        &[
            "spawn",
            "-n",
            "vp-agy-cli",
            "-m",
            "agy subcommand probe",
            "--",
            &agy_str,
            "mcp",
            "--help",
        ],
        &env_pairs,
    );
    probe_help.assert_success();
    let help_combined = format!(
        "{}\n{}",
        probe_help.stdout_str(),
        probe_help.stderr_str()
    )
    .to_ascii_lowercase();
    assert!(
        help_combined.contains("add") && help_combined.contains("remove"),
        "expected agy mcp --help usage via loom spawn: stdout={}\nstderr={}",
        probe_help.stdout_str(),
        probe_help.stderr_str()
    );

    let vp_agy_mcp = repo.viewport_dir("vp-agy-mcp");
    let vp_agy_mcp_str = vp_agy_mcp.to_string_lossy().into_owned();
    let session_out = repo.loom_with_env(
        &[
            "spawn",
            "-n",
            "vp-agy-mcp",
            "-m",
            "agy mcp session",
            "--",
            &agy_str,
            "-p",
            "Call loom_radar_poll and reply ok",
            "--output-format",
            "json",
            "--dangerously-skip-permissions",
            "--settings",
            &settings_str,
            "--print-timeout",
            "15s",
            "--add-dir",
            &vp_agy_mcp_str,
        ],
        &env_pairs,
    );

    run_agy(&["mcp", "remove", "loom-e2e"]).assert_success();

    assert!(
        vp_agy_mcp.is_dir(),
        "vp-agy-mcp viewport directory must be materialized"
    );
    let combined = format!("{}\n{}", session_out.stdout_str(), session_out.stderr_str());
    assert!(
        !combined.contains("panicked at"),
        "loom/agy panicked during spawn session: {combined}"
    );
    let combined_lc = combined.to_ascii_lowercase();
    assert!(
        !combined_lc.contains("unknown option")
            && !combined_lc.contains("unexpected argument")
            && !combined_lc.contains("unknown setting")
            && !combined_lc.contains("error: --settings"),
        "agy rejected CLI flags or --settings: {combined}"
    );
    if session_out.success() {
        assert!(
            serde_json::from_str::<Value>(&session_out.stdout_str()).is_ok()
                || session_out.stdout_str().contains('{'),
            "expected JSON output from agy --output-format json: {}",
            session_out.stdout_str()
        );
    }
}

// ============================================================================
// F19 — loom radar CLI/TUI (ratatui) (5 tests)
// ============================================================================

#[test]
fn tier1_f19_radar_json_snapshot() {
    let repo = init_loom_repo_with_files(&[(
        "src/lib.rs",
        "pub fn a() -> i32 { 1 }\n\npub fn b() -> i32 { 2 }\n",
    )]);
    let vp1 = spawn_vp(&repo, "vp1", "json snapshot 1");
    let vp2 = spawn_vp(&repo, "vp2", "json snapshot 2");

    write_vp_file(
        &vp1,
        "src/lib.rs",
        "pub fn a() -> i32 { 10 }\n\npub fn b() -> i32 { 2 }\n",
    );
    write_vp_file(
        &vp2,
        "src/lib.rs",
        "pub fn a() -> i32 { 1 }\n\npub fn b() -> i32 { 20 }\n",
    );
    wait_for_radar_status(&vp1, Duration::from_secs(5), |v| {
        json_has_status(v, &["GREEN"])
    })
    .expect("vp1 GREEN");
    wait_for_radar_status(&vp2, Duration::from_secs(5), |v| {
        json_has_status(v, &["GREEN"])
    })
    .expect("vp2 GREEN");

    let out = repo.loom(&["radar", "--json"]);
    out.assert_success();
    let j = out.stdout_json().expect("parse radar --json");
    assert!(json_contains_ident(&j, "vp1"), "missing vp1: {j}");
    assert!(json_contains_ident(&j, "vp2"), "missing vp2: {j}");
    assert!(json_has_status(&j, &["GREEN"]), "expected GREEN: {j}");
}

#[test]
fn tier1_f19_radar_human_snapshot() {
    let repo = init_loom_repo_with_files(&[(
        "src/lib.rs",
        "pub fn a() -> i32 { 1 }\n\npub fn b() -> i32 { 2 }\n",
    )]);
    let vp1 = spawn_vp(&repo, "vp1", "human snapshot 1");
    let vp2 = spawn_vp(&repo, "vp2", "human snapshot 2");

    wait_for_radar_status(&vp1, Duration::from_secs(5), |v| {
        json_has_status(v, &["GREEN"])
    })
    .expect("vp1 GREEN");
    wait_for_radar_status(&vp2, Duration::from_secs(5), |v| {
        json_has_status(v, &["GREEN"])
    })
    .expect("vp2 GREEN");

    let out = repo.loom(&["radar"]);
    out.assert_success();
    let stdout_lc = out.stdout_str().to_ascii_lowercase();
    assert!(stdout_lc.contains("vp1"), "missing vp1 in radar: {}", out.stdout_str());
    assert!(stdout_lc.contains("vp2"), "missing vp2 in radar: {}", out.stdout_str());
    assert!(
        stdout_lc.contains("green"),
        "missing GREEN status in human radar: {}",
        out.stdout_str()
    );
}

#[test]
fn tier1_f19_radar_watch_json_streams_updates() {
    let repo = init_loom_repo_with_files(&[("src/lib.rs", "pub fn target() -> i32 { 1 }\n")]);
    let vp1 = spawn_vp(&repo, "vp1", "watch json stream 1");
    let vp2 = spawn_vp(&repo, "vp2", "watch json stream 2");

    wait_for_radar_status(&vp1, Duration::from_secs(5), |v| {
        json_has_status(v, &["GREEN"])
    })
    .expect("initial GREEN");

    let mut bg = repo
        .spawn_loom_bg(&["radar", "--watch", "--json"], None, &[])
        .expect("spawn loom radar --watch --json");

    let has_valid_json_with = |text: &str, token: &str| -> bool {
        text.lines().any(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty()
                && serde_json::from_str::<Value>(trimmed)
                    .map(|v| json_has_status(&v, &[token]))
                    .unwrap_or(false)
        })
    };

    let start = Instant::now();
    while !has_valid_json_with(&bg.stdout_str(), "GREEN") {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timed out waiting for initial GREEN JSON stream; stdout={}; stderr={}",
            bg.stdout_str(),
            bg.stderr_str()
        );
        thread::sleep(Duration::from_millis(30));
    }

    write_vp_file(&vp1, "src/lib.rs", "pub fn target() -> i32 { 10 }\n");
    write_vp_file(&vp2, "src/lib.rs", "pub fn target() -> i32 { 20 }\n");

    let conflict_start = Instant::now();
    while !has_valid_json_with(&bg.stdout_str(), "RED") {
        assert!(
            conflict_start.elapsed() < Duration::from_secs(8),
            "timed out waiting for RED in JSON stream; stdout={}; stderr={}",
            bg.stdout_str(),
            bg.stderr_str()
        );
        thread::sleep(Duration::from_millis(30));
    }

    let stderr = bg.stderr_str();
    bg.kill();
    assert!(
        !stderr.contains("panicked at"),
        "radar --watch --json panicked: {stderr}"
    );
}

#[test]
fn tier1_f19_radar_reflects_conflict_details() {
    let repo = init_loom_repo_with_files(&[(
        "src/lib.rs",
        "pub fn shared_op(x: i32) -> i32 { x }\n",
    )]);
    let vp1 = spawn_vp(&repo, "vp1", "conflict details 1");
    let vp2 = spawn_vp(&repo, "vp2", "conflict details 2");

    write_vp_file(
        &vp1,
        "src/lib.rs",
        "pub fn shared_op(x: i32) -> i32 { x + 5 }\n",
    );
    write_vp_file(
        &vp2,
        "src/lib.rs",
        "pub fn shared_op(x: i32) -> i32 { x * 5 }\n",
    );

    wait_for_radar_status(&vp1, Duration::from_secs(6), |v| {
        json_has_status(v, &["RED"])
    })
    .expect("radar reaches RED");

    let out = repo.loom(&["radar", "--json"]);
    out.assert_success();
    let j = out.stdout_json().expect("parse radar --json");
    assert!(json_has_status(&j, &["RED"]), "expected RED: {j}");
    assert!(json_contains_ident(&j, "vp1"), "missing vp1: {j}");
    assert!(json_contains_ident(&j, "vp2"), "missing vp2: {j}");
    assert!(
        json_contains_ident(&j, "shared_op") || json_contains_ident(&j, "src/lib.rs"),
        "expected shared_op or src/lib.rs in conflict details: {j}"
    );
}

#[test]
fn tier1_f19_radar_watch_tui_clean_startup_and_teardown() {
    let repo = init_loom_repo_with_files(&[("src/lib.rs", "pub fn tui_fn() -> i32 { 1 }\n")]);
    let vp1 = spawn_vp(&repo, "vp1", "tui watch");
    wait_for_radar_status(&vp1, Duration::from_secs(5), |_| true)
        .expect("vp1 radar ready");

    let start = Instant::now();
    let mut cmd = Command::new(loom_bin());
    cmd.args(["radar", "--watch"])
        .current_dir(repo.root())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn loom radar --watch");

    let mut stdin_opt = child.stdin.take();
    let stdout_pipe = child.stdout.take().expect("piped stdout");
    let stderr_pipe = child.stderr.take().expect("piped stderr");

    let stdout_buf = Arc::new(Mutex::new(Vec::new()));
    let stderr_buf = Arc::new(Mutex::new(Vec::new()));
    let drains = vec![
        TuiChildGuard::spawn_drain(stdout_pipe, Arc::clone(&stdout_buf)),
        TuiChildGuard::spawn_drain(stderr_pipe, Arc::clone(&stderr_buf)),
    ];

    let mut guard = TuiChildGuard {
        child: Some(child),
        drains,
    };

    thread::sleep(Duration::from_millis(250));
    let early_exit = guard
        .child
        .as_mut()
        .expect("child present")
        .try_wait()
        .expect("try_wait tui");

    if early_exit.is_none() {
        if let Some(mut stdin) = stdin_opt.take() {
            let _ = stdin.write_all(b"q");
            let _ = stdin.flush();
            drop(stdin);
        }
        let quit_deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < quit_deadline {
            if let Ok(Some(_)) = guard.child.as_mut().expect("child present").try_wait() {
                break;
            }
            thread::sleep(Duration::from_millis(30));
        }
    } else {
        drop(stdin_opt);
    }

    guard.terminate_and_wait();
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "TUI startup and teardown exceeded 5s ({:?})",
        start.elapsed()
    );

    let stdout_str = String::from_utf8_lossy(&stdout_buf.lock().unwrap_or_else(|e| e.into_inner()))
        .into_owned();
    let stderr_str = String::from_utf8_lossy(&stderr_buf.lock().unwrap_or_else(|e| e.into_inner()))
        .into_owned();
    assert!(
        !stdout_str.contains("panicked at") && !stderr_str.contains("panicked at"),
        "loom radar --watch panicked: stdout={stdout_str}; stderr={stderr_str}"
    );
    let combined_lc = format!("{stdout_str}\n{stderr_str}").to_ascii_lowercase();
    assert!(
        !combined_lc.contains("unexpected argument")
            && !combined_lc.contains("unrecognized")
            && !combined_lc.contains("unknown option"),
        "loom radar --watch failed CLI parsing: stdout={stdout_str}; stderr={stderr_str}"
    );
    if let Some(status) = early_exit {
        let has_graceful_non_tty_notice = combined_lc.contains("tty")
            || combined_lc.contains("terminal")
            || combined_lc.contains("interactive")
            || combined_lc.contains("console");
        assert!(
            status.success() || has_graceful_non_tty_notice,
            "loom radar --watch exited early ({status}) without success or graceful non-TTY notice: stdout={stdout_str}; stderr={stderr_str}"
        );
    }

    repo.loom(&["status"]).assert_success();
}
