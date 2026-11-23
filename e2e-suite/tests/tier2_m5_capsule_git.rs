use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use serde_json::{json, Map, Value};

use e2e_suite::{CommandOutput, HarnessError, McpClient, TestRepo};

#[cfg(windows)]
const PASS_CMD: &str = "cmd /C exit 0";
#[cfg(windows)]
const FAIL_CMD: &str = "cmd /C exit 1";

#[cfg(not(windows))]
const PASS_CMD: &str = "true";
#[cfg(not(windows))]
const FAIL_CMD: &str = "false";

fn combined_output(out: &CommandOutput) -> String {
    format!("{}\n{}", out.stdout_str(), out.stderr_str())
}

#[track_caller]
fn assert_no_panic(out: &CommandOutput) {
    let stdout = out.stdout_str();
    let stderr = out.stderr_str();
    assert!(
        !stdout.contains("panicked at")
            && !stderr.contains("panicked at")
            && !stdout.contains("RUST_BACKTRACE")
            && !stderr.contains("RUST_BACKTRACE"),
        "command panicked unexpectedly (exit={:?})\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.exit_code(),
        stdout,
        stderr
    );
}

#[track_caller]
fn assert_clean_failure(out: &CommandOutput, context: &str) {
    out.assert_failure();
    assert_no_panic(out);
    assert!(
        !combined_output(out).trim().is_empty(),
        "{context}: expected non-empty diagnostic output on failure"
    );
}

#[track_caller]
fn assert_no_conflict_markers(text: &str, context: &str) {
    assert!(
        !text.contains("<<<<<<<") && !text.contains("=======") && !text.contains(">>>>>>>"),
        "{context}: unexpected conflict markers in content:\n{text}"
    );
}

fn init_loom_repo_with_file(rel: &str, content: &str, colocate_git: bool) -> TestRepo {
    let repo = TestRepo::new().expect("create TestRepo");
    repo.write_file(rel, content).expect("write initial file");
    if colocate_git {
        repo.init_git_repo().expect("init_git_repo");
        repo.loom(&["init", "--colocate-git"]).assert_success();
    } else {
        repo.loom(&["init"]).assert_success();
    }
    repo
}

fn spawn_and_edit_vp(
    repo: &TestRepo,
    vp_name: &str,
    intent: &str,
    rel: &str,
    new_content: &str,
) -> PathBuf {
    repo.loom(&["spawn", "-n", vp_name, "-m", intent])
        .assert_success();
    let vp_dir = repo.viewport_dir(vp_name);
    let target = vp_dir.join(rel);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).expect("create parent dirs in viewport");
    }
    fs::write(&target, new_content).expect("write viewport file");
    // Allow notify watcher (~15ms debounce) to capture unstaged AstPatch in WAL.
    thread::sleep(Duration::from_millis(150));
    vp_dir
}

fn setup_colocated_with_bare(rel: &str, content: &str) -> (TestRepo, PathBuf) {
    let repo = init_loom_repo_with_file(rel, content, true);
    let bare = repo.init_bare_remote().expect("init_bare_remote");
    let bare_str = bare.to_string_lossy().into_owned();
    repo.git(&["remote", "add", "origin", &bare_str])
        .assert_success();
    repo.git(&["push", "-u", "origin", "main"])
        .assert_success();
    (repo, bare)
}

fn init_synced_clone_repo(bare: &Path, user_name: &str, user_email: &str) -> TestRepo {
    let repo = TestRepo::new().expect("create second TestRepo");
    let bare_str = bare.to_string_lossy().into_owned();
    repo.git(&["init", "-b", "main"]).assert_success();
    repo.git(&["config", "user.name", user_name])
        .assert_success();
    repo.git(&["config", "user.email", user_email])
        .assert_success();
    repo.git(&["config", "core.autocrlf", "false"])
        .assert_success();
    repo.git(&["remote", "add", "origin", &bare_str])
        .assert_success();
    repo.git(&["pull", "origin", "main"]).assert_success();
    repo.loom(&["init", "--colocate-git"]).assert_success();
    repo
}

fn spawn_mcp_handshaken(repo: &TestRepo) -> McpClient {
    let mut mcp = repo
        .spawn_mcp_stdio(None)
        .expect("spawn loom mcp serve")
        .with_timeout(Duration::from_secs(15));
    mcp.handshake().expect("MCP handshake");
    mcp
}

fn find_tool<'a>(tools_list_val: &'a Value, tool_name: &str) -> &'a Value {
    tools_list_val
        .get("tools")
        .and_then(Value::as_array)
        .and_then(|arr| {
            arr.iter()
                .find(|t| t.get("name").and_then(Value::as_str) == Some(tool_name))
        })
        .unwrap_or_else(|| panic!("tool {tool_name:?} not found in tools/list: {tools_list_val}"))
}

fn tool_prop(
    tools_list_val: &Value,
    tool_name: &str,
    candidates: &[&str],
    expected_type: Option<&str>,
) -> Option<String> {
    let tool = find_tool(tools_list_val, tool_name);
    let props = tool
        .get("inputSchema")
        .and_then(|s| s.get("properties"))
        .and_then(Value::as_object)?;

    for candidate in candidates {
        if props.contains_key(*candidate) {
            return Some((*candidate).to_string());
        }
    }

    if let Some(ty) = expected_type {
        for (k, v) in props {
            let matches_type = match v.get("type") {
                Some(Value::String(s)) => s == ty,
                Some(Value::Array(arr)) => arr.iter().any(|item| item.as_str() == Some(ty)),
                _ => false,
            };
            if matches_type {
                return Some(k.clone());
            }
        }
    }

    None
}

