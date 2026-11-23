use super::lang::node_has_syntax_error;
use super::trivia::compute_leading_trivia_start;
use crate::model::{Blake3Hash, LanguageKind, NodeAnchor};
use std::collections::HashMap;
use std::ops::Range;
use tree_sitter::{Node, Tree};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum VisibilityKind {
    Private,
    Protected,
    CrateOrInternal,
    Public,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SymbolKind {
    Function,
    Method,
    Struct,
    Enum,
    Union,
    Trait,
    Impl,
    Module,
    Interface,
    TypeAlias,
    Const,
    Static,
    Var,
    Field,
    EnumVariant,
    Macro,
    Import,
    Export,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclUnit {
    pub anchor: NodeAnchor,
    pub parent_anchor: Option<NodeAnchor>,
    pub language: LanguageKind,
    pub kind: SymbolKind,
    pub visibility: VisibilityKind,
    pub is_container: bool,
    pub is_comma_separated_container: bool,
    pub trivia_start_byte: usize,
    pub decl_start_byte: usize,
    pub end_byte: usize,
    pub header_range: Range<usize>,
    pub body_range: Option<Range<usize>>,
    pub normalized_signature: String,
    pub arity: Option<(usize, usize)>,
    pub has_syntax_error: bool,
    pub body_has_error: bool,
    pub children: Vec<DeclUnit>,
}

pub fn file_root_anchor(logical_file_id: Blake3Hash) -> NodeAnchor {
    NodeAnchor {
        logical_file_id,
        symbol_fqn: "<file>".to_string(),
        syntax_kind_id: 0,
        decl_header_hash: Blake3Hash::ZERO,
    }
}

pub fn extract_decl_units(
    lang: LanguageKind,
    tree: &Tree,
    source: &str,
    logical_file_id: Blake3Hash,
) -> Vec<DeclUnit> {
    let root = tree.root_node();
    match lang {
        LanguageKind::Rust => extract_rust_children(root, source, logical_file_id, "", None),
        LanguageKind::TypeScript | LanguageKind::Tsx => {
            extract_ts_children(lang, root, source, logical_file_id, "", None)
        }
        LanguageKind::Go => {
            let pkg = find_go_package_name(root, source);
            extract_go_children(root, source, logical_file_id, &pkg, None)
        }
        LanguageKind::Python => extract_python_children(root, source, logical_file_id, "", None),
        _ => Vec::new(),
    }
}

pub fn flatten_decl_units(units: &[DeclUnit]) -> Vec<&DeclUnit> {
    let mut out = Vec::new();
    for u in units {
        out.push(u);
        out.extend(flatten_decl_units(&u.children));
    }
    out
}

fn qualify_scope(scope: &str, name: &str) -> String {
    if scope.is_empty() {
        name.to_string()
    } else {
        format!("{scope}::{name}")
    }
}

fn collapse_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn collect_normalized_header_tokens(
    node: Node<'_>,
    decl_start_byte: usize,
    header_end_byte: usize,
    source: &str,
    tokens: &mut Vec<String>,
) {
    if node.end_byte() <= decl_start_byte || node.start_byte() >= header_end_byte {
        return;
    }
    let kind = node.kind();
    if matches!(
        kind,
        "comment"
            | "line_comment"
            | "block_comment"
            | "html_comment"
            | "decorator"
            | "visibility_modifier"
            | "accessibility_modifier"
    ) {
        return;
    }
    if node.child_count() == 0 {
        let start = node.start_byte().max(decl_start_byte);
        let end = node.end_byte().min(header_end_byte);
        if start < end {
            let tok = source[start..end].trim();
            if !tok.is_empty() && tok != ";" {
                tokens.push(tok.to_string());
            }
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_normalized_header_tokens(child, decl_start_byte, header_end_byte, source, tokens);
    }
}

fn compute_normalized_signature_and_hash(
    inner_node: Node<'_>,
    decl_start_byte: usize,
    header_end_byte: usize,
    source: &str,
) -> (String, Blake3Hash) {
    let mut tokens = Vec::new();
    collect_normalized_header_tokens(
        inner_node,
        decl_start_byte,
        header_end_byte,
        source,
        &mut tokens,
    );
    let sig = tokens.join(" ");
    let hash = Blake3Hash::from_bytes(sig.as_bytes());
    (sig, hash)
}

fn find_child_by_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.children(&mut cursor).find(|child| child.kind() == kind)
}

// ============================================================================
// Rust
// ============================================================================

fn extract_rust_children(
    container: Node<'_>,
    source: &str,
    logical_file_id: Blake3Hash,
    scope: &str,
    parent_anchor: Option<NodeAnchor>,
) -> Vec<DeclUnit> {
    let mut units = Vec::new();
    let mut impl_counts: HashMap<String, usize> = HashMap::new();
    let mut cursor = container.walk();
    for child in container.named_children(&mut cursor) {
        if let Some(unit) = extract_rust_node(
            child,
            source,
            logical_file_id,
            scope,
            parent_anchor.clone(),
            &mut impl_counts,
        ) {
            units.push(unit);
        }
    }
    units
}

fn rust_visibility(node: Node<'_>, source: &str) -> VisibilityKind {
    if let Some(vis) = find_child_by_kind(node, "visibility_modifier") {
        let text = collapse_ws(&source[vis.byte_range()]);
        if text == "pub" {
            VisibilityKind::Public
        } else {
            VisibilityKind::CrateOrInternal
        }
    } else {
        VisibilityKind::Private
    }
}

fn rust_arity(node: Node<'_>) -> Option<(usize, usize)> {
    let params = node.child_by_field_name("parameters")?;
    let mut min_p = 0usize;
    let mut max_p = 0usize;
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        match child.kind() {
            "self_parameter" | "line_comment" | "block_comment" => {}
            "variadic_parameter" => {
                max_p = usize::MAX;
            }
            _ => {
                min_p += 1;
                if max_p != usize::MAX {
                    max_p += 1;
                }
            }
        }
    }
    Some((min_p, max_p))
}

fn extract_rust_node(
    node: Node<'_>,
    source: &str,
    logical_file_id: Blake3Hash,
    scope: &str,
    parent_anchor: Option<NodeAnchor>,
    impl_counts: &mut HashMap<String, usize>,
) -> Option<DeclUnit> {
    let kind_str = node.kind();
    let (sym_kind, raw_name, body_node, is_container, is_comma_sep, visibility, arity) =
        match kind_str {
            "function_item" => {
                let name = node
                    .child_by_field_name("name")
                    .map(|n| source[n.byte_range()].to_string())?;
                (
                    SymbolKind::Function,
                    name,
                    node.child_by_field_name("body"),
                    false,
                    false,
                    rust_visibility(node, source),
                    rust_arity(node),
                )
            }
            "function_signature_item" => {
                let name = node
                    .child_by_field_name("name")
                    .map(|n| source[n.byte_range()].to_string())?;
                (
                    SymbolKind::Function,
                    name,
                    None,
                    false,
                    false,
                    rust_visibility(node, source),
                    rust_arity(node),
                )
            }
            "struct_item" => {
                let name = node
                    .child_by_field_name("name")
                    .map(|n| source[n.byte_range()].to_string())?;
                let body = node.child_by_field_name("body");
                let is_field_list = body
                    .map(|b| b.kind() == "field_declaration_list")
                    .unwrap_or(false);
                (
                    SymbolKind::Struct,
                    name,
                    if is_field_list { body } else { None },
                    is_field_list,
                    is_field_list,
                    rust_visibility(node, source),
                    None,
                )
            }
            "union_item" => {
                let name = node
                    .child_by_field_name("name")
                    .map(|n| source[n.byte_range()].to_string())?;
                let body = node.child_by_field_name("body");
                (
                    SymbolKind::Union,
                    name,
                    body,
                    body.is_some(),
                    true,
                    rust_visibility(node, source),
                    None,
                )
            }
            "enum_item" => {
                let name = node
                    .child_by_field_name("name")
                    .map(|n| source[n.byte_range()].to_string())?;
                let body = node.child_by_field_name("body");
                (
                    SymbolKind::Enum,
                    name,
                    body,
                    body.is_some(),
                    true,
                    rust_visibility(node, source),
                    None,
                )
            }
            "trait_item" => {
                let name = node
                    .child_by_field_name("name")
                    .map(|n| source[n.byte_range()].to_string())?;
                let body = node.child_by_field_name("body");
                (
                    SymbolKind::Trait,
                    name,
                    body,
                    body.is_some(),
                    false,
                    rust_visibility(node, source),
                    None,
                )
            }
            "impl_item" => {
                let ty = node
                    .child_by_field_name("type")
                    .map(|n| collapse_ws(&source[n.byte_range()]))
                    .unwrap_or_else(|| "Unknown".to_string());
                let mut has_bang = false;
                let mut c = node.walk();
                for ch in node.children(&mut c) {
                    if ch.kind() == "!" {
                        has_bang = true;
                    }
                    if ch.kind() == "for" {
                        break;
                    }
                }
                let base = if let Some(tr) = node.child_by_field_name("trait") {
                    let neg = if has_bang { "!" } else { "" };
                    format!(
                        "impl<{}{} for {}>",
                        neg,
                        collapse_ws(&source[tr.byte_range()]),
                        ty
                    )
                } else {
                    format!("impl<{ty}>")
                };
                let count = impl_counts
                    .entry(base.clone())
                    .and_modify(|e| *e += 1)
                    .or_insert(1);
                let disambiguated = if *count > 1 {
                    format!("{base}#{count}")
                } else {
                    base
                };
                let body = node.child_by_field_name("body");
                (
                    SymbolKind::Impl,
                    disambiguated,
                    body,
                    body.is_some(),
                    false,
                    VisibilityKind::Public,
                    None,
                )
            }
            "mod_item" => {
                let name = node
                    .child_by_field_name("name")
                    .map(|n| source[n.byte_range()].to_string())?;
                let body = node.child_by_field_name("body");
                (
                    SymbolKind::Module,
                    name,
                    body,
                    body.is_some(),
                    false,
                    rust_visibility(node, source),
                    None,
                )
            }
            "type_item" | "associated_type" => {
                let name = node
                    .child_by_field_name("name")
                    .map(|n| source[n.byte_range()].to_string())?;
                (
                    SymbolKind::TypeAlias,
                    name,
                    None,
                    false,
                    false,
                    rust_visibility(node, source),
                    None,
                )
            }
            "const_item" => {
                let name = node
                    .child_by_field_name("name")
                    .map(|n| source[n.byte_range()].to_string())?;
                (
                    SymbolKind::Const,
                    name,
                    None,
                    false,
                    false,
                    rust_visibility(node, source),
                    None,
                )
            }
            "static_item" => {
                let name = node
                    .child_by_field_name("name")
                    .map(|n| source[n.byte_range()].to_string())?;
                (
                    SymbolKind::Static,
                    name,
                    None,
                    false,
                    false,
                    rust_visibility(node, source),
                    None,
                )
            }
            "macro_definition" => {
                let name = node
                    .child_by_field_name("name")
                    .map(|n| source[n.byte_range()].to_string())?;
                (
                    SymbolKind::Macro,
                    name,
                    None,
                    false,
                    false,
                    VisibilityKind::Public,
                    None,
                )
            }
            "use_declaration" => {
                let arg = node
                    .child_by_field_name("argument")
                    .map(|n| collapse_ws(&source[n.byte_range()]))
                    .unwrap_or_else(|| collapse_ws(&source[node.byte_range()]));
                (
                    SymbolKind::Import,
                    format!("use::{arg}"),
                    None,
                    false,
                    false,
                    rust_visibility(node, source),
                    None,
                )
            }
            "field_declaration" => {
                let name = node
                    .child_by_field_name("name")
                    .map(|n| source[n.byte_range()].to_string())?;
                (
                    SymbolKind::Field,
                    name,
                    None,
                    false,
                    false,
                    rust_visibility(node, source),
                    None,
                )
            }
            "enum_variant" => {
                let name = node
                    .child_by_field_name("name")
                    .map(|n| source[n.byte_range()].to_string())?;
                (
                    SymbolKind::EnumVariant,
                    name,
                    None,
                    false,
                    false,
                    VisibilityKind::Public,
                    None,
                )
            }
            _ => return None,
        };

    let fqn = qualify_scope(scope, &raw_name);
    let trivia_start_byte = compute_leading_trivia_start(LanguageKind::Rust, source, node);
    let decl_start_byte = node.start_byte();
    let end_byte = node.end_byte();
    let header_end_byte = body_node.map(|b| b.start_byte()).unwrap_or(end_byte);
    let header_range = trivia_start_byte..header_end_byte;
    let body_range = body_node.map(|b| b.byte_range());

    let (normalized_signature, decl_header_hash) =
        compute_normalized_signature_and_hash(node, decl_start_byte, header_end_byte, source);

    let anchor = NodeAnchor {
        logical_file_id,
        symbol_fqn: fqn.clone(),
        syntax_kind_id: node.kind_id(),
        decl_header_hash,
    };

    let body_has_error = body_node.map(node_has_syntax_error).unwrap_or(false);
    let has_syntax_error = node.is_error()
        || node.is_missing()
        || (if body_node.is_some() {
            // Check header specifically for syntax errors if body exists
            let mut cursor = node.walk();
            node.children(&mut cursor)
                .filter(|c| Some(*c) != body_node)
                .any(node_has_syntax_error)
        } else {
            node_has_syntax_error(node)
        });

    let children = if is_container {
        if let Some(body) = body_node {
            extract_rust_children(body, source, logical_file_id, &fqn, Some(anchor.clone()))
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };

    Some(DeclUnit {
        anchor,
        parent_anchor,
        language: LanguageKind::Rust,
        kind: sym_kind,
        visibility,
        is_container,
        is_comma_separated_container: is_comma_sep,
        trivia_start_byte,
        decl_start_byte,
        end_byte,
        header_range,
        body_range,
        normalized_signature,
        arity,
        has_syntax_error,
        body_has_error,
        children,
    })
}

// ============================================================================
// TypeScript / TSX
// ============================================================================

fn extract_ts_children(
    lang: LanguageKind,
    container: Node<'_>,
    source: &str,
    logical_file_id: Blake3Hash,
    scope: &str,
    parent_anchor: Option<NodeAnchor>,
) -> Vec<DeclUnit> {
    let mut units = Vec::new();
    let mut overload_counts: HashMap<String, usize> = HashMap::new();
    let mut cursor = container.walk();
    for child in container.named_children(&mut cursor) {
        if let Some(unit) = extract_ts_node(
            lang,
            child,
            source,
            logical_file_id,
            scope,
            parent_anchor.clone(),
            &mut overload_counts,
        ) {
            units.push(unit);
        }
    }
    units
}

fn ts_member_visibility(node: Node<'_>, source: &str) -> VisibilityKind {
    if let Some(acc) = find_child_by_kind(node, "accessibility_modifier") {
        match source[acc.byte_range()].trim() {
            "private" => VisibilityKind::Private,
            "protected" => VisibilityKind::Protected,
            _ => VisibilityKind::Public,
        }
    } else {
        VisibilityKind::Public
    }
}

fn ts_arity(func_node: Node<'_>) -> Option<(usize, usize)> {
    let params = func_node.child_by_field_name("parameters")?;
    let mut min_p = 0usize;
    let mut max_p = 0usize;
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        match child.kind() {
            "required_parameter" => {
                let has_rest = child
                    .child_by_field_name("pattern")
                    .map(|p| p.kind() == "rest_pattern")
                    .unwrap_or(false)
                    || find_child_by_kind(child, "rest_pattern").is_some();
                if has_rest {
                    max_p = usize::MAX;
                } else {
                    min_p += 1;
                    if max_p != usize::MAX {
                        max_p += 1;
                    }
                }
            }
            "optional_parameter" => {
                if max_p != usize::MAX {
                    max_p += 1;
                }
            }
            "rest_parameter" => {
                max_p = usize::MAX;
            }
            _ => {}
        }
    }
    Some((min_p, max_p))
}

fn ts_first_non_decorator_start(node: Node<'_>) -> usize {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "decorator" {
            return child.start_byte();
        }
    }
    node.start_byte()
}

