use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use e2e_suite::TestRepo;

fn init_repo_with_files(files: &[(&str, &str)]) -> TestRepo {
    let repo = TestRepo::new().expect("create TestRepo");
    for (rel, content) in files {
        repo.write_file(rel, content)
            .unwrap_or_else(|e| panic!("write trunk fixture {rel}: {e}"));
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
    write_vp_bytes(vp, rel, content.as_bytes());
}

fn write_vp_bytes(vp: &Path, rel: &str, bytes: &[u8]) {
    let target = vp.join(rel);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .unwrap_or_else(|e| panic!("create parent dirs for {}: {e}", target.display()));
    }
    fs::write(&target, bytes)
        .unwrap_or_else(|e| panic!("write viewport file {}: {e}", target.display()));
}

fn seal_vp(repo: &TestRepo, vp: &Path, msg: &str) {
    repo.loom_in(vp, &["seal", "-m", msg]).assert_success();
}

fn norm_lf(s: &str) -> String {
    s.replace("\r\n", "\n")
}

fn assert_no_conflict_markers(s: &str, label: &str) {
    assert!(
        !s.contains("<<<<<<<") && !s.contains("=======") && !s.contains(">>>>>>>"),
        "{label} unexpectedly contained textual conflict markers:\n{s}"
    );
}