fn tool_requires_prop(tools_list_val: &Value, tool_name: &str, prop_name: &str) -> bool {
    let tool = find_tool(tools_list_val, tool_name);
    tool.get("inputSchema")
        .and_then(|s| s.get("required"))
        .and_then(Value::as_array)
        .map(|req| req.iter().any(|v| v.as_str() == Some(prop_name)))
        .unwrap_or(false)
}

fn build_preflight_args(tools: &Value, symbols: &[&str], viewport: Option<&str>) -> Value {
    let mut map = Map::new();
    let sym_key = tool_prop(
        tools,
        "loom_preflight_symbols",
        &["symbols", "symbol_fqns", "fqns", "targets", "names"],
        Some("array"),
    )
    .unwrap_or_else(|| "symbols".to_string());
    map.insert(sym_key, json!(symbols));

    if let Some(vp) = viewport {
        if let Some(vp_key) = tool_prop(
            tools,
            "loom_preflight_symbols",
            &["viewport", "viewport_name", "vp", "name"],
            None,
        ) {
            map.insert(vp_key, json!(vp));
        }
    } else if let Some(vp_key) = tool_prop(
        tools,
        "loom_preflight_symbols",
        &["viewport", "viewport_name", "vp"],
        None,
    ) {
        if tool_requires_prop(tools, "loom_preflight_symbols", &vp_key) {
            map.insert(vp_key, json!("default"));
        }
    }

    Value::Object(map)
}

fn build_record_deadend_args(
    tools: &Value,
    viewport: &str,
    reason: &str,
    kind: Option<&str>,
    symbols: &[&str],
) -> Value {
    let mut map = Map::new();
    let vp_key = tool_prop(
        tools,
        "loom_record_deadend",
        &["viewport", "viewport_name", "viewport_id", "vp", "name"],
        None,
    )
    .unwrap_or_else(|| "viewport".to_string());
    map.insert(vp_key, json!(viewport));
    let reason_key = tool_prop(
        tools,
        "loom_record_deadend",
        &["reason", "lesson", "lesson_summary", "summary", "message"],
        None,
    )
    .unwrap_or_else(|| "reason".to_string());
    map.insert(reason_key, json!(reason));

    if let Some(k) = kind {
        let kind_key = tool_prop(
            tools,
            "loom_record_deadend",
            &["kind", "failure_kind", "failureKind", "category"],
            None,
        )
        .unwrap_or_else(|| "kind".to_string());
        map.insert(kind_key, json!(k));
    }

    if let Some(sym_key) = tool_prop(
        tools,
        "loom_record_deadend",
        &["touched_symbols", "symbols", "symbol_fqns"],
        Some("array"),
    ) {
        if !symbols.is_empty() || tool_requires_prop(tools, "loom_record_deadend", &sym_key) {
            map.insert(sym_key, json!(symbols));
        }
    }

    Value::Object(map)
}

fn build_seal_capsule_args(tools: &Value, viewport: &str, summary: &str) -> Value {
    let mut map = Map::new();
    let vp_key = tool_prop(
        tools,
        "loom_seal_capsule",
        &["viewport", "viewport_name", "vp", "name"],
        None,
    )
    .unwrap_or_else(|| "viewport".to_string());
    map.insert(vp_key, json!(viewport));

    if let Some(sum_key) = tool_prop(
        tools,
        "loom_seal_capsule",
        &["summary", "message", "msg", "description"],
        None,
    ) {
        map.insert(sum_key, json!(summary));
    }

    Value::Object(map)
}

fn extract_mcp_text(val: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(content) = val.get("content").and_then(Value::as_array) {
        for item in content {
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                parts.push(text.to_string());
            }
        }
    }
    if let Some(structured) = val.get("structuredContent") {
        parts.push(structured.to_string());
    }
    if parts.is_empty() {
        parts.push(val.to_string());
    }
    parts.join("\n")
}

#[track_caller]
fn assert_mcp_ok(val: &Value, context: &str) {
    assert_ne!(
        val.get("isError").and_then(Value::as_bool),
        Some(true),
        "{context}: unexpected MCP tool error (isError=true): {val}"
    );
}

#[track_caller]
fn assert_mcp_error(res: e2e_suite::Result<Value>, context: &str) {
    match res {
        Err(HarnessError::JsonRpc { .. }) => {}
        Ok(val) if val.get("isError").and_then(Value::as_bool) == Some(true) => {}
        Ok(val) => {
            panic!("{context}: expected JSON-RPC error or isError=true, got success: {val}");
        }
        Err(other) => {
            panic!("{context}: expected clean JSON-RPC/tool error, got transport/protocol error: {other}");
        }
    }
}

// ============================================================================
// F20: Unstaged AstPatch Capture & loom seal (5 Tier 2 tests)
// ============================================================================

#[test]
fn tier2_f20_seal_failing_verify_cmd_aborts_merge() {
    let orig = "pub fn stable() -> i32 {\n    1\n}\n";
    let repo = init_loom_repo_with_file("src/lib.rs", orig, false);
    let updated = "pub fn stable() -> i32 {\n    1\n}\n\npub fn unverified_change() -> i32 {\n    99\n}\n";
    let vp = spawn_and_edit_vp(&repo, "vp-fail", "failing verify", "src/lib.rs", updated);

    let out = repo.loom_in(&vp, &["seal", "-v", FAIL_CMD, "-m", "aborted capsule"]);
    assert_clean_failure(&out, "seal with failing verify command");

    // Trunk must remain untouched
    let trunk = repo.read_file("src/lib.rs").expect("read trunk");
    assert_eq!(trunk, orig);
    assert!(!trunk.contains("unverified_change"));

    // Viewport edits must remain intact so agent can fix and retry
    let vp_content = fs::read_to_string(vp.join("src/lib.rs")).expect("read viewport src/lib.rs");
    assert!(vp_content.contains("unverified_change"));

    // Aborted capsule must not appear in capsule log
    let log_out = repo.loom(&["log", "--capsules"]);
    log_out.assert_success();
    assert!(!log_out.stdout_str().contains("aborted capsule"));

    // Recoverable when verify command passes
    repo.loom_in(&vp, &["seal", "-v", PASS_CMD, "-m", "recovered capsule"])
        .assert_success();
    let merged_trunk = repo.read_file("src/lib.rs").expect("read merged trunk");
    assert!(merged_trunk.contains("unverified_change"));
}

