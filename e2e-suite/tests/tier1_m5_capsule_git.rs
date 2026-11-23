use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use e2e_suite::{wait_for_radar_status, HarnessError, McpClient, TestRepo};

fn normalize_nl(s: &str) -> String {
    s.replace("\r\n", "\n")
}

fn init_loom_repo_with_src(files: &[(&str, &str)]) -> TestRepo {
    let repo = TestRepo::new().expect("create TestRepo");
    for (rel, content) in files {
        repo.write_file(rel, content)
            .expect("write initial source file");
    }
    let out = repo.loom(&["init"]);
    out.assert_success();
    repo
}

fn init_colocated_git_with_bare(files: &[(&str, &str)]) -> (TestRepo, PathBuf, String) {
    let repo = TestRepo::new().expect("create TestRepo");
    for (rel, content) in files {
        repo.write_file(rel, content)
            .expect("write initial source file");
    }
    // Always initialize git repo BEFORE `loom init --colocate-git`
    repo.init_git_repo()
        .expect("init_git_repo prior to loom init --colocate-git");
    let init_out = repo.loom(&["init", "--colocate-git"]);
    init_out.assert_success();

    let bare = repo.init_bare_remote().expect("init_bare_remote");
    // Use raw TempDir path (never fs::canonicalize / UNC \\?\)
    let bare_str = bare.to_string_lossy().into_owned();
    let add_out = repo.git(&["remote", "add", "origin", &bare_str]);
    add_out.assert_success();
    let push_out = repo.git(&["push", "-u", "origin", "main"]);
    push_out.assert_success();
    (repo, bare, bare_str)
}

fn spawn_vp(repo: &TestRepo, name: &str, intent: &str) -> PathBuf {
    let out = repo.loom(&["spawn", "-n", name, "-m", intent]);
    out.assert_success();
    repo.viewport_dir(name)
}

fn mcp_handshake(client: &mut McpClient) -> Value {
    let res = match client.handshake() {
        Ok(val) => val,
        Err(HarnessError::JsonRpc { .. }) => {
            let val = client
                .initialize_with_version("2025-03-26")
                .expect("MCP initialize fallback 2025-03-26");
            client
                .send_initialized()
                .expect("MCP notifications/initialized");
            val
        }
        Err(e) => panic!("MCP handshake failed: {e}"),
    };
    let proto = res
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        !proto.is_empty(),
        "expected non-empty protocolVersion in MCP initialize response: {res}"
    );
    res
}

fn find_tool_schema(tools_res: &Value, tool_name: &str) -> Option<Value> {
    let tools = tools_res.get("tools").and_then(Value::as_array)?;
    for tool in tools {
        if tool.get("name").and_then(Value::as_str) == Some(tool_name) {
            return tool.get("inputSchema").cloned();
        }
    }
    None
}

fn build_preflight_args(schema: &Value, symbols: &[&str], viewport: Option<&str>) -> Value {
    let props = schema.get("properties").and_then(Value::as_object);
    let mut map = Map::new();

    if let Some(p) = props {
        if p.contains_key("symbols") {
            map.insert("symbols".into(), json!(symbols));
        } else if p.contains_key("symbol_fqns") {
            map.insert("symbol_fqns".into(), json!(symbols));
        } else if p.contains_key("fqns") {
            map.insert("fqns".into(), json!(symbols));
        } else if p.contains_key("symbol") {
            map.insert("symbol".into(), json!(symbols.first().copied().unwrap_or("")));
        } else {
            map.insert("symbols".into(), json!(symbols));
        }

        if let Some(vp) = viewport {
            if p.contains_key("viewport") {
                map.insert("viewport".into(), json!(vp));
            } else if p.contains_key("viewport_name") {
                map.insert("viewport_name".into(), json!(vp));
            }
        }
    } else {
        map.insert("symbols".into(), json!(symbols));
    }

    Value::Object(map)
}

fn build_record_deadend_args(
    schema: &Value,
    viewport: &str,
    touched_symbols: &[&str],
    lesson: &str,
    kind: &str,
) -> Value {
    let props = schema.get("properties").and_then(Value::as_object);
    let mut map = Map::new();

    if let Some(p) = props {
        if p.contains_key("viewport") {
            map.insert("viewport".into(), json!(viewport));
        } else if p.contains_key("viewport_name") {
            map.insert("viewport_name".into(), json!(viewport));
        } else if p.contains_key("name") {
            map.insert("name".into(), json!(viewport));
        } else {
            map.insert("viewport".into(), json!(viewport));
        }

        if p.contains_key("touched_symbols") {
            map.insert("touched_symbols".into(), json!(touched_symbols));
        } else if p.contains_key("symbols") {
            map.insert("symbols".into(), json!(touched_symbols));
        } else {
            map.insert("touched_symbols".into(), json!(touched_symbols));
        }

        if p.contains_key("lesson_summary") {
            map.insert("lesson_summary".into(), json!(lesson));
        } else if p.contains_key("reason") {
            map.insert("reason".into(), json!(lesson));
        } else if p.contains_key("summary") {
            map.insert("summary".into(), json!(lesson));
        } else if p.contains_key("lesson") {
            map.insert("lesson".into(), json!(lesson));
        } else {
            map.insert("lesson_summary".into(), json!(lesson));
        }

        if p.contains_key("kind") {
            map.insert("kind".into(), json!(kind));
        } else if p.contains_key("failure_kind") {
            map.insert("failure_kind".into(), json!(kind));
        } else {
            map.insert("kind".into(), json!(kind));
        }
    } else {
        map.insert("viewport".into(), json!(viewport));
        map.insert("touched_symbols".into(), json!(touched_symbols));
        map.insert("lesson_summary".into(), json!(lesson));
        map.insert("kind".into(), json!(kind));
    }

    Value::Object(map)
}

