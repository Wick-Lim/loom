use loom_core::cst::{
    compute_input_edit, compute_logical_file_id, extract_file_symbols, parse_source_incremental,
    tags_queries_ready,
};
use loom_core::db::{StateDb, TABLE_NEGATIVE_INDEX, TABLE_SYMBOL_CONSUMES, TABLE_SYMBOL_PROVIDES};
use loom_core::model::{
    Blake3Hash, CausalCapsule, FailureKind, LanguageKind, NegativeEdge, RadarConflictItem,
    RadarLevel, SymbolConsumeEntry, SymbolLocation, SymbolProvideEntry, SymbolStatus,
    ViewportState,
};
use loom_core::rpc::{
    McpPreflightResult, RadarBatchMetrics, RadarMatrixCell, RadarSnapshot, RadarStreamEvent,
    RadarViewportSummary, RpcMethod, RpcPayload, RpcRequest, RpcResponse, read_frame, write_frame,
};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

#[test]
fn incremental_parse_and_compute_input_edit_all_languages_and_unicode() {
    let cases = [
        (
            LanguageKind::Rust,
            "/// doc\r\npub fn greet(name: &str) -> usize {\r\n    name.len()\r\n}\r\n",
            "/// 안녕하세요 🚀\r\npub fn greet(name: &str, extra: i32) -> usize {\r\n    name.len() + extra as usize\r\n}\r\n",
        ),
        (
            LanguageKind::TypeScript,
            "export function greet(name: string): number {\n  return name.length;\n}\n",
            "export function greet(name: string, count: number): number {\n  return name.length + count;\n}\n",
        ),
        (
            LanguageKind::Tsx,
            "export function Badge({ label }: { label: string }) {\n  return <span>{label}</span>;\n}\n",
            "export function Badge({ label }: { label: string }) {\n  return <div title=\"유니코드\">{label}</div>;\n}\n",
        ),
        (
            LanguageKind::Go,
            "package main\nfunc Greet(n int) int {\n\treturn n + 1\n}\n",
            "package main\nfunc Greet(n int, m int) int {\n\treturn n + m\n}\n",
        ),
        (
            LanguageKind::Python,
            "def greet(name: str) -> int:\n    return len(name)\n",
            "def greet(name: str, bonus: int = 1) -> int:\n    # 한글 주석\n    return len(name) + bonus\n",
        ),
    ];

    for (lang, old_src, new_src) in cases {
        assert!(compute_input_edit(old_src, old_src).is_none());
        let edit = compute_input_edit(old_src, new_src).expect("non-empty edit");
        assert!(old_src.is_char_boundary(edit.start_byte));
        assert!(old_src.is_char_boundary(edit.old_end_byte));
        assert!(new_src.is_char_boundary(edit.new_end_byte));

        let mut parser = tree_sitter::Parser::new();
        let ts_lang = loom_core::cst::ts_language(lang).unwrap();
        parser.set_language(&ts_lang).unwrap();

        let first = parse_source_incremental(lang, "", old_src, None, Some(&mut parser)).unwrap();
        assert!(!first.used_incremental);
        let mut old_tree = first.parsed.tree;

        let second =
            parse_source_incremental(lang, old_src, new_src, Some(&mut old_tree), Some(&mut parser))
                .unwrap();
        assert!(second.used_incremental);
        assert!(!second.parsed.tree.root_node().has_error());
        assert!(!second.changed_ranges.is_empty());
    }
}

