use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use e2e_suite::{CommandOutput, TestRepo};

const RADAR_TIMEOUT: Duration = Duration::from_secs(5);
const LARGE_RADAR_TIMEOUT: Duration = Duration::from_secs(10);
const STATUS_DLL_NOT_FOUND_EXIT: i32 = -1073741515; // 0xC0000135

fn normalize_token(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, '_' | '-' | ' ' | '\t' | '\r' | '\n'))
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn is_negated_phrase(s: &str) -> bool {
    let trimmed = s.trim().trim_end_matches(['.', '!']).trim();
    if trimmed.is_empty() {
        return false;
    }
    let norm = normalize_token(trimmed);
    matches!(
        norm.as_str(),
        "noerror"
            | "noerrors"
            | "noconflict"
            | "noconflicts"
            | "0conflict"
            | "0conflicts"
            | "0error"
            | "0errors"
            | "nosyntaxerror"
            | "nosyntaxerrors"
            | "0syntaxerror"
            | "0syntaxerrors"
            | "nopendingsyntax"
            | "nopendingsyntaxerror"
            | "nopendingsyntaxerrors"
            | "conflictfree"
            | "conflictsfree"
            | "errorfree"
            | "errorsfree"
            | "withoutconflict"
            | "withoutconflicts"
            | "withouterror"
            | "withouterrors"
            | "zeroconflict"
            | "zeroconflicts"
            | "zeroerror"
            | "zeroerrors"
            | "noactiveconflict"
            | "noactiveconflicts"
            | "0activeconflict"
            | "0activeconflicts"
            | "0conflictsfound"
            | "0conflictfound"
            | "noconflictsfound"
            | "noconflictfound"
            | "0errorsfound"
            | "0errorfound"
            | "noerrorsfound"
            | "noerrorfound"
            | "nosyntaxerrorsfound"
            | "nosyntaxerrorfound"
            | "conflictcheckok"
            | "conflictscheckok"
            | "errorcheckok"
            | "syntaxcheckok"
            | "noconflictdetected"
            | "noconflictsdetected"
            | "noerrordetected"
            | "noerrorsdetected"
    )
}

fn str_has_norm_token(haystack: &str, needle_norm: &str) -> bool {
    if needle_norm.is_empty() || is_negated_phrase(haystack) {
        return false;
    }
    if normalize_token(haystack) == needle_norm {
        return true;
    }
    haystack
        .split(|c: char| matches!(c, ',' | ';' | ':' | '(' | ')' | '[' | ']' | '{' | '}'))
        .map(str::trim)
        .filter(|clause| !clause.is_empty() && !is_negated_phrase(clause))
        .any(|clause| {
            clause
                .split_whitespace()
                .filter(|seg| !seg.is_empty())
                .any(|seg| normalize_token(seg) == needle_norm)
        })
}

fn is_metadata_key(k_norm: &str) -> bool {
    matches!(
        k_norm,
        "intent"
            | "file"
            | "path"
            | "filepath"
            | "viewport"
            | "viewportname"
            | "viewports"
            | "activeviewports"
            | "name"
            | "id"
            | "capsuleid"
            | "hash"
            | "message"
            | "summary"
            | "detail"
            | "details"
            | "description"
            | "reason"
            | "note"
            | "info"
    )
}

fn is_active_json_value(v: &Value) -> bool {
    match v {
        Value::Null => false,
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
                && !matches!(
                    trimmed.to_ascii_lowercase().as_str(),
                    "false" | "none" | "null" | "0" | "no"
                )
                && !trimmed.parse::<f64>().is_ok_and(|x| x <= 0.0)
                && !is_negated_phrase(trimmed)
        }
        Value::Array(arr) => !arr.is_empty() && arr.iter().any(is_active_json_value),
        Value::Object(map) => !map.is_empty() && map.values().any(is_active_json_value),
    }
}

fn json_metadata_container_has_norm_token(
    k_norm: &str,
    val: &Value,
    needle_norm: &str,
) -> bool {
    match val {
        Value::Object(map) => {
            if matches!(k_norm, "viewports" | "activeviewports") {
                map.values().any(|entry| match entry {
                    Value::Object(_) | Value::Array(_) | Value::String(_) => {
                        json_has_norm_token_inner(entry, needle_norm)
                    }
                    _ => false,
                })
            } else {
                json_has_norm_token_inner(val, needle_norm)
            }
        }
        Value::Array(items) => items.iter().any(|item| match item {
            Value::Object(_) => json_has_norm_token_inner(item, needle_norm),
            Value::Array(_) => json_metadata_container_has_norm_token(k_norm, item, needle_norm),
            _ => false,
        }),
        _ => false,
    }
}

fn json_has_norm_token_inner(val: &Value, needle_norm: &str) -> bool {
    match val {
        Value::String(s) => str_has_norm_token(s, needle_norm),
        Value::Array(items) => items
            .iter()
            .any(|item| json_has_norm_token_inner(item, needle_norm)),
        Value::Object(map) => map.iter().any(|(k, v)| {
            let k_norm = normalize_token(k);
            if is_metadata_key(&k_norm) {
                if matches!(
                    k_norm.as_str(),
                    "viewport" | "viewports" | "activeviewports" | "detail" | "details" | "info"
                ) && (v.is_object() || v.is_array())
                {
                    return json_metadata_container_has_norm_token(&k_norm, v, needle_norm);
                }
                return false;
            }
            (k_norm == needle_norm && is_active_json_value(v))
                || json_has_norm_token_inner(v, needle_norm)
        }),
        _ => false,
    }
}

fn json_has_norm_token(val: &Value, token: &str) -> bool {
    let needle_norm = normalize_token(token);
    json_has_norm_token_inner(val, &needle_norm)
}

fn radar_has_pending_syntax(val: &Value) -> bool {
    json_has_norm_token(val, "pendingsyntax")
        || json_has_norm_token(val, "pendingsyntaxfiles")
        || json_has_norm_token(val, "syntaxerrors")
}

fn radar_has_active_conflict(val: &Value) -> bool {
    json_has_norm_token(val, "red")
        || json_has_norm_token(val, "yellow")
        || json_has_norm_token(val, "structuralconflict")
        || json_has_norm_token(val, "semanticconflict")
        || json_has_norm_token(val, "conflict")
        || json_has_norm_token(val, "conflicts")
        || json_has_norm_token(val, "conflictcount")
        || json_has_norm_token(val, "hasconflict")
        || json_has_norm_token(val, "shadowconflicts")
}

fn assert_no_windows_dll_error_or_panic(out: &CommandOutput) {
    assert_ne!(
        out.exit_code(),
        Some(STATUS_DLL_NOT_FOUND_EXIT),
        "loom exited with STATUS_DLL_NOT_FOUND (0xC0000135): {}",
        out.stderr_str()
    );
    let combined = format!("{}\n{}", out.stdout_str(), out.stderr_str());
    assert!(
        !combined.contains("panicked at"),
        "loom panicked:\n{combined}"
    );
}

#[track_caller]
fn assert_failure_with_any_keyword(out: &CommandOutput, keywords: &[&str]) {
    assert_no_windows_dll_error_or_panic(out);
    out.assert_failure();
    let combined = format!("{}\n{}", out.stdout_str(), out.stderr_str()).to_lowercase();
    assert!(
        !combined.trim().is_empty(),
        "expected non-empty diagnostic output on failure (args={:?})",
        out.args
    );
    assert!(
        keywords.iter().any(|kw| combined.contains(&kw.to_lowercase())),
        "expected failure output to contain one of {:?}, got:\nstdout:\n{}\nstderr:\n{}",
        keywords,
        out.stdout_str(),
        out.stderr_str()
    );
}

