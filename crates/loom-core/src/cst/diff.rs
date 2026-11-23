use super::anchor::{DeclUnit, SymbolKind, extract_decl_units, file_root_anchor};
use super::lang::{CstError, collect_syntax_errors, parse_source};
use super::symbols::{ProvidedSymbol, compute_contract_deltas, extract_file_symbols};
use crate::cas::{CasStore, is_binary_content};
use crate::crdt;
use crate::model::{AstPatch, Blake3Hash, LanguageKind, NodeAnchor, SemanticOp, now_ns};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

pub fn compute_logical_file_id(rel_path: &Path) -> Blake3Hash {
    let normalized = rel_path
        .to_string_lossy()
        .replace('\\', "/")
        .trim_start_matches("./")
        .to_string();
    Blake3Hash::from_bytes(normalized.as_bytes())
}

#[derive(Debug, Clone)]
pub struct DiffContext<'a> {
    pub logical_file_id: Blake3Hash,
    pub file_path: &'a Path,
    pub language: LanguageKind,
    pub viewport_id: &'a str,
    pub actor_id: &'a str,
    pub lamport_ts: u64,
    pub parents: Vec<Blake3Hash>,
}

pub fn diff_source_to_patch(
    ctx: &DiffContext<'_>,
    old_src: &str,
    new_src: &str,
) -> Result<AstPatch, CstError> {
    diff_source_inner(ctx, old_src, new_src, false, None)
}

pub fn diff_source_to_patch_strict(
    ctx: &DiffContext<'_>,
    old_src: &str,
    new_src: &str,
) -> Result<AstPatch, CstError> {
    diff_source_inner(ctx, old_src, new_src, true, None)
}

pub fn diff_file_to_patch(
    ctx: &DiffContext<'_>,
    old_bytes: &[u8],
    new_bytes: &[u8],
    rename_from: Option<&Path>,
    cas: Option<&CasStore>,
) -> Result<AstPatch, CstError> {
    diff_file_to_patch_inner(ctx, old_bytes, new_bytes, rename_from, cas, false)
}

pub fn diff_file_to_patch_strict(
    ctx: &DiffContext<'_>,
    old_bytes: &[u8],
    new_bytes: &[u8],
    rename_from: Option<&Path>,
    cas: Option<&CasStore>,
) -> Result<AstPatch, CstError> {
    diff_file_to_patch_inner(ctx, old_bytes, new_bytes, rename_from, cas, true)
}