fn has_any_file_recursive(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            return true;
        }
        if path.is_dir() && has_any_file_recursive(&path) {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// F11: 4-Language Tree-sitter CST & Leading Trivia (5 Tests)
// ---------------------------------------------------------------------------

#[test]
fn tier1_f11_rust_leading_doc_and_attr_preserved() {
    let initial = concat!(
        "/// Computes alpha.\n",
        "#[inline]\n",
        "#[must_use]\n",
        "pub fn alpha(x: i32) -> i32 {\n",
        "    x + 1\n",
        "}\n",
        "\n",
        "/// Computes beta.\n",
        "#[inline]\n",
        "pub fn beta(x: i32) -> i32 {\n",
        "    x * 2\n",
        "}\n",
    );
    let repo = init_repo_with_files(&[("src/lib.rs", initial)]);
    let vp1 = spawn_vp(&repo, "vp1", "edit alpha and insert delta");
    let vp2 = spawn_vp(&repo, "vp2", "edit beta");

    write_vp_file(
        &vp1,
        "src/lib.rs",
        concat!(
            "/// Computes alpha.\n",
            "#[inline]\n",
            "#[must_use]\n",
            "pub fn alpha(x: i32) -> i32 {\n",
            "    x + 10\n",
            "}\n",
            "\n",
            "/// Computes delta.\n",
            "#[allow(dead_code)]\n",
            "pub fn delta() -> i32 {\n",
            "    4\n",
            "}\n",
            "\n",
            "/// Computes beta.\n",
            "#[inline]\n",
            "pub fn beta(x: i32) -> i32 {\n",
            "    x * 2\n",
            "}\n",
        ),
    );

    write_vp_file(
        &vp2,
        "src/lib.rs",
        concat!(
            "/// Computes alpha.\n",
            "#[inline]\n",
            "#[must_use]\n",
            "pub fn alpha(x: i32) -> i32 {\n",
            "    x + 1\n",
            "}\n",
            "\n",
            "/// Computes beta.\n",
            "#[inline]\n",
            "pub fn beta(x: i32) -> i32 {\n",
            "    x * 20\n",
            "}\n",
        ),
    );

    seal_vp(&repo, &vp1, "vp1");
    seal_vp(&repo, &vp2, "vp2");

    let trunk = norm_lf(&repo.read_file("src/lib.rs").expect("read trunk src/lib.rs"));
    assert_no_conflict_markers(&trunk, "src/lib.rs");
    assert!(trunk.contains("/// Computes alpha.\n#[inline]\n#[must_use]\npub fn alpha"));
    assert!(trunk.contains("/// Computes delta.\n#[allow(dead_code)]\npub fn delta"));
    assert!(trunk.contains("/// Computes beta.\n#[inline]\npub fn beta"));
    assert!(trunk.contains("x + 10"));
    assert!(trunk.contains("x * 20"));
    assert_eq!(trunk.matches("/// Computes alpha.").count(), 1);
    assert_eq!(trunk.matches("/// Computes delta.").count(), 1);
    assert_eq!(trunk.matches("/// Computes beta.").count(), 1);
}

#[test]
fn tier1_f11_typescript_jsdoc_and_decorator_preserved() {
    let initial = concat!(
        "function Loggable(_t: unknown) {}\n",
        "\n",
        "/** Service for greetings. */\n",
        "@Loggable\n",
        "export class GreeterService {\n",
        "  /** Returns greeting. */\n",
        "  greet(name: string): string {\n",
        "    return `hi ${name}`;\n",
        "  }\n",
        "}\n",
        "\n",
        "/** Formats an id. */\n",
        "export function formatId(id: number): string {\n",
        "  return `id:${id}`;\n",
        "}\n",
    );
    let repo = init_repo_with_files(&[("src/service.ts", initial)]);
    let vp1 = spawn_vp(&repo, "vp1", "add farewell method");
    let vp2 = spawn_vp(&repo, "vp2", "update formatId and add isValidId");

    write_vp_file(
        &vp1,
        "src/service.ts",
        concat!(
            "function Loggable(_t: unknown) {}\n",
            "\n",
            "/** Service for greetings. */\n",
            "@Loggable\n",
            "export class GreeterService {\n",
            "  /** Returns greeting. */\n",
            "  greet(name: string): string {\n",
            "    return `hi ${name}`;\n",
            "  }\n",
            "\n",
            "  /** Returns farewell. */\n",
            "  farewell(name: string): string {\n",
            "    return `bye ${name}`;\n",
            "  }\n",
            "}\n",
            "\n",
            "/** Formats an id. */\n",
            "export function formatId(id: number): string {\n",
            "  return `id:${id}`;\n",
            "}\n",
        ),
    );

    write_vp_file(
        &vp2,
        "src/service.ts",
        concat!(
            "function Loggable(_t: unknown) {}\n",
            "\n",
            "/** Service for greetings. */\n",
            "@Loggable\n",
            "export class GreeterService {\n",
            "  /** Returns greeting. */\n",
            "  greet(name: string): string {\n",
            "    return `hi ${name}`;\n",
            "  }\n",
            "}\n",
            "\n",
            "/** Formats an id. */\n",
            "export function formatId(id: number): string {\n",
            "  return `ID-${id}`;\n",
            "}\n",
            "\n",
            "/** Validates id. */\n",
            "export function isValidId(id: number): boolean {\n",
            "  return id > 0;\n",
            "}\n",
        ),
    );

    seal_vp(&repo, &vp1, "vp1 ts");
    seal_vp(&repo, &vp2, "vp2 ts");

    let trunk = norm_lf(&repo.read_file("src/service.ts").expect("read trunk src/service.ts"));
    assert_no_conflict_markers(&trunk, "src/service.ts");
    assert!(trunk.contains("/** Service for greetings. */\n@Loggable\nexport class GreeterService"));
    assert!(trunk.contains("/** Returns greeting. */"));
    assert!(trunk.contains("/** Returns farewell. */"));
    assert!(trunk.contains("farewell(name: string): string"));
    assert!(trunk.contains("/** Formats an id. */\nexport function formatId"));
    assert!(trunk.contains("return `ID-${id}`;"));
    assert!(trunk.contains("/** Validates id. */\nexport function isValidId"));
    assert_eq!(trunk.matches("/** Service for greetings. */").count(), 1);
}

#[test]
fn tier1_f11_go_doc_comment_preserved() {
    let initial = concat!(
        "package pkg\n",
        "\n",
        "// Add sums two integers.\n",
        "// It handles negative inputs cleanly.\n",
        "func Add(a int, b int) int {\n",
        "\treturn a + b\n",
        "}\n",
        "\n",
        "// Mul multiplies two integers.\n",
        "func Mul(a int, b int) int {\n",
        "\treturn a * b\n",
        "}\n",
    );
    let repo = init_repo_with_files(&[("pkg/calc.go", initial)]);
    let vp1 = spawn_vp(&repo, "vp1", "update Add and insert Sub");
    let vp2 = spawn_vp(&repo, "vp2", "update Mul");

    write_vp_file(
        &vp1,
        "pkg/calc.go",
        concat!(
            "package pkg\n",
            "\n",
            "// Add sums two integers.\n",
            "// It handles negative inputs cleanly.\n",
            "func Add(a int, b int) int {\n",
            "\tsum := a + b\n",
            "\treturn sum\n",
            "}\n",
            "\n",
            "// Sub subtracts b from a.\n",
            "func Sub(a int, b int) int {\n",
            "\treturn a - b\n",
            "}\n",
            "\n",
            "// Mul multiplies two integers.\n",
            "func Mul(a int, b int) int {\n",
            "\treturn a * b\n",
            "}\n",
        ),
    );

    write_vp_file(
        &vp2,
        "pkg/calc.go",
        concat!(
            "package pkg\n",
            "\n",
            "// Add sums two integers.\n",
            "// It handles negative inputs cleanly.\n",
            "func Add(a int, b int) int {\n",
            "\treturn a + b\n",
            "}\n",
            "\n",
            "// Mul multiplies two integers.\n",
            "func Mul(a int, b int) int {\n",
            "\tprod := a * b\n",
            "\treturn prod\n",
            "}\n",
        ),
    );

    seal_vp(&repo, &vp1, "vp1 go");
    seal_vp(&repo, &vp2, "vp2 go");

    let trunk = norm_lf(&repo.read_file("pkg/calc.go").expect("read trunk pkg/calc.go"));
    assert_no_conflict_markers(&trunk, "pkg/calc.go");
    assert!(trunk.contains("// Add sums two integers.\n// It handles negative inputs cleanly.\nfunc Add"));
    assert!(trunk.contains("sum := a + b"));
    assert!(trunk.contains("// Sub subtracts b from a.\nfunc Sub"));
    assert!(trunk.contains("// Mul multiplies two integers.\nfunc Mul"));
    assert!(trunk.contains("prod := a * b"));
    assert_eq!(trunk.matches("// Add sums two integers.").count(), 1);
    assert_eq!(trunk.matches("// Sub subtracts b from a.").count(), 1);
    assert_eq!(trunk.matches("// Mul multiplies two integers.").count(), 1);
}

#[test]
fn tier1_f11_python_comment_and_decorator_preserved() {
    let initial = concat!(
        "def traced(fn):\n",
        "    return fn\n",
        "\n",
        "# Handles user lookup requests.\n",
        "@traced\n",
        "def get_user(user_id: int) -> dict:\n",
        "    return {\"id\": user_id}\n",
        "\n",
        "# Handles health checks.\n",
        "@traced\n",
        "def health_check() -> str:\n",
        "    return \"ok\"\n",
    );
    let repo = init_repo_with_files(&[("app/handlers.py", initial)]);
    let vp1 = spawn_vp(&repo, "vp1", "edit get_user and add create_user");
    let vp2 = spawn_vp(&repo, "vp2", "edit health_check");

    write_vp_file(
        &vp1,
        "app/handlers.py",
        concat!(
            "def traced(fn):\n",
            "    return fn\n",
            "\n",
            "# Handles user lookup requests.\n",
            "@traced\n",
            "def get_user(user_id: int) -> dict:\n",
            "    return {\"id\": user_id, \"active\": True}\n",
            "\n",
            "# Creates a user.\n",
            "@traced\n",
            "def create_user(name: str) -> dict:\n",
            "    return {\"name\": name}\n",
            "\n",
            "# Handles health checks.\n",
            "@traced\n",
            "def health_check() -> str:\n",
            "    return \"ok\"\n",
        ),
    );

    write_vp_file(
        &vp2,
        "app/handlers.py",
        concat!(
            "def traced(fn):\n",
            "    return fn\n",
            "\n",
            "# Handles user lookup requests.\n",
            "@traced\n",
            "def get_user(user_id: int) -> dict:\n",
            "    return {\"id\": user_id}\n",
            "\n",
            "# Handles health checks.\n",
            "@traced\n",
            "def health_check() -> str:\n",
            "    return \"healthy\"\n",
        ),
    );

    seal_vp(&repo, &vp1, "vp1 py");
    seal_vp(&repo, &vp2, "vp2 py");

    let trunk = norm_lf(&repo.read_file("app/handlers.py").expect("read trunk app/handlers.py"));
    assert_no_conflict_markers(&trunk, "app/handlers.py");
    assert!(trunk.contains("# Handles user lookup requests.\n@traced\ndef get_user"));
    assert!(trunk.contains("\"active\": True"));
    assert!(trunk.contains("# Creates a user.\n@traced\ndef create_user"));
    assert!(trunk.contains("# Handles health checks.\n@traced\ndef health_check"));
    assert!(trunk.contains("return \"healthy\""));
    assert_eq!(trunk.matches("# Handles user lookup requests.").count(), 1);
    assert_eq!(trunk.matches("# Creates a user.").count(), 1);
    assert_eq!(trunk.matches("# Handles health checks.").count(), 1);
}

#[test]
fn tier1_f11_polyglot_trailing_and_interleaved_comments() {
    let rs_init = concat!(
        "// Banner: Rust polyglot fixture\n",
        "\n",
        "// Leading alpha rs\n",
        "pub fn alpha(x: i32) -> i32 {\n",
        "    x + 1 // inline alpha rs\n",
        "}\n",
        "\n",
        "// Leading beta rs\n",
        "pub fn beta(x: i32) -> i32 {\n",
        "    x + 2 // trailing beta rs\n",
        "}\n",
    );
    let ts_init = concat!(
        "// Banner: TS polyglot fixture\n",
        "\n",
        "// Leading alpha ts\n",
        "export function alpha(x: number): number {\n",
        "  return x + 1; // inline alpha ts\n",
        "}\n",
        "\n",
        "// Leading beta ts\n",
        "export function beta(x: number): number {\n",
        "  return x + 2; // trailing beta ts\n",
        "}\n",
    );
    let go_init = concat!(
        "// Banner: Go polyglot fixture\n",
        "\n",
        "package pkg\n",
        "\n",
        "// Leading alpha go\n",
        "func Alpha(x int) int {\n",
        "\treturn x + 1 // inline alpha go\n",
        "}\n",
        "\n",
        "// Leading beta go\n",
        "func Beta(x int) int {\n",
        "\treturn x + 2 // trailing beta go\n",
        "}\n",
    );
    let py_init = concat!(
        "# Banner: Python polyglot fixture\n",
        "\n",
        "# Leading alpha py\n",
        "def alpha(x: int) -> int:\n",
        "    return x + 1  # inline alpha py\n",
        "\n",
        "# Leading beta py\n",
        "def beta(x: int) -> int:\n",
        "    return x + 2  # trailing beta py\n",
    );

    let repo = init_repo_with_files(&[
        ("src/lib.rs", rs_init),
        ("src/util.ts", ts_init),
        ("pkg/util.go", go_init),
        ("app/util.py", py_init),
    ]);
    let vp1 = spawn_vp(&repo, "vp1", "edit alpha in 4 langs");
    let vp2 = spawn_vp(&repo, "vp2", "edit beta in 4 langs");

    write_vp_file(
        &vp1,
        "src/lib.rs",
        concat!(
            "// Banner: Rust polyglot fixture\n",
            "\n",
            "// Leading alpha rs\n",
            "pub fn alpha(x: i32) -> i32 {\n",
            "    // inner_vp1\n",
            "    x + 11 // inline alpha rs\n",
            "}\n",
            "\n",
            "// Leading beta rs\n",
            "pub fn beta(x: i32) -> i32 {\n",
            "    x + 2 // trailing beta rs\n",
            "}\n",
        ),
    );
    write_vp_file(
        &vp1,
        "src/util.ts",
        concat!(
            "// Banner: TS polyglot fixture\n",
            "\n",
            "// Leading alpha ts\n",
            "export function alpha(x: number): number {\n",
            "  // inner_vp1\n",
            "  return x + 11; // inline alpha ts\n",
            "}\n",
            "\n",
            "// Leading beta ts\n",
            "export function beta(x: number): number {\n",
            "  return x + 2; // trailing beta ts\n",
            "}\n",
        ),
    );
    write_vp_file(
        &vp1,
        "pkg/util.go",
        concat!(
            "// Banner: Go polyglot fixture\n",
            "\n",
            "package pkg\n",
            "\n",
            "// Leading alpha go\n",
            "func Alpha(x int) int {\n",
            "\t// inner_vp1\n",
            "\treturn x + 11 // inline alpha go\n",
            "}\n",
            "\n",
            "// Leading beta go\n",
            "func Beta(x int) int {\n",
            "\treturn x + 2 // trailing beta go\n",
            "}\n",
        ),
    );
    write_vp_file(
        &vp1,
        "app/util.py",
        concat!(
            "# Banner: Python polyglot fixture\n",
            "\n",
            "# Leading alpha py\n",
            "def alpha(x: int) -> int:\n",
            "    # inner_vp1\n",
            "    return x + 11  # inline alpha py\n",
            "\n",
            "# Leading beta py\n",
            "def beta(x: int) -> int:\n",
            "    return x + 2  # trailing beta py\n",
        ),
    );

    write_vp_file(
        &vp2,
        "src/lib.rs",
        concat!(
            "// Banner: Rust polyglot fixture\n",
            "\n",
            "// Leading alpha rs\n",
            "pub fn alpha(x: i32) -> i32 {\n",
            "    x + 1 // inline alpha rs\n",
            "}\n",
            "\n",
            "// Leading beta rs\n",
            "pub fn beta(x: i32) -> i32 {\n",
            "    x + 22 // trailing beta rs\n",
            "}\n",
        ),
    );
    write_vp_file(
        &vp2,
        "src/util.ts",
        concat!(
            "// Banner: TS polyglot fixture\n",
            "\n",
            "// Leading alpha ts\n",
            "export function alpha(x: number): number {\n",
            "  return x + 1; // inline alpha ts\n",
            "}\n",
            "\n",
            "// Leading beta ts\n",
            "export function beta(x: number): number {\n",
            "  return x + 22; // trailing beta ts\n",
            "}\n",
        ),
    );
    write_vp_file(
        &vp2,
        "pkg/util.go",
        concat!(
            "// Banner: Go polyglot fixture\n",
            "\n",
            "package pkg\n",
            "\n",
            "// Leading alpha go\n",
            "func Alpha(x int) int {\n",
            "\treturn x + 1 // inline alpha go\n",
            "}\n",
            "\n",
            "// Leading beta go\n",
            "func Beta(x int) int {\n",
            "\treturn x + 22 // trailing beta go\n",
            "}\n",
        ),
    );
    write_vp_file(
        &vp2,
        "app/util.py",
        concat!(
            "# Banner: Python polyglot fixture\n",
            "\n",
            "# Leading alpha py\n",
            "def alpha(x: int) -> int:\n",
            "    return x + 1  # inline alpha py\n",
            "\n",
            "# Leading beta py\n",
            "def beta(x: int) -> int:\n",
            "    return x + 22  # trailing beta py\n",
        ),
    );

    seal_vp(&repo, &vp1, "polyglot alpha");
    seal_vp(&repo, &vp2, "polyglot beta");

    let rs = norm_lf(&repo.read_file("src/lib.rs").unwrap());
    assert_no_conflict_markers(&rs, "src/lib.rs");
    assert_eq!(rs.matches("// Banner: Rust polyglot fixture").count(), 1);
    assert!(rs.contains("// Leading alpha rs\npub fn alpha"));
    assert!(rs.contains("// inner_vp1"));
    assert!(rs.contains("x + 11"));
    assert!(rs.contains("// Leading beta rs\npub fn beta"));
    assert!(rs.contains("x + 22 // trailing beta rs"));

    let ts = norm_lf(&repo.read_file("src/util.ts").unwrap());
    assert_no_conflict_markers(&ts, "src/util.ts");
    assert_eq!(ts.matches("// Banner: TS polyglot fixture").count(), 1);
    assert!(ts.contains("// Leading alpha ts\nexport function alpha"));
    assert!(ts.contains("// inner_vp1"));
    assert!(ts.contains("x + 11"));
    assert!(ts.contains("// Leading beta ts\nexport function beta"));
    assert!(ts.contains("x + 22; // trailing beta ts"));

    let go = norm_lf(&repo.read_file("pkg/util.go").unwrap());
    assert_no_conflict_markers(&go, "pkg/util.go");
    assert_eq!(go.matches("// Banner: Go polyglot fixture").count(), 1);
    assert!(go.contains("// Leading alpha go\nfunc Alpha"));
    assert!(go.contains("// inner_vp1"));
    assert!(go.contains("x + 11"));
    assert!(go.contains("// Leading beta go\nfunc Beta"));
    assert!(go.contains("x + 22 // trailing beta go"));

    let py = norm_lf(&repo.read_file("app/util.py").unwrap());
    assert_no_conflict_markers(&py, "app/util.py");
    assert_eq!(py.matches("# Banner: Python polyglot fixture").count(), 1);
    assert!(py.contains("# Leading alpha py\ndef alpha"));
    assert!(py.contains("# inner_vp1"));
    assert!(py.contains("x + 11"));
    assert!(py.contains("# Leading beta py\ndef beta"));
    assert!(py.contains("x + 22  # trailing beta py"));
}

// ---------------------------------------------------------------------------
// F12: Line-Independent NodeAnchor (5 Tests)
// ---------------------------------------------------------------------------

#[test]
fn tier1_f12_rust_anchor_survives_top_line_shift() {
    let initial = concat!(
        "pub fn first() -> i32 {\n",
        "    1\n",
        "}\n",
        "\n",
        "pub fn target_anchor(v: i32) -> i32 {\n",
        "    v + 1\n",
        "}\n",
    );
    let repo = init_repo_with_files(&[("src/lib.rs", initial)]);
    let vp1 = spawn_vp(&repo, "vp1", "prepend 15 lines at top");
    let vp2 = spawn_vp(&repo, "vp2", "edit target_anchor body");

    write_vp_file(
        &vp1,
        "src/lib.rs",
        concat!(
            "// line 1\n",
            "// line 2\n",
            "// line 3\n",
            "// line 4\n",
            "// line 5\n",
            "// line 6\n",
            "// line 7\n",
            "// line 8\n",
            "// line 9\n",
            "// line 10\n",
            "use std::fmt;\n",
            "\n",
            "pub fn prepended_helper() -> i32 {\n",
            "    0\n",
            "}\n",
            "\n",
            "pub fn first() -> i32 {\n",
            "    1\n",
            "}\n",
            "\n",
            "pub fn target_anchor(v: i32) -> i32 {\n",
            "    v + 1\n",
            "}\n",
        ),
    );

    write_vp_file(
        &vp2,
        "src/lib.rs",
        concat!(
            "pub fn first() -> i32 {\n",
            "    1\n",
            "}\n",
            "\n",
            "pub fn target_anchor(v: i32) -> i32 {\n",
            "    v * 42\n",
            "}\n",
        ),
    );

    seal_vp(&repo, &vp1, "prepend top lines");
    seal_vp(&repo, &vp2, "update target_anchor");

    let trunk = norm_lf(&repo.read_file("src/lib.rs").unwrap());
    assert_no_conflict_markers(&trunk, "src/lib.rs");
    assert!(trunk.contains("pub fn prepended_helper()"));
    assert!(trunk.contains("pub fn first()"));
    assert_eq!(trunk.matches("pub fn target_anchor").count(), 1);
    assert!(trunk.contains("v * 42"));
}

#[test]
fn tier1_f12_python_decorated_definition_unwrap() {
    let initial = concat!(
        "def route(path: str):\n",
        "    return lambda fn: fn\n",
        "\n",
        "@route(\"/v1/ping\")\n",
        "def ping_handler() -> str:\n",
        "    return \"pong\"\n",
        "\n",
        "@route(\"/v1/status\")\n",
        "def status_handler() -> str:\n",
        "    return \"ready\"\n",
    );
    let repo = init_repo_with_files(&[("app/routes.py", initial)]);
    let vp1 = spawn_vp(&repo, "vp1", "prepend helpers and edit ping_handler");
    let vp2 = spawn_vp(&repo, "vp2", "edit status_handler");

    write_vp_file(
        &vp1,
        "app/routes.py",
        concat!(
            "# header line 1\n",
            "# header line 2\n",
            "# header line 3\n",
            "# header line 4\n",
            "# header line 5\n",
            "API_VERSION = \"v1\"\n",
            "\n",
            "def helper_zero() -> int:\n",
            "    return 0\n",
            "\n",
            "def route(path: str):\n",
            "    return lambda fn: fn\n",
            "\n",
            "@route(\"/v1/ping\")\n",
            "def ping_handler() -> str:\n",
            "    return \"pong-v2\"\n",
            "\n",
            "@route(\"/v1/status\")\n",
            "def status_handler() -> str:\n",
            "    return \"ready\"\n",
        ),
    );

    write_vp_file(
        &vp2,
        "app/routes.py",
        concat!(
            "def route(path: str):\n",
            "    return lambda fn: fn\n",
            "\n",
            "@route(\"/v1/ping\")\n",
            "def ping_handler() -> str:\n",
            "    return \"pong\"\n",
            "\n",
            "@route(\"/v1/status\")\n",
            "def status_handler() -> str:\n",
            "    return \"ready-v2\"\n",
        ),
    );

    seal_vp(&repo, &vp1, "shift and edit ping");
    seal_vp(&repo, &vp2, "edit status");

    let trunk = norm_lf(&repo.read_file("app/routes.py").unwrap());
    assert_no_conflict_markers(&trunk, "app/routes.py");
    assert!(trunk.contains("def helper_zero()"));
    assert_eq!(trunk.matches("@route(\"/v1/ping\")").count(), 1);
    assert_eq!(trunk.matches("@route(\"/v1/status\")").count(), 1);
    assert!(trunk.contains("return \"pong-v2\""));
    assert!(trunk.contains("return \"ready-v2\""));
}

#[test]
fn tier1_f12_ts_export_statement_unwrap() {
    let initial = concat!(
        "export interface Item {\n",
        "  id: string;\n",
        "}\n",
        "\n",
        "export function fetchItem(id: string): Item {\n",
        "  return { id };\n",
        "}\n",
        "\n",
        "export class ItemStore {\n",
        "  count(): number {\n",
        "    return 0;\n",
        "  }\n",
        "}\n",
    );
    let repo = init_repo_with_files(&[("src/api.ts", initial)]);
    let vp1 = spawn_vp(&repo, "vp1", "prepend types and edit fetchItem");
    let vp2 = spawn_vp(&repo, "vp2", "edit ItemStore.count");

    write_vp_file(
        &vp1,
        "src/api.ts",
        concat!(
            "// prepended 1\n",
            "// prepended 2\n",
            "// prepended 3\n",
            "// prepended 4\n",
            "// prepended 5\n",
            "export type ItemId = string;\n",
            "export const DEFAULT_TAG = \"v1\";\n",
            "\n",
            "export function normalizeTag(t: string): string {\n",
            "  return t.trim();\n",
            "}\n",
            "\n",
            "export interface Item {\n",
            "  id: string;\n",
            "}\n",
            "\n",
            "export function fetchItem(id: string): Item {\n",
            "  return { id: id.trim() };\n",
            "}\n",
            "\n",
            "export class ItemStore {\n",
            "  count(): number {\n",
            "    return 0;\n",
            "  }\n",
            "}\n",
        ),
    );

    write_vp_file(
        &vp2,
        "src/api.ts",
        concat!(
            "export interface Item {\n",
            "  id: string;\n",
            "}\n",
            "\n",
            "export function fetchItem(id: string): Item {\n",
            "  return { id };\n",
            "}\n",
            "\n",
            "export class ItemStore {\n",
            "  count(): number {\n",
            "    return 42;\n",
            "  }\n",
            "}\n",
        ),
    );

    seal_vp(&repo, &vp1, "prepend ts types and edit fetchItem");
    seal_vp(&repo, &vp2, "edit ItemStore.count");

    let trunk = norm_lf(&repo.read_file("src/api.ts").unwrap());
    assert_no_conflict_markers(&trunk, "src/api.ts");
    assert!(trunk.contains("export type ItemId = string;"));
    assert!(trunk.contains("export function fetchItem(id: string): Item"));
    assert!(trunk.contains("return { id: id.trim() };"));
    assert!(trunk.contains("export class ItemStore"));
    assert!(trunk.contains("return 42;"));
}

#[test]
fn tier1_f12_go_method_receiver_anchor() {
    let initial = concat!(
        "package pkg\n",
        "\n",
        "type Counter struct {\n",
        "\tval int\n",
        "}\n",
        "\n",
        "type Gauge struct {\n",
        "\tval int\n",
        "}\n",
        "\n",
        "func (c *Counter) Value() int {\n",
        "\treturn c.val\n",
        "}\n",
        "\n",
        "func (g *Gauge) Value() int {\n",
        "\treturn g.val\n",
        "}\n",
    );
    let repo = init_repo_with_files(&[("pkg/store.go", initial)]);
    let vp1 = spawn_vp(&repo, "vp1", "add InitTag and update Counter.Value");
    let vp2 = spawn_vp(&repo, "vp2", "update Gauge.Value");

    write_vp_file(
        &vp1,
        "pkg/store.go",
        concat!(
            "package pkg\n",
            "\n",
            "func InitTag() string {\n",
            "\treturn \"v1\"\n",
            "}\n",
            "\n",
            "type Counter struct {\n",
            "\tval int\n",
            "}\n",
            "\n",
            "type Gauge struct {\n",
            "\tval int\n",
            "}\n",
            "\n",
            "func (c *Counter) Value() int {\n",
            "\treturn c.val + 1\n",
            "}\n",
            "\n",
            "func (g *Gauge) Value() int {\n",
            "\treturn g.val\n",
            "}\n",
        ),
    );

    write_vp_file(
        &vp2,
        "pkg/store.go",
        concat!(
            "package pkg\n",
            "\n",
            "type Counter struct {\n",
            "\tval int\n",
            "}\n",
            "\n",
            "type Gauge struct {\n",
            "\tval int\n",
            "}\n",
            "\n",
            "func (c *Counter) Value() int {\n",
            "\treturn c.val\n",
            "}\n",
            "\n",
            "func (g *Gauge) Value() int {\n",
            "\treturn g.val * 10\n",
            "}\n",
        ),
    );

    seal_vp(&repo, &vp1, "counter update");
    seal_vp(&repo, &vp2, "gauge update");

    let trunk = norm_lf(&repo.read_file("pkg/store.go").unwrap());
    assert_no_conflict_markers(&trunk, "pkg/store.go");
    assert!(trunk.contains("func InitTag() string"));
    assert!(trunk.contains("func (c *Counter) Value() int"));
    assert!(trunk.contains("return c.val + 1"));
    assert!(trunk.contains("func (g *Gauge) Value() int"));
    assert!(trunk.contains("return g.val * 10"));
}

#[test]
fn tier1_f12_same_named_methods_different_impls() {
    let initial = concat!(
        "pub struct Button;\n",
        "pub struct Label;\n",
        "\n",
        "impl Button {\n",
        "    pub fn render(&self) -> &'static str {\n",
        "        \"button\"\n",
        "    }\n",
        "}\n",
        "\n",
        "impl Label {\n",
        "    pub fn render(&self) -> &'static str {\n",
        "        \"label\"\n",
        "    }\n",
        "}\n",
    );
    let repo = init_repo_with_files(&[("src/widgets.rs", initial)]);
    let vp1 = spawn_vp(&repo, "vp1", "insert Spacer and update Button::render");
    let vp2 = spawn_vp(&repo, "vp2", "update Label::render");

    write_vp_file(
        &vp1,
        "src/widgets.rs",
        concat!(
            "pub struct Button;\n",
            "pub struct Label;\n",
            "pub struct Spacer;\n",
            "\n",
            "impl Spacer {\n",
            "    pub fn gap(&self) -> u32 {\n",
            "        4\n",
            "    }\n",
            "}\n",
            "\n",
            "impl Button {\n",
            "    pub fn render(&self) -> &'static str {\n",
            "        \"<button/>\"\n",
            "    }\n",
            "}\n",
            "\n",
            "impl Label {\n",
            "    pub fn render(&self) -> &'static str {\n",
            "        \"label\"\n",
            "    }\n",
            "}\n",
        ),
    );

    write_vp_file(
        &vp2,
        "src/widgets.rs",
        concat!(
            "pub struct Button;\n",
            "pub struct Label;\n",
            "\n",
            "impl Button {\n",
            "    pub fn render(&self) -> &'static str {\n",
            "        \"button\"\n",
            "    }\n",
            "}\n",
            "\n",
            "impl Label {\n",
            "    pub fn render(&self) -> &'static str {\n",
            "        \"<label/>\"\n",
            "    }\n",
            "}\n",
        ),
    );

    seal_vp(&repo, &vp1, "button and spacer");
    seal_vp(&repo, &vp2, "label");

    let trunk = norm_lf(&repo.read_file("src/widgets.rs").unwrap());
    assert_no_conflict_markers(&trunk, "src/widgets.rs");
    assert!(trunk.contains("pub struct Spacer;"));
    assert!(trunk.contains("pub fn gap(&self) -> u32"));
    assert!(trunk.contains("\"<button/>\""));
    assert!(trunk.contains("\"<label/>\""));
}

// ---------------------------------------------------------------------------
// F13: SemanticOp, AstPatch & Commutative Merge (5 Tests)
// ---------------------------------------------------------------------------

#[test]
fn tier1_f13_insert_member_commutative_same_block() {
    let initial = concat!(
        "pub struct Registry {\n",
        "    pub version: u32,\n",
        "}\n",
        "\n",
        "impl Registry {\n",
        "    pub fn new() -> Self {\n",
        "        Self { version: 1 }\n",
        "    }\n",
        "}\n",
    );
    let repo = init_repo_with_files(&[("src/lib.rs", initial)]);
    let vp1 = spawn_vp(&repo, "vp1", "insert get_version");
    let vp2 = spawn_vp(&repo, "vp2", "insert is_active");

    write_vp_file(
        &vp1,
        "src/lib.rs",
        concat!(
            "pub struct Registry {\n",
            "    pub version: u32,\n",
            "}\n",
            "\n",
            "impl Registry {\n",
            "    pub fn new() -> Self {\n",
            "        Self { version: 1 }\n",
            "    }\n",
            "\n",
            "    pub fn get_version(&self) -> u32 {\n",
            "        self.version\n",
            "    }\n",
            "}\n",
        ),
    );

    write_vp_file(
        &vp2,
        "src/lib.rs",
        concat!(
            "pub struct Registry {\n",
            "    pub version: u32,\n",
            "}\n",
            "\n",
            "impl Registry {\n",
            "    pub fn new() -> Self {\n",
            "        Self { version: 1 }\n",
            "    }\n",
            "\n",
            "    pub fn is_active(&self) -> bool {\n",
            "        self.version > 0\n",
            "    }\n",
            "}\n",
        ),
    );

    seal_vp(&repo, &vp1, "insert get_version");
    seal_vp(&repo, &vp2, "insert is_active");

    let trunk = norm_lf(&repo.read_file("src/lib.rs").unwrap());
    assert_no_conflict_markers(&trunk, "src/lib.rs");
    assert!(trunk.contains("pub fn new()"));
    assert!(trunk.contains("pub fn get_version(&self) -> u32"));
    assert!(trunk.contains("pub fn is_active(&self) -> bool"));
}

#[test]
fn tier1_f13_replace_body_disjoint_anchors() {
    let initial = concat!(
        "pub fn step_one(x: i32) -> i32 {\n",
        "    x + 1\n",
        "}\n",
        "\n",
        "pub fn step_two(x: i32) -> i32 {\n",
        "    x + 2\n",
        "}\n",
        "\n",
        "pub fn step_three(x: i32) -> i32 {\n",
        "    x + 3\n",
        "}\n",
    );
    let repo = init_repo_with_files(&[("src/math.rs", initial)]);
    let vp1 = spawn_vp(&repo, "vp1", "replace step_one");
    let vp2 = spawn_vp(&repo, "vp2", "replace step_three");

    write_vp_file(
        &vp1,
        "src/math.rs",
        concat!(
            "pub fn step_one(x: i32) -> i32 {\n",
            "    let y = x + 10;\n",
            "    y\n",
            "}\n",
            "\n",
            "pub fn step_two(x: i32) -> i32 {\n",
            "    x + 2\n",
            "}\n",
            "\n",
            "pub fn step_three(x: i32) -> i32 {\n",
            "    x + 3\n",
            "}\n",
        ),
    );

    write_vp_file(
        &vp2,
        "src/math.rs",
        concat!(
            "pub fn step_one(x: i32) -> i32 {\n",
            "    x + 1\n",
            "}\n",
            "\n",
            "pub fn step_two(x: i32) -> i32 {\n",
            "    x + 2\n",
            "}\n",
            "\n",
            "pub fn step_three(x: i32) -> i32 {\n",
            "    let z = x * 30;\n",
            "    z\n",
            "}\n",
        ),
    );

    seal_vp(&repo, &vp1, "update step_one");
    seal_vp(&repo, &vp2, "update step_three");

    let trunk = norm_lf(&repo.read_file("src/math.rs").unwrap());
    assert_no_conflict_markers(&trunk, "src/math.rs");
    assert!(trunk.contains("let y = x + 10;"));
    assert!(trunk.contains("x + 2"));
    assert!(trunk.contains("let z = x * 30;"));
}

#[test]
fn tier1_f13_commutative_determinism_lamport_hash_order() {
    let initial = concat!(
        "pub struct Bag;\n",
        "\n",
        "impl Bag {\n",
        "    pub fn base(&self) -> i32 {\n",
        "        0\n",
        "    }\n",
        "}\n",
    );
    let edit_a = concat!(
        "pub struct Bag;\n",
        "\n",
        "impl Bag {\n",
        "    pub fn base(&self) -> i32 {\n",
        "        0\n",
        "    }\n",
        "\n",
        "    pub fn alpha(&self) -> i32 {\n",
        "        1\n",
        "    }\n",
        "}\n",
    );
    let edit_b = concat!(
        "pub struct Bag;\n",
        "\n",
        "impl Bag {\n",
        "    pub fn base(&self) -> i32 {\n",
        "        0\n",
        "    }\n",
        "\n",
        "    pub fn beta(&self) -> i32 {\n",
        "        2\n",
        "    }\n",
        "}\n",
    );

    let repo_ab1 = init_repo_with_files(&[("src/lib.rs", initial)]);
    let ab1_a = spawn_vp(&repo_ab1, "vp_a", "add alpha");
    let ab1_b = spawn_vp(&repo_ab1, "vp_b", "add beta");
    write_vp_file(&ab1_a, "src/lib.rs", edit_a);
    write_vp_file(&ab1_b, "src/lib.rs", edit_b);
    seal_vp(&repo_ab1, &ab1_a, "seal a");
    seal_vp(&repo_ab1, &ab1_b, "seal b");

    let repo_ab2 = init_repo_with_files(&[("src/lib.rs", initial)]);
    let ab2_a = spawn_vp(&repo_ab2, "vp_a", "add alpha");
    let ab2_b = spawn_vp(&repo_ab2, "vp_b", "add beta");
    write_vp_file(&ab2_a, "src/lib.rs", edit_a);
    write_vp_file(&ab2_b, "src/lib.rs", edit_b);
    seal_vp(&repo_ab2, &ab2_a, "seal a");
    seal_vp(&repo_ab2, &ab2_b, "seal b");

    let repo_ba = init_repo_with_files(&[("src/lib.rs", initial)]);
    let ba_a = spawn_vp(&repo_ba, "vp_a", "add alpha");
    let ba_b = spawn_vp(&repo_ba, "vp_b", "add beta");
    write_vp_file(&ba_a, "src/lib.rs", edit_a);
    write_vp_file(&ba_b, "src/lib.rs", edit_b);
    seal_vp(&repo_ba, &ba_b, "seal b first");
    seal_vp(&repo_ba, &ba_a, "seal a second");

    let trunk_ab1 = norm_lf(&repo_ab1.read_file("src/lib.rs").unwrap());
    let trunk_ab2 = norm_lf(&repo_ab2.read_file("src/lib.rs").unwrap());
    let trunk_ba = norm_lf(&repo_ba.read_file("src/lib.rs").unwrap());

    assert_no_conflict_markers(&trunk_ab1, "repo_ab1");
    assert_no_conflict_markers(&trunk_ba, "repo_ba");
    assert_eq!(
        trunk_ab1, trunk_ab2,
        "identical seal order must produce deterministic trunk bytes"
    );

    for label_trunk in [&trunk_ab1, &trunk_ba] {
        assert!(label_trunk.contains("pub fn base(&self) -> i32"));
        assert!(label_trunk.contains("pub fn alpha(&self) -> i32"));
        assert!(label_trunk.contains("pub fn beta(&self) -> i32"));
    }
}

#[test]
fn tier1_f13_remove_node_clean_when_unreferenced() {
    let initial = concat!(
        "pub fn active_api(x: i32) -> i32 {\n",
        "    x + 1\n",
        "}\n",
        "\n",
        "pub fn deprecated_helper(x: i32) -> i32 {\n",
        "    x - 1\n",
        "}\n",
    );
    let repo = init_repo_with_files(&[("src/lib.rs", initial)]);
    let vp1 = spawn_vp(&repo, "vp1", "remove unreferenced deprecated_helper");
    let vp2 = spawn_vp(&repo, "vp2", "update active_api");

    write_vp_file(
        &vp1,
        "src/lib.rs",
        concat!("pub fn active_api(x: i32) -> i32 {\n", "    x + 1\n", "}\n"),
    );

    write_vp_file(
        &vp2,
        "src/lib.rs",
        concat!(
            "pub fn active_api(x: i32) -> i32 {\n",
            "    x + 100\n",
            "}\n",
            "\n",
            "pub fn deprecated_helper(x: i32) -> i32 {\n",
            "    x - 1\n",
            "}\n",
        ),
    );

    seal_vp(&repo, &vp1, "remove deprecated_helper");
    seal_vp(&repo, &vp2, "update active_api");

    let trunk = norm_lf(&repo.read_file("src/lib.rs").unwrap());
    assert_no_conflict_markers(&trunk, "src/lib.rs");
    assert!(trunk.contains("pub fn active_api"));
    assert!(trunk.contains("x + 100"));
    assert!(!trunk.contains("deprecated_helper"));
}

#[test]
fn tier1_f13_rename_or_move_file_with_disjoint_edit() {
    let repo = init_repo_with_files(&[
        ("src/old_mod.rs", "pub fn moved_fn() -> i32 {\n    7\n}\n"),
        ("src/other.rs", "pub fn other_fn() -> i32 {\n    9\n}\n"),
    ]);
    let vp1 = spawn_vp(&repo, "vp1", "move old_mod.rs to new_mod.rs");
    let vp2 = spawn_vp(&repo, "vp2", "edit other.rs");

    fs::remove_file(vp1.join("src").join("old_mod.rs"))
        .expect("remove old_mod.rs in vp1");
    write_vp_file(
        &vp1,
        "src/new_mod.rs",
        "pub fn moved_fn() -> i32 {\n    7\n}\n",
    );

    write_vp_file(
        &vp2,
        "src/other.rs",
        "pub fn other_fn() -> i32 {\n    99\n}\n",
    );

    seal_vp(&repo, &vp1, "rename old_mod -> new_mod");
    seal_vp(&repo, &vp2, "update other_fn");

    assert!(!repo.exists("src/old_mod.rs"));
    assert!(repo.exists("src/new_mod.rs"));
    let new_mod = norm_lf(&repo.read_file("src/new_mod.rs").unwrap());
    let other = norm_lf(&repo.read_file("src/other.rs").unwrap());
    assert!(new_mod.contains("pub fn moved_fn() -> i32"));
    assert!(other.contains("99"));
}

// ---------------------------------------------------------------------------
// F14: Non-Code Automerge CRDT & Binary CAS (5 Tests)
// ---------------------------------------------------------------------------

#[test]
fn tier1_f14_json_concurrent_disjoint_keys_crdt() {
    let initial = "{\n  \"service\": \"loom\",\n  \"port\": 8080,\n  \"retries\": 3\n}\n";
    let repo = init_repo_with_files(&[("config/settings.json", initial)]);
    let vp1 = spawn_vp(&repo, "vp1", "update port");
    let vp2 = spawn_vp(&repo, "vp2", "update retries and add log_level");

    write_vp_file(
        &vp1,
        "config/settings.json",
        "{\n  \"service\": \"loom\",\n  \"port\": 9090,\n  \"retries\": 3\n}\n",
    );
    write_vp_file(
        &vp2,
        "config/settings.json",
        "{\n  \"service\": \"loom\",\n  \"port\": 8080,\n  \"retries\": 5,\n  \"log_level\": \"info\"\n}\n",
    );

    seal_vp(&repo, &vp1, "json port");
    seal_vp(&repo, &vp2, "json retries and log_level");

    let trunk_raw = repo.read_file("config/settings.json").unwrap();
    assert_no_conflict_markers(&trunk_raw, "config/settings.json");
    let parsed: Value =
        serde_json::from_str(&trunk_raw).expect("merged config/settings.json must be valid JSON");
    assert_eq!(parsed["service"], "loom");
    assert_eq!(parsed["port"], 9090);
    assert_eq!(parsed["retries"], 5);
    assert_eq!(parsed["log_level"], "info");
}

#[test]
fn tier1_f14_yaml_concurrent_disjoint_keys_crdt() {
    let initial = "name: loom-worker\nreplicas: 2\ntimeout_sec: 30\n";
    let repo = init_repo_with_files(&[("deploy/app.yaml", initial)]);
    let vp1 = spawn_vp(&repo, "vp1", "scale replicas");
    let vp2 = spawn_vp(&repo, "vp2", "update timeout and add region");

    write_vp_file(
        &vp1,
        "deploy/app.yaml",
        "name: loom-worker\nreplicas: 4\ntimeout_sec: 30\n",
    );
    write_vp_file(
        &vp2,
        "deploy/app.yaml",
        "name: loom-worker\nreplicas: 2\ntimeout_sec: 60\nregion: us-east-1\n",
    );

    seal_vp(&repo, &vp1, "yaml replicas");
    seal_vp(&repo, &vp2, "yaml timeout and region");

    let trunk = norm_lf(&repo.read_file("deploy/app.yaml").unwrap());
    assert_no_conflict_markers(&trunk, "deploy/app.yaml");
    assert!(trunk.contains("name: loom-worker"));
    assert!(trunk.contains("replicas: 4"));
    assert!(trunk.contains("timeout_sec: 60"));
    assert!(trunk.contains("region: us-east-1"));
}

#[test]
fn tier1_f14_markdown_concurrent_sections_crdt() {
    let initial = concat!(
        "# Project Guide\n",
        "\n",
        "## Overview\n",
        "Initial overview.\n",
        "\n",
        "## Usage\n",
        "Initial usage.\n",
    );
    let repo = init_repo_with_files(&[("docs/GUIDE.md", initial)]);
    let vp1 = spawn_vp(&repo, "vp1", "update Overview section");
    let vp2 = spawn_vp(&repo, "vp2", "update Usage and add FAQ");

    write_vp_file(
        &vp1,
        "docs/GUIDE.md",
        concat!(
            "# Project Guide\n",
            "\n",
            "## Overview\n",
            "Updated overview from vp1.\n",
            "\n",
            "## Usage\n",
            "Initial usage.\n",
        ),
    );
    write_vp_file(
        &vp2,
        "docs/GUIDE.md",
        concat!(
            "# Project Guide\n",
            "\n",
            "## Overview\n",
            "Initial overview.\n",
            "\n",
            "## Usage\n",
            "Updated usage from vp2.\n",
            "\n",
            "## FAQ\n",
            "See docs.\n",
        ),
    );

    seal_vp(&repo, &vp1, "md overview");
    seal_vp(&repo, &vp2, "md usage and faq");

    let trunk = norm_lf(&repo.read_file("docs/GUIDE.md").unwrap());
    assert_no_conflict_markers(&trunk, "docs/GUIDE.md");
    assert!(trunk.contains("# Project Guide"));
    assert!(trunk.contains("Updated overview from vp1."));
    assert!(trunk.contains("Updated usage from vp2."));
    assert!(trunk.contains("## FAQ"));
    assert!(trunk.contains("See docs."));
}

#[test]
fn tier1_f14_binary_file_cas_chunk_roundtrip() {
    let mut initial_bytes = Vec::with_capacity(128 * 1024);
    for i in 0..(128 * 1024) {
        let b = match i % 6 {
            0 => 0x00,
            1 => 0xFF,
            2 => 0x28,
            3 => 0xB5,
            4 => 0x2F,
            _ => 0xFD,
        };
        initial_bytes.push(b ^ ((i >> 8) as u8));
    }

    let mut expected_bytes = Vec::with_capacity(192 * 1024);
    for i in 0..(192 * 1024) {
        let pattern = [0x00u8, 0xFF, 0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        expected_bytes.push(pattern[i % pattern.len()] ^ ((i / 17) as u8));
    }

    let repo = TestRepo::new().expect("create TestRepo");
    repo.write_bytes("assets/sprite.bin", &initial_bytes)
        .expect("write initial sprite.bin");
    repo.write_file("README.md", "# Assets\n")
        .expect("write README.md");
    repo.loom(&["init"]).assert_success();

    let vp1 = spawn_vp(&repo, "vp1", "replace binary sprite");
    let vp2 = spawn_vp(&repo, "vp2", "update README");

    write_vp_bytes(&vp1, "assets/sprite.bin", &expected_bytes);
    write_vp_file(&vp2, "README.md", "# Assets\nUpdated by vp2.\n");

    seal_vp(&repo, &vp1, "binary sprite update");
    seal_vp(&repo, &vp2, "readme update");

    assert_eq!(
        repo.read_bytes("assets/sprite.bin").unwrap(),
        expected_bytes,
        "binary asset must roundtrip bit-for-bit via CAS chunking"
    );
    assert!(repo.read_file("README.md").unwrap().contains("Updated by vp2."));
    assert!(
        has_any_file_recursive(&repo.root().join(".loom").join("cas")),
        ".loom/cas/ must store compressed CAS blobs/chunks"
    );
}

#[test]
fn tier1_f14_crdt_single_viewport_edit_preserves_formatting() {
    let initial_json = "{\n    \"z_last\": 1,\n    \"a_first\": 2\n}\n";
    let expected_json = "{\n    \"z_last\": 1,\n    \"a_first\": 99\n}\n";
    let initial_md = concat!(
        "# Notes\n",
        "\n",
        "- item 1\n",
        "\n",
        "```rust\n",
        "let x = 1;\n",
        "```\n",
    );
    let expected_md = concat!(
        "# Notes\n",
        "\n",
        "- item 1\n",
        "- item 2\n",
        "\n",
        "```rust\n",
        "let x = 1;\n",
        "```\n",
    );

    let repo = init_repo_with_files(&[
        ("config/pretty.json", initial_json),
        ("docs/NOTES.md", initial_md),
    ]);
    let vp1 = spawn_vp(&repo, "vp1", "preserve pretty json and markdown formatting");

    write_vp_file(&vp1, "config/pretty.json", expected_json);
    write_vp_file(&vp1, "docs/NOTES.md", expected_md);

    seal_vp(&repo, &vp1, "formatting-preserving crdt edit");

    assert_eq!(
        norm_lf(&repo.read_file("config/pretty.json").unwrap()),
        expected_json
    );
    assert_eq!(
        norm_lf(&repo.read_file("docs/NOTES.md").unwrap()),
        expected_md
    );
}