struct TsLexVarInfo<'a> {
    name: String,
    arrow_body: Option<Node<'a>>,
    arity: Option<(usize, usize)>,
}

fn ts_lex_var_info<'a>(node: Node<'a>, source: &str) -> Option<TsLexVarInfo<'a>> {
    let mut names = Vec::new();
    let mut arrow_body = None;
    let mut arity = None;
    let mut declarator_count = 0usize;
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "variable_declarator" {
            declarator_count += 1;
            if let Some(name_node) = child.child_by_field_name("name") {
                names.push(collapse_ws(&source[name_node.byte_range()]));
            }
            if declarator_count == 1
                && let Some(val) = child.child_by_field_name("value")
                && matches!(
                    val.kind(),
                    "arrow_function" | "function_expression" | "generator_function"
                )
            {
                arity = ts_arity(val).or_else(|| {
                    val.child_by_field_name("parameter").map(|_| (1, 1))
                });
                if let Some(body) = val.child_by_field_name("body")
                    && body.kind() == "statement_block"
                {
                    arrow_body = Some(body);
                }
            } else {
                arrow_body = None;
                arity = None;
            }
        }
    }
    if names.is_empty() {
        None
    } else {
        Some(TsLexVarInfo {
            name: names.join(","),
            arrow_body,
            arity,
        })
    }
}