#[track_caller]
fn assert_no_conflict_markers(text: &str) {
    assert!(
        !text.contains("<<<<<<<") && !text.contains("=======") && !text.contains(">>>>>>>"),
        "trunk file contains textual conflict markers:\n{text}"
    );
}

fn write_vp_bytes(vp_dir: &Path, rel: &str, bytes: &[u8]) {
    let target = vp_dir.join(rel);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).expect("create parent dirs in viewport");
    }
    fs::write(&target, bytes).expect("write viewport file");
}

fn write_vp(vp_dir: &Path, rel: &str, content: &str) {
    write_vp_bytes(vp_dir, rel, content.as_bytes());
}

fn init_and_spawn_two(repo: &TestRepo, intent1: &str, intent2: &str) -> (PathBuf, PathBuf) {
    let init_out = repo.loom(&["init"]);
    assert_no_windows_dll_error_or_panic(&init_out);
    init_out.assert_success();

    let s1 = repo.loom(&["spawn", "-n", "vp1", "-m", intent1]);
    assert_no_windows_dll_error_or_panic(&s1);
    s1.assert_success();

    let s2 = repo.loom(&["spawn", "-n", "vp2", "-m", intent2]);
    assert_no_windows_dll_error_or_panic(&s2);
    s2.assert_success();

    (repo.viewport_dir("vp1"), repo.viewport_dir("vp2"))
}

fn seal_vp(repo: &TestRepo, vp_name: &str, msg: &str) -> CommandOutput {
    let vp_dir = repo.viewport_dir(vp_name);
    let out = repo.loom_in(&vp_dir, &["seal", "-m", msg]);
    assert_no_windows_dll_error_or_panic(&out);
    out
}

fn radar_json_snapshot(repo: &TestRepo) -> Value {
    let out = repo.loom(&["radar", "--json"]);
    assert_no_windows_dll_error_or_panic(&out);
    out.assert_success();
    out.stdout_json().expect("loom radar --json must emit valid JSON")
}

fn wait_radar_green(repo: &TestRepo, vp_dir: &Path, timeout: Duration) -> Value {
    repo.wait_for_radar_status(vp_dir, timeout, |v| {
        json_has_norm_token(v, "green")
            && !radar_has_active_conflict(v)
            && !radar_has_pending_syntax(v)
            && !json_has_norm_token(v, "error")
    })
    .expect("wait for GREEN RADAR_STATUS.json")
}

// ============================================================================
// F11: 4-Language Tree-sitter CST & Leading Trivia (5 Tests)
// ============================================================================

#[test]
fn tier2_f11_empty_source_files_all_4_langs() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.write_file("src/empty.rs", "").unwrap();
    repo.write_file("src/empty.ts", "").unwrap();
    repo.write_file("src/empty.tsx", "").unwrap();
    repo.write_file("pkg/empty.go", "").unwrap();
    repo.write_file("py/empty.py", "").unwrap();
    repo.write_file("src/untouched.rs", "").unwrap();

    let (vp1, vp2) = init_and_spawn_two(
        &repo,
        "populate empty rs, ts, and tsx",
        "populate empty go and py",
    );

    // Initial 0-byte source files must not panic or mark PENDING_SYNTAX/RED
    for vp in [&vp1, &vp2] {
        repo.wait_for_radar_status(vp, RADAR_TIMEOUT, |v| {
            !radar_has_active_conflict(v) && !radar_has_pending_syntax(v)
        })
        .expect("0-byte initial files should not error in radar");
    }

    write_vp(
        &vp1,
        "src/empty.rs",
        "/// Rust doc\npub fn rs_added() -> i32 {\n    1\n}\n",
    );
    write_vp(
        &vp1,
        "src/empty.ts",
        "/** TS doc */\nexport function tsAdded(): number {\n    return 2;\n}\n",
    );
    write_vp(
        &vp1,
        "src/empty.tsx",
        "export function TsxComp(): string {\n    return \"ok\";\n}\n",
    );

    write_vp(
        &vp2,
        "pkg/empty.go",
        "package pkg\n\n// Go doc\nfunc GoAdded() int {\n    return 3\n}\n",
    );
    write_vp(
        &vp2,
        "py/empty.py",
        "# Py doc\ndef py_added() -> int:\n    return 4\n",
    );

    wait_radar_green(&repo, &vp1, RADAR_TIMEOUT);
    wait_radar_green(&repo, &vp2, RADAR_TIMEOUT);
    let radar = radar_json_snapshot(&repo);
    assert!(!json_has_norm_token(&radar, "red"));

    seal_vp(&repo, "vp1", "seal empty rs/ts/tsx").assert_success();
    seal_vp(&repo, "vp2", "seal empty go/py").assert_success();
    repo.loom(&["status"]).assert_success();

    let rs = repo.read_file("src/empty.rs").unwrap();
    let ts = repo.read_file("src/empty.ts").unwrap();
    let tsx = repo.read_file("src/empty.tsx").unwrap();
    let go = repo.read_file("pkg/empty.go").unwrap();
    let py = repo.read_file("py/empty.py").unwrap();
    let untouched = repo.read_file("src/untouched.rs").unwrap();

    assert!(rs.contains("rs_added"));
    assert!(ts.contains("tsAdded"));
    assert!(tsx.contains("TsxComp"));
    assert!(go.contains("GoAdded"));
    assert!(py.contains("py_added"));
    assert_eq!(untouched, "", "untouched 0-byte file must stay empty");

    for content in [&rs, &ts, &tsx, &go, &py, &untouched] {
        assert_no_conflict_markers(content);
    }
}

#[test]
fn tier2_f11_detached_comment_not_stolen_as_trivia() {
    let repo = TestRepo::new().expect("TestRepo::new");
    let base_rs = concat!(
        "// DETACHED_SECTION_BANNER: must survive target removal\n",
        "\n",
        "/// Attached doc comment for target_fn\n",
        "#[inline]\n",
        "pub fn target_fn() -> i32 {\n",
        "    10\n",
        "}\n",
        "\n",
        "pub fn keep_fn() -> i32 {\n",
        "    20\n",
        "}\n",
    );
    repo.write_file("src/lib.rs", base_rs).unwrap();

    let (vp1, vp2) = init_and_spawn_two(
        &repo,
        "insert new_top under detached banner",
        "remove target_fn and update keep_fn",
    );

    write_vp(
        &vp1,
        "src/lib.rs",
        concat!(
            "// DETACHED_SECTION_BANNER: must survive target removal\n",
            "\n",
            "/// Top doc\n",
            "pub fn new_top() -> i32 {\n",
            "    1\n",
            "}\n",
            "\n",
            "/// Attached doc comment for target_fn\n",
            "#[inline]\n",
            "pub fn target_fn() -> i32 {\n",
            "    10\n",
            "}\n",
            "\n",
            "pub fn keep_fn() -> i32 {\n",
            "    20\n",
            "}\n",
        ),
    );
    write_vp(
        &vp2,
        "src/lib.rs",
        concat!(
            "// DETACHED_SECTION_BANNER: must survive target removal\n",
            "\n",
            "pub fn keep_fn() -> i32 {\n",
            "    99\n",
            "}\n",
        ),
    );

    wait_radar_green(&repo, &vp1, RADAR_TIMEOUT);
    wait_radar_green(&repo, &vp2, RADAR_TIMEOUT);
    let radar = radar_json_snapshot(&repo);
    assert!(!json_has_norm_token(&radar, "red"));

    seal_vp(&repo, "vp1", "add new_top").assert_success();
    seal_vp(&repo, "vp2", "remove target_fn and update keep_fn").assert_success();

    let trunk = repo.read_file("src/lib.rs").unwrap();
    assert_no_conflict_markers(&trunk);
    assert!(
        trunk.contains("DETACHED_SECTION_BANNER: must survive target removal"),
        "detached banner comment was stolen/removed:\n{trunk}"
    );
    assert_eq!(
        trunk.matches("DETACHED_SECTION_BANNER").count(),
        1,
        "detached banner comment must appear exactly once (not duplicated):\n{trunk}"
    );
    assert!(
        trunk.contains("/// Top doc"),
        "new_top leading doc comment missing:\n{trunk}"
    );
    assert!(trunk.contains("pub fn new_top()"), "new_top missing:\n{trunk}");
    assert!(
        trunk.contains("pub fn keep_fn()") && trunk.contains("99"),
        "keep_fn update missing:\n{trunk}"
    );
    let banner_pos = trunk
        .find("DETACHED_SECTION_BANNER")
        .expect("banner position");
    let new_top_pos = trunk.find("pub fn new_top()").expect("new_top position");
    let keep_fn_pos = trunk.find("pub fn keep_fn()").expect("keep_fn position");
    assert!(
        banner_pos < new_top_pos && new_top_pos < keep_fn_pos,
        "expected DETACHED_SECTION_BANNER < new_top < keep_fn ordering:\n{trunk}"
    );
    assert!(
        !trunk.contains("target_fn")
            && !trunk.contains("Attached doc comment")
            && !trunk.contains("#[inline]"),
        "target_fn or its contiguous trivia leaked:\n{trunk}"
    );
}