fn diff_file_to_patch_inner(
    ctx: &DiffContext<'_>,
    old_bytes: &[u8],
    new_bytes: &[u8],
    rename_from: Option<&Path>,
    cas: Option<&CasStore>,
    strict: bool,
) -> Result<AstPatch, CstError> {
    let mut ops = Vec::new();
    if let Some(old_path) = rename_from
        && old_path != ctx.file_path
    {
        ops.push(SemanticOp::RenameOrMoveFile {
            logical_file_id: ctx.logical_file_id,
            old_path: old_path.to_path_buf(),
            new_path: ctx.file_path.to_path_buf(),
        });
    }

    let is_bin = ctx.language == LanguageKind::Binary
        || is_binary_content(ctx.file_path, old_bytes)
        || is_binary_content(ctx.file_path, new_bytes);

    let cas_before = cas.map(|c| c.put_bytes(old_bytes)).transpose()?;
    let cas_after = cas.map(|c| c.put_bytes(new_bytes)).transpose()?;

    if is_bin {
        let manifest = if let Some(c) = cas {
            Some(c.put_chunked_binary_default(new_bytes)?)
        } else {
            None
        };
        let mut patch = AstPatch {
            patch_hash: Blake3Hash::ZERO,
            viewport_id: ctx.viewport_id.to_string(),
            actor_id: ctx.actor_id.to_string(),
            lamport_ts: ctx.lamport_ts,
            parents: ctx.parents.clone(),
            logical_file_id: ctx.logical_file_id,
            file_path: ctx.file_path.to_path_buf(),
            ops,
            contract_deltas: Vec::new(),
            cas_before,
            cas_after,
            binary_manifest: manifest,
            created_at_ns: now_ns(),
        };
        patch.finalize_hash();
        return Ok(patch);
    }

    let old_str = std::str::from_utf8(old_bytes)
        .map_err(|_| CstError::InvalidUtf8(ctx.file_path.display().to_string()))?;
    let new_str = std::str::from_utf8(new_bytes)
        .map_err(|_| CstError::InvalidUtf8(ctx.file_path.display().to_string()))?;

    let mut patch = if ctx.language.is_code() {
        diff_source_inner(ctx, old_str, new_str, strict, Some((cas_before, cas_after)))?
    } else {
        let changes = crdt::diff_text_to_crdt_changes(
            old_str,
            new_str,
            ctx.actor_id,
            ctx.lamport_ts,
        )?;
        let mut crdt_ops = Vec::new();
        if !changes.is_empty() || old_str != new_str {
            crdt_ops.push(SemanticOp::TextCrdtFallback {
                logical_file_id: ctx.logical_file_id,
                file_path: ctx.file_path.to_path_buf(),
                automerge_changes: changes,
            });
        }
        AstPatch {
            patch_hash: Blake3Hash::ZERO,
            viewport_id: ctx.viewport_id.to_string(),
            actor_id: ctx.actor_id.to_string(),
            lamport_ts: ctx.lamport_ts,
            parents: ctx.parents.clone(),
            logical_file_id: ctx.logical_file_id,
            file_path: ctx.file_path.to_path_buf(),
            ops: crdt_ops,
            contract_deltas: Vec::new(),
            cas_before,
            cas_after,
            binary_manifest: None,
            created_at_ns: now_ns(),
        }
    };

    if !ops.is_empty() {
        ops.append(&mut patch.ops);
        patch.ops = ops;
    }
    patch.finalize_hash();
    Ok(patch)
}

