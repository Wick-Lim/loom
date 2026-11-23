use super::anchor::{
    DeclUnit, SymbolKind, VisibilityKind, extract_decl_units, flatten_decl_units,
};
use super::lang::{ParsedFile, node_in_error_subtree, ts_language};
use crate::model::{
    Blake3Hash, LanguageKind, NodeAnchor, SymbolConsumeEntry, SymbolContractDelta, SymbolLocation,
    SymbolProvideEntry, SymbolStatus,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::path::Path;
use std::sync::OnceLock;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Query, QueryCursor};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvidedSymbol {
    pub anchor: NodeAnchor,
    pub kind: SymbolKind,
    pub visibility: VisibilityKind,
    pub normalized_signature: String,
    pub arity: Option<(usize, usize)>,
    pub status: SymbolStatus,
    pub line: u32,
    pub column: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsumedSymbol {
    pub symbol_ref: String,
    pub location: SymbolLocation,
    pub byte_range: Range<usize>,
    pub call_arity: Option<usize>,
    pub in_error_subtree: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ExtractedSymbols {
    pub provides: Vec<ProvidedSymbol>,
    pub consumes: Vec<ConsumedSymbol>,
    pub provide_entries: Vec<SymbolProvideEntry>,
    pub consume_entries: Vec<SymbolConsumeEntry>,
    pub pending_syntax: bool,
}

struct LangQueries {
    rust: Query,
    ts: Query,
    tsx: Query,
    go: Query,
    python: Query,
}

static QUERIES: OnceLock<LangQueries> = OnceLock::new();
static TAG_QUERIES: OnceLock<LangQueries> = OnceLock::new();

fn get_tags_query(lang: LanguageKind) -> Option<&'static Query> {
    let qs = TAG_QUERIES.get_or_init(|| {
        let rust = Query::new(
            &ts_language(LanguageKind::Rust).expect("rust language"),
            tree_sitter_rust::TAGS_QUERY,
        )
        .expect("rust tags query");

        let ts = Query::new(
            &ts_language(LanguageKind::TypeScript).expect("ts language"),
            tree_sitter_typescript::TAGS_QUERY,
        )
        .expect("ts tags query");

        let tsx = Query::new(
            &ts_language(LanguageKind::Tsx).expect("tsx language"),
            tree_sitter_typescript::TAGS_QUERY,
        )
        .expect("tsx tags query");

        let go = Query::new(
            &ts_language(LanguageKind::Go).expect("go language"),
            tree_sitter_go::TAGS_QUERY,
        )
        .expect("go tags query");

        let python = Query::new(
            &ts_language(LanguageKind::Python).expect("python language"),
            tree_sitter_python::TAGS_QUERY,
        )
        .expect("python tags query");

        LangQueries {
            rust,
            ts,
            tsx,
            go,
            python,
        }
    });

    match lang {
        LanguageKind::Rust => Some(&qs.rust),
        LanguageKind::TypeScript => Some(&qs.ts),
        LanguageKind::Tsx => Some(&qs.tsx),
        LanguageKind::Go => Some(&qs.go),
        LanguageKind::Python => Some(&qs.python),
        _ => None,
    }
}

pub fn tags_queries_ready() -> bool {
    [
        LanguageKind::Rust,
        LanguageKind::TypeScript,
        LanguageKind::Tsx,
        LanguageKind::Go,
        LanguageKind::Python,
    ]
    .into_iter()
    .all(|lang| get_tags_query(lang).is_some())
}

fn get_query(lang: LanguageKind) -> Option<&'static Query> {
    let qs = QUERIES.get_or_init(|| {
        let rust = Query::new(
            &ts_language(LanguageKind::Rust).expect("rust language"),
            r#"
            (call_expression function: (_) @callee arguments: (arguments) @args)
            (macro_invocation macro: (_) @callee)
            (type_identifier) @type_ref
            (scoped_identifier) @scoped_ref
            (use_declaration argument: (_) @import_ref)
            "#,
        )
        .expect("rust consume query");

        let ts_src = r#"
            (call_expression function: (_) @callee arguments: (arguments) @args)
            (new_expression constructor: (_) @callee arguments: (arguments)? @args)
            (type_annotation (type_identifier) @type_ref)
            (import_specifier name: (identifier) @import_ref)
            "#;

        let ts = Query::new(
            &ts_language(LanguageKind::TypeScript).expect("ts language"),
            ts_src,
        )
        .expect("ts consume query");

        let tsx = Query::new(
            &ts_language(LanguageKind::Tsx).expect("tsx language"),
            ts_src,
        )
        .expect("tsx consume query");

        let go = Query::new(
            &ts_language(LanguageKind::Go).expect("go language"),
            r#"
            (call_expression function: (_) @callee arguments: (argument_list) @args)
            (type_identifier) @type_ref
            "#,
        )
        .expect("go consume query");

        let python = Query::new(
            &ts_language(LanguageKind::Python).expect("python language"),
            r#"
            (call function: (_) @callee arguments: (argument_list) @args)
            (import_from_statement name: (_) @import_ref)
            "#,
        )
        .expect("python consume query");

        LangQueries {
            rust,
            ts,
            tsx,
            go,
            python,
        }
    });

    match lang {
        LanguageKind::Rust => Some(&qs.rust),
        LanguageKind::TypeScript => Some(&qs.ts),
        LanguageKind::Tsx => Some(&qs.tsx),
        LanguageKind::Go => Some(&qs.go),
        LanguageKind::Python => Some(&qs.python),
        _ => None,
    }
}