fn extract_ts_node(
    lang: LanguageKind,
    outer_node: Node<'_>,
    source: &str,
    logical_file_id: Blake3Hash,
    scope: &str,
    parent_anchor: Option<NodeAnchor>,
    overload_counts: &mut HashMap<String, usize>,
) -> Option<DeclUnit> {
    let is_exported = outer_node.kind() == "export_statement";
    let mut cur_node = if is_exported {
        if let Some(decl) = outer_node.child_by_field_name("declaration") {
            decl
        } else if let Some(val) = outer_node.child_by_field_name("value") {
            if matches!(
                val.kind(),
                "class" | "function_expression" | "generator_function"
            ) {
                val
            } else {
                outer_node
            }
        } else {
            outer_node
        }
    } else {
        outer_node
    };

    if cur_node.kind() == "expression_statement" {
        let mut cursor = cur_node.walk();
        if let Some(child) = cur_node
            .named_children(&mut cursor)
            .find(|c| !matches!(c.kind(), "comment" | "html_comment"))
            && matches!(child.kind(), "internal_module" | "module")
        {
            cur_node = child;
        }
    }

    let mut is_ambient = false;
    let mut ambient_node_opt: Option<Node<'_>> = None;
    if cur_node.kind() == "ambient_declaration" {
        let mut cursor = cur_node.walk();
        if let Some(child) = cur_node
            .named_children(&mut cursor)
            .find(|c| !matches!(c.kind(), "comment" | "html_comment"))
        {
            is_ambient = true;
            ambient_node_opt = Some(cur_node);
            cur_node = child;
        }
    }

    let inner_node = cur_node;

    let kind_str = inner_node.kind();
    let mut is_comma_sep = false;
    let (sym_kind, raw_name, body_node, is_container, visibility, arity) = match kind_str {
        "function_declaration" | "generator_function_declaration" | "function_expression" => {
            let name = inner_node
                .child_by_field_name("name")
                .map(|n| source[n.byte_range()].to_string())
                .unwrap_or_else(|| "default".to_string());
            let vis = if is_exported {
                VisibilityKind::Public
            } else {
                VisibilityKind::Private
            };
            (
                SymbolKind::Function,
                name,
                inner_node.child_by_field_name("body"),
                false,
                vis,
                ts_arity(inner_node),
            )
        }
        "function_signature" => {
            let name = inner_node
                .child_by_field_name("name")
                .map(|n| source[n.byte_range()].to_string())?;
            let count = overload_counts
                .entry(name.clone())
                .and_modify(|e| *e += 1)
                .or_insert(1);
            let fqn_name = if *count > 1 {
                format!("{name}#sig:{count}")
            } else {
                name
            };
            let vis = if is_exported {
                VisibilityKind::Public
            } else {
                VisibilityKind::Private
            };
            (
                SymbolKind::Function,
                fqn_name,
                None,
                false,
                vis,
                ts_arity(inner_node),
            )
        }
        "class_declaration" | "abstract_class_declaration" | "class" => {
            let name = inner_node
                .child_by_field_name("name")
                .map(|n| source[n.byte_range()].to_string())
                .unwrap_or_else(|| "default".to_string());
            let body = inner_node.child_by_field_name("body");
            let vis = if is_exported {
                VisibilityKind::Public
            } else {
                VisibilityKind::Private
            };
            (
                SymbolKind::Struct,
                name,
                body,
                body.is_some(),
                vis,
                None,
            )
        }
        "interface_declaration" => {
            let name = inner_node
                .child_by_field_name("name")
                .map(|n| source[n.byte_range()].to_string())?;
            let body = inner_node.child_by_field_name("body");
            let vis = if is_exported {
                VisibilityKind::Public
            } else {
                VisibilityKind::Private
            };
            (
                SymbolKind::Interface,
                name,
                body,
                body.is_some(),
                vis,
                None,
            )
        }
        "type_alias_declaration" => {
            let name = inner_node
                .child_by_field_name("name")
                .map(|n| source[n.byte_range()].to_string())?;
            let vis = if is_exported {
                VisibilityKind::Public
            } else {
                VisibilityKind::Private
            };
            (SymbolKind::TypeAlias, name, None, false, vis, None)
        }
        "enum_declaration" => {
            let name = inner_node
                .child_by_field_name("name")
                .map(|n| source[n.byte_range()].to_string())?;
            let body = inner_node.child_by_field_name("body");
            is_comma_sep = body.is_some();
            let vis = if is_exported {
                VisibilityKind::Public
            } else {
                VisibilityKind::Private
            };
            (
                SymbolKind::Enum,
                name,
                body,
                body.is_some(),
                vis,
                None,
            )
        }
        "internal_module" | "module" => {
            let name = inner_node
                .child_by_field_name("name")
                .map(|n| collapse_ws(&source[n.byte_range()]))?;
            let body = inner_node.child_by_field_name("body");
            let vis = if is_exported {
                VisibilityKind::Public
            } else {
                VisibilityKind::Private
            };
            (
                SymbolKind::Module,
                name,
                body,
                body.is_some(),
                vis,
                None,
            )
        }
        "statement_block" if is_ambient => (
            SymbolKind::Module,
            "global".to_string(),
            Some(inner_node),
            true,
            VisibilityKind::Public,
            None,
        ),
        "method_definition" => {
            let raw = inner_node
                .child_by_field_name("name")
                .map(|n| collapse_ws(&source[n.byte_range()]))?;
            let mut is_static = false;
            let mut accessor = "";
            let mut cursor = inner_node.walk();
            for ch in inner_node.children(&mut cursor) {
                if !ch.is_named() {
                    match ch.kind() {
                        "static" => is_static = true,
                        "get" => accessor = "get:",
                        "set" => accessor = "set:",
                        _ => {}
                    }
                }
            }
            let static_prefix = if is_static { "static:" } else { "" };
            (
                SymbolKind::Method,
                format!("{static_prefix}{accessor}{raw}"),
                inner_node.child_by_field_name("body"),
                false,
                ts_member_visibility(inner_node, source),
                ts_arity(inner_node),
            )
        }
        "method_signature" | "abstract_method_signature" => {
            let raw = inner_node
                .child_by_field_name("name")
                .map(|n| collapse_ws(&source[n.byte_range()]))?;
            let mut is_static = false;
            let mut accessor = "";
            let mut cursor = inner_node.walk();
            for ch in inner_node.children(&mut cursor) {
                if !ch.is_named() {
                    match ch.kind() {
                        "static" => is_static = true,
                        "get" => accessor = "get:",
                        "set" => accessor = "set:",
                        _ => {}
                    }
                }
            }
            let static_prefix = if is_static { "static:" } else { "" };
            let name = format!("{static_prefix}{accessor}{raw}");
            let count = overload_counts
                .entry(name.clone())
                .and_modify(|e| *e += 1)
                .or_insert(1);
            let fqn_name = if *count > 1 {
                format!("{name}#sig:{count}")
            } else {
                name
            };
            (
                SymbolKind::Method,
                fqn_name,
                None,
                false,
                ts_member_visibility(inner_node, source),
                ts_arity(inner_node),
            )
        }
        "public_field_definition" | "property_signature" => {
            let raw = inner_node
                .child_by_field_name("name")
                .map(|n| collapse_ws(&source[n.byte_range()]))?;
            let mut is_static = false;
            let mut cursor = inner_node.walk();
            for ch in inner_node.children(&mut cursor) {
                if !ch.is_named() && ch.kind() == "static" {
                    is_static = true;
                    break;
                }
            }
            let name = if is_static {
                format!("static:{raw}")
            } else {
                raw
            };
            (
                SymbolKind::Field,
                name,
                None,
                false,
                ts_member_visibility(inner_node, source),
                None,
            )
        }
        "enum_assignment" => {
            let name = inner_node
                .child_by_field_name("name")
                .map(|n| collapse_ws(&source[n.byte_range()]))?;
            (
                SymbolKind::EnumVariant,
                name,
                None,
                false,
                VisibilityKind::Public,
                None,
            )
        }
        "property_identifier" => {
            // Bare enum member inside enum_body
            let parent_is_enum_body = inner_node
                .parent()
                .map(|p| p.kind() == "enum_body")
                .unwrap_or(false);
            if !parent_is_enum_body {
                return None;
            }
            let name = source[inner_node.byte_range()].to_string();
            (
                SymbolKind::EnumVariant,
                name,
                None,
                false,
                VisibilityKind::Public,
                None,
            )
        }
        "lexical_declaration" | "variable_declaration" => {
            let info = ts_lex_var_info(inner_node, source)?;
            let vis = if is_exported {
                VisibilityKind::Public
            } else {
                VisibilityKind::Private
            };
            let sk = if info.arity.is_some() {
                SymbolKind::Function
            } else {
                SymbolKind::Const
            };
            (sk, info.name, info.arrow_body, false, vis, info.arity)
        }
        "import_statement" => {
            let norm = collapse_ws(&source[inner_node.byte_range()]);
            (
                SymbolKind::Import,
                format!("import::{norm}"),
                None,
                false,
                VisibilityKind::Private,
                None,
            )
        }
        "export_statement" => {
            let norm = collapse_ws(&source[inner_node.byte_range()]);
            (
                SymbolKind::Export,
                format!("export::{norm}"),
                None,
                false,
                VisibilityKind::Public,
                None,
            )
        }
        _ => return None,
    };

    let fqn = qualify_scope(scope, &raw_name);
    let trivia_start_byte = compute_leading_trivia_start(lang, source, outer_node);
    let decl_start_byte = ambient_node_opt
        .map(|n| n.start_byte())
        .unwrap_or_else(|| ts_first_non_decorator_start(inner_node));
    let mut end_byte = outer_node.end_byte();
    if matches!(
        inner_node.kind(),
        "method_signature"
            | "abstract_method_signature"
            | "property_signature"
            | "public_field_definition"
    ) && outer_node.parent().is_some_and(|p| {
        matches!(p.kind(), "interface_body" | "class_body" | "object_type")
    }) && let Some(sib) = outer_node.next_sibling()
        && !sib.is_named()
        && (sib.kind() == ";"
            || (sib.kind() == ","
                && outer_node
                    .parent()
                    .is_some_and(|p| matches!(p.kind(), "interface_body" | "object_type"))))
    {
        end_byte = sib.end_byte();
    }
    let header_end_byte = body_node
        .map(|b| b.start_byte())
        .unwrap_or(inner_node.end_byte());
    let header_range = trivia_start_byte..header_end_byte;
    let body_range = body_node.map(|b| b.byte_range());

    let sig_node = ambient_node_opt.unwrap_or(inner_node);
    let (normalized_signature, decl_header_hash) = compute_normalized_signature_and_hash(
        sig_node,
        decl_start_byte,
        header_end_byte,
        source,
    );

    let anchor = NodeAnchor {
        logical_file_id,
        symbol_fqn: fqn.clone(),
        syntax_kind_id: inner_node.kind_id(),
        decl_header_hash,
    };

    let ambient_header_error = ambient_node_opt.is_some_and(|amb| {
        amb.is_error()
            || amb.is_missing()
            || {
                let mut cursor = amb.walk();
                amb.children(&mut cursor)
                    .filter(|c| *c != inner_node)
                    .any(node_has_syntax_error)
            }
    });
    let body_has_error = body_node.map(node_has_syntax_error).unwrap_or(false);
    let has_syntax_error = ambient_header_error
        || inner_node.is_error()
        || inner_node.is_missing()
        || (if let Some(bn) = body_node {
            if bn == inner_node {
                false
            } else {
                let mut cursor = inner_node.walk();
                inner_node
                    .children(&mut cursor)
                    .filter(|c| *c != bn)
                    .any(node_has_syntax_error)
            }
        } else {
            node_has_syntax_error(inner_node)
        });

    let children = if is_container {
        if let Some(body) = body_node {
            extract_ts_children(
                lang,
                body,
                source,
                logical_file_id,
                &fqn,
                Some(anchor.clone()),
            )
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };

    Some(DeclUnit {
        anchor,
        parent_anchor,
        language: lang,
        kind: sym_kind,
        visibility,
        is_container,
        is_comma_separated_container: is_comma_sep,
        trivia_start_byte,
        decl_start_byte,
        end_byte,
        header_range,
        body_range,
        normalized_signature,
        arity,
        has_syntax_error,
        body_has_error,
        children,
    })
}

// ============================================================================
// Go
// ============================================================================

fn find_go_package_name(root: Node<'_>, source: &str) -> String {
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() == "package_clause" {
            let mut c2 = child.walk();
            for sub in child.named_children(&mut c2) {
                if sub.kind() == "package_identifier" {
                    return source[sub.byte_range()].trim().to_string();
                }
            }
        }
    }
    String::new()
}