fn diff_source_inner(
    ctx: &DiffContext<'_>,
    old_src: &str,
    new_src: &str,
    strict: bool,
    cas_hashes: Option<(Option<Blake3Hash>, Option<Blake3Hash>)>,
) -> Result<AstPatch, CstError> {
    let (cas_before, cas_after) = cas_hashes.unwrap_or((None, None));
    if !ctx.language.is_code() {
        let changes = crdt::diff_text_to_crdt_changes(
            old_src,
            new_src,
            ctx.actor_id,
            ctx.lamport_ts,
        )?;
        let ops = if !changes.is_empty() || old_src != new_src {
            vec![SemanticOp::TextCrdtFallback {
                logical_file_id: ctx.logical_file_id,
                file_path: ctx.file_path.to_path_buf(),
                automerge_changes: changes,
            }]
        } else {
            Vec::new()
        };
        let mut patch = AstPatch {
            patch_hash: Blake3Hash::ZERO,
            viewport_id: ctx.viewport_id.to_string(),
            actor_id: ctx.actor_id.to_string(),
            lamport_ts: ctx.lamport_ts,
            parents: ctx.parents.clone(),
            logical_file_id: ctx.logical_file_id,
            file_path: ctx.file_path.to_path_buf(),
            ops,
            contract_deltas: Vec::new(),
            cas_before,
            cas_after,
            binary_manifest: None,
            created_at_ns: now_ns(),
        };
        patch.finalize_hash();
        return Ok(patch);
    }

    if old_src == new_src {
        let mut patch = AstPatch {
            patch_hash: Blake3Hash::ZERO,
            viewport_id: ctx.viewport_id.to_string(),
            actor_id: ctx.actor_id.to_string(),
            lamport_ts: ctx.lamport_ts,
            parents: ctx.parents.clone(),
            logical_file_id: ctx.logical_file_id,
            file_path: ctx.file_path.to_path_buf(),
            ops: Vec::new(),
            contract_deltas: Vec::new(),
            cas_before,
            cas_after,
            binary_manifest: None,
            created_at_ns: now_ns(),
        };
        patch.finalize_hash();
        return Ok(patch);
    }

    let old_parsed = parse_source(ctx.language, old_src, None)?;
    let new_parsed = parse_source(ctx.language, new_src, None)?;

    let old_errors = collect_syntax_errors(&old_parsed.tree);
    let new_errors = collect_syntax_errors(&new_parsed.tree);
    if !old_errors.is_empty() || !new_errors.is_empty() {
        if strict {
            let err = new_errors.first().or(old_errors.first()).unwrap();
            return Err(CstError::SyntaxError {
                file_path: ctx.file_path.display().to_string(),
                line: err.line,
                column: err.column,
            });
        }
        let changes = crdt::diff_text_to_crdt_changes(
            old_src,
            new_src,
            ctx.actor_id,
            ctx.lamport_ts,
        )?;
        let mut patch = AstPatch {
            patch_hash: Blake3Hash::ZERO,
            viewport_id: ctx.viewport_id.to_string(),
            actor_id: ctx.actor_id.to_string(),
            lamport_ts: ctx.lamport_ts,
            parents: ctx.parents.clone(),
            logical_file_id: ctx.logical_file_id,
            file_path: ctx.file_path.to_path_buf(),
            ops: vec![SemanticOp::TextCrdtFallback {
                logical_file_id: ctx.logical_file_id,
                file_path: ctx.file_path.to_path_buf(),
                automerge_changes: changes,
            }],
            contract_deltas: Vec::new(),
            cas_before,
            cas_after,
            binary_manifest: None,
            created_at_ns: now_ns(),
        };
        patch.finalize_hash();
        return Ok(patch);
    }

    let old_units = extract_decl_units(
        ctx.language,
        &old_parsed.tree,
        old_src,
        ctx.logical_file_id,
    );
    let new_units = extract_decl_units(
        ctx.language,
        &new_parsed.tree,
        new_src,
        ctx.logical_file_id,
    );

    let mut ops = Vec::new();
    let mut impl_remap: HashMap<String, String> = HashMap::new();
    let mut sig_remap: HashMap<(String, u16), String> = HashMap::new();
    let root_anchor = file_root_anchor(ctx.logical_file_id);
    diff_unit_lists(
        &root_anchor,
        "",
        "",
        &old_units,
        &new_units,
        old_src,
        new_src,
        &mut ops,
        &mut impl_remap,
        &mut sig_remap,
    );

    let ts = now_ns();
    let old_syms = extract_file_symbols(
        &old_parsed,
        ctx.logical_file_id,
        ctx.file_path,
        ctx.viewport_id,
        ts,
    );
    let mut new_syms = extract_file_symbols(
        &new_parsed,
        ctx.logical_file_id,
        ctx.file_path,
        ctx.viewport_id,
        ts,
    );
    if !impl_remap.is_empty() {
        remap_impl_provides(&mut new_syms.provides, &impl_remap);
    }
    if !sig_remap.is_empty() {
        remap_sig_provides(&mut new_syms.provides, &sig_remap);
    }
    let contract_deltas = compute_contract_deltas(&old_syms.provides, &new_syms.provides);

    let mut patch = AstPatch {
        patch_hash: Blake3Hash::ZERO,
        viewport_id: ctx.viewport_id.to_string(),
        actor_id: ctx.actor_id.to_string(),
        lamport_ts: ctx.lamport_ts,
        parents: ctx.parents.clone(),
        logical_file_id: ctx.logical_file_id,
        file_path: ctx.file_path.to_path_buf(),
        ops,
        contract_deltas,
        cas_before,
        cas_after,
        binary_manifest: None,
        created_at_ns: ts,
    };
    patch.finalize_hash();
    Ok(patch)
}

fn relative_fqn<'a>(fqn: &'a str, scope_prefix: &str) -> &'a str {
    if scope_prefix.is_empty() {
        fqn
    } else {
        fqn.strip_prefix(scope_prefix)
            .and_then(|s| s.strip_prefix("::"))
            .unwrap_or(fqn)
    }
}