#[test]
fn tags_queries_compile_and_extract_provides_consumes_with_error_guard() {
    assert!(tags_queries_ready(), "all 5 TAGS_QUERY queries must compile");

    let rs_src = r#"
pub trait Runner { fn run(&self); }
pub struct Job;
impl Runner for Job {
    fn run(&self) {
        helper();
    }
}
pub fn helper() {}
"#;
    let rs_parsed = loom_core::cst::parse_source(LanguageKind::Rust, rs_src, None).unwrap();
    let rs_syms = extract_file_symbols(
        &rs_parsed,
        compute_logical_file_id(Path::new("src/lib.rs")),
        Path::new("src/lib.rs"),
        "vp-1",
        10,
    );
    assert!(!rs_syms.pending_syntax);
    assert!(rs_syms.provide_entries.iter().any(|p| p.symbol_fqn.ends_with("helper")));
    assert!(rs_syms.consume_entries.iter().any(|c| c.symbol_fqn == "helper"));
    assert!(rs_syms.consume_entries.iter().any(|c| c.symbol_fqn == "Runner"));

    // Syntax error inside one function should mark pending_syntax and omit errored consumes
    let rs_broken = "pub fn ok() { good_call(); }\npub fn broken( { bad_call(); }\n";
    let broken_parsed = loom_core::cst::parse_source(LanguageKind::Rust, rs_broken, None).unwrap();
    let broken_syms = extract_file_symbols(
        &broken_parsed,
        compute_logical_file_id(Path::new("src/lib.rs")),
        Path::new("src/lib.rs"),
        "vp-1",
        20,
    );
    assert!(broken_syms.pending_syntax);
    assert!(broken_syms.consume_entries.iter().any(|c| c.symbol_fqn == "good_call"));
    assert!(!broken_syms.consume_entries.iter().any(|c| c.symbol_fqn == "bad_call"));
}

#[test]
fn db_clear_viewport_symbols_and_negative_edge_suffix_file_lookup() {
    let dir = TempDir::new().unwrap();
    let db = StateDb::open_or_create(&dir.path().join("state.redb")).unwrap();

    let wtx = db.begin_write().unwrap();
    {
        let mut sp = wtx.open_table(TABLE_SYMBOL_PROVIDES).unwrap();
        StateDb::upsert_symbol_provide_in_tx(
            &mut sp,
            &SymbolProvideEntry {
                symbol_fqn: "crate::auth::verify_token".into(),
                viewport_id: "vp-1".into(),
                file_path: PathBuf::from("src/auth.rs"),
                status: SymbolStatus::Ready,
                ..SymbolProvideEntry::default()
            },
        )
        .unwrap();
        StateDb::upsert_symbol_provide_in_tx(
            &mut sp,
            &SymbolProvideEntry {
                symbol_fqn: "crate::auth::verify_token".into(),
                viewport_id: "vp-2".into(),
                file_path: PathBuf::from("src/auth.rs"),
                status: SymbolStatus::Ready,
                ..SymbolProvideEntry::default()
            },
        )
        .unwrap();

        let mut sc = wtx.open_table(TABLE_SYMBOL_CONSUMES).unwrap();
        StateDb::upsert_symbol_consume_in_tx(
            &mut sc,
            &SymbolConsumeEntry {
                symbol_fqn: "verify_token".into(),
                viewport_id: "vp-1".into(),
                locations: vec![SymbolLocation {
                    file_path: PathBuf::from("src/main.rs"),
                    line: 3,
                    column: 5,
                    ..SymbolLocation::default()
                }],
                ..SymbolConsumeEntry::default()
            },
        )
        .unwrap();

        let mut neg = wtx.open_table(TABLE_NEGATIVE_INDEX).unwrap();
        let edge = NegativeEdge {
            edge_id: "ne-1".into(),
            edge_seq: 1,
            viewport_id: "vp-old".into(),
            intent: "jwt refactor".into(),
            touched_symbols: vec![
                "crate::auth::verify_token".into(),
                "crate::auth::parse_claims".into(),
            ],
            failure_kind: FailureKind::CompileError,
            lesson_summary: "avoid global mutex in verify_token".into(),
            ..NegativeEdge::default()
        };
        for sym in &edge.touched_symbols {
            StateDb::insert_negative_edge_in_tx(&mut neg, sym, &edge).unwrap();
        }
    }
    wtx.commit().unwrap();

    assert_eq!(db.list_symbol_provides_for_viewport("vp-1").unwrap().len(), 1);
    assert_eq!(db.list_symbol_consumes_for_viewport("vp-1").unwrap().len(), 1);

    // Suffix query matches and deduplicates multi-symbol NegativeEdge
    let hits = db
        .query_negative_edges_matching(&["verify_token".to_string()], &[])
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].edge_id, "ne-1");

    // File query matches module stem
    let file_hits = db
        .query_negative_edges_matching(&[], &[PathBuf::from("src/auth.rs")])
        .unwrap();
    assert_eq!(file_hits.len(), 1);

    // Clear only vp-1 symbols, leaving vp-2 intact
    let wtx2 = db.begin_write().unwrap();
    StateDb::clear_viewport_symbols_in_wtx(&wtx2, "vp-1").unwrap();
    wtx2.commit().unwrap();

    assert!(db.list_symbol_provides_for_viewport("vp-1").unwrap().is_empty());
    assert!(db.list_symbol_consumes_for_viewport("vp-1").unwrap().is_empty());
    assert_eq!(db.list_symbol_provides_for_viewport("vp-2").unwrap().len(), 1);
}