fn go_visibility(name: &str) -> VisibilityKind {
    if name.chars().next().is_some_and(|c| c.is_uppercase()) {
        VisibilityKind::Public
    } else {
        VisibilityKind::Private
    }
}

fn go_arity(node: Node<'_>) -> Option<(usize, usize)> {
    let params = node.child_by_field_name("parameters")?;
    let mut min_p = 0usize;
    let mut max_p = 0usize;
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        match child.kind() {
            "parameter_declaration" => {
                let mut name_count = 0usize;
                let mut c2 = child.walk();
                for sub in child.children_by_field_name("name", &mut c2) {
                    let _ = sub;
                    name_count += 1;
                }
                let n = name_count.max(1);
                min_p += n;
                if max_p != usize::MAX {
                    max_p += n;
                }
            }
            "variadic_parameter_declaration" => {
                max_p = usize::MAX;
            }
            _ => {}
        }
    }
    Some((min_p, max_p))
}

fn go_strip_type_args(ty_node: Node<'_>, source: &str) -> String {
    match ty_node.kind() {
        "generic_type" => ty_node
            .child_by_field_name("type")
            .map(|t| go_strip_type_args(t, source))
            .unwrap_or_else(|| collapse_ws(&source[ty_node.byte_range()])),
        "qualified_type" => collapse_ws(&source[ty_node.byte_range()]),
        _ => collapse_ws(&source[ty_node.byte_range()]),
    }
}

fn go_receiver_prefix(method_node: Node<'_>, source: &str) -> String {
    let Some(recv) = method_node.child_by_field_name("receiver") else {
        return "()".to_string();
    };
    let mut cursor = recv.walk();
    for child in recv.named_children(&mut cursor) {
        if child.kind() == "parameter_declaration"
            && let Some(ty) = child.child_by_field_name("type")
        {
            if ty.kind() == "pointer_type" {
                let mut c2 = ty.walk();
                if let Some(inner) = ty.named_children(&mut c2).next() {
                    return format!("(*{})", go_strip_type_args(inner, source));
                }
            }
            return format!("({})", go_strip_type_args(ty, source));
        }
    }
    "()".to_string()
}

#[derive(Default)]
struct GoGroupCounters {
    const_group_idx: usize,
    var_group_idx: usize,
    type_group_idx: usize,
    import_group_idx: usize,
}

