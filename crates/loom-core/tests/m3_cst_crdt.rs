use loom_core::cas::{CasError, CasStore, is_binary_content};
use loom_core::crdt::CrdtError;
use loom_core::cst::{
    CstError, DiffContext, MergeState, MergedFileContent, VisibilityKind, collect_syntax_errors,
    compute_contract_deltas, compute_logical_file_id, detect_divergent_base_conflicts,
    diff_file_to_patch, diff_source_to_patch, diff_source_to_patch_strict, extract_decl_units,
    extract_file_symbols, flatten_decl_units, merge_patches, merge_trees, parse_source,
};
use loom_core::db::symbol_matches_query;
use loom_core::model::{
    Blake3Hash, LanguageKind, MergeOutcome, SemanticOp, SymbolContractDelta, SymbolStatus,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn make_ctx<'a>(
    path: &'a Path,
    language: LanguageKind,
    viewport_id: &'a str,
    actor_id: &'a str,
    lamport_ts: u64,
) -> DiffContext<'a> {
    DiffContext {
        logical_file_id: compute_logical_file_id(path),
        file_path: path,
        language,
        viewport_id,
        actor_id,
        lamport_ts,
        parents: Vec::new(),
    }
}

// ============================================================================
// F11: Atomic Leading Trivia Preservation (4 Languages + CRLF/LF + Guards)
// ============================================================================

#[test]
fn f11_rust_leading_trivia_crlf_inner_and_trailing_guards() {
    let src = "// License header\r\n// Copyright 2026\r\n\r\n//! Module inner doc\r\n#![allow(dead_code)]\r\nfn alpha() {} // trailing inline comment\r\n/// Doc line 1\r\n#[cfg(test)]\r\n/// Doc line 2\r\n#[inline]\r\nfn beta() {}\r\n";
    let fid = compute_logical_file_id(Path::new("src/lib.rs"));
    let parsed = parse_source(LanguageKind::Rust, src, None).unwrap();
    let units = extract_decl_units(LanguageKind::Rust, &parsed.tree, src, fid);

    let alpha = units.iter().find(|u| u.anchor.symbol_fqn == "alpha").unwrap();
    let alpha_text = &src[alpha.trivia_start_byte..alpha.end_byte];
    assert!(
        !alpha_text.contains("License header"),
        "blank-line separated license must NOT attach: {alpha_text:?}"
    );
    assert!(
        !alpha_text.contains("Module inner doc"),
        "inner doc //! must NOT attach: {alpha_text:?}"
    );
    assert!(
        !alpha_text.contains("#![allow(dead_code)]"),
        "inner attribute #![...] must NOT attach: {alpha_text:?}"
    );

    let beta = units.iter().find(|u| u.anchor.symbol_fqn == "beta").unwrap();
    let beta_text = &src[beta.trivia_start_byte..beta.end_byte];
    assert!(
        !beta_text.contains("trailing inline comment"),
        "same-row trailing comment on alpha must NOT be stolen by beta: {beta_text:?}"
    );
    assert!(beta_text.contains("/// Doc line 1"));
    assert!(beta_text.contains("#[cfg(test)]"));
    assert!(beta_text.contains("/// Doc line 2"));
    assert!(beta_text.contains("#[inline]"));
    assert!(beta_text.ends_with("fn beta() {}"));
}

#[test]
fn f11_ts_go_python_leading_trivia_preservation() {
    // TypeScript
    let ts_src = "// File license\n\nconst a = 1; // trailing\n/** Class doc */\n@sealed\nexport class Service {\n    // method comment\n    @log\n    run() {}\n}\n";
    let ts_fid = compute_logical_file_id(Path::new("src/svc.ts"));
    let ts_parsed = parse_source(LanguageKind::TypeScript, ts_src, None).unwrap();
    let ts_units = extract_decl_units(LanguageKind::TypeScript, &ts_parsed.tree, ts_src, ts_fid);
    let svc = ts_units.iter().find(|u| u.anchor.symbol_fqn == "Service").unwrap();
    let svc_text = &ts_src[svc.trivia_start_byte..svc.end_byte];
    assert!(!svc_text.contains("File license"));
    assert!(!svc_text.contains("trailing"));
    assert!(svc_text.contains("/** Class doc */"));
    assert!(svc_text.contains("@sealed"));
    let run_m = svc.children.iter().find(|u| u.anchor.symbol_fqn == "Service::run").unwrap();
    let run_text = &ts_src[run_m.trivia_start_byte..run_m.end_byte];
    assert!(run_text.contains("// method comment"));
    assert!(run_text.contains("@log"));

    // Go
    let go_src = "// Copyright\n\npackage main\n\nvar x = 1 // inline note\n// Handle processes requests.\nfunc Handle() {}\n";
    let go_fid = compute_logical_file_id(Path::new("main.go"));
    let go_parsed = parse_source(LanguageKind::Go, go_src, None).unwrap();
    let go_units = extract_decl_units(LanguageKind::Go, &go_parsed.tree, go_src, go_fid);
    let handle = go_units.iter().find(|u| u.anchor.symbol_fqn == "main::Handle").unwrap();
    let handle_text = &go_src[handle.trivia_start_byte..handle.end_byte];
    assert!(!handle_text.contains("Copyright"));
    assert!(!handle_text.contains("inline note"));
    assert!(handle_text.starts_with("// Handle processes requests.\nfunc Handle()"));

    // Python
    let py_src = "# File header\n\nx = 1  # inline comment\n# Compute value\n@lru_cache\n@trace\ndef compute(n: int) -> int:\n    return n\n";
    let py_fid = compute_logical_file_id(Path::new("app.py"));
    let py_parsed = parse_source(LanguageKind::Python, py_src, None).unwrap();
    let py_units = extract_decl_units(LanguageKind::Python, &py_parsed.tree, py_src, py_fid);
    let comp = py_units.iter().find(|u| u.anchor.symbol_fqn == "compute").unwrap();
    let comp_text = &py_src[comp.trivia_start_byte..comp.end_byte];
    assert!(!comp_text.contains("File header"));
    assert!(!comp_text.contains("inline comment"));
    assert!(comp_text.contains("# Compute value"));
    assert!(comp_text.contains("@lru_cache"));
    assert!(comp_text.contains("@trace"));
}

// ============================================================================
// F12: Line-Independent NodeAnchor, Wrapper Unwrapping & Disambiguation
// ============================================================================

#[test]
fn f12_node_anchor_invariant_after_50_line_shift_and_body_edit_all_4_langs() {
    let shift = "// filler line\n".repeat(50);

    // 1. Rust
    let rs_base = "pub fn target(a: i32) -> i32 {\n    a + 1\n}\n";
    let rs_shifted = format!("{shift}pub fn target(a: i32) -> i32 {{\n    a * 99\n}}\n");
    let rs_fid = compute_logical_file_id(Path::new("lib.rs"));
    let u1 = extract_decl_units(
        LanguageKind::Rust,
        &parse_source(LanguageKind::Rust, rs_base, None).unwrap().tree,
        rs_base,
        rs_fid,
    );
    let u2 = extract_decl_units(
        LanguageKind::Rust,
        &parse_source(LanguageKind::Rust, &rs_shifted, None).unwrap().tree,
        &rs_shifted,
        rs_fid,
    );
    assert_eq!(u1[0].anchor, u2[0].anchor);

    // 2. TypeScript + TSX
    let ts_base = "function render(x: number): number {\n    return x;\n}\n";
    let ts_shifted = format!("{shift}function render(x: number): number {{\n    return x + 42;\n}}\n");
    let ts_fid = compute_logical_file_id(Path::new("mod.tsx"));
    let t1 = extract_decl_units(
        LanguageKind::Tsx,
        &parse_source(LanguageKind::Tsx, ts_base, None).unwrap().tree,
        ts_base,
        ts_fid,
    );
    let t2 = extract_decl_units(
        LanguageKind::Tsx,
        &parse_source(LanguageKind::Tsx, &ts_shifted, None).unwrap().tree,
        &ts_shifted,
        ts_fid,
    );
    assert_eq!(t1[0].anchor, t2[0].anchor);

    // 3. Go
    let go_base = "package p\n\nfunc Calc(x int) int {\n\treturn x\n}\n";
    let go_shifted = format!("package p\n\n{shift}func Calc(x int) int {{\n\treturn x * 2\n}}\n");
    let go_fid = compute_logical_file_id(Path::new("calc.go"));
    let g1 = extract_decl_units(
        LanguageKind::Go,
        &parse_source(LanguageKind::Go, go_base, None).unwrap().tree,
        go_base,
        go_fid,
    );
    let g2 = extract_decl_units(
        LanguageKind::Go,
        &parse_source(LanguageKind::Go, &go_shifted, None).unwrap().tree,
        &go_shifted,
        go_fid,
    );
    assert_eq!(g1[0].anchor, g2[0].anchor);

    // 4. Python
    let py_shift = "# filler\n".repeat(50);
    let py_base = "def work(x: int) -> int:\n    return x\n";
    let py_shifted = format!("{py_shift}def work(x: int) -> int:\n    return x + 10\n");
    let py_fid = compute_logical_file_id(Path::new("work.py"));
    let p1 = extract_decl_units(
        LanguageKind::Python,
        &parse_source(LanguageKind::Python, py_base, None).unwrap().tree,
        py_base,
        py_fid,
    );
    let p2 = extract_decl_units(
        LanguageKind::Python,
        &parse_source(LanguageKind::Python, &py_shifted, None).unwrap().tree,
        &py_shifted,
        py_fid,
    );
    assert_eq!(p1[0].anchor, p2[0].anchor);
}

#[test]
fn f12_python_decorator_and_ts_export_decorator_unwrap_invariance() {
    // Python @decorator toggle keeps NodeAnchor identical
    let py_plain = "def greet(name: str) -> str:\n    return name\n";
    let py_dec = "@cache\n@log\ndef greet(name: str) -> str:\n    return name\n";
    let py_fid = compute_logical_file_id(Path::new("greet.py"));
    let pu1 = extract_decl_units(
        LanguageKind::Python,
        &parse_source(LanguageKind::Python, py_plain, None).unwrap().tree,
        py_plain,
        py_fid,
    );
    let pu2 = extract_decl_units(
        LanguageKind::Python,
        &parse_source(LanguageKind::Python, py_dec, None).unwrap().tree,
        py_dec,
        py_fid,
    );
    assert_eq!(pu1[0].anchor, pu2[0].anchor);

    // TypeScript export + @decorator toggle keeps NodeAnchor identical across all 3 forms
    let ts_plain = "class Greeter {\n    say() {}\n}\n";
    let ts_exp = "export class Greeter {\n    say() {}\n}\n";
    let ts_dec_exp = "@sealed\nexport class Greeter {\n    say() {}\n}\n";
    let ts_exp_dec = "export @sealed class Greeter {\n    say() {}\n}\n";
    let ts_fid = compute_logical_file_id(Path::new("greeter.ts"));
    let parse_ts = |s: &str| {
        extract_decl_units(
            LanguageKind::TypeScript,
            &parse_source(LanguageKind::TypeScript, s, None).unwrap().tree,
            s,
            ts_fid,
        )
    };
    let u_plain = parse_ts(ts_plain);
    let u_exp = parse_ts(ts_exp);
    let u_dec_exp = parse_ts(ts_dec_exp);
    let u_exp_dec = parse_ts(ts_exp_dec);
    assert_eq!(u_plain[0].anchor, u_exp[0].anchor);
    assert_eq!(u_plain[0].anchor, u_dec_exp[0].anchor);
    assert_eq!(u_plain[0].anchor, u_exp_dec[0].anchor);
}

#[test]
fn f12_rust_impl_go_receiver_and_ts_getter_setter_disambiguation() {
    // Rust impl disambiguation
    let rs_src = r#"
struct User;
trait Display {}
impl User { fn id(&self) -> u32 { 1 } }
impl User { fn extra(&self) -> u32 { 2 } }
impl Display for User {}
"#;
    let rs_fid = compute_logical_file_id(Path::new("user.rs"));
    let rs_units = extract_decl_units(
        LanguageKind::Rust,
        &parse_source(LanguageKind::Rust, rs_src, None).unwrap().tree,
        rs_src,
        rs_fid,
    );
    let fqns: Vec<&str> = rs_units.iter().map(|u| u.anchor.symbol_fqn.as_str()).collect();
    assert!(fqns.contains(&"impl<User>"));
    assert!(fqns.contains(&"impl<User>#2"));
    assert!(fqns.contains(&"impl<Display for User>"));
    let flat_rs = flatten_decl_units(&rs_units);
    assert!(flat_rs.iter().any(|u| u.anchor.symbol_fqn == "impl<User>::id"));
    assert!(flat_rs.iter().any(|u| u.anchor.symbol_fqn == "impl<User>#2::extra"));

    // Go receiver disambiguation
    let go_src = r#"
package srv
type Server struct{}
func (s *Server) Handle() {}
func (s Server) Handle() {}
"#;
    let go_fid = compute_logical_file_id(Path::new("srv.go"));
    let go_units = extract_decl_units(
        LanguageKind::Go,
        &parse_source(LanguageKind::Go, go_src, None).unwrap().tree,
        go_src,
        go_fid,
    );
    let go_fqns: Vec<&str> = go_units.iter().map(|u| u.anchor.symbol_fqn.as_str()).collect();
    assert!(go_fqns.contains(&"srv::(*Server).Handle"));
    assert!(go_fqns.contains(&"srv::(Server).Handle"));

    // TS getter/setter & arrow-const
    let ts_src = r#"
export class Box {
    get value(): number { return 1; }
    set value(v: number) {}
}
export const add = (a: number, b: number): number => {
    return a + b;
};
"#;
    let ts_fid = compute_logical_file_id(Path::new("box.ts"));
    let ts_units = extract_decl_units(
        LanguageKind::TypeScript,
        &parse_source(LanguageKind::TypeScript, ts_src, None).unwrap().tree,
        ts_src,
        ts_fid,
    );
    let flat_ts = flatten_decl_units(&ts_units);
    let ts_fqns: Vec<&str> = flat_ts.iter().map(|u| u.anchor.symbol_fqn.as_str()).collect();
    assert!(ts_fqns.contains(&"Box::get:value"));
    assert!(ts_fqns.contains(&"Box::set:value"));
    assert!(ts_fqns.contains(&"add"));
    let add_unit = flat_ts.iter().find(|u| u.anchor.symbol_fqn == "add").unwrap();
    assert!(add_unit.body_range.is_some());
}

// ============================================================================
// F13: Commutative & Associative Merging (All 4 Languages)
// ============================================================================

#[test]
fn f13_commutative_insert_member_all_4_languages() {
    // 1. Rust impl block
    let rs_path = Path::new("src/user.rs");
    let rs_base = "struct User;\n\nimpl User {\n    fn id(&self) -> u32 {\n        1\n    }\n}\n";
    let rs_a = "struct User;\n\nimpl User {\n    fn id(&self) -> u32 {\n        1\n    }\n\n    /// Agent A doc\n    fn name(&self) -> &str {\n        \"a\"\n    }\n}\n";
    let rs_b = "struct User;\n\nimpl User {\n    fn id(&self) -> u32 {\n        1\n    }\n\n    /// Agent B doc\n    fn age(&self) -> u8 {\n        30\n    }\n}\n";
    let p_rs_a = diff_source_to_patch(&make_ctx(rs_path, LanguageKind::Rust, "vp1", "a1", 10), rs_base, rs_a).unwrap();
    let p_rs_b = diff_source_to_patch(&make_ctx(rs_path, LanguageKind::Rust, "vp2", "a2", 20), rs_base, rs_b).unwrap();
    let mut base_map = BTreeMap::new();
    base_map.insert(rs_path.to_path_buf(), rs_base.as_bytes().to_vec());
    let r12 = merge_patches(&base_map, &[p_rs_a.clone(), p_rs_b.clone()], None).unwrap();
    let r21 = merge_patches(&base_map, &[p_rs_b, p_rs_a], None).unwrap();
    assert!(r12.is_clean(), "rust merge conflicts: {:?}", r12.conflicts);
    assert_eq!(r12.text(rs_path), r21.text(rs_path));
    let rs_merged = r12.text(rs_path).unwrap();
    assert!(rs_merged.contains("/// Agent A doc\n    fn name"));
    assert!(rs_merged.contains("/// Agent B doc\n    fn age"));

    // 2. TypeScript class
    let ts_path = Path::new("src/svc.ts");
    let ts_base = "export class Svc {\n    ping(): string {\n        return 'pong';\n    }\n}\n";
    let ts_a = "export class Svc {\n    ping(): string {\n        return 'pong';\n    }\n\n    alpha(): number {\n        return 1;\n    }\n}\n";
    let ts_b = "export class Svc {\n    ping(): string {\n        return 'pong';\n    }\n\n    beta(): number {\n        return 2;\n    }\n}\n";
    let p_ts_a = diff_source_to_patch(&make_ctx(ts_path, LanguageKind::TypeScript, "vp1", "a1", 1), ts_base, ts_a).unwrap();
    let p_ts_b = diff_source_to_patch(&make_ctx(ts_path, LanguageKind::TypeScript, "vp2", "a2", 2), ts_base, ts_b).unwrap();
    let mut ts_map = BTreeMap::new();
    ts_map.insert(ts_path.to_path_buf(), ts_base.as_bytes().to_vec());
    let tr12 = merge_patches(&ts_map, &[p_ts_a.clone(), p_ts_b.clone()], None).unwrap();
    let tr21 = merge_patches(&ts_map, &[p_ts_b, p_ts_a], None).unwrap();
    assert!(tr12.is_clean(), "ts merge conflicts: {:?}", tr12.conflicts);
    assert_eq!(tr12.text(ts_path), tr21.text(ts_path));
    assert!(tr12.text(ts_path).unwrap().contains("alpha()"));
    assert!(tr12.text(ts_path).unwrap().contains("beta()"));

    // 3. Go file
    let go_path = Path::new("srv.go");
    let go_base = "package main\n\nfunc Base() int {\n\treturn 0\n}\n";
    let go_a = "package main\n\nfunc Base() int {\n\treturn 0\n}\n\nfunc Alpha() int {\n\treturn 1\n}\n";
    let go_b = "package main\n\nfunc Base() int {\n\treturn 0\n}\n\nfunc Beta() int {\n\treturn 2\n}\n";
    let p_go_a = diff_source_to_patch(&make_ctx(go_path, LanguageKind::Go, "vp1", "a1", 5), go_base, go_a).unwrap();
    let p_go_b = diff_source_to_patch(&make_ctx(go_path, LanguageKind::Go, "vp2", "a2", 6), go_base, go_b).unwrap();
    let mut go_map = BTreeMap::new();
    go_map.insert(go_path.to_path_buf(), go_base.as_bytes().to_vec());
    let gr12 = merge_patches(&go_map, &[p_go_a.clone(), p_go_b.clone()], None).unwrap();
    let gr21 = merge_patches(&go_map, &[p_go_b, p_go_a], None).unwrap();
    assert!(gr12.is_clean(), "go merge conflicts: {:?}", gr12.conflicts);
    assert_eq!(gr12.text(go_path), gr21.text(go_path));
    assert!(gr12.text(go_path).unwrap().contains("func Alpha()"));
    assert!(gr12.text(go_path).unwrap().contains("func Beta()"));

    // 4. Python class
    let py_path = Path::new("app.py");
    let py_base = "class App:\n    def base(self) -> int:\n        return 0\n";
    let py_a = "class App:\n    def base(self) -> int:\n        return 0\n\n    @staticmethod\n    def one() -> int:\n        return 1\n";
    let py_b = "class App:\n    def base(self) -> int:\n        return 0\n\n    def two(self) -> int:\n        return 2\n";
    let p_py_a = diff_source_to_patch(&make_ctx(py_path, LanguageKind::Python, "vp1", "a1", 1), py_base, py_a).unwrap();
    let p_py_b = diff_source_to_patch(&make_ctx(py_path, LanguageKind::Python, "vp2", "a2", 2), py_base, py_b).unwrap();
    let mut py_map = BTreeMap::new();
    py_map.insert(py_path.to_path_buf(), py_base.as_bytes().to_vec());
    let pr12 = merge_patches(&py_map, &[p_py_a.clone(), p_py_b.clone()], None).unwrap();
    let pr21 = merge_patches(&py_map, &[p_py_b, p_py_a], None).unwrap();
    assert!(pr12.is_clean(), "py merge conflicts: {:?}", pr12.conflicts);
    assert_eq!(pr12.text(py_path), pr21.text(py_path));
    assert!(pr12.text(py_path).unwrap().contains("@staticmethod\n    def one"));
    assert!(pr12.text(py_path).unwrap().contains("def two"));
}

#[test]
fn f13_rust_comma_separated_struct_and_enum_insert_member() {
    let path = Path::new("src/types.rs");
    let base = "pub struct Config {\n    pub host: String,\n    pub port: u16\n}\n\npub enum Mode {\n    Fast,\n    Safe\n}\n";
    let mod1 = "pub struct Config {\n    pub host: String,\n    pub port: u16,\n    pub tls: bool\n}\n\npub enum Mode {\n    Fast,\n    Safe\n}\n";
    let mod2 = "pub struct Config {\n    pub host: String,\n    pub port: u16,\n    pub workers: usize\n}\n\npub enum Mode {\n    Fast,\n    Safe,\n    Balanced\n}\n";

    let p1 = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp1", "a1", 1), base, mod1).unwrap();
    let p2 = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp2", "a2", 2), base, mod2).unwrap();

    let mut base_map = BTreeMap::new();
    base_map.insert(path.to_path_buf(), base.as_bytes().to_vec());

    let r12 = merge_patches(&base_map, &[p1.clone(), p2.clone()], None).unwrap();
    let r21 = merge_patches(&base_map, &[p2, p1], None).unwrap();
    assert!(r12.is_clean(), "comma container conflicts: {:?}", r12.conflicts);
    assert_eq!(r12.text(path), r21.text(path));
    let merged = r12.text(path).unwrap();
    assert!(merged.contains("pub tls: bool"));
    assert!(merged.contains("pub workers: usize"));
    assert!(merged.contains("Balanced"));
}

#[test]
fn f13_three_patch_permutations_and_merge_state_compose_associative() {
    let path = Path::new("src/lib.rs");
    let base = "fn seed() {}\n";
    let m1 = "fn first() {}\n\nfn seed() {}\n";
    let m2 = "fn second() {}\n\nfn seed() {}\n";
    let m3 = "fn third() {}\n\nfn seed() {}\n";

    // All 3 insert at after_anchor: None at file root
    let p1 = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp1", "a1", 10), base, m1).unwrap();
    let p2 = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp2", "a2", 20), base, m2).unwrap();
    let p3 = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp3", "a3", 30), base, m3).unwrap();

    let mut base_map = BTreeMap::new();
    base_map.insert(path.to_path_buf(), base.as_bytes().to_vec());

    let perms = [
        [&p1, &p2, &p3],
        [&p1, &p3, &p2],
        [&p2, &p1, &p3],
        [&p2, &p3, &p1],
        [&p3, &p1, &p2],
        [&p3, &p2, &p1],
    ];
    let baseline = merge_patches(&base_map, &[p1.clone(), p2.clone(), p3.clone()], None).unwrap();
    assert!(baseline.is_clean());
    for perm in perms {
        let r = merge_patches(&base_map, &[perm[0].clone(), perm[1].clone(), perm[2].clone()], None).unwrap();
        assert_eq!(r.text(path), baseline.text(path));
    }

    // MergeState::compose associativity: (P1 ⊕ P2) ⊕ P3 == P1 ⊕ (P2 ⊕ P3)
    let s1 = MergeState::from_patches(base_map.clone(), &[p1], None).unwrap();
    let s2 = MergeState::from_patches(base_map.clone(), &[p2], None).unwrap();
    let s3 = MergeState::from_patches(base_map, &[p3], None).unwrap();

    let left = MergeState::compose(&MergeState::compose(&s1, &s2, None).unwrap(), &s3, None).unwrap();
    let right = MergeState::compose(&s1, &MergeState::compose(&s2, &s3, None).unwrap(), None).unwrap();
    assert!(left.report.is_clean());
    assert_eq!(left.report.text(path), right.report.text(path));
    assert_eq!(left.report.text(path), baseline.text(path));
}

#[test]
fn f13_shifted_replace_body_mutate_signature_and_insert_plus_remove() {
    // ReplaceBody commutes across shifted lines + MutateSignature + ReplaceBody on same symbol
    let path = Path::new("src/calc.rs");
    let base = "fn alpha(x: i32) -> i32 {\n    x + 1\n}\n\nfn beta(y: i32) -> i32 {\n    y + 2\n}\n";
    let p1_src = "// header 1\n// header 2\nfn inserted_top() {}\n\npub fn alpha(x: i32) -> i32 {\n    x + 1\n}\n\nfn beta(y: i32) -> i32 {\n    y + 2\n}\n";
    let p2_src = "fn alpha(x: i32) -> i32 {\n    x * 10\n}\n\nfn beta(y: i32) -> i32 {\n    y * 20\n}\n";

    let p1 = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp1", "a1", 1), base, p1_src).unwrap();
    let p2 = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp2", "a2", 2), base, p2_src).unwrap();

    let mut base_map = BTreeMap::new();
    base_map.insert(path.to_path_buf(), base.as_bytes().to_vec());

    let r12 = merge_patches(&base_map, &[p1.clone(), p2.clone()], None).unwrap();
    let r21 = merge_patches(&base_map, &[p2, p1], None).unwrap();
    assert!(r12.is_clean(), "conflicts: {:?}", r12.conflicts);
    assert_eq!(r12.text(path), r21.text(path));
    let merged = r12.text(path).unwrap();
    assert!(merged.contains("pub fn alpha(x: i32) -> i32 {\n    x * 10\n}"));
    assert!(merged.contains("fn beta(y: i32) -> i32 {\n    y * 20\n}"));
    assert!(merged.contains("fn inserted_top() {}"));

    // InsertMember (after B) + RemoveNode(B) commutes via Phase C before Phase D
    let base_rm = "fn a() {}\n\nfn b() {}\n\nfn c() {}\n";
    let ins_after_b = "fn a() {}\n\nfn b() {}\n\nfn x() {}\n\nfn c() {}\n";
    let rem_b = "fn a() {}\n\nfn c() {}\n";
    let pi = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp1", "a1", 1), base_rm, ins_after_b).unwrap();
    let pr = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp2", "a2", 2), base_rm, rem_b).unwrap();
    let mut rm_map = BTreeMap::new();
    rm_map.insert(path.to_path_buf(), base_rm.as_bytes().to_vec());
    let rm12 = merge_patches(&rm_map, &[pi.clone(), pr.clone()], None).unwrap();
    let rm21 = merge_patches(&rm_map, &[pr, pi], None).unwrap();
    assert!(rm12.is_clean());
    assert_eq!(rm12.text(path), rm21.text(path));
    let rm_text = rm12.text(path).unwrap();
    assert!(rm_text.contains("fn x() {}"));
    assert!(!rm_text.contains("fn b() {}"));
}

#[test]
fn f13_syntax_error_strict_vs_fallback_and_pending_syntax() {
    let path = Path::new("src/broken.rs");
    let base = "fn ok_fn() -> i32 { 1 }\n";
    let broken = "fn ok_fn() -> i32 { helper(1) }\nfn incomplete( {\n";

    let ctx = make_ctx(path, LanguageKind::Rust, "vp1", "a1", 1);
    assert!(diff_source_to_patch_strict(&ctx, base, broken).is_err());

    let fallback_patch = diff_source_to_patch(&ctx, base, broken).unwrap();
    assert!(matches!(
        fallback_patch.ops.first(),
        Some(SemanticOp::TextCrdtFallback { .. })
    ));

    let parsed = parse_source(LanguageKind::Rust, broken, None).unwrap();
    let syms = extract_file_symbols(&parsed, ctx.logical_file_id, path, "vp1", 0);
    assert!(syms.pending_syntax);
    let ok_prov = syms.provides.iter().find(|p| p.anchor.symbol_fqn == "ok_fn").unwrap();
    assert_eq!(ok_prov.status, SymbolStatus::Ready);
    let ok_consume = syms.consumes.iter().find(|c| c.symbol_ref == "helper").unwrap();
    assert!(!ok_consume.in_error_subtree);
}

// ============================================================================
// F13 & F14: StructuralConflict & SemanticConflict Matrix
// ============================================================================

#[test]
fn f13_structural_conflict_matrix_symmetric() {
    let path = Path::new("src/mod.rs");
    let base = "pub mod m {\n    pub fn f() -> i32 { 1 }\n}\n";
    let mut base_map = BTreeMap::new();
    base_map.insert(path.to_path_buf(), base.as_bytes().to_vec());

    // 1. Incompatible ReplaceBody
    let b1 = "pub mod m {\n    pub fn f() -> i32 { 2 }\n}\n";
    let b2 = "pub mod m {\n    pub fn f() -> i32 { 3 }\n}\n";
    let p1 = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp1", "a1", 1), base, b1).unwrap();
    let p2 = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp2", "a2", 2), base, b2).unwrap();
    let r12 = merge_patches(&base_map, &[p1.clone(), p2.clone()], None).unwrap();
    let r21 = merge_patches(&base_map, &[p2, p1], None).unwrap();
    assert!(matches!(r12.outcome, MergeOutcome::StructuralConflict { .. }));
    assert_eq!(r12.outcome, r21.outcome);

    // 2. Incompatible MutateSignature
    let s1 = "pub mod m {\n    pub fn f(a: i32) -> i32 { 1 }\n}\n";
    let s2 = "pub mod m {\n    pub fn f(a: u64) -> i32 { 1 }\n}\n";
    let ps1 = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp1", "a1", 1), base, s1).unwrap();
    let ps2 = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp2", "a2", 2), base, s2).unwrap();
    assert!(matches!(
        merge_patches(&base_map, &[ps1, ps2], None).unwrap().outcome,
        MergeOutcome::StructuralConflict { .. }
    ));

    // 3. RemoveNode vs ReplaceBody
    let r_rem = "pub mod m {\n}\n";
    let pr = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp1", "a1", 1), base, r_rem).unwrap();
    let pb = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp2", "a2", 2), base, b1).unwrap();
    assert!(matches!(
        merge_patches(&base_map, &[pr, pb], None).unwrap().outcome,
        MergeOutcome::StructuralConflict { .. }
    ));

    // 4. RemoveNode on container vs InsertMember inside it
    let rm_mod = "";
    let ins_in_mod = "pub mod m {\n    pub fn f() -> i32 { 1 }\n\n    pub fn g() -> i32 { 2 }\n}\n";
    let p_rm_mod = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp1", "a1", 1), base, rm_mod).unwrap();
    let p_ins_mod = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp2", "a2", 2), base, ins_in_mod).unwrap();
    assert!(matches!(
        merge_patches(&base_map, &[p_rm_mod, p_ins_mod], None).unwrap().outcome,
        MergeOutcome::StructuralConflict { .. }
    ));

    // 5. Duplicate same-FQN InsertMember with different content (vs identical content -> Clean)
    let ins_g1 = "pub mod m {\n    pub fn f() -> i32 { 1 }\n\n    pub fn g() -> i32 { 10 }\n}\n";
    let ins_g2 = "pub mod m {\n    pub fn f() -> i32 { 1 }\n\n    pub fn g() -> i32 { 20 }\n}\n";
    let pg1 = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp1", "a1", 1), base, ins_g1).unwrap();
    let pg2 = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp2", "a2", 2), base, ins_g2).unwrap();
    assert!(matches!(
        merge_patches(&base_map, &[pg1.clone(), pg2], None).unwrap().outcome,
        MergeOutcome::StructuralConflict { .. }
    ));
    let pg1_dup = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp2", "a2", 2), base, ins_g1).unwrap();
    assert!(merge_patches(&base_map, &[pg1, pg1_dup], None).unwrap().is_clean());

    // 6. Conflicting RenameOrMoveFile
    let ren1 = diff_file_to_patch(
        &make_ctx(Path::new("src/new1.rs"), LanguageKind::Rust, "vp1", "a1", 1),
        base.as_bytes(),
        base.as_bytes(),
        Some(path),
        None,
    )
    .unwrap();
    let mut ren2 = diff_file_to_patch(
        &make_ctx(Path::new("src/new2.rs"), LanguageKind::Rust, "vp2", "a2", 2),
        base.as_bytes(),
        base.as_bytes(),
        Some(path),
        None,
    )
    .unwrap();
    ren2.logical_file_id = ren1.logical_file_id;
    ren2.finalize_hash();
    assert!(matches!(
        merge_patches(&base_map, &[ren1, ren2], None).unwrap().outcome,
        MergeOutcome::StructuralConflict { .. }
    ));

    // 7. Divergent BinaryManifest
    let dir = TempDir::new().unwrap();
    let cas = CasStore::open_default(dir.path().join("cas")).unwrap();
    let bin_path = Path::new("logo.png");
    let bin_base = b"\x89PNG\r\n\x1a\nbase".to_vec();
    let mut bin_map = BTreeMap::new();
    bin_map.insert(bin_path.to_path_buf(), bin_base.clone());
    let pb1 = diff_file_to_patch(
        &make_ctx(bin_path, LanguageKind::Binary, "vp1", "a1", 1),
        &bin_base,
        b"\x89PNG\r\n\x1a\nv1",
        None,
        Some(&cas),
    )
    .unwrap();
    let pb2 = diff_file_to_patch(
        &make_ctx(bin_path, LanguageKind::Binary, "vp2", "a2", 2),
        &bin_base,
        b"\x89PNG\r\n\x1a\nv2",
        None,
        Some(&cas),
    )
    .unwrap();
    assert!(matches!(
        merge_patches(&bin_map, &[pb1, pb2], Some(&cas)).unwrap().outcome,
        MergeOutcome::StructuralConflict { .. }
    ));

    // 8. Mixed TextCrdtFallback vs CST SemanticOp on same file
    let mixed_cst = diff_source_to_patch(
        &make_ctx(path, LanguageKind::Rust, "vp1", "a1", 1),
        base,
        b1,
    )
    .unwrap();
    let mixed_crdt = diff_source_to_patch(
        &make_ctx(path, LanguageKind::Rust, "vp2", "a2", 2),
        base,
        "pub mod m { broken( ",
    )
    .unwrap();
    assert!(matches!(
        merge_patches(&base_map, &[mixed_cst, mixed_crdt], None)
            .unwrap()
            .outcome,
        MergeOutcome::StructuralConflict { .. }
    ));
}

#[test]
fn f13_semantic_conflict_single_file_and_cross_file() {
    // 1. Single-file SemanticConflict: P1 changes helper signature, P2 adds caller using old arity
    let lib_path = Path::new("src/lib.rs");
    let lib_base = "pub fn helper(x: i32) -> i32 {\n    x + 1\n}\n\npub fn caller() -> i32 {\n    helper(1)\n}\n";
    let lib_p1 = "pub fn helper(x: i32, y: i32) -> i32 {\n    x + y\n}\n\npub fn caller() -> i32 {\n    helper(1)\n}\n";
    let lib_p2 = "pub fn helper(x: i32) -> i32 {\n    x + 1\n}\n\npub fn caller() -> i32 {\n    helper(42)\n}\n";

    let p1 = diff_source_to_patch(&make_ctx(lib_path, LanguageKind::Rust, "vp1", "a1", 1), lib_base, lib_p1).unwrap();
    let p2 = diff_source_to_patch(&make_ctx(lib_path, LanguageKind::Rust, "vp2", "a2", 2), lib_base, lib_p2).unwrap();

    let mut single_map = BTreeMap::new();
    single_map.insert(lib_path.to_path_buf(), lib_base.as_bytes().to_vec());

    let res12 = merge_patches(&single_map, &[p1.clone(), p2.clone()], None).unwrap();
    let res21 = merge_patches(&single_map, &[p2, p1], None).unwrap();
    assert!(
        matches!(res12.outcome, MergeOutcome::SemanticConflict { ref symbol_fqn, .. } if symbol_fqn == "helper"),
        "expected SemanticConflict on helper, got {:?}",
        res12.outcome
    );
    assert_eq!(res12.outcome, res21.outcome);

    // 2. Cross-file SemanticConflict: api.ts reduces visibility or removes symbol, app.ts consumes it
    let api_path = Path::new("src/api.ts");
    let app_path = Path::new("src/app.ts");
    let api_base = "export function fetchData(id: number): string {\n    return String(id);\n}\n";
    let app_base = "import { fetchData } from './api';\nexport function run(): string {\n    return fetchData(1);\n}\n";

    let api_mod = "function fetchData(id: number): string {\n    return String(id);\n}\n";
    let app_mod = "import { fetchData } from './api';\nexport function run(): string {\n    return fetchData(99);\n}\n";

    let pa = diff_source_to_patch(&make_ctx(api_path, LanguageKind::TypeScript, "vp1", "a1", 1), api_base, api_mod).unwrap();
    let pb = diff_source_to_patch(&make_ctx(app_path, LanguageKind::TypeScript, "vp2", "a2", 2), app_base, app_mod).unwrap();

    let mut cross_map = BTreeMap::new();
    cross_map.insert(api_path.to_path_buf(), api_base.as_bytes().to_vec());
    cross_map.insert(app_path.to_path_buf(), app_base.as_bytes().to_vec());

    let cross_res = merge_patches(&cross_map, &[pa.clone(), pb], None).unwrap();
    assert!(
        matches!(cross_res.outcome, MergeOutcome::SemanticConflict { ref symbol_fqn, .. } if symbol_fqn == "fetchData"),
        "expected cross-file SemanticConflict on fetchData, got {:?}",
        cross_res.outcome
    );

    // 3. merge_trees catches SemanticConflict even when consumer file is untouched in base_files
    let tree_res = merge_trees(&cross_map, &[&[pa]], None).unwrap();
    assert!(matches!(
        tree_res.outcome,
        MergeOutcome::SemanticConflict { .. }
    ));
}

#[test]
fn f14_file_diff_and_merge_for_crdt_and_binary() {
    let dir = TempDir::new().unwrap();
    let cas = CasStore::open_default(dir.path().join("cas")).unwrap();

    let json_path = PathBuf::from("settings.json");
    let json_base = b"{\n  \"alpha\": 1,\n  \"beta\": 2\n}\n".to_vec();
    let json_a = b"{\n  \"alpha\": 100,\n  \"beta\": 2\n}\n".to_vec();
    let json_b = b"{\n  \"alpha\": 1,\n  \"beta\": 200\n}\n".to_vec();

    let pa = diff_file_to_patch(
        &make_ctx(&json_path, LanguageKind::Json, "vp1", "a1", 1),
        &json_base,
        &json_a,
        None,
        Some(&cas),
    )
    .unwrap();
    let pb = diff_file_to_patch(
        &make_ctx(&json_path, LanguageKind::Json, "vp2", "a2", 2),
        &json_base,
        &json_b,
        None,
        Some(&cas),
    )
    .unwrap();

    let mut base_files = BTreeMap::new();
    base_files.insert(json_path.clone(), json_base);

    let report = merge_patches(&base_files, &[pa, pb], Some(&cas)).unwrap();
    assert!(report.is_clean(), "conflicts: {:?}", report.conflicts);
    let merged_json: serde_json::Value =
        serde_json::from_str(report.text(&json_path).unwrap()).unwrap();
    assert_eq!(merged_json["alpha"], 100);
    assert_eq!(merged_json["beta"], 200);

    // Binary file diff & clean identical merge
    let bin_path = PathBuf::from("blob.bin");
    let bin_base = vec![1u8; 70_000];
    let bin_new = vec![2u8; 70_000];
    let pbin1 = diff_file_to_patch(
        &make_ctx(&bin_path, LanguageKind::Binary, "vp1", "a1", 1),
        &bin_base,
        &bin_new,
        None,
        Some(&cas),
    )
    .unwrap();
    let pbin2 = diff_file_to_patch(
        &make_ctx(&bin_path, LanguageKind::Binary, "vp2", "a2", 2),
        &bin_base,
        &bin_new,
        None,
        Some(&cas),
    )
    .unwrap();
    let mut bin_base_map = BTreeMap::new();
    bin_base_map.insert(bin_path.clone(), bin_base);
    let bin_report = merge_patches(&bin_base_map, &[pbin1, pbin2], Some(&cas)).unwrap();
    assert!(bin_report.is_clean());
    let restored = match bin_report.merged_files.get(&bin_path).unwrap() {
        MergedFileContent::Binary(m) => cas.get_chunked_binary(m).unwrap(),
        _ => panic!("expected binary"),
    };
    assert_eq!(restored, bin_new);
}

#[test]
fn f13_python_decorator_plus_body_edit_go_root_insert_and_tsx_merge() {
    // 1. Python @decorator addition (MutateSignature) + ReplaceBody on same function
    let py_path = Path::new("calc.py");
    let py_base = "def calc(x: int) -> int:\n    return x + 1\n";
    let py_dec = "@lru_cache\ndef calc(x: int) -> int:\n    return x + 1\n";
    let py_body = "def calc(x: int) -> int:\n    return x * 42\n";
    let p_dec = diff_source_to_patch(&make_ctx(py_path, LanguageKind::Python, "vp1", "a1", 1), py_base, py_dec).unwrap();
    let p_body = diff_source_to_patch(&make_ctx(py_path, LanguageKind::Python, "vp2", "a2", 2), py_base, py_body).unwrap();
    let mut py_map = BTreeMap::new();
    py_map.insert(py_path.to_path_buf(), py_base.as_bytes().to_vec());
    let pr12 = merge_patches(&py_map, &[p_dec.clone(), p_body.clone()], None).unwrap();
    let pr21 = merge_patches(&py_map, &[p_body, p_dec], None).unwrap();
    assert!(pr12.is_clean(), "py dec+body conflicts: {:?}", pr12.conflicts);
    assert_eq!(pr12.text(py_path), pr21.text(py_path));
    assert_eq!(
        pr12.text(py_path).unwrap(),
        "@lru_cache\ndef calc(x: int) -> int:\n    return x * 42\n"
    );

    // 2. Go file-root InsertMember (after_anchor: None) keeps package clause first
    let go_path = Path::new("pkg.go");
    let go_base = "package app\n\nfunc Tail() {}\n";
    let go_a = "package app\n\nfunc FirstA() {}\n\nfunc Tail() {}\n";
    let go_b = "package app\n\nfunc FirstB() {}\n\nfunc Tail() {}\n";
    let ga = diff_source_to_patch(&make_ctx(go_path, LanguageKind::Go, "vp1", "a1", 1), go_base, go_a).unwrap();
    let gb = diff_source_to_patch(&make_ctx(go_path, LanguageKind::Go, "vp2", "a2", 2), go_base, go_b).unwrap();
    let mut go_map = BTreeMap::new();
    go_map.insert(go_path.to_path_buf(), go_base.as_bytes().to_vec());
    let gr12 = merge_patches(&go_map, &[ga.clone(), gb.clone()], None).unwrap();
    let gr21 = merge_patches(&go_map, &[gb, ga], None).unwrap();
    assert!(gr12.is_clean(), "go root insert conflicts: {:?}", gr12.conflicts);
    assert_eq!(gr12.text(go_path), gr21.text(go_path));
    let go_merged = gr12.text(go_path).unwrap();
    assert!(go_merged.starts_with("package app"));
    assert!(go_merged.contains("func FirstA()"));
    assert!(go_merged.contains("func FirstB()"));

    // 3. TSX component merge
    let tsx_path = Path::new("Button.tsx");
    let tsx_base = "export function Primary(): JSX.Element {\n    return <button>Ok</button>;\n}\n";
    let tsx_a = "export function Primary(): JSX.Element {\n    return <button className=\"primary\">Save</button>;\n}\n";
    let tsx_b = "export function Primary(): JSX.Element {\n    return <button>Ok</button>;\n}\n\nexport function Secondary(): JSX.Element {\n    return <button>Cancel</button>;\n}\n";
    let ta = diff_source_to_patch(&make_ctx(tsx_path, LanguageKind::Tsx, "vp1", "a1", 1), tsx_base, tsx_a).unwrap();
    let tb = diff_source_to_patch(&make_ctx(tsx_path, LanguageKind::Tsx, "vp2", "a2", 2), tsx_base, tsx_b).unwrap();
    let mut tsx_map = BTreeMap::new();
    tsx_map.insert(tsx_path.to_path_buf(), tsx_base.as_bytes().to_vec());
    let tr12 = merge_patches(&tsx_map, &[ta.clone(), tb.clone()], None).unwrap();
    let tr21 = merge_patches(&tsx_map, &[tb, ta], None).unwrap();
    assert!(tr12.is_clean(), "tsx conflicts: {:?}", tr12.conflicts);
    assert_eq!(tr12.text(tsx_path), tr21.text(tsx_path));
    let tsx_merged = tr12.text(tsx_path).unwrap();
    assert!(tsx_merged.contains("Save"));
    assert!(tsx_merged.contains("Secondary"));
}

// ============================================================================
// Iteration 2 Remediation Regression Tests (Findings 1–9)
// ============================================================================

#[test]
fn r2_crlf_single_and_multi_line_comment_trivia_all_languages() {
    // 1. Rust CRLF: single //, multi //, ///, /* */ attach; blank \r\n\r\n detaches
    let rs_src = "// header\r\n\r\n// single doc\r\n// line 2\r\nfn alpha() {}\r\n/// triple doc\r\nfn beta() {}\r\n/* block doc */\r\nfn gamma() {}\r\n";
    let rs_fid = compute_logical_file_id(Path::new("src/crlf.rs"));
    let rs_units = extract_decl_units(
        LanguageKind::Rust,
        &parse_source(LanguageKind::Rust, rs_src, None).unwrap().tree,
        rs_src,
        rs_fid,
    );
    let alpha = &rs_units[0];
    let alpha_slice = &rs_src[alpha.trivia_start_byte..alpha.end_byte];
    assert!(!alpha_slice.contains("header"));
    assert!(alpha_slice.starts_with("// single doc\r\n// line 2\r\nfn alpha() {}"));
    let beta_slice = &rs_src[rs_units[1].trivia_start_byte..rs_units[1].end_byte];
    assert!(beta_slice.starts_with("/// triple doc\r\nfn beta() {}"));
    let gamma_slice = &rs_src[rs_units[2].trivia_start_byte..rs_units[2].end_byte];
    assert!(gamma_slice.starts_with("/* block doc */\r\nfn gamma() {}"));

    // 2. TypeScript & TSX CRLF
    let ts_src = "// header\r\n\r\n// ts doc\r\nfunction foo(): void {}\r\n";
    let ts_fid = compute_logical_file_id(Path::new("src/crlf.ts"));
    let ts_units = extract_decl_units(
        LanguageKind::TypeScript,
        &parse_source(LanguageKind::TypeScript, ts_src, None).unwrap().tree,
        ts_src,
        ts_fid,
    );
    assert!(ts_src[ts_units[0].trivia_start_byte..ts_units[0].end_byte].starts_with("// ts doc\r\nfunction foo"));

    let tsx_fid = compute_logical_file_id(Path::new("src/crlf.tsx"));
    let tsx_units = extract_decl_units(
        LanguageKind::Tsx,
        &parse_source(LanguageKind::Tsx, ts_src, None).unwrap().tree,
        ts_src,
        tsx_fid,
    );
    assert!(ts_src[tsx_units[0].trivia_start_byte..tsx_units[0].end_byte].starts_with("// ts doc\r\nfunction foo"));

    // 3. Go CRLF (single & multi // + RemoveNode does NOT leave orphan comment)
    let go_path = Path::new("pkg/main.go");
    let go_src = "package p\r\n\r\n// Target doc\r\n// second line\r\nfunc Target() int {\r\n\treturn 1\r\n}\r\n\r\n/* Keep doc */\r\nfunc Keep() int {\r\n\treturn 2\r\n}\r\n";
    let go_fid = compute_logical_file_id(go_path);
    let go_units = extract_decl_units(
        LanguageKind::Go,
        &parse_source(LanguageKind::Go, go_src, None).unwrap().tree,
        go_src,
        go_fid,
    );
    let target = &go_units[0];
    assert!(
        go_src[target.trivia_start_byte..target.end_byte].starts_with("// Target doc\r\n// second line\r\nfunc Target()"),
        "go CRLF trivia failed: {:?}",
        &go_src[target.trivia_start_byte..target.end_byte]
    );
    assert!(go_src[go_units[1].trivia_start_byte..go_units[1].end_byte].starts_with("/* Keep doc */"));

    let go_removed = "package p\r\n\r\n/* Keep doc */\r\nfunc Keep() int {\r\n\treturn 2\r\n}\r\n";
    let p_rm = diff_source_to_patch(&make_ctx(go_path, LanguageKind::Go, "vp1", "a1", 1), go_src, go_removed).unwrap();
    let mut go_map = BTreeMap::new();
    go_map.insert(go_path.to_path_buf(), go_src.as_bytes().to_vec());
    let go_merged = merge_patches(&go_map, &[p_rm], None).unwrap();
    assert!(go_merged.is_clean());
    assert!(!go_merged.text(go_path).unwrap().contains("Target doc"));

    // 4. Python CRLF (single & multi #)
    let py_src = "# file header\r\n\r\n# line 1\r\n# line 2\r\ndef foo():\r\n    pass\r\n";
    let py_fid = compute_logical_file_id(Path::new("crlf.py"));
    let py_units = extract_decl_units(
        LanguageKind::Python,
        &parse_source(LanguageKind::Python, py_src, None).unwrap().tree,
        py_src,
        py_fid,
    );
    let py_slice = &py_src[py_units[0].trivia_start_byte..py_units[0].end_byte];
    assert!(!py_slice.contains("file header"));
    assert!(py_slice.starts_with("# line 1\r\n# line 2\r\ndef foo():"));
}

#[test]
fn r2_container_remove_vs_descendant_edit_and_duplicate_rust_impl() {
    // 1. Rust mod removal vs child ReplaceBody & MutateSignature -> StructuralConflict
    let rs_path = Path::new("src/m.rs");
    let rs_base = "pub mod m {\n    pub fn f() -> i32 { 1 }\n}\n\npub fn keep() {}\n";
    let rs_rm = "pub fn keep() {}\n";
    let rs_body = "pub mod m {\n    pub fn f() -> i32 { 999 }\n}\n\npub fn keep() {}\n";
    let rs_sig = "pub mod m {\n    pub fn f(x: i32) -> i32 { 1 }\n}\n\npub fn keep() {}\n";

    let p_rm = diff_source_to_patch(&make_ctx(rs_path, LanguageKind::Rust, "vp1", "a1", 1), rs_base, rs_rm).unwrap();
    let p_body = diff_source_to_patch(&make_ctx(rs_path, LanguageKind::Rust, "vp2", "a2", 2), rs_base, rs_body).unwrap();
    let p_sig = diff_source_to_patch(&make_ctx(rs_path, LanguageKind::Rust, "vp3", "a3", 3), rs_base, rs_sig).unwrap();
    let mut rs_map = BTreeMap::new();
    rs_map.insert(rs_path.to_path_buf(), rs_base.as_bytes().to_vec());

    assert!(matches!(
        merge_patches(&rs_map, &[p_rm.clone(), p_body.clone()], None).unwrap().outcome,
        MergeOutcome::StructuralConflict { .. }
    ));
    assert!(matches!(
        merge_patches(&rs_map, &[p_body, p_rm.clone()], None).unwrap().outcome,
        MergeOutcome::StructuralConflict { .. }
    ));
    assert!(matches!(
        merge_patches(&rs_map, &[p_rm, p_sig], None).unwrap().outcome,
        MergeOutcome::StructuralConflict { .. }
    ));

    // 2. TypeScript class removal vs child ReplaceBody -> StructuralConflict
    let ts_path = Path::new("src/c.ts");
    let ts_base = "export class Foo {\n    bar(): number { return 1; }\n}\nexport function keep(): number { return 0; }\n";
    let ts_rm = "export function keep(): number { return 0; }\n";
    let ts_edit = "export class Foo {\n    bar(): number { return 999; }\n}\nexport function keep(): number { return 0; }\n";
    let p_ts_rm = diff_source_to_patch(&make_ctx(ts_path, LanguageKind::TypeScript, "vp1", "a1", 1), ts_base, ts_rm).unwrap();
    let p_ts_edit = diff_source_to_patch(&make_ctx(ts_path, LanguageKind::TypeScript, "vp2", "a2", 2), ts_base, ts_edit).unwrap();
    let mut ts_map = BTreeMap::new();
    ts_map.insert(ts_path.to_path_buf(), ts_base.as_bytes().to_vec());
    assert!(matches!(
        merge_patches(&ts_map, &[p_ts_rm, p_ts_edit], None).unwrap().outcome,
        MergeOutcome::StructuralConflict { .. }
    ));

    // 3. Duplicate Rust impl blocks: removing first impl S commutes cleanly with editing second impl S::b,
    //    while editing first impl S::a triggers StructuralConflict, and removing both commutes cleanly.
    let impl_path = Path::new("src/impls.rs");
    let impl_base = "pub struct S;\n\nimpl S {\n    pub fn a(&self) -> i32 { 1 }\n}\n\nimpl S {\n    pub fn b(&self) -> i32 { 2 }\n}\n";
    let impl_rm_first = "pub struct S;\n\nimpl S {\n    pub fn b(&self) -> i32 { 2 }\n}\n";
    let impl_rm_second = "pub struct S;\n\nimpl S {\n    pub fn a(&self) -> i32 { 1 }\n}\n";
    let impl_edit_b = "pub struct S;\n\nimpl S {\n    pub fn a(&self) -> i32 { 1 }\n}\n\nimpl S {\n    pub fn b(&self) -> i32 { 200 }\n}\n";
    let impl_edit_a = "pub struct S;\n\nimpl S {\n    pub fn a(&self) -> i32 { 100 }\n}\n\nimpl S {\n    pub fn b(&self) -> i32 { 2 }\n}\n";

    let pa_rm1 = diff_source_to_patch(&make_ctx(impl_path, LanguageKind::Rust, "vp1", "a1", 1), impl_base, impl_rm_first).unwrap();
    let pb_edit_b = diff_source_to_patch(&make_ctx(impl_path, LanguageKind::Rust, "vp2", "a2", 2), impl_base, impl_edit_b).unwrap();
    let pc_edit_a = diff_source_to_patch(&make_ctx(impl_path, LanguageKind::Rust, "vp3", "a3", 3), impl_base, impl_edit_a).unwrap();
    let pd_rm2 = diff_source_to_patch(&make_ctx(impl_path, LanguageKind::Rust, "vp4", "a4", 4), impl_base, impl_rm_second).unwrap();

    let mut impl_map = BTreeMap::new();
    impl_map.insert(impl_path.to_path_buf(), impl_base.as_bytes().to_vec());

    let r12 = merge_patches(&impl_map, &[pa_rm1.clone(), pb_edit_b.clone()], None).unwrap();
    let r21 = merge_patches(&impl_map, &[pb_edit_b, pa_rm1.clone()], None).unwrap();
    assert!(r12.is_clean(), "impl merge conflicts: {:?}", r12.conflicts);
    assert_eq!(r12.text(impl_path), r21.text(impl_path));
    let merged_impl = r12.text(impl_path).unwrap();
    assert!(!merged_impl.contains("fn a("));
    assert!(merged_impl.contains("pub fn b(&self) -> i32 { 200 }"));

    assert!(matches!(
        merge_patches(&impl_map, &[pa_rm1.clone(), pc_edit_a], None).unwrap().outcome,
        MergeOutcome::StructuralConflict { .. }
    ));

    let both_rm = merge_patches(&impl_map, &[pa_rm1, pd_rm2], None).unwrap();
    assert!(both_rm.is_clean(), "both impl remove conflicts: {:?}", both_rm.conflicts);
    assert_eq!(both_rm.text(impl_path).unwrap().trim(), "pub struct S;");
}

#[test]
fn r2_crdt_sequential_chained_patches_and_concurrent_branch() {
    let md_path = Path::new("README.md");
    let base = "# Doc\n\nA: 1\n\nB: 2\n\nC: 3\n";
    let v1 = "# Doc\n\nA: 10\n\nB: 2\n\nC: 3\n";
    let v2 = "# Doc\n\nA: 10\n\nB: 20\n\nC: 3\n";
    let q1_txt = "# Doc\n\nA: 1\n\nB: 2\n\nC: 30\n";

    let mut base_files = BTreeMap::new();
    base_files.insert(md_path.to_path_buf(), base.as_bytes().to_vec());

    // 1. Sequential chain WITH explicit parents
    let p1 = diff_file_to_patch(
        &make_ctx(md_path, LanguageKind::Markdown, "vp1", "a1", 1),
        base.as_bytes(),
        v1.as_bytes(),
        None,
        None,
    )
    .unwrap();
    let mut ctx2 = make_ctx(md_path, LanguageKind::Markdown, "vp1", "a1", 2);
    ctx2.parents = vec![p1.patch_hash];
    let p2 = diff_file_to_patch(&ctx2, v1.as_bytes(), v2.as_bytes(), None, None).unwrap();
    let q1 = diff_file_to_patch(
        &make_ctx(md_path, LanguageKind::Markdown, "vp2", "a2", 3),
        base.as_bytes(),
        q1_txt.as_bytes(),
        None,
        None,
    )
    .unwrap();

    let seq_12 = merge_patches(&base_files, &[p1.clone(), p2.clone()], None).unwrap();
    let seq_21 = merge_patches(&base_files, &[p2.clone(), p1.clone()], None).unwrap();
    assert!(seq_12.is_clean());
    assert_eq!(seq_12.text(md_path), Some(v2));
    assert_eq!(seq_21.text(md_path), Some(v2));

    // All 6 permutations of (P1 -> P2) concurrent with Q1 converge identically
    let perms = [
        [&p1, &p2, &q1],
        [&p1, &q1, &p2],
        [&p2, &p1, &q1],
        [&p2, &q1, &p1],
        [&q1, &p1, &p2],
        [&q1, &p2, &p1],
    ];
    for perm in perms {
        let rep = merge_patches(
            &base_files,
            &[perm[0].clone(), perm[1].clone(), perm[2].clone()],
            None,
        )
        .unwrap();
        assert!(rep.is_clean(), "conflicts: {:?}", rep.conflicts);
        let txt = rep.text(md_path).unwrap();
        assert!(txt.contains("A: 10"), "got: {txt}");
        assert!(txt.contains("B: 20"), "got: {txt}");
        assert!(txt.contains("C: 30"), "got: {txt}");
    }

    // 2. Sequential chain WITHOUT parents populated (implicit causal matching via Automerge heads)
    let p2_noparent = diff_file_to_patch(
        &make_ctx(md_path, LanguageKind::Markdown, "vp1", "a1", 2),
        v1.as_bytes(),
        v2.as_bytes(),
        None,
        None,
    )
    .unwrap();
    let implicit = merge_patches(&base_files, &[p2_noparent.clone(), p1], None).unwrap();
    assert!(implicit.is_clean());
    assert_eq!(implicit.text(md_path), Some(v2));

    // 3. Unmet dependency (p2 without p1) must NOT silently drop or return Clean with stale base
    let orphan_err = merge_patches(&base_files, &[p2_noparent], None);
    assert!(matches!(
        orphan_err,
        Err(CstError::Crdt(CrdtError::MissingDependencies(_)))
    ));
}

#[test]
fn r2_comma_separated_mid_head_insert_and_inline_comment() {
    let rs_path = Path::new("src/cfg.rs");
    let rs_base = "pub struct Config {\n    pub host: String, // primary host\n    pub port: u16,\n}\n\npub enum Mode {\n    Fast,\n    Safe,\n}\n";
    let rs_mid = "pub struct Config {\n    pub host: String, // primary host\n    pub tls: bool,\n    pub port: u16,\n}\n\npub enum Mode {\n    Fast,\n    Mid,\n    Safe,\n}\n";
    let rs_head = "pub struct Config {\n    pub id: u64,\n    pub host: String, // primary host\n    pub port: u16,\n}\n\npub enum Mode {\n    Zero,\n    Fast,\n    Safe,\n}\n";

    let p_mid = diff_source_to_patch(&make_ctx(rs_path, LanguageKind::Rust, "vp1", "a1", 1), rs_base, rs_mid).unwrap();
    let p_head = diff_source_to_patch(&make_ctx(rs_path, LanguageKind::Rust, "vp2", "a2", 2), rs_base, rs_head).unwrap();
    let mut rs_map = BTreeMap::new();
    rs_map.insert(rs_path.to_path_buf(), rs_base.as_bytes().to_vec());

    // Each individually and both concurrently must parse cleanly and preserve inline comment
    assert!(merge_patches(&rs_map, std::slice::from_ref(&p_mid), None).unwrap().is_clean());
    assert!(merge_patches(&rs_map, std::slice::from_ref(&p_head), None).unwrap().is_clean());
    let both = merge_patches(&rs_map, &[p_mid.clone(), p_head.clone()], None).unwrap();
    let both_rev = merge_patches(&rs_map, &[p_head, p_mid], None).unwrap();
    assert!(both.is_clean(), "comma conflicts: {:?}", both.conflicts);
    assert_eq!(both.text(rs_path), both_rev.text(rs_path));
    let merged = both.text(rs_path).unwrap();
    assert!(
        merged.contains("pub host: String, // primary host\n"),
        "inline comment detached: {merged}"
    );
    assert!(merged.contains("pub id: u64,"));
    assert!(merged.contains("pub tls: bool,"));
    assert!(merged.contains("Zero,"));
    assert!(merged.contains("Mid,"));

    // TypeScript enum mid + head insert
    let ts_path = Path::new("src/status.ts");
    let ts_base = "export enum Status {\n    Ok = 1, // ok comment\n    Err = 2,\n}\n";
    let ts_mid = "export enum Status {\n    Ok = 1, // ok comment\n    Warn = 3,\n    Err = 2,\n}\n";
    let ts_head = "export enum Status {\n    Init = 0,\n    Ok = 1, // ok comment\n    Err = 2,\n}\n";
    let pt_mid = diff_source_to_patch(&make_ctx(ts_path, LanguageKind::TypeScript, "vp1", "a1", 1), ts_base, ts_mid).unwrap();
    let pt_head = diff_source_to_patch(&make_ctx(ts_path, LanguageKind::TypeScript, "vp2", "a2", 2), ts_base, ts_head).unwrap();
    let mut ts_map = BTreeMap::new();
    ts_map.insert(ts_path.to_path_buf(), ts_base.as_bytes().to_vec());
    let ts_both = merge_patches(&ts_map, &[pt_mid, pt_head], None).unwrap();
    assert!(ts_both.is_clean(), "ts enum conflicts: {:?}", ts_both.conflicts);
    let ts_merged = ts_both.text(ts_path).unwrap();
    assert!(ts_merged.contains("Ok = 1, // ok comment\n"));
    assert!(ts_merged.contains("Init = 0,"));
    assert!(ts_merged.contains("Warn = 3,"));
}

#[test]
fn r2_text_vs_binary_conflict_and_missing_cas_chunk_rejected() {
    let dir = TempDir::new().unwrap();
    let cas = CasStore::open_default(dir.path().join("cas")).unwrap();
    let path = Path::new("data.txt");
    let base = b"hello world\n".to_vec();
    let mut base_map = BTreeMap::new();
    base_map.insert(path.to_path_buf(), base.clone());

    let p_text = diff_file_to_patch(
        &make_ctx(path, LanguageKind::PlainText, "vp1", "a1", 1),
        &base,
        b"hello loom\n",
        None,
        Some(&cas),
    )
    .unwrap();
    let p_bin = diff_file_to_patch(
        &make_ctx(path, LanguageKind::Binary, "vp2", "a2", 2),
        &base,
        b"\x00\x01\x02\x03",
        None,
        Some(&cas),
    )
    .unwrap();

    let r12 = merge_patches(&base_map, &[p_text.clone(), p_bin.clone()], Some(&cas)).unwrap();
    let r21 = merge_patches(&base_map, &[p_bin.clone(), p_text], Some(&cas)).unwrap();
    assert!(matches!(r12.outcome, MergeOutcome::StructuralConflict { .. }));
    assert_eq!(r12.outcome, r21.outcome);

    // Deleting a CAS chunk must cause merge_patches(..., Some(&cas)) to fail loudly (not Clean)
    let manifest = p_bin.binary_manifest.as_ref().unwrap();
    cas.delete(&manifest.chunk_hashes[0]).unwrap();
    let err = merge_patches(&base_map, &[p_bin], Some(&cas));
    assert!(matches!(err, Err(CstError::Cas(CasError::InvalidManifest(_)))));
}

#[test]
fn r2_rust_file_start_insert_respects_inner_attrs_and_docs() {
    let path = Path::new("src/lib.rs");
    let base = "//! Crate docs\n#![allow(dead_code)]\n\npub fn old() -> i32 { 1 }\n";
    let m1 = "//! Crate docs\n#![allow(dead_code)]\n\npub fn new_first() -> i32 { 2 }\n\npub fn old() -> i32 { 1 }\n";
    let m2 = "//! Crate docs\n#![allow(dead_code)]\n\npub fn old() -> i32 { 99 }\n";

    let p1 = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp1", "a1", 1), base, m1).unwrap();
    let p2 = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp2", "a2", 2), base, m2).unwrap();
    let mut base_map = BTreeMap::new();
    base_map.insert(path.to_path_buf(), base.as_bytes().to_vec());

    let rep = merge_patches(&base_map, &[p1, p2], None).unwrap();
    assert!(rep.is_clean(), "conflicts: {:?}", rep.conflicts);
    let merged = rep.text(path).unwrap();
    let attr_pos = merged.find("#![allow(dead_code)]").unwrap();
    let new_pos = merged.find("pub fn new_first()").unwrap();
    let old_pos = merged.find("pub fn old()").unwrap();
    assert!(attr_pos < new_pos && new_pos < old_pos, "wrong ordering:\n{merged}");
}

#[test]
fn r2_python_single_line_pass_class_and_docstring_placement() {
    // 1. Single-line `class Empty: pass`
    let empty_path = Path::new("empty.py");
    let empty_base = "class Empty: pass\n";
    let empty_mod = "class Empty:\n    def m(self): return 1\n";
    let pe = diff_source_to_patch(
        &make_ctx(empty_path, LanguageKind::Python, "vp1", "a1", 1),
        empty_base,
        empty_mod,
    )
    .unwrap();
    let mut empty_map = BTreeMap::new();
    empty_map.insert(empty_path.to_path_buf(), empty_base.as_bytes().to_vec());
    let re = merge_patches(&empty_map, &[pe], None).unwrap();
    assert!(re.is_clean(), "empty class conflicts: {:?}", re.conflicts);
    assert_eq!(re.text(empty_path).unwrap(), "class Empty:\n    def m(self): return 1\n");

    // 2. Class docstring stays first + module docstring stays first
    let doc_path = Path::new("svc.py");
    let doc_base = "\"\"\"Module docs.\"\"\"\n\nclass Svc:\n    \"\"\"Service docs.\"\"\"\n\n    def existing(self):\n        return 0\n";
    let doc_m1 = "\"\"\"Module docs.\"\"\"\n\ndef top_helper():\n    return 1\n\nclass Svc:\n    \"\"\"Service docs.\"\"\"\n\n    def alpha(self):\n        return 1\n\n    def existing(self):\n        return 0\n";
    let doc_m2 = "\"\"\"Module docs.\"\"\"\n\nclass Svc:\n    \"\"\"Service docs.\"\"\"\n\n    def beta(self):\n        return 2\n\n    def existing(self):\n        return 0\n";

    let pm1 = diff_source_to_patch(&make_ctx(doc_path, LanguageKind::Python, "vp1", "a1", 1), doc_base, doc_m1).unwrap();
    let pm2 = diff_source_to_patch(&make_ctx(doc_path, LanguageKind::Python, "vp2", "a2", 2), doc_base, doc_m2).unwrap();
    let mut doc_map = BTreeMap::new();
    doc_map.insert(doc_path.to_path_buf(), doc_base.as_bytes().to_vec());

    let r12 = merge_patches(&doc_map, &[pm1.clone(), pm2.clone()], None).unwrap();
    let r21 = merge_patches(&doc_map, &[pm2, pm1], None).unwrap();
    assert!(r12.is_clean(), "py docstring conflicts: {:?}", r12.conflicts);
    assert_eq!(r12.text(doc_path), r21.text(doc_path));
    let merged = r12.text(doc_path).unwrap();
    assert!(merged.starts_with("\"\"\"Module docs.\"\"\""));
    let svc_doc_pos = merged.find("\"\"\"Service docs.\"\"\"").unwrap();
    let alpha_pos = merged.find("def alpha").unwrap();
    let beta_pos = merged.find("def beta").unwrap();
    assert!(svc_doc_pos < alpha_pos && svc_doc_pos < beta_pos, "class docstring displaced:\n{merged}");
}

#[test]
fn r2_visibility_reduced_same_file_clean_cross_file_semantic_conflict() {
    let lib_path = Path::new("src/lib.rs");
    let main_path = Path::new("src/main.rs");
    let lib_base = "pub fn helper(a: i32) -> i32 {\n    a + 1\n}\n\npub fn caller() -> i32 {\n    helper(1)\n}\n";
    let lib_vis_reduced = "fn helper(a: i32) -> i32 {\n    a + 1\n}\n\npub fn caller() -> i32 {\n    helper(1)\n}\n";
    let lib_caller_edit = "pub fn helper(a: i32) -> i32 {\n    a + 1\n}\n\npub fn caller() -> i32 {\n    helper(42)\n}\n";
    let main_base = "fn use_it() -> i32 {\n    helper(1)\n}\n";
    let main_edit = "fn use_it() -> i32 {\n    helper(7)\n}\n";

    let p_vis = diff_source_to_patch(&make_ctx(lib_path, LanguageKind::Rust, "vp1", "a1", 1), lib_base, lib_vis_reduced).unwrap();
    let p_same = diff_source_to_patch(&make_ctx(lib_path, LanguageKind::Rust, "vp2", "a2", 2), lib_base, lib_caller_edit).unwrap();
    let p_cross = diff_source_to_patch(&make_ctx(main_path, LanguageKind::Rust, "vp3", "a3", 3), main_base, main_edit).unwrap();

    // Same-file caller edit + VisibilityReduced -> Clean!
    let mut same_map = BTreeMap::new();
    same_map.insert(lib_path.to_path_buf(), lib_base.as_bytes().to_vec());
    let same_res = merge_patches(&same_map, &[p_vis.clone(), p_same], None).unwrap();
    assert!(same_res.is_clean(), "unexpected same-file conflict: {:?}", same_res.conflicts);
    let same_txt = same_res.text(lib_path).unwrap();
    assert!(same_txt.contains("fn helper(a: i32)"));
    assert!(same_txt.contains("helper(42)"));

    // Cross-file caller + VisibilityReduced -> SemanticConflict!
    let mut cross_map = BTreeMap::new();
    cross_map.insert(lib_path.to_path_buf(), lib_base.as_bytes().to_vec());
    cross_map.insert(main_path.to_path_buf(), main_base.as_bytes().to_vec());
    let cross_res = merge_patches(&cross_map, &[p_vis.clone(), p_cross], None).unwrap();
    assert!(matches!(
        cross_res.outcome,
        MergeOutcome::SemanticConflict { ref symbol_fqn, .. } if symbol_fqn == "helper"
    ));

    // Untouched cross-file consumer via merge_trees -> SemanticConflict!
    let tree_res = merge_trees(&cross_map, &[&[p_vis]], None).unwrap();
    assert!(matches!(
        tree_res.outcome,
        MergeOutcome::SemanticConflict { ref symbol_fqn, .. } if symbol_fqn == "helper"
    ));
}

#[test]
fn r2_hardening_rename_collision_merge_trees_and_nul_over_8k() {
    // 1. Rename over untouched base file -> StructuralConflict
    let a_path = Path::new("src/a.rs");
    let b_path = Path::new("src/b.rs");
    let c_path = Path::new("src/c.rs");
    let a_src = b"pub fn a_fn() {}\n".to_vec();
    let b_src = b"pub fn b_fn() {}\n".to_vec();
    let mut files = BTreeMap::new();
    files.insert(a_path.to_path_buf(), a_src.clone());
    files.insert(b_path.to_path_buf(), b_src.clone());

    let p_ren_ab = diff_file_to_patch(
        &make_ctx(b_path, LanguageKind::Rust, "vp1", "a1", 1),
        &a_src,
        &a_src,
        Some(a_path),
        None,
    )
    .unwrap();
    let mut p_ren_ab_fid = p_ren_ab;
    p_ren_ab_fid.logical_file_id = compute_logical_file_id(a_path);
    p_ren_ab_fid.finalize_hash();

    let col_res = merge_patches(&files, &[p_ren_ab_fid.clone()], None).unwrap();
    assert!(matches!(col_res.outcome, MergeOutcome::StructuralConflict { .. }));

    // When b.rs itself vacates to c.rs, a.rs -> b.rs succeeds cleanly
    let mut p_ren_bc = diff_file_to_patch(
        &make_ctx(c_path, LanguageKind::Rust, "vp2", "a2", 2),
        &b_src,
        &b_src,
        Some(b_path),
        None,
    )
    .unwrap();
    p_ren_bc.logical_file_id = compute_logical_file_id(b_path);
    p_ren_bc.finalize_hash();
    let chain_res = merge_patches(&files, &[p_ren_ab_fid, p_ren_bc], None).unwrap();
    assert!(chain_res.is_clean(), "chain rename conflicts: {:?}", chain_res.conflicts);

    // 2. merge_trees skips old base_path of a renamed & updated consumer file
    let prod_path = Path::new("src/prod.rs");
    let cons_old = Path::new("src/cons.rs");
    let cons_new = Path::new("src/cons_v2.rs");
    let prod_base = b"pub fn old_api() {}\n";
    let prod_mod = b"pub fn new_api() {}\n";
    let cons_base = b"pub fn run() { old_api(); }\n";
    let cons_mod = b"pub fn run() { new_api(); }\n";

    let mut tree_files = BTreeMap::new();
    tree_files.insert(prod_path.to_path_buf(), prod_base.to_vec());
    tree_files.insert(cons_old.to_path_buf(), cons_base.to_vec());

    let p_prod = diff_file_to_patch(
        &make_ctx(prod_path, LanguageKind::Rust, "vp1", "a1", 1),
        prod_base,
        prod_mod,
        None,
        None,
    )
    .unwrap();
    let mut cons_ctx = make_ctx(cons_new, LanguageKind::Rust, "vp2", "a2", 2);
    cons_ctx.logical_file_id = compute_logical_file_id(cons_old);
    let p_cons = diff_file_to_patch(
        &cons_ctx,
        cons_base,
        cons_mod,
        Some(cons_old),
        None,
    )
    .unwrap();

    let tree_rep = merge_trees(&tree_files, &[&[p_prod], &[p_cons]], None).unwrap();
    assert!(tree_rep.is_clean(), "stale renamed path false conflict: {:?}", tree_rep.conflicts);

    // 3. >8 KiB prefix before NUL byte detects as Binary
    let mut payload = vec![b'x'; 8500];
    payload.push(0);
    payload.extend_from_slice(b"bin");
    assert_eq!(LanguageKind::detect(Path::new("notes.txt"), &payload), LanguageKind::Binary);
    assert!(is_binary_content(Path::new("notes.txt"), &payload));
    assert_ne!(Blake3Hash::from_bytes(&payload), Blake3Hash::ZERO);
}

#[test]
fn r3_chained_and_swap_renames_preserve_all_files_both_hash_directions() {
    let helper_rename = |old_p: &Path, new_p: &Path, bytes: &[u8], vp: &str, actor: &str, ts: u64| {
        let mut ctx = make_ctx(new_p, LanguageKind::Rust, vp, actor, ts);
        ctx.logical_file_id = compute_logical_file_id(old_p);
        diff_file_to_patch(&ctx, bytes, bytes, Some(old_p), None).unwrap()
    };

    // Direction 1: src/a.rs -> src/b.rs -> src/c.rs (where fid(b) < fid(a))
    let a_path = Path::new("src/a.rs");
    let b_path = Path::new("src/b.rs");
    let c_path = Path::new("src/c.rs");
    let a_src = b"pub fn from_a() -> i32 { 1 }\n";
    let b_src = b"pub fn from_b() -> i32 { 2 }\n";
    let mut files1 = BTreeMap::new();
    files1.insert(a_path.to_path_buf(), a_src.to_vec());
    files1.insert(b_path.to_path_buf(), b_src.to_vec());

    let p_ab = helper_rename(a_path, b_path, a_src, "vp1", "a1", 1);
    let p_bc = helper_rename(b_path, c_path, b_src, "vp2", "a2", 2);
    let r1 = merge_patches(&files1, &[p_ab.clone(), p_bc.clone()], None).unwrap();
    let r1_rev = merge_patches(&files1, &[p_bc, p_ab], None).unwrap();
    assert!(r1.is_clean(), "chain dir1 conflicts: {:?}", r1.conflicts);
    assert_eq!(r1.text(a_path), None);
    assert_eq!(r1.text(b_path), Some("pub fn from_a() -> i32 { 1 }\n"));
    assert_eq!(r1.text(c_path), Some("pub fn from_b() -> i32 { 2 }\n"));
    assert_eq!(r1.merged_files, r1_rev.merged_files);

    // Direction 2: src/b.rs -> src/a.rs -> src/c.rs (opposite hash direction: fid(b) < fid(a))
    let p_ba = helper_rename(b_path, a_path, b_src, "vp1", "a1", 1);
    let p_ac = helper_rename(a_path, c_path, a_src, "vp2", "a2", 2);
    let r2 = merge_patches(&files1, &[p_ba.clone(), p_ac.clone()], None).unwrap();
    let r2_rev = merge_patches(&files1, &[p_ac, p_ba], None).unwrap();
    assert!(r2.is_clean(), "chain dir2 conflicts: {:?}", r2.conflicts);
    assert_eq!(r2.text(b_path), None);
    assert_eq!(r2.text(a_path), Some("pub fn from_b() -> i32 { 2 }\n"));
    assert_eq!(r2.text(c_path), Some("pub fn from_a() -> i32 { 1 }\n"));
    assert_eq!(r2.merged_files, r2_rev.merged_files);

    // Direction 3: explicit x1 -> y1 -> z1 (also fid(x1) < fid(y1))
    let x1 = Path::new("src/x1.rs");
    let y1 = Path::new("src/y1.rs");
    let z1 = Path::new("src/z1.rs");
    assert!(compute_logical_file_id(x1) < compute_logical_file_id(y1));
    let mut files_xyz = BTreeMap::new();
    files_xyz.insert(x1.to_path_buf(), a_src.to_vec());
    files_xyz.insert(y1.to_path_buf(), b_src.to_vec());
    let p_xy = helper_rename(x1, y1, a_src, "vp1", "a1", 1);
    let p_yz = helper_rename(y1, z1, b_src, "vp2", "a2", 2);
    let r_xyz = merge_patches(&files_xyz, &[p_xy, p_yz], None).unwrap();
    assert!(r_xyz.is_clean(), "chain xyz conflicts: {:?}", r_xyz.conflicts);
    assert_eq!(r_xyz.text(x1), None);
    assert_eq!(r_xyz.text(y1), Some("pub fn from_a() -> i32 { 1 }\n"));
    assert_eq!(r_xyz.text(z1), Some("pub fn from_b() -> i32 { 2 }\n"));

    // Swap rename: src/a.rs <-> src/b.rs
    let swap_ab = helper_rename(a_path, b_path, a_src, "vp1", "a1", 1);
    let swap_ba = helper_rename(b_path, a_path, b_src, "vp2", "a2", 2);
    let r_swap1 = merge_patches(&files1, &[swap_ab.clone(), swap_ba.clone()], None).unwrap();
    let r_swap2 = merge_patches(&files1, &[swap_ba, swap_ab], None).unwrap();
    assert!(r_swap1.is_clean(), "swap conflicts: {:?}", r_swap1.conflicts);
    assert_eq!(r_swap1.text(a_path), Some("pub fn from_b() -> i32 { 2 }\n"));
    assert_eq!(r_swap1.text(b_path), Some("pub fn from_a() -> i32 { 1 }\n"));
    assert_eq!(r_swap1.merged_files, r_swap2.merged_files);
}

#[test]
fn r3_delete_first_rust_impl_block_no_false_removed_symbol_on_surviving_impl() {
    let lib_path = Path::new("src/lib.rs");
    let app_path = Path::new("src/app.rs");

    let lib_base = concat!(
        "pub struct S;\n",
        "impl S {\n    pub fn a(&self) -> i32 { 1 }\n}\n",
        "impl S {\n    pub fn b(&self) -> i32 { 2 }\n}\n",
    );
    let lib_del_first = concat!(
        "pub struct S;\n",
        "impl S {\n    pub fn b(&self) -> i32 { 2 }\n}\n",
    );

    let p_del_first = diff_source_to_patch(
        &make_ctx(lib_path, LanguageKind::Rust, "vp1", "a1", 1),
        lib_base,
        lib_del_first,
    )
    .unwrap();

    assert!(
        !p_del_first.contract_deltas.iter().any(|d| matches!(
            d,
            SymbolContractDelta::RemovedSymbol { symbol_fqn, .. } if symbol_fqn == "impl<S>#2::b" || symbol_fqn == "impl<S>#2"
        )),
        "surviving second impl must not emit false RemovedSymbol: {:?}",
        p_del_first.contract_deltas
    );
    assert!(
        p_del_first.contract_deltas.iter().any(|d| matches!(
            d,
            SymbolContractDelta::RemovedSymbol { symbol_fqn, .. } if symbol_fqn == "impl<S>::a"
        )),
        "deleted first impl method must emit RemovedSymbol: {:?}",
        p_del_first.contract_deltas
    );

    // Untouched app.rs calling s.b() -> Clean!
    let mut tree_b = BTreeMap::new();
    tree_b.insert(lib_path.to_path_buf(), lib_base.as_bytes().to_vec());
    tree_b.insert(
        app_path.to_path_buf(),
        b"pub fn run(s: &crate::S) -> i32 {\n    s.b()\n}\n".to_vec(),
    );
    let res_b = merge_trees(&tree_b, &[std::slice::from_ref(&p_del_first)], None).unwrap();
    assert!(
        res_b.is_clean(),
        "untouched caller of surviving impl#2::b must be Clean: {:?}",
        res_b.conflicts
    );

    // Untouched app.rs calling s.a() -> SemanticConflict!
    let mut tree_a = BTreeMap::new();
    tree_a.insert(lib_path.to_path_buf(), lib_base.as_bytes().to_vec());
    tree_a.insert(
        app_path.to_path_buf(),
        b"pub fn run(s: &crate::S) -> i32 {\n    s.a()\n}\n".to_vec(),
    );
    let res_a = merge_trees(&tree_a, &[&[p_del_first]], None).unwrap();
    assert!(matches!(
        res_a.outcome,
        MergeOutcome::SemanticConflict { ref symbol_fqn, .. } if symbol_fqn == "impl<S>::a"
    ));

    // Also: deleting 1st impl AND mutating b()'s signature in 2nd impl -> SemanticConflict for b
    let lib_del_and_sig = concat!(
        "pub struct S;\n",
        "impl S {\n    pub fn b(&self, extra: i32) -> i32 { 2 + extra }\n}\n",
    );
    let p_del_and_sig = diff_source_to_patch(
        &make_ctx(lib_path, LanguageKind::Rust, "vp1", "a1", 2),
        lib_base,
        lib_del_and_sig,
    )
    .unwrap();
    let res_sig = merge_trees(&tree_b, &[&[p_del_and_sig]], None).unwrap();
    assert!(matches!(
        res_sig.outcome,
        MergeOutcome::SemanticConflict { ref symbol_fqn, .. } if symbol_fqn == "impl<S>#2::b"
    ));
}

#[test]
fn r3_commaless_last_struct_field_with_inline_comment_insert_member() {
    let path = Path::new("src/tail.rs");
    let base = "pub struct Tail {\n    pub a: i32,\n    pub b: u16 // no trailing comma\n}\n";
    let m1 = "pub struct Tail {\n    pub a: i32,\n    pub b: u16, // no trailing comma\n    pub c: bool\n}\n";
    let m2 = "pub struct Tail {\n    pub a: i32,\n    pub b: u16, // no trailing comma\n    pub d: String\n}\n";

    let p1 = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp1", "a1", 1), base, m1).unwrap();
    let p2 = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp2", "a2", 2), base, m2).unwrap();

    let mut files = BTreeMap::new();
    files.insert(path.to_path_buf(), base.as_bytes().to_vec());

    // Single insert keeps `// no trailing comma` on `pub b: u16,`
    let single = merge_patches(&files, std::slice::from_ref(&p1), None).unwrap();
    assert!(single.is_clean(), "single insert conflicts: {:?}", single.conflicts);
    let single_txt = single.text(path).unwrap();
    assert!(
        single_txt.contains("pub b: u16, // no trailing comma\n"),
        "inline comment detached from pub b:\n{single_txt}"
    );
    assert!(
        single_txt.contains("    pub c: bool\n"),
        "missing inserted field:\n{single_txt}"
    );
    assert!(!parse_source(LanguageKind::Rust, single_txt, None).unwrap().tree.root_node().has_error());

    // Concurrent two-agent insert after comma-less last field commutes cleanly
    let r12 = merge_patches(&files, &[p1.clone(), p2.clone()], None).unwrap();
    let r21 = merge_patches(&files, &[p2, p1], None).unwrap();
    assert!(r12.is_clean(), "concurrent tail insert conflicts: {:?}", r12.conflicts);
    assert_eq!(r12.text(path), r21.text(path));
    let merged = r12.text(path).unwrap();
    assert!(merged.contains("pub b: u16, // no trailing comma\n"), "merged:\n{merged}");
    assert!(merged.contains("pub c: bool"), "merged:\n{merged}");
    assert!(merged.contains("pub d: String"), "merged:\n{merged}");
    assert!(!parse_source(LanguageKind::Rust, merged, None).unwrap().tree.root_node().has_error());
}

#[test]
fn r3_typescript_accessibility_modifier_same_file_clean_cross_file_semantic_conflict() {
    let s_path = Path::new("src/s.ts");
    let app_path = Path::new("src/app.ts");
    let s_base = concat!(
        "export class S {\n",
        "    public helper(x: number): number {\n        return x + 1;\n    }\n",
        "    public run(): number {\n        return this.helper(1);\n    }\n",
        "}\n",
    );
    let s_vis_reduced = concat!(
        "export class S {\n",
        "    private helper(x: number): number {\n        return x + 1;\n    }\n",
        "    public run(): number {\n        return this.helper(1);\n    }\n",
        "}\n",
    );
    let s_same_edit = concat!(
        "export class S {\n",
        "    public helper(x: number): number {\n        return x + 1;\n    }\n",
        "    public run(): number {\n        return this.helper(2);\n    }\n",
        "}\n",
    );
    let app_base = "import { S } from './s';\nexport function go(s: S): number {\n    return s.helper(1);\n}\n";
    let app_edit = "import { S } from './s';\nexport function go(s: S): number {\n    return s.helper(9);\n}\n";

    let p_vis = diff_source_to_patch(
        &make_ctx(s_path, LanguageKind::TypeScript, "vp1", "a1", 1),
        s_base,
        s_vis_reduced,
    )
    .unwrap();
    assert!(
        p_vis.contract_deltas.iter().any(|d| matches!(
            d,
            SymbolContractDelta::VisibilityReduced { symbol_fqn, .. } if symbol_fqn == "S::helper"
        )),
        "expected VisibilityReduced for S::helper: {:?}",
        p_vis.contract_deltas
    );
    assert!(
        !p_vis.contract_deltas.iter().any(|d| matches!(
            d,
            SymbolContractDelta::SignatureChanged { symbol_fqn, .. } if symbol_fqn == "S::helper"
        )),
        "accessibility_modifier must not emit SignatureChanged: {:?}",
        p_vis.contract_deltas
    );

    // Same-file caller edit + public->private -> Clean
    let p_same = diff_source_to_patch(
        &make_ctx(s_path, LanguageKind::TypeScript, "vp2", "a2", 2),
        s_base,
        s_same_edit,
    )
    .unwrap();
    let mut same_files = BTreeMap::new();
    same_files.insert(s_path.to_path_buf(), s_base.as_bytes().to_vec());
    let same_res = merge_patches(&same_files, &[p_vis.clone(), p_same], None).unwrap();
    assert!(same_res.is_clean(), "TS same-file visibility conflicts: {:?}", same_res.conflicts);
    let merged_s = same_res.text(s_path).unwrap();
    assert!(merged_s.contains("private helper(x: number): number"));
    assert!(merged_s.contains("this.helper(2)"));

    // Cross-file caller edit + public->private -> SemanticConflict
    let p_cross = diff_source_to_patch(
        &make_ctx(app_path, LanguageKind::TypeScript, "vp3", "a3", 3),
        app_base,
        app_edit,
    )
    .unwrap();
    let mut cross_files = BTreeMap::new();
    cross_files.insert(s_path.to_path_buf(), s_base.as_bytes().to_vec());
    cross_files.insert(app_path.to_path_buf(), app_base.as_bytes().to_vec());
    let cross_res = merge_patches(&cross_files, &[p_vis.clone(), p_cross], None).unwrap();
    assert!(matches!(
        cross_res.outcome,
        MergeOutcome::SemanticConflict { ref symbol_fqn, .. } if symbol_fqn == "S::helper"
    ));

    // Untouched cross-file consumer via merge_trees -> SemanticConflict
    let tree_res = merge_trees(&cross_files, &[&[p_vis]], None).unwrap();
    assert!(matches!(
        tree_res.outcome,
        MergeOutcome::SemanticConflict { ref symbol_fqn, .. } if symbol_fqn == "S::helper"
    ));
}

#[test]
fn r4_concurrent_insert_impl_before_removed_impl_and_edit_surviving_impl() {
    let path = Path::new("src/lib.rs");
    let base = concat!(
        "pub struct S;\n",
        "impl S {\n    pub fn a(&self) -> i32 { 1 }\n}\n",
        "impl S {\n    pub fn b(&self) -> i32 { 2 }\n}\n",
    );
    let mod1 = concat!(
        "pub struct S;\n",
        "impl S {\n    pub fn b(&self) -> i32 { 2 }\n}\n",
    );
    let mod2 = concat!(
        "pub struct S;\n",
        "impl S {\n    pub fn c(&self) -> i32 { 3 }\n}\n",
        "impl S {\n    pub fn a(&self) -> i32 { 1 }\n}\n",
        "impl S {\n    pub fn b(&self) -> i32 { 99 }\n}\n",
    );

    let p1 = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp1", "a1", 1), base, mod1).unwrap();
    let p2 = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp2", "a2", 2), base, mod2).unwrap();

    let mut files = BTreeMap::new();
    files.insert(path.to_path_buf(), base.as_bytes().to_vec());

    let r12 = merge_patches(&files, &[p1.clone(), p2.clone()], None).unwrap();
    let r21 = merge_patches(&files, &[p2, p1], None).unwrap();
    assert!(r12.is_clean(), "expected Clean merge, got {:?}", r12.conflicts);
    assert!(r21.is_clean(), "expected Clean merge (reversed), got {:?}", r21.conflicts);
    assert_eq!(r12.text(path), r21.text(path));

    let merged = r12.text(path).unwrap();
    assert!(merged.contains("pub fn c(&self) -> i32 { 3 }"), "missing inserted c:\n{merged}");
    assert!(!merged.contains("fn a("), "deleted a survived:\n{merged}");
    assert!(merged.contains("pub fn b(&self) -> i32 { 99 }"), "missing edited b:\n{merged}");
    assert!(
        !parse_source(LanguageKind::Rust, merged, None)
            .unwrap()
            .tree
            .root_node()
            .has_error(),
        "invalid Rust CST:\n{merged}"
    );
}

#[test]
fn r4_multi_impl_concurrent_add_remove_shift_matrix() {
    let path = Path::new("src/multi.rs");

    // Case A: Delete 2nd impl S while inserting new impl S before 1st impl S
    let base_2 = concat!(
        "pub struct S;\n",
        "impl S {\n    pub fn a(&self) -> i32 { 1 }\n}\n",
        "impl S {\n    pub fn b(&self) -> i32 { 2 }\n}\n",
    );
    let del_2nd = concat!(
        "pub struct S;\n",
        "impl S {\n    pub fn a(&self) -> i32 { 1 }\n}\n",
    );
    let ins_before_1st = concat!(
        "pub struct S;\n",
        "impl S {\n    pub fn c(&self) -> i32 { 3 }\n}\n",
        "impl S {\n    pub fn a(&self) -> i32 { 1 }\n}\n",
        "impl S {\n    pub fn b(&self) -> i32 { 2 }\n}\n",
    );
    let pa_del2 = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp1", "a1", 1), base_2, del_2nd).unwrap();
    let pa_ins1 = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp2", "a2", 2), base_2, ins_before_1st).unwrap();
    let mut files_2 = BTreeMap::new();
    files_2.insert(path.to_path_buf(), base_2.as_bytes().to_vec());

    let ra12 = merge_patches(&files_2, &[pa_del2.clone(), pa_ins1.clone()], None).unwrap();
    let ra21 = merge_patches(&files_2, &[pa_ins1, pa_del2], None).unwrap();
    assert!(ra12.is_clean(), "Case A conflicts: {:?}", ra12.conflicts);
    assert_eq!(ra12.text(path), ra21.text(path));
    let ma = ra12.text(path).unwrap();
    assert!(ma.contains("fn c("), "Case A missing c:\n{ma}");
    assert!(ma.contains("fn a("), "Case A missing a:\n{ma}");
    assert!(!ma.contains("fn b("), "Case A deleted b survived:\n{ma}");
    assert!(!parse_source(LanguageKind::Rust, ma, None).unwrap().tree.root_node().has_error());

    // Case B: Delete 1st impl S + insert new impl S before it while ALSO inserting method d() into surviving 2nd impl S
    let del_1st = concat!(
        "pub struct S;\n",
        "impl S {\n    pub fn b(&self) -> i32 { 2 }\n}\n",
    );
    let ins_c_and_method_d = concat!(
        "pub struct S;\n",
        "impl S {\n    pub fn c(&self) -> i32 { 3 }\n}\n",
        "impl S {\n    pub fn a(&self) -> i32 { 1 }\n}\n",
        "impl S {\n    pub fn b(&self) -> i32 { 2 }\n\n    pub fn d(&self) -> i32 { 4 }\n}\n",
    );
    let pb_del1 = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp1", "a1", 1), base_2, del_1st).unwrap();
    let pb_ins = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp2", "a2", 2), base_2, ins_c_and_method_d).unwrap();
    let rb12 = merge_patches(&files_2, &[pb_del1.clone(), pb_ins.clone()], None).unwrap();
    let rb21 = merge_patches(&files_2, &[pb_ins, pb_del1.clone()], None).unwrap();
    assert!(rb12.is_clean(), "Case B conflicts: {:?}", rb12.conflicts);
    assert_eq!(rb12.text(path), rb21.text(path));
    let mb = rb12.text(path).unwrap();
    assert!(mb.contains("fn c("), "Case B missing c:\n{mb}");
    assert!(!mb.contains("fn a("), "Case B deleted a survived:\n{mb}");
    assert!(mb.contains("fn b("), "Case B missing b:\n{mb}");
    assert!(mb.contains("fn d("), "Case B missing d:\n{mb}");
    assert!(!parse_source(LanguageKind::Rust, mb, None).unwrap().tree.root_node().has_error());

    // Case C: 3 base impl blocks (a, b, d): Patch 1 deletes 1st (a) + 3rd (d); Patch 2 inserts c at start + inserts e after 2nd (b)
    let base_3 = concat!(
        "pub struct S;\n",
        "impl S {\n    pub fn a(&self) -> i32 { 1 }\n}\n",
        "impl S {\n    pub fn b(&self) -> i32 { 2 }\n}\n",
        "impl S {\n    pub fn d(&self) -> i32 { 4 }\n}\n",
    );
    let del_1_and_3 = concat!(
        "pub struct S;\n",
        "impl S {\n    pub fn b(&self) -> i32 { 2 }\n}\n",
    );
    let ins_c_and_e = concat!(
        "pub struct S;\n",
        "impl S {\n    pub fn c(&self) -> i32 { 3 }\n}\n",
        "impl S {\n    pub fn a(&self) -> i32 { 1 }\n}\n",
        "impl S {\n    pub fn b(&self) -> i32 { 2 }\n}\n",
        "impl S {\n    pub fn e(&self) -> i32 { 5 }\n}\n",
        "impl S {\n    pub fn d(&self) -> i32 { 4 }\n}\n",
    );
    let pc_del = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp1", "a1", 1), base_3, del_1_and_3).unwrap();
    let pc_ins = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp2", "a2", 2), base_3, ins_c_and_e).unwrap();
    let mut files_3 = BTreeMap::new();
    files_3.insert(path.to_path_buf(), base_3.as_bytes().to_vec());
    let rc12 = merge_patches(&files_3, &[pc_del.clone(), pc_ins.clone()], None).unwrap();
    let rc21 = merge_patches(&files_3, &[pc_ins, pc_del], None).unwrap();
    assert!(rc12.is_clean(), "Case C conflicts: {:?}", rc12.conflicts);
    assert_eq!(rc12.text(path), rc21.text(path));
    let mc = rc12.text(path).unwrap();
    assert!(mc.contains("fn c("), "Case C missing c:\n{mc}");
    assert!(mc.contains("fn b("), "Case C missing b:\n{mc}");
    assert!(mc.contains("fn e("), "Case C missing e:\n{mc}");
    assert!(!mc.contains("fn a("), "Case C deleted a survived:\n{mc}");
    assert!(!mc.contains("fn d("), "Case C deleted d survived:\n{mc}");
    assert!(!parse_source(LanguageKind::Rust, mc, None).unwrap().tree.root_node().has_error());

    // Case D: Identical-content base impl S {} blocks: delete 1st + insert new impl S before it
    let base_ident = "pub struct S;\nimpl S {}\nimpl S {}\n";
    let del_ident = "pub struct S;\nimpl S {}\n";
    let ins_ident = "pub struct S;\nimpl S {\n    pub fn c(&self) -> i32 { 3 }\n}\nimpl S {}\nimpl S {}\n";
    let pd_del = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp1", "a1", 1), base_ident, del_ident).unwrap();
    let pd_ins = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp2", "a2", 2), base_ident, ins_ident).unwrap();
    let mut files_ident = BTreeMap::new();
    files_ident.insert(path.to_path_buf(), base_ident.as_bytes().to_vec());
    let rd12 = merge_patches(&files_ident, &[pd_del.clone(), pd_ins.clone()], None).unwrap();
    let rd21 = merge_patches(&files_ident, &[pd_ins, pd_del], None).unwrap();
    assert!(rd12.is_clean(), "Case D conflicts: {:?}", rd12.conflicts);
    assert_eq!(rd12.text(path), rd21.text(path));
    let md = rd12.text(path).unwrap();
    assert!(md.contains("fn c("), "Case D missing inserted c:\n{md}");
    assert_eq!(md.matches("impl S {}").count(), 1, "Case D expected 1 empty impl S:\n{md}");
    assert!(!parse_source(LanguageKind::Rust, md, None).unwrap().tree.root_node().has_error());

    // Case E: Nested mod container with positional impl S blocks
    let base_mod = concat!(
        "pub mod inner {\n",
        "    pub struct S;\n",
        "    impl S {\n        pub fn a(&self) -> i32 { 1 }\n    }\n",
        "    impl S {\n        pub fn b(&self) -> i32 { 2 }\n    }\n",
        "}\n",
    );
    let mod_del = concat!(
        "pub mod inner {\n",
        "    pub struct S;\n",
        "    impl S {\n        pub fn b(&self) -> i32 { 2 }\n    }\n",
        "}\n",
    );
    let mod_ins = concat!(
        "pub mod inner {\n",
        "    pub struct S;\n",
        "    impl S {\n        pub fn c(&self) -> i32 { 3 }\n    }\n",
        "    impl S {\n        pub fn a(&self) -> i32 { 1 }\n    }\n",
        "    impl S {\n        pub fn b(&self) -> i32 { 99 }\n    }\n",
        "}\n",
    );
    let pe_del = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp1", "a1", 1), base_mod, mod_del).unwrap();
    let pe_ins = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp2", "a2", 2), base_mod, mod_ins).unwrap();
    let mut files_mod = BTreeMap::new();
    files_mod.insert(path.to_path_buf(), base_mod.as_bytes().to_vec());
    let re12 = merge_patches(&files_mod, &[pe_del.clone(), pe_ins.clone()], None).unwrap();
    let re21 = merge_patches(&files_mod, &[pe_ins, pe_del], None).unwrap();
    assert!(re12.is_clean(), "Case E conflicts: {:?}", re12.conflicts);
    assert_eq!(re12.text(path), re21.text(path));
    let me = re12.text(path).unwrap();
    assert!(me.contains("fn c("), "Case E missing c:\n{me}");
    assert!(!me.contains("fn a("), "Case E deleted a survived:\n{me}");
    assert!(me.contains("99"), "Case E missing edited b:\n{me}");
    assert!(!parse_source(LanguageKind::Rust, me, None).unwrap().tree.root_node().has_error());

    // Case F: SemanticConflict detection still fires when shifted surviving impl calls removed impl method
    let ins_and_call_a = concat!(
        "pub struct S;\n",
        "impl S {\n    pub fn c(&self) -> i32 { 3 }\n}\n",
        "impl S {\n    pub fn a(&self) -> i32 { 1 }\n}\n",
        "impl S {\n    pub fn b(&self) -> i32 { self.a() }\n}\n",
    );
    let pf_call_a = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp2", "a2", 2), base_2, ins_and_call_a).unwrap();
    let rf12 = merge_patches(&files_2, &[pb_del1.clone(), pf_call_a.clone()], None).unwrap();
    let rf21 = merge_patches(&files_2, &[pf_call_a, pb_del1], None).unwrap();
    assert!(
        matches!(rf12.outcome, MergeOutcome::SemanticConflict { ref symbol_fqn, .. } if symbol_fqn == "impl<S>::a"),
        "Case F expected SemanticConflict on impl<S>::a, got {:?}",
        rf12.outcome
    );
    assert_eq!(rf12.outcome, rf21.outcome);

    // Case G: Single patch inserts e before a AND f after a while concurrent patch deletes a
    let ins_e_and_f = concat!(
        "pub struct S;\n",
        "impl S {\n    pub fn e(&self) -> i32 { 5 }\n}\n",
        "impl S {\n    pub fn a(&self) -> i32 { 1 }\n}\n",
        "impl S {\n    pub fn f(&self) -> i32 { 6 }\n}\n",
        "impl S {\n    pub fn b(&self) -> i32 { 2 }\n}\n",
    );
    let pg_del = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp1", "a1", 1), base_2, del_1st).unwrap();
    let pg_ins = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp2", "a2", 2), base_2, ins_e_and_f).unwrap();
    let rg12 = merge_patches(&files_2, &[pg_del.clone(), pg_ins.clone()], None).unwrap();
    let rg21 = merge_patches(&files_2, &[pg_ins, pg_del], None).unwrap();
    assert!(rg12.is_clean(), "Case G conflicts: {:?}", rg12.conflicts);
    assert_eq!(rg12.text(path), rg21.text(path));
    let mg = rg12.text(path).unwrap();
    assert!(mg.contains("fn e("), "Case G missing e:\n{mg}");
    assert!(mg.contains("fn f("), "Case G missing f:\n{mg}");
    assert!(mg.contains("fn b("), "Case G missing b:\n{mg}");
    assert!(!mg.contains("fn a("), "Case G deleted a survived:\n{mg}");
    assert!(!parse_source(LanguageKind::Rust, mg, None).unwrap().tree.root_node().has_error());
}

#[test]
fn r5_concurrent_distinct_inherent_impl_insert_repro_and_three_way() {
    // Challenger 2 R4 probe_8 exact reproduction + 3-way permutation + MergeState::compose + dedup
    let path = Path::new("src/edge.rs");
    let base = "pub struct S;\n";
    let mod1 = "pub struct S;\n\nimpl S {\n    pub fn foo(&self) -> i32 { 1 }\n}\n";
    let mod2 = "pub struct S;\n\nimpl S {\n    pub fn bar(&self) -> i32 { 2 }\n}\n";
    let mod3 = "pub struct S;\n\nimpl S {\n    pub fn baz(&self) -> i32 { 3 }\n}\n";

    let p1 = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp1", "a1", 1), base, mod1).unwrap();
    let p2 = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp2", "a2", 2), base, mod2).unwrap();
    let p3 = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp3", "a3", 3), base, mod3).unwrap();

    let mut files = BTreeMap::new();
    files.insert(path.to_path_buf(), base.as_bytes().to_vec());

    // 2-way both patch orders
    let r12 = merge_patches(&files, &[p1.clone(), p2.clone()], None).unwrap();
    let r21 = merge_patches(&files, &[p2.clone(), p1.clone()], None).unwrap();
    assert!(r12.is_clean(), "expected Clean merge for [p1, p2], got {:?}", r12.conflicts);
    assert!(r21.is_clean(), "expected Clean merge for [p2, p1], got {:?}", r21.conflicts);
    assert_eq!(r12.text(path), r21.text(path));

    let merged2 = r12.text(path).unwrap();
    assert!(merged2.contains("pub fn foo(&self) -> i32 { 1 }"), "missing foo:\n{merged2}");
    assert!(merged2.contains("pub fn bar(&self) -> i32 { 2 }"), "missing bar:\n{merged2}");
    assert!(
        !parse_source(LanguageKind::Rust, merged2, None)
            .unwrap()
            .tree
            .root_node()
            .has_error(),
        "invalid Rust CST:\n{merged2}"
    );

    // 3-way all 6 permutations + MergeState::compose
    let perms = [
        [&p1, &p2, &p3],
        [&p1, &p3, &p2],
        [&p2, &p1, &p3],
        [&p2, &p3, &p1],
        [&p3, &p1, &p2],
        [&p3, &p2, &p1],
    ];
    let mut expected_3way: Option<String> = None;
    for perm in perms {
        let ordered_slice: Vec<_> = perm.into_iter().cloned().collect();
        let rep = merge_patches(&files, &ordered_slice, None).unwrap();
        assert!(rep.is_clean(), "3-way perm conflicts: {:?}", rep.conflicts);
        let txt = rep.text(path).unwrap().to_string();
        assert!(txt.contains("pub fn foo("), "missing foo:\n{txt}");
        assert!(txt.contains("pub fn bar("), "missing bar:\n{txt}");
        assert!(txt.contains("pub fn baz("), "missing baz:\n{txt}");
        assert!(!parse_source(LanguageKind::Rust, &txt, None).unwrap().tree.root_node().has_error());
        if let Some(ref exp) = expected_3way {
            assert_eq!(&txt, exp, "permutation invariance failed");
        } else {
            expected_3way = Some(txt);
        }
    }

    let s12 = MergeState::from_patches(files.clone(), &[p1.clone(), p2.clone()], None).unwrap();
    let s3 = MergeState::from_patches(files.clone(), std::slice::from_ref(&p3), None).unwrap();
    let composed_a = MergeState::compose(&s12, &s3, None).unwrap();
    let composed_b = MergeState::compose(&s3, &s12, None).unwrap();
    assert!(composed_a.report.is_clean());
    assert_eq!(composed_a.report.text(path), expected_3way.as_deref());
    assert_eq!(composed_b.report.text(path), expected_3way.as_deref());

    // Identical inherent impl inserted concurrently by two agents still deduplicates cleanly
    let p1_dup = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp4", "a4", 4), base, mod1).unwrap();
    let r_dup = merge_patches(&files, &[p1, p1_dup], None).unwrap();
    assert!(r_dup.is_clean(), "identical impl dedup conflicts: {:?}", r_dup.conflicts);
    let dup_txt = r_dup.text(path).unwrap();
    assert_eq!(dup_txt.matches("pub fn foo(").count(), 1, "expected deduplicated foo:\n{dup_txt}");
}

#[test]
fn r5_concurrent_distinct_inherent_impl_positions_nested_mod_and_merge_trees() {
    // 1. Before an existing impl S
    let lib_path = Path::new("src/lib.rs");
    let base_existing = concat!(
        "pub struct S;\n\n",
        "impl S {\n    pub fn base_fn(&self) -> i32 { 0 }\n}\n",
    );
    let before1 = concat!(
        "pub struct S;\n\n",
        "impl S {\n    pub fn foo(&self) -> i32 { 1 }\n}\n\n",
        "impl S {\n    pub fn base_fn(&self) -> i32 { 0 }\n}\n",
    );
    let before2 = concat!(
        "pub struct S;\n\n",
        "impl S {\n    pub fn bar(&self) -> i32 { 2 }\n}\n\n",
        "impl S {\n    pub fn base_fn(&self) -> i32 { 0 }\n}\n",
    );
    let pb1 = diff_source_to_patch(&make_ctx(lib_path, LanguageKind::Rust, "vp1", "a1", 1), base_existing, before1).unwrap();
    let pb2 = diff_source_to_patch(&make_ctx(lib_path, LanguageKind::Rust, "vp2", "a2", 2), base_existing, before2).unwrap();
    let mut files_existing = BTreeMap::new();
    files_existing.insert(lib_path.to_path_buf(), base_existing.as_bytes().to_vec());

    let rb12 = merge_patches(&files_existing, &[pb1.clone(), pb2.clone()], None).unwrap();
    let rb21 = merge_patches(&files_existing, &[pb2, pb1], None).unwrap();
    assert!(rb12.is_clean(), "before existing impl conflicts: {:?}", rb12.conflicts);
    assert_eq!(rb12.text(lib_path), rb21.text(lib_path));
    let mb = rb12.text(lib_path).unwrap();
    assert!(mb.contains("pub fn foo(") && mb.contains("pub fn bar(") && mb.contains("pub fn base_fn("));
    assert!(!parse_source(LanguageKind::Rust, mb, None).unwrap().tree.root_node().has_error());

    // 2. Byte-0 insertion before pub struct S
    let base_struct = "pub struct S;\n";
    let byte0_1 = "impl S {\n    pub fn foo(&self) -> i32 { 1 }\n}\n\npub struct S;\n";
    let byte0_2 = "impl S {\n    pub fn bar(&self) -> i32 { 2 }\n}\n\npub struct S;\n";
    let pz1 = diff_source_to_patch(&make_ctx(lib_path, LanguageKind::Rust, "vp1", "a1", 1), base_struct, byte0_1).unwrap();
    let pz2 = diff_source_to_patch(&make_ctx(lib_path, LanguageKind::Rust, "vp2", "a2", 2), base_struct, byte0_2).unwrap();
    let mut files_struct = BTreeMap::new();
    files_struct.insert(lib_path.to_path_buf(), base_struct.as_bytes().to_vec());
    let rz12 = merge_patches(&files_struct, &[pz1.clone(), pz2.clone()], None).unwrap();
    let rz21 = merge_patches(&files_struct, &[pz2, pz1], None).unwrap();
    assert!(rz12.is_clean(), "byte-0 conflicts: {:?}", rz12.conflicts);
    assert_eq!(rz12.text(lib_path), rz21.text(lib_path));
    let mz = rz12.text(lib_path).unwrap();
    assert!(mz.contains("pub fn foo(") && mz.contains("pub fn bar(") && mz.contains("pub struct S;"));
    assert!(!parse_source(LanguageKind::Rust, mz, None).unwrap().tree.root_node().has_error());

    // 3. Nested mod inner { ... } (empty and before existing impl)
    let mod_path = Path::new("src/nested.rs");
    let base_mod = concat!(
        "pub mod inner {\n",
        "    pub struct S;\n\n",
        "    impl S {\n        pub fn existing(&self) -> i32 { 0 }\n    }\n",
        "}\n",
    );
    let mod_a = concat!(
        "pub mod inner {\n",
        "    pub struct S;\n\n",
        "    /// Foo methods\n",
        "    impl S {\n        pub fn foo(&self) -> i32 { 1 }\n    }\n\n",
        "    impl S {\n        pub fn existing(&self) -> i32 { 0 }\n    }\n",
        "}\n",
    );
    let mod_b = concat!(
        "pub mod inner {\n",
        "    pub struct S;\n\n",
        "    /// Bar methods\n",
        "    impl S {\n        pub fn bar(&self) -> i32 { 2 }\n    }\n\n",
        "    impl S {\n        pub fn existing(&self) -> i32 { 0 }\n    }\n",
        "}\n",
    );
    let pm1 = diff_source_to_patch(&make_ctx(mod_path, LanguageKind::Rust, "vp1", "a1", 1), base_mod, mod_a).unwrap();
    let pm2 = diff_source_to_patch(&make_ctx(mod_path, LanguageKind::Rust, "vp2", "a2", 2), base_mod, mod_b).unwrap();
    let mut files_mod = BTreeMap::new();
    files_mod.insert(mod_path.to_path_buf(), base_mod.as_bytes().to_vec());
    let rm12 = merge_patches(&files_mod, &[pm1.clone(), pm2.clone()], None).unwrap();
    let rm21 = merge_patches(&files_mod, &[pm2, pm1], None).unwrap();
    assert!(rm12.is_clean(), "nested mod conflicts: {:?}", rm12.conflicts);
    assert_eq!(rm12.text(mod_path), rm21.text(mod_path));
    let mm = rm12.text(mod_path).unwrap();
    assert!(mm.contains("/// Foo methods") && mm.contains("pub fn foo("));
    assert!(mm.contains("/// Bar methods") && mm.contains("pub fn bar("));
    assert!(mm.contains("pub fn existing("));
    assert!(!parse_source(LanguageKind::Rust, mm, None).unwrap().tree.root_node().has_error());

    // 4. merge_trees with untouched app.rs calling s.foo() and s.bar()
    let app_path = Path::new("src/app.rs");
    let app_src = "use crate::S;\n\npub fn run(s: &S) -> i32 {\n    s.foo() + s.bar()\n}\n";
    let mut tree_files = BTreeMap::new();
    tree_files.insert(lib_path.to_path_buf(), base_struct.as_bytes().to_vec());
    tree_files.insert(app_path.to_path_buf(), app_src.as_bytes().to_vec());

    let lib_foo = "pub struct S;\n\nimpl S {\n    pub fn foo(&self) -> i32 { 1 }\n}\n";
    let lib_bar = "pub struct S;\n\nimpl S {\n    pub fn bar(&self) -> i32 { 2 }\n}\n";
    let pt1 = diff_source_to_patch(&make_ctx(lib_path, LanguageKind::Rust, "vp1", "a1", 1), base_struct, lib_foo).unwrap();
    let pt2 = diff_source_to_patch(&make_ctx(lib_path, LanguageKind::Rust, "vp2", "a2", 2), base_struct, lib_bar).unwrap();

    let tree_res_12 = merge_trees(
        &tree_files,
        &[std::slice::from_ref(&pt1), std::slice::from_ref(&pt2)],
        None,
    )
    .unwrap();
    let tree_res_21 = merge_trees(&tree_files, &[&[pt2], &[pt1]], None).unwrap();
    assert!(tree_res_12.is_clean(), "merge_trees conflicts: {:?}", tree_res_12.conflicts);
    assert!(tree_res_21.is_clean());
    assert_eq!(tree_res_12.text(lib_path), tree_res_21.text(lib_path));
    assert_eq!(tree_res_12.text(app_path), Some(app_src));
    let merged_lib = tree_res_12.text(lib_path).unwrap();
    assert!(merged_lib.contains("pub fn foo(") && merged_lib.contains("pub fn bar("));
}

#[test]
fn r5_concurrent_inherent_impl_colliding_member_conflicts() {
    let path = Path::new("src/collide.rs");
    let base = "pub struct S;\n";
    let mod1 = "pub struct S;\n\nimpl S {\n    pub fn foo(&self) -> i32 { 1 }\n}\n";
    let mod2 = "pub struct S;\n\nimpl S {\n    pub fn foo(&self) -> i32 { 2 }\n}\n";

    let p1 = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp1", "a1", 1), base, mod1).unwrap();
    let p2 = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp2", "a2", 2), base, mod2).unwrap();

    let mut files = BTreeMap::new();
    files.insert(path.to_path_buf(), base.as_bytes().to_vec());

    let r12 = merge_patches(&files, &[p1.clone(), p2.clone()], None).unwrap();
    let r21 = merge_patches(&files, &[p2, p1], None).unwrap();
    assert!(
        matches!(r12.outcome, MergeOutcome::StructuralConflict { .. }),
        "expected StructuralConflict for colliding fn foo, got {:?}",
        r12.outcome
    );
    assert!(matches!(r21.outcome, MergeOutcome::StructuralConflict { .. }));

    // Collision across distinct positional slots (#1 vs #2) before/after an existing impl
    let base_ex = "pub struct S;\n\nimpl S {\n    pub fn orig(&self) -> i32 { 0 }\n}\n";
    let before = "pub struct S;\n\nimpl S {\n    pub fn dup(&self) -> i32 { 1 }\n}\n\nimpl S {\n    pub fn orig(&self) -> i32 { 0 }\n}\n";
    let after = "pub struct S;\n\nimpl S {\n    pub fn orig(&self) -> i32 { 0 }\n}\n\nimpl S {\n    pub fn dup(&self) -> i32 { 2 }\n}\n";
    let pb = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp1", "a1", 1), base_ex, before).unwrap();
    let pa = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp2", "a2", 2), base_ex, after).unwrap();
    let mut files_ex = BTreeMap::new();
    files_ex.insert(path.to_path_buf(), base_ex.as_bytes().to_vec());
    let rex = merge_patches(&files_ex, &[pb, pa], None).unwrap();
    assert!(
        matches!(rex.outcome, MergeOutcome::StructuralConflict { .. }),
        "expected StructuralConflict for cross-slot colliding fn dup, got {:?}",
        rex.outcome
    );
}

#[test]
fn r5_concurrent_trait_impl_and_regular_symbol_still_conflict() {
    let path = Path::new("src/trait_and_fn.rs");
    let base = "pub trait Tr {\n    fn val(&self) -> i32;\n}\n\npub struct S;\n";
    let tr1 = "pub trait Tr {\n    fn val(&self) -> i32;\n}\n\npub struct S;\n\nimpl Tr for S {\n    fn val(&self) -> i32 { 1 }\n}\n";
    let tr2 = "pub trait Tr {\n    fn val(&self) -> i32;\n}\n\npub struct S;\n\nimpl Tr for S {\n    fn val(&self) -> i32 { 2 }\n}\n";

    let pt1 = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp1", "a1", 1), base, tr1).unwrap();
    let pt2 = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp2", "a2", 2), base, tr2).unwrap();

    let mut files = BTreeMap::new();
    files.insert(path.to_path_buf(), base.as_bytes().to_vec());

    let rt12 = merge_patches(&files, &[pt1.clone(), pt2.clone()], None).unwrap();
    let rt21 = merge_patches(&files, &[pt2, pt1], None).unwrap();
    assert!(
        matches!(rt12.outcome, MergeOutcome::StructuralConflict { .. }),
        "expected StructuralConflict for concurrent impl Tr for S, got {:?}",
        rt12.outcome
    );
    assert!(matches!(rt21.outcome, MergeOutcome::StructuralConflict { .. }));

    // Duplicate-named regular function insertion still conflicts
    let fn1 = "pub trait Tr {\n    fn val(&self) -> i32;\n}\n\npub struct S;\n\nfn helper() -> i32 { 1 }\n";
    let fn2 = "pub trait Tr {\n    fn val(&self) -> i32;\n}\n\npub struct S;\n\nfn helper() -> i32 { 2 }\n";
    let pf1 = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp1", "a1", 1), base, fn1).unwrap();
    let pf2 = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp2", "a2", 2), base, fn2).unwrap();
    let rf = merge_patches(&files, &[pf1, pf2], None).unwrap();
    assert!(
        matches!(rf.outcome, MergeOutcome::StructuralConflict { .. }),
        "expected StructuralConflict for concurrent fn helper, got {:?}",
        rf.outcome
    );
}

#[test]
fn f13_go_const_group_prepend_preserves_member_placement_and_edits() {
    let path = Path::new("main.go");
    let base = "package main\n\nconst (\n\tA = 1\n\tB = 2\n)\n";
    let vp_prepend = "package main\n\nconst (\n\tX = 10\n)\n\nconst (\n\tA = 1\n\tB = 2\n)\n";
    let vp_insert = "package main\n\nconst (\n\tA = 1\n\tB = 2\n\tC = 3\n)\n";
    let vp_edit = "package main\n\nconst (\n\tA = 100\n\tB = 2\n)\n";

    let p_pre = diff_source_to_patch(&make_ctx(path, LanguageKind::Go, "vp1", "a1", 1), base, vp_prepend).unwrap();
    let p_ins = diff_source_to_patch(&make_ctx(path, LanguageKind::Go, "vp2", "a2", 2), base, vp_insert).unwrap();
    let p_edt = diff_source_to_patch(&make_ctx(path, LanguageKind::Go, "vp3", "a3", 3), base, vp_edit).unwrap();

    let mut base_files = BTreeMap::new();
    base_files.insert(path.to_path_buf(), base.as_bytes().to_vec());

    let rep_ins = merge_patches(&base_files, &[p_pre.clone(), p_ins], None).unwrap();
    assert!(rep_ins.is_clean(), "prepend + insert should merge cleanly: {:?}", rep_ins.outcome);
    let merged_ins = rep_ins.text(path).unwrap();
    let x_pos = merged_ins.find("X = 10").unwrap();
    let a_pos = merged_ins.find("A = 1").unwrap();
    let c_pos = merged_ins.find("C = 3").unwrap();
    assert!(
        x_pos < a_pos && a_pos < c_pos,
        "C = 3 must be inserted into the (A, B) const block, not (X):\n{merged_ins}"
    );

    let rep_edt = merge_patches(&base_files, &[p_pre, p_edt], None).unwrap();
    assert!(rep_edt.is_clean(), "prepend + member edit must not false-conflict: {:?}", rep_edt.outcome);
    let merged_edt = rep_edt.text(path).unwrap();
    assert!(merged_edt.contains("X = 10"));
    assert!(merged_edt.contains("A = 100"));
}

#[test]
fn f13_cst_insert_member_preserves_crlf_line_endings() {
    let path = Path::new("src/crlf.rs");
    let base = "pub fn a() -> i32 {\r\n    1\r\n}\r\n\r\npub fn b() -> i32 {\r\n    2\r\n}\r\n";
    let vp_a = "pub fn a() -> i32 {\r\n    1\r\n}\r\n\r\npub fn mid() -> i32 {\r\n    9\r\n}\r\n\r\npub fn b() -> i32 {\r\n    2\r\n}\r\n";
    let vp_b = "pub fn a() -> i32 {\r\n    1\r\n}\r\n\r\npub fn b() -> i32 {\r\n    20\r\n}\r\n";

    let pa = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp1", "a1", 1), base, vp_a).unwrap();
    let pb = diff_source_to_patch(&make_ctx(path, LanguageKind::Rust, "vp2", "a2", 2), base, vp_b).unwrap();
    let mut base_files = BTreeMap::new();
    base_files.insert(path.to_path_buf(), base.as_bytes().to_vec());

    let rep = merge_patches(&base_files, &[pa, pb], None).unwrap();
    assert!(rep.is_clean(), "expected clean merge, got {:?}", rep.outcome);
    let merged = rep.text(path).unwrap();
    let bare_lf = merged.matches('\n').count() - merged.matches("\r\n").count();
    assert_eq!(bare_lf, 0, "expected pure CRLF without bare LF:\n{merged:?}");

    // Also verify Python CRLF empty-class `pass` replacement
    let py_path = Path::new("empty.py");
    let py_base = "class Empty: pass\r\n";
    let py_mod = "class Empty:\r\n    def a(self):\r\n        return 1\r\n";
    let py_mod2 = "class Empty:\r\n    def b(self):\r\n        return 2\r\n";
    let ppa = diff_source_to_patch(&make_ctx(py_path, LanguageKind::Python, "vp1", "a1", 1), py_base, py_mod).unwrap();
    let ppb = diff_source_to_patch(&make_ctx(py_path, LanguageKind::Python, "vp2", "a2", 2), py_base, py_mod2).unwrap();
    let mut py_files = BTreeMap::new();
    py_files.insert(py_path.to_path_buf(), py_base.as_bytes().to_vec());
    let py_rep = merge_patches(&py_files, &[ppa, ppb], None).unwrap();
    assert!(py_rep.is_clean(), "expected clean python merge, got {:?}", py_rep.outcome);
    let py_merged = py_rep.text(py_path).unwrap();
    let py_bare_lf = py_merged.matches('\n').count() - py_merged.matches("\r\n").count();
    assert_eq!(py_bare_lf, 0, "expected pure CRLF in python merge:\n{py_merged:?}");
}

#[test]
fn f13_pass0_does_not_cross_match_zero_overlap_rust_impl_or_go_const_group() {
    let rs_path = Path::new("src/lib.rs");
    let rs_base = "pub struct Foo;\n\nimpl Foo {\n    pub fn a(&self) -> i32 { 1 }\n}\n\nimpl Foo {\n    pub fn b(&self) -> i32 { 2 }\n}\n";
    let rs_a1 = "pub struct Foo;\n\nimpl Foo {\n    pub fn b(&self) -> i32 { 2 }\n}\n\nimpl Foo {\n    pub fn c(&self) -> i32 { 3 }\n}\n";
    let rs_a2 = "pub struct Foo;\n\nimpl Foo {\n    pub fn b(&self) -> i32 { 2 }\n}\n";

    let rp1 = diff_source_to_patch(&make_ctx(rs_path, LanguageKind::Rust, "vp1", "a1", 1), rs_base, rs_a1).unwrap();
    let rp2 = diff_source_to_patch(&make_ctx(rs_path, LanguageKind::Rust, "vp2", "a2", 2), rs_base, rs_a2).unwrap();
    let mut rs_files = BTreeMap::new();
    rs_files.insert(rs_path.to_path_buf(), rs_base.as_bytes().to_vec());
    let rs_rep = merge_patches(&rs_files, &[rp1, rp2], None).unwrap();
    assert!(rs_rep.is_clean(), "Rust impl delete+add vs delete must merge cleanly: {:?}", rs_rep.outcome);
    let rs_merged = rs_rep.text(rs_path).unwrap();
    assert!(!rs_merged.contains("fn a"));
    assert!(rs_merged.contains("fn b"));
    assert!(rs_merged.contains("fn c"));

    let go_path = Path::new("main.go");
    let go_base = "package main\n\nconst (\n    A = 1\n)\n\nconst (\n    B = 2\n)\n";
    let go_a1 = "package main\n\nconst (\n    B = 2\n)\n\nconst (\n    C = 3\n)\n";
    let go_a2 = "package main\n\nconst (\n    B = 2\n)\n";

    let gp1 = diff_source_to_patch(&make_ctx(go_path, LanguageKind::Go, "vp1", "a1", 1), go_base, go_a1).unwrap();
    let gp2 = diff_source_to_patch(&make_ctx(go_path, LanguageKind::Go, "vp2", "a2", 2), go_base, go_a2).unwrap();
    let mut go_files = BTreeMap::new();
    go_files.insert(go_path.to_path_buf(), go_base.as_bytes().to_vec());
    let go_rep = merge_patches(&go_files, &[gp1, gp2], None).unwrap();
    assert!(go_rep.is_clean(), "Go const_group delete+add vs delete must merge cleanly: {:?}", go_rep.outcome);
    let go_merged = go_rep.text(go_path).unwrap();
    assert!(!go_merged.contains("A = 1"));
    assert!(go_merged.contains("B = 2"));
    assert!(go_merged.contains("C = 3"));
}

#[test]
fn f13_ts_method_signature_overloads_disambiguated_and_sig_overload_triggers_semantic_conflict() {
    // 1. Disjoint edits to overloaded interface method_signatures must merge cleanly
    let svc_path = Path::new("src/svc.ts");
    let svc_base = "export interface Service {\n    handle(x: string): string;\n    handle(x: number): number;\n}\n";
    let svc_a1 = "export interface Service {\n    handle(x: string): boolean;\n    handle(x: number): number;\n}\n";
    let svc_a2 = "export interface Service {\n    handle(x: string): string;\n    handle(x: number): void;\n}\n";

    let sp1 = diff_source_to_patch(&make_ctx(svc_path, LanguageKind::TypeScript, "vp1", "a1", 1), svc_base, svc_a1).unwrap();
    let sp2 = diff_source_to_patch(&make_ctx(svc_path, LanguageKind::TypeScript, "vp2", "a2", 2), svc_base, svc_a2).unwrap();
    let mut svc_files = BTreeMap::new();
    svc_files.insert(svc_path.to_path_buf(), svc_base.as_bytes().to_vec());
    let svc_rep = merge_patches(&svc_files, &[sp1, sp2], None).unwrap();
    assert!(svc_rep.is_clean(), "disjoint method_signature overloads must merge cleanly: {:?}", svc_rep.outcome);
    let merged_svc = svc_rep.text(svc_path).unwrap();
    assert!(merged_svc.contains("handle(x: string): boolean;"));
    assert!(merged_svc.contains("handle(x: number): void;"));

    // 2. Changing only overload #sig:2 must still trigger SemanticConflict against caller
    let api_path = Path::new("src/over.ts");
    let caller_path = Path::new("src/use.ts");
    let api_base = "export function fmt(x: string): string;\nexport function fmt(x: number): string;\nexport function fmt(x: any): string {\n    return String(x);\n}\n";
    let api_new = "export function fmt(x: string): string;\nexport function fmt(x: number, pad: number): string;\nexport function fmt(x: any): string {\n    return String(x);\n}\n";
    let caller_base = "import { fmt } from './over';\nexport function run() { return fmt(123); }\n";
    let caller_new = "import { fmt } from './over';\nexport function run() { const r = fmt(123); return r; }\n";

    let pa = diff_source_to_patch(&make_ctx(api_path, LanguageKind::TypeScript, "vp1", "a1", 1), api_base, api_new).unwrap();
    let pc = diff_source_to_patch(&make_ctx(caller_path, LanguageKind::TypeScript, "vp2", "a2", 2), caller_base, caller_new).unwrap();
    let mut files = BTreeMap::new();
    files.insert(api_path.to_path_buf(), api_base.as_bytes().to_vec());
    files.insert(caller_path.to_path_buf(), caller_base.as_bytes().to_vec());
    let rep = merge_patches(&files, &[pa, pc], None).unwrap();
    assert!(
        matches!(rep.outcome, MergeOutcome::SemanticConflict { .. }),
        "overload #sig:2 signature change must trigger SemanticConflict, got {:?}",
        rep.outcome
    );
}



#[test]
fn f13_divergent_base_conflicts_catches_crdt_and_rename_collisions() {
    let tmp = TempDir::new().unwrap();
    let cas = CasStore::open_default(tmp.path().join("cas")).unwrap();

    let lib_path = Path::new("src/lib.rs");
    let json_path = Path::new("config.json");
    let ren_old = Path::new("src/a.rs");
    let ren_x = Path::new("src/x.rs");
    let ren_y = Path::new("src/y.rs");

    let base_a_lib = b"pub fn old_fn() -> i32 { 1 }\npub fn keep_fn() -> i32 { 2 }\n";
    let base_b_lib = b"pub fn keep_fn() -> i32 { 2 }\npub fn added_on_trunk() -> i32 { 3 }\n";
    let base_json = b"{\n  \"a\": 1\n}\n";
    let ren_bytes = b"pub fn ren() -> i32 { 7 }\n";

    let pa_lib = diff_file_to_patch(
        &make_ctx(lib_path, LanguageKind::Rust, "vp-a", "a", 1),
        base_a_lib,
        b"pub fn old_fn() -> i32 { 10 }\npub fn keep_fn() -> i32 { 2 }\n",
        None,
        Some(&cas),
    )
    .unwrap();
    let pb_lib = diff_file_to_patch(
        &make_ctx(lib_path, LanguageKind::Rust, "vp-b", "b", 2),
        base_b_lib,
        b"pub fn keep_fn() -> i32 { 2 }\npub fn added_on_trunk() -> i32 { 30 }\n",
        None,
        Some(&cas),
    )
    .unwrap();
    let pa_json = diff_file_to_patch(
        &make_ctx(json_path, LanguageKind::Json, "vp-a", "a", 1),
        base_json,
        b"{\n  \"a\": 1,\n  \"k\": 10\n}\n",
        None,
        Some(&cas),
    )
    .unwrap();
    let pb_json = diff_file_to_patch(
        &make_ctx(json_path, LanguageKind::Json, "vp-b", "b", 2),
        base_json,
        b"{\n  \"a\": 1,\n  \"k\": 20\n}\n",
        None,
        Some(&cas),
    )
    .unwrap();
    let pa_ren = diff_file_to_patch(
        &make_ctx(ren_x, LanguageKind::Rust, "vp-a", "a", 1),
        ren_bytes,
        ren_bytes,
        Some(ren_old),
        Some(&cas),
    )
    .unwrap();
    let pb_ren = diff_file_to_patch(
        &make_ctx(ren_y, LanguageKind::Rust, "vp-b", "b", 2),
        ren_bytes,
        ren_bytes,
        Some(ren_old),
        Some(&cas),
    )
    .unwrap();

    let mut base_a = BTreeMap::new();
    base_a.insert(lib_path.to_path_buf(), base_a_lib.to_vec());
    base_a.insert(json_path.to_path_buf(), base_json.to_vec());
    base_a.insert(ren_old.to_path_buf(), ren_bytes.to_vec());

    let mut base_b = BTreeMap::new();
    base_b.insert(lib_path.to_path_buf(), base_b_lib.to_vec());
    base_b.insert(json_path.to_path_buf(), base_json.to_vec());
    base_b.insert(ren_old.to_path_buf(), ren_bytes.to_vec());

    let conflicts = loom_core::cst::detect_divergent_base_conflicts(
        &base_a,
        &[pa_lib, pa_json, pa_ren],
        &base_b,
        &[pb_lib, pb_json, pb_ren],
    )
    .unwrap();

    assert!(
        conflicts.iter().any(|c| matches!(c, MergeOutcome::StructuralConflict { file, .. } if file == json_path)),
        "expected JSON CRDT duplicate-key StructuralConflict: {conflicts:?}"
    );
    assert!(
        conflicts.iter().any(|c| matches!(c, MergeOutcome::StructuralConflict { reason, .. } if reason.contains("RenameOrMoveFile"))),
        "expected RenameOrMoveFile StructuralConflict: {conflicts:?}"
    );
}

#[test]
fn iter4_go_group_remove_vs_child_edit_and_crdt_and_python_future_import_fixes() {
    // 1. Go type_group & const_group RemoveNode vs child edit -> StructuralConflict
    let go_types = Path::new("pkg/types.go");
    let go_base = "package pkg\n\ntype (\n\tFoo struct {\n\t\tX int\n\t}\n)\n\nfunc Keep() int { return 1 }\n";
    let go_vp1 = "package pkg\n\nfunc Keep() int { return 1 }\n";
    let go_vp2 = "package pkg\n\ntype (\n\tFoo struct {\n\t\tX int\n\t\tY int\n\t}\n)\n\nfunc Keep() int { return 1 }\n";
    let gp1 = diff_source_to_patch(
        &make_ctx(go_types, LanguageKind::Go, "vp1", "a1", 1),
        go_base,
        go_vp1,
    )
    .unwrap();
    let gp2 = diff_source_to_patch(
        &make_ctx(go_types, LanguageKind::Go, "vp2", "a2", 2),
        go_base,
        go_vp2,
    )
    .unwrap();
    let mut gbase = BTreeMap::new();
    gbase.insert(go_types.to_path_buf(), go_base.as_bytes().to_vec());
    let grep = merge_patches(&gbase, &[gp1, gp2], None).unwrap();
    assert!(
        matches!(grep.outcome, MergeOutcome::StructuralConflict { .. }),
        "Go type_group RemoveNode vs child InsertMember must conflict: {:?}",
        grep.outcome
    );

    let go_consts = Path::new("pkg/consts.go");
    let c_base = "package pkg\n\nconst (\n\tA = 1\n\tB = 2\n)\n\nfunc Keep() int { return 1 }\n";
    let c_vp1 = "package pkg\n\nfunc Keep() int { return 1 }\n";
    let c_vp2 = "package pkg\n\nconst (\n\tA = 99\n\tB = 2\n)\n\nfunc Keep() int { return 1 }\n";
    let cp1 = diff_source_to_patch(
        &make_ctx(go_consts, LanguageKind::Go, "vp1", "a1", 1),
        c_base,
        c_vp1,
    )
    .unwrap();
    let cp2 = diff_source_to_patch(
        &make_ctx(go_consts, LanguageKind::Go, "vp2", "a2", 2),
        c_base,
        c_vp2,
    )
    .unwrap();
    let mut cbase = BTreeMap::new();
    cbase.insert(go_consts.to_path_buf(), c_base.as_bytes().to_vec());
    let crep = merge_patches(&cbase, &[cp1, cp2], None).unwrap();
    assert!(
        matches!(crep.outcome, MergeOutcome::StructuralConflict { .. }),
        "Go const_group RemoveNode vs child MutateSignature must conflict: {:?}",
        crep.outcome
    );

    // 2. Divergent-base disjoint JSON & Markdown edits -> no false StructuralConflict
    let json_p = Path::new("config.json");
    let jb_a = "{\n  \"a\": 1,\n  \"b\": 2\n}\n";
    let jv_a = "{\n  \"a\": 10,\n  \"b\": 2\n}\n";
    let jb_b = "{\n  \"a\": 1,\n  \"b\": 2,\n  \"c\": 3\n}\n";
    let jv_b = "{\n  \"a\": 1,\n  \"b\": 20,\n  \"c\": 3\n}\n";
    let jpa = diff_file_to_patch(
        &make_ctx(json_p, LanguageKind::Json, "vp1", "a1", 1),
        jb_a.as_bytes(),
        jv_a.as_bytes(),
        None,
        None,
    )
    .unwrap();
    let jpb = diff_file_to_patch(
        &make_ctx(json_p, LanguageKind::Json, "vp2", "a2", 2),
        jb_b.as_bytes(),
        jv_b.as_bytes(),
        None,
        None,
    )
    .unwrap();
    let md_p = Path::new("README.md");
    let mb_a = "# Title\n\n## Alpha\none\n\n## Beta\ntwo\n";
    let mv_a = "# Title\n\n## Alpha\none updated\n\n## Beta\ntwo\n";
    let mb_b = "# Title\n\n## Alpha\none\n\n## Beta\ntwo\n\n## Gamma\nthree\n";
    let mv_b = "# Title\n\n## Alpha\none\n\n## Beta\ntwo updated\n\n## Gamma\nthree\n";
    let mpa = diff_file_to_patch(
        &make_ctx(md_p, LanguageKind::Markdown, "vp1", "a1", 1),
        mb_a.as_bytes(),
        mv_a.as_bytes(),
        None,
        None,
    )
    .unwrap();
    let mpb = diff_file_to_patch(
        &make_ctx(md_p, LanguageKind::Markdown, "vp2", "a2", 2),
        mb_b.as_bytes(),
        mv_b.as_bytes(),
        None,
        None,
    )
    .unwrap();
    let mut base_a = BTreeMap::new();
    base_a.insert(json_p.to_path_buf(), jb_a.as_bytes().to_vec());
    base_a.insert(md_p.to_path_buf(), mb_a.as_bytes().to_vec());
    let mut base_b = BTreeMap::new();
    base_b.insert(json_p.to_path_buf(), jb_b.as_bytes().to_vec());
    base_b.insert(md_p.to_path_buf(), mb_b.as_bytes().to_vec());
    let d_conflicts =
        loom_core::cst::detect_divergent_base_conflicts(&base_a, &[jpa, mpa], &base_b, &[jpb, mpb])
            .unwrap();
    assert!(
        d_conflicts.is_empty(),
        "disjoint divergent JSON and Markdown should not conflict: {d_conflicts:?}"
    );

    // 3. Convergent deletion of JSON -> Clean (file removed), while delete-vs-modify -> StructuralConflict
    let mut jbase_only = BTreeMap::new();
    jbase_only.insert(json_p.to_path_buf(), jb_a.as_bytes().to_vec());
    let del_a = diff_file_to_patch(
        &make_ctx(json_p, LanguageKind::Json, "vp1", "a1", 1),
        jb_a.as_bytes(),
        b"",
        None,
        None,
    )
    .unwrap();
    let del_b = diff_file_to_patch(
        &make_ctx(json_p, LanguageKind::Json, "vp2", "a2", 2),
        jb_a.as_bytes(),
        b"",
        None,
        None,
    )
    .unwrap();
    let del_rep = merge_patches(&jbase_only, &[del_a.clone(), del_b], None).unwrap();
    assert!(
        matches!(del_rep.outcome, MergeOutcome::Clean { .. }),
        "convergent JSON delete should be Clean: {:?}",
        del_rep.outcome
    );
    assert!(!del_rep.merged_files.contains_key(json_p));

    let mod_b = diff_file_to_patch(
        &make_ctx(json_p, LanguageKind::Json, "vp2", "a2", 2),
        jb_a.as_bytes(),
        jv_a.as_bytes(),
        None,
        None,
    )
    .unwrap();
    let del_vs_mod = merge_patches(&jbase_only, &[del_a, mod_b], None).unwrap();
    assert!(
        matches!(del_vs_mod.outcome, MergeOutcome::StructuralConflict { .. }),
        "delete-vs-modify JSON must emit StructuralConflict: {:?}",
        del_vs_mod.outcome
    );

    // 4. Python root InsertMember preserves `from __future__ import` & `import` at top
    let py_p = Path::new("app/mod.py");
    let py_base = "from __future__ import annotations\nimport os\n\ndef existing() -> str:\n    return os.getcwd()\n";
    let py_vp1 = "from __future__ import annotations\nimport os\n\ndef helper() -> int:\n    return 42\n\ndef existing() -> str:\n    return os.getcwd()\n";
    let py_vp2 = "from __future__ import annotations\nimport os\n\ndef existing() -> str:\n    return os.path.abspath('.')\n";
    let pp1 = diff_source_to_patch(
        &make_ctx(py_p, LanguageKind::Python, "vp1", "a1", 1),
        py_base,
        py_vp1,
    )
    .unwrap();
    let pp2 = diff_source_to_patch(
        &make_ctx(py_p, LanguageKind::Python, "vp2", "a2", 2),
        py_base,
        py_vp2,
    )
    .unwrap();
    let mut pbase = BTreeMap::new();
    pbase.insert(py_p.to_path_buf(), py_base.as_bytes().to_vec());
    let prep = merge_patches(&pbase, &[pp1, pp2], None).unwrap();
    assert!(
        matches!(prep.outcome, MergeOutcome::Clean { .. }),
        "Python root InsertMember should merge cleanly: {:?}",
        prep.outcome
    );
    let merged_py = prep.text(py_p).unwrap();
    let future_pos = merged_py.find("from __future__ import annotations").unwrap();
    let import_pos = merged_py.find("import os").unwrap();
    let helper_pos = merged_py.find("def helper()").unwrap();
    assert!(
        future_pos < import_pos && import_pos < helper_pos,
        "unexpected order:\n{merged_py}"
    );
}

#[test]
fn ts_non_exported_namespace_and_ambient_declarations_preserved_and_merged() {
    let path = Path::new("src/ambient_ns.ts");
    let fid = compute_logical_file_id(path);
    let base = "\
namespace Internal {
    export function helper(n: number): number {
        return n + 1;
    }
}

namespace A.B {
    export function nested(x: number): number {
        return x * 2;
    }
}

declare namespace DecNs {
    function ping(): void;
}

declare module 'pkg' {
    export function modApi(): void;
}

declare module 'shortpkg';

export declare function extApi(x: string): number;

declare global {
    interface Window {
        __loom: boolean;
    }
}
";
    let vp1 = "\
namespace Internal {
    export function helper(n: number): number {
        return n + 10;
    }
    export function extra(): string {
        return 'ok';
    }
}

namespace A.B {
    export function nested(x: number): number {
        return x * 3;
    }
}

declare namespace DecNs {
    function ping(): void;
}

declare module 'pkg' {
    export function modApi(): void;
}

declare module 'shortpkg';

export declare function extApi(x: string, y: number): number;

declare global {
    interface Window {
        __loom: boolean;
    }
}
";
    let vp2 = "\
namespace Internal {
    export function helper(n: number): number {
        return n + 1;
    }
}

namespace A.B {
    export function nested(x: number): number {
        return x * 2;
    }
    export function addedB(): number {
        return 7;
    }
}

declare namespace DecNs {
    function ping(): void;
    function pong(id: number): string;
}

declare module 'pkg' {
    export function modApi(): void;
    export function modApi2(k: string): boolean;
}

declare module 'shortpkg';

export declare function extApi(x: string): number;

declare global {
    interface Window {
        __loom: boolean;
        __version: string;
    }
}
";

    let p1 = diff_source_to_patch(
        &make_ctx(path, LanguageKind::TypeScript, "vp1", "a1", 1),
        base,
        vp1,
    )
    .unwrap();
    let p2 = diff_source_to_patch(
        &make_ctx(path, LanguageKind::TypeScript, "vp2", "a2", 2),
        base,
        vp2,
    )
    .unwrap();

    assert!(
        !p1.ops.is_empty(),
        "namespace/declare edits must emit SemanticOps"
    );
    assert!(
        !p2.ops.is_empty(),
        "ambient namespace/module/global edits must emit SemanticOps"
    );

    let mut base_files = BTreeMap::new();
    base_files.insert(path.to_path_buf(), base.as_bytes().to_vec());

    let r12 = merge_patches(&base_files, &[p1.clone(), p2.clone()], None).unwrap();
    let r21 = merge_patches(&base_files, &[p2, p1], None).unwrap();
    assert!(r12.is_clean(), "unexpected conflicts: {:?}", r12.conflicts);
    assert_eq!(r12.text(path), r21.text(path));

    let merged = r12.text(path).unwrap();
    let parsed = parse_source(LanguageKind::TypeScript, merged, None).unwrap();
    assert!(
        !parsed.tree.root_node().has_error(),
        "merged TS has syntax error:\n{merged}"
    );
    assert!(merged.contains("return n + 10;"));
    assert!(merged.contains("export function extra()"));
    assert!(merged.contains("return x * 3;"));
    assert!(merged.contains("export function addedB()"));
    assert!(merged.contains("function pong(id: number): string;"));
    assert!(merged.contains("export function modApi2(k: string): boolean;"));
    assert!(merged.contains("declare module 'shortpkg';"));
    assert!(merged.contains("export declare function extApi(x: string, y: number): number;"));
    assert!(merged.contains("__version: string;"));

    // Verify extract_file_symbols / Radar provides inside non-exported namespace & declare blocks
    let syms = extract_file_symbols(&parsed, fid, path, "vp1", 0);
    let provided_fqns: Vec<&str> = syms
        .provides
        .iter()
        .map(|p| p.anchor.symbol_fqn.as_str())
        .collect();
    for expected in [
        "Internal",
        "Internal::helper",
        "Internal::extra",
        "A.B",
        "A.B::nested",
        "A.B::addedB",
        "DecNs",
        "DecNs::ping",
        "DecNs::pong",
        "'pkg'",
        "'pkg'::modApi",
        "'pkg'::modApi2",
        "'shortpkg'",
        "extApi",
        "global",
        "global::Window",
        "global::Window::__loom",
        "global::Window::__version",
    ] {
        assert!(
            provided_fqns.contains(&expected),
            "missing provided symbol `{expected}` in {provided_fqns:?}"
        );
    }

    // Verify declare vs non-declare have distinct decl_header_hash
    let non_dec_src = "function sameSig(x: number): void;\n";
    let dec_src = "declare function sameSig(x: number): void;\n";
    let u_non_dec = extract_decl_units(
        LanguageKind::TypeScript,
        &parse_source(LanguageKind::TypeScript, non_dec_src, None)
            .unwrap()
            .tree,
        non_dec_src,
        fid,
    );
    let u_dec = extract_decl_units(
        LanguageKind::TypeScript,
        &parse_source(LanguageKind::TypeScript, dec_src, None)
            .unwrap()
            .tree,
        dec_src,
        fid,
    );
    assert_ne!(
        u_non_dec[0].anchor.decl_header_hash,
        u_dec[0].anchor.decl_header_hash,
        "declare vs non-declare must produce distinct decl_header_hash"
    );

    // Verify TSX non-exported namespace + declare class/const + RemoveNode
    let tsx_path = Path::new("src/widget.tsx");
    let tsx_base = "\
namespace WidgetNs {
    export function renderNum(): number { return 1; }
    export function obsolete(): number { return 0; }
}
declare class DecWidget {
    id: number;
}
declare const WIDGET_FLAG: boolean;
";
    let tsx_v1 = "\
namespace WidgetNs {
    export function renderNum(): number { return 42; }
    export function obsolete(): number { return 0; }
}
declare class DecWidget {
    id: number;
}
declare const WIDGET_FLAG: boolean;
";
    let tsx_v2 = "\
namespace WidgetNs {
    export function renderNum(): number { return 1; }
}
declare class DecWidget {
    id: number;
    label: string;
}
declare const WIDGET_FLAG: boolean;
";
    let tp1 = diff_source_to_patch(
        &make_ctx(tsx_path, LanguageKind::Tsx, "vp1", "a1", 1),
        tsx_base,
        tsx_v1,
    )
    .unwrap();
    let tp2 = diff_source_to_patch(
        &make_ctx(tsx_path, LanguageKind::Tsx, "vp2", "a2", 2),
        tsx_base,
        tsx_v2,
    )
    .unwrap();
    let mut tsx_files = BTreeMap::new();
    tsx_files.insert(tsx_path.to_path_buf(), tsx_base.as_bytes().to_vec());
    let trep = merge_patches(&tsx_files, &[tp1, tp2], None).unwrap();
    assert!(trep.is_clean(), "TSX merge conflicts: {:?}", trep.conflicts);
    let tmerged = trep.text(tsx_path).unwrap();
    assert!(tmerged.contains("return 42;"));
    assert!(!tmerged.contains("obsolete"));
    assert!(tmerged.contains("label: string;"));
    assert!(tmerged.contains("declare const WIDGET_FLAG: boolean;"));
}

#[test]
fn go_const_var_type_group_survives_other_group_insert_above_without_false_conflict() {
    let path = Path::new("pkg/cfg.go");
    let fid = compute_logical_file_id(path);
    let base_src = "package pkg\n\nconst (\n\tA = 1\n\tB = 2\n)\n";
    let vp1_src = "package pkg\n\nvar (\n\tX = 42\n)\n\nconst (\n\tA = 10\n\tB = 2\n)\n";
    let vp2_src = "package pkg\n\nconst (\n\tA = 1\n\tB = 2\n\tC = 3\n)\n";

    let p1 = diff_source_to_patch(
        &DiffContext {
            logical_file_id: fid,
            file_path: path,
            language: LanguageKind::Go,
            viewport_id: "vp1",
            actor_id: "a1",
            lamport_ts: 1,
            parents: Vec::new(),
        },
        base_src,
        vp1_src,
    )
    .unwrap();
    assert!(
        !p1.ops.iter().any(|op| matches!(op, SemanticOp::RemoveNode { .. })),
        "inserting var(...) above const(...) must not emit RemoveNode for const_group: {:?}",
        p1.ops
    );
    assert!(
        !p1.contract_deltas
            .iter()
            .any(|d| matches!(d, SymbolContractDelta::RemovedSymbol { .. })),
        "inserting var(...) above const(...) must not emit RemovedSymbol: {:?}",
        p1.contract_deltas
    );

    let p2 = diff_source_to_patch(
        &DiffContext {
            logical_file_id: fid,
            file_path: path,
            language: LanguageKind::Go,
            viewport_id: "vp2",
            actor_id: "a2",
            lamport_ts: 2,
            parents: Vec::new(),
        },
        base_src,
        vp2_src,
    )
    .unwrap();

    let mut base = BTreeMap::new();
    base.insert(path.to_path_buf(), base_src.as_bytes().to_vec());
    let rep = merge_patches(&base, &[p1, p2], None).unwrap();
    assert!(
        rep.is_clean(),
        "expected clean Go merge, got: {:?}",
        rep.conflicts
    );
    let merged = rep.text(path).unwrap();
    assert!(merged.contains("X = 42"));
    assert!(merged.contains("A = 10"));
    assert!(merged.contains("C = 3"));
}

#[test]
fn ts_function_and_method_overload_delete_merges_cleanly_with_peer_second_overload_edit() {
    let path = Path::new("src/api.ts");
    let fid = compute_logical_file_id(path);
    let base_src = "export function foo(a: string): string;\nexport function foo(a: number): number;\nexport function foo(a: string | number): string | number {\n    return a;\n}\n";
    let vp1_src = "export function foo(a: number): number;\nexport function foo(a: string | number): string | number {\n    return a;\n}\n";
    let vp2_src = "export function foo(a: string): string;\nexport function foo(a: number): bigint;\nexport function foo(a: string | number): string | number | bigint {\n    return a;\n}\n";

    let p1 = diff_source_to_patch(
        &DiffContext {
            logical_file_id: fid,
            file_path: path,
            language: LanguageKind::TypeScript,
            viewport_id: "vp1",
            actor_id: "a1",
            lamport_ts: 1,
            parents: Vec::new(),
        },
        base_src,
        vp1_src,
    )
    .unwrap();
    assert!(
        !p1.contract_deltas.iter().any(|d| matches!(
            d,
            SymbolContractDelta::RemovedSymbol { symbol_fqn, .. } if symbol_fqn == "foo#sig:2"
        )),
        "deleting first overload must not emit spurious RemovedSymbol for surviving shifted foo#sig:2: {:?}",
        p1.contract_deltas
    );
    let p2 = diff_source_to_patch(
        &DiffContext {
            logical_file_id: fid,
            file_path: path,
            language: LanguageKind::TypeScript,
            viewport_id: "vp2",
            actor_id: "a2",
            lamport_ts: 2,
            parents: Vec::new(),
        },
        base_src,
        vp2_src,
    )
    .unwrap();

    let mut base = BTreeMap::new();
    base.insert(path.to_path_buf(), base_src.as_bytes().to_vec());
    let rep = merge_patches(&base, &[p1, p2], None).unwrap();
    assert!(
        rep.is_clean(),
        "expected clean TS overload merge, got: {:?}",
        rep.conflicts
    );
    let merged = rep.text(path).unwrap();
    assert!(!merged.contains("a: string): string;"));
    assert!(merged.contains("export function foo(a: number): bigint;"));

    let path_i = Path::new("src/iface.ts");
    let fid_i = compute_logical_file_id(path_i);
    let base_i = "export interface Codec {\n    decode(raw: string): string;\n    decode(raw: Uint8Array): Uint8Array;\n}\n";
    let ivp1 = "export interface Codec {\n    decode(raw: Uint8Array): Uint8Array;\n}\n";
    let ivp2 = "export interface Codec {\n    decode(raw: string): string;\n    decode(raw: Uint8Array): ArrayBuffer;\n}\n";
    let ip1 = diff_source_to_patch(
        &DiffContext {
            logical_file_id: fid_i,
            file_path: path_i,
            language: LanguageKind::TypeScript,
            viewport_id: "vp1",
            actor_id: "a1",
            lamport_ts: 1,
            parents: Vec::new(),
        },
        base_i,
        ivp1,
    )
    .unwrap();
    assert!(
        !ip1.contract_deltas.iter().any(|d| matches!(
            d,
            SymbolContractDelta::RemovedSymbol { symbol_fqn, .. } if symbol_fqn == "Codec::decode#sig:2"
        )),
        "deleting first method overload must not emit spurious RemovedSymbol for Codec::decode#sig:2: {:?}",
        ip1.contract_deltas
    );
    let ip2 = diff_source_to_patch(
        &DiffContext {
            logical_file_id: fid_i,
            file_path: path_i,
            language: LanguageKind::TypeScript,
            viewport_id: "vp2",
            actor_id: "a2",
            lamport_ts: 2,
            parents: Vec::new(),
        },
        base_i,
        ivp2,
    )
    .unwrap();
    let mut bi = BTreeMap::new();
    bi.insert(path_i.to_path_buf(), base_i.as_bytes().to_vec());
    let rep_i = merge_patches(&bi, &[ip1, ip2], None).unwrap();
    assert!(
        rep_i.is_clean(),
        "expected clean interface overload merge, got: {:?}",
        rep_i.conflicts
    );
    let merged_i = rep_i.text(path_i).unwrap();
    assert!(!merged_i.contains("raw: string"));
    assert!(merged_i.contains("decode(raw: Uint8Array): ArrayBuffer;"));
}

#[test]
fn go_concurrent_disjoint_const_var_type_group_inserts_merge_cleanly_while_colliding_member_conflicts() {
    // 1. Disjoint const(...) groups (above vs below func Anchor) merge cleanly and commutatively
    let path = Path::new("pkg/c.go");
    let base_src = "package pkg\n\nfunc Anchor() {}\n";
    let vp1_src = "package pkg\n\nconst (\n\tA = 1\n)\n\nfunc Anchor() {}\n";
    let vp2_src = "package pkg\n\nfunc Anchor() {}\n\nconst (\n\tB = 2\n)\n";

    let p1 = diff_source_to_patch(&make_ctx(path, LanguageKind::Go, "vp1", "a1", 1), base_src, vp1_src).unwrap();
    let p2 = diff_source_to_patch(&make_ctx(path, LanguageKind::Go, "vp2", "a2", 2), base_src, vp2_src).unwrap();

    let mut base = BTreeMap::new();
    base.insert(path.to_path_buf(), base_src.as_bytes().to_vec());
    let rep12 = merge_patches(&base, &[p1.clone(), p2.clone()], None).unwrap();
    let rep21 = merge_patches(&base, &[p2, p1.clone()], None).unwrap();
    assert!(rep12.is_clean(), "expected clean Go disjoint const_group merge, got: {:?}", rep12.conflicts);
    assert_eq!(rep12.text(path), rep21.text(path));
    let merged = rep12.text(path).unwrap();
    assert!(merged.contains("A = 1"), "missing A = 1:\n{merged}");
    assert!(merged.contains("B = 2"), "missing B = 2:\n{merged}");
    assert!(!parse_source(LanguageKind::Go, merged, None).unwrap().tree.root_node().has_error());

    // Colliding constant name inside concurrent const(...) groups must still conflict
    let vp3_src = "package pkg\n\nfunc Anchor() {}\n\nconst (\n\tA = 99\n)\n";
    let p3 = diff_source_to_patch(&make_ctx(path, LanguageKind::Go, "vp3", "a3", 2), base_src, vp3_src).unwrap();
    let rep_col = merge_patches(&base, &[p1, p3], None).unwrap();
    assert!(
        matches!(rep_col.outcome, MergeOutcome::StructuralConflict { .. }),
        "colliding Go const member in concurrent groups must conflict: {:?}",
        rep_col.outcome
    );

    // 2. Disjoint var(...) groups merge cleanly and commutatively, while colliding var conflicts
    let vpath = Path::new("pkg/v.go");
    let vbase_src = "package pkg\n\nfunc Anchor() {}\n";
    let v_vp1 = "package pkg\n\nvar (\n\tX = 1\n)\n\nfunc Anchor() {}\n";
    let v_vp2 = "package pkg\n\nfunc Anchor() {}\n\nvar (\n\tY = 2\n)\n";
    let v_vp3 = "package pkg\n\nfunc Anchor() {}\n\nvar (\n\tX = 99\n)\n";

    let vp_v1 = diff_source_to_patch(&make_ctx(vpath, LanguageKind::Go, "vp1", "a1", 1), vbase_src, v_vp1).unwrap();
    let vp_v2 = diff_source_to_patch(&make_ctx(vpath, LanguageKind::Go, "vp2", "a2", 2), vbase_src, v_vp2).unwrap();
    let vp_v3 = diff_source_to_patch(&make_ctx(vpath, LanguageKind::Go, "vp3", "a3", 2), vbase_src, v_vp3).unwrap();
    let mut vbase = BTreeMap::new();
    vbase.insert(vpath.to_path_buf(), vbase_src.as_bytes().to_vec());
    let vrep12 = merge_patches(&vbase, &[vp_v1.clone(), vp_v2.clone()], None).unwrap();
    let vrep21 = merge_patches(&vbase, &[vp_v2, vp_v1.clone()], None).unwrap();
    assert!(vrep12.is_clean(), "expected clean Go disjoint var_group merge, got: {:?}", vrep12.conflicts);
    assert_eq!(vrep12.text(vpath), vrep21.text(vpath));
    let vmerged = vrep12.text(vpath).unwrap();
    assert!(vmerged.contains("X = 1") && vmerged.contains("Y = 2"));
    assert!(!parse_source(LanguageKind::Go, vmerged, None).unwrap().tree.root_node().has_error());

    let vrep_col = merge_patches(&vbase, &[vp_v1, vp_v3], None).unwrap();
    assert!(
        matches!(vrep_col.outcome, MergeOutcome::StructuralConflict { .. }),
        "colliding Go var member in concurrent groups must conflict: {:?}",
        vrep_col.outcome
    );

    // 3. Disjoint type(...) groups merge cleanly and commutatively, while colliding type conflicts
    let tpath = Path::new("pkg/t.go");
    let tbase_src = "package pkg\n\nfunc Anchor() {}\n";
    let t_vp1 = "package pkg\n\ntype (\n\tFoo struct {\n\t\tA int\n\t}\n)\n\nfunc Anchor() {}\n";
    let t_vp2 = "package pkg\n\nfunc Anchor() {}\n\ntype (\n\tBar struct {\n\t\tB string\n\t}\n)\n";
    let t_vp3 = "package pkg\n\nfunc Anchor() {}\n\ntype (\n\tFoo struct {\n\t\tZ bool\n\t}\n)\n";

    let tp_1 = diff_source_to_patch(&make_ctx(tpath, LanguageKind::Go, "vp1", "a1", 1), tbase_src, t_vp1).unwrap();
    let tp_2 = diff_source_to_patch(&make_ctx(tpath, LanguageKind::Go, "vp2", "a2", 2), tbase_src, t_vp2).unwrap();
    let tp_3 = diff_source_to_patch(&make_ctx(tpath, LanguageKind::Go, "vp3", "a3", 2), tbase_src, t_vp3).unwrap();
    let mut tbase = BTreeMap::new();
    tbase.insert(tpath.to_path_buf(), tbase_src.as_bytes().to_vec());
    let trep12 = merge_patches(&tbase, &[tp_1.clone(), tp_2.clone()], None).unwrap();
    let trep21 = merge_patches(&tbase, &[tp_2, tp_1.clone()], None).unwrap();
    assert!(trep12.is_clean(), "expected clean Go disjoint type_group merge, got: {:?}", trep12.conflicts);
    assert_eq!(trep12.text(tpath), trep21.text(tpath));
    let tmerged = trep12.text(tpath).unwrap();
    assert!(tmerged.contains("Foo struct") && tmerged.contains("Bar struct"));
    assert!(!parse_source(LanguageKind::Go, tmerged, None).unwrap().tree.root_node().has_error());

    let trep_col = merge_patches(&tbase, &[tp_1, tp_3], None).unwrap();
    assert!(
        matches!(trep_col.outcome, MergeOutcome::StructuralConflict { .. }),
        "colliding Go type member in concurrent groups must conflict: {:?}",
        trep_col.outcome
    );
}

#[test]
fn ts_concurrent_distinct_overload_inserts_merge_cleanly() {
    // 1. Disjoint-slot overload delete + insert (both new overloads land on positional #sig:2)
    let path = Path::new("src/disjoint_ins.ts");
    let base_src = "export function foo(a: string): string;\nexport function foo(a: number): number;\nexport function foo(a: string | number | boolean | bigint): unknown {\n    return a;\n}\n";
    let vp1_src = "export function foo(a: number): number;\nexport function foo(a: boolean): boolean;\nexport function foo(a: string | number | boolean | bigint): unknown {\n    return a;\n}\n";
    let vp2_src = "export function foo(a: string): string;\nexport function foo(a: bigint): bigint;\nexport function foo(a: number): number;\nexport function foo(a: string | number | boolean | bigint): unknown {\n    return a;\n}\n";

    let p1 = diff_source_to_patch(&make_ctx(path, LanguageKind::TypeScript, "vp1", "a1", 1), base_src, vp1_src).unwrap();
    let p2 = diff_source_to_patch(&make_ctx(path, LanguageKind::TypeScript, "vp2", "a2", 2), base_src, vp2_src).unwrap();

    let mut base = BTreeMap::new();
    base.insert(path.to_path_buf(), base_src.as_bytes().to_vec());
    let rep12 = merge_patches(&base, &[p1.clone(), p2.clone()], None).unwrap();
    let rep21 = merge_patches(&base, &[p2, p1], None).unwrap();
    assert!(rep12.is_clean(), "expected clean TS disjoint overload merge, got: {:?}", rep12.conflicts);
    assert_eq!(rep12.text(path), rep21.text(path));
    let merged = rep12.text(path).unwrap();
    assert!(!merged.contains("a: string): string;"));
    assert!(merged.contains("export function foo(a: bigint): bigint;"));
    assert!(merged.contains("export function foo(a: boolean): boolean;"));
    assert!(merged.contains("export function foo(a: number): number;"));
    assert!(!parse_source(LanguageKind::TypeScript, merged, None).unwrap().tree.root_node().has_error());

    // 2. Concurrent distinct function overload additions in the same slot
    let opath = Path::new("src/overloads.ts");
    let obase_src = "export function foo(a: string): string;\nexport function foo(a: string | number | boolean): string | number | boolean {\n    return a;\n}\n";
    let o_vp1 = "export function foo(a: string): string;\nexport function foo(a: number): number;\nexport function foo(a: string | number | boolean): string | number | boolean {\n    return a;\n}\n";
    let o_vp2 = "export function foo(a: string): string;\nexport function foo(a: boolean): boolean;\nexport function foo(a: string | number | boolean): string | number | boolean {\n    return a;\n}\n";
    let op1 = diff_source_to_patch(&make_ctx(opath, LanguageKind::TypeScript, "vp1", "a1", 1), obase_src, o_vp1).unwrap();
    let op2 = diff_source_to_patch(&make_ctx(opath, LanguageKind::TypeScript, "vp2", "a2", 2), obase_src, o_vp2).unwrap();
    let mut obase = BTreeMap::new();
    obase.insert(opath.to_path_buf(), obase_src.as_bytes().to_vec());
    let orep12 = merge_patches(&obase, &[op1.clone(), op2.clone()], None).unwrap();
    let orep21 = merge_patches(&obase, &[op2, op1], None).unwrap();
    assert!(orep12.is_clean(), "expected clean TS same-slot distinct overload merge, got: {:?}", orep12.conflicts);
    assert_eq!(orep12.text(opath), orep21.text(opath));
    let omerged = orep12.text(opath).unwrap();
    assert!(omerged.contains("export function foo(a: number): number;"));
    assert!(omerged.contains("export function foo(a: boolean): boolean;"));
    assert!(!parse_source(LanguageKind::TypeScript, omerged, None).unwrap().tree.root_node().has_error());

    // 3. Concurrent distinct interface method_signature overload additions
    let ipath = Path::new("src/iface_overloads.ts");
    let ibase_src = "export interface Codec {\n    decode(raw: string): string;\n}\n";
    let i_vp1 = "export interface Codec {\n    decode(raw: string): string;\n    decode(raw: Uint8Array): Uint8Array;\n}\n";
    let i_vp2 = "export interface Codec {\n    decode(raw: string): string;\n    decode(raw: ArrayBuffer): string;\n}\n";
    let ip1 = diff_source_to_patch(&make_ctx(ipath, LanguageKind::TypeScript, "vp1", "a1", 1), ibase_src, i_vp1).unwrap();
    let ip2 = diff_source_to_patch(&make_ctx(ipath, LanguageKind::TypeScript, "vp2", "a2", 2), ibase_src, i_vp2).unwrap();
    let mut ibase = BTreeMap::new();
    ibase.insert(ipath.to_path_buf(), ibase_src.as_bytes().to_vec());
    let irep12 = merge_patches(&ibase, &[ip1.clone(), ip2.clone()], None).unwrap();
    let irep21 = merge_patches(&ibase, &[ip2, ip1], None).unwrap();
    assert!(irep12.is_clean(), "expected clean TS interface method_signature overload merge, got: {:?}", irep12.conflicts);
    assert_eq!(irep12.text(ipath), irep21.text(ipath));
    let imerged = irep12.text(ipath).unwrap();
    assert!(imerged.contains("decode(raw: Uint8Array): Uint8Array"));
    assert!(imerged.contains("decode(raw: ArrayBuffer): string"));
    assert!(!parse_source(LanguageKind::TypeScript, imerged, None).unwrap().tree.root_node().has_error());

    // 4. TSX abstract_method_signature concurrent distinct overload additions
    let tpath = Path::new("src/widget.tsx");
    let tbase_src = "export abstract class Widget {\n    abstract render(v: string): JSX.Element;\n}\n";
    let t_vp1 = "export abstract class Widget {\n    abstract render(v: string): JSX.Element;\n    abstract render(v: number): JSX.Element;\n}\n";
    let t_vp2 = "export abstract class Widget {\n    abstract render(v: string): JSX.Element;\n    abstract render(v: boolean): JSX.Element;\n}\n";
    let tp1 = diff_source_to_patch(&make_ctx(tpath, LanguageKind::Tsx, "vp1", "a1", 1), tbase_src, t_vp1).unwrap();
    let tp2 = diff_source_to_patch(&make_ctx(tpath, LanguageKind::Tsx, "vp2", "a2", 2), tbase_src, t_vp2).unwrap();
    let mut tbase = BTreeMap::new();
    tbase.insert(tpath.to_path_buf(), tbase_src.as_bytes().to_vec());
    let trep12 = merge_patches(&tbase, &[tp1.clone(), tp2.clone()], None).unwrap();
    let trep21 = merge_patches(&tbase, &[tp2, tp1], None).unwrap();
    assert!(trep12.is_clean(), "expected clean TSX abstract_method_signature overload merge, got: {:?}", trep12.conflicts);
    assert_eq!(trep12.text(tpath), trep21.text(tpath));
    let tmerged = trep12.text(tpath).unwrap();
    assert!(tmerged.contains("abstract render(v: number): JSX.Element"));
    assert!(tmerged.contains("abstract render(v: boolean): JSX.Element"));
    assert!(!parse_source(LanguageKind::Tsx, tmerged, None).unwrap().tree.root_node().has_error());

    // 5. Identical decl_header_hash with differing trivia (txt_a != txt_b) and non-overload function_declaration still conflict
    let dup_sig_1 = "export function foo(a: string): string;\n// doc A\nexport function foo(a: number): number;\nexport function foo(a: string | number): string | number { return a; }\n";
    let dup_sig_2 = "export function foo(a: string): string;\n// doc B\nexport function foo(a: number): number;\nexport function foo(a: string | number): string | number { return a; }\n";
    let dp1 = diff_source_to_patch(&make_ctx(opath, LanguageKind::TypeScript, "vp1", "a1", 1), obase_src, dup_sig_1).unwrap();
    let dp2 = diff_source_to_patch(&make_ctx(opath, LanguageKind::TypeScript, "vp2", "a2", 2), obase_src, dup_sig_2).unwrap();
    let drep = merge_patches(&obase, &[dp1, dp2], None).unwrap();
    assert!(
        matches!(drep.outcome, MergeOutcome::StructuralConflict { .. }),
        "identical overload header with differing trivia must still conflict: {:?}",
        drep.outcome
    );

    let fpath = Path::new("src/dup_fn.ts");
    let fbase_src = "export function keep(): number { return 0; }\n";
    let f_vp1 = "export function keep(): number { return 0; }\nexport function helper(a: string): string { return a; }\n";
    let f_vp2 = "export function keep(): number { return 0; }\nexport function helper(a: number): number { return a; }\n";
    let fp1 = diff_source_to_patch(&make_ctx(fpath, LanguageKind::TypeScript, "vp1", "a1", 1), fbase_src, f_vp1).unwrap();
    let fp2 = diff_source_to_patch(&make_ctx(fpath, LanguageKind::TypeScript, "vp2", "a2", 2), fbase_src, f_vp2).unwrap();
    let mut fbase = BTreeMap::new();
    fbase.insert(fpath.to_path_buf(), fbase_src.as_bytes().to_vec());
    let frep = merge_patches(&fbase, &[fp1, fp2], None).unwrap();
    assert!(
        matches!(frep.outcome, MergeOutcome::StructuralConflict { .. }),
        "non-overload function_declaration with same name must still conflict: {:?}",
        frep.outcome
    );
}

#[test]
fn single_line_container_prepend_and_ts_semicolon_comma_removal() {
    // 1. TS single-line interface method_signature prepend
    let p_ts_m = Path::new("src/codec.ts");
    let b_ts_m = "export interface Codec { decode(raw: string): string; }\n";
    let v_ts_m = "export interface Codec {\n    decode(raw: Uint8Array): Uint8Array;\n    decode(raw: string): string;\n}\n";
    let patch_ts_m = diff_source_to_patch(
        &make_ctx(p_ts_m, LanguageKind::TypeScript, "vp1", "a1", 1),
        b_ts_m,
        v_ts_m,
    )
    .unwrap();
    let mut base_ts_m = BTreeMap::new();
    base_ts_m.insert(p_ts_m.to_path_buf(), b_ts_m.as_bytes().to_vec());
    let rep_ts_m = merge_patches(&base_ts_m, &[patch_ts_m], None).unwrap();
    assert!(rep_ts_m.is_clean(), "TS single-line method prepend failed: {:?}", rep_ts_m.conflicts);
    let m_ts_m = rep_ts_m.text(p_ts_m).unwrap();
    assert!(m_ts_m.contains("decode(raw: Uint8Array): Uint8Array;"));
    assert!(m_ts_m.contains("decode(raw: string): string;"));
    assert!(!parse_source(LanguageKind::TypeScript, m_ts_m, None).unwrap().tree.root_node().has_error());

    // 2. TSX single-line abstract class abstract_method_signature prepend
    let p_tsx = Path::new("src/widget.tsx");
    let b_tsx = "export abstract class Widget { abstract render(v: string): JSX.Element; }\n";
    let v_tsx = "export abstract class Widget {\n    abstract render(v: number): JSX.Element;\n    abstract render(v: string): JSX.Element;\n}\n";
    let patch_tsx = diff_source_to_patch(
        &make_ctx(p_tsx, LanguageKind::Tsx, "vp1", "a1", 1),
        b_tsx,
        v_tsx,
    )
    .unwrap();
    let mut base_tsx = BTreeMap::new();
    base_tsx.insert(p_tsx.to_path_buf(), b_tsx.as_bytes().to_vec());
    let rep_tsx = merge_patches(&base_tsx, &[patch_tsx], None).unwrap();
    assert!(rep_tsx.is_clean(), "TSX single-line abstract prepend failed: {:?}", rep_tsx.conflicts);
    assert!(!parse_source(LanguageKind::Tsx, rep_tsx.text(p_tsx).unwrap(), None).unwrap().tree.root_node().has_error());

    // 3. TS single-line interface property_signature, class public_field_definition, and no-semicolon interface
    for (rel, base_s, new_s) in [
        (
            "src/prop.ts",
            "export interface Cfg { host: string; }\n",
            "export interface Cfg {\n    port: number;\n    host: string;\n}\n",
        ),
        (
            "src/field.ts",
            "export class Cfg { host: string = 'a'; }\n",
            "export class Cfg {\n    port: number = 80;\n    host: string = 'a';\n}\n",
        ),
        (
            "src/nosemi.ts",
            "export interface C { a(): void }\n",
            "export interface C {\n    x(): void\n    a(): void\n}\n",
        ),
    ] {
        let p = Path::new(rel);
        let patch = diff_source_to_patch(
            &make_ctx(p, LanguageKind::TypeScript, "vp1", "a1", 1),
            base_s,
            new_s,
        )
        .unwrap();
        let mut bm = BTreeMap::new();
        bm.insert(p.to_path_buf(), base_s.as_bytes().to_vec());
        let rep = merge_patches(&bm, &[patch], None).unwrap();
        assert!(rep.is_clean(), "{rel} single-line prepend failed: {:?}", rep.conflicts);
        let merged = rep.text(p).unwrap();
        assert!(!parse_source(LanguageKind::TypeScript, merged, None).unwrap().tree.root_node().has_error());
    }

    // 4. Go single-line struct & interface prepend
    for (rel, base_s, new_s) in [
        (
            "pkg/s.go",
            "package pkg\n\ntype S struct { A int }\n",
            "package pkg\n\ntype S struct {\n\tX int\n\tA int\n}\n",
        ),
        (
            "pkg/i.go",
            "package pkg\n\ntype I interface { A() }\n",
            "package pkg\n\ntype I interface {\n\tX()\n\tA()\n}\n",
        ),
    ] {
        let p = Path::new(rel);
        let patch = diff_source_to_patch(
            &make_ctx(p, LanguageKind::Go, "vp1", "a1", 1),
            base_s,
            new_s,
        )
        .unwrap();
        let mut bm = BTreeMap::new();
        bm.insert(p.to_path_buf(), base_s.as_bytes().to_vec());
        let rep = merge_patches(&bm, &[patch], None).unwrap();
        assert!(rep.is_clean(), "{rel} single-line Go prepend failed: {:?}", rep.conflicts);
        let merged = rep.text(p).unwrap();
        assert!(!parse_source(LanguageKind::Go, merged, None).unwrap().tree.root_node().has_error());
    }

    // 5. TS RemoveNode on semicolon-separated interface & class and comma-separated interface leaves no orphaned ';' or ','
    let p_rem_s = Path::new("src/rem_semi.ts");
    let b_rem_s = "export interface Codec {\n    a(): void;\n    b(): void;\n}\n";
    let v_rem_s = "export interface Codec {\n    b(): void;\n}\n";
    let patch_rem_s = diff_source_to_patch(
        &make_ctx(p_rem_s, LanguageKind::TypeScript, "vp1", "a1", 1),
        b_rem_s,
        v_rem_s,
    )
    .unwrap();
    let mut bm_rem_s = BTreeMap::new();
    bm_rem_s.insert(p_rem_s.to_path_buf(), b_rem_s.as_bytes().to_vec());
    let rep_rem_s = merge_patches(&bm_rem_s, &[patch_rem_s], None).unwrap();
    assert!(rep_rem_s.is_clean());
    let m_rem_s = rep_rem_s.text(p_rem_s).unwrap();
    assert!(!m_rem_s.lines().any(|l| l.trim() == ";"), "orphaned ';' in:\n{m_rem_s}");

    let p_rem_c = Path::new("src/rem_comma.ts");
    let b_rem_c = "export interface Pair {\n    a: number,\n    b: number\n}\n";
    let v_rem_c = "export interface Pair {\n    b: number\n}\n";
    let patch_rem_c = diff_source_to_patch(
        &make_ctx(p_rem_c, LanguageKind::TypeScript, "vp1", "a1", 1),
        b_rem_c,
        v_rem_c,
    )
    .unwrap();
    let mut bm_rem_c = BTreeMap::new();
    bm_rem_c.insert(p_rem_c.to_path_buf(), b_rem_c.as_bytes().to_vec());
    let rep_rem_c = merge_patches(&bm_rem_c, &[patch_rem_c], None).unwrap();
    assert!(rep_rem_c.is_clean());
    let m_rem_c = rep_rem_c.text(p_rem_c).unwrap();
    assert!(!m_rem_c.lines().any(|l| l.trim() == ","), "orphaned ',' in:\n{m_rem_c}");

    let p_rem_cls = Path::new("src/rem_cls.ts");
    let b_rem_cls = "export class Store {\n    x: number = 1;\n    y: string = 'ok';\n}\n";
    let v_rem_cls = "export class Store {\n    y: string = 'ok';\n}\n";
    let patch_rem_cls = diff_source_to_patch(
        &make_ctx(p_rem_cls, LanguageKind::TypeScript, "vp1", "a1", 1),
        b_rem_cls,
        v_rem_cls,
    )
    .unwrap();
    let mut bm_rem_cls = BTreeMap::new();
    bm_rem_cls.insert(p_rem_cls.to_path_buf(), b_rem_cls.as_bytes().to_vec());
    let rep_rem_cls = merge_patches(&bm_rem_cls, &[patch_rem_cls], None).unwrap();
    assert!(rep_rem_cls.is_clean());
    let m_rem_cls = rep_rem_cls.text(p_rem_cls).unwrap();
    assert!(!m_rem_cls.lines().any(|l| l.trim() == ";"), "orphaned ';' in class:\n{m_rem_cls}");
}

#[test]
fn intra_patch_positional_after_anchor_chaining_preserves_order() {
    // 1. Go: insert const_group + func Bar() BEFORE existing const_group
    let gp = Path::new("pkg/order.go");
    let g_base = "package pkg\n\nconst (\n\tY = 2\n)\n";
    let g_before = "package pkg\n\nconst (\n\tX = 1\n)\n\nfunc Bar() {}\n\nconst (\n\tY = 2\n)\n";
    let gp_before = diff_source_to_patch(
        &make_ctx(gp, LanguageKind::Go, "vp1", "a1", 1),
        g_base,
        g_before,
    )
    .unwrap();
    let mut g_bm = BTreeMap::new();
    g_bm.insert(gp.to_path_buf(), g_base.as_bytes().to_vec());
    let g_rep = merge_patches(&g_bm, &[gp_before], None).unwrap();
    assert!(g_rep.is_clean(), "Go merge failed: {:?}", g_rep.conflicts);
    let g_merged = g_rep.text(gp).unwrap();
    let px = g_merged.find("X = 1").unwrap();
    let pbar = g_merged.find("func Bar()").unwrap();
    let py = g_merged.find("Y = 2").unwrap();
    assert!(px < pbar && pbar < py, "Go order broken:\n{g_merged}");

    // Ensure inserting const_group before Y and func Bar() AFTER Y keeps Bar after Y!
    let g_split = "package pkg\n\nconst (\n\tX = 1\n)\n\nconst (\n\tY = 2\n)\n\nfunc Bar() {}\n";
    let gp_split = diff_source_to_patch(
        &make_ctx(gp, LanguageKind::Go, "vp1", "a1", 1),
        g_base,
        g_split,
    )
    .unwrap();
    let g_rep_split = merge_patches(&g_bm, &[gp_split], None).unwrap();
    assert!(g_rep_split.is_clean());
    let gs_merged = g_rep_split.text(gp).unwrap();
    assert!(
        gs_merged.find("X = 1").unwrap() < gs_merged.find("Y = 2").unwrap()
            && gs_merged.find("Y = 2").unwrap() < gs_merged.find("func Bar()").unwrap(),
        "Go split order broken:\n{gs_merged}"
    );

    // 2. Rust: insert impl Foo + pub fn helper() BEFORE existing impl Foo
    let rp = Path::new("src/order.rs");
    let r_base = "pub struct Foo;\nimpl Foo { pub fn y(&self) -> i32 { 2 } }\n";
    let r_new = "pub struct Foo;\nimpl Foo { pub fn x(&self) -> i32 { 1 } }\n\npub fn helper() -> i32 { 3 }\n\nimpl Foo { pub fn y(&self) -> i32 { 2 } }\n";
    let rp_new = diff_source_to_patch(
        &make_ctx(rp, LanguageKind::Rust, "vp1", "a1", 1),
        r_base,
        r_new,
    )
    .unwrap();
    let mut r_bm = BTreeMap::new();
    r_bm.insert(rp.to_path_buf(), r_base.as_bytes().to_vec());
    let r_rep = merge_patches(&r_bm, std::slice::from_ref(&rp_new), None).unwrap();
    assert!(r_rep.is_clean(), "Rust merge failed: {:?}", r_rep.conflicts);
    let r_merged = r_rep.text(rp).unwrap();
    let rx = r_merged.find("fn x(").unwrap();
    let rhelp = r_merged.find("fn helper(").unwrap();
    let ry = r_merged.find("fn y(").unwrap();
    assert!(rx < rhelp && rhelp < ry, "Rust order broken:\n{r_merged}");

    // Cross-slot collision between prepended impl (#added) and appended impl (#2) with duplicate member `x`
    let r_peer_col = "pub struct Foo;\nimpl Foo { pub fn y(&self) -> i32 { 2 } }\nimpl Foo { pub fn x(&self) -> i32 { 99 } }\n";
    let rp_col = diff_source_to_patch(
        &make_ctx(rp, LanguageKind::Rust, "vp2", "a2", 2),
        r_base,
        r_peer_col,
    )
    .unwrap();
    let r_col_rep = merge_patches(&r_bm, &[rp_new, rp_col], None).unwrap();
    assert!(
        matches!(r_col_rep.outcome, MergeOutcome::StructuralConflict { .. }),
        "prepended #added vs appended #2 colliding Rust impl must conflict: {:?}",
        r_col_rep.outcome
    );

    // 3. TypeScript: insert declare function foo(number) + declare const A BEFORE existing declare function foo(string)
    let tp = Path::new("src/order.ts");
    let t_base = "declare function foo(a: string): string;\n";
    let t_new = "declare function foo(a: number): number;\ndeclare const A: number;\ndeclare function foo(a: string): string;\n";
    let tp_new = diff_source_to_patch(
        &make_ctx(tp, LanguageKind::TypeScript, "vp1", "a1", 1),
        t_base,
        t_new,
    )
    .unwrap();
    let mut t_bm = BTreeMap::new();
    t_bm.insert(tp.to_path_buf(), t_base.as_bytes().to_vec());
    let t_rep = merge_patches(&t_bm, &[tp_new], None).unwrap();
    assert!(t_rep.is_clean(), "TS merge failed: {:?}", t_rep.conflicts);
    let t_merged = t_rep.text(tp).unwrap();
    let tnum = t_merged.find("a: number").unwrap();
    let ta = t_merged.find("declare const A").unwrap();
    let tstr = t_merged.find("a: string").unwrap();
    assert!(tnum < ta && ta < tstr, "TS order broken:\n{t_merged}");
}

#[test]
fn go_decl_group_literal_name_and_non_paren_vs_paren_collision() {
    // Obs B: literal symbol names `const_group`, `var_group`, `type_group` must collide
    let p_obs_b = Path::new("pkg/obs_b.go");
    let b_obs_b = "package pkg\n\nfunc Anchor() {}\n";
    let v1_fn = "package pkg\n\nfunc Anchor() {}\n\nfunc const_group() int { return 1 }\n";
    let v2_fn = "package pkg\n\nfunc Anchor() {}\n\nfunc const_group() int { return 2 }\n";
    let p1 = diff_source_to_patch(&make_ctx(p_obs_b, LanguageKind::Go, "vp1", "a1", 1), b_obs_b, v1_fn).unwrap();
    let p2 = diff_source_to_patch(&make_ctx(p_obs_b, LanguageKind::Go, "vp2", "a2", 2), b_obs_b, v2_fn).unwrap();
    let mut bm_b = BTreeMap::new();
    bm_b.insert(p_obs_b.to_path_buf(), b_obs_b.as_bytes().to_vec());
    let rep_fn = merge_patches(&bm_b, &[p1, p2], None).unwrap();
    assert!(
        matches!(rep_fn.outcome, MergeOutcome::StructuralConflict { .. }),
        "concurrent func const_group() must conflict: {:?}",
        rep_fn.outcome
    );

    let v1_ty = "package pkg\n\nfunc Anchor() {}\n\ntype type_group int\n";
    let v2_ty = "package pkg\n\nfunc Anchor() {}\n\ntype type_group string\n";
    let pt1 = diff_source_to_patch(&make_ctx(p_obs_b, LanguageKind::Go, "vp1", "a1", 1), b_obs_b, v1_ty).unwrap();
    let pt2 = diff_source_to_patch(&make_ctx(p_obs_b, LanguageKind::Go, "vp2", "a2", 2), b_obs_b, v2_ty).unwrap();
    let rep_ty = merge_patches(&bm_b, &[pt1, pt2], None).unwrap();
    assert!(
        matches!(rep_ty.outcome, MergeOutcome::StructuralConflict { .. }),
        "concurrent type type_group must conflict: {:?}",
        rep_ty.outcome
    );

    // Obs C: non-parenthesized `const A = 1` vs parenthesized `const (\n\tA = 2\n)` -> StructuralConflict
    // while disjoint `const A = 1` vs `const (\n\tB = 2\n)` -> Clean
    let p_obs_c = Path::new("pkg/obs_c.go");
    let b_obs_c = "package pkg\n\nfunc Anchor() {}\n";
    let single_const = "package pkg\n\nfunc Anchor() {}\n\nconst A = 1\n";
    let group_const_col = "package pkg\n\nfunc Anchor() {}\n\nconst (\n\tA = 2\n)\n";
    let group_const_dis = "package pkg\n\nfunc Anchor() {}\n\nconst (\n\tB = 2\n)\n";

    let ps = diff_source_to_patch(&make_ctx(p_obs_c, LanguageKind::Go, "vp1", "a1", 1), b_obs_c, single_const).unwrap();
    let pg_col = diff_source_to_patch(&make_ctx(p_obs_c, LanguageKind::Go, "vp2", "a2", 2), b_obs_c, group_const_col).unwrap();
    let pg_dis = diff_source_to_patch(&make_ctx(p_obs_c, LanguageKind::Go, "vp2", "a2", 2), b_obs_c, group_const_dis).unwrap();
    let mut bm_c = BTreeMap::new();
    bm_c.insert(p_obs_c.to_path_buf(), b_obs_c.as_bytes().to_vec());

    let rep_col = merge_patches(&bm_c, &[ps.clone(), pg_col], None).unwrap();
    assert!(
        matches!(rep_col.outcome, MergeOutcome::StructuralConflict { .. }),
        "const A = 1 vs const ( A = 2 ) must conflict: {:?}",
        rep_col.outcome
    );

    let rep_dis12 = merge_patches(&bm_c, &[ps.clone(), pg_dis.clone()], None).unwrap();
    let rep_dis21 = merge_patches(&bm_c, &[pg_dis, ps], None).unwrap();
    assert!(rep_dis12.is_clean(), "disjoint const A vs const ( B ) must merge cleanly: {:?}", rep_dis12.conflicts);
    assert_eq!(rep_dis12.text(p_obs_c), rep_dis21.text(p_obs_c));
    let merged_c = rep_dis12.text(p_obs_c).unwrap();
    assert!(merged_c.contains("const A = 1"));
    assert!(merged_c.contains("B = 2"));

    // Also verify var & type non-paren vs paren collisions
    let single_var = "package pkg\n\nfunc Anchor() {}\n\nvar V = 1\n";
    let group_var = "package pkg\n\nfunc Anchor() {}\n\nvar (\n\tV = 2\n)\n";
    let pvs = diff_source_to_patch(&make_ctx(p_obs_c, LanguageKind::Go, "vp1", "a1", 1), b_obs_c, single_var).unwrap();
    let pvg = diff_source_to_patch(&make_ctx(p_obs_c, LanguageKind::Go, "vp2", "a2", 2), b_obs_c, group_var).unwrap();
    assert!(matches!(
        merge_patches(&bm_c, &[pvs, pvg], None).unwrap().outcome,
        MergeOutcome::StructuralConflict { .. }
    ));

    let single_type = "package pkg\n\nfunc Anchor() {}\n\ntype T int\n";
    let group_type = "package pkg\n\nfunc Anchor() {}\n\ntype (\n\tT string\n)\n";
    let pts = diff_source_to_patch(&make_ctx(p_obs_c, LanguageKind::Go, "vp1", "a1", 1), b_obs_c, single_type).unwrap();
    let ptg = diff_source_to_patch(&make_ctx(p_obs_c, LanguageKind::Go, "vp2", "a2", 2), b_obs_c, group_type).unwrap();
    assert!(matches!(
        merge_patches(&bm_c, &[pts, ptg], None).unwrap().outcome,
        MergeOutcome::StructuralConflict { .. }
    ));
}

#[test]
fn python_single_line_class_prepend_ellipsis_docstring_and_semicolon_remove() {
    // 1. `class One: x = 1\n` -> single and concurrent method prepends
    let p_one = Path::new("one.py");
    let b_one = "class One: x = 1\n";
    let v_one_1 = "class One:\n    def m(self):\n        return 1\n    x = 1\n";
    let v_one_2 = "class One:\n    def n(self):\n        return 2\n    x = 1\n";
    let patch_one_1 = diff_source_to_patch(
        &make_ctx(p_one, LanguageKind::Python, "vp1", "a1", 1),
        b_one,
        v_one_1,
    )
    .unwrap();
    let patch_one_2 = diff_source_to_patch(
        &make_ctx(p_one, LanguageKind::Python, "vp2", "a2", 2),
        b_one,
        v_one_2,
    )
    .unwrap();
    let mut bm_one = BTreeMap::new();
    bm_one.insert(p_one.to_path_buf(), b_one.as_bytes().to_vec());

    let rep_one_single = merge_patches(&bm_one, std::slice::from_ref(&patch_one_1), None).unwrap();
    assert!(rep_one_single.is_clean(), "class One prepend failed: {:?}", rep_one_single.conflicts);
    let m_one_single = rep_one_single.text(p_one).unwrap();
    assert_eq!(m_one_single, "class One:\n    def m(self):\n        return 1\n    x = 1\n");
    assert!(!parse_source(LanguageKind::Python, m_one_single, None).unwrap().tree.root_node().has_error());

    let rep_one_both = merge_patches(&bm_one, &[patch_one_1, patch_one_2], None).unwrap();
    assert!(rep_one_both.is_clean(), "concurrent class One prepend failed: {:?}", rep_one_both.conflicts);
    let m_one_both = rep_one_both.text(p_one).unwrap();
    assert!(m_one_both.lines().any(|l| l == "    def m(self):"));
    assert!(m_one_both.lines().any(|l| l == "    def n(self):"));
    assert!(m_one_both.lines().any(|l| l == "    x = 1"));
    assert!(!parse_source(LanguageKind::Python, m_one_both, None).unwrap().tree.root_node().has_error());

    // 2. `class Empty: ...\n` -> single and concurrent method inserts replace ellipsis stub
    let p_empty = Path::new("empty_ellipsis.py");
    let b_empty = "class Empty: ...\n";
    let v_empty_1 = "class Empty:\n    def m(self):\n        pass\n";
    let v_empty_2 = "class Empty:\n    def n(self):\n        return 42\n";
    let pe1 = diff_source_to_patch(
        &make_ctx(p_empty, LanguageKind::Python, "vp1", "a1", 1),
        b_empty,
        v_empty_1,
    )
    .unwrap();
    let pe2 = diff_source_to_patch(
        &make_ctx(p_empty, LanguageKind::Python, "vp2", "a2", 2),
        b_empty,
        v_empty_2,
    )
    .unwrap();
    let mut bm_empty = BTreeMap::new();
    bm_empty.insert(p_empty.to_path_buf(), b_empty.as_bytes().to_vec());

    let rep_empty_1 = merge_patches(&bm_empty, std::slice::from_ref(&pe1), None).unwrap();
    assert!(rep_empty_1.is_clean(), "class Empty: ... insert failed: {:?}", rep_empty_1.conflicts);
    let m_empty_1 = rep_empty_1.text(p_empty).unwrap();
    assert!(!m_empty_1.contains("..."), "ellipsis stub should be replaced:\n{m_empty_1}");
    assert_eq!(m_empty_1, "class Empty:\n    def m(self):\n        pass\n");
    assert!(!parse_source(LanguageKind::Python, m_empty_1, None).unwrap().tree.root_node().has_error());

    let rep_empty_both = merge_patches(&bm_empty, &[pe1, pe2], None).unwrap();
    assert!(rep_empty_both.is_clean(), "concurrent class Empty: ... failed: {:?}", rep_empty_both.conflicts);
    let m_empty_both = rep_empty_both.text(p_empty).unwrap();
    assert!(m_empty_both.contains("def m(self):"));
    assert!(m_empty_both.contains("def n(self):"));
    assert!(!parse_source(LanguageKind::Python, m_empty_both, None).unwrap().tree.root_node().has_error());

    // 3. `class Doc: """doc"""; x = 1\n` -> docstring moved off colon line, semicolon stripped, x = 1 on indented line
    let p_doc = Path::new("doc.py");
    let b_doc = "class Doc: \"\"\"doc\"\"\"; x = 1\n";
    let v_doc = "class Doc:\n    \"\"\"doc\"\"\"\n    def m(self): pass\n    x = 1\n";
    let p_doc_patch = diff_source_to_patch(
        &make_ctx(p_doc, LanguageKind::Python, "vp1", "a1", 1),
        b_doc,
        v_doc,
    )
    .unwrap();
    let mut bm_doc = BTreeMap::new();
    bm_doc.insert(p_doc.to_path_buf(), b_doc.as_bytes().to_vec());
    let rep_doc = merge_patches(&bm_doc, &[p_doc_patch], None).unwrap();
    assert!(rep_doc.is_clean(), "class Doc prepend failed: {:?}", rep_doc.conflicts);
    let m_doc = rep_doc.text(p_doc).unwrap();
    assert_eq!(
        m_doc,
        "class Doc:\n    \"\"\"doc\"\"\"\n    def m(self): pass\n    x = 1\n"
    );
    assert!(!parse_source(LanguageKind::Python, m_doc, None).unwrap().tree.root_node().has_error());

    // 4. `class DocOnly: """doc"""\n` -> docstring moved off colon line cleanly
    let p_doc_only = Path::new("doc_only.py");
    let b_doc_only = "class DocOnly: \"\"\"doc\"\"\"\n";
    let v_doc_only = "class DocOnly:\n    \"\"\"doc\"\"\"\n    def m(self): pass\n";
    let pdo_patch = diff_source_to_patch(
        &make_ctx(p_doc_only, LanguageKind::Python, "vp1", "a1", 1),
        b_doc_only,
        v_doc_only,
    )
    .unwrap();
    let mut bm_do = BTreeMap::new();
    bm_do.insert(p_doc_only.to_path_buf(), b_doc_only.as_bytes().to_vec());
    let rep_do = merge_patches(&bm_do, &[pdo_patch], None).unwrap();
    assert!(rep_do.is_clean(), "class DocOnly insert failed: {:?}", rep_do.conflicts);
    let m_do = rep_do.text(p_doc_only).unwrap();
    assert_eq!(m_do, "class DocOnly:\n    \"\"\"doc\"\"\"\n    def m(self): pass\n");
    assert!(!parse_source(LanguageKind::Python, m_do, None).unwrap().tree.root_node().has_error());

    // 5. `class Two: x = 1; y = 2\n` -> prepend AND remove `x = 1`
    let p_two = Path::new("two.py");
    let b_two = "class Two: x = 1; y = 2\n";
    let v_two_pre = "class Two:\n    def m(self): pass\n    x = 1; y = 2\n";
    let v_two_rem = "class Two: y = 2\n";
    let pt_pre = diff_source_to_patch(
        &make_ctx(p_two, LanguageKind::Python, "vp1", "a1", 1),
        b_two,
        v_two_pre,
    )
    .unwrap();
    let pt_rem = diff_source_to_patch(
        &make_ctx(p_two, LanguageKind::Python, "vp2", "a2", 2),
        b_two,
        v_two_rem,
    )
    .unwrap();
    let mut bm_two = BTreeMap::new();
    bm_two.insert(p_two.to_path_buf(), b_two.as_bytes().to_vec());

    let rep_two_pre = merge_patches(&bm_two, std::slice::from_ref(&pt_pre), None).unwrap();
    assert!(rep_two_pre.is_clean(), "class Two prepend failed: {:?}", rep_two_pre.conflicts);
    assert_eq!(rep_two_pre.text(p_two).unwrap(), v_two_pre);

    let rep_two_rem = merge_patches(&bm_two, std::slice::from_ref(&pt_rem), None).unwrap();
    assert!(rep_two_rem.is_clean(), "class Two remove x failed: {:?}", rep_two_rem.conflicts);
    assert_eq!(rep_two_rem.text(p_two).unwrap(), "class Two: y = 2\n");

    let rep_two_both = merge_patches(&bm_two, &[pt_pre, pt_rem], None).unwrap();
    assert!(rep_two_both.is_clean(), "class Two prepend + remove x failed: {:?}", rep_two_both.conflicts);
    let m_two_both = rep_two_both.text(p_two).unwrap();
    assert_eq!(m_two_both, "class Two:\n    def m(self): pass\n    y = 2\n");
    assert!(!parse_source(LanguageKind::Python, m_two_both, None).unwrap().tree.root_node().has_error());

    // 6. Indented semicolon-separated class members: removing first preserves indent of second
    let p_ind = Path::new("two_ind.py");
    let b_ind = "class TwoInd:\n    x = 1; y = 2\n";
    let v_ind = "class TwoInd:\n    y = 2\n";
    let p_ind_patch = diff_source_to_patch(
        &make_ctx(p_ind, LanguageKind::Python, "vp1", "a1", 1),
        b_ind,
        v_ind,
    )
    .unwrap();
    let mut bm_ind = BTreeMap::new();
    bm_ind.insert(p_ind.to_path_buf(), b_ind.as_bytes().to_vec());
    let rep_ind = merge_patches(&bm_ind, &[p_ind_patch], None).unwrap();
    assert!(rep_ind.is_clean(), "indented semicolon remove failed: {:?}", rep_ind.conflicts);
    assert_eq!(rep_ind.text(p_ind).unwrap(), "class TwoInd:\n    y = 2\n");
}

#[test]
fn python_overload_disambiguation_merges_cleanly_and_preserves_conflicts() {
    // 1. Top-level @overload deletion + concurrent implementation body edit
    let ov_path = Path::new("ov.py");
    let ov_base = "from typing import overload\n\n@overload\ndef f(x: int) -> int: ...\n@overload\ndef f(x: str) -> str: ...\ndef f(x):\n    return x\n";
    let ov_del = "from typing import overload\n\n@overload\ndef f(x: str) -> str: ...\ndef f(x):\n    return x\n";
    let ov_edit = "from typing import overload\n\n@overload\ndef f(x: int) -> int: ...\n@overload\ndef f(x: str) -> str: ...\ndef f(x):\n    return x + x\n";

    let p_del = diff_source_to_patch(&make_ctx(ov_path, LanguageKind::Python, "vp1", "a1", 1), ov_base, ov_del).unwrap();
    let p_edit = diff_source_to_patch(&make_ctx(ov_path, LanguageKind::Python, "vp2", "a2", 2), ov_base, ov_edit).unwrap();

    // Verify Pass 0b remaps surviving overload f#sig:2 -> f#sig:1 without spurious RemovedSymbol for f#sig:2 or f
    assert!(
        p_del.contract_deltas.iter().any(|d| matches!(
            d,
            loom_core::model::SymbolContractDelta::RemovedSymbol { symbol_fqn, .. } if symbol_fqn == "f#sig:1"
        )),
        "expected RemovedSymbol for f#sig:1: {:?}",
        p_del.contract_deltas
    );
    assert!(
        !p_del.contract_deltas.iter().any(|d| matches!(
            d,
            loom_core::model::SymbolContractDelta::RemovedSymbol { symbol_fqn, .. } if symbol_fqn == "f#sig:2" || symbol_fqn == "f"
        )),
        "surviving overload f#sig:2 and impl f must not report RemovedSymbol: {:?}",
        p_del.contract_deltas
    );

    let mut ov_files = BTreeMap::new();
    ov_files.insert(ov_path.to_path_buf(), ov_base.as_bytes().to_vec());
    let rep12 = merge_patches(&ov_files, &[p_del.clone(), p_edit.clone()], None).unwrap();
    let rep21 = merge_patches(&ov_files, &[p_edit, p_del], None).unwrap();
    assert!(rep12.is_clean(), "Python @overload delete + impl edit must merge cleanly: {:?}", rep12.conflicts);
    assert_eq!(rep12.text(ov_path), rep21.text(ov_path));
    let merged_ov = rep12.text(ov_path).unwrap();
    assert!(!merged_ov.contains("x: int"));
    assert!(merged_ov.contains("def f(x: str) -> str: ..."));
    assert!(merged_ov.contains("return x + x"));
    assert!(!parse_source(LanguageKind::Python, merged_ov, None).unwrap().tree.root_node().has_error());

    // 2. Concurrent distinct @overload inserts (@overload and @typing.overload)
    let ov_add_base = "import typing\nfrom typing import overload\n\n@overload\ndef f(x: int) -> int: ...\ndef f(x):\n    return x\n";
    let ov_add_1 = "import typing\nfrom typing import overload\n\n@overload\ndef f(x: int) -> int: ...\n@overload\ndef f(x: str) -> str: ...\ndef f(x):\n    return x\n";
    let ov_add_2 = "import typing\nfrom typing import overload\n\n@overload\ndef f(x: int) -> int: ...\n@typing.overload\ndef f(x: float) -> float: ...\ndef f(x):\n    return x\n";
    let pa1 = diff_source_to_patch(&make_ctx(ov_path, LanguageKind::Python, "vp1", "a1", 1), ov_add_base, ov_add_1).unwrap();
    let pa2 = diff_source_to_patch(&make_ctx(ov_path, LanguageKind::Python, "vp2", "a2", 2), ov_add_base, ov_add_2).unwrap();
    let mut ov_add_files = BTreeMap::new();
    ov_add_files.insert(ov_path.to_path_buf(), ov_add_base.as_bytes().to_vec());
    let rep_add12 = merge_patches(&ov_add_files, &[pa1.clone(), pa2.clone()], None).unwrap();
    let rep_add21 = merge_patches(&ov_add_files, &[pa2, pa1], None).unwrap();
    assert!(rep_add12.is_clean(), "concurrent distinct Python @overload inserts failed: {:?}", rep_add12.conflicts);
    assert_eq!(rep_add12.text(ov_path), rep_add21.text(ov_path));
    let m_add = rep_add12.text(ov_path).unwrap();
    assert!(m_add.contains("x: int"));
    assert!(m_add.contains("x: str"));
    assert!(m_add.contains("x: float"));
    assert!(!parse_source(LanguageKind::Python, m_add, None).unwrap().tree.root_node().has_error());

    // 3. Class method @overload deletion + implementation edit
    let cls_path = Path::new("cls_ov.py");
    let cls_base = "from typing import overload\n\nclass Parser:\n    @overload\n    def parse(self, raw: bytes) -> dict: ...\n    @overload\n    def parse(self, raw: str) -> dict: ...\n    def parse(self, raw):\n        return {}\n";
    let cls_del = "from typing import overload\n\nclass Parser:\n    @overload\n    def parse(self, raw: str) -> dict: ...\n    def parse(self, raw):\n        return {}\n";
    let cls_edit = "from typing import overload\n\nclass Parser:\n    @overload\n    def parse(self, raw: bytes) -> dict: ...\n    @overload\n    def parse(self, raw: str) -> dict: ...\n    def parse(self, raw):\n        return {\"raw\": raw}\n";
    let pc1 = diff_source_to_patch(&make_ctx(cls_path, LanguageKind::Python, "vp1", "a1", 1), cls_base, cls_del).unwrap();
    let pc2 = diff_source_to_patch(&make_ctx(cls_path, LanguageKind::Python, "vp2", "a2", 2), cls_base, cls_edit).unwrap();
    let mut cls_files = BTreeMap::new();
    cls_files.insert(cls_path.to_path_buf(), cls_base.as_bytes().to_vec());
    let cls_rep = merge_patches(&cls_files, &[pc1, pc2], None).unwrap();
    assert!(cls_rep.is_clean(), "class method @overload delete + edit failed: {:?}", cls_rep.conflicts);
    let m_cls = cls_rep.text(cls_path).unwrap();
    assert!(!m_cls.contains("raw: bytes"));
    assert!(m_cls.contains("raw: str"));
    assert!(m_cls.contains("{\"raw\": raw}"));

    // 4. Negative conflict guards: editing vs deleting the SAME @overload, or the SAME implementation, must conflict
    let ov_mut_sig = "from typing import overload\n\n@overload\ndef f(x: int) -> bool: ...\n@overload\ndef f(x: str) -> str: ...\ndef f(x):\n    return x\n";
    let p_mut = diff_source_to_patch(&make_ctx(ov_path, LanguageKind::Python, "vp2", "a2", 2), ov_base, ov_mut_sig).unwrap();
    let p_del_same = diff_source_to_patch(&make_ctx(ov_path, LanguageKind::Python, "vp1", "a1", 1), ov_base, ov_del).unwrap();
    let rep_same_ov = merge_patches(&ov_files, &[p_del_same, p_mut], None).unwrap();
    assert!(
        matches!(rep_same_ov.outcome, MergeOutcome::StructuralConflict { .. }),
        "deleting vs mutating same @overload must conflict: {:?}",
        rep_same_ov.outcome
    );

    let ov_del_impl = "from typing import overload\n\n@overload\ndef f(x: int) -> int: ...\n@overload\ndef f(x: str) -> str: ...\n";
    let p_del_impl = diff_source_to_patch(&make_ctx(ov_path, LanguageKind::Python, "vp1", "a1", 1), ov_base, ov_del_impl).unwrap();
    let p_edit_impl = diff_source_to_patch(&make_ctx(ov_path, LanguageKind::Python, "vp2", "a2", 2), ov_base, ov_edit).unwrap();
    let rep_same_impl = merge_patches(&ov_files, &[p_del_impl, p_edit_impl], None).unwrap();
    assert!(
        matches!(rep_same_impl.outcome, MergeOutcome::StructuralConflict { .. }),
        "deleting vs editing implementation def must conflict: {:?}",
        rep_same_impl.outcome
    );
}

#[test]
fn go_single_line_semicolon_struct_and_interface_remove_node() {
    // 1. Go single-line semicolon-separated struct RemoveNode + concurrent field mutation
    let sp = Path::new("pkg/s_semi.go");
    let sb = "package pkg\n\ntype S struct { A int; B int }\n";
    let s_rem_a = "package pkg\n\ntype S struct { B int }\n";
    let s_mut_b = "package pkg\n\ntype S struct { A int; B string }\n";
    let p_rem = diff_source_to_patch(&make_ctx(sp, LanguageKind::Go, "vp1", "a1", 1), sb, s_rem_a).unwrap();
    let p_mut = diff_source_to_patch(&make_ctx(sp, LanguageKind::Go, "vp2", "a2", 2), sb, s_mut_b).unwrap();
    let mut s_files = BTreeMap::new();
    s_files.insert(sp.to_path_buf(), sb.as_bytes().to_vec());

    let rep_single = merge_patches(&s_files, std::slice::from_ref(&p_rem), None).unwrap();
    assert!(rep_single.is_clean(), "Go struct single-line RemoveNode failed: {:?}", rep_single.conflicts);
    let ms = rep_single.text(sp).unwrap();
    assert_eq!(ms, "package pkg\n\ntype S struct { B int }\n");
    assert!(!parse_source(LanguageKind::Go, ms, None).unwrap().tree.root_node().has_error());

    let rep_both = merge_patches(&s_files, &[p_rem, p_mut], None).unwrap();
    assert!(rep_both.is_clean(), "Go struct RemoveNode + MutateSignature failed: {:?}", rep_both.conflicts);
    let ms_both = rep_both.text(sp).unwrap();
    assert_eq!(ms_both, "package pkg\n\ntype S struct { B string }\n");
    assert!(!parse_source(LanguageKind::Go, ms_both, None).unwrap().tree.root_node().has_error());

    // 2. Go single-line semicolon-separated interface RemoveNode + concurrent method mutation
    let ip = Path::new("pkg/i_semi.go");
    let ib = "package pkg\n\ntype I interface { A(); B() }\n";
    let i_rem_a = "package pkg\n\ntype I interface { B() }\n";
    let i_mut_b = "package pkg\n\ntype I interface { A(); B() int }\n";
    let pi_rem = diff_source_to_patch(&make_ctx(ip, LanguageKind::Go, "vp1", "a1", 1), ib, i_rem_a).unwrap();
    let pi_mut = diff_source_to_patch(&make_ctx(ip, LanguageKind::Go, "vp2", "a2", 2), ib, i_mut_b).unwrap();
    let mut i_files = BTreeMap::new();
    i_files.insert(ip.to_path_buf(), ib.as_bytes().to_vec());

    let rep_i_single = merge_patches(&i_files, std::slice::from_ref(&pi_rem), None).unwrap();
    assert!(rep_i_single.is_clean(), "Go interface single-line RemoveNode failed: {:?}", rep_i_single.conflicts);
    let mi = rep_i_single.text(ip).unwrap();
    assert_eq!(mi, "package pkg\n\ntype I interface { B() }\n");
    assert!(!parse_source(LanguageKind::Go, mi, None).unwrap().tree.root_node().has_error());

    let rep_i_both = merge_patches(&i_files, &[pi_rem, pi_mut], None).unwrap();
    assert!(rep_i_both.is_clean(), "Go interface RemoveNode + MutateSignature failed: {:?}", rep_i_both.conflicts);
    let mi_both = rep_i_both.text(ip).unwrap();
    assert_eq!(mi_both, "package pkg\n\ntype I interface { B() int }\n");
    assert!(!parse_source(LanguageKind::Go, mi_both, None).unwrap().tree.root_node().has_error());
}

fn assert_no_python_colon_row_mixed_with_indented_body(src: &str) {
    assert!(
        !parse_source(LanguageKind::Python, src, None)
            .unwrap()
            .tree
            .root_node()
            .has_error(),
        "Python CST has syntax error:\n{src}"
    );
    let lines: Vec<&str> = src.lines().collect();
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("class ")
            && let Some(colon_pos) = trimmed.find(':')
        {
            let class_indent = line.len() - trimmed.len();
            let after_colon = trimmed[colon_pos + 1..].trim();
            let has_inline_stmt = !after_colon.is_empty() && !after_colon.starts_with('#');
            if has_inline_stmt
                && let Some(next_non_empty) = lines[idx + 1..].iter().find(|l| !l.trim().is_empty())
            {
                let next_indent = next_non_empty.len() - next_non_empty.trim_start().len();
                assert!(
                    next_indent <= class_indent,
                    "invalid Python mixing colon-row suite with indented member on line {}:\n{src}",
                    idx + 1
                );
            }
        }
    }
}

#[test]
fn python_single_line_class_append_and_mid_insert() {
    // 1. `class One: x = 1` + concurrent `other()` edit and concurrent appends after `x = 1`
    let p_one = Path::new("one_append.py");
    let b_one = "class One: x = 1\n\ndef other():\n    return 0\n";
    let v_other = "class One: x = 1\n\ndef other():\n    return 1\n";
    let v_append_n = "class One:\n    x = 1\n    def n(self):\n        return 2\n\ndef other():\n    return 0\n";
    let v_append_m = "class One:\n    x = 1\n    def m(self):\n        return 3\n\ndef other():\n    return 0\n";

    let p_other =
        diff_source_to_patch(&make_ctx(p_one, LanguageKind::Python, "vp1", "a1", 1), b_one, v_other)
            .unwrap();
    let p_n = diff_source_to_patch(
        &make_ctx(p_one, LanguageKind::Python, "vp2", "a2", 2),
        b_one,
        v_append_n,
    )
    .unwrap();
    let p_m = diff_source_to_patch(
        &make_ctx(p_one, LanguageKind::Python, "vp3", "a3", 3),
        b_one,
        v_append_m,
    )
    .unwrap();
    let mut bm_one = BTreeMap::new();
    bm_one.insert(p_one.to_path_buf(), b_one.as_bytes().to_vec());

    let rep_single = merge_patches(&bm_one, std::slice::from_ref(&p_n), None).unwrap();
    assert!(rep_single.is_clean(), "single append on class One failed: {:?}", rep_single.conflicts);
    let m_single = rep_single.text(p_one).unwrap();
    assert_no_python_colon_row_mixed_with_indented_body(m_single);
    assert!(m_single.contains("class One:\n    x = 1\n\n    def n(self):\n        return 2"));

    let rep_other_n_12 = merge_patches(&bm_one, &[p_other.clone(), p_n.clone()], None).unwrap();
    let rep_other_n_21 = merge_patches(&bm_one, &[p_n.clone(), p_other.clone()], None).unwrap();
    assert!(rep_other_n_12.is_clean(), "concurrent other + append n failed: {:?}", rep_other_n_12.conflicts);
    assert_eq!(rep_other_n_12.text(p_one), rep_other_n_21.text(p_one));
    let m_other_n = rep_other_n_12.text(p_one).unwrap();
    assert_no_python_colon_row_mixed_with_indented_body(m_other_n);
    assert!(m_other_n.contains("class One:\n    x = 1\n\n    def n(self):\n        return 2"));
    assert!(m_other_n.contains("def other():\n    return 1"));

    let rep_three = merge_patches(&bm_one, &[p_other, p_n, p_m], None).unwrap();
    assert!(rep_three.is_clean(), "concurrent appends after x = 1 failed: {:?}", rep_three.conflicts);
    let m_three = rep_three.text(p_one).unwrap();
    assert_no_python_colon_row_mixed_with_indented_body(m_three);
    assert!(m_three.lines().any(|l| l == "    x = 1"));
    assert!(m_three.lines().any(|l| l == "    def n(self):"));
    assert!(m_three.lines().any(|l| l == "    def m(self):"));

    // 2. `class Doc: """doc"""; x = 1` + append after `x = 1`
    let p_doc = Path::new("doc_append.py");
    let b_doc = "class Doc: \"\"\"doc\"\"\"; x = 1\n\ndef other():\n    return 0\n";
    let v_doc_other = "class Doc: \"\"\"doc\"\"\"; x = 1\n\ndef other():\n    return 9\n";
    let v_doc_app = "class Doc:\n    \"\"\"doc\"\"\"\n    x = 1\n    def n(self):\n        return 2\n\ndef other():\n    return 0\n";
    let pd1 = diff_source_to_patch(
        &make_ctx(p_doc, LanguageKind::Python, "vp1", "a1", 1),
        b_doc,
        v_doc_other,
    )
    .unwrap();
    let pd2 = diff_source_to_patch(
        &make_ctx(p_doc, LanguageKind::Python, "vp2", "a2", 2),
        b_doc,
        v_doc_app,
    )
    .unwrap();
    let mut bm_doc = BTreeMap::new();
    bm_doc.insert(p_doc.to_path_buf(), b_doc.as_bytes().to_vec());
    let rep_doc = merge_patches(&bm_doc, &[pd1, pd2], None).unwrap();
    assert!(rep_doc.is_clean(), "class Doc append failed: {:?}", rep_doc.conflicts);
    let m_doc = rep_doc.text(p_doc).unwrap();
    assert_no_python_colon_row_mixed_with_indented_body(m_doc);
    assert!(
        m_doc.contains("class Doc:\n    \"\"\"doc\"\"\"\n    x = 1\n\n    def n(self):\n        return 2"),
        "unexpected Doc merge:\n{m_doc}"
    );
    assert!(m_doc.contains("return 9"));
    assert!(!m_doc.contains(';'), "semicolon should be stripped:\n{m_doc}");

    // 3. `class Two: x = 1; y = 2` -> mid-insert after `x = 1` and end-insert after `y = 2`
    let p_two = Path::new("two_append.py");
    let b_two = "class Two: x = 1; y = 2\n";
    let v_two_mid = "class Two:\n    x = 1\n    def mid(self):\n        return 1\n    y = 2\n";
    let v_two_end = "class Two:\n    x = 1\n    y = 2\n    def tail(self):\n        return 2\n";
    let pt_mid = diff_source_to_patch(
        &make_ctx(p_two, LanguageKind::Python, "vp1", "a1", 1),
        b_two,
        v_two_mid,
    )
    .unwrap();
    let pt_end = diff_source_to_patch(
        &make_ctx(p_two, LanguageKind::Python, "vp2", "a2", 2),
        b_two,
        v_two_end,
    )
    .unwrap();
    let mut bm_two = BTreeMap::new();
    bm_two.insert(p_two.to_path_buf(), b_two.as_bytes().to_vec());

    let rep_mid = merge_patches(&bm_two, std::slice::from_ref(&pt_mid), None).unwrap();
    assert!(rep_mid.is_clean(), "class Two mid-insert failed: {:?}", rep_mid.conflicts);
    let m_mid = rep_mid.text(p_two).unwrap();
    assert_no_python_colon_row_mixed_with_indented_body(m_mid);
    assert!(m_mid.contains("class Two:\n    x = 1\n\n    def mid(self):\n        return 1\n    y = 2"));
    assert!(!m_mid.contains(';'));

    let rep_end = merge_patches(&bm_two, std::slice::from_ref(&pt_end), None).unwrap();
    assert!(rep_end.is_clean(), "class Two end-insert failed: {:?}", rep_end.conflicts);
    let m_end = rep_end.text(p_two).unwrap();
    assert_no_python_colon_row_mixed_with_indented_body(m_end);
    assert!(m_end.contains("class Two:\n    x = 1\n    y = 2\n\n    def tail(self):\n        return 2"));
    assert!(!m_end.contains(';'));

    let rep_two_both = merge_patches(&bm_two, &[pt_mid, pt_end], None).unwrap();
    assert!(rep_two_both.is_clean(), "concurrent mid + end insert on class Two failed: {:?}", rep_two_both.conflicts);
    let m_two_both = rep_two_both.text(p_two).unwrap();
    assert_no_python_colon_row_mixed_with_indented_body(m_two_both);
    assert!(m_two_both.lines().any(|l| l == "    x = 1"));
    assert!(m_two_both.lines().any(|l| l == "    def mid(self):"));
    assert!(m_two_both.lines().any(|l| l == "    y = 2"));
    assert!(m_two_both.lines().any(|l| l == "    def tail(self):"));
    assert!(!m_two_both.contains(';'));
}

#[test]
fn python_overload_dunder_visibility_is_public() {
    let path = Path::new("dunder_overload.py");
    let fid = compute_logical_file_id(path);
    let base_src = "from typing import overload\n\nclass Seq:\n    @overload\n    def __getitem__(self, i: int) -> int: ...\n    def __getitem__(self, i):\n        return i\n    @overload\n    def _helper(self, x: int) -> int: ...\n    def _helper(self, x):\n        return x\n";
    let new_src = "from typing import overload\n\nclass Seq:\n    @overload\n    def __getitem__(self, i: int) -> int: ...\n    @overload\n    def __getitem__(self, i: slice) -> list[int]: ...\n    def __getitem__(self, i):\n        return i\n    @overload\n    def _helper(self, x: int) -> int: ...\n    def _helper(self, x):\n        return x\n";

    let parsed = parse_source(LanguageKind::Python, base_src, None).unwrap();
    let units = extract_decl_units(LanguageKind::Python, &parsed.tree, base_src, fid);
    let seq = units.iter().find(|u| u.anchor.symbol_fqn == "Seq").unwrap();
    let dunder_ov = seq
        .children
        .iter()
        .find(|u| u.anchor.symbol_fqn == "Seq::__getitem__#sig:1")
        .expect("Seq::__getitem__#sig:1 must exist");
    assert_eq!(dunder_ov.visibility, VisibilityKind::Public);

    let dunder_impl = seq
        .children
        .iter()
        .find(|u| u.anchor.symbol_fqn == "Seq::__getitem__")
        .expect("Seq::__getitem__ impl must exist");
    assert_eq!(dunder_impl.visibility, VisibilityKind::Public);

    let priv_ov = seq
        .children
        .iter()
        .find(|u| u.anchor.symbol_fqn == "Seq::_helper#sig:1")
        .expect("Seq::_helper#sig:1 must exist");
    assert_eq!(priv_ov.visibility, VisibilityKind::Private);

    let syms = extract_file_symbols(&parsed, fid, path, "vp1", 0);
    let dunder_prov = syms
        .provides
        .iter()
        .find(|p| p.anchor.symbol_fqn == "Seq::__getitem__#sig:1")
        .unwrap();
    assert_eq!(dunder_prov.visibility, VisibilityKind::Public);

    let patch = diff_source_to_patch(
        &make_ctx(path, LanguageKind::Python, "vp1", "a1", 1),
        base_src,
        new_src,
    )
    .unwrap();
    assert!(
        patch.contract_deltas.iter().any(|d| matches!(
            d,
            SymbolContractDelta::AddedSymbol { symbol_fqn, .. } if symbol_fqn == "Seq::__getitem__#sig:2"
        )),
        "public dunder @overload addition must emit AddedSymbol delta: {:?}",
        patch.contract_deltas
    );
}

#[test]
fn single_line_go_ts_mid_insert_and_trailing_comment_preservation() {
    // 1. Go single-line struct mid-insert after A (both single & concurrent) and append before '}'
    let go_sp = Path::new("pkg/s.go");
    let go_sb = "package pkg\n\ntype S struct { A int; B string }\n";
    let go_s1 = "package pkg\n\ntype S struct {\n    A int\n    X float64\n    B string\n}\n";
    let go_s2 = "package pkg\n\ntype S struct {\n    A int\n    Y bool\n    B string\n}\n";
    let p_go1 = diff_source_to_patch(&make_ctx(go_sp, LanguageKind::Go, "vp1", "a1", 1), go_sb, go_s1).unwrap();
    let p_go2 = diff_source_to_patch(&make_ctx(go_sp, LanguageKind::Go, "vp2", "a2", 2), go_sb, go_s2).unwrap();
    let mut go_files = BTreeMap::new();
    go_files.insert(go_sp.to_path_buf(), go_sb.as_bytes().to_vec());

    let rep_go_single = merge_patches(&go_files, std::slice::from_ref(&p_go1), None).unwrap();
    assert!(rep_go_single.is_clean(), "Go single-line struct mid-insert failed: {:?}", rep_go_single.conflicts);
    let m_go_s = rep_go_single.text(go_sp).unwrap();
    assert!(m_go_s.contains("A int") && m_go_s.contains("X float64") && m_go_s.contains("B string"));
    assert!(!parse_source(LanguageKind::Go, m_go_s, None).unwrap().tree.root_node().has_error(), "Go CST error:\n{m_go_s}");

    let rep_go_12 = merge_patches(&go_files, &[p_go1.clone(), p_go2.clone()], None).unwrap();
    let rep_go_21 = merge_patches(&go_files, &[p_go2, p_go1], None).unwrap();
    assert!(rep_go_12.is_clean(), "Go single-line struct concurrent mid-insert failed: {:?}", rep_go_12.conflicts);
    assert_eq!(rep_go_12.text(go_sp), rep_go_21.text(go_sp));
    let m_go_both = rep_go_12.text(go_sp).unwrap();
    assert!(m_go_both.contains("X float64") && m_go_both.contains("Y bool") && m_go_both.contains("B string"));
    assert!(!parse_source(LanguageKind::Go, m_go_both, None).unwrap().tree.root_node().has_error(), "Go CST error:\n{m_go_both}");

    // Go single-line struct append after sole member before '}'
    let go_app_b = "package pkg\n\ntype S struct { A int }\n";
    let go_app_1 = "package pkg\n\ntype S struct {\n    A int\n    B string\n}\n";
    let p_go_app = diff_source_to_patch(&make_ctx(go_sp, LanguageKind::Go, "vp1", "a1", 1), go_app_b, go_app_1).unwrap();
    let mut go_app_files = BTreeMap::new();
    go_app_files.insert(go_sp.to_path_buf(), go_app_b.as_bytes().to_vec());
    let rep_go_app = merge_patches(&go_app_files, &[p_go_app], None).unwrap();
    assert!(rep_go_app.is_clean(), "Go single-line struct append before '}}' failed: {:?}", rep_go_app.conflicts);
    let m_go_app = rep_go_app.text(go_sp).unwrap();
    assert!(m_go_app.contains("A int") && m_go_app.contains("B string"));
    assert!(!parse_source(LanguageKind::Go, m_go_app, None).unwrap().tree.root_node().has_error(), "Go CST error:\n{m_go_app}");

    // 2. Go single-line interface mid-insert after A()
    let go_ip = Path::new("pkg/i.go");
    let go_ib = "package pkg\n\ntype I interface { A(); B() }\n";
    let go_i1 = "package pkg\n\ntype I interface {\n    A()\n    X()\n    B()\n}\n";
    let p_gi = diff_source_to_patch(&make_ctx(go_ip, LanguageKind::Go, "vp1", "a1", 1), go_ib, go_i1).unwrap();
    let mut gi_files = BTreeMap::new();
    gi_files.insert(go_ip.to_path_buf(), go_ib.as_bytes().to_vec());
    let rep_gi = merge_patches(&gi_files, &[p_gi], None).unwrap();
    assert!(rep_gi.is_clean(), "Go single-line interface mid-insert failed: {:?}", rep_gi.conflicts);
    let m_gi = rep_gi.text(go_ip).unwrap();
    assert!(m_gi.contains("A()") && m_gi.contains("X()") && m_gi.contains("B()"));
    assert!(!parse_source(LanguageKind::Go, m_gi, None).unwrap().tree.root_node().has_error(), "Go interface CST error:\n{m_gi}");

    // 3. TS single-line interface and class mid-insert + append before '}'
    let ts_ip = Path::new("s.ts");
    let ts_ib = "export interface S { a: number; b: string }\n";
    let ts_i1 = "export interface S {\n    a: number;\n    x: boolean\n    b: string;\n}\n";
    let p_tsi = diff_source_to_patch(&make_ctx(ts_ip, LanguageKind::TypeScript, "vp1", "a1", 1), ts_ib, ts_i1).unwrap();
    let mut tsi_files = BTreeMap::new();
    tsi_files.insert(ts_ip.to_path_buf(), ts_ib.as_bytes().to_vec());
    let rep_tsi = merge_patches(&tsi_files, &[p_tsi], None).unwrap();
    assert!(rep_tsi.is_clean(), "TS single-line interface mid-insert failed: {:?}", rep_tsi.conflicts);
    let m_tsi = rep_tsi.text(ts_ip).unwrap();
    assert!(m_tsi.contains("a: number;") && m_tsi.contains("x: boolean") && m_tsi.contains("b: string"));
    assert!(!parse_source(LanguageKind::TypeScript, m_tsi, None).unwrap().tree.root_node().has_error(), "TS interface CST error:\n{m_tsi}");

    let ts_cb = "export class C { a = 1; b = 2 }\n";
    let ts_c1 = "export class C {\n    a = 1;\n    x = 3\n    b = 2;\n}\n";
    let p_tsc = diff_source_to_patch(&make_ctx(ts_ip, LanguageKind::TypeScript, "vp1", "a1", 1), ts_cb, ts_c1).unwrap();
    let mut tsc_files = BTreeMap::new();
    tsc_files.insert(ts_ip.to_path_buf(), ts_cb.as_bytes().to_vec());
    let rep_tsc = merge_patches(&tsc_files, &[p_tsc], None).unwrap();
    assert!(rep_tsc.is_clean(), "TS single-line class mid-insert failed: {:?}", rep_tsc.conflicts);
    let m_tsc = rep_tsc.text(ts_ip).unwrap();
    assert!(m_tsc.contains("a = 1;") && m_tsc.contains("x = 3") && m_tsc.contains("b = 2"));
    assert!(!parse_source(LanguageKind::TypeScript, m_tsc, None).unwrap().tree.root_node().has_error(), "TS class CST error:\n{m_tsc}");

    let ts_app_b = "export interface Single { a: number }\n";
    let ts_app_1 = "export interface Single {\n    a: number;\n    b: string\n}\n";
    let p_ts_app = diff_source_to_patch(&make_ctx(ts_ip, LanguageKind::TypeScript, "vp1", "a1", 1), ts_app_b, ts_app_1).unwrap();
    let mut ts_app_files = BTreeMap::new();
    ts_app_files.insert(ts_ip.to_path_buf(), ts_app_b.as_bytes().to_vec());
    let rep_ts_app = merge_patches(&ts_app_files, &[p_ts_app], None).unwrap();
    assert!(rep_ts_app.is_clean(), "TS single-line interface append before '}}' failed: {:?}", rep_ts_app.conflicts);
    let m_ts_app = rep_ts_app.text(ts_ip).unwrap();
    assert!(m_ts_app.contains("a: number") && m_ts_app.contains("b: string"));
    assert!(!parse_source(LanguageKind::TypeScript, m_ts_app, None).unwrap().tree.root_node().has_error(), "TS append CST error:\n{m_ts_app}");

    // 4. Same-line trailing comment preservation on after_unit (Rust, Go, TS)
    let rs_p = Path::new("trailing.rs");
    let rs_b = "pub fn a() -> i32 { 1 } // trailing on a\n\npub fn c() -> i32 { 3 }\n";
    let rs_1 = "pub fn a() -> i32 { 1 } // trailing on a\n\npub fn b() -> i32 { 2 }\n\npub fn c() -> i32 { 3 }\n";
    let p_rs = diff_source_to_patch(&make_ctx(rs_p, LanguageKind::Rust, "vp1", "a1", 1), rs_b, rs_1).unwrap();
    let mut rs_files = BTreeMap::new();
    rs_files.insert(rs_p.to_path_buf(), rs_b.as_bytes().to_vec());
    let rep_rs = merge_patches(&rs_files, &[p_rs], None).unwrap();
    assert!(rep_rs.is_clean(), "Rust trailing comment insert failed: {:?}", rep_rs.conflicts);
    assert_eq!(
        rep_rs.text(rs_p).unwrap(),
        "pub fn a() -> i32 { 1 } // trailing on a\n\npub fn b() -> i32 { 2 }\n\npub fn c() -> i32 { 3 }\n"
    );

    let go_tc_b = "package pkg\n\ntype S struct {\n    A int // trailing A\n    C string\n}\n";
    let go_tc_1 = "package pkg\n\ntype S struct {\n    A int // trailing A\n    B float64\n    C string\n}\n";
    let p_gtc = diff_source_to_patch(&make_ctx(go_sp, LanguageKind::Go, "vp1", "a1", 1), go_tc_b, go_tc_1).unwrap();
    let mut gtc_files = BTreeMap::new();
    gtc_files.insert(go_sp.to_path_buf(), go_tc_b.as_bytes().to_vec());
    let rep_gtc = merge_patches(&gtc_files, &[p_gtc], None).unwrap();
    assert!(rep_gtc.is_clean(), "Go trailing comment insert failed: {:?}", rep_gtc.conflicts);
    let m_gtc = rep_gtc.text(go_sp).unwrap();
    assert!(m_gtc.contains("A int // trailing A\n"), "trailing comment moved off A:\n{m_gtc}");
    assert!(m_gtc.contains("B float64"));
}

#[test]
fn python_colon_row_remove_last_and_only_inline_statement() {
    let py_p = Path::new("colon_rem.py");

    // 1. Remove last inline statement followed by simple module-level statement
    let b1 = "class Box: x = 1; y = 2\nz = 3\n";
    let v1 = "class Box: x = 1\nz = 3\n";
    let p1 = diff_source_to_patch(&make_ctx(py_p, LanguageKind::Python, "vp1", "a1", 1), b1, v1).unwrap();
    let mut fm1 = BTreeMap::new();
    fm1.insert(py_p.to_path_buf(), b1.as_bytes().to_vec());
    let rep1 = merge_patches(&fm1, &[p1], None).unwrap();
    assert!(rep1.is_clean(), "colon-row last stmt remove failed: {:?}", rep1.conflicts);
    assert_eq!(rep1.text(py_p).unwrap(), "class Box: x = 1\nz = 3\n");

    // 2. Remove last inline statement followed by top-level `def after(): pass`
    let b2 = "class Box: x = 1; y = 2\ndef after(): pass\n";
    let v2 = "class Box: x = 1\ndef after(): pass\n";
    let p2 = diff_source_to_patch(&make_ctx(py_p, LanguageKind::Python, "vp1", "a1", 1), b2, v2).unwrap();
    let mut fm2 = BTreeMap::new();
    fm2.insert(py_p.to_path_buf(), b2.as_bytes().to_vec());
    let rep2 = merge_patches(&fm2, &[p2], None).unwrap();
    assert!(rep2.is_clean(), "colon-row last stmt before def failed: {:?}", rep2.conflicts);
    assert_eq!(rep2.text(py_p).unwrap(), "class Box: x = 1\ndef after(): pass\n");

    // 3. Nested class colon-row last stmt followed by outer class method
    let b3 = "class Outer:\n    class Inner: x = 1; y = 2\n    def outer_m(self): pass\n";
    let v3 = "class Outer:\n    class Inner: x = 1\n    def outer_m(self): pass\n";
    let p3 = diff_source_to_patch(&make_ctx(py_p, LanguageKind::Python, "vp1", "a1", 1), b3, v3).unwrap();
    let mut fm3 = BTreeMap::new();
    fm3.insert(py_p.to_path_buf(), b3.as_bytes().to_vec());
    let rep3 = merge_patches(&fm3, &[p3], None).unwrap();
    assert!(rep3.is_clean(), "nested colon-row last stmt failed: {:?}", rep3.conflicts);
    assert_eq!(
        rep3.text(py_p).unwrap(),
        "class Outer:\n    class Inner: x = 1\n    def outer_m(self): pass\n"
    );

    // 4. Colon-row last stmt with trailing `# comment`
    let b4 = "class Box: x = 1; y = 2 # keep note\nz = 3\n";
    let v4 = "class Box: x = 1 # keep note\nz = 3\n";
    let p4 = diff_source_to_patch(&make_ctx(py_p, LanguageKind::Python, "vp1", "a1", 1), b4, v4).unwrap();
    let mut fm4 = BTreeMap::new();
    fm4.insert(py_p.to_path_buf(), b4.as_bytes().to_vec());
    let rep4 = merge_patches(&fm4, &[p4], None).unwrap();
    assert!(rep4.is_clean(), "colon-row last stmt with comment failed: {:?}", rep4.conflicts);
    assert_eq!(rep4.text(py_p).unwrap(), "class Box: x = 1 # keep note\nz = 3\n");

    // 5. Concurrent removal of both inline statements on colon row -> `class Box: pass\nz = 3\n`
    let v1_x = "class Box: y = 2\nz = 3\n";
    let v1_y = "class Box: x = 1\nz = 3\n";
    let px = diff_source_to_patch(&make_ctx(py_p, LanguageKind::Python, "vp1", "a1", 1), b1, v1_x).unwrap();
    let py = diff_source_to_patch(&make_ctx(py_p, LanguageKind::Python, "vp2", "a2", 2), b1, v1_y).unwrap();
    let rep_both12 = merge_patches(&fm1, &[px.clone(), py.clone()], None).unwrap();
    let rep_both21 = merge_patches(&fm1, &[py, px], None).unwrap();
    assert!(rep_both12.is_clean(), "concurrent remove both inline stmts failed: {:?}", rep_both12.conflicts);
    assert_eq!(rep_both12.text(py_p), rep_both21.text(py_p));
    assert_eq!(rep_both12.text(py_p).unwrap(), "class Box: pass\nz = 3\n");

    // 6. Removing middle + last in 3-stmt colon row (`x = 1; y = 2; w = 4`)
    let b6 = "class Box: x = 1; y = 2; w = 4\nz = 3\n";
    let v6_y = "class Box: x = 1; w = 4\nz = 3\n";
    let v6_w = "class Box: x = 1; y = 2\nz = 3\n";
    let p6y = diff_source_to_patch(&make_ctx(py_p, LanguageKind::Python, "vp1", "a1", 1), b6, v6_y).unwrap();
    let p6w = diff_source_to_patch(&make_ctx(py_p, LanguageKind::Python, "vp2", "a2", 2), b6, v6_w).unwrap();
    let mut fm6 = BTreeMap::new();
    fm6.insert(py_p.to_path_buf(), b6.as_bytes().to_vec());
    let rep6 = merge_patches(&fm6, &[p6y, p6w], None).unwrap();
    assert!(rep6.is_clean(), "3-stmt colon row remove middle+last failed: {:?}", rep6.conflicts);
    assert_eq!(rep6.text(py_p).unwrap(), "class Box: x = 1\nz = 3\n");

    // 7. Removing the ONLY inline statement on a colon row (`class Box: x = 1\nz = 3\n`)
    let b7 = "class Box: x = 1\nz = 3\n";
    let v7 = "class Box: pass\nz = 3\n";
    let p7 = diff_source_to_patch(&make_ctx(py_p, LanguageKind::Python, "vp1", "a1", 1), b7, v7).unwrap();
    let mut fm7 = BTreeMap::new();
    fm7.insert(py_p.to_path_buf(), b7.as_bytes().to_vec());
    let rep7 = merge_patches(&fm7, &[p7], None).unwrap();
    assert!(rep7.is_clean(), "only inline stmt remove failed: {:?}", rep7.conflicts);
    assert_eq!(rep7.text(py_p).unwrap(), "class Box: pass\nz = 3\n");
}

#[test]
fn cross_file_rename_cancels_false_removed_symbol_conflict_while_preserving_genuine_breakage() {
    let old_p = Path::new("src/Foo.rs");
    let new_p = Path::new("src/foo.rs");
    let caller_p = Path::new("src/caller.rs");

    let foo_src = "pub fn helper_fn() -> i32 { 1 }\n";
    let caller_base = "pub fn call() -> i32 { helper_fn() }\n";
    let caller_edit = "pub fn call() -> i32 { helper_fn() + 1 }\n";

    let mut base_files = BTreeMap::new();
    base_files.insert(old_p.to_path_buf(), foo_src.as_bytes().to_vec());
    base_files.insert(caller_p.to_path_buf(), caller_base.as_bytes().to_vec());

    // 1. Clean rename `Foo.rs` -> `foo.rs` with identical signature + concurrent edit to `caller.rs`
    let p_del_old =
        diff_source_to_patch(&make_ctx(old_p, LanguageKind::Rust, "remote", "remote", 1), foo_src, "")
            .unwrap();
    let p_add_new =
        diff_source_to_patch(&make_ctx(new_p, LanguageKind::Rust, "remote", "remote", 1), "", foo_src)
            .unwrap();
    let p_caller = diff_source_to_patch(
        &make_ctx(caller_p, LanguageKind::Rust, "vp1", "vp1", 2),
        caller_base,
        caller_edit,
    )
    .unwrap();

    let rep_clean = merge_patches(&base_files, &[p_del_old.clone(), p_add_new, p_caller.clone()], None)
        .unwrap();
    assert!(
        rep_clean.is_clean(),
        "identical cross-file rename must not emit SemanticConflict: {:?}",
        rep_clean.conflicts
    );

    // Also via merge_trees with untouched caller.rs
    let p_del_only = merge_trees(
        &base_files,
        &[&[
            p_del_old.clone(),
            diff_source_to_patch(
                &make_ctx(new_p, LanguageKind::Rust, "remote", "remote", 1),
                "",
                foo_src,
            )
            .unwrap(),
        ]],
        None,
    )
    .unwrap();
    assert!(
        p_del_only.is_clean(),
        "merge_trees with untouched caller must not emit false SemanticConflict: {:?}",
        p_del_only.conflicts
    );

    // 2. Rename + incompatible signature change (`helper_fn(x: i32)`) -> SemanticConflict!
    let foo_changed_sig = "pub fn helper_fn(x: i32) -> i32 { x }\n";
    let p_add_changed = diff_source_to_patch(
        &make_ctx(new_p, LanguageKind::Rust, "remote", "remote", 1),
        "",
        foo_changed_sig,
    )
    .unwrap();
    let rep_sig_break =
        merge_patches(&base_files, &[p_del_old.clone(), p_add_changed, p_caller.clone()], None)
            .unwrap();
    assert!(
        matches!(
            rep_sig_break.outcome,
            MergeOutcome::SemanticConflict { ref symbol_fqn, .. } if symbol_fqn == "helper_fn"
        ),
        "rename with incompatible signature must emit SemanticConflict: {:?}",
        rep_sig_break.outcome
    );

    // 3. Rename + visibility reduction (`pub fn` -> `fn`) -> SemanticConflict!
    let foo_priv = "fn helper_fn() -> i32 { 1 }\n";
    let p_add_priv = diff_source_to_patch(
        &make_ctx(new_p, LanguageKind::Rust, "remote", "remote", 1),
        "",
        foo_priv,
    )
    .unwrap();
    let rep_vis_break =
        merge_patches(&base_files, &[p_del_old.clone(), p_add_priv, p_caller.clone()], None)
            .unwrap();
    assert!(
        matches!(
            rep_vis_break.outcome,
            MergeOutcome::SemanticConflict { ref symbol_fqn, .. } if symbol_fqn == "helper_fn"
        ),
        "rename with reduced visibility must emit SemanticConflict: {:?}",
        rep_vis_break.outcome
    );

    // 4. Genuine deletion (no `foo.rs` created) -> SemanticConflict!
    let rep_gen_del = merge_patches(&base_files, &[p_del_old, p_caller], None).unwrap();
    assert!(
        matches!(
            rep_gen_del.outcome,
            MergeOutcome::SemanticConflict { ref symbol_fqn, .. } if symbol_fqn == "helper_fn"
        ),
        "genuine symbol deletion must still emit SemanticConflict: {:?}",
        rep_gen_del.outcome
    );
}

#[test]
fn ts_var_arrow_block_and_non_block_transitions() {
    let p = Path::new("src/a.ts");

    let block_arrow = "export const f = (x: number) => { return x + 1; };\n";
    let expr_arrow = "export const f = (x: number) => x + 1;\n";
    let block_zero = "export const f = () => { return 1; };\n";
    let literal_val = "export const f = 1;\n";
    let fn_expr = "const h = function(x: number) { return x; };\n";
    let fn_arrow_expr = "const h = (x: number) => x;\n";

    let cases = [
        ("Block -> Expr", block_arrow, expr_arrow),
        ("Expr -> Block", expr_arrow, block_arrow),
        ("Block -> Literal", block_zero, literal_val),
        ("Literal -> Block", literal_val, block_zero),
        ("FnExpr -> ExprArrow", fn_expr, fn_arrow_expr),
        ("ExprArrow -> FnExpr", fn_arrow_expr, fn_expr),
    ];

    for (label, before, after) in cases {
        let patch =
            diff_source_to_patch(&make_ctx(p, LanguageKind::TypeScript, "vp1", "a1", 1), before, after)
                .unwrap();
        let mut fm = BTreeMap::new();
        fm.insert(p.to_path_buf(), before.as_bytes().to_vec());
        let rep = merge_patches(&fm, &[patch], None).unwrap();
        assert!(
            rep.is_clean(),
            "{label} must merge cleanly, got {:?}",
            rep.conflicts
        );
        let merged = rep.text(p).unwrap();
        assert_eq!(merged.trim(), after.trim(), "{label} corrupted output:\n{merged}");
        assert!(
            !parse_source(LanguageKind::TypeScript, merged, None)
                .unwrap()
                .tree
                .root_node()
                .has_error(),
            "{label} produced TS CST syntax error:\n{merged}"
        );
    }

    // Concurrent edit to removed block body + Block -> Expr conversion must yield StructuralConflict
    let block_body_edit = "export const f = (x: number) => { return x + 99; };\n";
    let p_conv = diff_source_to_patch(
        &make_ctx(p, LanguageKind::TypeScript, "vp1", "a1", 1),
        block_arrow,
        expr_arrow,
    )
    .unwrap();
    let p_edit = diff_source_to_patch(
        &make_ctx(p, LanguageKind::TypeScript, "vp2", "a2", 2),
        block_arrow,
        block_body_edit,
    )
    .unwrap();
    let mut fm_conc = BTreeMap::new();
    fm_conc.insert(p.to_path_buf(), block_arrow.as_bytes().to_vec());
    let rep_conc = merge_patches(&fm_conc, &[p_conv, p_edit], None).unwrap();
    assert!(
        matches!(rep_conc.outcome, MergeOutcome::StructuralConflict { .. }),
        "Block -> Expr vs concurrent ReplaceBody must conflict: {:?}",
        rep_conc.outcome
    );
}

#[test]
fn python_concurrent_remove_all_class_members_inserts_pass() {
    let py_p = Path::new("empty_cls.py");

    // 1. Top-level class followed by statement
    let b1 = "class Box:\n    x = 1\n    y = 2\nz = 3\n";
    let v1_x = "class Box:\n    y = 2\nz = 3\n";
    let v1_y = "class Box:\n    x = 1\nz = 3\n";
    let p1x = diff_source_to_patch(&make_ctx(py_p, LanguageKind::Python, "vp1", "a1", 1), b1, v1_x).unwrap();
    let p1y = diff_source_to_patch(&make_ctx(py_p, LanguageKind::Python, "vp2", "a2", 2), b1, v1_y).unwrap();
    let mut fm1 = BTreeMap::new();
    fm1.insert(py_p.to_path_buf(), b1.as_bytes().to_vec());
    let r12 = merge_patches(&fm1, &[p1x.clone(), p1y.clone()], None).unwrap();
    let r21 = merge_patches(&fm1, &[p1y, p1x], None).unwrap();
    assert!(r12.is_clean(), "top-level empty class failed: {:?}", r12.conflicts);
    assert_eq!(r12.text(py_p), r21.text(py_p));
    assert_eq!(r12.text(py_p).unwrap(), "class Box:\n    pass\nz = 3\n");

    // 2. Nested class
    let b2 = "class Outer:\n    class Box:\n        x = 1\n        y = 2\n    def m(self): pass\n";
    let v2_x = "class Outer:\n    class Box:\n        y = 2\n    def m(self): pass\n";
    let v2_y = "class Outer:\n    class Box:\n        x = 1\n    def m(self): pass\n";
    let p2x = diff_source_to_patch(&make_ctx(py_p, LanguageKind::Python, "vp1", "a1", 1), b2, v2_x).unwrap();
    let p2y = diff_source_to_patch(&make_ctx(py_p, LanguageKind::Python, "vp2", "a2", 2), b2, v2_y).unwrap();
    let mut fm2 = BTreeMap::new();
    fm2.insert(py_p.to_path_buf(), b2.as_bytes().to_vec());
    let r2 = merge_patches(&fm2, &[p2x, p2y], None).unwrap();
    assert!(r2.is_clean(), "nested empty class failed: {:?}", r2.conflicts);
    assert_eq!(
        r2.text(py_p).unwrap(),
        "class Outer:\n    class Box:\n        pass\n    def m(self): pass\n"
    );

    // 3. Class at EOF
    let b3 = "class Box:\n    x = 1\n    y = 2\n";
    let v3_x = "class Box:\n    y = 2\n";
    let v3_y = "class Box:\n    x = 1\n";
    let p3x = diff_source_to_patch(&make_ctx(py_p, LanguageKind::Python, "vp1", "a1", 1), b3, v3_x).unwrap();
    let p3y = diff_source_to_patch(&make_ctx(py_p, LanguageKind::Python, "vp2", "a2", 2), b3, v3_y).unwrap();
    let mut fm3 = BTreeMap::new();
    fm3.insert(py_p.to_path_buf(), b3.as_bytes().to_vec());
    let r3 = merge_patches(&fm3, &[p3x, p3y], None).unwrap();
    assert!(r3.is_clean(), "EOF empty class failed: {:?}", r3.conflicts);
    assert_eq!(r3.text(py_p).unwrap(), "class Box:\n    pass\n");

    // 4. CRLF preservation
    let b4 = "class Box:\r\n    x = 1\r\n    y = 2\r\nz = 3\r\n";
    let v4_x = "class Box:\r\n    y = 2\r\nz = 3\r\n";
    let v4_y = "class Box:\r\n    x = 1\r\nz = 3\r\n";
    let p4x = diff_source_to_patch(&make_ctx(py_p, LanguageKind::Python, "vp1", "a1", 1), b4, v4_x).unwrap();
    let p4y = diff_source_to_patch(&make_ctx(py_p, LanguageKind::Python, "vp2", "a2", 2), b4, v4_y).unwrap();
    let mut fm4 = BTreeMap::new();
    fm4.insert(py_p.to_path_buf(), b4.as_bytes().to_vec());
    let r4 = merge_patches(&fm4, &[p4x, p4y], None).unwrap();
    assert!(r4.is_clean(), "CRLF empty class failed: {:?}", r4.conflicts);
    assert_eq!(r4.text(py_p).unwrap(), "class Box:\r\n    pass\r\nz = 3\r\n");

    // 5. Class with docstring keeps docstring without inserting redundant pass
    let b5 = "class Box:\n    \"\"\"Doc.\"\"\"\n    x = 1\n    y = 2\nz = 3\n";
    let v5_x = "class Box:\n    \"\"\"Doc.\"\"\"\n    y = 2\nz = 3\n";
    let v5_y = "class Box:\n    \"\"\"Doc.\"\"\"\n    x = 1\nz = 3\n";
    let p5x = diff_source_to_patch(&make_ctx(py_p, LanguageKind::Python, "vp1", "a1", 1), b5, v5_x).unwrap();
    let p5y = diff_source_to_patch(&make_ctx(py_p, LanguageKind::Python, "vp2", "a2", 2), b5, v5_y).unwrap();
    let mut fm5 = BTreeMap::new();
    fm5.insert(py_p.to_path_buf(), b5.as_bytes().to_vec());
    let r5 = merge_patches(&fm5, &[p5x, p5y], None).unwrap();
    assert!(r5.is_clean(), "docstring class failed: {:?}", r5.conflicts);
    assert_eq!(
        r5.text(py_p).unwrap(),
        "class Box:\n    \"\"\"Doc.\"\"\"\nz = 3\n"
    );
}

#[test]
fn ts_comma_separated_interface_mid_insert_and_remove() {
    let p = Path::new("src/s.ts");
    let base = "export interface S { a: number, b: string }\n";
    let v1 = "export interface S {\n    a: number,\n    x: boolean;\n    b: string\n}\n";
    let v2 = "export interface S {\n    a: number,\n    y: bigint;\n    b: string\n}\n";

    let p1 = diff_source_to_patch(&make_ctx(p, LanguageKind::TypeScript, "vp1", "a1", 1), base, v1).unwrap();
    let p2 = diff_source_to_patch(&make_ctx(p, LanguageKind::TypeScript, "vp2", "a2", 2), base, v2).unwrap();
    let mut fm = BTreeMap::new();
    fm.insert(p.to_path_buf(), base.as_bytes().to_vec());

    // 1. Single mid-insert with ';' after comma-terminated member
    let rep_single = merge_patches(&fm, std::slice::from_ref(&p1), None).unwrap();
    assert!(
        rep_single.is_clean(),
        "TS comma-separated interface mid-insert failed: {:?}",
        rep_single.conflicts
    );
    let ms = rep_single.text(p).unwrap();
    assert!(ms.contains("a: number,") && ms.contains("x: boolean;") && ms.contains("b: string"));
    assert!(
        !parse_source(LanguageKind::TypeScript, ms, None)
            .unwrap()
            .tree
            .root_node()
            .has_error(),
        "TS CST error:\n{ms}"
    );

    // 2. Concurrent mid-inserts after comma-terminated member
    let rep12 = merge_patches(&fm, &[p1.clone(), p2.clone()], None).unwrap();
    let rep21 = merge_patches(&fm, &[p2, p1], None).unwrap();
    assert!(
        rep12.is_clean(),
        "TS comma-separated interface concurrent mid-insert failed: {:?}",
        rep12.conflicts
    );
    assert_eq!(rep12.text(p), rep21.text(p));
    let mb = rep12.text(p).unwrap();
    assert!(
        mb.contains("a: number,")
            && mb.contains("x: boolean;")
            && mb.contains("y: bigint;")
            && mb.contains("b: string")
    );
    assert!(
        !parse_source(LanguageKind::TypeScript, mb, None)
            .unwrap()
            .tree
            .root_node()
            .has_error(),
        "TS concurrent CST error:\n{mb}"
    );

    // 3. RemoveNode on comma-terminated first member does not leave orphaned leading comma
    let v_rem = "export interface S { b: string }\n";
    let p_rem =
        diff_source_to_patch(&make_ctx(p, LanguageKind::TypeScript, "vp3", "a3", 3), base, v_rem)
            .unwrap();
    let rep_rem = merge_patches(&fm, &[p_rem], None).unwrap();
    assert!(rep_rem.is_clean(), "TS comma interface RemoveNode failed: {:?}", rep_rem.conflicts);
    let mr = rep_rem.text(p).unwrap();
    assert!(!mr.contains(','), "orphaned comma left after RemoveNode:\n{mr}");
    assert!(
        !parse_source(LanguageKind::TypeScript, mr, None)
            .unwrap()
            .tree
            .root_node()
            .has_error(),
        "TS RemoveNode CST error:\n{mr}"
    );
}

#[test]
fn python_inline_stub_with_trailing_comment_concurrent_insert_members() {
    let py_p = Path::new("stub_comment.py");

    // 1. `class Foo: pass # stub\n` + concurrent 1-line defs
    let b1 = "class Foo: pass # stub\n";
    let v1_a = "class Foo:\n    def a(self): return 1\n";
    let v1_b = "class Foo:\n    def b(self): return 2\n";
    let p1a = diff_source_to_patch(&make_ctx(py_p, LanguageKind::Python, "vp1", "a1", 1), b1, v1_a).unwrap();
    let p1b = diff_source_to_patch(&make_ctx(py_p, LanguageKind::Python, "vp2", "a2", 2), b1, v1_b).unwrap();
    let mut fm1 = BTreeMap::new();
    fm1.insert(py_p.to_path_buf(), b1.as_bytes().to_vec());
    let r12 = merge_patches(&fm1, &[p1a.clone(), p1b.clone()], None).unwrap();
    let r21 = merge_patches(&fm1, &[p1b, p1a], None).unwrap();
    assert!(r12.is_clean(), "1-line pass # stub failed: {:?}", r12.conflicts);
    assert_eq!(r12.text(py_p), r21.text(py_p));
    let m1 = r12.text(py_p).unwrap();
    assert!(m1.contains("class Foo: # stub"), "header comment must be preserved on class line:\n{m1}");
    assert!(m1.contains("def a(self): return 1") && m1.contains("def b(self): return 2"));
    assert!(
        !parse_source(LanguageKind::Python, m1, None).unwrap().tree.root_node().has_error(),
        "Python CST error:\n{m1}"
    );

    // 2. `class Foo: pass # stub\n` + concurrent multi-line defs
    let v2_a = "class Foo:\n    def a(self):\n        return 1\n";
    let v2_b = "class Foo:\n    def b(self):\n        return 2\n";
    let p2a = diff_source_to_patch(&make_ctx(py_p, LanguageKind::Python, "vp1", "a1", 1), b1, v2_a).unwrap();
    let p2b = diff_source_to_patch(&make_ctx(py_p, LanguageKind::Python, "vp2", "a2", 2), b1, v2_b).unwrap();
    let r2 = merge_patches(&fm1, &[p2a, p2b], None).unwrap();
    assert!(r2.is_clean(), "multi-line pass # stub failed: {:?}", r2.conflicts);
    let m2 = r2.text(py_p).unwrap();
    assert!(m2.contains("class Foo: # stub"));
    assert!(m2.contains("def a(self):") && m2.contains("def b(self):"));
    assert!(
        !parse_source(LanguageKind::Python, m2, None).unwrap().tree.root_node().has_error(),
        "Python CST error:\n{m2}"
    );

    // 3. `class Foo: ... # stub\n` + concurrent defs
    let b3 = "class Foo: ... # stub\n";
    let p3a = diff_source_to_patch(&make_ctx(py_p, LanguageKind::Python, "vp1", "a1", 1), b3, v2_a).unwrap();
    let p3b = diff_source_to_patch(&make_ctx(py_p, LanguageKind::Python, "vp2", "a2", 2), b3, v2_b).unwrap();
    let mut fm3 = BTreeMap::new();
    fm3.insert(py_p.to_path_buf(), b3.as_bytes().to_vec());
    let r3 = merge_patches(&fm3, &[p3a, p3b], None).unwrap();
    assert!(r3.is_clean(), "ellipsis # stub failed: {:?}", r3.conflicts);
    let m3 = r3.text(py_p).unwrap();
    assert!(m3.contains("class Foo: # stub"));
    assert!(m3.contains("def a(self):") && m3.contains("def b(self):"));
    assert!(
        !parse_source(LanguageKind::Python, m3, None).unwrap().tree.root_node().has_error(),
        "Python CST error:\n{m3}"
    );

    // 4. EOF without trailing newline and CRLF
    for b_edge in ["class Foo: pass # stub", "class Foo: pass # stub\r\n"] {
        let p_ea = diff_source_to_patch(&make_ctx(py_p, LanguageKind::Python, "vp1", "a1", 1), b_edge, v1_a).unwrap();
        let p_eb = diff_source_to_patch(&make_ctx(py_p, LanguageKind::Python, "vp2", "a2", 2), b_edge, v1_b).unwrap();
        let mut fm_e = BTreeMap::new();
        fm_e.insert(py_p.to_path_buf(), b_edge.as_bytes().to_vec());
        let r_e = merge_patches(&fm_e, &[p_ea, p_eb], None).unwrap();
        assert!(r_e.is_clean(), "edge case {b_edge:?} failed: {:?}", r_e.conflicts);
        let me = r_e.text(py_p).unwrap();
        assert!(me.contains("class Foo: # stub"));
        assert!(me.contains("def a(self): return 1") && me.contains("def b(self): return 2"));
        assert!(
            !parse_source(LanguageKind::Python, me, None).unwrap().tree.root_node().has_error(),
            "Python CST error for {b_edge:?}:\n{me}"
        );
    }
}

#[test]
fn moved_symbol_visibility_reduced_checks_destination_file_ts_and_rust() {
    // Part A: TypeScript a.ts -> b.ts (move export helper -> non-exported function helper)
    {
        let pa = Path::new("src/a.ts");
        let pb = Path::new("src/b.ts");
        let pc = Path::new("src/c.ts");

        let base_a = "export function helper(): number { return 1; }\nexport function useA(): number { return 0; }\n";
        let base_b = "export function useB(): number { return 0; }\n";
        let base_c = "export function useC(): number { return 0; }\n";

        let mut base = BTreeMap::new();
        base.insert(pa.to_path_buf(), base_a.as_bytes().to_vec());
        base.insert(pb.to_path_buf(), base_b.as_bytes().to_vec());
        base.insert(pc.to_path_buf(), base_c.as_bytes().to_vec());

        // Producer moves helper from a.ts to b.ts as non-exported (Private)
        let move_a = "export function useA(): number { return 0; }\n";
        let move_b = "function helper(): number { return 1; }\nexport function useB(): number { return 0; }\n";
        let pm_a = diff_source_to_patch(&make_ctx(pa, LanguageKind::TypeScript, "vp1", "a1", 1), base_a, move_a).unwrap();
        let pm_b = diff_source_to_patch(&make_ctx(pb, LanguageKind::TypeScript, "vp1", "a1", 2), base_b, move_b).unwrap();

        // 1. Caller in a.ts (origin file) -> SemanticConflict
        let call_a = "export function helper(): number { return 1; }\nexport function useA(): number { return helper(); }\n";
        let pc_a = diff_source_to_patch(&make_ctx(pa, LanguageKind::TypeScript, "vp2", "a2", 3), base_a, call_a).unwrap();
        let rep_a = merge_patches(&base, &[pm_a.clone(), pm_b.clone(), pc_a.clone()], None).unwrap();
        assert!(
            matches!(rep_a.outcome, MergeOutcome::SemanticConflict { .. }),
            "TS caller in origin a.ts must trigger SemanticConflict when moved+unexported to b.ts: {:?}",
            rep_a.outcome
        );
        let div_a = detect_divergent_base_conflicts(&base, &[pm_a.clone(), pm_b.clone()], &base, &[pc_a]).unwrap();
        assert!(
            div_a.iter().any(|c| matches!(c, MergeOutcome::SemanticConflict { symbol_fqn, .. } if symbol_fqn == "helper")),
            "TS divergent_base caller in a.ts must trigger SemanticConflict: {div_a:?}"
        );

        // 2. Caller in b.ts (destination file) -> Clean
        let call_b = "export function useB(): number { return helper(); }\n";
        let pc_b = diff_source_to_patch(&make_ctx(pb, LanguageKind::TypeScript, "vp2", "a2", 3), base_b, call_b).unwrap();
        let rep_b = merge_patches(&base, &[pm_a.clone(), pm_b.clone(), pc_b.clone()], None).unwrap();
        assert!(
            rep_b.is_clean(),
            "TS caller in destination b.ts must merge Clean when helper moved+unexported into b.ts: {:?}",
            rep_b.conflicts
        );
        let div_b = detect_divergent_base_conflicts(&base, &[pm_a.clone(), pm_b.clone()], &base, &[pc_b]).unwrap();
        assert!(
            !div_b.iter().any(|c| matches!(c, MergeOutcome::SemanticConflict { .. })),
            "TS divergent_base caller in destination b.ts must NOT emit SemanticConflict: {div_b:?}"
        );

        // 3. Caller in c.ts (unrelated third file) -> SemanticConflict
        let call_c = "export function useC(): number { return helper(); }\n";
        let pc_c = diff_source_to_patch(&make_ctx(pc, LanguageKind::TypeScript, "vp2", "a2", 3), base_c, call_c).unwrap();
        let rep_c = merge_patches(&base, &[pm_a.clone(), pm_b.clone(), pc_c.clone()], None).unwrap();
        assert!(
            matches!(rep_c.outcome, MergeOutcome::SemanticConflict { .. }),
            "TS caller in third file c.ts must trigger SemanticConflict: {:?}",
            rep_c.outcome
        );
        let div_c = detect_divergent_base_conflicts(&base, &[pm_a, pm_b], &base, &[pc_c]).unwrap();
        assert!(
            div_c.iter().any(|c| matches!(c, MergeOutcome::SemanticConflict { symbol_fqn, .. } if symbol_fqn == "helper")),
            "TS divergent_base caller in c.ts must trigger SemanticConflict: {div_c:?}"
        );
    }

    // Part B: Rust src/a.rs -> src/b.rs (move pub fn helper -> private fn helper)
    {
        let pa = Path::new("src/a.rs");
        let pb = Path::new("src/b.rs");
        let pc = Path::new("src/c.rs");

        let base_a = "pub fn helper() -> i32 { 1 }\npub fn use_a() -> i32 { 0 }\n";
        let base_b = "pub fn use_b() -> i32 { 0 }\n";
        let base_c = "pub fn use_c() -> i32 { 0 }\n";

        let mut base = BTreeMap::new();
        base.insert(pa.to_path_buf(), base_a.as_bytes().to_vec());
        base.insert(pb.to_path_buf(), base_b.as_bytes().to_vec());
        base.insert(pc.to_path_buf(), base_c.as_bytes().to_vec());

        let move_a = "pub fn use_a() -> i32 { 0 }\n";
        let move_b = "fn helper() -> i32 { 1 }\npub fn use_b() -> i32 { 0 }\n";
        let pm_a = diff_source_to_patch(&make_ctx(pa, LanguageKind::Rust, "vp1", "a1", 1), base_a, move_a).unwrap();
        let pm_b = diff_source_to_patch(&make_ctx(pb, LanguageKind::Rust, "vp1", "a1", 2), base_b, move_b).unwrap();

        // 1. Caller in src/a.rs -> SemanticConflict
        let call_a = "pub fn helper() -> i32 { 1 }\npub fn use_a() -> i32 { helper() }\n";
        let pc_a = diff_source_to_patch(&make_ctx(pa, LanguageKind::Rust, "vp2", "a2", 3), base_a, call_a).unwrap();
        let rep_a = merge_patches(&base, &[pm_a.clone(), pm_b.clone(), pc_a.clone()], None).unwrap();
        assert!(
            matches!(rep_a.outcome, MergeOutcome::SemanticConflict { .. }),
            "Rust caller in origin a.rs must trigger SemanticConflict: {:?}",
            rep_a.outcome
        );
        let div_a = detect_divergent_base_conflicts(&base, &[pm_a.clone(), pm_b.clone()], &base, &[pc_a]).unwrap();
        assert!(
            div_a.iter().any(|c| matches!(c, MergeOutcome::SemanticConflict { symbol_fqn, .. } if symbol_fqn == "helper")),
            "Rust divergent_base caller in a.rs must trigger SemanticConflict: {div_a:?}"
        );

        // 2. Caller in src/b.rs -> Clean
        let call_b = "pub fn use_b() -> i32 { helper() }\n";
        let pc_b = diff_source_to_patch(&make_ctx(pb, LanguageKind::Rust, "vp2", "a2", 3), base_b, call_b).unwrap();
        let rep_b = merge_patches(&base, &[pm_a.clone(), pm_b.clone(), pc_b.clone()], None).unwrap();
        assert!(
            rep_b.is_clean(),
            "Rust caller in destination b.rs must merge Clean: {:?}",
            rep_b.conflicts
        );
        let div_b = detect_divergent_base_conflicts(&base, &[pm_a.clone(), pm_b.clone()], &base, &[pc_b]).unwrap();
        assert!(
            !div_b.iter().any(|c| matches!(c, MergeOutcome::SemanticConflict { .. })),
            "Rust divergent_base caller in destination b.rs must NOT emit SemanticConflict: {div_b:?}"
        );

        // 3. Caller in src/c.rs -> SemanticConflict
        let call_c = "pub fn use_c() -> i32 { helper() }\n";
        let pc_c = diff_source_to_patch(&make_ctx(pc, LanguageKind::Rust, "vp2", "a2", 3), base_c, call_c).unwrap();
        let rep_c = merge_patches(&base, &[pm_a.clone(), pm_b.clone(), pc_c.clone()], None).unwrap();
        assert!(
            matches!(rep_c.outcome, MergeOutcome::SemanticConflict { .. }),
            "Rust caller in third file c.rs must trigger SemanticConflict: {:?}",
            rep_c.outcome
        );
        let div_c = detect_divergent_base_conflicts(&base, &[pm_a, pm_b], &base, &[pc_c]).unwrap();
        assert!(
            div_c.iter().any(|c| matches!(c, MergeOutcome::SemanticConflict { symbol_fqn, .. } if symbol_fqn == "helper")),
            "Rust divergent_base caller in c.ts must trigger SemanticConflict: {div_c:?}"
        );
    }
}

#[test]
fn python_inline_origin_insertions_preserve_member_indent_across_all_actions() {
    let py_p = Path::new("inline_indent.py");

    let cases: &[(&str, &str, &str, &str, &str)] = &[
        // Case A: AfterDocstring (on_colon_row = true, single statement after docstring)
        (
            "after_docstring_lf",
            "class Doc: \"\"\"doc\"\"\"\n",
            "class Doc: \"\"\"doc\"\"\"; z = 1\n",
            "class Doc:\n    \"\"\"doc\"\"\"\n    def b(self): return 2\n",
            "    z = 1",
        ),
        // Case A2: AfterDocstring with CRLF and decorated multi-line def
        (
            "after_docstring_crlf_decorated",
            "class Doc: \"\"\"doc\"\"\"\r\n",
            "class Doc: \"\"\"doc\"\"\"; z = 1\r\n",
            "class Doc:\r\n    \"\"\"doc\"\"\"\r\n    @staticmethod\r\n    def b():\r\n        return 2\r\n",
            "    z = 1",
        ),
        // Case A3: AfterDocstring with inline trailing stmt (has_same_line_stmt = true)
        (
            "after_docstring_same_line_stmt",
            "class Doc: \"\"\"doc\"\"\"; x = 1\n",
            "class Doc: \"\"\"doc\"\"\"; z = 2; x = 1\n",
            "class Doc:\n    \"\"\"doc\"\"\"\n    def b(self):\n        return 2\n    x = 1\n",
            "    z = 2",
        ),
        // Case B: AfterColon with inline existing statements
        (
            "after_colon_lf",
            "class Two: x = 1; y = 2\n",
            "class Two: z = 3; x = 1; y = 2\n",
            "class Two:\n    def b(self): return 2\n    x = 1; y = 2\n",
            "    z = 3",
        ),
        // Case B2: AfterColon with CRLF and decorated multi-line def
        (
            "after_colon_crlf_decorated",
            "class Two: x = 1; y = 2\r\n",
            "class Two: z = 3; x = 1; y = 2\r\n",
            "class Two:\r\n    @staticmethod\r\n    def b():\r\n        return 2\r\n    x = 1; y = 2\r\n",
            "    z = 3",
        ),
        // Case C: ReplacePass with inline-origin statement
        (
            "replace_pass_inline_origin",
            "class Foo: pass\n",
            "class Foo: z = 1\n",
            "class Foo:\n    def b(self):\n        return 2\n",
            "    z = 1",
        ),
    ];

    for &(label, base, v1, v2, expected_indented) in cases {
        let p1_early = diff_source_to_patch(
            &make_ctx(py_p, LanguageKind::Python, "vp1", "a1", 1),
            base,
            v1,
        )
        .unwrap();
        let p2_late = diff_source_to_patch(
            &make_ctx(py_p, LanguageKind::Python, "vp2", "a2", 2),
            base,
            v2,
        )
        .unwrap();

        let p2_early = diff_source_to_patch(
            &make_ctx(py_p, LanguageKind::Python, "vp2", "a2", 1),
            base,
            v2,
        )
        .unwrap();
        let p1_late = diff_source_to_patch(
            &make_ctx(py_p, LanguageKind::Python, "vp1", "a1", 2),
            base,
            v1,
        )
        .unwrap();

        let mut fm = BTreeMap::new();
        fm.insert(py_p.to_path_buf(), base.as_bytes().to_vec());

        // Order 1: inline-origin first, indented second (both patch slice orders)
        let r12_a = merge_patches(&fm, &[p1_early.clone(), p2_late.clone()], None).unwrap();
        let r12_b = merge_patches(&fm, &[p2_late, p1_early], None).unwrap();
        assert!(r12_a.is_clean(), "{label} (inline first) failed: {:?}", r12_a.conflicts);
        assert_eq!(r12_a.text(py_p), r12_b.text(py_p), "{label} not commutative (inline first)");
        let m12 = r12_a.text(py_p).unwrap();
        assert!(
            m12.contains(expected_indented),
            "{label} missing indented inline member {expected_indented:?}:\n{m12}"
        );
        assert!(
            !m12.lines().any(|l| l == expected_indented.trim_start()),
            "{label} emitted unindented column-0 member:\n{m12}"
        );
        assert!(
            !parse_source(LanguageKind::Python, m12, None)
                .unwrap()
                .tree
                .root_node()
                .has_error(),
            "{label} (inline first) CST error:\n{m12}"
        );

        // Order 2: indented first, inline-origin second (both patch slice orders)
        let r21_a = merge_patches(&fm, &[p2_early.clone(), p1_late.clone()], None).unwrap();
        let r21_b = merge_patches(&fm, &[p1_late, p2_early], None).unwrap();
        assert!(r21_a.is_clean(), "{label} (indented first) failed: {:?}", r21_a.conflicts);
        assert_eq!(r21_a.text(py_p), r21_b.text(py_p), "{label} not commutative (indented first)");
        let m21 = r21_a.text(py_p).unwrap();
        assert!(
            m21.contains(expected_indented),
            "{label} (indented first) missing indented inline member:\n{m21}"
        );
        assert!(
            !parse_source(LanguageKind::Python, m21, None)
                .unwrap()
                .tree
                .root_node()
                .has_error(),
            "{label} (indented first) CST error:\n{m21}"
        );
    }
}

#[test]
fn python_semicolon_terminated_inline_stubs_concurrent_insert_members() {
    let py_p = Path::new("stub_semi.py");

    let bases: &[(&str, &str, bool)] = &[
        ("pass_semi_comment", "class Foo: pass; # stub\n", true),
        ("pass_semi", "class Foo: pass;\n", false),
        ("ellipsis_semi_comment", "class Foo: ...; # stub\n", true),
        ("ellipsis_semi", "class Foo: ...;\n", false),
        ("pass_ws_semi_ws", "class Foo: pass ;  \n", false),
        ("pass_semi_comment_crlf", "class Foo: pass; # stub\r\n", true),
        ("pass_semi_eof", "class Foo: pass;", false),
    ];

    let v_multi_a = "class Foo:\n    def a(self):\n        return 1\n";
    let v_multi_b = "class Foo:\n    def b(self):\n        return 2\n";
    let v_one_a = "class Foo:\n    def a(self): return 1\n";
    let v_one_b = "class Foo:\n    def b(self): return 2\n";

    for &(label, base, has_comment) in bases {
        for (variant, va, vb) in [("multiline", v_multi_a, v_multi_b), ("oneline", v_one_a, v_one_b)] {
            let pa = diff_source_to_patch(
                &make_ctx(py_p, LanguageKind::Python, "vp1", "a1", 1),
                base,
                va,
            )
            .unwrap();
            let pb = diff_source_to_patch(
                &make_ctx(py_p, LanguageKind::Python, "vp2", "a2", 2),
                base,
                vb,
            )
            .unwrap();

            let mut fm = BTreeMap::new();
            fm.insert(py_p.to_path_buf(), base.as_bytes().to_vec());

            let r12 = merge_patches(&fm, &[pa.clone(), pb.clone()], None).unwrap();
            let r21 = merge_patches(&fm, &[pb, pa], None).unwrap();
            assert!(r12.is_clean(), "{label}/{variant} failed: {:?}", r12.conflicts);
            assert_eq!(r12.text(py_p), r21.text(py_p), "{label}/{variant} not commutative");
            let merged = r12.text(py_p).unwrap();
            assert!(!merged.contains(';'), "{label}/{variant} leaked trailing semicolon:\n{merged}");
            if has_comment {
                assert!(
                    merged.contains("class Foo: # stub"),
                    "{label}/{variant} lost header comment:\n{merged}"
                );
            }
            assert!(
                merged.contains("def a(self)") && merged.contains("def b(self)"),
                "{label}/{variant} missing merged methods:\n{merged}"
            );
            assert!(
                !parse_source(LanguageKind::Python, merged, None)
                    .unwrap()
                    .tree
                    .root_node()
                    .has_error(),
                "{label}/{variant} CST error:\n{merged}"
            );
        }
    }
}

#[test]
fn go_added_single_decl_vs_decl_group_detects_structural_conflict() {
    let go_p = Path::new("p.go");

    // 1. const A = 1 in base -> concurrent `const A = 2` (p::A#added) vs `const ( A = 3; B = 4 )`
    let base_const = "package p\n\nconst A = 1\n";
    let v_const_single = "package p\n\nconst A = 1\n\nconst A = 2\n";
    let v_const_group = "package p\n\nconst A = 1\n\nconst (\n\tA = 3\n\tB = 4\n)\n";
    let v_const_group_disjoint = "package p\n\nconst A = 1\n\nconst (\n\tC = 3\n\tD = 4\n)\n";

    let pc_s = diff_source_to_patch(
        &make_ctx(go_p, LanguageKind::Go, "vp1", "a1", 1),
        base_const,
        v_const_single,
    )
    .unwrap();
    let pc_g = diff_source_to_patch(
        &make_ctx(go_p, LanguageKind::Go, "vp2", "a2", 2),
        base_const,
        v_const_group,
    )
    .unwrap();
    let pc_gd = diff_source_to_patch(
        &make_ctx(go_p, LanguageKind::Go, "vp3", "a3", 3),
        base_const,
        v_const_group_disjoint,
    )
    .unwrap();

    let mut fm_c = BTreeMap::new();
    fm_c.insert(go_p.to_path_buf(), base_const.as_bytes().to_vec());

    let rep_c12 = merge_patches(&fm_c, &[pc_s.clone(), pc_g.clone()], None).unwrap();
    let rep_c21 = merge_patches(&fm_c, &[pc_g, pc_s.clone()], None).unwrap();
    assert!(
        matches!(rep_c12.outcome, MergeOutcome::StructuralConflict { .. }),
        "Go const #added vs const_group must emit StructuralConflict: {:?}",
        rep_c12.outcome
    );
    assert!(
        matches!(rep_c21.outcome, MergeOutcome::StructuralConflict { .. }),
        "Go const_group vs const #added must emit StructuralConflict: {:?}",
        rep_c21.outcome
    );

    let rep_c_ok = merge_patches(&fm_c, &[pc_s, pc_gd], None).unwrap();
    assert!(
        rep_c_ok.is_clean(),
        "Go disjoint const #added vs const_group must merge Clean: {:?}",
        rep_c_ok.conflicts
    );

    // 2. var X = 1 in base -> concurrent `var X = 2` (p::X#added) vs `var ( X = 3; Y = 4 )`
    let base_var = "package p\n\nvar X = 1\n";
    let v_var_single = "package p\n\nvar X = 1\n\nvar X = 2\n";
    let v_var_group = "package p\n\nvar X = 1\n\nvar (\n\tX = 3\n\tY = 4\n)\n";
    let pv_s = diff_source_to_patch(
        &make_ctx(go_p, LanguageKind::Go, "vp1", "a1", 1),
        base_var,
        v_var_single,
    )
    .unwrap();
    let pv_g = diff_source_to_patch(
        &make_ctx(go_p, LanguageKind::Go, "vp2", "a2", 2),
        base_var,
        v_var_group,
    )
    .unwrap();
    let mut fm_v = BTreeMap::new();
    fm_v.insert(go_p.to_path_buf(), base_var.as_bytes().to_vec());
    let rep_v = merge_patches(&fm_v, &[pv_s, pv_g], None).unwrap();
    assert!(
        matches!(rep_v.outcome, MergeOutcome::StructuralConflict { .. }),
        "Go var #added vs var_group must emit StructuralConflict: {:?}",
        rep_v.outcome
    );

    // 3. type T int in base -> concurrent `type T string` (p::T#added) vs `type ( T bool; U int )`
    let base_type = "package p\n\ntype T int\n";
    let v_type_single = "package p\n\ntype T int\n\ntype T string\n";
    let v_type_group = "package p\n\ntype T int\n\ntype (\n\tT bool\n\tU int\n)\n";
    let pt_s = diff_source_to_patch(
        &make_ctx(go_p, LanguageKind::Go, "vp1", "a1", 1),
        base_type,
        v_type_single,
    )
    .unwrap();
    let pt_g = diff_source_to_patch(
        &make_ctx(go_p, LanguageKind::Go, "vp2", "a2", 2),
        base_type,
        v_type_group,
    )
    .unwrap();
    let mut fm_t = BTreeMap::new();
    fm_t.insert(go_p.to_path_buf(), base_type.as_bytes().to_vec());
    let rep_t = merge_patches(&fm_t, &[pt_s, pt_g], None).unwrap();
    assert!(
        matches!(rep_t.outcome, MergeOutcome::StructuralConflict { .. }),
        "Go type #added vs type_group must emit StructuralConflict: {:?}",
        rep_t.outcome
    );
}

fn assert_python_class_bodies_uniform_indent(src: &str, label: &str) {
    let parsed = parse_source(LanguageKind::Python, src, None).unwrap();
    assert!(
        !parsed.tree.root_node().has_error(),
        "{label}: Tree-sitter Python syntax error:\n{src}"
    );
    fn check_node(node: tree_sitter::Node<'_>, src: &str, label: &str) {
        if node.kind() == "class_definition"
            && let Some(body) = node.child_by_field_name("body")
        {
            let class_indent = node.start_position().column;
            let mut body_indents: Vec<usize> = Vec::new();
            let mut last_row: Option<usize> = None;
            let mut cursor = body.walk();
            for child in body.named_children(&mut cursor) {
                if child.kind() != "comment" {
                    let row = child.start_position().row;
                    let col = child.start_position().column;
                    if row > node.start_position().row && last_row != Some(row) {
                        last_row = Some(row);
                        assert!(
                            col > class_indent,
                            "{label}: class body statement dedented to col {col} <= class col {class_indent}:\n{src}"
                        );
                        body_indents.push(col);
                    }
                }
            }
            if let Some(&first_col) = body_indents.first() {
                for &col in &body_indents {
                    assert_eq!(
                        col, first_col,
                        "{label}: mixed class body indentation ({first_col} vs {col}):\n{src}"
                    );
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            check_node(child, src, label);
        }
    }
    check_node(parsed.tree.root_node(), src, label);
}

#[test]
fn python_custom_2sp_and_6sp_body_indent_vs_inline_origin_merges_cleanly_without_mixed_indent() {
    let py_p = Path::new("custom_indent_mixed.py");

    let cases: &[(&str, &str, &str, &str, &[&str])] = &[
        // 1. ReplacePass: inline `z = 1` vs 2-space `def b`
        (
            "custom_2sp_replace_pass",
            "class Foo: pass\n",
            "class Foo: z = 1\n",
            "class Foo:\n  def b(self):\n    return 2\n",
            &["  z = 1", "  def b(self):", "    return 2"],
        ),
        // 2. AfterDocstring: inline `z = 1` vs 2-space `def b`
        (
            "custom_2sp_after_docstring",
            "class Doc: \"\"\"doc\"\"\"\n",
            "class Doc: \"\"\"doc\"\"\"; z = 1\n",
            "class Doc:\n  \"\"\"doc\"\"\"\n  def b(self):\n    return 2\n",
            &["  \"\"\"doc\"\"\"", "  z = 1", "  def b(self):", "    return 2"],
        ),
        // 3. AfterColon: inline `z = 3` vs 2-space `def b`
        (
            "custom_2sp_after_colon",
            "class Two: x = 1; y = 2\n",
            "class Two: z = 3; x = 1; y = 2\n",
            "class Two:\n  def b(self):\n    return 2\n  x = 1; y = 2\n",
            &["  z = 3", "  def b(self):", "    return 2", "  x = 1; y = 2"],
        ),
        // 4. Nested Outer (4sp) -> Inner (6sp) vs inline `z = 1`
        (
            "nested_outer_4sp_inner_6sp_vs_inline",
            "class Outer:\n    class Inner: pass\n",
            "class Outer:\n    class Inner: z = 1\n",
            "class Outer:\n    class Inner:\n      def b(self):\n        return 2\n",
            &["      z = 1", "      def b(self):", "        return 2"],
        ),
        // 5. 2-space base reformatted inline (`class Two: z = 3; x = 1; y = 2`) vs 2-space `def b`
        (
            "custom_2sp_base_reformatted_inline",
            "class Two:\n  x = 1\n  y = 2\n",
            "class Two: z = 3; x = 1; y = 2\n",
            "class Two:\n  def b(self):\n    return 2\n  x = 1\n  y = 2\n",
            &["  z = 3", "  def b(self):", "    return 2", "  x = 1", "  y = 2"],
        ),
        // 6. Inline multi-line parenthesized expression vs 2-space `def b`
        (
            "custom_2sp_vs_inline_multiline_paren",
            "class Foo: pass\n",
            "class Foo: z = (\n    1 +\n    2\n)\n",
            "class Foo:\n  def b(self):\n    return 2\n",
            &["  z = (", "  def b(self):"],
        ),
    ];

    for &(label, base, v1, v2, expected_lines) in cases {
        let p1_early = diff_source_to_patch(
            &make_ctx(py_p, LanguageKind::Python, "vp1", "a1", 1),
            base,
            v1,
        )
        .unwrap();
        let p2_late = diff_source_to_patch(
            &make_ctx(py_p, LanguageKind::Python, "vp2", "a2", 2),
            base,
            v2,
        )
        .unwrap();

        let p2_early = diff_source_to_patch(
            &make_ctx(py_p, LanguageKind::Python, "vp2", "a2", 1),
            base,
            v2,
        )
        .unwrap();
        let p1_late = diff_source_to_patch(
            &make_ctx(py_p, LanguageKind::Python, "vp1", "a1", 2),
            base,
            v1,
        )
        .unwrap();

        let mut fm = BTreeMap::new();
        fm.insert(py_p.to_path_buf(), base.as_bytes().to_vec());

        // Single-patch apply of p1_early must also keep uniform indentation
        let r_single = merge_patches(&fm, std::slice::from_ref(&p1_early), None).unwrap();
        assert!(
            r_single.is_clean(),
            "{label} single p1 failed: {:?}",
            r_single.conflicts
        );
        assert_python_class_bodies_uniform_indent(
            r_single.text(py_p).unwrap(),
            &format!("{label}/single_p1"),
        );

        // Normal timestamps (ts1 < ts2), both patch slice orders
        let r12_a = merge_patches(&fm, &[p1_early.clone(), p2_late.clone()], None).unwrap();
        let r12_b = merge_patches(&fm, &[p2_late, p1_early], None).unwrap();
        assert!(r12_a.is_clean(), "{label} normal_ts failed: {:?}", r12_a.conflicts);
        assert_eq!(
            r12_a.text(py_p),
            r12_b.text(py_p),
            "{label} normal_ts not commutative"
        );
        let m12 = r12_a.text(py_p).unwrap();
        assert_python_class_bodies_uniform_indent(m12, &format!("{label}/normal_ts"));
        for exp in expected_lines {
            assert!(
                m12.lines().any(|l| l == *exp),
                "{label} normal_ts missing line {exp:?}:\n{m12}"
            );
        }

        // Reversed timestamps (ts2 < ts1), both patch slice orders
        let r21_a = merge_patches(&fm, &[p2_early.clone(), p1_late.clone()], None).unwrap();
        let r21_b = merge_patches(&fm, &[p1_late, p2_early], None).unwrap();
        assert!(
            r21_a.is_clean(),
            "{label} reversed_ts failed: {:?}",
            r21_a.conflicts
        );
        assert_eq!(
            r21_a.text(py_p),
            r21_b.text(py_p),
            "{label} reversed_ts not commutative"
        );
        let m21 = r21_a.text(py_p).unwrap();
        assert_python_class_bodies_uniform_indent(m21, &format!("{label}/reversed_ts"));
        for exp in expected_lines {
            assert!(
                m21.lines().any(|l| l == *exp),
                "{label} reversed_ts missing line {exp:?}:\n{m21}"
            );
        }
    }

    // Verify Case 5 does not emit destructive MutateSignature ops on whitespace-only inline shift
    let base_5 = "class Two:\n  x = 1\n  y = 2\n";
    let v1_5 = "class Two: z = 3; x = 1; y = 2\n";
    let p_5 = diff_source_to_patch(
        &make_ctx(py_p, LanguageKind::Python, "vp1", "a1", 1),
        base_5,
        v1_5,
    )
    .unwrap();
    assert!(
        !p_5.ops
            .iter()
            .any(|op| matches!(op, SemanticOp::MutateSignature { .. })),
        "whitespace-only indented->inline reformat must not emit MutateSignature: {:?}",
        p_5.ops
    );
}

#[test]
fn python_conflicting_explicit_indents_and_genuine_inline_mutate_signature_normalize_cleanly() {
    let py_p = Path::new("indent_normalization.py");

    // 1. Conflicting explicit body indents on inline base (4-space vs 2-space) -> deterministic & uniform
    let base_inline = "class Foo: pass\n";
    let v_4sp = "class Foo:\n    def a(self):\n        return 1\n";
    let v_2sp = "class Foo:\n  def b(self):\n    return 2\n";
    let p4_1 = diff_source_to_patch(&make_ctx(py_p, LanguageKind::Python, "vp1", "a1", 1), base_inline, v_4sp).unwrap();
    let p2_2 = diff_source_to_patch(&make_ctx(py_p, LanguageKind::Python, "vp2", "a2", 2), base_inline, v_2sp).unwrap();
    let p2_1 = diff_source_to_patch(&make_ctx(py_p, LanguageKind::Python, "vp2", "a2", 1), base_inline, v_2sp).unwrap();
    let p4_2 = diff_source_to_patch(&make_ctx(py_p, LanguageKind::Python, "vp1", "a1", 2), base_inline, v_4sp).unwrap();
    let mut fm_inline = BTreeMap::new();
    fm_inline.insert(py_p.to_path_buf(), base_inline.as_bytes().to_vec());

    let r_4first_a = merge_patches(&fm_inline, &[p4_1.clone(), p2_2.clone()], None).unwrap();
    let r_4first_b = merge_patches(&fm_inline, &[p2_2, p4_1], None).unwrap();
    assert!(r_4first_a.is_clean(), "4sp vs 2sp failed: {:?}", r_4first_a.conflicts);
    assert_eq!(r_4first_a.text(py_p), r_4first_b.text(py_p));
    assert_python_class_bodies_uniform_indent(r_4first_a.text(py_p).unwrap(), "conflicting_4sp_first");

    let r_2first_a = merge_patches(&fm_inline, &[p2_1.clone(), p4_2.clone()], None).unwrap();
    let r_2first_b = merge_patches(&fm_inline, &[p4_2, p2_1], None).unwrap();
    assert!(r_2first_a.is_clean(), "2sp vs 4sp failed: {:?}", r_2first_a.conflicts);
    assert_eq!(r_2first_a.text(py_p), r_2first_b.text(py_p));
    assert_python_class_bodies_uniform_indent(r_2first_a.text(py_p).unwrap(), "conflicting_2sp_first");

    // 2. Established 2-space body in base + incoming 4-space decorated method with nested block & col-0 docstring line
    let base_2sp = "class Bar:\n  x = 1\n";
    let v_4sp_complex = "\
class Bar:
    x = 1
    @staticmethod
    def helper(n):
        \"\"\"summary
col0 continuation line
        \"\"\"
        if n > 0:
            return n
        return 0
";
    let p_complex = diff_source_to_patch(
        &make_ctx(py_p, LanguageKind::Python, "vp1", "a1", 1),
        base_2sp,
        v_4sp_complex,
    )
    .unwrap();
    let mut fm_2sp = BTreeMap::new();
    fm_2sp.insert(py_p.to_path_buf(), base_2sp.as_bytes().to_vec());
    let r_complex = merge_patches(&fm_2sp, &[p_complex], None).unwrap();
    assert!(r_complex.is_clean(), "2sp base + 4sp complex failed: {:?}", r_complex.conflicts);
    let m_complex = r_complex.text(py_p).unwrap();
    assert_python_class_bodies_uniform_indent(m_complex, "2sp_base_vs_4sp_complex");
    assert!(
        m_complex.contains("\ncol0 continuation line\n"),
        "column-0 docstring continuation line must remain verbatim:\n{m_complex}"
    );
    assert!(m_complex.lines().any(|l| l == "  @staticmethod"));
    assert!(m_complex.lines().any(|l| l == "  def helper(n):"));

    // 3. Genuine MutateSignature (type annotation changed) while reformatting inline preserves original indent
    let base_sig = "class Two:\n  x: int = 1\n  y = 2\n";
    let v1_sig = "class Two: z = 3; x: str = 'a'; y = 2\n";
    let v2_sig = "class Two:\n  x: int = 1\n  def b(self):\n    return 2\n  y = 2\n";
    let ps1 = diff_source_to_patch(&make_ctx(py_p, LanguageKind::Python, "vp1", "a1", 1), base_sig, v1_sig).unwrap();
    let ps2 = diff_source_to_patch(&make_ctx(py_p, LanguageKind::Python, "vp2", "a2", 2), base_sig, v2_sig).unwrap();
    assert!(
        ps1.ops.iter().any(|op| matches!(op, SemanticOp::MutateSignature { .. })),
        "genuine signature change while moving inline must still emit MutateSignature: {:?}",
        ps1.ops
    );
    let mut fm_sig = BTreeMap::new();
    fm_sig.insert(py_p.to_path_buf(), base_sig.as_bytes().to_vec());
    let r_sig = merge_patches(&fm_sig, &[ps1, ps2], None).unwrap();
    assert!(r_sig.is_clean(), "genuine MutateSignature inline merge failed: {:?}", r_sig.conflicts);
    let m_sig = r_sig.text(py_p).unwrap();
    assert_python_class_bodies_uniform_indent(m_sig, "genuine_inline_mutate_signature");
    assert!(m_sig.lines().any(|l| l == "  x: str = 'a'"), "expected 2sp mutated x:\n{m_sig}");

    // 4. Non-Python (Rust & TS) genuine visibility/signature edits inside containers still emit MutateSignature
    let rs_p = Path::new("sig_check.rs");
    let rs_base = "pub struct S;\nimpl S {\n    pub fn f(&self, a: i32) -> i32 { a }\n}\n";
    let rs_vis = "pub struct S;\nimpl S {\n    fn f(&self, a: i32) -> i32 { a }\n}\n";
    let rs_patch = diff_source_to_patch(&make_ctx(rs_p, LanguageKind::Rust, "vp1", "a1", 1), rs_base, rs_vis).unwrap();
    assert!(
        rs_patch.ops.iter().any(|op| matches!(op, SemanticOp::MutateSignature { .. })),
        "Rust visibility change inside impl must still emit MutateSignature: {:?}",
        rs_patch.ops
    );
}

fn assert_python_class_bodies_uniform_indent_with_style(src: &str, label: &str) {
    assert_python_class_bodies_uniform_indent(src, label);
    let parsed = parse_source(LanguageKind::Python, src, None).unwrap();
    fn check_blocks(node: tree_sitter::Node<'_>, src: &str, label: &str) {
        if matches!(node.kind(), "class_definition" | "function_definition")
            && let Some(body) = node.child_by_field_name("body")
        {
            let hdr_row = node.start_position().row;
            let hdr_ws_start = src[..node.start_byte()]
                .rfind(['\n', '\r'])
                .map_or(0, |p| p + 1);
            let hdr_ws = &src[hdr_ws_start..node.start_byte()];
            let mut cursor = body.walk();
            for child in body.named_children(&mut cursor) {
                if child.kind() != "comment" && child.start_position().row > hdr_row {
                    let line_start = src[..child.start_byte()]
                        .rfind(['\n', '\r'])
                        .map_or(0, |p| p + 1);
                    let child_ws = &src[line_start..child.start_byte()];
                    assert!(
                        child_ws.len() > hdr_ws.len() && child_ws.starts_with(hdr_ws),
                        "{label}: child indent {child_ws:?} does not strictly extend header indent {hdr_ws:?}:\n{src}"
                    );
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            check_blocks(child, src, label);
        }
    }
    check_blocks(parsed.tree.root_node(), src, label);
}

fn run_single_and_two_way_py_check(
    py_p: &Path,
    label: &str,
    base: &str,
    v1: &str,
    v2: &str,
    expected_lines: &[&str],
) -> String {
    let p1_early = diff_source_to_patch(
        &make_ctx(py_p, LanguageKind::Python, "vp1", "a1", 1),
        base,
        v1,
    )
    .unwrap();
    let p2_late = diff_source_to_patch(
        &make_ctx(py_p, LanguageKind::Python, "vp2", "a2", 2),
        base,
        v2,
    )
    .unwrap();
    let p2_early = diff_source_to_patch(
        &make_ctx(py_p, LanguageKind::Python, "vp2", "a2", 1),
        base,
        v2,
    )
    .unwrap();
    let p1_late = diff_source_to_patch(
        &make_ctx(py_p, LanguageKind::Python, "vp1", "a1", 2),
        base,
        v1,
    )
    .unwrap();

    let mut fm = BTreeMap::new();
    fm.insert(py_p.to_path_buf(), base.as_bytes().to_vec());

    let r_single = merge_patches(&fm, std::slice::from_ref(&p1_early), None).unwrap();
    assert!(
        r_single.is_clean(),
        "{label} single p1 failed: {:?}",
        r_single.conflicts
    );
    assert_python_class_bodies_uniform_indent_with_style(
        r_single.text(py_p).unwrap(),
        &format!("{label}/single_p1"),
    );

    let r_single_p2 = merge_patches(&fm, std::slice::from_ref(&p2_early), None).unwrap();
    assert!(
        r_single_p2.is_clean(),
        "{label} single p2 failed: {:?}",
        r_single_p2.conflicts
    );
    assert_python_class_bodies_uniform_indent_with_style(
        r_single_p2.text(py_p).unwrap(),
        &format!("{label}/single_p2"),
    );

    let r12_a = merge_patches(&fm, &[p1_early.clone(), p2_late.clone()], None).unwrap();
    let r12_b = merge_patches(&fm, &[p2_late, p1_early], None).unwrap();
    assert!(r12_a.is_clean(), "{label} normal_ts failed: {:?}", r12_a.conflicts);
    assert_eq!(
        r12_a.text(py_p),
        r12_b.text(py_p),
        "{label} normal_ts not commutative"
    );
    let m12 = r12_a.text(py_p).unwrap();
    assert_python_class_bodies_uniform_indent_with_style(m12, &format!("{label}/normal_ts"));
    for exp in expected_lines {
        assert!(
            m12.lines().any(|l| l == *exp),
            "{label} normal_ts missing line {exp:?}:\n{m12}"
        );
    }

    let r21_a = merge_patches(&fm, &[p2_early.clone(), p1_late.clone()], None).unwrap();
    let r21_b = merge_patches(&fm, &[p1_late, p2_early], None).unwrap();
    assert!(
        r21_a.is_clean(),
        "{label} reversed_ts failed: {:?}",
        r21_a.conflicts
    );
    assert_eq!(
        r21_a.text(py_p),
        r21_b.text(py_p),
        "{label} reversed_ts not commutative"
    );
    let m21 = r21_a.text(py_p).unwrap();
    assert_python_class_bodies_uniform_indent_with_style(m21, &format!("{label}/reversed_ts"));
    for exp in expected_lines {
        assert!(
            m21.lines().any(|l| l == *exp),
            "{label} reversed_ts missing line {exp:?}:\n{m21}"
        );
    }

    m12.to_string()
}

#[test]
fn python_cross_indent_mutate_signature_normalizes_cleanly() {
    let py_p = Path::new("cross_indent_mut_sig.py");

    let cases: &[(&str, &str, &str, &str, &[&str])] = &[
        // 1. 4sp -> 2sp attribute MutateSignature with unchanged sibling y
        (
            "4sp_to_2sp_attr_mut_sig",
            "class Two:\n    x: int = 1\n    y = 2\n",
            "class Two:\n  x: str = 'a'\n  y = 2\n",
            "class Two:\n    x: int = 1\n    def b(self):\n        return 2\n    y = 2\n",
            &["    x: str = 'a'", "    def b(self):", "    y = 2"],
        ),
        // 2. 2sp -> 4sp attribute MutateSignature with unchanged sibling y
        (
            "2sp_to_4sp_attr_mut_sig",
            "class Two:\n  x: int = 1\n  y = 2\n",
            "class Two:\n    x: str = 'a'\n    y = 2\n",
            "class Two:\n  x: int = 1\n  def b(self):\n    return 2\n  y = 2\n",
            &["  x: str = 'a'", "  def b(self):", "  y = 2"],
        ),
        // 3. tab -> 4sp attribute MutateSignature with unchanged sibling y
        (
            "tab_to_4sp_attr_mut_sig",
            "class Two:\n\tx: int = 1\n\ty = 2\n",
            "class Two:\n    x: str = 'a'\n    y = 2\n",
            "class Two:\n\tx: int = 1\n\tdef b(self):\n\t\treturn 2\n\ty = 2\n",
            &["\tx: str = 'a'", "\tdef b(self):", "\ty = 2"],
        ),
        // 4. 4sp -> tab attribute MutateSignature with unchanged sibling y
        (
            "4sp_to_tab_attr_mut_sig",
            "class Two:\n    x: int = 1\n    y = 2\n",
            "class Two:\n\tx: str = 'a'\n\ty = 2\n",
            "class Two:\n    x: int = 1\n    def b(self):\n        return 2\n    y = 2\n",
            &["    x: str = 'a'", "    def b(self):", "    y = 2"],
        ),
        // 5. 2sp -> 4sp decorated @staticmethod -> @classmethod + signature + body
        (
            "2sp_to_4sp_decorated_mut_sig",
            "class Foo:\n  x = 1\n  @staticmethod\n  def f(a: int) -> int:\n    return a\n",
            "class Foo:\n    x = 1\n    @classmethod\n    def f(cls, a: str) -> str:\n        return a\n",
            "class Foo:\n  x = 1\n  @staticmethod\n  def f(a: int) -> int:\n    return a\n  def g(self):\n    return 2\n",
            &[
                "  x = 1",
                "  @classmethod",
                "  def f(cls, a: str) -> str:",
                "    return a",
                "  def g(self):",
            ],
        ),
        // 6. 4sp -> 2sp decorated @staticmethod -> @classmethod + signature + body
        (
            "4sp_to_2sp_decorated_mut_sig",
            "class Foo:\n    x = 1\n    @staticmethod\n    def f(a: int) -> int:\n        return a\n",
            "class Foo:\n  x = 1\n  @classmethod\n  def f(cls, a: str) -> str:\n    return a\n",
            "class Foo:\n    x = 1\n    @staticmethod\n    def f(a: int) -> int:\n        return a\n    def g(self):\n        return 2\n",
            &[
                "    x = 1",
                "    @classmethod",
                "    def f(cls, a: str) -> str:",
                "        return a",
                "    def g(self):",
            ],
        ),
        // 7. 4sp -> 2sp multi-line parenthesized method signature
        (
            "4sp_to_2sp_multiline_sig",
            "class Foo:\n    x = 1\n    def f(\n        self,\n        a: int,\n    ) -> int:\n        return a\n",
            "class Foo:\n  x = 1\n  def f(\n    self,\n    a: str,\n  ) -> str:\n    return a\n",
            "class Foo:\n    x = 1\n    def f(\n        self,\n        a: int,\n    ) -> int:\n        return a\n    def g(self):\n        return 2\n",
            &[
                "    x = 1",
                "    def f(",
                "    ) -> str:",
                "        return a",
                "    def g(self):",
            ],
        ),
        // 8. Col-0 leading comment before indented member during 4sp -> 2sp MutateSignature
        (
            "4sp_to_2sp_col0_comment_mut_sig",
            "class Foo:\n# note\n    x: int = 1\n    y = 2\n",
            "class Foo:\n# note\n  x: str = 'a'\n  y = 2\n",
            "class Foo:\n# note\n    x: int = 1\n    def b(self):\n        return 2\n    y = 2\n",
            &["    x: str = 'a'", "    def b(self):", "    y = 2"],
        ),
    ];

    for &(label, base, v1, v2, expected) in cases {
        run_single_and_two_way_py_check(py_p, label, base, v1, v2, expected);
    }
}

#[test]
fn python_cross_indent_replace_body_normalizes_cleanly() {
    let py_p = Path::new("cross_indent_replace_body.py");

    let cases: &[(&str, &str, &str, &str, &[&str])] = &[
        // 1. 4sp -> 2sp ReplaceBody only (adv8)
        (
            "4sp_to_2sp_replace_body",
            "class Foo:\n    def f(self):\n        return 1\n    y = 2\n",
            "class Foo:\n  def f(self):\n    return 2\n  y = 2\n",
            "class Foo:\n    def f(self):\n        return 1\n    def g(self):\n        return 3\n    y = 2\n",
            &["    def f(self):", "        return 2", "    def g(self):", "        return 3", "    y = 2"],
        ),
        // 2. 2sp -> 4sp ReplaceBody only (adv8b)
        (
            "2sp_to_4sp_replace_body",
            "class Foo:\n  def f(self):\n    return 1\n  y = 2\n",
            "class Foo:\n    def f(self):\n        return 2\n    y = 2\n",
            "class Foo:\n  def f(self):\n    return 1\n  def g(self):\n    return 3\n  y = 2\n",
            &["  def f(self):", "    return 2", "  def g(self):", "    return 3", "  y = 2"],
        ),
        // 3. tab -> 4sp ReplaceBody only (adv8c)
        (
            "tab_to_4sp_replace_body",
            "class Foo:\n\tdef f(self):\n\t\treturn 1\n\ty = 2\n",
            "class Foo:\n    def f(self):\n        return 2\n    y = 2\n",
            "class Foo:\n\tdef f(self):\n\t\treturn 1\n\tdef g(self):\n\t\treturn 3\n\ty = 2\n",
            &["\tdef f(self):", "\t\treturn 2", "\tdef g(self):", "\t\treturn 3", "\ty = 2"],
        ),
        // 4. 4sp -> tab ReplaceBody only
        (
            "4sp_to_tab_replace_body",
            "class Foo:\n    def f(self):\n        return 1\n    y = 2\n",
            "class Foo:\n\tdef f(self):\n\t\treturn 2\n\ty = 2\n",
            "class Foo:\n    def f(self):\n        return 1\n    def g(self):\n        return 3\n    y = 2\n",
            &["    def f(self):", "        return 2", "    def g(self):", "        return 3", "    y = 2"],
        ),
        // 5. Combined MutateSignature + ReplaceBody (adv2b 4sp -> 2sp)
        (
            "4sp_to_2sp_mut_sig_and_replace_body",
            "class Foo:\n    x = 1\n    def f(self, a: int) -> int:\n        return a\n",
            "class Foo:\n  x = 1\n  def f(self, a: str) -> str:\n    return a\n",
            "class Foo:\n    x = 1\n    def f(self, a: int) -> int:\n        return a\n    def g(self):\n        return 2\n",
            &["    x = 1", "    def f(self, a: str) -> str:", "        return a", "    def g(self):"],
        ),
        // 6. Nested if + col-0 multi-line docstring inside 4sp -> 2sp ReplaceBody
        (
            "4sp_to_2sp_nested_if_and_multiline_string",
            "class Foo:\n    def f(self, n):\n        return n\n",
            "class Foo:\n  def f(self, n):\n    \"\"\"summary\ncol0 doc line\n    \"\"\"\n    if n > 0:\n      return n\n    return 0\n",
            "class Foo:\n    def f(self, n):\n        return n\n    def g(self):\n        return 9\n",
            &[
                "    def f(self, n):",
                "        \"\"\"summary",
                "col0 doc line",
                "        if n > 0:",
                "          return n",
                "        return 0",
                "    def g(self):",
            ],
        ),
        // 7. Nested if + col-0 multi-line docstring inside 4sp -> tab ReplaceBody
        (
            "4sp_to_tab_nested_if_and_multiline_string",
            "class Foo:\n    def f(self, n):\n        return n\n",
            "class Foo:\n\tdef f(self, n):\n\t\t\"\"\"summary\ncol0 doc line\n\t\t\"\"\"\n\t\tif n > 0:\n\t\t\treturn n\n\t\treturn 0\n",
            "class Foo:\n    def f(self, n):\n        return n\n    def g(self):\n        return 9\n",
            &[
                "    def f(self, n):",
                "        \"\"\"summary",
                "col0 doc line",
                "        if n > 0:",
                "            return n",
                "        return 0",
                "    def g(self):",
            ],
        ),
        // 8. Single-line inline method body -> multi-line 4sp in 2sp class
        (
            "2sp_single_line_to_4sp_multiline_body",
            "class Foo:\n  def f(self): return 1\n  y = 2\n",
            "class Foo:\n    def f(self):\n        return 2\n    y = 2\n",
            "class Foo:\n  def f(self): return 1\n  def g(self):\n    return 3\n  y = 2\n",
            &["  def f(self):", "    return 2", "  def g(self):", "    return 3", "  y = 2"],
        ),
    ];

    for &(label, base, v1, v2, expected) in cases {
        let merged = run_single_and_two_way_py_check(py_p, label, base, v1, v2, expected);
        // Verify Phase C did not misplace `def g` before `return 2` / `return a` / `return 0`
        if let (Some(f_pos), Some(g_pos)) = (merged.find("def f("), merged.find("def g(")) {
            assert!(
                f_pos < g_pos,
                "{label}: expected def f before def g:\n{merged}"
            );
            let between = &merged[f_pos..g_pos];
            assert!(
                between.contains("return "),
                "{label}: def g was spliced inside def f before its return:\n{merged}"
            );
        }
    }

    // 9. CRLF 4sp -> 2sp ReplaceBody preserves CRLF and uniform indentation
    let base_crlf = "class Foo:\r\n    def f(self):\r\n        return 1\r\n";
    let v1_crlf = "class Foo:\r\n  def f(self):\r\n    return 2\r\n";
    let v2_crlf = "class Foo:\r\n    def f(self):\r\n        return 1\r\n    def g(self):\r\n        return 3\r\n";
    let merged_crlf = run_single_and_two_way_py_check(
        py_p,
        "crlf_4sp_to_2sp_replace_body",
        base_crlf,
        v1_crlf,
        v2_crlf,
        &["    def f(self):", "        return 2", "    def g(self):", "        return 3"],
    );
    assert!(
        merged_crlf.contains("\r\n"),
        "expected CRLF line endings preserved:\n{merged_crlf:?}"
    );
    assert!(
        !merged_crlf
            .as_bytes()
            .windows(2)
            .any(|w| w[0] != b'\r' && w[1] == b'\n'),
        "unexpected bare LF in CRLF merge:\n{merged_crlf:?}"
    );
}

#[test]
fn python_inline_container_mutate_signature_and_replace_body_edge_cases() {
    let py_p = Path::new("inline_mut_sig_and_body.py");

    let cases: &[(&str, &str, &str, &str, &[&str])] = &[
        // 1. reviewer_i16c_2 Finding 1: second inline member mutated with leading # comment + concurrent InsertMember
        (
            "inline_second_attr_reformat_with_comment",
            "class Foo: x: int = 1; y: int = 2\n",
            "class Foo:\n    x: int = 1\n    # comment on y\n    y: str = '2'\n",
            "class Foo:\n    x: int = 1\n    y: int = 2\n    z: int = 3\n",
            &["    x: int = 1", "    # comment on y", "    y: str = '2'", "    z: int = 3"],
        ),
        // 2. reviewer_i16c_2 Finding 1: first inline member mutated (`# comment on x` precedes block in tree-sitter-python) vs second inline member mutated with # comment
        (
            "inline_first_attr_reformat_with_comment_vs_mutate_second_attr",
            "class Foo: x: int = 1; y: int = 2\n",
            "class Foo:\n    # comment on x\n    x: str = '1'\n    y: int = 2\n",
            "class Foo:\n    x: int = 1\n    # comment on y\n    y: str = '2'\n",
            &[
                "    x: str = '1'",
                "    # comment on y",
                "    y: str = '2'",
            ],
        ),
        // 2b. 3-member inline container where non-first members carry # comments in MutateSignature
        (
            "inline_three_attrs_middle_and_last_reformat_with_comments",
            "class Foo: a: int = 0; x: int = 1; y: int = 2\n",
            "class Foo:\n    a: int = 0\n    # comment on x\n    x: str = '1'\n    y: int = 2\n",
            "class Foo:\n    a: int = 0\n    x: int = 1\n    # comment on y\n    y: str = '2'\n",
            &[
                "    a: int = 0",
                "    # comment on x",
                "    x: str = '1'",
                "    # comment on y",
                "    y: str = '2'",
            ],
        ),
        // 3. Block-row semicolon-separated members (`class Foo:\n    a = 0\n    x = 1; y = 2`) with comments on both
        (
            "inline_block_row_semicolon_attr_comments",
            "class Foo:\n    a: int = 0\n    x: int = 1; y: int = 2\n",
            "class Foo:\n    a: int = 0\n    x: int = 1\n    # comment on y\n    y: str = '2'\n",
            "class Foo:\n    a: int = 0\n    # comment on x\n    x: str = '1'\n    y: int = 2\n",
            &[
                "    a: int = 0",
                "    # comment on x",
                "    x: str = '1'",
                "    # comment on y",
                "    y: str = '2'",
            ],
        ),
        // 4. Colon-row comment before block (`class Foo: # orig\n    x: int = 1`) removed & reindented 2sp
        (
            "inline_colon_row_comment_before_block_removed",
            "class Foo: # orig\n    x: int = 1\n    y: int = 2\n",
            "class Foo:\n  x: str = '1'\n  y: int = 2\n",
            "class Foo: # orig\n    x: int = 1\n    # c2\n    y: str = '2'\n",
            &["    x: str = '1'", "    # c2", "    y: str = '2'"],
        ),
        // 5. Multi-line parenthesized expression mutation on first inline member vs comment on second
        (
            "inline_first_attr_multiline_paren_vs_second_comment",
            "class Foo: x: int = 1; y: int = 2\n",
            "class Foo:\n    x: str = (\n        '1'\n    )\n    y: int = 2\n",
            "class Foo:\n    x: int = 1\n    # comment on y\n    y: str = '2'\n",
            &[
                "    x: str = (",
                "        '1'",
                "    )",
                "    # comment on y",
                "    y: str = '2'",
            ],
        ),
        // 6. Nested class inline container (`Outer` 4sp -> `Inner` 8sp) with comments on inline attrs
        (
            "nested_inline_inner_class_attr_comments",
            "class Outer:\n    class Inner: a: int = 0; x: int = 1; y: int = 2\n",
            "class Outer:\n    class Inner:\n        a: int = 0\n        x: int = 1\n        # c on y\n        y: str = '2'\n",
            "class Outer:\n    class Inner:\n        a: int = 0\n        # c on x\n        x: str = '1'\n        y: int = 2\n",
            &[
                "        a: int = 0",
                "        # c on x",
                "        x: str = '1'",
                "        # c on y",
                "        y: str = '2'",
            ],
        ),
        // 7. Inline-body methods (`def f(self): return 1`, `def g(self): return 2`) with @decorator + ReplaceBody vs # comment + ReplaceBody
        (
            "inline_class_methods_decorator_first_and_comment_second",
            "class Foo:\n    def f(self): return 1\n    def g(self): return 2\n",
            "class Foo:\n    @staticmethod\n    def f():\n        return 10\n    def g(self): return 2\n",
            "class Foo:\n    def f(self): return 1\n    # comment on g\n    def g(self, n: int = 2):\n        return n\n",
            &[
                "    @staticmethod",
                "    def f():",
                "        return 10",
                "    # comment on g",
                "    def g(self, n: int = 2):",
                "        return n",
            ],
        ),
        // 8. Inline-body methods with @decorator on second member + concurrent InsertMember
        (
            "inline_class_methods_decorator_second_vs_insert_member",
            "class Foo:\n    def f(self): return 1\n    def g(self): return 2\n",
            "class Foo:\n    def f(self): return 1\n    @classmethod\n    def g(cls):\n        return 20\n",
            "class Foo:\n    def f(self): return 1\n    def g(self): return 2\n    def h(self):\n        return 30\n",
            &[
                "    def f(self): return 1",
                "    @classmethod",
                "    def g(cls):",
                "        return 20",
                "    def h(self):",
                "        return 30",
            ],
        ),
        // 9. Inline semicolon-separated class with comment MutateSignature vs decorated InsertMember
        (
            "inline_semicolon_attr_comment_vs_decorated_insert_member",
            "class Foo: x: int = 1; y: int = 2\n",
            "class Foo:\n    x: int = 1\n    # comment on y\n    y: str = '2'\n",
            "class Foo:\n    x: int = 1\n    y: int = 2\n    @staticmethod\n    def h():\n        return 3\n",
            &[
                "    x: int = 1",
                "    # comment on y",
                "    y: str = '2'",
                "    @staticmethod",
                "    def h():",
                "        return 3",
            ],
        ),
        // 9b. Inline semicolon-separated function body (`def f(): a = 1; b = 2`) MutateSignature vs ReplaceBody
        (
            "inline_semicolon_func_body_mutate_sig_vs_replace_body",
            "def f(): a = 1; b = 2\n",
            "def f(\n    x: int = 1,\n): a = 1; b = 2\n",
            "def f():\n    a = 10\n    b = 20\n",
            &[
                "def f(",
                "    x: int = 1,",
                "):",
                "    a = 10",
                "    b = 20",
            ],
        ),
        // 10. Inline first attr comment mutation vs RemoveNode on second attr
        (
            "inline_second_attr_comment_vs_remove_first_attr",
            "class Foo: x: int = 1; y: int = 2\n",
            "class Foo:\n    x: int = 1\n    # comment on y\n    y: str = '2'\n",
            "class Foo: y: int = 2\n",
            &["    # comment on y", "    y: str = '2'"],
        ),
        // 11. No-EOF-newline inline container with comment mutations
        (
            "inline_no_eof_newline_attr_comments",
            "class Foo: a: int = 0; x: int = 1; y: int = 2",
            "class Foo:\n    a: int = 0\n    # cx\n    x: str = '1'\n    y: int = 2",
            "class Foo:\n    a: int = 0\n    x: int = 1\n    # cy\n    y: str = '2'",
            &["    a: int = 0", "    # cx", "    x: str = '1'", "    # cy", "    y: str = '2'"],
        ),
    ];

    for &(label, base, v1, v2, expected) in cases {
        diff_source_to_patch_strict(&make_ctx(py_p, LanguageKind::Python, "vp1", "a1", 1), base, v1)
            .unwrap_or_else(|e| panic!("{label} v1 strict diff failed: {e}"));
        diff_source_to_patch_strict(&make_ctx(py_p, LanguageKind::Python, "vp2", "a2", 2), base, v2)
            .unwrap_or_else(|e| panic!("{label} v2 strict diff failed: {e}"));
        let merged = run_single_and_two_way_py_check(py_p, label, base, v1, v2, expected);
        if label == "inline_second_attr_comment_vs_remove_first_attr" {
            assert!(
                !merged.contains("x: int = 1"),
                "{label}: removed attr x leaked into merged output:\n{merged}"
            );
        }
    }

    // 12. CRLF inline container with comment mutations preserves CRLF
    let base_crlf = "class Foo: a: int = 0; x: int = 1; y: int = 2\r\n";
    let v1_crlf = "class Foo:\r\n    a: int = 0\r\n    x: int = 1\r\n    # cy\r\n    y: str = '2'\r\n";
    let v2_crlf = "class Foo:\r\n    a: int = 0\r\n    # cx\r\n    x: str = '1'\r\n    y: int = 2\r\n";
    let merged_crlf = run_single_and_two_way_py_check(
        py_p,
        "inline_crlf_attr_comments",
        base_crlf,
        v1_crlf,
        v2_crlf,
        &["    a: int = 0", "    # cx", "    x: str = '1'", "    # cy", "    y: str = '2'"],
    );
    assert!(merged_crlf.contains("\r\n"));
    assert!(
        !merged_crlf
            .as_bytes()
            .windows(2)
            .any(|w| w[0] != b'\r' && w[1] == b'\n'),
        "unexpected bare LF in CRLF inline merge:\n{merged_crlf:?}"
    );
}

#[test]
fn go_unexposed_missing_token_detected_by_collect_syntax_errors_and_strict_diff() {
    let broken_go = "package pkg\nvar ( a = 1; b = 2 )\n";
    let parsed = parse_source(LanguageKind::Go, broken_go, None).unwrap();
    assert!(
        parsed.tree.root_node().has_error(),
        "Go semicolon inside var_spec_list must mark tree has_error"
    );
    let errors = collect_syntax_errors(&parsed.tree);
    assert!(
        !errors.is_empty(),
        "collect_syntax_errors must report unexposed MISSING token when root.has_error() is true"
    );
    assert!(errors.iter().any(|e| e.is_missing));

    let go_path = Path::new("pkg/vars.go");
    let err = diff_source_to_patch_strict(
        &make_ctx(go_path, LanguageKind::Go, "vp1", "a1", 1),
        "",
        broken_go,
    )
    .expect_err("diff_source_to_patch_strict must reject Go tree with unexposed MISSING token");
    assert!(
        matches!(err, CstError::SyntaxError { .. }),
        "expected SyntaxError, got {err:?}"
    );

    // Non-strict diff must fall back to TextCrdtFallback rather than emitting broken CST ops.
    let fallback = diff_source_to_patch(
        &make_ctx(go_path, LanguageKind::Go, "vp1", "a1", 2),
        "",
        broken_go,
    )
    .unwrap();
    assert!(
        fallback
            .ops
            .iter()
            .any(|op| matches!(op, SemanticOp::TextCrdtFallback { .. })),
        "expected TextCrdtFallback for broken Go input, got {:?}",
        fallback.ops
    );
}

#[test]
fn python_property_getter_setter_deleter_disambiguated_merge_and_contract_deltas() {
    let py_path = Path::new("app/prop.py");
    let fid = compute_logical_file_id(py_path);
    let base = concat!(
        "class Temp:\n",
        "    @property\n",
        "    def c(self):\n",
        "        return self._c\n",
        "\n",
        "    @c.setter\n",
        "    def c(self, v):\n",
        "        self._c = v\n",
        "\n",
        "    @c.deleter\n",
        "    def c(self):\n",
        "        del self._c\n",
        "\n",
        "    @property\n",
        "    def _secret(self):\n",
        "        return 0\n",
    );

    let parsed = parse_source(LanguageKind::Python, base, None).unwrap();
    let units = extract_decl_units(LanguageKind::Python, &parsed.tree, base, fid);
    let flat = flatten_decl_units(&units);
    let fqns: Vec<&str> = flat.iter().map(|u| u.anchor.symbol_fqn.as_str()).collect();
    assert!(fqns.contains(&"Temp::get:c"), "missing Temp::get:c in {fqns:?}");
    assert!(fqns.contains(&"Temp::set:c"), "missing Temp::set:c in {fqns:?}");
    assert!(fqns.contains(&"Temp::del:c"), "missing Temp::del:c in {fqns:?}");
    assert!(
        fqns.contains(&"Temp::get:_secret"),
        "missing Temp::get:_secret in {fqns:?}"
    );
    let secret_unit = flat
        .iter()
        .find(|u| u.anchor.symbol_fqn == "Temp::get:_secret")
        .unwrap();
    assert_eq!(secret_unit.visibility, VisibilityKind::Private);

    // Concurrent disjoint body edits to @property getter and @c.setter must merge Clean.
    let v1 = base.replace("return self._c", "return float(self._c)");
    let v2 = base.replace("self._c = v", "self._c = float(v)");
    let p1 = diff_source_to_patch_strict(
        &make_ctx(py_path, LanguageKind::Python, "vp1", "a1", 10),
        base,
        &v1,
    )
    .unwrap();
    let p2 = diff_source_to_patch_strict(
        &make_ctx(py_path, LanguageKind::Python, "vp2", "a2", 11),
        base,
        &v2,
    )
    .unwrap();

    let mut base_files = BTreeMap::new();
    base_files.insert(py_path.to_path_buf(), base.as_bytes().to_vec());
    let report = merge_patches(&base_files, &[p1, p2], None).unwrap();
    assert!(
        report.is_clean(),
        "concurrent @property and @c.setter edits must merge Clean: {:?}",
        report.conflicts
    );
    let merged = report.text(py_path).unwrap();
    assert!(merged.contains("return float(self._c)"), "{merged}");
    assert!(merged.contains("self._c = float(v)"), "{merged}");

    // Removing @c.deleter must emit RemovedSymbol("Temp::del:c"), NOT bogus SignatureChanged.
    let without_deleter = concat!(
        "class Temp:\n",
        "    @property\n",
        "    def c(self):\n",
        "        return self._c\n",
        "\n",
        "    @c.setter\n",
        "    def c(self, v):\n",
        "        self._c = v\n",
        "\n",
        "    @property\n",
        "    def _secret(self):\n",
        "        return 0\n",
    );
    let old_syms = extract_file_symbols(&parsed, fid, py_path, "vp1", 1);
    let new_parsed = parse_source(LanguageKind::Python, without_deleter, None).unwrap();
    let new_syms = extract_file_symbols(&new_parsed, fid, py_path, "vp1", 2);
    let deltas = compute_contract_deltas(&old_syms.provides, &new_syms.provides);
    assert_eq!(deltas.len(), 1, "expected 1 RemovedSymbol delta, got {deltas:?}");
    assert!(
        matches!(
            &deltas[0],
            SymbolContractDelta::RemovedSymbol { symbol_fqn, .. } if symbol_fqn == "Temp::del:c"
        ),
        "expected RemovedSymbol(Temp::del:c), got {deltas:?}"
    );

    // Query matching: unqualified queries match accessors, while distinct accessor queries do not collide.
    assert!(symbol_matches_query("Temp::get:c", "c"));
    assert!(symbol_matches_query("Temp::get:c", "Temp::c"));
    assert!(symbol_matches_query("Temp::get:c", "Temp::get:c"));
    assert!(!symbol_matches_query("Temp::get:c", "Temp::set:c"));
    assert!(!symbol_matches_query("Temp::set:c", "Temp::del:c"));
}

#[test]
fn ts_static_vs_instance_method_and_accessor_disambiguated_merge_and_contract_deltas() {
    let ts_path = Path::new("src/user.ts");
    let fid = compute_logical_file_id(ts_path);
    let base = concat!(
        "export class User {\n",
        "    static parse(raw: string): User {\n",
        "        return new User(raw);\n",
        "    }\n",
        "    parse(raw: string): number {\n",
        "        return raw.length;\n",
        "    }\n",
        "    static get count(): number {\n",
        "        return 1;\n",
        "    }\n",
        "    get count(): number {\n",
        "        return 2;\n",
        "    }\n",
        "}\n",
    );

    let parsed = parse_source(LanguageKind::TypeScript, base, None).unwrap();
    let units = extract_decl_units(LanguageKind::TypeScript, &parsed.tree, base, fid);
    let flat = flatten_decl_units(&units);
    let fqns: Vec<&str> = flat.iter().map(|u| u.anchor.symbol_fqn.as_str()).collect();
    assert!(
        fqns.contains(&"User::static:parse"),
        "missing User::static:parse in {fqns:?}"
    );
    assert!(fqns.contains(&"User::parse"), "missing User::parse in {fqns:?}");
    assert!(
        fqns.contains(&"User::static:get:count"),
        "missing User::static:get:count in {fqns:?}"
    );
    assert!(
        fqns.contains(&"User::get:count"),
        "missing User::get:count in {fqns:?}"
    );

    // Concurrent disjoint body edits to static parse and instance parse (and static/instance get count) merge Clean.
    let v1 = base
        .replace("return new User(raw);", "return new User(raw.trim());")
        .replace("return 1;", "return 10;");
    let v2 = base
        .replace("return raw.length;", "return raw.trim().length;")
        .replace("return 2;", "return 20;");
    let p1 = diff_source_to_patch_strict(
        &make_ctx(ts_path, LanguageKind::TypeScript, "vp1", "a1", 10),
        base,
        &v1,
    )
    .unwrap();
    let p2 = diff_source_to_patch_strict(
        &make_ctx(ts_path, LanguageKind::TypeScript, "vp2", "a2", 11),
        base,
        &v2,
    )
    .unwrap();

    let mut base_files = BTreeMap::new();
    base_files.insert(ts_path.to_path_buf(), base.as_bytes().to_vec());
    let report = merge_patches(&base_files, &[p1, p2], None).unwrap();
    assert!(
        report.is_clean(),
        "concurrent TS static vs instance edits must merge Clean: {:?}",
        report.conflicts
    );
    let merged = report.text(ts_path).unwrap();
    assert!(merged.contains("return new User(raw.trim());"), "{merged}");
    assert!(merged.contains("return raw.trim().length;"), "{merged}");
    assert!(merged.contains("return 10;"), "{merged}");
    assert!(merged.contains("return 20;"), "{merged}");

    // Removing static parse emits RemovedSymbol("User::static:parse") and cross-file consumer triggers SemanticConflict.
    let caller_path = Path::new("src/caller.ts");
    let caller_base = "import { User } from './user';\nexport function run(): void {}\n";
    let caller_v2 = "import { User } from './user';\nexport function run(): User { return User.parse('x'); }\n";
    let without_static_parse = concat!(
        "export class User {\n",
        "    parse(raw: string): number {\n",
        "        return raw.length;\n",
        "    }\n",
        "    static get count(): number {\n",
        "        return 1;\n",
        "    }\n",
        "    get count(): number {\n",
        "        return 2;\n",
        "    }\n",
        "}\n",
    );
    let old_syms = extract_file_symbols(&parsed, fid, ts_path, "vp1", 1);
    let new_parsed = parse_source(LanguageKind::TypeScript, without_static_parse, None).unwrap();
    let new_syms = extract_file_symbols(&new_parsed, fid, ts_path, "vp1", 2);
    let deltas = compute_contract_deltas(&old_syms.provides, &new_syms.provides);
    assert!(
        deltas.iter().any(|d| matches!(
            d,
            SymbolContractDelta::RemovedSymbol { symbol_fqn, .. } if symbol_fqn == "User::static:parse"
        )),
        "expected RemovedSymbol(User::static:parse), got {deltas:?}"
    );

    let p_remove = diff_source_to_patch_strict(
        &make_ctx(ts_path, LanguageKind::TypeScript, "vp1", "a1", 20),
        base,
        without_static_parse,
    )
    .unwrap();
    let p_call = diff_source_to_patch_strict(
        &make_ctx(caller_path, LanguageKind::TypeScript, "vp2", "a2", 21),
        caller_base,
        caller_v2,
    )
    .unwrap();
    base_files.insert(caller_path.to_path_buf(), caller_base.as_bytes().to_vec());
    let sem_report = merge_patches(&base_files, &[p_remove, p_call], None).unwrap();
    assert!(
        matches!(sem_report.outcome, MergeOutcome::SemanticConflict { .. })
            || sem_report
                .conflicts
                .iter()
                .any(|c| matches!(c, MergeOutcome::SemanticConflict { .. })),
        "removing User::static:parse while caller calls User.parse must emit SemanticConflict: {:?}",
        sem_report.outcome
    );

    // Query matching checks.
    assert!(symbol_matches_query("User::static:parse", "parse"));
    assert!(symbol_matches_query("User::static:parse", "User::parse"));
    assert!(symbol_matches_query("User::static:parse", "User::static:parse"));
    assert!(!symbol_matches_query("User::parse", "User::static:parse"));
    assert!(symbol_matches_query("User::static:get:count", "get:count"));
    assert!(symbol_matches_query("User::static:get:count", "count"));
}

#[test]
fn go_and_python_imports_extract_and_merge_cleanly() {
    use loom_core::cst::SymbolKind;

    // 1. Python import forms extraction (no provides/contract_deltas emitted)
    let py_path = Path::new("app/main.py");
    let py_fid = compute_logical_file_id(py_path);
    let py_all_imports = concat!(
        "\"\"\"Module doc.\"\"\"\n",
        "from __future__ import annotations\n",
        "import os\n",
        "import os as _os\n",
        "import sys, json\n",
        "from pkg import a, b\n",
        "from . import x\n",
        "from ..pkg import y\n",
        "from pkg import (\n",
        "    alpha,\n",
        "    beta,\n",
        ")\n\n",
        "def run():\n",
        "    return 1\n",
    );
    let py_parsed = parse_source(LanguageKind::Python, py_all_imports, None).unwrap();
    assert!(!py_parsed.tree.root_node().has_error());
    let py_units = extract_decl_units(LanguageKind::Python, &py_parsed.tree, py_all_imports, py_fid);
    let py_import_units: Vec<_> = py_units
        .iter()
        .filter(|u| u.kind == SymbolKind::Import)
        .collect();
    assert_eq!(
        py_import_units.len(),
        8,
        "expected 8 Python import DeclUnits, got {:?}",
        py_units.iter().map(|u| (&u.kind, &u.anchor.symbol_fqn)).collect::<Vec<_>>()
    );
    let py_syms = extract_file_symbols(&py_parsed, py_fid, py_path, "vp1", 1);
    assert!(
        !py_syms.provides.iter().any(|p| p.kind == SymbolKind::Import),
        "imports must not appear in provides"
    );

    // 2. Python import-only add vs function body edit merges Clean & keeps import
    let py_base = "def run():\n    return 1\n";
    let py_vp1 = "import os\n\ndef run():\n    return 1\n";
    let py_vp2 = "def run():\n    return 2\n";
    let p_py1 = diff_source_to_patch_strict(
        &make_ctx(py_path, LanguageKind::Python, "vp1", "a1", 1),
        py_base,
        py_vp1,
    )
    .unwrap();
    assert!(!p_py1.ops.is_empty(), "Python import addition must produce InsertMember op");
    assert!(
        p_py1.contract_deltas.is_empty(),
        "Python import addition must not emit contract_deltas: {:?}",
        p_py1.contract_deltas
    );
    let p_py2 = diff_source_to_patch_strict(
        &make_ctx(py_path, LanguageKind::Python, "vp2", "a2", 2),
        py_base,
        py_vp2,
    )
    .unwrap();
    let mut py_files = BTreeMap::new();
    py_files.insert(py_path.to_path_buf(), py_base.as_bytes().to_vec());
    let py_r12 = merge_patches(&py_files, &[p_py1.clone(), p_py2.clone()], None).unwrap();
    let py_r21 = merge_patches(&py_files, &[p_py2, p_py1], None).unwrap();
    assert!(py_r12.is_clean(), "py merge conflicts: {:?}", py_r12.conflicts);
    assert_eq!(py_r12.text(py_path), py_r21.text(py_path));
    let py_merged = py_r12.text(py_path).unwrap();
    assert!(py_merged.contains("import os"), "missing import os:\n{py_merged}");
    assert!(py_merged.contains("return 2"), "missing body edit:\n{py_merged}");
    assert!(!parse_source(LanguageKind::Python, py_merged, None).unwrap().tree.root_node().has_error());

    // 3. Python concurrent imports + `from __future__ import annotations` stays after docstring & before defs
    let py_doc_base = "\"\"\"Module doc.\"\"\"\n\nimport json\n\ndef run():\n    return 1\n";
    let py_doc_v1 = "\"\"\"Module doc.\"\"\"\n\nimport json\n\ndef helper():\n    return 42\n\ndef run():\n    return 1\n";
    let py_doc_v2 = "\"\"\"Module doc.\"\"\"\n\nfrom __future__ import annotations\nimport json\n\ndef run():\n    return 1\n";
    let pd1 = diff_source_to_patch_strict(
        &make_ctx(py_path, LanguageKind::Python, "vp1", "a1", 1),
        py_doc_base,
        py_doc_v1,
    )
    .unwrap();
    let pd2 = diff_source_to_patch_strict(
        &make_ctx(py_path, LanguageKind::Python, "vp2", "a2", 2),
        py_doc_base,
        py_doc_v2,
    )
    .unwrap();
    let mut py_doc_files = BTreeMap::new();
    py_doc_files.insert(py_path.to_path_buf(), py_doc_base.as_bytes().to_vec());
    let pd_m12 = merge_patches(&py_doc_files, &[pd1.clone(), pd2.clone()], None).unwrap();
    let pd_m21 = merge_patches(&py_doc_files, &[pd2, pd1], None).unwrap();
    assert!(pd_m12.is_clean(), "py future+helper conflicts: {:?}", pd_m12.conflicts);
    assert_eq!(pd_m12.text(py_path), pd_m21.text(py_path));
    let pd_txt = pd_m12.text(py_path).unwrap();
    let doc_pos = pd_txt.find("\"\"\"Module doc.\"\"\"").unwrap();
    let fut_pos = pd_txt.find("from __future__ import annotations").unwrap();
    let json_pos = pd_txt.find("import json").unwrap();
    let help_pos = pd_txt.find("def helper():").unwrap();
    assert!(
        doc_pos < fut_pos && fut_pos < json_pos && json_pos < help_pos,
        "from __future__ must follow docstring and precede imports/defs:\n{pd_txt}"
    );
    assert!(!parse_source(LanguageKind::Python, pd_txt, None).unwrap().tree.root_node().has_error());

    // Python concurrent `from __future__` (higher lamport_ts) vs prepended `def` (lower lamport_ts) on bare file
    let py_bare = "def run():\n    return 1\n";
    let py_bare_def = "def helper():\n    return 0\n\ndef run():\n    return 1\n";
    let py_bare_fut = "from __future__ import annotations\nimport os\n\ndef run():\n    return 1\n";
    let pb1 = diff_source_to_patch_strict(
        &make_ctx(py_path, LanguageKind::Python, "vp1", "a1", 1),
        py_bare,
        py_bare_def,
    )
    .unwrap();
    let pb2 = diff_source_to_patch_strict(
        &make_ctx(py_path, LanguageKind::Python, "vp2", "a2", 10),
        py_bare,
        py_bare_fut,
    )
    .unwrap();
    let mut pb_files = BTreeMap::new();
    pb_files.insert(py_path.to_path_buf(), py_bare.as_bytes().to_vec());
    let pb_m12 = merge_patches(&pb_files, &[pb1.clone(), pb2.clone()], None).unwrap();
    let pb_m21 = merge_patches(&pb_files, &[pb2, pb1], None).unwrap();
    assert!(pb_m12.is_clean(), "py bare future+def conflicts: {:?}", pb_m12.conflicts);
    assert_eq!(pb_m12.text(py_path), pb_m21.text(py_path));
    let pb_txt = pb_m12.text(py_path).unwrap();
    assert!(
        pb_txt.find("from __future__ import annotations").unwrap()
            < pb_txt.find("import os").unwrap()
            && pb_txt.find("import os").unwrap() < pb_txt.find("def helper():").unwrap(),
        "Python header tiering violated:\n{pb_txt}"
    );

    // 4. Go single & grouped import extraction
    let go_path = Path::new("pkg/main.go");
    let go_fid = compute_logical_file_id(go_path);
    let go_all_imports = concat!(
        "package pkg\n\n",
        "import \"fmt\"\n",
        "import alias \"os\"\n",
        "import . \"math\"\n",
        "import _ \"strings\"\n",
        "import (\n\t\"io\"\n\t\"bytes\"\n)\n\n",
        "func Run() int {\n\treturn 1\n}\n",
    );
    let go_parsed = parse_source(LanguageKind::Go, go_all_imports, None).unwrap();
    assert!(!go_parsed.tree.root_node().has_error());
    let go_units = extract_decl_units(LanguageKind::Go, &go_parsed.tree, go_all_imports, go_fid);
    assert_eq!(
        go_units.iter().filter(|u| u.kind == SymbolKind::Import).count(),
        5,
        "expected 4 single + 1 grouped Go import DeclUnits"
    );
    let grp = go_units
        .iter()
        .find(|u| u.anchor.symbol_fqn == "pkg::import_group#1")
        .expect("expected pkg::import_group#1");
    assert_eq!(grp.children.len(), 2);

    // 5. Go single import add vs func body edit merges Clean & keeps import after package
    let go_base = "package pkg\n\nfunc Run() int {\n\treturn 1\n}\n";
    let go_vp1 = "package pkg\n\nimport \"fmt\"\n\nfunc Run() int {\n\treturn 1\n}\n";
    let go_vp2 = "package pkg\n\nfunc Run() int {\n\treturn 2\n}\n";
    let pg1 = diff_source_to_patch_strict(
        &make_ctx(go_path, LanguageKind::Go, "vp1", "a1", 1),
        go_base,
        go_vp1,
    )
    .unwrap();
    assert!(!pg1.ops.is_empty(), "Go import addition must produce InsertMember op");
    assert!(
        pg1.contract_deltas.is_empty(),
        "Go import addition must not emit contract_deltas: {:?}",
        pg1.contract_deltas
    );
    let pg2 = diff_source_to_patch_strict(
        &make_ctx(go_path, LanguageKind::Go, "vp2", "a2", 2),
        go_base,
        go_vp2,
    )
    .unwrap();
    let mut go_files = BTreeMap::new();
    go_files.insert(go_path.to_path_buf(), go_base.as_bytes().to_vec());
    let go_r12 = merge_patches(&go_files, &[pg1.clone(), pg2.clone()], None).unwrap();
    let go_r21 = merge_patches(&go_files, &[pg2, pg1], None).unwrap();
    assert!(go_r12.is_clean(), "go merge conflicts: {:?}", go_r12.conflicts);
    assert_eq!(go_r12.text(go_path), go_r21.text(go_path));
    let go_merged = go_r12.text(go_path).unwrap();
    assert!(go_merged.starts_with("package pkg"), "{go_merged}");
    assert!(go_merged.contains("import \"fmt\""), "{go_merged}");
    assert!(go_merged.contains("return 2"), "{go_merged}");
    assert!(!parse_source(LanguageKind::Go, go_merged, None).unwrap().tree.root_node().has_error());

    // 6. Go concurrent `func Helper()` (lower lamport_ts) + `import "fmt"` + peer `import "os"` keeps all imports before funcs
    let go_fn_and_imp = "package pkg\n\nimport \"fmt\"\n\nfunc Helper() int {\n\treturn 0\n}\n\nfunc Run() int {\n\treturn 1\n}\n";
    let go_imp_only = "package pkg\n\nimport \"os\"\n\nfunc Run() int {\n\treturn 1\n}\n";
    let pg_a = diff_source_to_patch_strict(
        &make_ctx(go_path, LanguageKind::Go, "vp1", "a1", 1),
        go_base,
        go_fn_and_imp,
    )
    .unwrap();
    let pg_b = diff_source_to_patch_strict(
        &make_ctx(go_path, LanguageKind::Go, "vp2", "a2", 2),
        go_base,
        go_imp_only,
    )
    .unwrap();
    let go_order12 = merge_patches(&go_files, &[pg_a.clone(), pg_b.clone()], None).unwrap();
    let go_order21 = merge_patches(&go_files, &[pg_b, pg_a], None).unwrap();
    assert!(go_order12.is_clean(), "go tiering conflicts: {:?}", go_order12.conflicts);
    assert_eq!(go_order12.text(go_path), go_order21.text(go_path));
    let go_ord_txt = go_order12.text(go_path).unwrap();
    assert!(!parse_source(LanguageKind::Go, go_ord_txt, None).unwrap().tree.root_node().has_error());
    assert!(go_ord_txt.contains("import \"fmt\""));
    assert!(go_ord_txt.contains("import \"os\""));
    assert!(go_ord_txt.contains("func Helper()"));
    assert!(
        go_ord_txt.find("import \"fmt\"").unwrap() < go_ord_txt.find("func Helper()").unwrap()
            && go_ord_txt.find("import \"os\"").unwrap() < go_ord_txt.find("func Helper()").unwrap(),
        "Go imports must precede functions:\n{go_ord_txt}"
    );

    // 7. Go grouped `import ( ... )` concurrent member additions & disjoint/colliding import_group
    let go_grp_base = "package pkg\n\nimport (\n\t\"fmt\"\n)\n\nfunc Run() {}\n";
    let go_grp_1 = "package pkg\n\nimport (\n\t\"fmt\"\n\t\"os\"\n)\n\nfunc Run() {}\n";
    let go_grp_2 = "package pkg\n\nimport (\n\t\"fmt\"\n\t\"strings\"\n)\n\nfunc Run() {}\n";
    let pgg1 = diff_source_to_patch_strict(
        &make_ctx(go_path, LanguageKind::Go, "vp1", "a1", 1),
        go_grp_base,
        go_grp_1,
    )
    .unwrap();
    let pgg2 = diff_source_to_patch_strict(
        &make_ctx(go_path, LanguageKind::Go, "vp2", "a2", 2),
        go_grp_base,
        go_grp_2,
    )
    .unwrap();
    let mut go_grp_files = BTreeMap::new();
    go_grp_files.insert(go_path.to_path_buf(), go_grp_base.as_bytes().to_vec());
    let ggr12 = merge_patches(&go_grp_files, &[pgg1.clone(), pgg2.clone()], None).unwrap();
    let ggr21 = merge_patches(&go_grp_files, &[pgg2, pgg1], None).unwrap();
    assert!(ggr12.is_clean(), "go grouped import merge conflicts: {:?}", ggr12.conflicts);
    assert_eq!(ggr12.text(go_path), ggr21.text(go_path));
    let ggr_txt = ggr12.text(go_path).unwrap();
    assert!(ggr_txt.contains("\"fmt\"") && ggr_txt.contains("\"os\"") && ggr_txt.contains("\"strings\""));
    assert!(!parse_source(LanguageKind::Go, ggr_txt, None).unwrap().tree.root_node().has_error());

    // Disjoint vs colliding import_group creation
    let go_new_grp_1 = "package pkg\n\nimport (\n\t\"fmt\"\n)\n\nfunc Run() int {\n\treturn 1\n}\n";
    let go_new_grp_2 = "package pkg\n\nimport (\n\t\"os\"\n)\n\nfunc Run() int {\n\treturn 1\n}\n";
    let go_new_grp_dup = "package pkg\n\nimport (\n\t\"fmt\"\n\t\"strings\"\n)\n\nfunc Run() int {\n\treturn 1\n}\n";
    let png1 = diff_source_to_patch_strict(
        &make_ctx(go_path, LanguageKind::Go, "vp1", "a1", 1),
        go_base,
        go_new_grp_1,
    )
    .unwrap();
    let png2 = diff_source_to_patch_strict(
        &make_ctx(go_path, LanguageKind::Go, "vp2", "a2", 2),
        go_base,
        go_new_grp_2,
    )
    .unwrap();
    let png_dup = diff_source_to_patch_strict(
        &make_ctx(go_path, LanguageKind::Go, "vp3", "a3", 3),
        go_base,
        go_new_grp_dup,
    )
    .unwrap();
    let disjoint_grp = merge_patches(&go_files, &[png1.clone(), png2], None).unwrap();
    assert!(disjoint_grp.is_clean(), "disjoint Go import_group must merge Clean: {:?}", disjoint_grp.conflicts);
    let dup_grp = merge_patches(&go_files, &[png1, png_dup], None).unwrap();
    assert!(
        matches!(dup_grp.outcome, MergeOutcome::StructuralConflict { .. }),
        "duplicate Go import inside import_group must emit StructuralConflict: {:?}",
        dup_grp.outcome
    );
}

#[test]
fn rust_impl_methods_match_type_method_calls_and_disambiguate_blocks() {
    let foo_path = Path::new("src/foo.rs");
    let caller_path = Path::new("src/caller.rs");
    let foo_base = concat!(
        "pub struct Foo;\n",
        "pub struct Bar;\n",
        "pub trait Display { fn fmt(&self) -> i32; }\n\n",
        "impl Foo {\n    pub fn new(a: i32) -> Self { Foo }\n}\n\n",
        "impl Foo {\n    pub fn bar(x: i32) -> i32 { x + 1 }\n}\n\n",
        "impl Display for Foo {\n    fn fmt(&self) -> i32 { 0 }\n}\n",
    );
    let foo_sig_new = concat!(
        "pub struct Foo;\n",
        "pub struct Bar;\n",
        "pub trait Display { fn fmt(&self) -> i32; }\n\n",
        "impl Foo {\n    pub fn new(a: i32, b: i32) -> Self { Foo }\n}\n\n",
        "impl Foo {\n    pub fn bar(x: i32) -> i32 { x + 1 }\n}\n\n",
        "impl Display for Foo {\n    fn fmt(&self) -> i32 { 0 }\n}\n",
    );
    let foo_sig_bar2 = concat!(
        "pub struct Foo;\n",
        "pub struct Bar;\n",
        "pub trait Display { fn fmt(&self) -> i32; }\n\n",
        "impl Foo {\n    pub fn new(a: i32) -> Self { Foo }\n}\n\n",
        "impl Foo {\n    pub fn bar(x: i32, y: i32) -> i32 { x + y }\n}\n\n",
        "impl Display for Foo {\n    fn fmt(&self) -> i32 { 0 }\n}\n",
    );
    let foo_rem_trait = concat!(
        "pub struct Foo;\n",
        "pub struct Bar;\n",
        "pub trait Display { fn fmt(&self) -> i32; }\n\n",
        "impl Foo {\n    pub fn new(a: i32) -> Self { Foo }\n}\n\n",
        "impl Foo {\n    pub fn bar(x: i32) -> i32 { x + 1 }\n}\n",
    );

    let caller_base = "use crate::foo::{Bar, Display, Foo};\npub fn run() {}\n";
    let caller_new = "use crate::foo::{Bar, Display, Foo};\npub fn run() { let _ = Foo::new(1); }\n";
    let caller_bar2 = "use crate::foo::{Bar, Display, Foo};\npub fn run() { let _ = Foo::bar(2); }\n";
    let caller_bar_other = "use crate::foo::{Bar, Display, Foo};\npub fn run() { let _ = Bar::new(1); }\n";
    let caller_fmt = "use crate::foo::{Bar, Display, Foo};\npub fn run() { let f = Foo; let _ = Foo::fmt(&f); }\n";

    let p_new = diff_source_to_patch_strict(
        &make_ctx(foo_path, LanguageKind::Rust, "vp1", "a1", 1),
        foo_base,
        foo_sig_new,
    )
    .unwrap();
    let p_bar2 = diff_source_to_patch_strict(
        &make_ctx(foo_path, LanguageKind::Rust, "vp1", "a1", 2),
        foo_base,
        foo_sig_bar2,
    )
    .unwrap();
    let p_rem_tr = diff_source_to_patch_strict(
        &make_ctx(foo_path, LanguageKind::Rust, "vp1", "a1", 3),
        foo_base,
        foo_rem_trait,
    )
    .unwrap();

    let pc_new = diff_source_to_patch_strict(
        &make_ctx(caller_path, LanguageKind::Rust, "vp2", "a2", 4),
        caller_base,
        caller_new,
    )
    .unwrap();
    let pc_bar2 = diff_source_to_patch_strict(
        &make_ctx(caller_path, LanguageKind::Rust, "vp2", "a2", 5),
        caller_base,
        caller_bar2,
    )
    .unwrap();
    let pc_other = diff_source_to_patch_strict(
        &make_ctx(caller_path, LanguageKind::Rust, "vp2", "a2", 6),
        caller_base,
        caller_bar_other,
    )
    .unwrap();
    let pc_fmt = diff_source_to_patch_strict(
        &make_ctx(caller_path, LanguageKind::Rust, "vp2", "a2", 7),
        caller_base,
        caller_fmt,
    )
    .unwrap();

    let mut base_files = BTreeMap::new();
    base_files.insert(foo_path.to_path_buf(), foo_base.as_bytes().to_vec());
    base_files.insert(caller_path.to_path_buf(), caller_base.as_bytes().to_vec());

    // 1. merge_patches: impl<Foo>::new vs Foo::new(1) -> SemanticConflict
    let m1 = merge_patches(&base_files, &[p_new.clone(), pc_new], None).unwrap();
    assert!(
        matches!(
            m1.outcome,
            MergeOutcome::SemanticConflict { ref symbol_fqn, .. } if symbol_fqn == "impl<Foo>::new"
        ),
        "expected SemanticConflict for impl<Foo>::new vs Foo::new(1), got: {:?}",
        m1.conflicts
    );

    // 2. merge_patches: impl<Foo>#2::bar vs Foo::bar(2) -> SemanticConflict
    let m2 = merge_patches(&base_files, &[p_bar2, pc_bar2], None).unwrap();
    assert!(
        matches!(
            m2.outcome,
            MergeOutcome::SemanticConflict { ref symbol_fqn, .. } if symbol_fqn == "impl<Foo>#2::bar"
        ),
        "expected SemanticConflict for impl<Foo>#2::bar vs Foo::bar(2), got: {:?}",
        m2.conflicts
    );

    // 3. merge_patches: impl<Display for Foo>::fmt removal vs Foo::fmt(&f) -> SemanticConflict
    let m3 = merge_patches(&base_files, &[p_rem_tr, pc_fmt], None).unwrap();
    assert!(
        matches!(
            m3.outcome,
            MergeOutcome::SemanticConflict { ref symbol_fqn, .. } if symbol_fqn == "impl<Display for Foo>::fmt"
        ),
        "expected SemanticConflict for impl<Display for Foo>::fmt, got: {:?}",
        m3.conflicts
    );

    // 4. Negative check: Bar::new(1) must NOT false-conflict with impl<Foo>::new
    let m4 = merge_patches(&base_files, &[p_new.clone(), pc_other], None).unwrap();
    assert!(m4.is_clean(), "Bar::new(1) must not false-conflict with impl<Foo>::new: {:?}", m4.conflicts);

    // 5. merge_trees: untouched caller file calling Foo::new(1) -> SemanticConflict
    let mut tree_files = BTreeMap::new();
    tree_files.insert(foo_path.to_path_buf(), foo_base.as_bytes().to_vec());
    tree_files.insert(caller_path.to_path_buf(), caller_new.as_bytes().to_vec());
    let mt = merge_trees(&tree_files, &[&[p_new]], None).unwrap();
    assert!(
        matches!(
            mt.outcome,
            MergeOutcome::SemanticConflict { ref symbol_fqn, .. } if symbol_fqn == "impl<Foo>::new"
        ),
        "merge_trees untouched caller Foo::new(1) must emit SemanticConflict: {:?}",
        mt.conflicts
    );
}
