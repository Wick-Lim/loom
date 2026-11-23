use crate::model::LanguageKind;
use tree_sitter::Node;

pub fn count_newlines(s: &str) -> usize {
    let bytes = s.as_bytes();
    let mut count = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' {
            count += 1;
            if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                i += 2;
            } else {
                i += 1;
            }
        } else if bytes[i] == b'\n' {
            count += 1;
            i += 1;
        } else {
            i += 1;
        }
    }
    count
}

pub fn has_blank_line_gap(source: &str, prev_node: Node<'_>, next_start_byte: usize) -> bool {
    let prev_end = prev_node.end_byte().min(next_start_byte);
    let gap = &source[prev_end..next_start_byte];
    let prev_start = prev_node.start_byte().min(prev_end);
    let prev_text = &source[prev_start..prev_end];
    let split_crlf =
        prev_text.ends_with('\r') && !prev_text.ends_with("\r\n") && gap.starts_with('\n');
    let trailing_in_prev = if split_crlf {
        0
    } else {
        usize::from(prev_text.ends_with('\n') || prev_text.ends_with('\r'))
    };
    count_newlines(gap) + trailing_in_prev >= 2
}

pub fn is_trivia_node(lang: LanguageKind, node: Node<'_>) -> bool {
    let kind = node.kind();
    match lang {
        LanguageKind::Rust => matches!(kind, "line_comment" | "block_comment" | "attribute_item"),
        LanguageKind::TypeScript | LanguageKind::Tsx => {
            matches!(kind, "comment" | "html_comment" | "decorator")
        }
        LanguageKind::Go => kind == "comment",
        LanguageKind::Python => kind == "comment",
        _ => false,
    }
}

fn is_rust_inner_trivia(node: Node<'_>) -> bool {
    let kind = node.kind();
    if kind == "inner_attribute_item" {
        return true;
    }
    if matches!(kind, "line_comment" | "block_comment") && node.child_by_field_name("inner").is_some()
    {
        return true;
    }
    false
}

pub fn expand_left_to_line_start_if_indented(source: &str, byte_idx: usize) -> usize {
    let bytes = source.as_bytes();
    let mut i = byte_idx.min(bytes.len());
    while i > 0 && (bytes[i - 1] == b' ' || bytes[i - 1] == b'\t') {
        i -= 1;
    }
    if i == 0 || bytes[i - 1] == b'\n' || bytes[i - 1] == b'\r' {
        i
    } else {
        byte_idx
    }
}

pub fn compute_leading_trivia_start(
    lang: LanguageKind,
    source: &str,
    outer_node: Node<'_>,
) -> usize {
    let mut trivia_start = outer_node.start_byte();
    let mut cursor = outer_node;

    while let Some(prev) = cursor.prev_sibling() {
        if lang == LanguageKind::Rust && is_rust_inner_trivia(prev) {
            break;
        }
        if !is_trivia_node(lang, prev) {
            break;
        }

        let kind = prev.kind();
        let is_attr_or_decorator = matches!(kind, "attribute_item" | "decorator");

        if !is_attr_or_decorator {
            // Trailing-Comment Guard: do not steal an inline comment belonging to the previous node.
            if let Some(before) = prev.prev_sibling()
                && !is_trivia_node(lang, before)
                && before.end_position().row == prev.start_position().row
            {
                break;
            }
            if has_blank_line_gap(source, prev, trivia_start) {
                break;
            }
        }

        trivia_start = prev.start_byte();
        cursor = prev;
    }

    expand_left_to_line_start_if_indented(source, trivia_start)
}
