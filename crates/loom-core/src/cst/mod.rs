pub mod anchor;
pub mod diff;
pub mod lang;
pub mod merge;
pub mod symbols;
pub mod trivia;

pub use anchor::{
    DeclUnit, SymbolKind, VisibilityKind, extract_decl_units, file_root_anchor,
    flatten_decl_units,
};
pub use diff::{
    DiffContext, compute_logical_file_id, diff_file_to_patch, diff_file_to_patch_strict,
    diff_source_to_patch, diff_source_to_patch_strict,
};
pub use lang::{
    CstError, IncrementalParseResult, ParsedFile, SyntaxErrorSpan, byte_to_ts_point,
    collect_syntax_errors, compute_input_edit, parse_source, parse_source_incremental, ts_language,
};
pub use merge::{
    MergeReport, MergeState, MergedFileContent, detect_divergent_base_conflicts, merge_patches,
    merge_trees,
};
pub use symbols::{
    ConsumedSymbol, ExtractedSymbols, ProvidedSymbol, compute_contract_deltas,
    extract_file_symbols, tags_queries_ready,
};
pub use trivia::{compute_leading_trivia_start, count_newlines, has_blank_line_gap};