#[test]
fn tier2_f11_syntax_error_at_merge_time_rejected() {
    let repo = TestRepo::new().expect("TestRepo::new");
    let base_rs = "pub fn valid_fn() -> i32 {\n    1\n}\n";
    repo.write_file("src/lib.rs", base_rs).unwrap();

    let (vp1, vp2) = init_and_spawn_two(
        &repo,
        "write incomplete rust fn",
        "add clean helper fn",
    );

    write_vp(&vp1, "src/lib.rs", "pub fn valid_fn( -> i32 { let x = ;;; }\n");
    write_vp(
        &vp2,
        "src/lib.rs",
        "pub fn valid_fn() -> i32 {\n    1\n}\n\npub fn clean_fn() -> i32 {\n    2\n}\n",
    );

    repo.wait_for_radar_status(&vp1, RADAR_TIMEOUT, |v| {
        radar_has_pending_syntax(v)
            || json_has_norm_token(v, "error")
            || radar_has_active_conflict(v)
    })
    .expect("radar should reflect broken syntax state");

    let seal1 = seal_vp(&repo, "vp1", "attempt sealing syntax error");
    assert_failure_with_any_keyword(
        &seal1,
        &["syntax", "parse", "error", "invalid", "pending", "cst", "malformed"],
    );

    let trunk_after_reject = repo.read_file("src/lib.rs").unwrap();
    assert_eq!(
        trunk_after_reject, base_rs,
        "trunk must remain untouched after rejected broken syntax seal"
    );
    assert_no_conflict_markers(&trunk_after_reject);

    repo.loom(&["status"]).assert_success();

    seal_vp(&repo, "vp2", "seal clean addition").assert_success();
    let trunk_after_vp2 = repo.read_file("src/lib.rs").unwrap();
    assert!(trunk_after_vp2.contains("clean_fn"));
    assert_no_conflict_markers(&trunk_after_vp2);
}

#[test]
fn tier2_f11_unsupported_extension_fallback() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.write_file(
        "notes/todo.txt",
        "line1_top\nline2_ctx\nline3_ctx\nline4_ctx\nline5_ctx\nline6_bot\n",
    )
    .unwrap();
    repo.write_file("config/app.toml", "[app]\nname = \"loom\"\nport = 8080\n")
        .unwrap();

    let (vp1, vp2) = init_and_spawn_two(
        &repo,
        "edit top of todo.txt and add sql",
        "edit bottom of todo.txt, edit toml, and add css",
    );

    write_vp(
        &vp1,
        "notes/todo.txt",
        "line1_top_vp1\nline2_ctx\nline3_ctx\nline4_ctx\nline5_ctx\nline6_bot\n",
    );
    write_vp(&vp1, "notes/query.sql", "SELECT 1;\n");

    write_vp(
        &vp2,
        "notes/todo.txt",
        "line1_top\nline2_ctx\nline3_ctx\nline4_ctx\nline5_ctx\nline6_bot_vp2\n",
    );
    write_vp(
        &vp2,
        "config/app.toml",
        "[app]\nname = \"loom\"\nport = 9090\n",
    );
    write_vp(&vp2, "notes/style.css", "body { margin: 0; }\n");

    wait_radar_green(&repo, &vp1, RADAR_TIMEOUT);
    wait_radar_green(&repo, &vp2, RADAR_TIMEOUT);
    let radar = radar_json_snapshot(&repo);
    assert!(!json_has_norm_token(&radar, "red"));

    seal_vp(&repo, "vp1", "seal txt top and sql").assert_success();
    seal_vp(&repo, "vp2", "seal txt bottom, toml, and css").assert_success();
    repo.loom(&["status"]).assert_success();

    let todo = repo.read_file("notes/todo.txt").unwrap();
    let app_toml = repo.read_file("config/app.toml").unwrap();
    let sql = repo.read_file("notes/query.sql").unwrap();
    let css = repo.read_file("notes/style.css").unwrap();

    assert!(
        todo.contains("line1_top_vp1") && todo.contains("line6_bot_vp2"),
        "disjoint fallback edits in todo.txt lost:\n{todo}"
    );
    assert!(app_toml.contains("port = 9090"));
    assert!(sql.contains("SELECT 1;"));
    assert!(css.contains("body { margin: 0; }"));

    for text in [&todo, &app_toml, &sql, &css] {
        assert_no_conflict_markers(text);
    }
}