#[test]
fn tier2_f20_seal_with_no_changes_clean_handling() {
    let base = "pub fn base() -> i32 {\n    1\n}\n";
    let repo = init_loom_repo_with_file("src/lib.rs", base, false);
    repo.loom(&["spawn", "-n", "vp-noop", "-m", "no-op viewport"])
        .assert_success();
    let vp = repo.viewport_dir("vp-noop");
    thread::sleep(Duration::from_millis(100));

    let out = repo.loom_in(&vp, &["seal", "-v", PASS_CMD, "-m", "empty seal"]);
    assert_no_panic(&out);

    let trunk = repo.read_file("src/lib.rs").expect("read trunk");
    assert_eq!(trunk, base);

    if !out.success() {
        assert!(!combined_output(&out).trim().is_empty());
        let log_out = repo.loom(&["log", "--capsules"]);
        log_out.assert_success();
        assert!(!log_out.stdout_str().contains("empty seal"));
    }

    repo.loom(&["status"]).assert_success();

    // Verify daemon & WAL remain healthy for subsequent real seal
    let vp_real = spawn_and_edit_vp(
        &repo,
        "vp-real",
        "real edit",
        "src/lib.rs",
        "pub fn base() -> i32 {\n    2\n}\n",
    );
    repo.loom_in(&vp_real, &["seal", "-v", PASS_CMD, "-m", "real seal"])
        .assert_success();
    assert!(repo
        .read_file("src/lib.rs")
        .expect("read trunk")
        .contains("2"));
}

#[test]
fn tier2_f20_seal_nonexistent_viewport_errors() {
    let orig = "pub fn ok() -> i32 {\n    1\n}\n";
    let repo = init_loom_repo_with_file("src/lib.rs", orig, false);

    // 1. Trunk CWD with zero active viewports
    let root_out = repo.loom(&["seal", "-m", "ghost-seal"]);
    assert_clean_failure(&root_out, "seal from trunk with zero viewports");

    // 2. Check if `loom seal --help` exposes an explicit viewport flag
    let help = repo.loom(&["seal", "--help"]);
    help.assert_success();
    let help_text = combined_output(&help);
    if help_text.contains("--viewport") {
        let flagged = repo.loom(&["seal", "--viewport", "nonexistent-vp-e2e", "-m", "ghost-seal"]);
        assert_clean_failure(&flagged, "seal --viewport nonexistent");
    }

    // 3. Unregistered directory under .loom/viewports/ while a real unsealed canary viewport exists
    let _canary = spawn_and_edit_vp(
        &repo,
        "vp-canary",
        "canary",
        "src/lib.rs",
        "pub fn ok() -> i32 {\n    777\n}\n",
    );
    let ghost_dir = repo.viewport_dir("nonexistent-vp-e2e");
    fs::create_dir_all(&ghost_dir).expect("create ghost viewport dir on disk");
    let cwd_out = repo.loom_in(&ghost_dir, &["seal", "-m", "ghost-seal"]);
    assert_clean_failure(&cwd_out, "seal inside unregistered viewport directory");

    // 4. MCP loom_seal_capsule targeting nonexistent viewport
    let mut mcp = spawn_mcp_handshaken(&repo);
    let tools = mcp.tools_list().expect("tools/list");
    let args = build_seal_capsule_args(&tools, "nonexistent-vp-e2e", "ghost-seal");
    let res = mcp.tools_call("loom_seal_capsule", args);
    assert_mcp_error(res, "loom_seal_capsule on nonexistent viewport");
    mcp.tools_list().expect("MCP server remains alive after error");
    drop(mcp);

    // Trunk must remain untouched and ghost capsule absent
    assert_eq!(repo.read_file("src/lib.rs").expect("read trunk"), orig);
    let log_out = repo.loom(&["log", "--capsules"]);
    log_out.assert_success();
    assert!(!log_out.stdout_str().contains("ghost-seal"));
}

#[test]
fn tier2_f20_seal_on_conflict_with_updated_trunk_blocked() {
    let orig = "pub fn compute(x: i32) -> i32 {\n    let base = x + 1;\n    base * 2\n}\n";
    let repo = init_loom_repo_with_file("src/lib.rs", orig, false);

    repo.loom(&["spawn", "-n", "vp1", "-m", "vp1 edit"])
        .assert_success();
    repo.loom(&["spawn", "-n", "vp2", "-m", "vp2 conflicting edit"])
        .assert_success();

    let vp1 = repo.viewport_dir("vp1");
    let vp2 = repo.viewport_dir("vp2");

    fs::write(
        vp1.join("src/lib.rs"),
        "pub fn compute(x: i32) -> i32 {\n    let base = x + 10;\n    base * 3\n}\n",
    )
    .expect("write vp1");
    fs::write(
        vp2.join("src/lib.rs"),
        "pub fn compute(x: i32) -> i32 {\n    let base = x + 99;\n    base * 7\n}\n",
    )
    .expect("write vp2");
    thread::sleep(Duration::from_millis(150));

    repo.loom_in(&vp1, &["seal", "-v", PASS_CMD, "-m", "vp1 sealed"])
        .assert_success();

    let vp2_out = repo.loom_in(&vp2, &["seal", "-v", PASS_CMD, "-m", "vp2 blocked"]);
    assert_clean_failure(&vp2_out, "conflicting seal against updated trunk");

    let trunk = repo.read_file("src/lib.rs").expect("read trunk");
    assert!(trunk.contains("x + 10") && trunk.contains("base * 3"));
    assert!(!trunk.contains("x + 99") && !trunk.contains("base * 7"));
    assert_no_conflict_markers(&trunk, "trunk after blocked conflicting seal");

    let caps = repo.loom(&["log", "--capsules"]);
    caps.assert_success();
    assert!(caps.stdout_str().contains("vp1 sealed"));
    assert!(!caps.stdout_str().contains("vp2 blocked"));

    repo.loom_in(&vp2, &["drop", "--reason", "superseded by vp1"])
        .assert_success();
}