fn extract_go_children(
    container: Node<'_>,
    source: &str,
    logical_file_id: Blake3Hash,
    scope: &str,
    parent_anchor: Option<NodeAnchor>,
) -> Vec<DeclUnit> {
    let mut units = Vec::new();
    let mut group_counters = GoGroupCounters::default();
    let mut cursor = container.walk();
    for child in container.named_children(&mut cursor) {
        if let Some(unit) = extract_go_node(
            child,
            source,
            logical_file_id,
            scope,
            parent_anchor.clone(),
            &mut group_counters,
        ) {
            units.push(unit);
        }
    }
    units
}

fn extract_go_node(
    outer_node: Node<'_>,
    source: &str,
    logical_file_id: Blake3Hash,
    scope: &str,
    parent_anchor: Option<NodeAnchor>,
    group_counters: &mut GoGroupCounters,
) -> Option<DeclUnit> {
    let kind_str = outer_node.kind();
    match kind_str {
        "import_declaration" => {
            let has_parens = {
                let mut c = outer_node.walk();
                outer_node
                    .children(&mut c)
                    .any(|ch| ch.kind() == "(" || ch.kind() == "import_spec_list")
            };
            if !has_parens {
                let mut c = outer_node.walk();
                let spec = outer_node
                    .named_children(&mut c)
                    .find(|ch| ch.kind() == "import_spec")?;
                let spec_name = format!("import::{}", collapse_ws(&source[spec.byte_range()]));
                let fqn = qualify_scope(scope, &spec_name);
                Some(build_go_unit(
                    outer_node,
                    spec,
                    source,
                    logical_file_id,
                    parent_anchor,
                    SymbolKind::Import,
                    fqn,
                    VisibilityKind::Private,
                    None,
                    false,
                    None,
                ))
            } else {
                group_counters.import_group_idx += 1;
                let idx = group_counters.import_group_idx;
                let fqn = qualify_scope(scope, &format!("import_group#{idx}"));
                let trivia_start =
                    compute_leading_trivia_start(LanguageKind::Go, source, outer_node);
                let decl_start = outer_node.start_byte();
                let end_byte = outer_node.end_byte();
                let container_node = find_child_by_kind(outer_node, "import_spec_list")
                    .or_else(|| find_child_by_kind(outer_node, "("));
                let header_end = container_node.map(|n| n.start_byte()).unwrap_or(end_byte);
                let (sig, hash) = compute_normalized_signature_and_hash(
                    outer_node,
                    decl_start,
                    header_end,
                    source,
                );
                let anchor = NodeAnchor {
                    logical_file_id,
                    symbol_fqn: fqn,
                    syntax_kind_id: outer_node.kind_id(),
                    decl_header_hash: hash,
                };
                let spec_parent =
                    find_child_by_kind(outer_node, "import_spec_list").unwrap_or(outer_node);
                let mut children = Vec::new();
                let mut c = spec_parent.walk();
                for spec in spec_parent.named_children(&mut c) {
                    if spec.kind() == "import_spec" {
                        let spec_name =
                            format!("import::{}", collapse_ws(&source[spec.byte_range()]));
                        let child_fqn = qualify_scope(scope, &spec_name);
                        children.push(build_go_unit(
                            spec,
                            spec,
                            source,
                            logical_file_id,
                            Some(anchor.clone()),
                            SymbolKind::Import,
                            child_fqn,
                            VisibilityKind::Private,
                            None,
                            false,
                            None,
                        ));
                    }
                }
                Some(DeclUnit {
                    anchor,
                    parent_anchor,
                    language: LanguageKind::Go,
                    kind: SymbolKind::Import,
                    visibility: VisibilityKind::Private,
                    is_container: true,
                    is_comma_separated_container: false,
                    trivia_start_byte: trivia_start,
                    decl_start_byte: decl_start,
                    end_byte,
                    header_range: trivia_start..header_end,
                    body_range: container_node.map(|p| p.start_byte()..end_byte),
                    normalized_signature: sig,
                    arity: None,
                    has_syntax_error: node_has_syntax_error(outer_node),
                    body_has_error: false,
                    children,
                })
            }
        }
        "function_declaration" => {
            let name = outer_node
                .child_by_field_name("name")
                .map(|n| source[n.byte_range()].to_string())?;
            let vis = go_visibility(&name);
            let fqn = qualify_scope(scope, &name);
            let body = outer_node.child_by_field_name("body");
            Some(build_go_unit(
                outer_node,
                outer_node,
                source,
                logical_file_id,
                parent_anchor,
                SymbolKind::Function,
                fqn,
                vis,
                body,
                false,
                go_arity(outer_node),
            ))
        }
        "method_declaration" => {
            let name = outer_node
                .child_by_field_name("name")
                .map(|n| source[n.byte_range()].to_string())?;
            let vis = go_visibility(&name);
            let recv = go_receiver_prefix(outer_node, source);
            let fqn = qualify_scope(scope, &format!("{recv}.{name}"));
            let body = outer_node.child_by_field_name("body");
            Some(build_go_unit(
                outer_node,
                outer_node,
                source,
                logical_file_id,
                parent_anchor,
                SymbolKind::Method,
                fqn,
                vis,
                body,
                false,
                go_arity(outer_node),
            ))
        }
        "type_declaration" => {
            let has_parens = {
                let mut c = outer_node.walk();
                outer_node.children(&mut c).any(|ch| ch.kind() == "(")
            };
            if !has_parens {
                let mut c = outer_node.walk();
                let spec = outer_node
                    .named_children(&mut c)
                    .find(|ch| ch.kind() == "type_spec" || ch.kind() == "type_alias")?;
                extract_go_type_spec(
                    outer_node,
                    spec,
                    source,
                    logical_file_id,
                    scope,
                    parent_anchor,
                )
            } else {
                group_counters.type_group_idx += 1;
                let idx = group_counters.type_group_idx;
                let fqn = qualify_scope(scope, &format!("type_group#{idx}"));
                let trivia_start =
                    compute_leading_trivia_start(LanguageKind::Go, source, outer_node);
                let decl_start = outer_node.start_byte();
                let end_byte = outer_node.end_byte();
                let open_paren = find_child_by_kind(outer_node, "(");
                let header_end = open_paren.map(|n| n.start_byte()).unwrap_or(end_byte);
                let (sig, hash) = compute_normalized_signature_and_hash(
                    outer_node,
                    decl_start,
                    header_end,
                    source,
                );
                let anchor = NodeAnchor {
                    logical_file_id,
                    symbol_fqn: fqn,
                    syntax_kind_id: outer_node.kind_id(),
                    decl_header_hash: hash,
                };
                let mut children = Vec::new();
                let mut c = outer_node.walk();
                for spec in outer_node.named_children(&mut c) {
                    if matches!(spec.kind(), "type_spec" | "type_alias")
                        && let Some(u) = extract_go_type_spec(
                            spec,
                            spec,
                            source,
                            logical_file_id,
                            scope,
                            Some(anchor.clone()),
                        )
                    {
                        children.push(u);
                    }
                }
                Some(DeclUnit {
                    anchor,
                    parent_anchor,
                    language: LanguageKind::Go,
                    kind: SymbolKind::TypeAlias,
                    visibility: VisibilityKind::Private,
                    is_container: true,
                    is_comma_separated_container: false,
                    trivia_start_byte: trivia_start,
                    decl_start_byte: decl_start,
                    end_byte,
                    header_range: trivia_start..header_end,
                    body_range: open_paren.map(|p| p.start_byte()..end_byte),
                    normalized_signature: sig,
                    arity: None,
                    has_syntax_error: node_has_syntax_error(outer_node),
                    body_has_error: false,
                    children,
                })
            }
        }
        "const_declaration" | "var_declaration" => {
            let sym_kind = if kind_str == "const_declaration" {
                SymbolKind::Const
            } else {
                SymbolKind::Var
            };
            let has_parens = {
                let mut c = outer_node.walk();
                outer_node
                    .children(&mut c)
                    .any(|ch| ch.kind() == "(" || ch.kind() == "var_spec_list")
            };
            if !has_parens {
                let mut c = outer_node.walk();
                let spec = outer_node
                    .named_children(&mut c)
                    .find(|ch| matches!(ch.kind(), "const_spec" | "var_spec"))?;
                let names = go_spec_names(spec, source);
                let vis = go_visibility(&names);
                let fqn = qualify_scope(scope, &names);
                Some(build_go_unit(
                    outer_node,
                    spec,
                    source,
                    logical_file_id,
                    parent_anchor,
                    sym_kind,
                    fqn,
                    vis,
                    None,
                    false,
                    None,
                ))
            } else {
                let (prefix, idx) = if sym_kind == SymbolKind::Const {
                    group_counters.const_group_idx += 1;
                    ("const_group", group_counters.const_group_idx)
                } else {
                    group_counters.var_group_idx += 1;
                    ("var_group", group_counters.var_group_idx)
                };
                let fqn = qualify_scope(scope, &format!("{prefix}#{idx}"));
                let trivia_start =
                    compute_leading_trivia_start(LanguageKind::Go, source, outer_node);
                let decl_start = outer_node.start_byte();
                let end_byte = outer_node.end_byte();
                let container_node = find_child_by_kind(outer_node, "var_spec_list")
                    .or_else(|| find_child_by_kind(outer_node, "("));
                let header_end = container_node.map(|n| n.start_byte()).unwrap_or(end_byte);
                let (sig, hash) = compute_normalized_signature_and_hash(
                    outer_node,
                    decl_start,
                    header_end,
                    source,
                );
                let anchor = NodeAnchor {
                    logical_file_id,
                    symbol_fqn: fqn,
                    syntax_kind_id: outer_node.kind_id(),
                    decl_header_hash: hash,
                };
                let spec_parent = find_child_by_kind(outer_node, "var_spec_list").unwrap_or(outer_node);
                let mut children = Vec::new();
                let mut c = spec_parent.walk();
                for spec in spec_parent.named_children(&mut c) {
                    if matches!(spec.kind(), "const_spec" | "var_spec") {
                        let names = go_spec_names(spec, source);
                        let vis = go_visibility(&names);
                        let child_fqn = qualify_scope(scope, &names);
                        children.push(build_go_unit(
                            spec,
                            spec,
                            source,
                            logical_file_id,
                            Some(anchor.clone()),
                            sym_kind,
                            child_fqn,
                            vis,
                            None,
                            false,
                            None,
                        ));
                    }
                }
                Some(DeclUnit {
                    anchor,
                    parent_anchor,
                    language: LanguageKind::Go,
                    kind: sym_kind,
                    visibility: VisibilityKind::Private,
                    is_container: true,
                    is_comma_separated_container: false,
                    trivia_start_byte: trivia_start,
                    decl_start_byte: decl_start,
                    end_byte,
                    header_range: trivia_start..header_end,
                    body_range: container_node.map(|p| p.start_byte()..end_byte),
                    normalized_signature: sig,
                    arity: None,
                    has_syntax_error: node_has_syntax_error(outer_node),
                    body_has_error: false,
                    children,
                })
            }
        }
        "field_declaration" => {
            let mut names = Vec::new();
            let mut c = outer_node.walk();
            for ch in outer_node.children_by_field_name("name", &mut c) {
                names.push(source[ch.byte_range()].trim().to_string());
            }
            let raw_name = if names.is_empty() {
                // Embedded field
                outer_node
                    .child_by_field_name("type")
                    .map(|t| collapse_ws(&source[t.byte_range()]))?
            } else {
                names.join(",")
            };
            let vis = go_visibility(&raw_name);
            let fqn = qualify_scope(scope, &raw_name);
            Some(build_go_unit(
                outer_node,
                outer_node,
                source,
                logical_file_id,
                parent_anchor,
                SymbolKind::Field,
                fqn,
                vis,
                None,
                false,
                None,
            ))
        }
        "method_elem" => {
            let name = outer_node
                .child_by_field_name("name")
                .map(|n| source[n.byte_range()].to_string())?;
            let vis = go_visibility(&name);
            let fqn = qualify_scope(scope, &name);
            Some(build_go_unit(
                outer_node,
                outer_node,
                source,
                logical_file_id,
                parent_anchor,
                SymbolKind::Method,
                fqn,
                vis,
                None,
                false,
                go_arity(outer_node),
            ))
        }
        _ => None,
    }
}