#[test]
fn tier2_f11_unicode_identifiers_and_comments_4_langs() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.write_file(
        "src/lib.rs",
        "/// 한국어 문서 주석 🚀\npub fn 계산_함수() -> i32 {\n    1\n}\n\npub fn keep_rs() -> i32 {\n    2\n}\n",
    )
    .unwrap();
    repo.write_file(
        "src/app.ts",
        "/** 타입스크립트 주석 ✨ */\nexport function 계산TS(): number {\n    return 10;\n}\n\nexport function keepTs(): number {\n    return 20;\n}\n",
    )
    .unwrap();
    repo.write_file(
        "pkg/calc.go",
        "package pkg\n\n// 고 언어 주석 💡\nfunc 계산Go() int {\n    return 100\n}\n\nfunc KeepGo() int {\n    return 200\n}\n",
    )
    .unwrap();
    repo.write_file(
        "py/calc.py",
        "# 파이썬 주석 🐍\ndef 계산_파이썬() -> int:\n    return 1000\n\ndef keep_py() -> int:\n    return 2000\n",
    )
    .unwrap();

    let (vp1, vp2) = init_and_spawn_two(
        &repo,
        "unicode edits batch 1",
        "unicode edits batch 2",
    );

    write_vp(
        &vp1,
        "src/lib.rs",
        concat!(
            "/// 한국어 문서 주석 🚀\n",
            "pub fn 계산_함수() -> i32 {\n",
            "    11\n",
            "}\n",
            "\n",
            "/// 추가 함수 🎯\n",
            "pub fn 보조_함수() -> i32 {\n",
            "    3\n",
            "}\n",
            "\n",
            "pub fn keep_rs() -> i32 {\n",
            "    2\n",
            "}\n",
        ),
    );
    write_vp(
        &vp1,
        "src/app.ts",
        "/** 타입스크립트 주석 ✨ */\nexport function 계산TS(): number {\n    return 11;\n}\n\nexport function keepTs(): number {\n    return 20;\n}\n",
    );
    write_vp(
        &vp1,
        "pkg/calc.go",
        "package pkg\n\n// 고 언어 주석 💡\nfunc 계산Go() int {\n    return 100\n}\n\nfunc KeepGo() int {\n    return 202\n}\n",
    );
    write_vp(
        &vp1,
        "py/calc.py",
        "# 파이썬 주석 🐍\ndef 계산_파이썬() -> int:\n    return 1000\n\ndef keep_py() -> int:\n    return 2002\n",
    );

    write_vp(
        &vp2,
        "src/lib.rs",
        "/// 한국어 문서 주석 🚀\npub fn 계산_함수() -> i32 {\n    1\n}\n\npub fn keep_rs() -> i32 {\n    22\n}\n",
    );
    write_vp(
        &vp2,
        "src/app.ts",
        "/** 타입스크립트 주석 ✨ */\nexport function 계산TS(): number {\n    return 10;\n}\n\nexport function keepTs(): number {\n    return 22;\n}\n",
    );
    write_vp(
        &vp2,
        "pkg/calc.go",
        "package pkg\n\n// 고 언어 주석 💡\nfunc 계산Go() int {\n    return 111\n}\n\nfunc KeepGo() int {\n    return 200\n}\n",
    );
    write_vp(
        &vp2,
        "py/calc.py",
        "# 파이썬 주석 🐍\ndef 계산_파이썬() -> int:\n    return 1111\n\ndef keep_py() -> int:\n    return 2000\n",
    );

    wait_radar_green(&repo, &vp1, RADAR_TIMEOUT);
    wait_radar_green(&repo, &vp2, RADAR_TIMEOUT);
    let radar = radar_json_snapshot(&repo);
    assert!(!json_has_norm_token(&radar, "red"));

    seal_vp(&repo, "vp1", "seal unicode vp1").assert_success();
    seal_vp(&repo, "vp2", "seal unicode vp2").assert_success();

    let rs = repo.read_file("src/lib.rs").unwrap();
    let ts = repo.read_file("src/app.ts").unwrap();
    let go = repo.read_file("pkg/calc.go").unwrap();
    let py = repo.read_file("py/calc.py").unwrap();

    assert!(rs.contains("한국어 문서 주석 🚀") && rs.contains("추가 함수 🎯"));
    assert!(rs.contains("계산_함수") && rs.contains("보조_함수"));
    assert!(rs.contains("11") && rs.contains("22") && rs.contains("3"));

    assert!(ts.contains("타입스크립트 주석 ✨") && ts.contains("계산TS"));
    assert!(ts.contains("11") && ts.contains("22"));

    assert!(go.contains("고 언어 주석 💡") && go.contains("계산Go"));
    assert!(go.contains("111") && go.contains("202"));

    assert!(py.contains("파이썬 주석 🐍") && py.contains("계산_파이썬"));
    assert!(py.contains("1111") && py.contains("2002"));

    for text in [&rs, &ts, &go, &py] {
        assert!(!text.contains('\u{FFFD}'), "UTF-8 replacement char found:\n{text}");
        assert_no_conflict_markers(text);
    }
}

// ============================================================================
// F12: Line-Independent NodeAnchor (5 Tests)
// ============================================================================

#[test]
fn tier2_f12_anchor_conflict_same_renamed_header() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.write_file(
        "src/lib.rs",
        "pub fn alpha(x: i32) -> i32 {\n    x + 1\n}\n\npub fn helper() -> i32 {\n    0\n}\n",
    )
    .unwrap();

    let (vp1, vp2) = init_and_spawn_two(
        &repo,
        "rename alpha to renamed(x: i32)",
        "rename alpha to renamed(x: i32, y: i32)",
    );

    write_vp(
        &vp1,
        "src/lib.rs",
        "pub fn renamed(x: i32) -> i32 {\n    x + 10\n}\n\npub fn helper() -> i32 {\n    0\n}\n",
    );
    write_vp(
        &vp2,
        "src/lib.rs",
        "pub fn renamed(x: i32, y: i32) -> i32 {\n    x + y\n}\n\npub fn helper() -> i32 {\n    0\n}\n",
    );

    repo.wait_for_radar_status(&vp1, RADAR_TIMEOUT, radar_has_active_conflict)
        .expect("radar should detect renamed header conflict");

    let radar = radar_json_snapshot(&repo);
    assert!(radar_has_active_conflict(&radar));

    seal_vp(&repo, "vp1", "seal renamed(x)").assert_success();
    let seal2 = seal_vp(&repo, "vp2", "attempt sealing conflicting renamed(x, y)");
    assert_failure_with_any_keyword(
        &seal2,
        &[
            "conflict",
            "anchor",
            "header",
            "signature",
            "duplicate",
            "missing",
            "merge",
            "removed",
            "deleted",
            "structural",
            "not found",
        ],
    );

    let trunk = repo.read_file("src/lib.rs").unwrap();
    assert_eq!(trunk.matches("fn renamed").count(), 1, "unexpected duplicate:\n{trunk}");
    assert!(trunk.contains("x + 10") && trunk.contains("helper"));
    assert!(!trunk.contains("x + y"));
    assert_no_conflict_markers(&trunk);
}

#[test]
fn tier2_f12_anchor_when_target_node_deleted_in_other_vp() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.write_file(
        "src/lib.rs",
        "pub fn stable_fn() -> i32 {\n    1\n}\n\npub fn target_fn(x: i32) -> i32 {\n    x * 2\n}\n",
    )
    .unwrap();

    let (vp1, vp2) = init_and_spawn_two(
        &repo,
        "delete target_fn",
        "edit target_fn body",
    );

    write_vp(&vp1, "src/lib.rs", "pub fn stable_fn() -> i32 {\n    1\n}\n");
    write_vp(
        &vp2,
        "src/lib.rs",
        "pub fn stable_fn() -> i32 {\n    1\n}\n\npub fn target_fn(x: i32) -> i32 {\n    x * 10\n}\n",
    );

    repo.wait_for_radar_status(&vp1, RADAR_TIMEOUT, radar_has_active_conflict)
        .expect("radar should detect delete vs edit conflict");

    let radar = radar_json_snapshot(&repo);
    assert!(radar_has_active_conflict(&radar));

    seal_vp(&repo, "vp1", "delete target_fn").assert_success();
    let seal2 = seal_vp(&repo, "vp2", "attempt editing deleted target_fn");
    assert_failure_with_any_keyword(
        &seal2,
        &["conflict", "delete", "removed", "missing", "anchor", "structural", "merge"],
    );

    let trunk = repo.read_file("src/lib.rs").unwrap();
    assert!(trunk.contains("stable_fn"));
    assert!(!trunk.contains("target_fn") && !trunk.contains("x * 10"));
    assert_no_conflict_markers(&trunk);
}