fn requalify_anchor(
    anchor: &NodeAnchor,
    old_scope_prefix: &str,
    new_scope_prefix: &str,
) -> NodeAnchor {
    if old_scope_prefix == new_scope_prefix
        || old_scope_prefix.is_empty()
        || new_scope_prefix.is_empty()
    {
        return anchor.clone();
    }
    if let Some(rest) = anchor
        .symbol_fqn
        .strip_prefix(new_scope_prefix)
        .and_then(|s| s.strip_prefix("::"))
    {
        let mut out = anchor.clone();
        out.symbol_fqn = format!("{old_scope_prefix}::{rest}");
        out
    } else {
        anchor.clone()
    }
}

fn strip_impl_disambiguator(fqn: &str) -> &str {
    if let Some((prefix, suffix)) = fqn.rsplit_once('#')
        && !suffix.is_empty()
        && suffix.bytes().all(|b| b.is_ascii_digit())
    {
        prefix
    } else {
        fqn
    }
}

fn strip_sig_disambiguator(fqn: &str) -> &str {
    if let Some((prefix, suffix)) = fqn.rsplit_once("#sig:")
        && !suffix.is_empty()
        && suffix.bytes().all(|b| b.is_ascii_digit())
    {
        prefix
    } else {
        fqn
    }
}

fn has_sig_disambiguator(fqn: &str) -> bool {
    if let Some((_, suffix)) = fqn.rsplit_once("#sig:") {
        !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit())
    } else {
        false
    }
}

fn impl_match_score(ou: &DeclUnit, nu: &DeclUnit) -> (usize, usize, usize) {
    let mut overlap_score = 0usize;
    for nc in &nu.children {
        let n_rel = relative_fqn(&nc.anchor.symbol_fqn, &nu.anchor.symbol_fqn);
        let mut best_child = 0usize;
        for oc in &ou.children {
            let o_rel = relative_fqn(&oc.anchor.symbol_fqn, &ou.anchor.symbol_fqn);
            if o_rel == n_rel {
                if oc.anchor.syntax_kind_id == nc.anchor.syntax_kind_id
                    && oc.anchor.decl_header_hash == nc.anchor.decl_header_hash
                {
                    best_child = best_child.max(100);
                } else if oc.anchor.syntax_kind_id == nc.anchor.syntax_kind_id {
                    best_child = best_child.max(50);
                } else {
                    best_child = best_child.max(20);
                }
            }
        }
        overlap_score += best_child;
    }
    let same_header = usize::from(ou.anchor.decl_header_hash == nu.anchor.decl_header_hash);
    let same_fqn = usize::from(ou.anchor.symbol_fqn == nu.anchor.symbol_fqn);
    (overlap_score, same_header, same_fqn)
}

fn remap_impl_provides(
    provides: &mut [ProvidedSymbol],
    impl_remap: &HashMap<String, String>,
) {
    let claimed_old: HashSet<&str> = impl_remap.values().map(String::as_str).collect();
    for prov in provides {
        if let Some((new_prefix, old_prefix)) = impl_remap
            .iter()
            .filter(|(k, _)| {
                prov.anchor.symbol_fqn == **k
                    || prov
                        .anchor
                        .symbol_fqn
                        .strip_prefix(k.as_str())
                        .is_some_and(|rest| rest.starts_with("::"))
            })
            .max_by_key(|(k, _)| k.len())
        {
            if prov.anchor.symbol_fqn == *new_prefix {
                prov.anchor.symbol_fqn = old_prefix.clone();
            } else if let Some(rest) = prov.anchor.symbol_fqn.strip_prefix(new_prefix.as_str()) {
                prov.anchor.symbol_fqn = format!("{old_prefix}{rest}");
            }
        } else if let Some(&claimed) = claimed_old
            .iter()
            .filter(|&&c| {
                prov.anchor.symbol_fqn == c
                    || prov
                        .anchor
                        .symbol_fqn
                        .strip_prefix(c)
                        .is_some_and(|rest| rest.starts_with("::"))
            })
            .max_by_key(|c| c.len())
        {
            if prov.anchor.symbol_fqn == claimed {
                prov.anchor.symbol_fqn = format!("{claimed}#added");
            } else if let Some(rest) = prov.anchor.symbol_fqn.strip_prefix(claimed) {
                prov.anchor.symbol_fqn = format!("{claimed}#added{rest}");
            }
        }
    }
}