fn mcp_tool_text(call_res: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(sc) = call_res.get("structuredContent") {
        if !sc.is_null() {
            parts.push(sc.to_string());
        }
    }
    if let Some(items) = call_res.get("content").and_then(Value::as_array) {
        for item in items {
            if let Some(txt) = item.get("text").and_then(Value::as_str) {
                if !txt.is_empty() {
                    parts.push(txt.to_string());
                }
            }
        }
    }
    if parts.is_empty() {
        call_res.to_string()
    } else {
        parts.join("\n")
    }
}

fn portable_pass_cmd(marker: &str, rel_win: &str, rel_unix: &str) -> String {
    if cfg!(windows) {
        format!("cmd /C findstr /C:{marker} {rel_win} >nul")
    } else {
        format!("grep -q {marker} {rel_unix}")
    }
}

fn wal_total_bytes(repo: &TestRepo) -> u64 {
    let wal_dir = repo.root().join(".loom").join("wal");
    let Ok(entries) = fs::read_dir(&wal_dir) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        if let Ok(meta) = entry.metadata() {
            if meta.is_file() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

fn wait_for_wal_growth(repo: &TestRepo, baseline: u64, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if wal_total_bytes(repo) > baseline {
            return;
        }
        let now = Instant::now();
        if now >= deadline {
            panic!(
                "WAL size did not grow above baseline {baseline} bytes within {timeout:?} (current={})",
                wal_total_bytes(repo)
            );
        }
        std::thread::sleep(Duration::from_millis(25).min(deadline - now));
    }
}

fn extract_extra_failure_kind(help_text: &str) -> Option<String> {
    let lower = help_text.to_ascii_lowercase();
    let idx = lower.find("possible values:")?;
    let rest = &help_text[idx + "possible values:".len()..];
    let end = rest.find(']').unwrap_or(rest.len());
    let list = &rest[..end];
    for raw in list.split(',') {
        let candidate = raw.trim().trim_matches(|c| c == '[' || c == ']' || c == '"' || c == '\'');
        if !candidate.is_empty() && !candidate.eq_ignore_ascii_case("deadlock") {
            return Some(candidate.to_string());
        }
    }
    None
}

// ============================================================================
// Feature F20: Unstaged AstPatch Capture & loom seal (5 Tests)
// ============================================================================

#[test]
fn tier1_f20_unstaged_edits_captured_without_staging() {
    let repo = init_loom_repo_with_src(&[("src/lib.rs", "pub fn alpha() -> i32 { 1 }\n")]);
    let vp1 = spawn_vp(&repo, "vp1", "unstaged alpha edit");
    let _ = wait_for_radar_status(&vp1, Duration::from_secs(5), |_| true);

    let wal_before = wal_total_bytes(&repo);

    // Edit directly in viewport without any `add` or staging command
    fs::write(vp1.join("src/lib.rs"), "pub fn alpha() -> i32 { 42 }\n")
        .expect("overwrite vp1 src/lib.rs");

    wait_for_wal_growth(&repo, wal_before, Duration::from_secs(5));

    let seal_out = repo.loom_in(&vp1, &["seal", "-m", "seal unstaged edit"]);
    seal_out.assert_success();

    let trunk = repo.read_file("src/lib.rs").expect("read trunk src/lib.rs");
    assert!(
        trunk.contains("42"),
        "expected sealed trunk src/lib.rs to contain 42, got:\n{trunk}"
    );
}

#[test]
fn tier1_f20_seal_with_passing_verify_cmd() {
    let repo = init_loom_repo_with_src(&[("src/lib.rs", "pub fn compute() -> i32 { 1 }\n")]);
    let vp1 = spawn_vp(&repo, "vp1", "verify passing");

    fs::write(
        vp1.join("src/lib.rs"),
        "pub fn compute() -> i32 { /* seal_verified_marker */ 77 }\n",
    )
    .expect("write verified edit in vp1");

    let vcmd = portable_pass_cmd("seal_verified_marker", r"src\lib.rs", "src/lib.rs");
    let seal_out = repo.loom_in(&vp1, &["seal", "-v", &vcmd, "-m", "verified compute update"]);
    seal_out.assert_success();

    let trunk = repo.read_file("src/lib.rs").expect("read trunk src/lib.rs");
    assert!(
        trunk.contains("77") && trunk.contains("seal_verified_marker"),
        "expected trunk to contain verified edit, got:\n{trunk}"
    );

    let list_out = repo.loom(&["list"]);
    list_out.assert_success();
    let status_out = repo.loom(&["status"]);
    status_out.assert_success();

    let list_str = normalize_nl(&list_out.stdout_str());
    let status_str = normalize_nl(&status_out.stdout_str());
    let list_vp1_lines: Vec<&str> = list_str
        .lines()
        .filter(|line| line.contains("vp1"))
        .collect();
    assert!(
        !list_vp1_lines.is_empty(),
        "expected vp1 to appear in `loom list`, got:\n{list_str}"
    );
    assert!(
        list_vp1_lines
            .iter()
            .any(|line| line.to_ascii_lowercase().contains("sealed")),
        "expected vp1 in Sealed state in `loom list`, got lines:\n{}",
        list_vp1_lines.join("\n")
    );
    assert!(
        !list_vp1_lines
            .iter()
            .any(|line| line.to_ascii_lowercase().contains("active")),
        "expected vp1 not to remain Active in `loom list` after seal, got lines:\n{}",
        list_vp1_lines.join("\n")
    );

    let combined = format!("{list_str}\n{status_str}");
    assert!(
        combined.to_ascii_lowercase().contains("sealed"),
        "expected Sealed state in `loom list` / `loom status`, got:\n{combined}"
    );
}

#[test]
fn tier1_f20_seal_without_verify_cmd() {
    let repo = init_loom_repo_with_src(&[("src/lib.rs", "pub fn helper() -> i32 { 10 }\n")]);
    let vp1 = spawn_vp(&repo, "vp1", "seal without verify");

    fs::write(vp1.join("src/lib.rs"), "pub fn helper() -> i32 { 20 }\n")
        .expect("write unverified edit in vp1");

    let seal_out = repo.loom_in(&vp1, &["seal", "-m", "unverified quick seal"]);
    seal_out.assert_success();

    let trunk = repo.read_file("src/lib.rs").expect("read trunk src/lib.rs");
    assert!(
        trunk.contains("20"),
        "expected trunk to contain 20, got:\n{trunk}"
    );

    let log_out = repo.loom(&["log", "--capsules"]);
    log_out.assert_success();
    let log_str = normalize_nl(&log_out.stdout_str());
    assert!(
        log_str.contains("unverified quick seal"),
        "expected `loom log --capsules` to include summary, got:\n{log_str}"
    );
}

#[test]
fn tier1_f20_seal_creates_verification_proof_in_log() {
    let repo = init_loom_repo_with_src(&[("src/lib.rs", "pub fn target_fn() -> i32 { 1 }\n")]);
    let vp1 = spawn_vp(&repo, "vp1", "proof audit");

    fs::write(
        vp1.join("src/lib.rs"),
        "pub fn target_fn() -> i32 { /* proof_token */ 99 }\n",
    )
    .expect("write target_fn edit in vp1");

    let vcmd = portable_pass_cmd("proof_token", r"src\lib.rs", "src/lib.rs");
    let seal_out = repo.loom_in(&vp1, &["seal", "-v", &vcmd, "-m", "capsule with proof"]);
    seal_out.assert_success();

    let log_out = repo.loom(&["log", "--capsules"]);
    log_out.assert_success();
    let log_str = normalize_nl(&log_out.stdout_str());
    let log_lower = log_str.to_ascii_lowercase();
    assert!(
        log_str.contains("capsule with proof"),
        "expected capsule summary in `loom log --capsules`, got:\n{log_str}"
    );
    assert!(
        log_lower.contains("proof_token")
            || log_lower.contains("verified")
            || log_lower.contains("passed")
            || log_lower.contains("exit_code")
            || log_lower.contains("exit: 0")
            || log_lower.contains("proof"),
        "expected VerificationProof details in `loom log --capsules`, got:\n{log_str}"
    );

    let sym_out = repo.loom(&["log", "--capsules", "--symbol", "target_fn"]);
    sym_out.assert_success();
    let sym_str = normalize_nl(&sym_out.stdout_str());
    assert!(
        sym_str.contains("capsule with proof"),
        "expected `loom log --capsules --symbol target_fn` to match capsule, got:\n{sym_str}"
    );
}

#[test]
fn tier1_f20_sequential_seals_from_two_viewports() {
    let repo = init_loom_repo_with_src(&[(
        "src/lib.rs",
        "pub fn fn_a() -> i32 { 1 }\n\npub fn fn_b() -> i32 { 2 }\n",
    )]);

    let vp1 = spawn_vp(&repo, "vp1", "edit fn_a");
    let vp2 = spawn_vp(&repo, "vp2", "edit fn_b");

    fs::write(
        vp1.join("src/lib.rs"),
        "pub fn fn_a() -> i32 { 101 }\n\npub fn fn_b() -> i32 { 2 }\n",
    )
    .expect("edit fn_a in vp1");
    fs::write(
        vp2.join("src/lib.rs"),
        "pub fn fn_a() -> i32 { 1 }\n\npub fn fn_b() -> i32 { 202 }\n",
    )
    .expect("edit fn_b in vp2");

    repo.loom_in(&vp1, &["seal", "-m", "seal fn_a"])
        .assert_success();
    repo.loom_in(&vp2, &["seal", "-m", "seal fn_b"])
        .assert_success();

    let trunk = repo.read_file("src/lib.rs").expect("read trunk src/lib.rs");
    assert!(
        trunk.contains("101") && trunk.contains("202"),
        "expected sequential disjoint seals to merge both 101 and 202 into trunk, got:\n{trunk}"
    );

    let log_out = repo.loom(&["log", "--capsules"]);
    log_out.assert_success();
    let log_str = normalize_nl(&log_out.stdout_str());
    assert!(
        log_str.contains("seal fn_a") && log_str.contains("seal fn_b"),
        "expected both capsules in `loom log --capsules`, got:\n{log_str}"
    );
}

// ============================================================================
// Feature F21: NegativeEdge Failure Ledger (`loom drop`) (5 Tests)
// ============================================================================

#[test]
fn tier1_f21_drop_records_negative_edge_and_restores_clean() {
    let repo = init_loom_repo_with_src(&[("src/lib.rs", "pub fn stable() -> i32 { 1 }\n")]);
    let vp1 = spawn_vp(&repo, "vp1", "bad experiment");

    fs::write(vp1.join("src/lib.rs"), "pub fn stable() -> i32 { 999 }\n")
        .expect("modify stable in vp1");
    fs::write(vp1.join("src/scratch.rs"), "pub fn scratch() {}\n")
        .expect("create scratch file in vp1");
    let _ = wait_for_radar_status(&vp1, Duration::from_secs(2), |_| true);

    repo.loom_in(
        &vp1,
        &[
            "drop",
            "--reason",
            "breaks invariant X",
            "--kind",
            "Deadlock",
        ],
    )
    .assert_success();

    let trunk = normalize_nl(&repo.read_file("src/lib.rs").expect("read trunk"));
    assert_eq!(
        trunk, "pub fn stable() -> i32 { 1 }\n",
        "dropped viewport changes must never merge to trunk"
    );

    if vp1.join("src/lib.rs").exists() {
        let vp1_src = fs::read_to_string(vp1.join("src/lib.rs")).expect("read vp1 src/lib.rs");
        assert!(
            vp1_src.contains("1") && !vp1_src.contains("999"),
            "expected clean viewport restore on vp1, got:\n{vp1_src}"
        );
        assert!(
            !vp1.join("src/scratch.rs").exists(),
            "expected untracked scratch.rs to be removed on clean restore"
        );
    }

    let list_out = repo.loom(&["list"]);
    list_out.assert_success();
    let status_out = repo.loom(&["status"]);
    status_out.assert_success();
    let combined_state = format!(
        "{}\n{}",
        list_out.stdout_str().to_ascii_lowercase(),
        status_out.stdout_str().to_ascii_lowercase()
    );
    assert!(
        combined_state.contains("dropped"),
        "expected Dropped state in list/status, got:\n{combined_state}"
    );

    let log_out = repo.loom(&["log"]);
    log_out.assert_success();
    let mut mcp = repo.spawn_mcp_stdio(None).expect("spawn mcp stdio");
    mcp_handshake(&mut mcp);
    let tools = mcp.tools_list().expect("tools/list");
    let schema = find_tool_schema(&tools, "loom_preflight_symbols")
        .expect("loom_preflight_symbols listed in tools/list");
    let preflight = mcp
        .tools_call(
            "loom_preflight_symbols",
            build_preflight_args(&schema, &["stable"], None),
        )
        .expect("call loom_preflight_symbols");
    let preflight_txt = mcp_tool_text(&preflight);

    assert!(
        log_out.stdout_str().contains("breaks invariant X")
            || preflight_txt.contains("breaks invariant X"),
        "expected NegativeEdge reason recorded in log/preflight; log={} preflight={}",
        log_out.stdout_str(),
        preflight_txt
    );
}

#[test]
fn tier1_f21_preflight_injects_negative_edge_warning() {
    let repo = init_loom_repo_with_src(&[(
        "src/auth.rs",
        "pub fn authenticate(user: &str) -> bool { !user.is_empty() }\n",
    )]);

    let vp1 = spawn_vp(&repo, "vp1", "attempt auth optimization");
    fs::write(
        vp1.join("src/auth.rs"),
        "pub fn authenticate(_user: &str) -> bool { false }\n",
    )
    .expect("edit authenticate in vp1");
    repo.loom_in(
        &vp1,
        &[
            "drop",
            "--reason",
            "mutex lock inversion on auth_cache",
            "--kind",
            "Deadlock",
        ],
    )
    .assert_success();

    let _vp2 = spawn_vp(&repo, "vp2", "retry authenticate");

    let mut mcp = repo.spawn_mcp_stdio(None).expect("spawn mcp stdio");
    mcp_handshake(&mut mcp);
    let tools = mcp.tools_list().expect("tools/list");
    let schema = find_tool_schema(&tools, "loom_preflight_symbols")
        .expect("loom_preflight_symbols listed in tools/list");

    let res = mcp
        .tools_call(
            "loom_preflight_symbols",
            build_preflight_args(&schema, &["authenticate"], Some("vp2")),
        )
        .expect("call loom_preflight_symbols");
    assert_ne!(
        res.get("isError"),
        Some(&Value::Bool(true)),
        "expected non-error preflight response: {res}"
    );

    let txt = mcp_tool_text(&res);
    assert!(
        txt.contains("mutex lock inversion on auth_cache") && txt.contains("Deadlock"),
        "expected NegativeEdge warning and kind in preflight output, got:\n{txt}"
    );
}

#[test]
fn tier1_f21_mcp_record_deadend_records_failure() {
    let repo = init_loom_repo_with_src(&[("src/cache.rs", "pub fn evict_lru() -> usize { 0 }\n")]);
    let vp1 = spawn_vp(&repo, "vp1", "mcp deadend");

    fs::write(
        vp1.join("src/cache.rs"),
        "pub fn evict_lru() -> usize { 42 }\n",
    )
    .expect("edit evict_lru in vp1");

    let mut mcp = repo.spawn_mcp_stdio(None).expect("spawn mcp stdio");
    mcp_handshake(&mut mcp);
    let tools = mcp.tools_list().expect("tools/list");

    let deadend_schema = find_tool_schema(&tools, "loom_record_deadend")
        .expect("loom_record_deadend listed in tools/list");
    let deadend_args = build_record_deadend_args(
        &deadend_schema,
        "vp1",
        &["evict_lru"],
        "evicting during read lock stalls workers",
        "Deadlock",
    );
    let rec_res = mcp
        .tools_call("loom_record_deadend", deadend_args)
        .expect("call loom_record_deadend");
    assert_ne!(
        rec_res.get("isError"),
        Some(&Value::Bool(true)),
        "expected loom_record_deadend to succeed, got: {rec_res}"
    );

    let preflight_schema = find_tool_schema(&tools, "loom_preflight_symbols")
        .expect("loom_preflight_symbols listed in tools/list");
    let preflight_res = mcp
        .tools_call(
            "loom_preflight_symbols",
            build_preflight_args(&preflight_schema, &["evict_lru"], None),
        )
        .expect("call loom_preflight_symbols");
    let preflight_txt = mcp_tool_text(&preflight_res);
    assert!(
        preflight_txt.contains("evicting during read lock stalls workers"),
        "expected recorded deadend warning in preflight, got:\n{preflight_txt}"
    );

    let trunk = repo.read_file("src/cache.rs").expect("read trunk cache.rs");
    assert!(
        trunk.contains("0") && !trunk.contains("42"),
        "expected deadend edit not to merge to trunk, got:\n{trunk}"
    );
}

#[test]
fn tier1_f21_drop_unrelated_symbol_does_not_warn_clean_symbol() {
    let repo = init_loom_repo_with_src(&[(
        "src/lib.rs",
        "pub fn dirty_fn() -> i32 { 1 }\n\npub fn clean_fn() -> i32 { 2 }\n",
    )]);
    let vp1 = spawn_vp(&repo, "vp1", "dirty only");

    fs::write(
        vp1.join("src/lib.rs"),
        "pub fn dirty_fn() -> i32 { -1 }\n\npub fn clean_fn() -> i32 { 2 }\n",
    )
    .expect("edit dirty_fn only");

    repo.loom_in(
        &vp1,
        &[
            "drop",
            "--reason",
            "overflow in dirty_fn",
            "--kind",
            "Deadlock",
        ],
    )
    .assert_success();

    let mut mcp = repo.spawn_mcp_stdio(None).expect("spawn mcp stdio");
    mcp_handshake(&mut mcp);
    let tools = mcp.tools_list().expect("tools/list");
    let schema = find_tool_schema(&tools, "loom_preflight_symbols")
        .expect("loom_preflight_symbols listed in tools/list");

    let clean_res = mcp
        .tools_call(
            "loom_preflight_symbols",
            build_preflight_args(&schema, &["clean_fn"], None),
        )
        .expect("preflight clean_fn");
    let clean_txt = mcp_tool_text(&clean_res);
    assert!(
        !clean_txt.contains("overflow in dirty_fn"),
        "clean_fn must not receive warning from dirty_fn, got:\n{clean_txt}"
    );

    let dirty_res = mcp
        .tools_call(
            "loom_preflight_symbols",
            build_preflight_args(&schema, &["dirty_fn"], None),
        )
        .expect("preflight dirty_fn");
    let dirty_txt = mcp_tool_text(&dirty_res);
    assert!(
        dirty_txt.contains("overflow in dirty_fn"),
        "dirty_fn must receive recorded warning, got:\n{dirty_txt}"
    );
}

#[test]
fn tier1_f21_multiple_failure_kinds_supported() {
    let repo = init_loom_repo_with_src(&[(
        "src/lib.rs",
        "pub fn sym_a() -> i32 { 1 }\n\npub fn sym_b() -> i32 { 2 }\n",
    )]);

    let help_out = repo.loom(&["drop", "--help"]);
    help_out.assert_success();
    let help_text = format!("{}\n{}", help_out.stdout_str(), help_out.stderr_str());
    let extra_kind = extract_extra_failure_kind(&help_text);

    let vp_default = spawn_vp(&repo, "vp-default", "default kind test");
    fs::write(
        vp_default.join("src/lib.rs"),
        "pub fn sym_a() -> i32 { -10 }\n\npub fn sym_b() -> i32 { 2 }\n",
    )
    .expect("edit sym_a in vp_default");
    repo.loom_in(&vp_default, &["drop", "--reason", "default kind lesson"])
        .assert_success();

    let vp_deadlock = spawn_vp(&repo, "vp-deadlock", "deadlock kind test");
    fs::write(
        vp_deadlock.join("src/lib.rs"),
        "pub fn sym_a() -> i32 { 1 }\n\npub fn sym_b() -> i32 { -20 }\n",
    )
    .expect("edit sym_b in vp_deadlock");
    repo.loom_in(
        &vp_deadlock,
        &[
            "drop",
            "--reason",
            "deadlock kind lesson",
            "--kind",
            "Deadlock",
        ],
    )
    .assert_success();

    if let Some(ref k2) = extra_kind {
        let vp_extra = spawn_vp(&repo, "vp-extra", "extra kind test");
        fs::write(
            vp_extra.join("src/lib.rs"),
            "pub fn sym_a() -> i32 { -30 }\n\npub fn sym_b() -> i32 { 2 }\n",
        )
        .expect("edit sym_a in vp_extra");
        repo.loom_in(
            &vp_extra,
            &["drop", "--reason", "second explicit kind", "--kind", k2],
        )
        .assert_success();
    }

    let mut mcp = repo.spawn_mcp_stdio(None).expect("spawn mcp stdio");
    mcp_handshake(&mut mcp);
    let tools = mcp.tools_list().expect("tools/list");
    let schema = find_tool_schema(&tools, "loom_preflight_symbols")
        .expect("loom_preflight_symbols listed in tools/list");

    let res = mcp
        .tools_call(
            "loom_preflight_symbols",
            build_preflight_args(&schema, &["sym_a", "sym_b"], None),
        )
        .expect("call loom_preflight_symbols");
    let txt = mcp_tool_text(&res);
    assert!(
        txt.contains("default kind lesson")
            && txt.contains("deadlock kind lesson")
            && txt.contains("Deadlock"),
        "expected default and Deadlock failure kinds in preflight, got:\n{txt}"
    );
    if extra_kind.is_some() {
        assert!(
            txt.contains("second explicit kind"),
            "expected additional --kind failure lesson in preflight, got:\n{txt}"
        );
    }
}

// ============================================================================
// Feature F22: gix Bidirectional Git Push & Pull (5 Tests)
// ============================================================================

#[test]
fn tier1_f22_git_push_creates_trailers_on_bare_remote() {
    let (repo, bare, _bare_str) =
        init_colocated_git_with_bare(&[("src/lib.rs", "pub fn base() -> i32 { 1 }\n")]);

    let vp1 = spawn_vp(&repo, "vp1", "feature alpha");
    fs::write(vp1.join("src/lib.rs"), "pub fn base() -> i32 { 10 }\n")
        .expect("edit base in vp1");

    let vcmd = portable_pass_cmd("10", r"src\lib.rs", "src/lib.rs");
    repo.loom_in(&vp1, &["seal", "-v", &vcmd, "-m", "implement feature alpha"])
        .assert_success();

    repo.loom(&["git", "push", "origin", "main"])
        .assert_success();

    let log_out = repo.git_in(&bare, &["log", "-n", "1", "--format=%B", "main"]);
    log_out.assert_success();
    let commit_msg = normalize_nl(&log_out.stdout_str());
    assert!(
        commit_msg.contains("Loom-Capsule-Id:")
            && commit_msg.contains("Loom-Agent:")
            && commit_msg.contains("Loom-Verified:"),
        "expected Loom trailers on pushed commit, got:\n{commit_msg}"
    );

    let show_out = repo.git_in(&bare, &["show", "main:src/lib.rs"]);
    show_out.assert_success();
    assert!(
        show_out.stdout_str().contains("10"),
        "expected bare remote main:src/lib.rs to contain 10, got:\n{}",
        show_out.stdout_str()
    );
}

#[test]
fn tier1_f22_git_push_writes_refs_notes_loom() {
    let (repo, bare, _bare_str) =
        init_colocated_git_with_bare(&[("src/lib.rs", "pub fn note_fn() -> i32 { 1 }\n")]);

    let vp1 = spawn_vp(&repo, "vp1", "capsule note");
    fs::write(vp1.join("src/lib.rs"), "pub fn note_fn() -> i32 { 2 }\n")
        .expect("edit note_fn in vp1");
    repo.loom_in(&vp1, &["seal", "-m", "capsule for git notes"])
        .assert_success();

    repo.loom(&["git", "push", "origin", "main"])
        .assert_success();

    let refs_out = repo.git_in(&bare, &["show-ref"]);
    refs_out.assert_success();
    assert!(
        refs_out.stdout_str().contains("refs/notes/loom"),
        "expected refs/notes/loom on bare remote, got:\n{}",
        refs_out.stdout_str()
    );

    let list_out = repo.git_in(&bare, &["notes", "--ref=loom", "list"]);
    list_out.assert_success();
    let list_str = normalize_nl(&list_out.stdout_str());
    let target_sha = list_str
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .next()
        .expect("non-empty refs/notes/loom list");

    let note_out = repo.git_in(&bare, &["notes", "--ref=loom", "show", target_sha]);
    note_out.assert_success();
    let note_body = normalize_nl(&note_out.stdout_str());
    assert!(
        !note_body.trim().is_empty()
            && (note_body.contains("capsule for git notes")
                || note_body.to_ascii_lowercase().contains("capsule")
                || note_body.contains("note_fn")),
        "expected capsule metadata in refs/notes/loom note, got:\n{note_body}"
    );
}

#[test]
fn tier1_f22_git_pull_absorbs_external_git_commit() {
    let (repo, _bare, bare_str) =
        init_colocated_git_with_bare(&[("src/lib.rs", "pub fn init_fn() -> i32 { 1 }\n")]);

    let ext = TestRepo::new().expect("create external git contributor");
    ext.git(&["clone", "-b", "main", &bare_str, "."])
        .assert_success();
    ext.git(&["config", "user.name", "External Contributor"])
        .assert_success();
    ext.git(&["config", "user.email", "ext@example.com"])
        .assert_success();
    ext.git(&["config", "core.autocrlf", "false"])
        .assert_success();
    ext.write_file("src/external.rs", "pub fn ext_added() -> i32 { 777 }\n")
        .expect("write external.rs");
    ext.git(&["add", "src/external.rs"]).assert_success();
    ext.git(&["commit", "-m", "ext: add external module"])
        .assert_success();
    ext.git(&["push", "origin", "main"]).assert_success();

    repo.loom(&["git", "pull", "origin", "main"])
        .assert_success();

    assert!(
        repo.exists("src/external.rs"),
        "expected pulled src/external.rs in trunk"
    );
    let ext_src = repo
        .read_file("src/external.rs")
        .expect("read pulled src/external.rs");
    assert!(
        ext_src.contains("ext_added"),
        "expected ext_added in pulled file, got:\n{ext_src}"
    );

    let git_log = repo.git(&["log", "--oneline"]);
    git_log.assert_success();
    assert!(
        git_log.stdout_str().contains("ext: add external module"),
        "expected external commit in git log, got:\n{}",
        git_log.stdout_str()
    );

    let loom_log = repo.loom(&["log"]);
    loom_log.assert_success();
    assert!(
        loom_log.stdout_str().contains("ext: add external module"),
        "expected absorbed external commit in `loom log`, got:\n{}",
        loom_log.stdout_str()
    );
}

#[test]
fn tier1_f22_git_pull_restores_negative_edge_notes() {
    let (repo_a, bare, bare_str) = init_colocated_git_with_bare(&[
        ("src/lib.rs", "pub fn risky() -> i32 { 1 }\n"),
        ("src/clean.rs", "pub fn ok_fn() -> i32 { 1 }\n"),
    ]);

    let repo_b = TestRepo::new().expect("create peer repo_b");
    repo_b
        .git(&["clone", "-b", "main", &bare_str, "."])
        .assert_success();
    repo_b
        .git(&["config", "user.name", "Loom Peer B"])
        .assert_success();
    repo_b
        .git(&["config", "user.email", "peerb@example.com"])
        .assert_success();
    repo_b
        .git(&["config", "core.autocrlf", "false"])
        .assert_success();
    repo_b
        .loom(&["init", "--colocate-git"])
        .assert_success();

    // Record NegativeEdge in repo_a and seal a benign capsule so a new commit carries notes
    let vp_bad = spawn_vp(&repo_a, "vp-bad", "risky deadend");
    fs::write(vp_bad.join("src/lib.rs"), "pub fn risky() -> i32 { -99 }\n")
        .expect("edit risky in vp_bad");
    repo_a
        .loom_in(
            &vp_bad,
            &[
                "drop",
                "--reason",
                "causes stack overflow on edge input",
                "--kind",
                "Deadlock",
            ],
        )
        .assert_success();

    let vp_ok = spawn_vp(&repo_a, "vp-ok", "benign commit for push");
    fs::write(vp_ok.join("src/clean.rs"), "pub fn ok_fn() -> i32 { 2 }\n")
        .expect("edit clean.rs in vp_ok");
    repo_a
        .loom_in(&vp_ok, &["seal", "-m", "benign seal to carry notes"])
        .assert_success();

    repo_a
        .loom(&["git", "push", "origin", "main"])
        .assert_success();

    let notes_list = repo_a.git_in(&bare, &["notes", "--ref=loom", "list"]);
    notes_list.assert_success();
    let mut bare_has_negative_note = false;
    for target_sha in notes_list
        .stdout_str()
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
    {
        let note = repo_a.git_in(&bare, &["notes", "--ref=loom", "show", target_sha]);
        note.assert_success();
        if note
            .stdout_str()
            .contains("causes stack overflow on edge input")
        {
            bare_has_negative_note = true;
            break;
        }
    }
    assert!(
        bare_has_negative_note,
        "expected bare refs/notes/loom to carry NegativeEdge lesson"
    );

    repo_b
        .loom(&["git", "pull", "origin", "main"])
        .assert_success();

    let mut mcp = repo_b.spawn_mcp_stdio(None).expect("spawn mcp in repo_b");
    mcp_handshake(&mut mcp);
    let tools = mcp.tools_list().expect("tools/list in repo_b");
    let schema = find_tool_schema(&tools, "loom_preflight_symbols")
        .expect("loom_preflight_symbols listed in repo_b");
    let preflight = mcp
        .tools_call(
            "loom_preflight_symbols",
            build_preflight_args(&schema, &["risky"], None),
        )
        .expect("preflight risky in repo_b");
    let txt = mcp_tool_text(&preflight);
    assert!(
        txt.contains("causes stack overflow on edge input") && txt.contains("Deadlock"),
        "expected NegativeEdge restored into repo_b preflight, got:\n{txt}"
    );
}

#[test]
fn tier1_f22_git_pull_rebases_active_disjoint_viewport() {
    let (repo, _bare, bare_str) = init_colocated_git_with_bare(&[
        ("src/alpha.rs", "pub fn alpha() -> i32 { 1 }\n"),
        ("src/beta.rs", "pub fn beta() -> i32 { 2 }\n"),
    ]);

    let vp_beta = spawn_vp(&repo, "vp-beta", "edit beta only");
    fs::write(vp_beta.join("src/beta.rs"), "pub fn beta() -> i32 { 200 }\n")
        .expect("edit beta.rs in active vp_beta");

    let ext = TestRepo::new().expect("create external contributor");
    ext.git(&["clone", "-b", "main", &bare_str, "."])
        .assert_success();
    ext.git(&["config", "user.name", "Upstream Author"])
        .assert_success();
    ext.git(&["config", "user.email", "upstream@example.com"])
        .assert_success();
    ext.git(&["config", "core.autocrlf", "false"])
        .assert_success();
    ext.write_file(
        "src/alpha.rs",
        "/// updated upstream\npub fn alpha() -> i32 { 100 }\n",
    )
    .expect("update alpha.rs upstream");
    ext.git(&["add", "src/alpha.rs"]).assert_success();
    ext.git(&["commit", "-m", "upstream: update alpha"])
        .assert_success();
    ext.git(&["push", "origin", "main"]).assert_success();

    repo.loom(&["git", "pull", "origin", "main"])
        .assert_success();

    let trunk_alpha = repo.read_file("src/alpha.rs").expect("read trunk alpha.rs");
    assert!(
        trunk_alpha.contains("100"),
        "expected trunk alpha.rs to contain 100, got:\n{trunk_alpha}"
    );

    // Background active viewport rebase may complete shortly after `loom git pull` returns
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut vp_alpha = String::new();
    loop {
        if let Ok(content) = fs::read_to_string(vp_beta.join("src/alpha.rs")) {
            vp_alpha = content;
            if vp_alpha.contains("100") {
                break;
            }
        }
        let now = Instant::now();
        if now >= deadline {
            panic!(
                "expected active viewport vp-beta src/alpha.rs to be rebased with 100, got:\n{vp_alpha}"
            );
        }
        std::thread::sleep(Duration::from_millis(25).min(deadline - now));
    }

    let vp_beta_src =
        fs::read_to_string(vp_beta.join("src/beta.rs")).expect("read vp-beta src/beta.rs");
    assert!(
        vp_beta_src.contains("200"),
        "expected uncommitted beta.rs edits in vp-beta to be preserved, got:\n{vp_beta_src}"
    );

    repo.loom_in(&vp_beta, &["seal", "-m", "seal rebased beta"])
        .assert_success();

    let final_alpha = repo.read_file("src/alpha.rs").expect("read final alpha.rs");
    let final_beta = repo.read_file("src/beta.rs").expect("read final beta.rs");
    assert!(
        final_alpha.contains("100") && final_beta.contains("200"),
        "expected final trunk to contain both alpha=100 and beta=200; alpha:\n{final_alpha}\nbeta:\n{final_beta}"
    );
}