#[test]
fn tier2_f20_seal_empty_summary_or_long_summary() {
    // Subcase A: empty summary `-m ""`
    let repo_empty = init_loom_repo_with_file("src/lib.rs", "pub fn alpha() -> i32 { 1 }\n", false);
    let vp_empty = spawn_and_edit_vp(
        &repo_empty,
        "vp-empty-msg",
        "empty summary",
        "src/lib.rs",
        "pub fn alpha() -> i32 { 2 }\n",
    );
    let out_empty = repo_empty.loom_in(&vp_empty, &["seal", "-v", PASS_CMD, "-m", ""]);
    assert_no_panic(&out_empty);
    if out_empty.success() {
        assert!(repo_empty
            .read_file("src/lib.rs")
            .expect("read trunk")
            .contains("2"));
        repo_empty.loom(&["log", "--capsules"]).assert_success();
        repo_empty.loom(&["status"]).assert_success();
    } else {
        assert!(!combined_output(&out_empty).trim().is_empty());
        assert!(repo_empty
            .read_file("src/lib.rs")
            .expect("read trunk")
            .contains("1"));
        repo_empty
            .loom_in(&vp_empty, &["seal", "-v", PASS_CMD, "-m", "fallback summary"])
            .assert_success();
    }

    // Subcase B: very long multi-byte Unicode summary (2048 chars = 6144 bytes)
    let repo_long = init_loom_repo_with_file("src/lib.rs", "pub fn beta() -> i32 { 10 }\n", false);
    let vp_long = spawn_and_edit_vp(
        &repo_long,
        "vp-long-msg",
        "long summary",
        "src/lib.rs",
        "pub fn beta() -> i32 { 20 }\n",
    );
    let long_summary = "가".repeat(2048);
    let out_long = repo_long.loom_in(&vp_long, &["seal", "-v", PASS_CMD, "-m", &long_summary]);
    assert_no_panic(&out_long);
    if out_long.success() {
        assert!(repo_long
            .read_file("src/lib.rs")
            .expect("read trunk")
            .contains("20"));
        repo_long.loom(&["log", "--capsules"]).assert_success();
        repo_long.loom(&["status"]).assert_success();
    } else {
        assert!(!combined_output(&out_long).trim().is_empty());
        assert!(repo_long
            .read_file("src/lib.rs")
            .expect("read trunk")
            .contains("10"));
        repo_long
            .loom_in(&vp_long, &["seal", "-v", PASS_CMD, "-m", "short summary"])
            .assert_success();
    }
}

// ============================================================================
// F21: NegativeEdge Failure Ledger (5 Tier 2 tests)
// ============================================================================