fn remap_sig_provides(
    provides: &mut [ProvidedSymbol],
    sig_remap: &HashMap<(String, u16), String>,
) {
    let claimed_old: HashSet<(&str, u16)> = sig_remap
        .iter()
        .map(|((_, kind_id), old_fqn)| (old_fqn.as_str(), *kind_id))
        .collect();
    for prov in provides {
        let key = (prov.anchor.symbol_fqn.clone(), prov.anchor.syntax_kind_id);
        if let Some(old_fqn) = sig_remap.get(&key) {
            prov.anchor.symbol_fqn = old_fqn.clone();
        } else if claimed_old.contains(&(
            prov.anchor.symbol_fqn.as_str(),
            prov.anchor.syntax_kind_id,
        )) {
            prov.anchor.symbol_fqn = format!("{}#added", prov.anchor.symbol_fqn);
        }
    }
}

fn is_disambiguable_container(u: &DeclUnit, rel_fqn: &str) -> bool {
    if !u.is_container {
        return false;
    }
    if u.kind == SymbolKind::Impl {
        return true;
    }
    u.language == LanguageKind::Go
        && matches!(
            strip_impl_disambiguator(rel_fqn).rsplit("::").next(),
            Some("const_group" | "var_group" | "type_group" | "import_group")
        )
}