#[tokio::test]
async fn rpc_m4_radar_and_mcp_frames_roundtrip() {
    let snap = RadarSnapshot {
        sequence: 3,
        repo_root: PathBuf::from("repo"),
        batch_mode: true,
        debounce_ms: 200,
        metrics: RadarBatchMetrics {
            batch_mode: true,
            effective_debounce_ms: 200,
            batch_switches: 1,
            coalesced_events: 22,
        },
        viewports: vec![RadarViewportSummary {
            viewport_id: "vp-1".into(),
            viewport_name: "a1".into(),
            intent: "fix".into(),
            state: ViewportState::Active,
            level: RadarLevel::Yellow,
            pending_syntax: true,
            pending_syntax_files: vec![PathBuf::from("src/lib.rs")],
            conflicts: vec![RadarConflictItem {
                peer_viewport_id: "vp-2".into(),
                peer_viewport_name: "a2".into(),
                level: RadarLevel::Yellow,
                kind: "consumed_symbol_modified".into(),
                file_path: Some(PathBuf::from("src/lib.rs")),
                symbol_fqn: Some("foo".into()),
                message: "modified".into(),
            }],
            negative_warnings: vec![],
            dirty_files: vec![PathBuf::from("src/lib.rs")],
            updated_at_ms: 42,
        }],
        matrix: vec![RadarMatrixCell {
            viewport_a_id: "vp-1".into(),
            viewport_a_name: "a1".into(),
            viewport_b_id: "vp-2".into(),
            viewport_b_name: "a2".into(),
            level: RadarLevel::Yellow,
            conflicts: vec![],
        }],
        updated_at_ms: 42,
    };

    let reqs = vec![
        RpcRequest {
            id: 1,
            method: RpcMethod::RadarSubscribe {
                viewport_id: Some("vp-1".into()),
            },
        },
        RpcRequest {
            id: 2,
            method: RpcMethod::McpPreflightEx {
                viewport_id: None,
                symbols: vec!["foo".into()],
                files: vec![PathBuf::from("src/lib.rs")],
            },
        },
    ];
    let mut buf = Vec::new();
    for r in &reqs {
        write_frame(&mut buf, r).await.unwrap();
    }
    let mut cur = std::io::Cursor::new(buf);
    for r in &reqs {
        let dec: RpcRequest = read_frame(&mut cur).await.unwrap().unwrap();
        assert_eq!(dec.id, r.id);
    }

    let resps = vec![
        RpcResponse {
            id: 1,
            result: Ok(RpcPayload::RadarSnapshot(snap.clone())),
        },
        RpcResponse {
            id: 0,
            result: Ok(RpcPayload::RadarEvent(RadarStreamEvent {
                snapshot: snap.clone(),
                changed_viewports: vec!["vp-1".into()],
                escalated_notifications: vec![],
            })),
        },
        RpcResponse {
            id: 2,
            result: Ok(RpcPayload::McpPreflight(McpPreflightResult {
                viewport_id: Some("vp-1".into()),
                provides: vec![],
                consumes: vec![],
                radar: snap,
                negative_warnings: vec![],
            })),
        },
        RpcResponse {
            id: 3,
            result: Ok(RpcPayload::McpRecordDeadend(NegativeEdge {
                edge_id: "ne-1".into(),
                edge_seq: 1,
                lesson_summary: "test".into(),
                ..NegativeEdge::default()
            })),
        },
        RpcResponse {
            id: 4,
            result: Ok(RpcPayload::CapsuleSeal(CausalCapsule {
                capsule_id: "cap-1".into(),
                summary: "sealed".into(),
                ..CausalCapsule::default()
            })),
        },
    ];
    let mut rbuf = Vec::new();
    for resp in &resps {
        write_frame(&mut rbuf, resp).await.unwrap();
    }
    let mut rcur = std::io::Cursor::new(rbuf);
    for resp in &resps {
        let dec: RpcResponse = read_frame(&mut rcur).await.unwrap().unwrap();
        assert_eq!(dec.id, resp.id);
    }

    let _ = Blake3Hash::ZERO;
}