#[test]
fn tier2_f12_duplicate_symbol_names_same_scope() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.write_file(
        "src/overload.ts",
        concat!(
            "export function parse(x: string): number;\n",
            "export function parse(x: number): number;\n",
            "export function parse(x: string | number): number {\n",
            "    return typeof x === \"string\" ? x.length : x;\n",
            "}\n",
            "\n",
            "export function other(): number {\n",
            "    return 42;\n",
            "}\n",
        ),
    )
    .unwrap();
    repo.write_file(
        "pkg/dup.go",
        concat!(
            "package pkg\n",
            "\n",
            "var a = 0\n",
            "var b = 0\n",
            "\n",
            "func init() {\n",
            "    a = 1\n",
            "}\n",
            "\n",
            "func init() {\n",
            "    b = 2\n",
            "}\n",
        ),
    )
    .unwrap();

    // Part A: TypeScript function overloads (distinct decl_header_hash -> clean merge)
    let (vp1, vp2) = init_and_spawn_two(
        &repo,
        "add TS overload and edit other()",
        "edit TS parse() impl body",
    );

    write_vp(
        &vp1,
        "src/overload.ts",
        concat!(
            "export function parse(x: string): number;\n",
            "export function parse(x: number): number;\n",
            "export function parse(x: boolean): number;\n",
            "export function parse(x: string | number): number {\n",
            "    return typeof x === \"string\" ? x.length : x;\n",
            "}\n",
            "\n",
            "export function other(): number {\n",
            "    return 420;\n",
            "}\n",
        ),
    );
    write_vp(
        &vp2,
        "src/overload.ts",
        concat!(
            "export function parse(x: string): number;\n",
            "export function parse(x: number): number;\n",
            "export function parse(x: string | number): number {\n",
            "    return 999;\n",
            "}\n",
            "\n",
            "export function other(): number {\n",
            "    return 42;\n",
            "}\n",
        ),
    );

    wait_radar_green(&repo, &vp1, RADAR_TIMEOUT);
    wait_radar_green(&repo, &vp2, RADAR_TIMEOUT);
    seal_vp(&repo, "vp1", "seal TS overload addition").assert_success();
    seal_vp(&repo, "vp2", "seal TS overload impl body").assert_success();

    let trunk_ts = repo.read_file("src/overload.ts").unwrap();
    assert!(trunk_ts.contains("export function parse(x: boolean): number;"));
    assert!(trunk_ts.contains("return 999;"));
    assert!(trunk_ts.contains("return 420;"));
    assert_no_conflict_markers(&trunk_ts);

    // Part B: Go identical-header `func init()` (must either disambiguate cleanly OR conflict cleanly)
    repo.loom(&["spawn", "-n", "vp3", "-m", "edit 1st go init"]).assert_success();
    repo.loom(&["spawn", "-n", "vp4", "-m", "edit 2nd go init"]).assert_success();
    let vp3 = repo.viewport_dir("vp3");
    let vp4 = repo.viewport_dir("vp4");

    write_vp(
        &vp3,
        "pkg/dup.go",
        "package pkg\n\nvar a = 0\nvar b = 0\n\nfunc init() {\n    a = 10\n}\n\nfunc init() {\n    b = 2\n}\n",
    );
    write_vp(
        &vp4,
        "pkg/dup.go",
        "package pkg\n\nvar a = 0\nvar b = 0\n\nfunc init() {\n    a = 1\n}\n\nfunc init() {\n    b = 20\n}\n",
    );

    seal_vp(&repo, "vp3", "seal 1st go init").assert_success();
    let seal4 = seal_vp(&repo, "vp4", "seal 2nd go init");
    let trunk_go = repo.read_file("pkg/dup.go").unwrap();
    assert_no_conflict_markers(&trunk_go);

    if seal4.success() {
        assert_eq!(
            trunk_go.matches("func init()").count(),
            2,
            "if identical-header Go init() merged cleanly, expected exactly 2 func init() definitions:\n{trunk_go}"
        );
        assert!(
            trunk_go.contains("a = 10") && trunk_go.contains("b = 20"),
            "if identical-header Go init() merged cleanly, both edits must survive:\n{trunk_go}"
        );
    } else {
        assert_failure_with_any_keyword(
            &seal4,
            &["conflict", "duplicate", "anchor", "ambiguous", "merge", "init"],
        );
        assert!(
            trunk_go.contains("a = 10") && !trunk_go.contains("b = 20"),
            "rejected second seal must preserve first-sealed Go init():\n{trunk_go}"
        );
    }
    repo.loom(&["status"]).assert_success();
}

#[test]
fn tier2_f12_nested_closure_or_anonymous_functions() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.write_file(
        "src/lib.rs",
        concat!(
            "pub fn outer_a(nums: &[i32]) -> Vec<i32> {\n",
            "    nums.iter().map(|x| x + 1).collect()\n",
            "}\n",
            "\n",
            "pub fn outer_b(nums: &[i32]) -> Vec<i32> {\n",
            "    nums.iter().map(|x| x * 2).collect()\n",
            "}\n",
        ),
    )
    .unwrap();
    repo.write_file(
        "src/closures.ts",
        concat!(
            "export function mapA(xs: number[]): number[] {\n",
            "    return xs.map((x) => x + 1);\n",
            "}\n",
            "\n",
            "export function mapB(xs: number[]): number[] {\n",
            "    return xs.map((x) => x * 2);\n",
            "}\n",
        ),
    )
    .unwrap();

    let (vp1, vp2) = init_and_spawn_two(
        &repo,
        "update nested closures in outer_a and mapA",
        "shift top lines and update closures in outer_b and mapB",
    );

    write_vp(
        &vp1,
        "src/lib.rs",
        concat!(
            "pub fn outer_a(nums: &[i32]) -> Vec<i32> {\n",
            "    nums.iter().map(|x| { let f = |y: i32| y + 10; f(*x) }).collect()\n",
            "}\n",
            "\n",
            "pub fn outer_b(nums: &[i32]) -> Vec<i32> {\n",
            "    nums.iter().map(|x| x * 2).collect()\n",
            "}\n",
        ),
    );
    write_vp(
        &vp1,
        "src/closures.ts",
        concat!(
            "export function mapA(xs: number[]): number[] {\n",
            "    return xs.map((x) => x + 10);\n",
            "}\n",
            "\n",
            "export function mapB(xs: number[]): number[] {\n",
            "    return xs.map((x) => x * 2);\n",
            "}\n",
        ),
    );

    write_vp(
        &vp2,
        "src/lib.rs",
        concat!(
            "// shift line 1\n",
            "// shift line 2\n",
            "// shift line 3\n",
            "// shift line 4\n",
            "// shift line 5\n",
            "pub fn outer_a(nums: &[i32]) -> Vec<i32> {\n",
            "    nums.iter().map(|x| x + 1).collect()\n",
            "}\n",
            "\n",
            "pub fn outer_b(nums: &[i32]) -> Vec<i32> {\n",
            "    nums.iter().map(|x| x * 20).collect()\n",
            "}\n",
        ),
    );
    write_vp(
        &vp2,
        "src/closures.ts",
        concat!(
            "// shift ts 1\n",
            "// shift ts 2\n",
            "// shift ts 3\n",
            "// shift ts 4\n",
            "// shift ts 5\n",
            "export function mapA(xs: number[]): number[] {\n",
            "    return xs.map((x) => x + 1);\n",
            "}\n",
            "\n",
            "export function mapB(xs: number[]): number[] {\n",
            "    return xs.map((x) => x * 20);\n",
            "}\n",
        ),
    );

    wait_radar_green(&repo, &vp1, RADAR_TIMEOUT);
    wait_radar_green(&repo, &vp2, RADAR_TIMEOUT);
    let radar = radar_json_snapshot(&repo);
    assert!(!json_has_norm_token(&radar, "red"));

    seal_vp(&repo, "vp1", "seal outer_a/mapA closure edits").assert_success();
    seal_vp(&repo, "vp2", "seal shifted outer_b/mapB closure edits").assert_success();

    let rs = repo.read_file("src/lib.rs").unwrap();
    let ts = repo.read_file("src/closures.ts").unwrap();

    assert!(rs.contains("// shift line 5") && rs.contains("y + 10") && rs.contains("x * 20"));
    assert!(ts.contains("// shift ts 5") && ts.contains("x + 10") && ts.contains("x * 20"));
    assert_no_conflict_markers(&rs);
    assert_no_conflict_markers(&ts);
}