#[test]
fn tier2_f21_lesson_summary_499_500_501_char_boundary() {
    let base = "pub fn sym_499() -> i32 { 1 }\n\
                pub fn sym_500() -> i32 { 1 }\n\
                pub fn sym_501() -> i32 { 1 }\n\
                pub fn sym_u500() -> i32 { 1 }\n\
                pub fn sym_u501() -> i32 { 1 }\n";
    let repo = init_loom_repo_with_file("src/lib.rs", base, false);

    let s499 = format!("B499_{}_E499", "x".repeat(489));
    let s500 = format!("B500_{}_E500", "y".repeat(490));
    let s501 = format!("B501_{}_E501", "z".repeat(491));
    let u500 = format!("U50_{}_END", "한".repeat(492));
    let u501 = format!("U51_{}_END", "한".repeat(493));
    assert_eq!(s499.chars().count(), 499);
    assert_eq!(s500.chars().count(), 500);
    assert_eq!(s501.chars().count(), 501);
    assert_eq!(u500.chars().count(), 500);
    assert_eq!(u501.chars().count(), 501);

    // 1. 499 ASCII chars -> must succeed verbatim & restore clean
    let vp499 = spawn_and_edit_vp(
        &repo,
        "vp499",
        "test 499",
        "src/lib.rs",
        &base.replace("sym_499() -> i32 { 1 }", "sym_499() -> i32 { 2 }"),
    );
    repo.loom_in(&vp499, &["drop", "--reason", &s499])
        .assert_success();
    if vp499.join("src/lib.rs").exists() {
        assert_eq!(
            fs::read_to_string(vp499.join("src/lib.rs")).expect("read vp499"),
            base
        );
    }

    // 2. 500 ASCII chars -> must succeed verbatim
    let vp500 = spawn_and_edit_vp(
        &repo,
        "vp500",
        "test 500",
        "src/lib.rs",
        &base.replace("sym_500() -> i32 { 1 }", "sym_500() -> i32 { 2 }"),
    );
    repo.loom_in(&vp500, &["drop", "--reason", &s500])
        .assert_success();

    // 3. 501 ASCII chars -> must reject cleanly OR truncate to <= 500 chars
    let vp501 = spawn_and_edit_vp(
        &repo,
        "vp501",
        "test 501",
        "src/lib.rs",
        &base.replace("sym_501() -> i32 { 1 }", "sym_501() -> i32 { 2 }"),
    );
    let out501 = repo.loom_in(&vp501, &["drop", "--reason", &s501]);
    assert_no_panic(&out501);
    if !out501.success() {
        assert!(!combined_output(&out501).trim().is_empty());
    }

    // 4. Multi-byte UTF-8 500 and 501 chars -> 500 must succeed; 501 must never panic or corrupt UTF-8 boundaries
    let vpu500 = spawn_and_edit_vp(
        &repo,
        "vpu500",
        "test u500",
        "src/lib.rs",
        &base.replace("sym_u500() -> i32 { 1 }", "sym_u500() -> i32 { 2 }"),
    );
    repo.loom_in(&vpu500, &["drop", "--reason", &u500])
        .assert_success();

    let vpu501 = spawn_and_edit_vp(
        &repo,
        "vpu501",
        "test u501",
        "src/lib.rs",
        &base.replace("sym_u501() -> i32 { 1 }", "sym_u501() -> i32 { 2 }"),
    );
    let out_u501 = repo.loom_in(&vpu501, &["drop", "--reason", &u501]);
    assert_no_panic(&out_u501);
    if !out_u501.success() {
        assert!(!combined_output(&out_u501).trim().is_empty());
    }

    // 5. Verify via MCP loom_preflight_symbols & loom_record_deadend
    let mut mcp = spawn_mcp_handshaken(&repo);
    let tools = mcp.tools_list().expect("tools/list");

    let res499 = mcp
        .tools_call(
            "loom_preflight_symbols",
            build_preflight_args(&tools, &["sym_499"], None),
        )
        .expect("preflight sym_499");
    assert_mcp_ok(&res499, "preflight sym_499");
    let text499 = extract_mcp_text(&res499);
    assert!(
        text499.contains(&s499),
        "expected full 499-char lesson in preflight, got: {text499}"
    );

    let res500 = mcp
        .tools_call(
            "loom_preflight_symbols",
            build_preflight_args(&tools, &["sym_500"], None),
        )
        .expect("preflight sym_500");
    assert_mcp_ok(&res500, "preflight sym_500");
    let text500 = extract_mcp_text(&res500);
    assert!(
        text500.contains(&s500),
        "expected full 500-char lesson in preflight, got: {text500}"
    );

    let res501 = mcp
        .tools_call(
            "loom_preflight_symbols",
            build_preflight_args(&tools, &["sym_501"], None),
        )
        .expect("preflight sym_501");
    assert_mcp_ok(&res501, "preflight sym_501");
    let text501 = extract_mcp_text(&res501);
    assert!(
        !text501.contains(&s501) && !text501.contains("_E501"),
        "501-char lesson must not be stored verbatim at 501 chars: {text501}"
    );
    if out501.success() {
        assert!(
            text501.contains("B501_"),
            "truncated 501-char lesson must preserve prefix B501_: {text501}"
        );
    }

    let res_u500 = mcp
        .tools_call(
            "loom_preflight_symbols",
            build_preflight_args(&tools, &["sym_u500"], None),
        )
        .expect("preflight sym_u500");
    assert_mcp_ok(&res_u500, "preflight sym_u500");
    let text_u500 = extract_mcp_text(&res_u500);
    assert!(
        text_u500.contains(&u500),
        "expected full 500-char Unicode lesson in preflight, got: {text_u500}"
    );

    let res_u501 = mcp
        .tools_call(
            "loom_preflight_symbols",
            build_preflight_args(&tools, &["sym_u501"], None),
        )
        .expect("preflight sym_u501");
    assert_mcp_ok(&res_u501, "preflight sym_u501");
    let text_u501 = extract_mcp_text(&res_u501);
    assert!(
        !text_u501.contains(&u501) && !text_u501.contains("_END"),
        "501-char Unicode lesson must not be stored verbatim at 501 chars: {text_u501}"
    );
    if out_u501.success() {
        assert!(
            text_u501.contains("U51_"),
            "truncated 501-char Unicode lesson must preserve prefix U51_: {text_u501}"
        );
    }

    // Also test MCP loom_record_deadend 501-char boundary (if vp501 was rejected and remains active, or on new vp)
    let vp_mcp501 = spawn_and_edit_vp(
        &repo,
        "vp-mcp501",
        "mcp 501",
        "src/lib.rs",
        &base.replace("sym_501() -> i32 { 1 }", "sym_501() -> i32 { 3 }"),
    );
    let _ = vp_mcp501;
    let mcp_deadend_res = mcp.tools_call(
        "loom_record_deadend",
        build_record_deadend_args(&tools, "vp-mcp501", &s501, None, &["sym_501"]),
    );
    match &mcp_deadend_res {
        Ok(val) => {
            if val.get("isError").and_then(Value::as_bool) != Some(true) {
                let after = mcp
                    .tools_call(
                        "loom_preflight_symbols",
                        build_preflight_args(&tools, &["sym_501"], None),
                    )
                    .expect("preflight after mcp_record_deadend");
                assert_mcp_ok(&after, "preflight sym_501 after mcp_record_deadend");
                let after_text = extract_mcp_text(&after);
                assert!(
                    !after_text.contains(&s501) && !after_text.contains("_E501"),
                    "MCP loom_record_deadend must enforce <=500 chars: {after_text}"
                );
                assert!(
                    after_text.contains("B501_"),
                    "truncated MCP 501-char lesson must preserve prefix B501_: {after_text}"
                );
            }
        }
        Err(HarnessError::JsonRpc { .. }) => {}
        Err(other) => {
            panic!(
                "loom_record_deadend 501-char check: expected Ok or JsonRpc error, got transport/protocol error: {other}"
            );
        }
    }
    mcp.tools_list().expect("MCP server healthy after boundary checks");
}