#[allow(clippy::too_many_arguments)]
fn diff_unit_lists(
    parent_anchor: &NodeAnchor,
    old_scope_prefix: &str,
    new_scope_prefix: &str,
    old_units: &[DeclUnit],
    new_units: &[DeclUnit],
    old_src: &str,
    new_src: &str,
    ops: &mut Vec<SemanticOp>,
    container_remap: &mut HashMap<String, String>,
    sig_remap: &mut HashMap<(String, u16), String>,
) {
    let mut old_used = vec![false; old_units.len()];
    let mut new_matched_old: Vec<Option<usize>> = vec![None; new_units.len()];

    // Pass 0: Disambiguate positional container blocks (Rust impl blocks and Go
    // const_group/var_group/type_group blocks) sharing a stripped base FQN by
    // best child-FQN overlap before falling back to exact positional FQN matching.
    type ContainerGroupKey<'a> = (Blake3Hash, u16, &'a str);
    let mut container_groups: BTreeMap<ContainerGroupKey<'_>, (Vec<usize>, Vec<usize>)> =
        BTreeMap::new();
    for (oi, ou) in old_units.iter().enumerate() {
        let rel = relative_fqn(&ou.anchor.symbol_fqn, old_scope_prefix);
        if is_disambiguable_container(ou, rel) {
            let key = (
                ou.anchor.logical_file_id,
                ou.anchor.syntax_kind_id,
                strip_impl_disambiguator(rel),
            );
            container_groups.entry(key).or_default().0.push(oi);
        }
    }
    for (ni, nu) in new_units.iter().enumerate() {
        let rel = relative_fqn(&nu.anchor.symbol_fqn, new_scope_prefix);
        if is_disambiguable_container(nu, rel) {
            let key = (
                nu.anchor.logical_file_id,
                nu.anchor.syntax_kind_id,
                strip_impl_disambiguator(rel),
            );
            container_groups.entry(key).or_default().1.push(ni);
        }
    }
    for (_key, (old_idxs, new_idxs)) in container_groups {
        if old_idxs.is_empty() || new_idxs.is_empty() {
            continue;
        }
        if old_idxs.len() == 1
            && new_idxs.len() == 1
            && old_units[old_idxs[0]].anchor.symbol_fqn
                == new_units[new_idxs[0]].anchor.symbol_fqn
        {
            continue;
        }
        let mut candidates: Vec<((usize, usize, usize), usize, usize)> = Vec::new();
        for &oi in &old_idxs {
            for &ni in &new_idxs {
                let score = impl_match_score(&old_units[oi], &new_units[ni]);
                candidates.push((score, oi, ni));
            }
        }
        candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| (a.1, a.2).cmp(&(b.1, b.2))));
        for (score, oi, ni) in candidates {
            if score.0 == 0 {
                continue;
            }
            if !old_used[oi]
                && new_matched_old[ni].is_none()
                && old_units[oi].is_container == new_units[ni].is_container
                && old_units[oi].body_range.is_some() == new_units[ni].body_range.is_some()
            {
                old_used[oi] = true;
                new_matched_old[ni] = Some(oi);
                emit_matched_unit_ops(
                    &old_units[oi],
                    &new_units[ni],
                    old_src,
                    new_src,
                    ops,
                    container_remap,
                    sig_remap,
                );
            }
        }
    }

    // Pass 0b: Disambiguate TypeScript/TSX and Python @overload declarations (#sig:N)
    // by exact decl_header_hash before positional matching so deleting/prepending an
    // overload preserves surviving shifted overloads and suppresses spurious deltas.
    type OverloadGroupKey<'a> = (Blake3Hash, u16, &'a str);
    let mut overload_groups: BTreeMap<OverloadGroupKey<'_>, (Vec<usize>, Vec<usize>)> =
        BTreeMap::new();
    for (oi, ou) in old_units.iter().enumerate() {
        if old_used[oi]
            || ou.is_container
            || !(matches!(ou.language, LanguageKind::TypeScript | LanguageKind::Tsx)
                || (ou.language == LanguageKind::Python
                    && has_sig_disambiguator(&ou.anchor.symbol_fqn)))
        {
            continue;
        }
        let rel = relative_fqn(&ou.anchor.symbol_fqn, old_scope_prefix);
        let key = (
            ou.anchor.logical_file_id,
            ou.anchor.syntax_kind_id,
            strip_sig_disambiguator(rel),
        );
        overload_groups.entry(key).or_default().0.push(oi);
    }
    for (ni, nu) in new_units.iter().enumerate() {
        if new_matched_old[ni].is_some()
            || nu.is_container
            || !(matches!(nu.language, LanguageKind::TypeScript | LanguageKind::Tsx)
                || (nu.language == LanguageKind::Python
                    && has_sig_disambiguator(&nu.anchor.symbol_fqn)))
        {
            continue;
        }
        let rel = relative_fqn(&nu.anchor.symbol_fqn, new_scope_prefix);
        let key = (
            nu.anchor.logical_file_id,
            nu.anchor.syntax_kind_id,
            strip_sig_disambiguator(rel),
        );
        overload_groups.entry(key).or_default().1.push(ni);
    }
    for (_key, (old_idxs, new_idxs)) in overload_groups {
        if old_idxs.is_empty() || new_idxs.is_empty() {
            continue;
        }
        let any_has_sig = old_idxs
            .iter()
            .any(|&oi| has_sig_disambiguator(&old_units[oi].anchor.symbol_fqn))
            || new_idxs
                .iter()
                .any(|&ni| has_sig_disambiguator(&new_units[ni].anchor.symbol_fqn));
        if !any_has_sig {
            continue;
        }
        for &oi in &old_idxs {
            if old_used[oi] {
                continue;
            }
            if let Some(&ni) = new_idxs.iter().find(|&&ni| {
                new_matched_old[ni].is_none()
                    && old_units[oi].body_range.is_some() == new_units[ni].body_range.is_some()
                    && old_units[oi].anchor.decl_header_hash
                        == new_units[ni].anchor.decl_header_hash
            }) {
                old_used[oi] = true;
                new_matched_old[ni] = Some(oi);
                emit_matched_unit_ops(
                    &old_units[oi],
                    &new_units[ni],
                    old_src,
                    new_src,
                    ops,
                    container_remap,
                    sig_remap,
                );
            }
        }
    }

    // Pass 1: Exact NodeAnchor match (same logical_file_id, relative symbol_fqn, syntax_kind_id, decl_header_hash)
    for (ni, nu) in new_units.iter().enumerate() {
        if new_matched_old[ni].is_some() {
            continue;
        }
        let n_rel = relative_fqn(&nu.anchor.symbol_fqn, new_scope_prefix);
        if let Some(oi) = old_units.iter().enumerate().position(|(oi, ou)| {
            !old_used[oi]
                && ou.is_container == nu.is_container
                && ou.body_range.is_some() == nu.body_range.is_some()
                && ou.anchor.logical_file_id == nu.anchor.logical_file_id
                && ou.anchor.syntax_kind_id == nu.anchor.syntax_kind_id
                && ou.anchor.decl_header_hash == nu.anchor.decl_header_hash
                && relative_fqn(&ou.anchor.symbol_fqn, old_scope_prefix) == n_rel
        }) {
            old_used[oi] = true;
            new_matched_old[ni] = Some(oi);
            emit_matched_unit_ops(
                &old_units[oi],
                nu,
                old_src,
                new_src,
                ops,
                container_remap,
                sig_remap,
            );
        }
    }

    // Pass 2: Signature mutation match (same relative symbol_fqn + syntax_kind_id, changed decl_header_hash)
    for (ni, nu) in new_units.iter().enumerate() {
        if new_matched_old[ni].is_some() {
            continue;
        }
        let n_rel = relative_fqn(&nu.anchor.symbol_fqn, new_scope_prefix);
        if let Some(oi) = old_units.iter().enumerate().position(|(oi, ou)| {
            !old_used[oi]
                && ou.is_container == nu.is_container
                && ou.body_range.is_some() == nu.body_range.is_some()
                && ou.anchor.logical_file_id == nu.anchor.logical_file_id
                && relative_fqn(&ou.anchor.symbol_fqn, old_scope_prefix) == n_rel
                && ou.anchor.syntax_kind_id == nu.anchor.syntax_kind_id
        }) {
            old_used[oi] = true;
            new_matched_old[ni] = Some(oi);
            emit_matched_unit_ops(
                &old_units[oi],
                nu,
                old_src,
                new_src,
                ops,
                container_remap,
                sig_remap,
            );
        }
    }

    // Pass 3: Unmatched new units -> InsertMember (in source order)
    let old_unit_keys: HashSet<(String, u16)> = old_units
        .iter()
        .map(|ou| (ou.anchor.symbol_fqn.clone(), ou.anchor.syntax_kind_id))
        .collect();
    let mut emitted_new_anchors: Vec<Option<NodeAnchor>> = vec![None; new_units.len()];

    for (ni, nu) in new_units.iter().enumerate() {
        if new_matched_old[ni].is_some() {
            continue;
        }
        let mut member_anchor = requalify_anchor(&nu.anchor, old_scope_prefix, new_scope_prefix);
        if old_unit_keys.contains(&(member_anchor.symbol_fqn.clone(), member_anchor.syntax_kind_id))
        {
            member_anchor.symbol_fqn = format!("{}#added", member_anchor.symbol_fqn);
        }
        let after_anchor = if ni == 0 {
            None
        } else if let Some(prev_oi) = new_matched_old[ni - 1] {
            Some(old_units[prev_oi].anchor.clone())
        } else {
            Some(emitted_new_anchors[ni - 1].clone().unwrap_or_else(|| {
                requalify_anchor(&new_units[ni - 1].anchor, old_scope_prefix, new_scope_prefix)
            }))
        };
        emitted_new_anchors[ni] = Some(member_anchor.clone());
        let full_text_with_trivia = new_src[nu.trivia_start_byte..nu.end_byte].to_string();
        ops.push(SemanticOp::InsertMember {
            parent_anchor: parent_anchor.clone(),
            member_anchor,
            full_text_with_trivia,
            after_anchor,
        });
    }

    // Pass 4: Unmatched old units -> RemoveNode
    for (oi, ou) in old_units.iter().enumerate() {
        if old_used[oi] {
            continue;
        }
        let full = &old_src[ou.trivia_start_byte..ou.end_byte];
        ops.push(SemanticOp::RemoveNode {
            target: ou.anchor.clone(),
            removed_hash: Blake3Hash::from_bytes(full.as_bytes()),
        });
    }
}

