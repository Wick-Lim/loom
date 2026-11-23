use crate::model::LanguageKind;
use std::ops::Range;
use thiserror::Error;
use tree_sitter::{InputEdit, Language, Node, Parser, Point, Tree, TreeCursor};

#[derive(Debug, Error)]
pub enum CstError {
    #[error("unsupported language for CST parsing: {0:?}")]
    UnsupportedLanguage(LanguageKind),
    #[error("failed to set tree-sitter language: {0}")]
    LanguageSet(String),
    #[error("tree-sitter failed to produce a parse tree")]
    ParseFailed,
    #[error("syntax error in {file_path} at line {line}, column {column}")]
    SyntaxError {
        file_path: String,
        line: usize,
        column: usize,
    },
    #[error("source file for {0} is not valid UTF-8")]
    InvalidUtf8(String),
    #[error("target anchor not found during CST merge: {0}")]
    AnchorNotFound(String),
    #[error("CRDT error: {0}")]
    Crdt(#[from] crate::crdt::CrdtError),
    #[error("CAS error: {0}")]
    Cas(#[from] crate::cas::CasError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxErrorSpan {
    pub byte_range: Range<usize>,
    pub line: usize,
    pub column: usize,
    pub is_missing: bool,
}

#[derive(Debug, Clone)]
pub struct ParsedFile {
    pub language: LanguageKind,
    pub source: String,
    pub tree: Tree,
}

#[derive(Debug, Clone)]
pub struct IncrementalParseResult {
    pub parsed: ParsedFile,
    pub changed_ranges: Vec<tree_sitter::Range>,
    pub used_incremental: bool,
}

pub fn byte_to_ts_point(source: &str, byte_idx: usize) -> Point {
    let clamped = byte_idx.min(source.len());
    let mut row = 0usize;
    let mut column = 0usize;
    for &b in &source.as_bytes()[..clamped] {
        if b == b'\n' {
            row += 1;
            column = 0;
        } else {
            column += 1;
        }
    }
    Point { row, column }
}

/// Compute a UTF-8 char-boundary-safe `tree_sitter::InputEdit` transforming `old_src` into `new_src`.
/// Returns `None` if `old_src == new_src`.
pub fn compute_input_edit(old_src: &str, new_src: &str) -> Option<InputEdit> {
    if old_src == new_src {
        return None;
    }
    let old_bytes = old_src.as_bytes();
    let new_bytes = new_src.as_bytes();
    let min_len = old_bytes.len().min(new_bytes.len());

    let mut prefix = 0usize;
    while prefix < min_len && old_bytes[prefix] == new_bytes[prefix] {
        prefix += 1;
    }
    while prefix > 0 && (!old_src.is_char_boundary(prefix) || !new_src.is_char_boundary(prefix)) {
        prefix -= 1;
    }

    let max_suffix = (old_bytes.len() - prefix).min(new_bytes.len() - prefix);
    let mut suffix = 0usize;
    while suffix < max_suffix
        && old_bytes[old_bytes.len() - 1 - suffix] == new_bytes[new_bytes.len() - 1 - suffix]
    {
        suffix += 1;
    }
    while suffix > 0
        && (!old_src.is_char_boundary(old_bytes.len() - suffix)
            || !new_src.is_char_boundary(new_bytes.len() - suffix))
    {
        suffix -= 1;
    }

    let old_end_byte = old_bytes.len() - suffix;
    let new_end_byte = new_bytes.len() - suffix;

    Some(InputEdit {
        start_byte: prefix,
        old_end_byte,
        new_end_byte,
        start_position: byte_to_ts_point(old_src, prefix),
        old_end_position: byte_to_ts_point(old_src, old_end_byte),
        new_end_position: byte_to_ts_point(new_src, new_end_byte),
    })
}

pub fn ts_language(lang: LanguageKind) -> Result<Language, CstError> {
    match lang {
        LanguageKind::Rust => Ok(Language::from(tree_sitter_rust::LANGUAGE)),
        LanguageKind::TypeScript => Ok(Language::from(tree_sitter_typescript::LANGUAGE_TYPESCRIPT)),
        LanguageKind::Tsx => Ok(Language::from(tree_sitter_typescript::LANGUAGE_TSX)),
        LanguageKind::Go => Ok(Language::from(tree_sitter_go::LANGUAGE)),
        LanguageKind::Python => Ok(Language::from(tree_sitter_python::LANGUAGE)),
        other => Err(CstError::UnsupportedLanguage(other)),
    }
}

pub fn parse_source(
    language: LanguageKind,
    source: &str,
    old_tree: Option<&Tree>,
) -> Result<ParsedFile, CstError> {
    let ts_lang = ts_language(language)?;
    let mut parser = Parser::new();
    parser
        .set_language(&ts_lang)
        .map_err(|e| CstError::LanguageSet(e.to_string()))?;
    let tree = parser
        .parse(source, old_tree)
        .ok_or(CstError::ParseFailed)?;
    Ok(ParsedFile {
        language,
        source: source.to_string(),
        tree,
    })
}

pub fn parse_source_incremental(
    language: LanguageKind,
    old_src: &str,
    new_src: &str,
    old_tree: Option<&mut Tree>,
    parser: Option<&mut Parser>,
) -> Result<IncrementalParseResult, CstError> {
    let mut owned_parser: Option<Parser> = None;
    let p = match parser {
        Some(p) => {
            if p.language().is_none() {
                let ts_lang = ts_language(language)?;
                p.set_language(&ts_lang)
                    .map_err(|e| CstError::LanguageSet(e.to_string()))?;
            }
            p
        }
        None => {
            let ts_lang = ts_language(language)?;
            let mut np = Parser::new();
            np.set_language(&ts_lang)
                .map_err(|e| CstError::LanguageSet(e.to_string()))?;
            owned_parser.insert(np)
        }
    };

    if let Some(tree) = old_tree {
        if let Some(edit) = compute_input_edit(old_src, new_src) {
            tree.edit(&edit);
        }
        let new_tree = p.parse(new_src, Some(&*tree)).ok_or(CstError::ParseFailed)?;
        let changed_ranges = tree.changed_ranges(&new_tree).collect();
        Ok(IncrementalParseResult {
            parsed: ParsedFile {
                language,
                source: new_src.to_string(),
                tree: new_tree,
            },
            changed_ranges,
            used_incremental: true,
        })
    } else {
        let new_tree = p.parse(new_src, None).ok_or(CstError::ParseFailed)?;
        Ok(IncrementalParseResult {
            parsed: ParsedFile {
                language,
                source: new_src.to_string(),
                tree: new_tree,
            },
            changed_ranges: Vec::new(),
            used_incremental: false,
        })
    }
}

pub fn collect_syntax_errors(tree: &Tree) -> Vec<SyntaxErrorSpan> {
    let mut out = Vec::new();
    let root = tree.root_node();
    if !root.has_error() {
        return out;
    }
    let mut cursor = root.walk();
    collect_errors_recursive(&mut cursor, &mut out);
    out
}

fn collect_errors_recursive(cursor: &mut TreeCursor<'_>, out: &mut Vec<SyntaxErrorSpan>) {
    let node = cursor.node();
    if node.is_error() || node.is_missing() {
        let pos = node.start_position();
        out.push(SyntaxErrorSpan {
            byte_range: node.byte_range(),
            line: pos.row + 1,
            column: pos.column + 1,
            is_missing: node.is_missing(),
        });
        return;
    }
    if !node.has_error() {
        return;
    }
    let before_len = out.len();
    if cursor.goto_first_child() {
        loop {
            collect_errors_recursive(cursor, out);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
    if out.len() == before_len {
        let pos = node.start_position();
        out.push(SyntaxErrorSpan {
            byte_range: node.byte_range(),
            line: pos.row + 1,
            column: pos.column + 1,
            is_missing: true,
        });
    }
}

pub fn node_has_syntax_error(node: Node<'_>) -> bool {
    node.has_error() || node.is_error() || node.is_missing()
}

pub fn node_in_error_subtree(node: Node<'_>) -> bool {
    if node.is_error() || node.is_missing() || node.has_error() {
        return true;
    }
    let mut cur = node.parent();
    while let Some(anc) = cur {
        let kind = anc.kind();
        if kind == "source_file" || kind == "program" || kind == "module" {
            break;
        }
        if anc.is_error() || anc.is_missing() || anc.has_error() {
            return true;
        }
        cur = anc.parent();
    }
    false
}