#[test]
fn tier2_f21_drop_empty_reason_rejected() {
    let orig = "pub fn target_fn() -> i32 { 1 }\n";
    let repo = init_loom_repo_with_file("src/lib.rs", orig, false);
    let vp = spawn_and_edit_vp(
        &repo,
        "vp-empty-reason",
        "edit target_fn",
        "src/lib.rs",
        "pub fn target_fn() -> i32 { 42 }\n",
    );

    let mut mcp = spawn_mcp_handshaken(&repo);
    let tools = mcp.tools_list().expect("tools/list");
    let baseline_preflight = mcp
        .tools_call(
            "loom_preflight_symbols",
            build_preflight_args(&tools, &["target_fn"], None),
        )
        .expect("baseline preflight target_fn");
    assert_mcp_ok(&baseline_preflight, "baseline preflight target_fn");
    let baseline_text = extract_mcp_text(&baseline_preflight);

    let cli_empty = repo.loom_in(&vp, &["drop", "--reason", ""]);
    assert_clean_failure(&cli_empty, "drop with empty reason");

    let cli_ws = repo.loom_in(&vp, &["drop", "--reason", "   "]);
    assert_clean_failure(&cli_ws, "drop with whitespace-only reason");

    let mcp_res = mcp.tools_call(
        "loom_record_deadend",
        build_record_deadend_args(&tools, "vp-empty-reason", "", None, &["target_fn"]),
    );
    assert_mcp_error(mcp_res, "loom_record_deadend with empty reason");
    mcp.tools_list().expect("MCP server remains alive");

    let preflight_before_valid = mcp
        .tools_call(
            "loom_preflight_symbols",
            build_preflight_args(&tools, &["target_fn"], None),
        )
        .expect("preflight target_fn before valid drop");
    assert_mcp_ok(
        &preflight_before_valid,
        "preflight target_fn before valid drop",
    );
    assert_eq!(
        extract_mcp_text(&preflight_before_valid),
        baseline_text,
        "rejected empty/whitespace drops must not pollute negative_index"
    );

    // Viewport must still be active and cleanly droppable with a valid reason
    repo.loom_in(&vp, &["drop", "--reason", "valid fallback reason"])
        .assert_success();
    assert_eq!(repo.read_file("src/lib.rs").expect("read trunk"), orig);

    let preflight_after_valid = mcp
        .tools_call(
            "loom_preflight_symbols",
            build_preflight_args(&tools, &["target_fn"], None),
        )
        .expect("preflight target_fn after valid drop");
    assert_mcp_ok(
        &preflight_after_valid,
        "preflight target_fn after valid drop",
    );
    assert!(
        extract_mcp_text(&preflight_after_valid).contains("valid fallback reason"),
        "expected valid fallback reason in preflight after drop"
    );
    drop(mcp);
}

#[test]
fn tier2_f21_drop_invalid_failure_kind_rejected() {
    let orig = "pub fn risky_fn() -> i32 { 1 }\n";
    let repo = init_loom_repo_with_file("src/lib.rs", orig, false);
    let vp = spawn_and_edit_vp(
        &repo,
        "vp-bad-kind",
        "edit risky_fn",
        "src/lib.rs",
        "pub fn risky_fn() -> i32 { 9 }\n",
    );

    let cli_out = repo.loom_in(
        &vp,
        &[
            "drop",
            "--reason",
            "invalid kind test",
            "--kind",
            "NotARealFailureKind_XYZ",
        ],
    );
    assert_clean_failure(&cli_out, "drop with invalid --kind");

    let mut mcp = spawn_mcp_handshaken(&repo);
    let tools = mcp.tools_list().expect("tools/list");
    let mcp_res = mcp.tools_call(
        "loom_record_deadend",
        build_record_deadend_args(
            &tools,
            "vp-bad-kind",
            "invalid kind test",
            Some("NotARealFailureKind_XYZ"),
            &["risky_fn"],
        ),
    );
    assert_mcp_error(mcp_res, "loom_record_deadend with invalid FailureKind");

    let preflight = mcp
        .tools_call(
            "loom_preflight_symbols",
            build_preflight_args(&tools, &["risky_fn"], None),
        )
        .expect("preflight risky_fn");
    assert_mcp_ok(&preflight, "preflight risky_fn");
    assert!(
        !extract_mcp_text(&preflight).contains("invalid kind test"),
        "rejected invalid-kind deadend must not pollute negative_index"
    );
    drop(mcp);

    repo.loom(&["status"]).assert_success();
    repo.loom_in(&vp, &["drop", "--reason", "valid default kind"])
        .assert_success();
}

#[test]
fn tier2_f21_drop_with_no_active_viewport() {
    let orig = "pub fn idle_fn() -> i32 { 1 }\n";
    let repo = init_loom_repo_with_file("src/lib.rs", orig, false);

    // 1. Fresh repo with 0 viewports
    let out_zero = repo.loom(&["drop", "--reason", "no viewport exists"]);
    assert_clean_failure(&out_zero, "drop with zero viewports");

    // 2. Spawn once, edit, and drop cleanly -> back to 0 active viewports
    let vp_once = spawn_and_edit_vp(
        &repo,
        "vp-once",
        "legit attempt",
        "src/lib.rs",
        "pub fn idle_fn() -> i32 { 2 }\n",
    );
    repo.loom_in(&vp_once, &["drop", "--reason", "legit drop"])
        .assert_success();

    let out_after = repo.loom(&["drop", "--reason", "no active viewport left"]);
    assert_clean_failure(&out_after, "drop after all viewports dropped");

    // 3. Double drop on already-dropped viewport if dir still present
    if vp_once.exists() {
        let out_double = repo.loom_in(&vp_once, &["drop", "--reason", "double drop"]);
        assert_clean_failure(&out_double, "double drop on dropped viewport");
    }

    // 4. MCP record_deadend targeting nonexistent viewport
    let mut mcp = spawn_mcp_handshaken(&repo);
    let tools = mcp.tools_list().expect("tools/list");
    let mcp_res = mcp.tools_call(
        "loom_record_deadend",
        build_record_deadend_args(&tools, "nonexistent-vp", "ghost deadend", None, &["idle_fn"]),
    );
    assert_mcp_error(mcp_res, "loom_record_deadend on nonexistent viewport");

    let preflight = mcp
        .tools_call(
            "loom_preflight_symbols",
            build_preflight_args(&tools, &["idle_fn"], None),
        )
        .expect("preflight idle_fn");
    assert_mcp_ok(&preflight, "preflight idle_fn");
    let text = extract_mcp_text(&preflight);
    assert!(text.contains("legit drop"));
    assert!(!text.contains("no viewport exists"));
    assert!(!text.contains("no active viewport left"));
    assert!(!text.contains("double drop"));
    assert!(!text.contains("ghost deadend"));
    drop(mcp);

    assert_eq!(repo.read_file("src/lib.rs").expect("read trunk"), orig);
    repo.loom(&["status"]).assert_success();
}