fn go_spec_names(spec: Node<'_>, source: &str) -> String {
    let mut names = Vec::new();
    let mut c = spec.walk();
    for ch in spec.children_by_field_name("name", &mut c) {
        names.push(source[ch.byte_range()].trim().to_string());
    }
    if names.is_empty() {
        collapse_ws(&source[spec.byte_range()])
    } else {
        names.join(",")
    }
}

fn extract_go_type_spec(
    outer_node: Node<'_>,
    spec: Node<'_>,
    source: &str,
    logical_file_id: Blake3Hash,
    scope: &str,
    parent_anchor: Option<NodeAnchor>,
) -> Option<DeclUnit> {
    let name = spec
        .child_by_field_name("name")
        .map(|n| source[n.byte_range()].to_string())?;
    let vis = go_visibility(&name);
    let fqn = qualify_scope(scope, &name);
    let ty = spec.child_by_field_name("type");
    let (sym_kind, body_range_override, container_node) = match ty.map(|t| t.kind()) {
        Some("struct_type") => {
            let struct_n = ty.unwrap();
            let fdl = find_child_by_kind(struct_n, "field_declaration_list");
            (SymbolKind::Struct, fdl.map(|b| b.byte_range()), fdl)
        }
        Some("interface_type") => {
            let iface_n = ty.unwrap();
            let brace = find_child_by_kind(iface_n, "{");
            let br = brace.map(|b| b.start_byte()..iface_n.end_byte());
            (SymbolKind::Interface, br, Some(iface_n))
        }
        _ => (SymbolKind::TypeAlias, None, None),
    };

    let trivia_start_byte = compute_leading_trivia_start(LanguageKind::Go, source, outer_node);
    let decl_start_byte = spec.start_byte();
    let end_byte = outer_node.end_byte();
    let header_end_byte = body_range_override
        .as_ref()
        .map(|r| r.start)
        .unwrap_or(end_byte);
    let header_range = trivia_start_byte..header_end_byte;
    let (normalized_signature, decl_header_hash) =
        compute_normalized_signature_and_hash(spec, decl_start_byte, header_end_byte, source);

    let anchor = NodeAnchor {
        logical_file_id,
        symbol_fqn: fqn.clone(),
        syntax_kind_id: spec.kind_id(),
        decl_header_hash,
    };

    let is_container = container_node.is_some();
    let children = if let Some(cn) = container_node {
        extract_go_children(cn, source, logical_file_id, &fqn, Some(anchor.clone()))
    } else {
        Vec::new()
    };

    Some(DeclUnit {
        anchor,
        parent_anchor,
        language: LanguageKind::Go,
        kind: sym_kind,
        visibility: vis,
        is_container,
        is_comma_separated_container: false,
        trivia_start_byte,
        decl_start_byte,
        end_byte,
        header_range,
        body_range: body_range_override,
        normalized_signature,
        arity: None,
        has_syntax_error: node_has_syntax_error(spec),
        body_has_error: container_node.map(node_has_syntax_error).unwrap_or(false),
        children,
    })
}

#[allow(clippy::too_many_arguments)]
fn build_go_unit(
    outer_node: Node<'_>,
    inner_node: Node<'_>,
    source: &str,
    logical_file_id: Blake3Hash,
    parent_anchor: Option<NodeAnchor>,
    kind: SymbolKind,
    fqn: String,
    visibility: VisibilityKind,
    body_node: Option<Node<'_>>,
    is_container: bool,
    arity: Option<(usize, usize)>,
) -> DeclUnit {
    let trivia_start_byte = compute_leading_trivia_start(LanguageKind::Go, source, outer_node);
    let decl_start_byte = inner_node.start_byte();
    let mut end_byte = outer_node.end_byte();
    if matches!(inner_node.kind(), "field_declaration" | "method_elem")
        && outer_node.parent().is_some_and(|p| {
            matches!(p.kind(), "field_declaration_list" | "interface_type")
        })
        && let Some(sib) = outer_node.next_sibling()
        && !sib.is_named()
        && sib.kind() == ";"
    {
        end_byte = sib.end_byte();
    }
    let header_end_byte = body_node
        .map(|b| b.start_byte())
        .unwrap_or(outer_node.end_byte());
    let header_range = trivia_start_byte..header_end_byte;
    let body_range = body_node.map(|b| b.byte_range());
    let (normalized_signature, decl_header_hash) =
        compute_normalized_signature_and_hash(inner_node, decl_start_byte, header_end_byte, source);
    let anchor = NodeAnchor {
        logical_file_id,
        symbol_fqn: fqn,
        syntax_kind_id: inner_node.kind_id(),
        decl_header_hash,
    };
    let body_has_error = body_node.map(node_has_syntax_error).unwrap_or(false);
    let has_syntax_error = inner_node.is_error()
        || inner_node.is_missing()
        || (if body_node.is_some() {
            let mut cursor = inner_node.walk();
            inner_node
                .children(&mut cursor)
                .filter(|c| Some(*c) != body_node)
                .any(node_has_syntax_error)
        } else {
            node_has_syntax_error(inner_node)
        });

    DeclUnit {
        anchor,
        parent_anchor,
        language: LanguageKind::Go,
        kind,
        visibility,
        is_container,
        is_comma_separated_container: false,
        trivia_start_byte,
        decl_start_byte,
        end_byte,
        header_range,
        body_range,
        normalized_signature,
        arity,
        has_syntax_error,
        body_has_error,
        children: Vec::new(),
    }
}