fn emit_matched_unit_ops(
    ou: &DeclUnit,
    nu: &DeclUnit,
    old_src: &str,
    new_src: &str,
    ops: &mut Vec<SemanticOp>,
    container_remap: &mut HashMap<String, String>,
    sig_remap: &mut HashMap<(String, u16), String>,
) {
    if is_disambiguable_container(ou, &ou.anchor.symbol_fqn)
        && ou.anchor.symbol_fqn != nu.anchor.symbol_fqn
    {
        container_remap.insert(nu.anchor.symbol_fqn.clone(), ou.anchor.symbol_fqn.clone());
    }
    if !ou.is_container
        && matches!(
            ou.language,
            LanguageKind::TypeScript | LanguageKind::Tsx | LanguageKind::Python
        )
        && ou.anchor.symbol_fqn != nu.anchor.symbol_fqn
        && (has_sig_disambiguator(&ou.anchor.symbol_fqn)
            || has_sig_disambiguator(&nu.anchor.symbol_fqn))
    {
        sig_remap.insert(
            (nu.anchor.symbol_fqn.clone(), nu.anchor.syntax_kind_id),
            ou.anchor.symbol_fqn.clone(),
        );
    }

    let old_header = &old_src[ou.header_range.clone()];
    let new_header = &new_src[nu.header_range.clone()];

    let inline_reindent_only = ou.parent_anchor.is_some()
        && ou.anchor.decl_header_hash == nu.anchor.decl_header_hash
        && old_header.trim_start_matches([' ', '\t'])
            == new_header.trim_start_matches([' ', '\t']);

    if !inline_reindent_only
        && (ou.anchor.decl_header_hash != nu.anchor.decl_header_hash || old_header != new_header)
    {
        ops.push(SemanticOp::MutateSignature {
            target: ou.anchor.clone(),
            old_header_hash: ou.anchor.decl_header_hash,
            new_header_text: new_header.to_string(),
            new_header_hash: nu.anchor.decl_header_hash,
        });
    }

    if ou.is_container {
        diff_unit_lists(
            &ou.anchor,
            &ou.anchor.symbol_fqn,
            &nu.anchor.symbol_fqn,
            &ou.children,
            &nu.children,
            old_src,
            new_src,
            ops,
            container_remap,
            sig_remap,
        );
    } else {
        let old_body = ou.body_range.as_ref().map(|r| &old_src[r.clone()]);
        let new_body = nu.body_range.as_ref().map(|r| &new_src[r.clone()]);
        match (old_body, new_body) {
            (Some(ob), Some(nb)) if ob != nb => {
                ops.push(SemanticOp::ReplaceBody {
                    target: ou.anchor.clone(),
                    old_body_hash: Blake3Hash::from_bytes(ob.as_bytes()),
                    new_body_text: nb.to_string(),
                });
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_file_to_patch_strict_rejects_syntax_errors_while_non_strict_falls_back() {
        let path = Path::new("src/lib.rs");
        let ctx = DiffContext {
            logical_file_id: compute_logical_file_id(path),
            file_path: path,
            language: LanguageKind::Rust,
            viewport_id: "vp1",
            actor_id: "agent1",
            lamport_ts: 1,
            parents: Vec::new(),
        };
        let old_bytes = b"pub fn ok() -> i32 { 1 }\n";
        let broken_bytes = b"pub fn broken( -> i32 { let x = ;;; }\n";

        let strict_err = diff_file_to_patch_strict(&ctx, old_bytes, broken_bytes, None, None);
        assert!(matches!(strict_err, Err(CstError::SyntaxError { .. })));

        let non_strict = diff_file_to_patch(&ctx, old_bytes, broken_bytes, None, None).unwrap();
        assert!(non_strict
            .ops
            .iter()
            .any(|op| matches!(op, SemanticOp::TextCrdtFallback { .. })));
    }
}