#[test]
fn tier2_f12_deeply_nested_module_namespace_fqn() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.write_file(
        "src/lib.rs",
        concat!(
            "pub mod org {\n",
            "    pub mod team {\n",
            "        pub mod alpha {\n",
            "            pub struct Worker;\n",
            "            impl Worker {\n",
            "                pub fn run() -> i32 { 1 }\n",
            "            }\n",
            "        }\n",
            "        pub mod beta {\n",
            "            pub struct Worker;\n",
            "            impl Worker {\n",
            "                pub fn run() -> i32 { 2 }\n",
            "            }\n",
            "        }\n",
            "    }\n",
            "}\n",
        ),
    )
    .unwrap();
    repo.write_file(
        "src/ns.ts",
        concat!(
            "export namespace Org {\n",
            "    export namespace Team {\n",
            "        export namespace Alpha {\n",
            "            export function run(): number { return 1; }\n",
            "        }\n",
            "        export namespace Beta {\n",
            "            export function run(): number { return 2; }\n",
            "        }\n",
            "    }\n",
            "}\n",
        ),
    )
    .unwrap();

    let (vp1, vp2) = init_and_spawn_two(
        &repo,
        "edit alpha::Worker::run and Org.Team.Alpha.run",
        "shift top lines and edit beta::Worker::run and Org.Team.Beta.run",
    );

    write_vp(
        &vp1,
        "src/lib.rs",
        concat!(
            "pub mod org {\n",
            "    pub mod team {\n",
            "        pub mod alpha {\n",
            "            pub struct Worker;\n",
            "            impl Worker {\n",
            "                pub fn run() -> i32 { 100 }\n",
            "            }\n",
            "        }\n",
            "        pub mod beta {\n",
            "            pub struct Worker;\n",
            "            impl Worker {\n",
            "                pub fn run() -> i32 { 2 }\n",
            "            }\n",
            "        }\n",
            "    }\n",
            "}\n",
        ),
    );
    write_vp(
        &vp1,
        "src/ns.ts",
        concat!(
            "export namespace Org {\n",
            "    export namespace Team {\n",
            "        export namespace Alpha {\n",
            "            export function run(): number { return 100; }\n",
            "        }\n",
            "        export namespace Beta {\n",
            "            export function run(): number { return 2; }\n",
            "        }\n",
            "    }\n",
            "}\n",
        ),
    );

    write_vp(
        &vp2,
        "src/lib.rs",
        concat!(
            "// rs top shift 1\n",
            "// rs top shift 2\n",
            "// rs top shift 3\n",
            "// rs top shift 4\n",
            "pub mod org {\n",
            "    pub mod team {\n",
            "        pub mod alpha {\n",
            "            pub struct Worker;\n",
            "            impl Worker {\n",
            "                pub fn run() -> i32 { 1 }\n",
            "            }\n",
            "        }\n",
            "        pub mod beta {\n",
            "            pub struct Worker;\n",
            "            impl Worker {\n",
            "                pub fn run() -> i32 { 200 }\n",
            "            }\n",
            "        }\n",
            "    }\n",
            "}\n",
        ),
    );
    write_vp(
        &vp2,
        "src/ns.ts",
        concat!(
            "// ts top shift 1\n",
            "// ts top shift 2\n",
            "// ts top shift 3\n",
            "// ts top shift 4\n",
            "export namespace Org {\n",
            "    export namespace Team {\n",
            "        export namespace Alpha {\n",
            "            export function run(): number { return 1; }\n",
            "        }\n",
            "        export namespace Beta {\n",
            "            export function run(): number { return 200; }\n",
            "        }\n",
            "    }\n",
            "}\n",
        ),
    );

    wait_radar_green(&repo, &vp1, RADAR_TIMEOUT);
    wait_radar_green(&repo, &vp2, RADAR_TIMEOUT);
    let radar = radar_json_snapshot(&repo);
    assert!(!json_has_norm_token(&radar, "red"));

    seal_vp(&repo, "vp1", "seal nested alpha edits").assert_success();
    seal_vp(&repo, "vp2", "seal shifted nested beta edits").assert_success();

    let rs = repo.read_file("src/lib.rs").unwrap();
    let ts = repo.read_file("src/ns.ts").unwrap();

    assert!(rs.contains("// rs top shift 4") && rs.contains("100") && rs.contains("200"));
    assert!(ts.contains("// ts top shift 4") && ts.contains("return 100;") && ts.contains("return 200;"));
    assert_no_conflict_markers(&rs);
    assert_no_conflict_markers(&ts);
}

// ============================================================================
// F13: SemanticOp, AstPatch & Commutative Merge (5 Tests)
// ============================================================================

#[test]
fn tier2_f13_structural_conflict_same_body_edit() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.write_file("src/lib.rs", "pub fn compute(x: i32) -> i32 {\n    x + 1\n}\n")
        .unwrap();

    let (vp1, vp2) = init_and_spawn_two(&repo, "compute impl A", "compute impl B");

    write_vp(&vp1, "src/lib.rs", "pub fn compute(x: i32) -> i32 {\n    x * 2 + 10\n}\n");
    write_vp(&vp2, "src/lib.rs", "pub fn compute(x: i32) -> i32 {\n    x * 3 - 5\n}\n");

    repo.wait_for_radar_status(&vp1, RADAR_TIMEOUT, radar_has_active_conflict)
        .expect("radar should detect structural conflict");

    let radar = radar_json_snapshot(&repo);
    assert!(radar_has_active_conflict(&radar));

    seal_vp(&repo, "vp1", "seal compute impl A").assert_success();
    let seal2 = seal_vp(&repo, "vp2", "attempt conflicting compute impl B");
    assert_failure_with_any_keyword(&seal2, &["conflict", "structural", "merge", "diverged"]);

    let trunk = repo.read_file("src/lib.rs").unwrap();
    assert!(trunk.contains("x * 2 + 10"));
    assert!(!trunk.contains("x * 3 - 5"));
    assert_no_conflict_markers(&trunk);
    repo.loom(&["status"]).assert_success();
}

#[test]
fn tier2_f13_semantic_conflict_signature_vs_caller() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.write_file(
        "src/lib.rs",
        concat!(
            "pub fn process_order(id: u64) -> u64 {\n",
            "    id * 2\n",
            "}\n",
            "\n",
            "pub fn submit() -> u64 {\n",
            "    process_order(1)\n",
            "}\n",
        ),
    )
    .unwrap();

    let (vp1, vp2) = init_and_spawn_two(
        &repo,
        "mutate process_order signature",
        "add disjoint caller using old signature",
    );

    write_vp(
        &vp1,
        "src/lib.rs",
        concat!(
            "pub fn process_order(id: u64, priority: u8) -> u64 {\n",
            "    id * 2 + priority as u64\n",
            "}\n",
            "\n",
            "pub fn submit() -> u64 {\n",
            "    process_order(1, 0)\n",
            "}\n",
        ),
    );
    write_vp(
        &vp2,
        "src/lib.rs",
        concat!(
            "pub fn process_order(id: u64) -> u64 {\n",
            "    id * 2\n",
            "}\n",
            "\n",
            "pub fn submit() -> u64 {\n",
            "    process_order(1)\n",
            "}\n",
            "\n",
            "pub fn batch_submit() -> u64 {\n",
            "    process_order(99)\n",
            "}\n",
        ),
    );

    repo.wait_for_radar_status(&vp1, RADAR_TIMEOUT, radar_has_active_conflict)
        .expect("radar should detect semantic conflict");

    let radar = radar_json_snapshot(&repo);
    assert!(radar_has_active_conflict(&radar));

    seal_vp(&repo, "vp1", "seal signature mutation").assert_success();
    let seal2 = seal_vp(&repo, "vp2", "attempt sealing stale caller");
    assert_failure_with_any_keyword(
        &seal2,
        &["semantic", "conflict", "signature", "contract", "caller", "process_order"],
    );

    let trunk = repo.read_file("src/lib.rs").unwrap();
    assert!(trunk.contains("(id: u64, priority: u8)") && trunk.contains("process_order(1, 0)"));
    assert!(!trunk.contains("batch_submit"));
    assert_no_conflict_markers(&trunk);
}