// ============================================================================
// Python
// ============================================================================

fn extract_python_children(
    container: Node<'_>,
    source: &str,
    logical_file_id: Blake3Hash,
    scope: &str,
    parent_anchor: Option<NodeAnchor>,
) -> Vec<DeclUnit> {
    let mut units = Vec::new();
    let mut overload_counts: HashMap<String, usize> = HashMap::new();
    let mut cursor = container.walk();
    for child in container.named_children(&mut cursor) {
        if let Some(unit) = extract_python_node(
            child,
            source,
            logical_file_id,
            scope,
            parent_anchor.clone(),
            &mut overload_counts,
        ) {
            units.push(unit);
        }
    }
    units
}

fn is_python_overload_decorated(outer_node: Node<'_>, source: &str) -> bool {
    if outer_node.kind() != "decorated_definition" {
        return false;
    }
    let mut cursor = outer_node.walk();
    for child in outer_node.children(&mut cursor) {
        if child.kind() == "decorator" {
            let text = source[child.byte_range()].trim();
            let expr = text.strip_prefix('@').unwrap_or(text).trim();
            let base_expr = expr.split('(').next().unwrap_or(expr).trim();
            if base_expr == "overload" || base_expr.ends_with(".overload") {
                return true;
            }
        }
    }
    false
}

fn python_accessor_prefix(outer_node: Node<'_>, source: &str, name: &str) -> &'static str {
    if outer_node.kind() != "decorated_definition" {
        return "";
    }
    let getter_suffix = format!(".{name}.getter");
    let setter_suffix = format!(".{name}.setter");
    let deleter_suffix = format!(".{name}.deleter");
    let mut cursor = outer_node.walk();
    for child in outer_node.children(&mut cursor) {
        if child.kind() == "decorator" {
            let text = source[child.byte_range()].trim();
            let expr = text.strip_prefix('@').unwrap_or(text).trim();
            let base_expr = expr.split('(').next().unwrap_or(expr).trim();
            if base_expr == "property"
                || base_expr == "builtins.property"
                || base_expr == format!("{name}.getter")
                || base_expr.ends_with(&getter_suffix)
            {
                return "get:";
            }
            if base_expr == format!("{name}.setter") || base_expr.ends_with(&setter_suffix) {
                return "set:";
            }
            if base_expr == format!("{name}.deleter") || base_expr.ends_with(&deleter_suffix) {
                return "del:";
            }
        }
    }
    ""
}

fn python_visibility(name: &str) -> VisibilityKind {
    if name.starts_with('_') && !(name.starts_with("__") && name.ends_with("__")) {
        VisibilityKind::Private
    } else {
        VisibilityKind::Public
    }
}

fn python_arity_with_source(
    func_node: Node<'_>,
    source: &str,
    is_method: bool,
) -> Option<(usize, usize)> {
    let params = func_node.child_by_field_name("parameters")?;
    let mut min_p = 0usize;
    let mut max_p = 0usize;
    let mut first = true;
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        if child.kind() == "comment" {
            continue;
        }
        if first && is_method {
            first = false;
            let txt = source[child.byte_range()].trim();
            if txt == "self" || txt == "cls" || txt.starts_with("self:") || txt.starts_with("cls:") {
                continue;
            }
        }
        first = false;
        match child.kind() {
            "identifier" | "typed_parameter" => {
                min_p += 1;
                if max_p != usize::MAX {
                    max_p += 1;
                }
            }
            "default_parameter" | "typed_default_parameter" => {
                if max_p != usize::MAX {
                    max_p += 1;
                }
            }
            "list_splat_parameter" | "dictionary_splat_parameter" => {
                max_p = usize::MAX;
            }
            "positional_separator" | "keyword_separator" => {}
            _ => {
                min_p += 1;
                if max_p != usize::MAX {
                    max_p += 1;
                }
            }
        }
    }
    Some((min_p, max_p))
}

fn python_colon_before_body<'a>(inner_node: Node<'a>, body: Node<'a>) -> Option<Node<'a>> {
    let mut cursor = inner_node.walk();
    let mut last_colon = None;
    for child in inner_node.children(&mut cursor) {
        if child.start_byte() >= body.start_byte() {
            break;
        }
        if child.kind() == ":" {
            last_colon = Some(child);
        }
    }
    last_colon
}