#[test]
fn tier2_f21_preflight_empty_symbol_list_clean() {
    let orig = "pub fn existing() -> i32 { 1 }\n";
    let repo = init_loom_repo_with_file("src/lib.rs", orig, false);

    let mut mcp = spawn_mcp_handshaken(&repo);
    let tools = mcp.tools_list().expect("tools/list");

    // Phase A: empty negative_index + empty symbol list
    let res_a = mcp
        .tools_call(
            "loom_preflight_symbols",
            build_preflight_args(&tools, &[], None),
        )
        .expect("preflight empty symbols (empty index)");
    assert_ne!(
        res_a.get("isError").and_then(Value::as_bool),
        Some(true),
        "empty symbol list should succeed cleanly: {res_a}"
    );

    // Phase B: populate negative_index, then query empty symbol list again
    let vp = spawn_and_edit_vp(
        &repo,
        "vp-seed",
        "seed negative edge",
        "src/lib.rs",
        "pub fn existing() -> i32 { 99 }\n",
    );
    repo.loom_in(
        &vp,
        &[
            "drop",
            "--reason",
            "known deadlock on existing",
            "--kind",
            "Deadlock",
        ],
    )
    .assert_success();

    let res_b = mcp
        .tools_call(
            "loom_preflight_symbols",
            build_preflight_args(&tools, &[], None),
        )
        .expect("preflight empty symbols (populated index)");
    assert_ne!(
        res_b.get("isError").and_then(Value::as_bool),
        Some(true),
        "empty symbol list should succeed cleanly with populated index: {res_b}"
    );
    assert!(
        !extract_mcp_text(&res_b).contains("known deadlock on existing"),
        "empty symbol query must not return unrelated NegativeEdge warnings"
    );

    // Control: querying ["existing"] surfaces the seeded warning
    let res_ctrl = mcp
        .tools_call(
            "loom_preflight_symbols",
            build_preflight_args(&tools, &["existing"], None),
        )
        .expect("preflight control query");
    assert_mcp_ok(&res_ctrl, "preflight control query");
    assert!(
        extract_mcp_text(&res_ctrl).contains("known deadlock on existing"),
        "control query for 'existing' must surface NegativeEdge warning"
    );

    // Confirm MCP server still handles radar poll cleanly
    mcp.tools_call("loom_radar_poll", json!({}))
        .expect("loom_radar_poll after empty preflight");
}

// ============================================================================
// F22: gix Bidirectional Git Push & Pull (5 Tier 2 tests)
// ============================================================================

#[test]
fn tier2_f22_git_push_nonexistent_remote_errors() {
    let repo = init_loom_repo_with_file("src/lib.rs", "pub fn alpha() -> i32 { 1 }\n", true);
    let vp = spawn_and_edit_vp(
        &repo,
        "vp1",
        "add beta",
        "src/lib.rs",
        "pub fn alpha() -> i32 { 1 }\npub fn beta() -> i32 { 2 }\n",
    );
    repo.loom_in(&vp, &["seal", "-v", PASS_CMD, "-m", "seal beta"])
        .assert_success();

    let out_named = repo.loom(&["git", "push", "missing-remote-e2e", "main"]);
    assert_clean_failure(&out_named, "git push to nonexistent named remote");

    let out_default = repo.loom(&["git", "push"]);
    assert_clean_failure(&out_default, "git push with no configured origin remote");

    let log_out = repo.loom(&["log", "--capsules"]);
    log_out.assert_success();
    assert!(log_out.stdout_str().contains("seal beta"));
    repo.git(&["status", "--porcelain"]).assert_success();
}

#[test]
fn tier2_f22_git_push_without_colocated_git_errors() {
    let repo = init_loom_repo_with_file("src/lib.rs", "pub fn base() -> i32 { 0 }\n", false);
    let vp = spawn_and_edit_vp(
        &repo,
        "vp1",
        "update base",
        "src/lib.rs",
        "pub fn base() -> i32 { 1 }\n",
    );
    repo.loom_in(&vp, &["seal", "-v", PASS_CMD, "-m", "update base"])
        .assert_success();

    let push_out = repo.loom(&["git", "push", "origin", "main"]);
    assert_clean_failure(&push_out, "git push without colocated .git");

    let pull_out = repo.loom(&["git", "pull", "origin", "main"]);
    assert_clean_failure(&pull_out, "git pull without colocated .git");

    assert!(
        !repo.exists(".git"),
        "loom git push/pull must not synthesize a .git directory"
    );
    repo.loom(&["status"]).assert_success();
    repo.loom(&["log", "--capsules"]).assert_success();
}