#[test]
fn tier2_f13_concurrent_identical_body_edit_convergence() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.write_file(
        "src/lib.rs",
        "pub fn target(x: i32) -> i32 {\n    x + 1\n}\n\npub fn untouched() -> i32 {\n    0\n}\n",
    )
    .unwrap();

    let (vp1, vp2) = init_and_spawn_two(
        &repo,
        "identical body edit 1",
        "identical body edit 2",
    );

    let updated =
        "pub fn target(x: i32) -> i32 {\n    x + 42\n}\n\npub fn untouched() -> i32 {\n    0\n}\n";
    write_vp(&vp1, "src/lib.rs", updated);
    write_vp(&vp2, "src/lib.rs", updated);

    seal_vp(&repo, "vp1", "seal identical edit 1").assert_success();
    seal_vp(&repo, "vp2", "seal identical edit 2").assert_success();

    let radar = radar_json_snapshot(&repo);
    assert!(!json_has_norm_token(&radar, "red"));

    let trunk = repo.read_file("src/lib.rs").unwrap();
    assert_eq!(trunk.matches("fn target").count(), 1, "unexpected duplicate:\n{trunk}");
    assert!(trunk.contains("x + 42") && trunk.contains("untouched"));
    assert_no_conflict_markers(&trunk);
}

#[test]
fn tier2_f13_concurrent_rename_same_file_different_targets() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.write_file("src/old.rs", "pub fn shared() -> i32 {\n    7\n}\n")
        .unwrap();

    let (vp1, vp2) = init_and_spawn_two(
        &repo,
        "rename old.rs to alpha.rs",
        "rename old.rs to beta.rs",
    );

    fs::rename(vp1.join("src/old.rs"), vp1.join("src/alpha.rs"))
        .expect("rename old.rs -> alpha.rs in vp1");
    fs::rename(vp2.join("src/old.rs"), vp2.join("src/beta.rs"))
        .expect("rename old.rs -> beta.rs in vp2");

    repo.wait_for_radar_status(&vp1, RADAR_TIMEOUT, radar_has_active_conflict)
        .expect("radar should detect divergent rename conflict");

    let radar = radar_json_snapshot(&repo);
    assert!(radar_has_active_conflict(&radar));

    seal_vp(&repo, "vp1", "seal rename to alpha.rs").assert_success();
    let seal2 = seal_vp(&repo, "vp2", "attempt divergent rename to beta.rs");
    assert_failure_with_any_keyword(
        &seal2,
        &["conflict", "rename", "move", "missing", "deleted", "merge"],
    );

    assert!(repo.exists("src/alpha.rs"));
    assert!(!repo.exists("src/beta.rs"));
    assert!(!repo.exists("src/old.rs"));
    let alpha = repo.read_file("src/alpha.rs").unwrap();
    assert!(alpha.contains("pub fn shared() -> i32"));
    assert_no_conflict_markers(&alpha);
}

#[test]
fn tier2_f13_large_file_500_functions_merge() {
    let repo = TestRepo::new().expect("TestRepo::new");

    let build_source = |f005_val: i32, f495_val: i32, add_top: bool, add_bot: bool| -> String {
        let mut out = String::with_capacity(32 * 1024);
        for i in 0..500 {
            let val = match i {
                5 => f005_val,
                495 => f495_val,
                _ => i,
            };
            out.push_str(&format!("pub fn f{i:03}() -> i32 {{ {val} }}\n"));
            if add_top && i == 5 {
                out.push_str("pub fn f_new_top() -> i32 { 9001 }\n");
            }
        }
        if add_bot {
            out.push_str("pub fn f_new_bot() -> i32 { 9002 }\n");
        }
        out
    };

    repo.write_file("src/big.rs", &build_source(5, 495, false, false))
        .unwrap();

    let (vp1, vp2) = init_and_spawn_two(
        &repo,
        "edit f005 and insert f_new_top",
        "edit f495 and append f_new_bot",
    );

    write_vp(&vp1, "src/big.rs", &build_source(5005, 495, true, false));
    write_vp(&vp2, "src/big.rs", &build_source(5, 49500, false, true));

    wait_radar_green(&repo, &vp1, LARGE_RADAR_TIMEOUT);
    wait_radar_green(&repo, &vp2, LARGE_RADAR_TIMEOUT);
    let radar = radar_json_snapshot(&repo);
    assert!(!json_has_norm_token(&radar, "red"));

    seal_vp(&repo, "vp1", "seal top slice of 500-fn file").assert_success();
    seal_vp(&repo, "vp2", "seal bottom slice of 500-fn file").assert_success();

    let trunk = repo.read_file("src/big.rs").unwrap();
    assert_eq!(
        trunk.matches("pub fn f").count(),
        502,
        "expected 502 functions in merged big.rs"
    );
    assert!(trunk.contains("pub fn f005() -> i32 { 5005 }"));
    assert!(!trunk.contains("pub fn f005() -> i32 { 5 }"));
    assert!(trunk.contains("pub fn f495() -> i32 { 49500 }"));
    assert!(!trunk.contains("pub fn f495() -> i32 { 495 }"));
    assert!(trunk.contains("pub fn f_new_top() -> i32 { 9001 }"));
    assert!(trunk.contains("pub fn f_new_bot() -> i32 { 9002 }"));
    assert_no_conflict_markers(&trunk);
}

// ============================================================================
// F14: Non-Code Automerge CRDT & Binary CAS (5 Tests)
// ============================================================================

#[test]
fn tier2_f14_post_merge_invalid_json_rejected() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.write_file("config.json", "{\"flag\": null, \"keep\": 1}\n")
        .unwrap();

    let (vp1, vp2) = init_and_spawn_two(
        &repo,
        "set flag to true",
        "set flag to false",
    );

    let vp1_json = "{\"flag\": true, \"keep\": 1}\n";
    let vp2_json = "{\"flag\": false, \"keep\": 1}\n";
    assert!(serde_json::from_str::<Value>(vp1_json).is_ok());
    assert!(serde_json::from_str::<Value>(vp2_json).is_ok());

    write_vp(&vp1, "config.json", vp1_json);
    write_vp(&vp2, "config.json", vp2_json);

    seal_vp(&repo, "vp1", "seal valid json flag=true").assert_success();
    let parsed_after_vp1: Value =
        serde_json::from_str(&repo.read_file("config.json").unwrap()).expect("valid trunk json");
    assert_eq!(parsed_after_vp1, json!({"flag": true, "keep": 1}));

    let seal2 = seal_vp(&repo, "vp2", "attempt sealing colliding json flag=false");
    assert_failure_with_any_keyword(
        &seal2,
        &["json", "invalid", "syntax", "parse", "malformed", "conflict", "validation"],
    );

    let trunk_raw = repo.read_file("config.json").unwrap();
    let parsed_after_reject: Value =
        serde_json::from_str(&trunk_raw).expect("trunk json must remain valid");
    assert_eq!(parsed_after_reject, json!({"flag": true, "keep": 1}));
    assert_no_conflict_markers(&trunk_raw);
}

#[test]
fn tier2_f14_post_merge_invalid_yaml_rejected() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.write_file("deploy.yaml", "mode: \"init\"\nreplicas: 1\n")
        .unwrap();

    let (vp1, vp2) = init_and_spawn_two(
        &repo,
        "set yaml mode to sequence",
        "set yaml mode to mapping",
    );

    let vp1_yaml = "mode: [1, 2]\nreplicas: 1\n";
    let vp2_yaml = "mode: {a: 1}\nreplicas: 1\n";
    write_vp(&vp1, "deploy.yaml", vp1_yaml);
    write_vp(&vp2, "deploy.yaml", vp2_yaml);

    seal_vp(&repo, "vp1", "seal valid yaml sequence").assert_success();
    let trunk_after_vp1 = repo.read_file("deploy.yaml").unwrap();
    assert_eq!(trunk_after_vp1.trim(), vp1_yaml.trim());

    let seal2 = seal_vp(&repo, "vp2", "attempt sealing colliding yaml mapping");
    assert_failure_with_any_keyword(
        &seal2,
        &["yaml", "invalid", "syntax", "parse", "malformed", "conflict", "validation"],
    );

    let trunk_after_reject = repo.read_file("deploy.yaml").unwrap();
    assert_eq!(
        trunk_after_reject.trim(),
        vp1_yaml.trim(),
        "trunk deploy.yaml must remain valid after rejected CRDT merge"
    );
    assert_no_conflict_markers(&trunk_after_reject);
}