fn extract_python_node(
    outer_node: Node<'_>,
    source: &str,
    logical_file_id: Blake3Hash,
    scope: &str,
    parent_anchor: Option<NodeAnchor>,
    overload_counts: &mut HashMap<String, usize>,
) -> Option<DeclUnit> {
    let inner_node = if outer_node.kind() == "decorated_definition" {
        outer_node.child_by_field_name("definition")?
    } else {
        outer_node
    };

    let is_method = !scope.is_empty();
    let (sym_kind, vis_name, raw_name, body_node, is_container, arity) = match inner_node.kind() {
        "function_definition" => {
            let name = inner_node
                .child_by_field_name("name")
                .map(|n| source[n.byte_range()].to_string())?;
            let accessor = python_accessor_prefix(outer_node, source, &name);
            let tagged_name = format!("{accessor}{name}");
            let fqn_name = if is_python_overload_decorated(outer_node, source) {
                let count = overload_counts
                    .entry(tagged_name.clone())
                    .and_modify(|e| *e += 1)
                    .or_insert(1);
                format!("{tagged_name}#sig:{count}")
            } else {
                tagged_name
            };
            (
                if is_method {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                },
                name,
                fqn_name,
                inner_node.child_by_field_name("body"),
                false,
                python_arity_with_source(inner_node, source, is_method),
            )
        }
        "class_definition" => {
            let name = inner_node
                .child_by_field_name("name")
                .map(|n| source[n.byte_range()].to_string())?;
            let body = inner_node.child_by_field_name("body");
            (
                SymbolKind::Struct,
                name.clone(),
                name,
                body,
                body.is_some(),
                None,
            )
        }
        "type_alias_statement" => {
            let left = inner_node
                .child_by_field_name("left")
                .map(|n| collapse_ws(&source[n.byte_range()]))?;
            (SymbolKind::TypeAlias, left.clone(), left, None, false, None)
        }
        "import_statement" | "import_from_statement" | "future_import_statement" => {
            let norm = collapse_ws(&source[inner_node.byte_range()]);
            (
                SymbolKind::Import,
                String::new(),
                format!("import::{norm}"),
                None,
                false,
                None,
            )
        }
        "expression_statement" => {
            let mut c = inner_node.walk();
            let assign = inner_node
                .named_children(&mut c)
                .find(|ch| ch.kind() == "assignment")?;
            let left = assign.child_by_field_name("left")?;
            if left.kind() != "identifier" {
                return None;
            }
            let name = source[left.byte_range()].trim().to_string();
            (SymbolKind::Var, name.clone(), name, None, false, None)
        }
        _ => return None,
    };

    let visibility = if sym_kind == SymbolKind::Import {
        VisibilityKind::Private
    } else {
        python_visibility(&vis_name)
    };
    let fqn = qualify_scope(scope, &raw_name);
    let trivia_start_byte =
        compute_leading_trivia_start(LanguageKind::Python, source, outer_node);
    let decl_start_byte = inner_node.start_byte();
    let end_byte = outer_node.end_byte();

    let (header_end_byte, body_range) = if let Some(body) = body_node {
        if let Some(colon) = python_colon_before_body(inner_node, body) {
            (colon.end_byte(), Some(colon.end_byte()..end_byte))
        } else {
            (body.start_byte(), Some(body.start_byte()..end_byte))
        }
    } else {
        (end_byte, None)
    };

    let header_range = trivia_start_byte..header_end_byte;
    let (normalized_signature, decl_header_hash) = compute_normalized_signature_and_hash(
        inner_node,
        decl_start_byte,
        header_end_byte,
        source,
    );

    let anchor = NodeAnchor {
        logical_file_id,
        symbol_fqn: fqn.clone(),
        syntax_kind_id: inner_node.kind_id(),
        decl_header_hash,
    };

    let body_has_error = body_node.map(node_has_syntax_error).unwrap_or(false);
    let has_syntax_error = inner_node.is_error()
        || inner_node.is_missing()
        || (if body_node.is_some() {
            let mut cursor = inner_node.walk();
            inner_node
                .children(&mut cursor)
                .filter(|c| Some(*c) != body_node)
                .any(node_has_syntax_error)
        } else {
            node_has_syntax_error(inner_node)
        });

    let children = if is_container {
        if let Some(body) = body_node {
            extract_python_children(body, source, logical_file_id, &fqn, Some(anchor.clone()))
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };

    Some(DeclUnit {
        anchor,
        parent_anchor,
        language: LanguageKind::Python,
        kind: sym_kind,
        visibility,
        is_container,
        is_comma_separated_container: false,
        trivia_start_byte,
        decl_start_byte,
        end_byte,
        header_range,
        body_range,
        normalized_signature,
        arity,
        has_syntax_error,
        body_has_error,
        children,
    })
}

// ============================================================================
// Rust `impl<...>` FQN Normalization Helpers (shared by merge.rs & db.rs)
// ============================================================================

fn split_top_level_colons(s: &str) -> Vec<&str> {
    let bytes = s.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut angle = 0usize;
    let mut paren = 0usize;
    let mut bracket = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'<' => angle += 1,
            b'>' => {
                if i == 0 || (bytes[i - 1] != b'-' && bytes[i - 1] != b'=') {
                    angle = angle.saturating_sub(1);
                }
            }
            b'(' => paren += 1,
            b')' => paren = paren.saturating_sub(1),
            b'[' => bracket += 1,
            b']' => bracket = bracket.saturating_sub(1),
            b':' if angle == 0
                && paren == 0
                && bracket == 0
                && i + 1 < bytes.len()
                && bytes[i + 1] == b':' =>
            {
                parts.push(&s[start..i]);
                i += 2;
                start = i;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(&s[start..]);
    parts
}

pub(crate) fn strip_rust_generic_args(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0usize;
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '<' {
            if depth > 0 {
                depth += 1;
            } else if !out.is_empty() && !out.ends_with("::") {
                depth = 1;
            } else if out.ends_with("::") && out.len() > 2 {
                // Turbofish `Foo::<T>`
                out.truncate(out.len() - 2);
                depth = 1;
            } else {
                out.push(ch);
            }
        } else if ch == '>' {
            let is_arrow = i > 0 && (chars[i - 1] == '-' || chars[i - 1] == '=');
            if depth > 0 && !is_arrow {
                depth -= 1;
            } else if depth == 0 {
                out.push(ch);
            }
        } else if depth == 0 {
            out.push(ch);
        }
        i += 1;
    }
    out.split("::")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("::")
}

fn strip_rust_type_prefixes(mut s: &str) -> &str {
    loop {
        s = s.trim();
        if let Some(rest) = s.strip_prefix('&') {
            s = rest.trim_start();
            if s.starts_with('\'') {
                if let Some(sp) = s.find(|c: char| c.is_whitespace()) {
                    s = s[sp..].trim_start();
                } else {
                    return "";
                }
            }
            if let Some(rest_mut) = s.strip_prefix("mut")
                && rest_mut.chars().next().is_some_and(|c| c.is_whitespace())
            {
                s = rest_mut.trim_start();
            }
            continue;
        }
        break;
    }
    s
}

fn clean_rust_impl_path(s: &str) -> String {
    strip_rust_generic_args(strip_rust_type_prefixes(s))
}

fn find_top_level_for(inner: &str) -> Option<usize> {
    let bytes = inner.as_bytes();
    let mut angle = 0usize;
    let mut paren = 0usize;
    let mut bracket = 0usize;
    for i in 0..bytes.len() {
        match bytes[i] {
            b'<' => angle += 1,
            b'>' => {
                if i == 0 || (bytes[i - 1] != b'-' && bytes[i - 1] != b'=') {
                    angle = angle.saturating_sub(1);
                }
            }
            b'(' => paren += 1,
            b')' => paren = paren.saturating_sub(1),
            b'[' => bracket += 1,
            b']' => bracket = bracket.saturating_sub(1),
            _ => {}
        }
        if angle == 0 && paren == 0 && bracket == 0 && inner[i..].starts_with(" for ") {
            return Some(i);
        }
    }
    None
}

pub(crate) fn expand_rust_impl_fqn_candidates(fqn: &str) -> Vec<String> {
    if !fqn.contains("impl<") {
        return Vec::new();
    }
    let parts = split_top_level_colons(fqn);
    let mut seg_options: Vec<Vec<String>> = Vec::with_capacity(parts.len());
    let mut qualified_impl_tails: Vec<String> = Vec::new();
    let mut has_impl = false;

    for (idx, part) in parts.iter().enumerate() {
        let no_hash = if let Some((prefix, suffix)) = part.rsplit_once('#')
            && !suffix.is_empty()
            && suffix.bytes().all(|b| b.is_ascii_digit())
        {
            prefix
        } else {
            *part
        };
        if idx + 1 < parts.len()
            && let Some(inner) = no_hash.strip_prefix("impl<").and_then(|s| s.strip_suffix('>'))
        {
            has_impl = true;
            let mut opts = Vec::new();
            if let Some(for_idx) = find_top_level_for(inner) {
                let tr_raw = inner[..for_idx].trim().trim_start_matches('!').trim();
                let ty_raw = inner[for_idx + 5..].trim();
                let ty_clean = clean_rust_impl_path(ty_raw);
                let tr_clean = clean_rust_impl_path(tr_raw);
                if !ty_clean.is_empty() {
                    if idx > 0 && ty_clean.contains("::") {
                        let tail_parts = &parts[idx + 1..];
                        if tail_parts.is_empty() {
                            qualified_impl_tails.push(ty_clean.clone());
                        } else {
                            qualified_impl_tails.push(format!("{ty_clean}::{}", tail_parts.join("::")));
                        }
                    }
                    opts.push(ty_clean);
                }
                if !tr_clean.is_empty() && !opts.contains(&tr_clean) {
                    opts.push(tr_clean);
                }
            } else {
                let ty_clean = clean_rust_impl_path(inner);
                if !ty_clean.is_empty() {
                    if idx > 0 && ty_clean.contains("::") {
                        let tail_parts = &parts[idx + 1..];
                        if tail_parts.is_empty() {
                            qualified_impl_tails.push(ty_clean.clone());
                        } else {
                            qualified_impl_tails.push(format!("{ty_clean}::{}", tail_parts.join("::")));
                        }
                    }
                    opts.push(ty_clean);
                }
            }
            if opts.is_empty() {
                opts.push((*part).to_string());
            }
            seg_options.push(opts);
        } else {
            seg_options.push(vec![(*part).to_string()]);
        }
    }

    if !has_impl {
        return Vec::new();
    }

    let mut out = vec![String::new()];
    for opts in seg_options {
        let mut next = Vec::with_capacity(out.len() * opts.len());
        for prefix in &out {
            for opt in &opts {
                if prefix.is_empty() {
                    next.push(opt.clone());
                } else {
                    next.push(format!("{prefix}::{opt}"));
                }
            }
        }
        out = next;
    }
    for qt in qualified_impl_tails {
        if !out.contains(&qt) {
            out.push(qt);
        }
    }
    out
}