#[test]
fn tier2_f22_git_pull_non_fast_forward_or_conflict() {
    // Declare repo_a (owner of bare) BEFORE repo_b so repo_b drops first on exit.
    let (repo_a, bare) = setup_colocated_with_bare("src/lib.rs", "pub fn shared() -> i32 { 1 }\n");
    let repo_b = init_synced_clone_repo(&bare, "Loom E2E B", "loom-b@example.com");

    // Advance bare from repo_a
    let vp_a = spawn_and_edit_vp(
        &repo_a,
        "vp-a",
        "set 100",
        "src/lib.rs",
        "pub fn shared() -> i32 { 100 }\n",
    );
    repo_a
        .loom_in(&vp_a, &["seal", "-v", PASS_CMD, "-m", "set 100"])
        .assert_success();
    repo_a
        .loom(&["git", "push", "origin", "main"])
        .assert_success();

    // Diverge locally in repo_b on the same symbol `shared`
    repo_b
        .write_file("src/lib.rs", "pub fn shared() -> i32 { 200 }\n")
        .expect("write diverged file in repo_b");
    repo_b.git(&["add", "src/lib.rs"]).assert_success();
    repo_b
        .git(&["commit", "-m", "local divergence set 200"])
        .assert_success();
    let head_b_before = repo_b
        .git(&["rev-parse", "HEAD"])
        .assert_success()
        .stdout_str()
        .trim()
        .to_string();

    let pull_out = repo_b.loom(&["git", "pull", "origin", "main"]);
    assert_clean_failure(&pull_out, "git pull with non-fast-forward conflict");

    let content_b = repo_b.read_file("src/lib.rs").expect("read repo_b src/lib.rs");
    assert_no_conflict_markers(&content_b, "repo_b after failed pull");
    assert!(
        content_b.contains("200"),
        "expected repo_b local state preserved on aborted pull, got:\n{content_b}"
    );
    assert_eq!(
        repo_b
            .git(&["rev-parse", "HEAD"])
            .assert_success()
            .stdout_str()
            .trim(),
        head_b_before
    );
    repo_b.loom(&["status"]).assert_success();
}

#[test]
fn tier2_f22_git_push_idempotent_when_up_to_date() {
    let (repo, bare) = setup_colocated_with_bare("src/lib.rs", "pub fn base() -> i32 { 1 }\n");
    let vp = spawn_and_edit_vp(
        &repo,
        "vp1",
        "add second",
        "src/lib.rs",
        "pub fn base() -> i32 { 1 }\npub fn second() -> i32 { 2 }\n",
    );
    repo.loom_in(&vp, &["seal", "-v", PASS_CMD, "-m", "add second"])
        .assert_success();

    let initial_tip = repo
        .git_in(&bare, &["rev-parse", "refs/heads/main"])
        .assert_success()
        .stdout_str()
        .trim()
        .to_string();

    repo.loom(&["git", "push", "origin", "main"])
        .assert_success();
    let tip1 = repo
        .git_in(&bare, &["rev-parse", "refs/heads/main"])
        .assert_success()
        .stdout_str()
        .trim()
        .to_string();
    let count1 = repo
        .git_in(&bare, &["rev-list", "--count", "refs/heads/main"])
        .assert_success()
        .stdout_str()
        .trim()
        .to_string();
    assert_ne!(
        tip1, initial_tip,
        "first loom git push must advance bare refs/heads/main beyond initial commit"
    );
    assert!(
        count1
            .parse::<u64>()
            .expect("parse bare commit count after first push")
            >= 2,
        "expected at least 2 commits on bare refs/heads/main after push, got {count1}"
    );
    let notes1 = repo.git_in(&bare, &["show-ref", "refs/notes/loom"]);
    notes1.assert_success();

    // Second push with zero new capsules or dead-ends must be idempotent
    repo.loom(&["git", "push", "origin", "main"])
        .assert_success();
    let tip2 = repo
        .git_in(&bare, &["rev-parse", "refs/heads/main"])
        .assert_success()
        .stdout_str()
        .trim()
        .to_string();
    let count2 = repo
        .git_in(&bare, &["rev-list", "--count", "refs/heads/main"])
        .assert_success()
        .stdout_str()
        .trim()
        .to_string();
    let notes2 = repo.git_in(&bare, &["show-ref", "refs/notes/loom"]);
    notes2.assert_success();

    assert_eq!(tip1, tip2);
    assert_eq!(count1, count2);
    assert_eq!(notes1.stdout_str().trim(), notes2.stdout_str().trim());
}

#[test]
fn tier2_f22_git_pull_empty_bare_remote() {
    let orig = "pub fn local_only() -> i32 { 42 }\n";
    let repo = init_loom_repo_with_file("src/lib.rs", orig, true);
    let bare = repo.init_bare_remote().expect("init_bare_remote");
    let bare_str = bare.to_string_lossy().into_owned();
    repo.git(&["remote", "add", "origin", &bare_str])
        .assert_success();

    assert!(
        repo.git_in(&bare, &["show-ref"])
            .stdout_str()
            .trim()
            .is_empty(),
        "bare remote must start completely empty"
    );
    let head_before = repo
        .git(&["rev-parse", "HEAD"])
        .assert_success()
        .stdout_str()
        .trim()
        .to_string();

    let pull_out = repo.loom(&["git", "pull", "origin", "main"]);
    assert!(
        pull_out.exit_code().is_some(),
        "loom git pull on empty bare remote must exit cleanly"
    );
    assert_no_panic(&pull_out);
    if !pull_out.success() {
        assert!(!combined_output(&pull_out).trim().is_empty());
    }

    assert_eq!(repo.read_file("src/lib.rs").expect("read trunk"), orig);
    assert_eq!(
        repo.git(&["rev-parse", "HEAD"])
            .assert_success()
            .stdout_str()
            .trim(),
        head_before
    );
    assert!(repo
        .git_in(&bare, &["show-ref"])
        .stdout_str()
        .trim()
        .is_empty());
    repo.loom(&["status"]).assert_success();
}