#[test]
fn tier2_f14_concurrent_binary_edit_conflict() {
    let repo = TestRepo::new().expect("TestRepo::new");

    let mut base_bin = vec![0x89u8, 0x50, 0x4E, 0x47, 0x00, 0xFF, 0xFE, 0x28, 0xB5, 0x2F, 0xFD];
    base_bin.resize(4096, 0x7E);
    repo.write_bytes("assets/sprite.bin", &base_bin).unwrap();

    let (vp1, vp2) = init_and_spawn_two(
        &repo,
        "binary payload update 1",
        "binary payload update 2",
    );

    let mut bin_v1 = vec![0x00u8, 0xFF, 0x11, 0x89, 0x50, 0x4E, 0x47, 0xDE, 0xAD, 0xBE, 0xEF];
    bin_v1.resize(4096, 0xAA);
    let mut bin_v2 = vec![0x00u8, 0xFE, 0x22, 0x89, 0x50, 0x4E, 0x47, 0xCA, 0xFE, 0xBA, 0xBE];
    bin_v2.resize(4096, 0x55);

    write_vp_bytes(&vp1, "assets/sprite.bin", &bin_v1);
    write_vp_bytes(&vp2, "assets/sprite.bin", &bin_v2);

    repo.wait_for_radar_status(&vp1, RADAR_TIMEOUT, radar_has_active_conflict)
        .expect("radar should detect concurrent binary conflict");

    let radar = radar_json_snapshot(&repo);
    assert!(radar_has_active_conflict(&radar));

    seal_vp(&repo, "vp1", "seal binary v1").assert_success();
    let trunk_bytes_1 = repo.read_bytes("assets/sprite.bin").unwrap();
    assert_eq!(trunk_bytes_1, bin_v1, "trunk binary must match vp1 bit-exactly");

    let seal2 = seal_vp(&repo, "vp2", "attempt sealing conflicting binary v2");
    assert_failure_with_any_keyword(
        &seal2,
        &["conflict", "binary", "manifest", "hash", "blob", "merge"],
    );

    let trunk_bytes_2 = repo.read_bytes("assets/sprite.bin").unwrap();
    assert_eq!(
        trunk_bytes_2, bin_v1,
        "trunk binary must stay bit-exact after rejected binary conflict"
    );
}

#[test]
fn tier2_f14_empty_json_yaml_md_files() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.write_file("empty.json", "").unwrap();
    repo.write_file("empty.yaml", "").unwrap();
    repo.write_file("empty.md", "").unwrap();
    repo.write_file("notes.md", "").unwrap();
    repo.write_file("stay_empty.md", "").unwrap();
    repo.write_file("data.json", "{\"a\": 1}\n").unwrap();

    let (vp1, vp2) = init_and_spawn_two(
        &repo,
        "populate empty.json and empty.md",
        "populate empty.yaml and notes.md",
    );

    for vp in [&vp1, &vp2] {
        repo.wait_for_radar_status(vp, RADAR_TIMEOUT, |v| {
            !radar_has_active_conflict(v) && !radar_has_pending_syntax(v)
        })
        .expect("0-byte CRDT files must not panic radar");
    }

    write_vp(&vp1, "empty.json", "{\"service\": \"loom\"}\n");
    write_vp(&vp1, "empty.md", "# Overview\n\nAlpha.\n");
    write_vp(&vp2, "empty.yaml", "service: loom\n");
    write_vp(&vp2, "notes.md", "## Notes\n\nBeta.\n");

    wait_radar_green(&repo, &vp1, RADAR_TIMEOUT);
    wait_radar_green(&repo, &vp2, RADAR_TIMEOUT);

    seal_vp(&repo, "vp1", "seal populated empty.json and empty.md").assert_success();
    seal_vp(&repo, "vp2", "seal populated empty.yaml and notes.md").assert_success();

    let empty_json: Value =
        serde_json::from_str(&repo.read_file("empty.json").unwrap()).expect("valid empty.json");
    assert_eq!(empty_json, json!({"service": "loom"}));
    assert!(repo.read_file("empty.yaml").unwrap().contains("service: loom"));
    assert!(repo.read_file("empty.md").unwrap().contains("# Overview"));
    assert!(repo.read_file("notes.md").unwrap().contains("## Notes"));
    assert_eq!(repo.read_file("stay_empty.md").unwrap(), "");

    // Part B: Sealing 0-byte empty JSON over valid trunk JSON must fail serde_json gate
    repo.loom(&["spawn", "-n", "vp3", "-m", "truncate data.json to 0 bytes"])
        .assert_success();
    let vp3 = repo.viewport_dir("vp3");
    write_vp(&vp3, "data.json", "");

    let seal3 = seal_vp(&repo, "vp3", "attempt sealing 0-byte data.json");
    assert_failure_with_any_keyword(
        &seal3,
        &["json", "invalid", "empty", "syntax", "parse", "eof", "validation"],
    );

    let data_json_raw = repo.read_file("data.json").unwrap();
    let data_json: Value =
        serde_json::from_str(&data_json_raw).expect("trunk data.json must remain valid JSON");
    assert_eq!(data_json, json!({"a": 1}));
    assert_no_conflict_markers(&data_json_raw);
    repo.loom(&["status"]).assert_success();
}

#[test]
fn tier2_f14_crlf_vs_lf_markdown_crdt() {
    let repo = TestRepo::new().expect("TestRepo::new");
    repo.write_file(
        "docs/SPEC.md",
        "# Specification\n\n## Section A\nInitial A\n\n## Section B\nInitial B\n",
    )
    .unwrap();

    let (vp1, vp2) = init_and_spawn_two(
        &repo,
        "edit Section A with CRLF",
        "edit Section B with LF",
    );

    write_vp(
        &vp1,
        "docs/SPEC.md",
        "# Specification\r\n\r\n## Section A\r\nUpdated A from vp1 (CRLF)\r\n\r\n## Section B\r\nInitial B\r\n",
    );
    write_vp(
        &vp2,
        "docs/SPEC.md",
        "# Specification\n\n## Section A\nInitial A\n\n## Section B\nUpdated B from vp2 (LF)\n",
    );

    wait_radar_green(&repo, &vp1, RADAR_TIMEOUT);
    wait_radar_green(&repo, &vp2, RADAR_TIMEOUT);
    let radar = radar_json_snapshot(&repo);
    assert!(!json_has_norm_token(&radar, "red"));

    seal_vp(&repo, "vp1", "seal CRLF Section A").assert_success();
    seal_vp(&repo, "vp2", "seal LF Section B").assert_success();

    let raw = repo.read_file("docs/SPEC.md").unwrap();
    assert_no_conflict_markers(&raw);
    let normalized = raw.replace("\r\n", "\n");
    assert!(normalized.contains("# Specification"));
    assert!(
        normalized.contains("Updated A from vp1 (CRLF)"),
        "CRLF Section A edit missing:\n{normalized}"
    );
    assert!(
        normalized.contains("Updated B from vp2 (LF)"),
        "LF Section B edit missing:\n{normalized}"
    );
    assert!(
        !normalized.contains("Initial A") && !normalized.contains("Initial B"),
        "stale section text survived:\n{normalized}"
    );
}