fn byte_to_line_col(source: &str, byte_idx: usize) -> (u32, u32) {
    let prefix = &source[..byte_idx.min(source.len())];
    let mut line = 1u32;
    let mut col = 1u32;
    for ch in prefix.chars() {
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

fn count_call_arguments(args_node: Node<'_>) -> usize {
    let mut count = 0usize;
    let mut cursor = args_node.walk();
    for child in args_node.named_children(&mut cursor) {
        if !matches!(
            child.kind(),
            "comment" | "line_comment" | "block_comment" | "html_comment"
        ) {
            count += 1;
        }
    }
    count
}

fn extract_callee_name(node: Node<'_>, source: &str) -> String {
    match node.kind() {
        "field_expression" => node
            .child_by_field_name("field")
            .map(|n| source[n.byte_range()].trim().to_string())
            .unwrap_or_else(|| source[node.byte_range()].trim().to_string()),
        "member_expression" => node
            .child_by_field_name("property")
            .map(|n| source[n.byte_range()].trim().to_string())
            .unwrap_or_else(|| source[node.byte_range()].trim().to_string()),
        "selector_expression" => node
            .child_by_field_name("field")
            .map(|n| source[n.byte_range()].trim().to_string())
            .unwrap_or_else(|| source[node.byte_range()].trim().to_string()),
        "attribute" => node
            .child_by_field_name("attribute")
            .map(|n| source[n.byte_range()].trim().to_string())
            .unwrap_or_else(|| source[node.byte_range()].trim().to_string()),
        _ => source[node.byte_range()]
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(""),
    }
}

fn is_defining_name_node(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if let Some(name_child) = parent.child_by_field_name("name")
        && name_child.id() == node.id()
    {
        return matches!(
            parent.kind(),
            "struct_item"
                | "enum_item"
                | "union_item"
                | "trait_item"
                | "type_item"
                | "associated_type"
                | "type_spec"
                | "type_alias"
                | "class_declaration"
                | "abstract_class_declaration"
                | "interface_declaration"
                | "type_alias_declaration"
        );
    }
    false
}

pub fn extract_file_symbols(
    parsed: &ParsedFile,
    logical_file_id: Blake3Hash,
    file_path: &Path,
    viewport_id: &str,
    now_ns: u64,
) -> ExtractedSymbols {
    let units = extract_decl_units(
        parsed.language,
        &parsed.tree,
        &parsed.source,
        logical_file_id,
    );
    let flat_units: Vec<&DeclUnit> = flatten_decl_units(&units);
    let pending_syntax = parsed.tree.root_node().has_error();

    let mut provides = Vec::with_capacity(flat_units.len());
    let mut provide_entries = Vec::with_capacity(flat_units.len());

    for unit in &flat_units {
        if matches!(unit.kind, SymbolKind::Import | SymbolKind::Export) {
            continue;
        }
        let status = if unit.has_syntax_error {
            SymbolStatus::PendingSyntax
        } else {
            SymbolStatus::Ready
        };
        let (line, column) = byte_to_line_col(&parsed.source, unit.decl_start_byte);
        provides.push(ProvidedSymbol {
            anchor: unit.anchor.clone(),
            kind: unit.kind,
            visibility: unit.visibility,
            normalized_signature: unit.normalized_signature.clone(),
            arity: unit.arity,
            status,
            line,
            column,
        });
        provide_entries.push(SymbolProvideEntry {
            symbol_fqn: unit.anchor.symbol_fqn.clone(),
            viewport_id: viewport_id.to_string(),
            logical_file_id,
            file_path: file_path.to_path_buf(),
            node_anchor: Some(unit.anchor.clone()),
            signature_hash: unit.anchor.decl_header_hash,
            signature_text: unit.normalized_signature.clone(),
            status,
            updated_at_ns: now_ns,
        });
    }

    let mut consumes = Vec::new();
    let mut grouped_consumes: BTreeMap<String, Vec<SymbolLocation>> = BTreeMap::new();
    let mut seen_consume_spans: HashSet<(String, usize, usize)> = HashSet::new();
    let mut seen_grouped_locs: HashSet<(String, u32, u32)> = HashSet::new();

    if let Some(query) = get_query(parsed.language) {
        let mut cursor = QueryCursor::new();
        let src_bytes = parsed.source.as_bytes();
        let capture_names = query.capture_names();
        let mut matches = cursor.matches(query, parsed.tree.root_node(), src_bytes);

        while let Some(m) = matches.next() {
            let mut callee_node: Option<Node<'_>> = None;
            let mut args_node: Option<Node<'_>> = None;
            let mut ref_node: Option<Node<'_>> = None;

            for cap in m.captures() {
                let cname = capture_names[cap.index as usize];
                match cname {
                    "callee" => callee_node = Some(cap.node),
                    "args" => args_node = Some(cap.node),
                    "type_ref" | "scoped_ref" | "import_ref" => ref_node = Some(cap.node),
                    _ => {}
                }
            }

            if let Some(cn) = callee_node {
                let sym_ref = extract_callee_name(cn, &parsed.source);
                if !sym_ref.is_empty() {
                    let in_err = node_in_error_subtree(cn)
                        || args_node.is_some_and(node_in_error_subtree);
                    let pos = cn.start_position();
                    let loc = SymbolLocation {
                        logical_file_id,
                        file_path: file_path.to_path_buf(),
                        line: (pos.row + 1) as u32,
                        column: (pos.column + 1) as u32,
                    };
                    let arity = args_node.map(count_call_arguments);
                    if !in_err
                        && seen_grouped_locs.insert((sym_ref.clone(), loc.line, loc.column))
                    {
                        grouped_consumes
                            .entry(sym_ref.clone())
                            .or_default()
                            .push(loc.clone());
                    }
                    let range = cn.byte_range();
                    if seen_consume_spans.insert((sym_ref.clone(), range.start, range.end)) {
                        consumes.push(ConsumedSymbol {
                            symbol_ref: sym_ref,
                            location: loc,
                            byte_range: range,
                            call_arity: arity,
                            in_error_subtree: in_err,
                        });
                    }
                }
            } else if let Some(rn) = ref_node {
                if is_defining_name_node(rn) {
                    continue;
                }
                let sym_ref = parsed.source[rn.byte_range()]
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join("");
                if !sym_ref.is_empty() {
                    let in_err = node_in_error_subtree(rn);
                    let pos = rn.start_position();
                    let loc = SymbolLocation {
                        logical_file_id,
                        file_path: file_path.to_path_buf(),
                        line: (pos.row + 1) as u32,
                        column: (pos.column + 1) as u32,
                    };
                    if !in_err
                        && seen_grouped_locs.insert((sym_ref.clone(), loc.line, loc.column))
                    {
                        grouped_consumes
                            .entry(sym_ref.clone())
                            .or_default()
                            .push(loc.clone());
                    }
                    let range = rn.byte_range();
                    if seen_consume_spans.insert((sym_ref.clone(), range.start, range.end)) {
                        consumes.push(ConsumedSymbol {
                            symbol_ref: sym_ref,
                            location: loc,
                            byte_range: range,
                            call_arity: None,
                            in_error_subtree: in_err,
                        });
                    }
                }
            }
        }
    }

    if let Some(tags_query) = get_tags_query(parsed.language) {
        let mut cursor = QueryCursor::new();
        let src_bytes = parsed.source.as_bytes();
        let capture_names = tags_query.capture_names();
        let mut matches = cursor.matches(tags_query, parsed.tree.root_node(), src_bytes);

        while let Some(m) = matches.next() {
            let mut has_ref_capture = false;
            let mut name_node: Option<Node<'_>> = None;
            let mut ref_node: Option<Node<'_>> = None;

            for cap in m.captures() {
                let cname = capture_names[cap.index as usize];
                if cname.starts_with("reference.") {
                    has_ref_capture = true;
                    ref_node = Some(cap.node);
                } else if cname == "name" {
                    name_node = Some(cap.node);
                }
            }

            if !has_ref_capture {
                continue;
            }
            let Some(target) = name_node.or(ref_node) else {
                continue;
            };
            if is_defining_name_node(target) || node_in_error_subtree(target) {
                continue;
            }
            let sym_ref = parsed.source[target.byte_range()]
                .split_whitespace()
                .collect::<Vec<_>>()
                .join("");
            if sym_ref.is_empty() {
                continue;
            }
            let pos = target.start_position();
            let loc = SymbolLocation {
                logical_file_id,
                file_path: file_path.to_path_buf(),
                line: (pos.row + 1) as u32,
                column: (pos.column + 1) as u32,
            };
            if seen_grouped_locs.insert((sym_ref.clone(), loc.line, loc.column)) {
                grouped_consumes
                    .entry(sym_ref.clone())
                    .or_default()
                    .push(loc.clone());
            }
            let range = target.byte_range();
            if seen_consume_spans.insert((sym_ref.clone(), range.start, range.end)) {
                consumes.push(ConsumedSymbol {
                    symbol_ref: sym_ref,
                    location: loc,
                    byte_range: range,
                    call_arity: None,
                    in_error_subtree: false,
                });
            }
        }
    }

    let consume_entries = grouped_consumes
        .into_iter()
        .map(|(symbol_fqn, locations)| SymbolConsumeEntry {
            symbol_fqn,
            viewport_id: viewport_id.to_string(),
            locations,
            expected_signature_hash: None,
            updated_at_ns: now_ns,
        })
        .collect();

    ExtractedSymbols {
        provides,
        consumes,
        provide_entries,
        consume_entries,
        pending_syntax,
    }
}

pub fn compute_contract_deltas(
    old_provides: &[ProvidedSymbol],
    new_provides: &[ProvidedSymbol],
) -> Vec<SymbolContractDelta> {
    let mut deltas = Vec::new();
    let mut old_by_key: HashMap<(&str, u16), &ProvidedSymbol> = HashMap::new();
    for p in old_provides {
        if p.status == SymbolStatus::PendingSyntax {
            continue;
        }
        old_by_key.insert((&p.anchor.symbol_fqn, p.anchor.syntax_kind_id), p);
    }

    let mut matched_old = std::collections::HashSet::new();

    for new_p in new_provides {
        if new_p.status == SymbolStatus::PendingSyntax {
            continue;
        }
        let key = (
            new_p.anchor.symbol_fqn.as_str(),
            new_p.anchor.syntax_kind_id,
        );
        if let Some(old_p) = old_by_key.get(&key) {
            matched_old.insert(key);
            if old_p.anchor.decl_header_hash != new_p.anchor.decl_header_hash {
                deltas.push(SymbolContractDelta::SignatureChanged {
                    symbol_fqn: new_p.anchor.symbol_fqn.clone(),
                    old_signature_hash: old_p.anchor.decl_header_hash,
                    new_signature_hash: new_p.anchor.decl_header_hash,
                    old_signature: old_p.normalized_signature.clone(),
                    new_signature: new_p.normalized_signature.clone(),
                });
            }
            if new_p.visibility < old_p.visibility {
                deltas.push(SymbolContractDelta::VisibilityReduced {
                    symbol_fqn: new_p.anchor.symbol_fqn.clone(),
                    detail: format!("{:?} -> {:?}", old_p.visibility, new_p.visibility),
                });
            }
        } else if new_p.visibility != VisibilityKind::Private {
            deltas.push(SymbolContractDelta::AddedSymbol {
                symbol_fqn: new_p.anchor.symbol_fqn.clone(),
                signature_hash: new_p.anchor.decl_header_hash,
                signature_text: new_p.normalized_signature.clone(),
            });
        }
    }

    for old_p in old_provides {
        if old_p.status == SymbolStatus::PendingSyntax {
            continue;
        }
        let key = (
            old_p.anchor.symbol_fqn.as_str(),
            old_p.anchor.syntax_kind_id,
        );
        if !matched_old.contains(&key) {
            deltas.push(SymbolContractDelta::RemovedSymbol {
                symbol_fqn: old_p.anchor.symbol_fqn.clone(),
                previous_hash: old_p.anchor.decl_header_hash,
            });
        }
    }

    deltas
}
