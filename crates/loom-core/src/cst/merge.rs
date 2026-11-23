use super::anchor::{DeclUnit, SymbolKind, extract_decl_units, flatten_decl_units};
use super::diff::compute_logical_file_id;
use super::lang::{CstError, ParsedFile, collect_syntax_errors, parse_source};
use super::symbols::{ExtractedSymbols, extract_file_symbols};
use crate::cas::{CasError, CasStore};
use crate::crdt;
use crate::model::{
    AstPatch, BinaryManifest, Blake3Hash, LanguageKind, MergeOutcome, NodeAnchor, SemanticOp,
    SymbolContractDelta,
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergedFileContent {
    Text(String),
    Binary(BinaryManifest),
}

impl MergedFileContent {
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(s) => Some(s),
            Self::Binary(_) => None,
        }
    }

    pub fn as_binary(&self) -> Option<&BinaryManifest> {
        match self {
            Self::Binary(m) => Some(m),
            Self::Text(_) => None,
        }
    }

    pub fn into_bytes(self, cas: Option<&CasStore>) -> Result<Vec<u8>, CstError> {
        match self {
            Self::Text(s) => Ok(s.into_bytes()),
            Self::Binary(m) => {
                if let Some(store) = cas {
                    Ok(store.get_chunked_binary(&m)?)
                } else {
                    Ok(Vec::new())
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeReport {
    pub outcome: MergeOutcome,
    pub conflicts: Vec<MergeOutcome>,
    pub merged_files: BTreeMap<PathBuf, MergedFileContent>,
    pub applied_patches: Vec<Blake3Hash>,
}

impl MergeReport {
    pub fn is_clean(&self) -> bool {
        matches!(self.outcome, MergeOutcome::Clean { .. })
    }

    pub fn text(&self, path: &Path) -> Option<&str> {
        self.merged_files.get(path).and_then(|c| c.as_text())
    }

    pub fn binary(&self, path: &Path) -> Option<&BinaryManifest> {
        self.merged_files.get(path).and_then(|c| c.as_binary())
    }
}

#[derive(Debug, Clone)]
pub struct MergeState {
    pub base_files: BTreeMap<PathBuf, Vec<u8>>,
    pub patches: BTreeMap<(u64, Blake3Hash), AstPatch>,
    pub report: MergeReport,
}

impl MergeState {
    pub fn new(base_files: BTreeMap<PathBuf, Vec<u8>>) -> Self {
        let mut merged_files = BTreeMap::new();
        for (path, bytes) in &base_files {
            if let Ok(text) = std::str::from_utf8(bytes) {
                merged_files.insert(path.clone(), MergedFileContent::Text(text.to_string()));
            }
        }
        let paths: Vec<PathBuf> = merged_files.keys().cloned().collect();
        Self {
            base_files,
            patches: BTreeMap::new(),
            report: MergeReport {
                outcome: MergeOutcome::Clean {
                    merged_files: paths,
                    applied_patches: Vec::new(),
                },
                conflicts: Vec::new(),
                merged_files,
                applied_patches: Vec::new(),
            },
        }
    }

    pub fn from_patches(
        base_files: BTreeMap<PathBuf, Vec<u8>>,
        patches: &[AstPatch],
        cas: Option<&CasStore>,
    ) -> Result<Self, CstError> {
        let mut map = BTreeMap::new();
        for p in patches {
            let mut normalized = p.clone();
            if normalized.patch_hash == Blake3Hash::ZERO {
                normalized.finalize_hash();
            }
            map.insert((normalized.lamport_ts, normalized.patch_hash), normalized);
        }
        let ordered: Vec<AstPatch> = map.values().cloned().collect();
        let report = merge_patches(&base_files, &ordered, cas)?;
        Ok(Self {
            base_files,
            patches: map,
            report,
        })
    }

    pub fn apply(&self, patch: AstPatch, cas: Option<&CasStore>) -> Result<Self, CstError> {
        let mut combined = self.patches.clone();
        let mut normalized = patch;
        if normalized.patch_hash == Blake3Hash::ZERO {
            normalized.finalize_hash();
        }
        combined.insert((normalized.lamport_ts, normalized.patch_hash), normalized);
        let ordered: Vec<AstPatch> = combined.values().cloned().collect();
        let report = merge_patches(&self.base_files, &ordered, cas)?;
        Ok(Self {
            base_files: self.base_files.clone(),
            patches: combined,
            report,
        })
    }

    pub fn compose(a: &Self, b: &Self, cas: Option<&CasStore>) -> Result<Self, CstError> {
        let mut combined = a.patches.clone();
        for (k, v) in &b.patches {
            combined.insert(*k, v.clone());
        }
        let ordered: Vec<AstPatch> = combined.values().cloned().collect();
        let report = merge_patches(&a.base_files, &ordered, cas)?;
        Ok(Self {
            base_files: a.base_files.clone(),
            patches: combined,
            report,
        })
    }
}

type NodeKey = (Blake3Hash, String, u16);

fn node_key(anchor: &NodeAnchor) -> NodeKey {
    (
        anchor.logical_file_id,
        anchor.symbol_fqn.clone(),
        anchor.syntax_kind_id,
    )
}

fn canonical_patch_hash(p: &AstPatch) -> Blake3Hash {
    if p.patch_hash == Blake3Hash::ZERO {
        p.compute_content_hash()
    } else {
        p.patch_hash
    }
}

fn sort_and_dedup_patches(patches: &[AstPatch]) -> Vec<AstPatch> {
    let mut map: BTreeMap<(u64, Blake3Hash), AstPatch> = BTreeMap::new();
    for p in patches {
        let mut np = p.clone();
        np.patch_hash = canonical_patch_hash(&np);
        map.insert((np.lamport_ts, np.patch_hash), np);
    }
    map.into_values().collect()
}

pub fn merge_patches(
    base_files: &BTreeMap<PathBuf, Vec<u8>>,
    patches: &[AstPatch],
    cas: Option<&CasStore>,
) -> Result<MergeReport, CstError> {
    let ordered = sort_and_dedup_patches(patches);
    let applied_patches: Vec<Blake3Hash> = ordered.iter().map(|p| p.patch_hash).collect();

    let mut merged_files: BTreeMap<PathBuf, MergedFileContent> = BTreeMap::new();
    for (path, bytes) in base_files {
        if let Ok(s) = std::str::from_utf8(bytes) {
            merged_files.insert(path.clone(), MergedFileContent::Text(s.to_string()));
        }
    }

    if ordered.is_empty() {
        let paths: Vec<PathBuf> = merged_files.keys().cloned().collect();
        return Ok(MergeReport {
            outcome: MergeOutcome::Clean {
                merged_files: paths,
                applied_patches: applied_patches.clone(),
            },
            conflicts: Vec::new(),
            merged_files,
            applied_patches,
        });
    }

    // Resolve each logical_file_id to its base path and bytes.
    let mut fid_to_base_path: HashMap<Blake3Hash, PathBuf> = HashMap::new();
    let mut path_to_fid: HashMap<PathBuf, Blake3Hash> = HashMap::new();
    for path in base_files.keys() {
        let fid = compute_logical_file_id(path);
        fid_to_base_path.insert(fid, path.clone());
        path_to_fid.insert(path.clone(), fid);
    }
    for p in &ordered {
        fid_to_base_path.entry(p.logical_file_id).or_insert_with(|| {
            // Check if any RenameOrMoveFile in this patch specifies old_path in base_files
            for op in &p.ops {
                if let SemanticOp::RenameOrMoveFile { old_path, .. } = op
                    && base_files.contains_key(old_path)
                {
                    return old_path.clone();
                }
            }
            p.file_path.clone()
        });
    }

    // Group patches by logical_file_id
    let mut by_fid: BTreeMap<Blake3Hash, Vec<&AstPatch>> = BTreeMap::new();
    for p in &ordered {
        by_fid.entry(p.logical_file_id).or_default().push(p);
    }

    let mut conflicts: Vec<MergeOutcome> = Vec::new();
    let mut final_path_map: HashMap<Blake3Hash, PathBuf> = HashMap::new();

    // Check cross-file RenameOrMoveFile collisions (including against untouched base_files)
    let mut moved_from_paths: HashSet<PathBuf> = HashSet::new();
    let mut fid_renames: HashMap<Blake3Hash, (PathBuf, PathBuf, Blake3Hash)> = HashMap::new();
    for (&fid, group) in &by_fid {
        let base_path = fid_to_base_path
            .get(&fid)
            .cloned()
            .unwrap_or_else(|| group[0].file_path.clone());
        let mut renames: Vec<(PathBuf, Blake3Hash)> = Vec::new();
        for p in group {
            for op in &p.ops {
                if let SemanticOp::RenameOrMoveFile { new_path, .. } = op {
                    renames.push((new_path.clone(), p.patch_hash));
                }
            }
        }
        let mut final_path = base_path.clone();
        if let Some((first_p, first_h)) = renames.first() {
            final_path = first_p.clone();
            for (other_p, other_h) in renames.iter().skip(1) {
                if other_p != first_p {
                    conflicts.push(MergeOutcome::StructuralConflict {
                        file: base_path.clone(),
                        anchor: None,
                        patch_a: *first_h,
                        patch_b: *other_h,
                        reason: format!(
                            "conflicting RenameOrMoveFile: '{}' vs '{}'",
                            first_p.display(),
                            other_p.display()
                        ),
                    });
                }
            }
        }
        if final_path != base_path {
            moved_from_paths.insert(base_path.clone());
        }
        let cur_h = renames
            .first()
            .map(|x| x.1)
            .unwrap_or(group[0].patch_hash);
        fid_renames.insert(fid, (base_path, final_path, cur_h));
    }

    let mut target_path_owners: BTreeMap<PathBuf, (Blake3Hash, Blake3Hash)> = BTreeMap::new();
    for &fid in by_fid.keys() {
        let (base_path, final_path, cur_h) = fid_renames.get(&fid).unwrap().clone();
        if let Some((prev_fid, prev_h)) = target_path_owners.get(&final_path)
            && *prev_fid != fid
        {
            conflicts.push(MergeOutcome::StructuralConflict {
                file: final_path.clone(),
                anchor: None,
                patch_a: *prev_h,
                patch_b: cur_h,
                reason: format!(
                    "multiple logical files moved/created at '{}'",
                    final_path.display()
                ),
            });
        } else if final_path != base_path
            && base_files.contains_key(&final_path)
            && !moved_from_paths.contains(&final_path)
        {
            conflicts.push(MergeOutcome::StructuralConflict {
                file: final_path.clone(),
                anchor: None,
                patch_a: cur_h,
                patch_b: cur_h,
                reason: format!(
                    "RenameOrMoveFile target '{}' already exists in base tree",
                    final_path.display()
                ),
            });
        } else {
            target_path_owners.insert(final_path.clone(), (fid, cur_h));
        }
        final_path_map.insert(fid, final_path);
    }

    // Pre-remove all vacated source paths before inserting renamed destinations,
    // so chained (A -> B, B -> C) or swap (A <-> B) renames cannot delete an
    // earlier iteration's inserted destination when iterating in Blake3Hash order.
    for moved_src in &moved_from_paths {
        merged_files.remove(moved_src);
    }

    // Process each logical file
    for (&fid, group) in &by_fid {
        let base_path = fid_to_base_path
            .get(&fid)
            .cloned()
            .unwrap_or_else(|| group[0].file_path.clone());
        let dest_path = final_path_map
            .get(&fid)
            .cloned()
            .unwrap_or_else(|| base_path.clone());

        let base_bytes = base_files
            .get(&base_path)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);

        // Check binary manifests
        let binary_patches: Vec<(&AstPatch, &BinaryManifest)> = group
            .iter()
            .filter_map(|p| p.binary_manifest.as_ref().map(|m| (*p, m)))
            .collect();
        if !binary_patches.is_empty() {
            let first = binary_patches[0];
            let non_binary_patch = group.iter().copied().find(|p| {
                p.ops.iter().any(|op| {
                    matches!(
                        op,
                        SemanticOp::TextCrdtFallback { .. }
                            | SemanticOp::InsertMember { .. }
                            | SemanticOp::ReplaceBody { .. }
                            | SemanticOp::MutateSignature { .. }
                            | SemanticOp::RemoveNode { .. }
                    )
                })
            });
            if let Some(p_non_bin) = non_binary_patch {
                let p_bin = first.0;
                let (pa, pb) = if (p_bin.lamport_ts, p_bin.patch_hash)
                    <= (p_non_bin.lamport_ts, p_non_bin.patch_hash)
                {
                    (p_bin.patch_hash, p_non_bin.patch_hash)
                } else {
                    (p_non_bin.patch_hash, p_bin.patch_hash)
                };
                conflicts.push(MergeOutcome::StructuralConflict {
                    file: dest_path.clone(),
                    anchor: None,
                    patch_a: pa,
                    patch_b: pb,
                    reason:
                        "cannot merge binary manifest with text/CST semantic ops on the same file"
                            .into(),
                });
                continue;
            }
            let mut bin_conflict = false;
            for other in binary_patches.iter().skip(1) {
                if other.1.file_hash != first.1.file_hash {
                    conflicts.push(MergeOutcome::StructuralConflict {
                        file: dest_path.clone(),
                        anchor: None,
                        patch_a: first.0.patch_hash,
                        patch_b: other.0.patch_hash,
                        reason: format!(
                            "divergent concurrent binary edits ({} vs {})",
                            first.1.file_hash.short_hex(12),
                            other.1.file_hash.short_hex(12)
                        ),
                    });
                    bin_conflict = true;
                }
            }
            if !bin_conflict {
                if let Some(store) = cas
                    && !store.verify_binary_manifest(first.1)?
                {
                    return Err(CstError::Cas(CasError::InvalidManifest(
                        "binary manifest verification failed: missing or corrupt chunk".into(),
                    )));
                }
                merged_files.insert(dest_path, MergedFileContent::Binary(first.1.clone()));
            }
            continue;
        }

        let base_str = std::str::from_utf8(base_bytes)
            .map_err(|_| CstError::InvalidUtf8(base_path.display().to_string()))?;

        // Detect mixed CRDT vs CST ops
        let has_crdt = group.iter().any(|p| {
            p.ops
                .iter()
                .any(|op| matches!(op, SemanticOp::TextCrdtFallback { .. }))
        });
        let has_cst = group.iter().any(|p| {
            p.ops.iter().any(|op| {
                matches!(
                    op,
                    SemanticOp::InsertMember { .. }
                        | SemanticOp::ReplaceBody { .. }
                        | SemanticOp::MutateSignature { .. }
                        | SemanticOp::RemoveNode { .. }
                )
            })
        });

        if has_crdt && has_cst {
            let p_crdt = group
                .iter()
                .find(|p| {
                    p.ops
                        .iter()
                        .any(|op| matches!(op, SemanticOp::TextCrdtFallback { .. }))
                })
                .unwrap();
            let p_cst = group
                .iter()
                .find(|p| {
                    p.ops.iter().any(|op| {
                        matches!(
                            op,
                            SemanticOp::InsertMember { .. }
                                | SemanticOp::ReplaceBody { .. }
                                | SemanticOp::MutateSignature { .. }
                                | SemanticOp::RemoveNode { .. }
                        )
                    })
                })
                .unwrap();
            let (pa, pb) = if (p_crdt.lamport_ts, p_crdt.patch_hash)
                <= (p_cst.lamport_ts, p_cst.patch_hash)
            {
                (p_crdt.patch_hash, p_cst.patch_hash)
            } else {
                (p_cst.patch_hash, p_crdt.patch_hash)
            };
            conflicts.push(MergeOutcome::StructuralConflict {
                file: dest_path.clone(),
                anchor: None,
                patch_a: pa,
                patch_b: pb,
                reason: "cannot merge TextCrdtFallback with CST semantic ops on the same file"
                    .into(),
            });
            continue;
        }

        if has_crdt {
            let lang = LanguageKind::detect(&dest_path, base_bytes);
            let inputs: Vec<crdt::CrdtPatchInput<'_>> = group
                .iter()
                .map(|p| {
                    let blobs: Vec<&[u8]> = p
                        .ops
                        .iter()
                        .filter_map(|op| match op {
                            SemanticOp::TextCrdtFallback {
                                automerge_changes, ..
                            } => Some(automerge_changes.as_slice()),
                            _ => None,
                        })
                        .collect();
                    crdt::CrdtPatchInput {
                        patch_hash: p.patch_hash,
                        actor_id: &p.actor_id,
                        lamport_ts: p.lamport_ts,
                        parents: &p.parents,
                        blobs,
                    }
                })
                .collect();
            let pa = group.first().map(|p| p.patch_hash).unwrap_or(Blake3Hash::ZERO);
            let pb = group
                .get(1)
                .map(|p| p.patch_hash)
                .unwrap_or(pa);
            let merged_text = crdt::merge_crdt_patch_dag(base_str, &inputs)?;
            let (all_empty, delete_vs_modify) = resolve_crdt_branch_tips(base_str, &inputs);
            if merged_text.is_empty() && !base_str.is_empty() && all_empty {
                merged_files.remove(&dest_path);
            } else if delete_vs_modify {
                conflicts.push(MergeOutcome::StructuralConflict {
                    file: dest_path,
                    anchor: None,
                    patch_a: pa,
                    patch_b: pb,
                    reason: "delete-vs-modify conflict on CRDT file".into(),
                });
            } else {
                match crdt::validate_merged_non_code(lang, &dest_path, merged_text, pa, pb)? {
                    Ok(valid_text) => {
                        merged_files.insert(dest_path, MergedFileContent::Text(valid_text));
                    }
                    Err(conflict) => {
                        conflicts.push(conflict);
                    }
                }
            }
            continue;
        }

        // CST merge path
        let lang = LanguageKind::from_path(&dest_path);
        if !lang.is_code() {
            merged_files.insert(dest_path, MergedFileContent::Text(base_str.to_string()));
            continue;
        }

        let file_conflicts =
            detect_cst_structural_conflicts(lang, fid, &dest_path, base_str, group);
        if !file_conflicts.is_empty() {
            conflicts.extend(file_conflicts);
            continue;
        }

        match apply_cst_ops_for_file(lang, fid, &dest_path, base_str, group)? {
            Ok(merged_text) => {
                merged_files.insert(dest_path, MergedFileContent::Text(merged_text));
            }
            Err(conflict) => {
                conflicts.push(conflict);
            }
        }
    }

    // Run semantic conflict detection across all patches in the merge set.
    let sem_conflicts = detect_semantic_conflicts(base_files, &ordered, &fid_to_base_path)?;
    conflicts.extend(sem_conflicts);

    let outcome = if let Some(first_conflict) = conflicts.first() {
        first_conflict.clone()
    } else {
        let mut paths: Vec<PathBuf> = merged_files.keys().cloned().collect();
        paths.sort();
        MergeOutcome::Clean {
            merged_files: paths,
            applied_patches: applied_patches.clone(),
        }
    };

    Ok(MergeReport {
        outcome,
        conflicts,
        merged_files,
        applied_patches,
    })
}

pub fn merge_trees(
    base_files: &BTreeMap<PathBuf, Vec<u8>>,
    patch_sets: &[&[AstPatch]],
    cas: Option<&CasStore>,
) -> Result<MergeReport, CstError> {
    let mut all_patches = Vec::new();
    for ps in patch_sets {
        all_patches.extend_from_slice(ps);
    }
    let mut report = merge_patches(base_files, &all_patches, cas)?;
    let ordered = sort_and_dedup_patches(&all_patches);

    // Additionally check unmodified base files across the workspace for cross-file SemanticConflicts
    // (e.g., producer changed a symbol in file A, while untouched file B in the tree consumes it).
    let mut touched_paths: BTreeSet<PathBuf> = BTreeSet::new();
    let mut touched_fids: HashSet<Blake3Hash> = HashSet::new();
    for p in &ordered {
        touched_paths.insert(p.file_path.clone());
        touched_fids.insert(p.logical_file_id);
        for op in &p.ops {
            if let SemanticOp::RenameOrMoveFile {
                old_path,
                new_path,
                ..
            } = op
            {
                touched_paths.insert(old_path.clone());
                touched_paths.insert(new_path.clone());
            }
        }
    }
    let mut extra_sem = Vec::new();

    struct TreeSemCache {
        patch_syms: HashMap<Blake3Hash, (ExtractedSymbols, Vec<Range<usize>>)>,
        fid_map: HashMap<Blake3Hash, PathBuf>,
        base_syms: HashMap<PathBuf, ExtractedSymbols>,
    }
    let mut sem_cache: Option<TreeSemCache> = None;

    for (path, bytes) in base_files {
        let fid = compute_logical_file_id(path);
        if touched_paths.contains(path) || touched_fids.contains(&fid) {
            continue;
        }
        let lang = LanguageKind::from_path(path);
        if !lang.is_code() {
            continue;
        }
        let Ok(src) = std::str::from_utf8(bytes) else {
            continue;
        };
        let parsed = parse_source(lang, src, None)?;
        let syms = extract_file_symbols(&parsed, fid, path, "", 0);

        for producer in &ordered {
            for delta in &producer.contract_deltas {
                let effective = if matches!(delta, SymbolContractDelta::RemovedSymbol { .. }) {
                    let sc = sem_cache.get_or_insert_with(|| {
                        let mut fm: HashMap<Blake3Hash, PathBuf> = base_files
                            .keys()
                            .map(|p| (compute_logical_file_id(p), p.clone()))
                            .collect();
                        for p in &ordered {
                            fm.entry(p.logical_file_id).or_insert_with(|| {
                                for op in &p.ops {
                                    if let SemanticOp::RenameOrMoveFile { old_path, .. } = op
                                        && base_files.contains_key(old_path)
                                    {
                                        return old_path.clone();
                                    }
                                }
                                p.file_path.clone()
                            });
                        }
                        let mut c = HashMap::new();
                        for p in &ordered {
                            if let Ok(res) = reconstruct_single_patch_symbols(base_files, p, &fm) {
                                c.insert(p.patch_hash, res);
                            }
                        }
                        TreeSemCache {
                            patch_syms: c,
                            fid_map: fm,
                            base_syms: HashMap::new(),
                        }
                    });
                    let batch: Vec<&AstPatch> = ordered.iter().collect();
                    resolve_effective_breaking_delta(
                        producer,
                        delta,
                        true,
                        None,
                        &batch,
                        &sc.patch_syms,
                        base_files,
                        &sc.fid_map,
                        &mut sc.base_syms,
                    )
                } else {
                    breaking_delta_info(delta, true).map(|(s, r)| (s, r, None))
                };
                let Some((sym_fqn, reason, _moved_prov)) = effective else {
                    continue;
                };
                for c in &syms.consumes {
                    if c.in_error_subtree {
                        continue;
                    }
                    if symbols_match(&c.symbol_ref, sym_fqn) {
                        extra_sem.push(MergeOutcome::SemanticConflict {
                            symbol_fqn: sym_fqn.to_string(),
                            producer_patch: producer.patch_hash,
                            consumer_patch: producer.patch_hash,
                            detail: format!(
                                "{reason}; still consumed in '{}' at line {}",
                                path.display(),
                                c.location.line
                            ),
                        });
                        break;
                    }
                }
            }
        }
    }

    if !extra_sem.is_empty() {
        for sc in extra_sem {
            if !report.conflicts.contains(&sc) {
                report.conflicts.push(sc);
            }
        }
        if matches!(report.outcome, MergeOutcome::Clean { .. })
            && let Some(first) = report.conflicts.first()
        {
            report.outcome = first.clone();
        }
    }

    Ok(report)
}

fn resolve_crdt_branch_tips<'a>(
    base_str: &str,
    inputs: &'a [crdt::CrdtPatchInput<'a>],
) -> (bool, bool) {
    if inputs.is_empty() || base_str.is_empty() {
        return (false, false);
    }
    let mut by_hash: HashMap<Blake3Hash, usize> = HashMap::with_capacity(inputs.len());
    for (idx, inp) in inputs.iter().enumerate() {
        by_hash.insert(inp.patch_hash, idx);
    }
    let mut has_child = vec![false; inputs.len()];
    for inp in inputs {
        for p in inp.parents {
            if let Some(&p_idx) = by_hash.get(p) {
                has_child[p_idx] = true;
            }
        }
    }
    let mut empty_count = 0usize;
    let mut non_empty_count = 0usize;
    for idx in 0..inputs.len() {
        if has_child[idx] {
            continue;
        }
        let mut needed = vec![false; inputs.len()];
        let mut stack = vec![idx];
        needed[idx] = true;
        while let Some(cur) = stack.pop() {
            for p in inputs[cur].parents {
                if let Some(&p_idx) = by_hash.get(p)
                    && !needed[p_idx]
                {
                    needed[p_idx] = true;
                    stack.push(p_idx);
                }
            }
        }
        let branch_inputs: Vec<crdt::CrdtPatchInput<'_>> = inputs
            .iter()
            .enumerate()
            .filter(|(i, _)| needed[*i])
            .map(|(_, x)| x.clone())
            .collect();
        if let Ok(post) = crdt::merge_crdt_patch_dag(base_str, &branch_inputs) {
            if post.is_empty() {
                empty_count += 1;
            } else {
                non_empty_count += 1;
            }
        } else {
            return (false, false);
        }
    }
    let tip_count = empty_count + non_empty_count;
    (
        tip_count > 0 && empty_count == tip_count,
        empty_count > 0 && non_empty_count > 0,
    )
}

fn collect_unit_ancestors(
    units: &[DeclUnit],
    current_ancestors: &[NodeKey],
    out: &mut HashMap<NodeKey, HashSet<NodeKey>>,
) {
    for u in units {
        let key = node_key(&u.anchor);
        if !current_ancestors.is_empty() {
            out.entry(key.clone())
                .or_default()
                .extend(current_ancestors.iter().cloned());
        }
        if !u.children.is_empty() {
            let mut next = current_ancestors.to_vec();
            next.push(key);
            collect_unit_ancestors(&u.children, &next, out);
        }
    }
}

fn build_base_ancestor_map(
    lang: LanguageKind,
    fid: Blake3Hash,
    base_str: &str,
) -> HashMap<NodeKey, HashSet<NodeKey>> {
    let mut map = HashMap::new();
    if !lang.is_code() || base_str.is_empty() {
        return map;
    }
    if let Ok(parsed) = parse_source(lang, base_str, None) {
        let units = extract_decl_units(lang, &parsed.tree, base_str, fid);
        collect_unit_ancestors(&units, &[], &mut map);
    }
    map
}

fn detect_cst_structural_conflicts(
    lang: LanguageKind,
    fid: Blake3Hash,
    file: &Path,
    base_str: &str,
    group: &[&AstPatch],
) -> Vec<MergeOutcome> {
    let mut conflicts = Vec::new();
    let ancestors = build_base_ancestor_map(lang, fid, base_str);

    for i in 0..group.len() {
        for j in (i + 1)..group.len() {
            let pa = group[i];
            let pb = group[j];
            if pa.patch_hash == pb.patch_hash {
                continue;
            }
            for op_a in &pa.ops {
                for op_b in &pb.ops {
                    if let Some(c) = check_op_pair_conflict(
                        lang,
                        file,
                        pa.patch_hash,
                        op_a,
                        pb.patch_hash,
                        op_b,
                        &[&ancestors],
                    ) {
                        conflicts.push(c);
                    }
                }
            }
        }
    }

    conflicts
}

fn strip_impl_disambiguator(fqn: &str) -> &str {
    let fqn = fqn.strip_suffix("#added").unwrap_or(fqn);
    if let Some((prefix, suffix)) = fqn.rsplit_once('#')
        && !suffix.is_empty()
        && suffix.bytes().all(|b| b.is_ascii_digit())
    {
        prefix
    } else {
        fqn
    }
}

fn is_go_decl_group_fqn(fqn: &str) -> bool {
    let stripped = strip_impl_disambiguator(fqn);
    stripped != fqn
        && matches!(
            stripped.rsplit("::").next(),
            Some("const_group" | "var_group" | "type_group" | "import_group")
        )
}

fn go_group_matches_single_kind(group_fqn: &str, single_syntax_kind_id: u16) -> bool {
    let Ok(go_lang) = super::lang::ts_language(LanguageKind::Go) else {
        return false;
    };
    let expected_kinds: &[&str] = match strip_impl_disambiguator(group_fqn).rsplit("::").next() {
        Some("const_group") => &["const_spec"],
        Some("var_group") => &["var_spec"],
        Some("type_group") => &["type_spec", "type_alias"],
        Some("import_group") => &["import_spec"],
        _ => return false,
    };
    expected_kinds.iter().any(|k| {
        let id = go_lang.id_for_node_kind(k, true);
        id != 0 && id == single_syntax_kind_id
    })
}

fn parse_go_group_members(txt: &str, logical_file_id: Blake3Hash) -> Option<Vec<String>> {
    let wrapped = format!("package __loom_probe__\n{txt}\n");
    let parsed = parse_source(LanguageKind::Go, &wrapped, None).ok()?;
    if !collect_syntax_errors(&parsed.tree).is_empty() {
        return None;
    }
    let root = parsed.tree.root_node();
    let mut cursor = root.walk();
    let mut non_trivia_nodes = root
        .named_children(&mut cursor)
        .filter(|n| !matches!(n.kind(), "package_clause" | "comment"));
    let decl_node = non_trivia_nodes.next()?;
    if non_trivia_nodes.next().is_some()
        || !matches!(
            decl_node.kind(),
            "const_declaration" | "var_declaration" | "type_declaration" | "import_declaration"
        )
    {
        return None;
    }
    let units = extract_decl_units(LanguageKind::Go, &parsed.tree, &wrapped, logical_file_id);
    if units.len() != 1
        || !units[0].is_container
        || !is_go_decl_group_fqn(&units[0].anchor.symbol_fqn)
    {
        return None;
    }
    let members = units[0]
        .children
        .iter()
        .map(|c| {
            c.anchor
                .symbol_fqn
                .strip_prefix("__loom_probe__::")
                .unwrap_or(&c.anchor.symbol_fqn)
                .to_string()
        })
        .collect();
    Some(members)
}

fn is_ts_overload_signature_kind(lang: LanguageKind, syntax_kind_id: u16) -> bool {
    if !matches!(lang, LanguageKind::TypeScript | LanguageKind::Tsx) {
        return false;
    }
    let Ok(ts_lang) = super::lang::ts_language(lang) else {
        return false;
    };
    [
        "function_signature",
        "method_signature",
        "abstract_method_signature",
    ]
    .into_iter()
    .any(|kind| {
        let id = ts_lang.id_for_node_kind(kind, true);
        id != 0 && id == syntax_kind_id
    })
}

fn is_overload_signature_anchor(lang: LanguageKind, anchor: &NodeAnchor) -> bool {
    if is_ts_overload_signature_kind(lang, anchor.syntax_kind_id) {
        return true;
    }
    if lang == LanguageKind::Python {
        let base = anchor
            .symbol_fqn
            .strip_suffix("#added")
            .unwrap_or(&anchor.symbol_fqn);
        return strip_sig_overload_suffix(base) != base;
    }
    false
}

fn parse_rust_inherent_impl_members(txt: &str, logical_file_id: Blake3Hash) -> Option<Vec<String>> {
    let parsed = parse_source(LanguageKind::Rust, txt, None).ok()?;
    if !collect_syntax_errors(&parsed.tree).is_empty() {
        return None;
    }
    let root = parsed.tree.root_node();
    let mut cursor = root.walk();
    let mut non_trivia_nodes = root.named_children(&mut cursor).filter(|n| {
        !matches!(
            n.kind(),
            "line_comment" | "block_comment" | "attribute_item"
        )
    });
    let impl_node = non_trivia_nodes.next()?;
    if non_trivia_nodes.next().is_some()
        || impl_node.kind() != "impl_item"
        || impl_node.child_by_field_name("trait").is_some()
    {
        return None;
    }

    let units = extract_decl_units(LanguageKind::Rust, &parsed.tree, txt, logical_file_id);
    if units.len() != 1 || units[0].kind != SymbolKind::Impl {
        return None;
    }
    let impl_fqn = &units[0].anchor.symbol_fqn;
    let members = units[0]
        .children
        .iter()
        .map(|c| {
            c.anchor
                .symbol_fqn
                .strip_prefix(impl_fqn.as_str())
                .and_then(|rest| rest.strip_prefix("::"))
                .unwrap_or(&c.anchor.symbol_fqn)
                .to_string()
        })
        .collect();
    Some(members)
}

fn is_ancestor_or_same_fqn(
    ancestor: &NodeAnchor,
    child: &NodeAnchor,
    ancestor_maps: &[&HashMap<NodeKey, HashSet<NodeKey>>],
) -> bool {
    if ancestor.logical_file_id != child.logical_file_id {
        return false;
    }
    if ancestor.symbol_fqn == child.symbol_fqn && ancestor.syntax_kind_id == child.syntax_kind_id
    {
        return true;
    }
    if child
        .symbol_fqn
        .starts_with(&format!("{}::", ancestor.symbol_fqn))
    {
        return true;
    }
    let anc_key = node_key(ancestor);
    let child_key = node_key(child);
    ancestor_maps.iter().any(|map| {
        map.get(&child_key)
            .is_some_and(|ancs| ancs.contains(&anc_key))
    })
}

fn check_op_pair_conflict(
    lang: LanguageKind,
    file: &Path,
    hash_a: Blake3Hash,
    op_a: &SemanticOp,
    hash_b: Blake3Hash,
    op_b: &SemanticOp,
    ancestor_maps: &[&HashMap<NodeKey, HashSet<NodeKey>>],
) -> Option<MergeOutcome> {
    match (op_a, op_b) {
        (
            SemanticOp::ReplaceBody {
                target: ta,
                new_body_text: ba,
                ..
            },
            SemanticOp::ReplaceBody {
                target: tb,
                new_body_text: bb,
                ..
            },
        ) if node_key(ta) == node_key(tb) && ba != bb => Some(MergeOutcome::StructuralConflict {
            file: file.to_path_buf(),
            anchor: Some(ta.clone()),
            patch_a: hash_a,
            patch_b: hash_b,
            reason: format!("incompatible ReplaceBody on '{}'", ta.symbol_fqn),
        }),

        (
            SemanticOp::MutateSignature {
                target: ta,
                new_header_hash: ha,
                new_header_text: s_a,
                ..
            },
            SemanticOp::MutateSignature {
                target: tb,
                new_header_hash: hb,
                new_header_text: s_b,
                ..
            },
        ) if node_key(ta) == node_key(tb) && (ha != hb || s_a != s_b) => {
            Some(MergeOutcome::StructuralConflict {
                file: file.to_path_buf(),
                anchor: Some(ta.clone()),
                patch_a: hash_a,
                patch_b: hash_b,
                reason: format!("incompatible MutateSignature on '{}'", ta.symbol_fqn),
            })
        }

        (
            SemanticOp::RemoveNode { target: rem, .. },
            SemanticOp::ReplaceBody { target: mod_t, .. }
            | SemanticOp::MutateSignature { target: mod_t, .. },
        )
        | (
            SemanticOp::ReplaceBody { target: mod_t, .. }
            | SemanticOp::MutateSignature { target: mod_t, .. },
            SemanticOp::RemoveNode { target: rem, .. },
        ) if is_ancestor_or_same_fqn(rem, mod_t, ancestor_maps) => {
            Some(MergeOutcome::StructuralConflict {
                file: file.to_path_buf(),
                anchor: Some(rem.clone()),
                patch_a: hash_a,
                patch_b: hash_b,
                reason: format!(
                    "RemoveNode on '{}' conflicts with edit on '{}'",
                    rem.symbol_fqn, mod_t.symbol_fqn
                ),
            })
        }

        (
            SemanticOp::RemoveNode { target: rem, .. },
            SemanticOp::InsertMember { parent_anchor, .. },
        )
        | (
            SemanticOp::InsertMember { parent_anchor, .. },
            SemanticOp::RemoveNode { target: rem, .. },
        ) if is_ancestor_or_same_fqn(rem, parent_anchor, ancestor_maps) => {
            Some(MergeOutcome::StructuralConflict {
                file: file.to_path_buf(),
                anchor: Some(rem.clone()),
                patch_a: hash_a,
                patch_b: hash_b,
                reason: format!(
                    "RemoveNode on container '{}' conflicts with InsertMember inside it",
                    rem.symbol_fqn
                ),
            })
        }

        (
            SemanticOp::InsertMember {
                parent_anchor: pa,
                member_anchor: ma,
                full_text_with_trivia: txt_a,
                ..
            },
            SemanticOp::InsertMember {
                parent_anchor: pb,
                member_anchor: mb,
                full_text_with_trivia: txt_b,
                ..
            },
        ) if node_key(pa) == node_key(pb) => {
            let same_member = node_key(ma) == node_key(mb);
            if same_member && txt_a == txt_b {
                None
            } else if lang == LanguageKind::Rust
                && ma.syntax_kind_id == mb.syntax_kind_id
                && strip_impl_disambiguator(&ma.symbol_fqn)
                    == strip_impl_disambiguator(&mb.symbol_fqn)
                && let (Some(members_a), Some(members_b)) = (
                    parse_rust_inherent_impl_members(txt_a, ma.logical_file_id),
                    parse_rust_inherent_impl_members(txt_b, mb.logical_file_id),
                )
            {
                let set_a: HashSet<&str> = members_a.iter().map(String::as_str).collect();
                members_b
                    .iter()
                    .find(|m| set_a.contains(m.as_str()))
                    .map(|colliding| MergeOutcome::StructuralConflict {
                        file: file.to_path_buf(),
                        anchor: Some(ma.clone()),
                        patch_a: hash_a,
                        patch_b: hash_b,
                        reason: format!(
                            "conflicting InsertMember for inherent impl '{}': duplicate member '{}' in '{}'",
                            strip_impl_disambiguator(&ma.symbol_fqn),
                            colliding,
                            pa.symbol_fqn
                        ),
                    })
            } else if lang == LanguageKind::Go
                && ma.syntax_kind_id == mb.syntax_kind_id
                && strip_impl_disambiguator(&ma.symbol_fqn)
                    == strip_impl_disambiguator(&mb.symbol_fqn)
                && is_go_decl_group_fqn(&ma.symbol_fqn)
                && let (Some(members_a), Some(members_b)) = (
                    parse_go_group_members(txt_a, ma.logical_file_id),
                    parse_go_group_members(txt_b, mb.logical_file_id),
                )
            {
                let is_import_group = strip_impl_disambiguator(&ma.symbol_fqn)
                    .rsplit("::")
                    .next()
                    == Some("import_group");
                let colliding = if is_import_group {
                    let set_a: HashSet<&str> = members_a.iter().map(String::as_str).collect();
                    members_b.iter().map(String::as_str).find(|m| set_a.contains(m))
                } else {
                    let set_a: HashSet<&str> =
                        members_a.iter().flat_map(|m| m.split(',')).collect();
                    members_b
                        .iter()
                        .flat_map(|m| m.split(','))
                        .find(|m| set_a.contains(m))
                };
                colliding.map(|colliding| MergeOutcome::StructuralConflict {
                    file: file.to_path_buf(),
                    anchor: Some(ma.clone()),
                    patch_a: hash_a,
                    patch_b: hash_b,
                    reason: format!(
                        "conflicting InsertMember for Go declaration group '{}': duplicate member '{}' in '{}'",
                        strip_impl_disambiguator(&ma.symbol_fqn),
                        colliding,
                        pa.symbol_fqn
                    ),
                })
            } else if lang == LanguageKind::Go
                && (is_go_decl_group_fqn(&ma.symbol_fqn) ^ is_go_decl_group_fqn(&mb.symbol_fqn))
            {
                let (group_anchor, group_txt, single_anchor) =
                    if is_go_decl_group_fqn(&ma.symbol_fqn) {
                        (ma, txt_a, mb)
                    } else {
                        (mb, txt_b, ma)
                    };
                if go_group_matches_single_kind(&group_anchor.symbol_fqn, single_anchor.syntax_kind_id)
                    && let Some(group_members) =
                        parse_go_group_members(group_txt, group_anchor.logical_file_id)
                {
                    let is_import_group = strip_impl_disambiguator(&group_anchor.symbol_fqn)
                        .rsplit("::")
                        .next()
                        == Some("import_group");
                    let colliding = if is_import_group {
                        let no_added = single_anchor
                            .symbol_fqn
                            .strip_suffix("#added")
                            .unwrap_or(&single_anchor.symbol_fqn);
                        let single_rel = if let Some(idx) = no_added.find("import::") {
                            &no_added[idx..]
                        } else {
                            no_added
                        };
                        let group_set: HashSet<&str> =
                            group_members.iter().map(String::as_str).collect();
                        if group_set.contains(single_rel) {
                            Some(single_rel)
                        } else {
                            None
                        }
                    } else {
                        let single_raw = single_anchor
                            .symbol_fqn
                            .rsplit("::")
                            .next()
                            .unwrap_or(&single_anchor.symbol_fqn);
                        let single_rel = single_raw.strip_suffix("#added").unwrap_or(single_raw);
                        let group_set: HashSet<&str> =
                            group_members.iter().flat_map(|m| m.split(',')).collect();
                        single_rel.split(',').find(|name| group_set.contains(name))
                    };
                    colliding.map(|colliding| MergeOutcome::StructuralConflict {
                        file: file.to_path_buf(),
                        anchor: Some(single_anchor.clone()),
                        patch_a: hash_a,
                        patch_b: hash_b,
                        reason: format!(
                            "conflicting InsertMember between Go declaration '{}' and group '{}': duplicate member '{}' in '{}'",
                            single_anchor.symbol_fqn,
                            strip_impl_disambiguator(&group_anchor.symbol_fqn),
                            colliding,
                            pa.symbol_fqn
                        ),
                    })
                } else {
                    None
                }
            } else if same_member
                && is_overload_signature_anchor(lang, ma)
                && ma.decl_header_hash != mb.decl_header_hash
            {
                None
            } else if same_member && txt_a != txt_b {
                Some(MergeOutcome::StructuralConflict {
                    file: file.to_path_buf(),
                    anchor: Some(ma.clone()),
                    patch_a: hash_a,
                    patch_b: hash_b,
                    reason: format!(
                        "conflicting InsertMember for identical symbol '{}' in '{}'",
                        ma.symbol_fqn, pa.symbol_fqn
                    ),
                })
            } else {
                None
            }
        }

        _ => None,
    }
}

#[derive(Clone)]
struct OrderedOp<'a> {
    lamport_ts: u64,
    patch_hash: Blake3Hash,
    op_idx: usize,
    op: &'a SemanticOp,
}

#[derive(Debug, Clone)]
struct TrackedBaseUnit {
    base_anchor: NodeAnchor,
    is_container: bool,
    is_comma_separated_container: bool,
    parent_idx: Option<usize>,
    live_span: Range<usize>,
    removed: bool,
}

#[derive(Debug, Clone)]
struct TrackedInsertUnit {
    member_anchor: NodeAnchor,
    live_span: Range<usize>,
}

#[derive(Debug, Clone, Copy)]
enum ResolvedTarget {
    Base(usize),
    Inserted(usize),
}

fn collect_tracked_base(
    units: &[DeclUnit],
    parent_idx: Option<usize>,
    out: &mut Vec<TrackedBaseUnit>,
) {
    for u in units {
        let idx = out.len();
        out.push(TrackedBaseUnit {
            base_anchor: u.anchor.clone(),
            is_container: u.is_container,
            is_comma_separated_container: u.is_comma_separated_container,
            parent_idx,
            live_span: u.trivia_start_byte..u.end_byte,
            removed: false,
        });
        collect_tracked_base(&u.children, Some(idx), out);
    }
}

fn find_tracked_base_idx(
    tracked: &[TrackedBaseUnit],
    target: &NodeAnchor,
    transitions: &HashMap<NodeKey, (Blake3Hash, Blake3Hash)>,
) -> Option<usize> {
    if let Some(idx) = tracked.iter().position(|tb| tb.base_anchor == *target) {
        return Some(idx);
    }
    let key = node_key(target);
    if let Some((old_h, new_h)) = transitions.get(&key)
        && (*old_h == target.decl_header_hash || *new_h == target.decl_header_hash)
        && let Some(idx) = tracked
            .iter()
            .position(|tb| node_key(&tb.base_anchor) == key && tb.base_anchor.decl_header_hash == *old_h)
    {
        return Some(idx);
    }
    tracked.iter().position(|tb| node_key(&tb.base_anchor) == key)
}

fn is_inside_any_inserted(u: &DeclUnit, tracked_inserts: &[TrackedInsertUnit]) -> bool {
    tracked_inserts
        .iter()
        .any(|ins| ins.live_span.start <= u.decl_start_byte && u.end_byte <= ins.live_span.end)
}

fn resolve_base_unit<'a>(
    units: &'a [DeclUnit],
    tb: &TrackedBaseUnit,
    tracked_inserts: &[TrackedInsertUnit],
) -> Option<&'a DeclUnit> {
    let candidates: Vec<&'a DeclUnit> = flatten_decl_units(units)
        .into_iter()
        .filter(|u| !is_inside_any_inserted(u, tracked_inserts))
        .collect();
    if let Some(u) = candidates.iter().find(|u| {
        u.trivia_start_byte == tb.live_span.start
            && u.end_byte == tb.live_span.end
            && u.anchor.syntax_kind_id == tb.base_anchor.syntax_kind_id
    }) {
        return Some(*u);
    }
    candidates
        .into_iter()
        .filter(|u| {
            u.anchor.syntax_kind_id == tb.base_anchor.syntax_kind_id
                && u.is_container == tb.is_container
                && u.trivia_start_byte < tb.live_span.end
                && tb.live_span.start < u.end_byte
        })
        .max_by_key(|u| {
            let ov_start = u.trivia_start_byte.max(tb.live_span.start);
            let ov_end = u.end_byte.min(tb.live_span.end);
            ov_end.saturating_sub(ov_start)
        })
}

fn resolve_inserted_unit<'a>(
    units: &'a [DeclUnit],
    parent_unit: Option<&'a DeclUnit>,
    ins: &TrackedInsertUnit,
) -> Option<&'a DeclUnit> {
    let search_slice: &[DeclUnit] = match parent_unit {
        Some(pu) => &pu.children,
        None => units,
    };
    if let Some(u) = search_slice.iter().find(|u| {
        ins.live_span.start <= u.decl_start_byte
            && u.end_byte <= ins.live_span.end
            && u.anchor.syntax_kind_id == ins.member_anchor.syntax_kind_id
    }) {
        return Some(u);
    }
    flatten_decl_units(units).into_iter().find(|u| {
        ins.live_span.start <= u.decl_start_byte
            && u.end_byte <= ins.live_span.end
            && u.anchor.syntax_kind_id == ins.member_anchor.syntax_kind_id
    })
}

fn adjust_spans_for_edit(
    edit_start: usize,
    edit_end: usize,
    new_len: usize,
    enclosing_base_idx: Option<usize>,
    tracked_base: &mut [TrackedBaseUnit],
    tracked_inserts: &mut [TrackedInsertUnit],
) {
    let old_len = edit_end - edit_start;
    let delta = new_len as isize - old_len as isize;
    if delta == 0 {
        return;
    }

    let mut enclosing_set: HashSet<usize> = HashSet::new();
    let mut cur = enclosing_base_idx;
    while let Some(idx) = cur {
        if !enclosing_set.insert(idx) {
            break;
        }
        cur = tracked_base[idx].parent_idx;
    }

    let shift = |v: usize| -> usize { (v as isize + delta) as usize };

    for (idx, tb) in tracked_base.iter_mut().enumerate() {
        if enclosing_set.contains(&idx) {
            tb.live_span.end = shift(tb.live_span.end);
        } else if tb.live_span.end <= edit_start {
            // Strictly before or ending at insertion point: unchanged
        } else if tb.live_span.start >= edit_end {
            tb.live_span.start = shift(tb.live_span.start);
            tb.live_span.end = shift(tb.live_span.end);
        } else if tb.live_span.start <= edit_start && edit_end <= tb.live_span.end {
            tb.live_span.end = shift(tb.live_span.end);
        }
    }

    for ins in tracked_inserts.iter_mut() {
        if ins.live_span.end <= edit_start {
            // Strictly before or ending at insertion point: unchanged
        } else if ins.live_span.start >= edit_end {
            ins.live_span.start = shift(ins.live_span.start);
            ins.live_span.end = shift(ins.live_span.end);
        } else if ins.live_span.start <= edit_start && edit_end <= ins.live_span.end {
            ins.live_span.end = shift(ins.live_span.end);
        }
    }
}

fn is_consecutive_positional(prev: &NodeAnchor, cur: &NodeAnchor) -> bool {
    if prev.logical_file_id != cur.logical_file_id || prev.syntax_kind_id != cur.syntax_kind_id {
        return false;
    }
    let prev_fqn = prev.symbol_fqn.strip_suffix("#added").unwrap_or(&prev.symbol_fqn);
    let cur_fqn = cur.symbol_fqn.strip_suffix("#added").unwrap_or(&cur.symbol_fqn);
    if let Some((base, num_str)) = cur_fqn.rsplit_once("#sig:")
        && !num_str.is_empty()
        && num_str.bytes().all(|b| b.is_ascii_digit())
        && let Ok(n) = num_str.parse::<usize>()
        && n >= 2
    {
        return if n == 2 {
            prev_fqn == base || prev_fqn == format!("{base}#sig:1")
        } else {
            prev_fqn == format!("{base}#sig:{}", n - 1)
        };
    }
    if let Some((base, num_str)) = cur_fqn.rsplit_once('#')
        && !num_str.is_empty()
        && num_str.bytes().all(|b| b.is_ascii_digit())
        && let Ok(n) = num_str.parse::<usize>()
        && n >= 2
    {
        return if n == 2 {
            prev_fqn == base || prev_fqn == format!("{base}#1")
        } else {
            prev_fqn == format!("{base}#{}", n - 1)
        };
    }
    false
}

fn file_root_insert_tier(
    lang: LanguageKind,
    parent_anchor: &NodeAnchor,
    member_anchor: &NodeAnchor,
) -> u8 {
    if parent_anchor.symbol_fqn != "<file>" {
        return 2;
    }
    match lang {
        LanguageKind::Python => {
            if let Ok(py_lang) = super::lang::ts_language(LanguageKind::Python) {
                let fut_id = py_lang.id_for_node_kind("future_import_statement", true);
                if fut_id != 0 && member_anchor.syntax_kind_id == fut_id {
                    return 0;
                }
                let imp_id = py_lang.id_for_node_kind("import_statement", true);
                let from_id = py_lang.id_for_node_kind("import_from_statement", true);
                if (imp_id != 0 && member_anchor.syntax_kind_id == imp_id)
                    || (from_id != 0 && member_anchor.syntax_kind_id == from_id)
                {
                    return 1;
                }
            }
            2
        }
        LanguageKind::Go => {
            if let Ok(go_lang) = super::lang::ts_language(LanguageKind::Go) {
                let spec_id = go_lang.id_for_node_kind("import_spec", true);
                let decl_id = go_lang.id_for_node_kind("import_declaration", true);
                if (spec_id != 0 && member_anchor.syntax_kind_id == spec_id)
                    || (decl_id != 0 && member_anchor.syntax_kind_id == decl_id)
                {
                    return 1;
                }
            }
            2
        }
        _ => 2,
    }
}

struct PendingInsert<'a> {
    insert_id: usize,
    patch_hash: Blake3Hash,
    parent_anchor: &'a NodeAnchor,
    member_anchor: &'a NodeAnchor,
    full_text_with_trivia: &'a str,
    after_anchor: Option<&'a NodeAnchor>,
    intra_patch_pred_id: Option<usize>,
    header_tier: u8,
    slot_key: (NodeKey, Option<NodeKey>, u8),
}

struct TrackedFileApply {
    merged_src: String,
    touched_spans: Vec<Range<usize>>,
}

fn apply_cst_ops_for_file(
    lang: LanguageKind,
    logical_file_id: Blake3Hash,
    file_path: &Path,
    base_str: &str,
    group: &[&AstPatch],
) -> Result<Result<String, MergeOutcome>, CstError> {
    match apply_cst_ops_for_file_tracked(lang, logical_file_id, file_path, base_str, group)? {
        Ok(res) => Ok(Ok(res.merged_src)),
        Err(conflict) => Ok(Err(conflict)),
    }
}

fn apply_cst_ops_for_file_tracked(
    lang: LanguageKind,
    logical_file_id: Blake3Hash,
    file_path: &Path,
    base_str: &str,
    group: &[&AstPatch],
) -> Result<Result<TrackedFileApply, MergeOutcome>, CstError> {
    let mut live_src = base_str.to_string();

    let mut mut_sigs: Vec<OrderedOp<'_>> = Vec::new();
    let mut rep_bodies: Vec<OrderedOp<'_>> = Vec::new();
    let mut inserts: Vec<OrderedOp<'_>> = Vec::new();
    let mut removes: Vec<OrderedOp<'_>> = Vec::new();

    for p in group {
        for (op_idx, op) in p.ops.iter().enumerate() {
            let entry = OrderedOp {
                lamport_ts: p.lamport_ts,
                patch_hash: p.patch_hash,
                op_idx,
                op,
            };
            match op {
                SemanticOp::MutateSignature { .. } => mut_sigs.push(entry),
                SemanticOp::ReplaceBody { .. } => rep_bodies.push(entry),
                SemanticOp::InsertMember { .. } => inserts.push(entry),
                SemanticOp::RemoveNode { .. } => removes.push(entry),
                SemanticOp::RenameOrMoveFile { .. } | SemanticOp::TextCrdtFallback { .. } => {}
            }
        }
    }

    if mut_sigs.is_empty() && rep_bodies.is_empty() && inserts.is_empty() && removes.is_empty() {
        return Ok(Ok(TrackedFileApply {
            merged_src: live_src,
            touched_spans: Vec::new(),
        }));
    }

    let sort_key = |o: &OrderedOp<'_>| (o.lamport_ts, o.patch_hash, o.op_idx);
    mut_sigs.sort_by_key(sort_key);
    rep_bodies.sort_by_key(sort_key);
    inserts.sort_by_key(sort_key);
    removes.sort_by_key(sort_key);

    let base_parsed = parse_source(lang, base_str, None)?;
    let base_units = extract_decl_units(lang, &base_parsed.tree, base_str, logical_file_id);
    let mut tracked_base: Vec<TrackedBaseUnit> = Vec::new();
    collect_tracked_base(&base_units, None, &mut tracked_base);
    let mut tracked_inserts: Vec<TrackedInsertUnit> = Vec::new();
    let mut touched_base_indices: HashSet<usize> = HashSet::new();

    let mut header_transitions: HashMap<NodeKey, (Blake3Hash, Blake3Hash)> = HashMap::new();

    // Phase A: MutateSignature
    let mut seen_mut_sigs: HashSet<(NodeKey, Blake3Hash, String)> = HashSet::new();
    for (mut_idx, item) in mut_sigs.iter().enumerate() {
        if let SemanticOp::MutateSignature {
            target,
            old_header_hash,
            new_header_text,
            new_header_hash,
        } = item.op
        {
            let dedup_key = (
                node_key(target),
                *new_header_hash,
                new_header_text.clone(),
            );
            if !seen_mut_sigs.insert(dedup_key) {
                continue;
            }
            let base_idx = find_tracked_base_idx(&tracked_base, target, &header_transitions)
                .ok_or_else(|| {
                    CstError::AnchorNotFound(format!(
                        "MutateSignature target '{}' in '{}'",
                        target.symbol_fqn,
                        file_path.display()
                    ))
                })?;
            let mut parsed = parse_source(lang, &live_src, None)?;
            let mut units = extract_decl_units(lang, &parsed.tree, &live_src, logical_file_id);
            let mut unit = resolve_base_unit(&units, &tracked_base[base_idx], &tracked_inserts)
                .ok_or_else(|| {
                    CstError::AnchorNotFound(format!(
                        "MutateSignature target '{}' in '{}'",
                        target.symbol_fqn,
                        file_path.display()
                    ))
                })?;
            let mut hdr_range = unit.header_range.clone();
            let effective_new_header: std::borrow::Cow<'_, str> =
                if lang == LanguageKind::Python && unit.parent_anchor.is_some() {
                    let at_line_start = hdr_range.start == 0
                        || matches!(live_src.as_bytes()[hdr_range.start - 1], b'\n' | b'\r');
                    if at_line_start {
                        let target_ws = line_indent_at(&live_src, unit.decl_start_byte);
                        if !target_ws.is_empty() {
                            std::borrow::Cow::Owned(normalize_python_member_indent(
                                new_header_text,
                                target_ws,
                            ))
                        } else {
                            std::borrow::Cow::Borrowed(new_header_text.as_str())
                        }
                    } else if live_src[hdr_range.start..unit.decl_start_byte]
                        .contains(['\n', '\r'])
                    {
                        let target_ws = line_indent_at(&live_src, unit.decl_start_byte);
                        let nl = detect_python_eol(&live_src, new_header_text);
                        if let Some((first_line, first_eol, rest)) =
                            split_first_line(new_header_text)
                            && first_line.starts_with('#')
                        {
                            let normalized_rest = if !target_ws.is_empty() {
                                normalize_python_member_indent(rest, target_ws)
                            } else {
                                rest.to_string()
                            };
                            std::borrow::Cow::Owned(format!(
                                "{first_line}{first_eol}{normalized_rest}"
                            ))
                        } else {
                            let normalized = if !target_ws.is_empty() {
                                normalize_python_member_indent(new_header_text, target_ws)
                            } else {
                                new_header_text.to_string()
                            };
                            std::borrow::Cow::Owned(format!("{nl}{normalized}"))
                        }
                    } else if !new_header_text.contains(['\n', '\r']) {
                        std::borrow::Cow::Borrowed(new_header_text.trim_start_matches([' ', '\t']))
                    } else {
                        let my_parent_idx = tracked_base[base_idx].parent_idx;
                        let sibling_hints: Vec<&str> = mut_sigs[mut_idx + 1..]
                            .iter()
                            .filter_map(|other| {
                                if let SemanticOp::MutateSignature {
                                    target: other_target,
                                    new_header_text: other_hdr,
                                    ..
                                } = other.op
                                    && let Some(other_idx) = find_tracked_base_idx(
                                        &tracked_base,
                                        other_target,
                                        &header_transitions,
                                    )
                                    && tracked_base[other_idx].parent_idx == my_parent_idx
                                {
                                    Some(other_hdr.as_str())
                                } else {
                                    None
                                }
                            })
                            .collect();
                        expand_python_inline_unit_to_line_start(
                            &mut live_src,
                            &mut parsed,
                            &mut units,
                            logical_file_id,
                            base_idx,
                            &mut tracked_base,
                            &mut tracked_inserts,
                            new_header_text,
                            &sibling_hints,
                            new_header_text,
                        )?;
                        unit =
                            resolve_base_unit(&units, &tracked_base[base_idx], &tracked_inserts)
                                .ok_or_else(|| {
                                    CstError::AnchorNotFound(format!(
                                        "MutateSignature target '{}' in '{}'",
                                        target.symbol_fqn,
                                        file_path.display()
                                    ))
                                })?;
                        hdr_range = unit.header_range.clone();
                        tracked_base[base_idx].live_span = unit.trivia_start_byte..unit.end_byte;
                        let now_at_line_start = hdr_range.start == 0
                            || matches!(live_src.as_bytes()[hdr_range.start - 1], b'\n' | b'\r');
                        if now_at_line_start {
                            let target_ws = line_indent_at(&live_src, unit.decl_start_byte);
                            if !target_ws.is_empty() {
                                std::borrow::Cow::Owned(normalize_python_member_indent(
                                    new_header_text,
                                    target_ws,
                                ))
                            } else {
                                std::borrow::Cow::Borrowed(new_header_text.as_str())
                            }
                        } else if !new_header_text
                            .trim_start_matches([' ', '\t'])
                            .starts_with('@')
                        {
                            std::borrow::Cow::Borrowed(
                                new_header_text.trim_start_matches([' ', '\t']),
                            )
                        } else {
                            std::borrow::Cow::Borrowed(new_header_text.as_str())
                        }
                    }
                } else {
                    std::borrow::Cow::Borrowed(new_header_text.as_str())
                };
            live_src.replace_range(hdr_range.clone(), effective_new_header.as_ref());
            adjust_spans_for_edit(
                hdr_range.start,
                hdr_range.end,
                effective_new_header.len(),
                Some(base_idx),
                &mut tracked_base,
                &mut tracked_inserts,
            );
            touched_base_indices.insert(base_idx);
            header_transitions.insert(node_key(target), (*old_header_hash, *new_header_hash));
        }
    }

    // Phase B: ReplaceBody
    let mut seen_rep_bodies: HashSet<(NodeKey, String)> = HashSet::new();
    for item in &rep_bodies {
        if let SemanticOp::ReplaceBody {
            target,
            new_body_text,
            ..
        } = item.op
        {
            let dedup_key = (node_key(target), new_body_text.clone());
            if !seen_rep_bodies.insert(dedup_key) {
                continue;
            }
            let base_idx = find_tracked_base_idx(&tracked_base, target, &header_transitions)
                .ok_or_else(|| {
                    CstError::AnchorNotFound(format!(
                        "ReplaceBody target '{}' in '{}'",
                        target.symbol_fqn,
                        file_path.display()
                    ))
                })?;
            let mut parsed = parse_source(lang, &live_src, None)?;
            let mut units = extract_decl_units(lang, &parsed.tree, &live_src, logical_file_id);
            let mut unit = resolve_base_unit(&units, &tracked_base[base_idx], &tracked_inserts)
                .ok_or_else(|| {
                    CstError::AnchorNotFound(format!(
                        "ReplaceBody target '{}' in '{}'",
                        target.symbol_fqn,
                        file_path.display()
                    ))
                })?;
            if lang == LanguageKind::Python && unit.parent_anchor.is_some() {
                let hdr_ws = line_indent_at(&live_src, unit.decl_start_byte);
                let line_start = live_src[..unit.decl_start_byte]
                    .rfind(['\n', '\r'])
                    .map_or(0, |p| p + 1);
                let decl_at_line_start = unit.decl_start_byte == line_start + hdr_ws.len();
                let is_multiline_block_body = split_first_line(new_body_text).is_some_and(
                    |(colon_tail, _, _)| {
                        colon_tail.trim().is_empty() || colon_tail.trim_start().starts_with('#')
                    },
                );
                if !decl_at_line_start && is_multiline_block_body {
                    expand_python_inline_unit_to_line_start(
                        &mut live_src,
                        &mut parsed,
                        &mut units,
                        logical_file_id,
                        base_idx,
                        &mut tracked_base,
                        &mut tracked_inserts,
                        "",
                        &[],
                        new_body_text,
                    )?;
                    unit = resolve_base_unit(&units, &tracked_base[base_idx], &tracked_inserts)
                        .ok_or_else(|| {
                            CstError::AnchorNotFound(format!(
                                "ReplaceBody target '{}' in '{}'",
                                target.symbol_fqn,
                                file_path.display()
                            ))
                        })?;
                    tracked_base[base_idx].live_span = unit.trivia_start_byte..unit.end_byte;
                }
            }
            let range = unit.body_range.clone().ok_or_else(|| {
                CstError::AnchorNotFound(format!(
                    "ReplaceBody target '{}' has no body_range",
                    target.symbol_fqn
                ))
            })?;
            let effective_new_body: std::borrow::Cow<'_, str> = if lang == LanguageKind::Python {
                normalize_python_replace_body(
                    &live_src,
                    unit,
                    &units,
                    &live_src[range.clone()],
                    new_body_text,
                )
            } else {
                std::borrow::Cow::Borrowed(new_body_text.as_str())
            };
            live_src.replace_range(range.clone(), effective_new_body.as_ref());
            adjust_spans_for_edit(
                range.start,
                range.end,
                effective_new_body.len(),
                Some(base_idx),
                &mut tracked_base,
                &mut tracked_inserts,
            );
            touched_base_indices.insert(base_idx);
        }
    }

    // Phase C: InsertMember (BEFORE RemoveNode, with per-slot cursor advancement and tracked base spans)
    let mut seen_inserts: HashSet<(NodeKey, NodeKey, String)> = HashSet::new();
    let mut pending_inserts: Vec<PendingInsert<'_>> = Vec::new();
    for item in inserts {
        if let SemanticOp::InsertMember {
            parent_anchor,
            member_anchor,
            full_text_with_trivia,
            after_anchor,
        } = item.op
        {
            let dk = (
                node_key(parent_anchor),
                node_key(member_anchor),
                full_text_with_trivia.clone(),
            );
            if seen_inserts.insert(dk) {
                let insert_id = pending_inserts.len();
                let header_tier = file_root_insert_tier(lang, parent_anchor, member_anchor);
                pending_inserts.push(PendingInsert {
                    insert_id,
                    patch_hash: item.patch_hash,
                    parent_anchor,
                    member_anchor,
                    full_text_with_trivia,
                    after_anchor: after_anchor.as_ref(),
                    intra_patch_pred_id: None,
                    header_tier,
                    slot_key: (
                        node_key(parent_anchor),
                        after_anchor.as_ref().map(node_key),
                        header_tier,
                    ),
                });
            }
        }
    }

    // Identify genuine intra-patch chained InsertMembers without colliding on shifted base positional FQNs.
    for i in 0..pending_inserts.len() {
        let Some(after) = pending_inserts[i].after_anchor else {
            continue;
        };
        let after_in_base =
            find_tracked_base_idx(&tracked_base, after, &header_transitions).is_some();
        let after_k = node_key(after);
        let parent_k = node_key(pending_inserts[i].parent_anchor);
        let patch_h = pending_inserts[i].patch_hash;

        let pred_pos = (0..i).rev().find(|&j| {
            pending_inserts[j].patch_hash == patch_h
                && node_key(pending_inserts[j].parent_anchor) == parent_k
                && node_key(pending_inserts[j].member_anchor) == after_k
                && (!after_in_base
                    || is_consecutive_positional(
                        pending_inserts[j].member_anchor,
                        pending_inserts[i].member_anchor,
                    ))
        });
        if let Some(j) = pred_pos {
            pending_inserts[i].intra_patch_pred_id = Some(pending_inserts[j].insert_id);
            if pending_inserts[i].header_tier == pending_inserts[j].header_tier {
                pending_inserts[i].slot_key = pending_inserts[j].slot_key.clone();
            }
        }
    }

    let mut slot_cursors: HashMap<(NodeKey, Option<NodeKey>, u8), usize> = HashMap::new();
    let mut applied_insert_ids: HashMap<usize, usize> = HashMap::new();

    while !pending_inserts.is_empty() {
        let parsed = parse_source(lang, &live_src, None)?;
        let units = extract_decl_units(lang, &parsed.tree, &live_src, logical_file_id);

        let ready_idx = pending_inserts
            .iter()
            .enumerate()
            .filter(|(_, item)| match item.intra_patch_pred_id {
                None => true,
                Some(pred_id) => applied_insert_ids.contains_key(&pred_id),
            })
            .min_by_key(|(pos, item)| (item.header_tier, *pos))
            .map(|(pos, _)| pos);

        let idx = ready_idx.unwrap_or(0);
        let item = pending_inserts.remove(idx);

        let parent_base_idx = if item.parent_anchor.symbol_fqn == "<file>" {
            None
        } else {
            Some(
                find_tracked_base_idx(&tracked_base, item.parent_anchor, &header_transitions)
                    .ok_or_else(|| {
                        CstError::AnchorNotFound(format!(
                            "InsertMember parent_anchor '{}'",
                            item.parent_anchor.symbol_fqn
                        ))
                    })?,
            )
        };

        let effective_after = if let Some(&cursor_ins_idx) = slot_cursors.get(&item.slot_key) {
            Some(ResolvedTarget::Inserted(cursor_ins_idx))
        } else if let Some(pred_id) = item.intra_patch_pred_id
            && let Some(&pred_ins_idx) = applied_insert_ids.get(&pred_id)
        {
            Some(ResolvedTarget::Inserted(pred_ins_idx))
        } else if let Some(after_anc) = item.after_anchor {
            let after_base_idx =
                find_tracked_base_idx(&tracked_base, after_anc, &header_transitions).ok_or_else(
                    || {
                        CstError::AnchorNotFound(format!(
                            "InsertMember after_anchor '{}'",
                            after_anc.symbol_fqn
                        ))
                    },
                )?;
            Some(ResolvedTarget::Base(after_base_idx))
        } else {
            None
        };

        let sibling_pending_texts: Vec<&str> = if lang == LanguageKind::Python
            && item.parent_anchor.symbol_fqn != "<file>"
        {
            let target_parent_key = node_key(item.parent_anchor);
            pending_inserts
                .iter()
                .filter(|p| node_key(p.parent_anchor) == target_parent_key)
                .map(|p| p.full_text_with_trivia)
                .collect()
        } else {
            Vec::new()
        };

        let inserted_span = apply_single_insert(
            lang,
            &parsed.tree,
            &units,
            &mut live_src,
            item.parent_anchor,
            parent_base_idx,
            effective_after,
            item.header_tier,
            item.full_text_with_trivia,
            &sibling_pending_texts,
            &mut tracked_base,
            &mut tracked_inserts,
        )?;

        let ins_idx = tracked_inserts.len();
        tracked_inserts.push(TrackedInsertUnit {
            member_anchor: item.member_anchor.clone(),
            live_span: inserted_span,
        });
        slot_cursors.insert(item.slot_key, ins_idx);
        applied_insert_ids.insert(item.insert_id, ins_idx);
    }

    // Phase D: RemoveNode (resolve against tracked base units, never matching Phase C insertions)
    let mut seen_remove_indices: HashSet<usize> = HashSet::new();
    let mut removal_targets: Vec<(usize, usize, usize, bool)> = Vec::new();
    if !removes.is_empty() {
        let parsed = parse_source(lang, &live_src, None)?;
        let units = extract_decl_units(lang, &parsed.tree, &live_src, logical_file_id);
        for item in &removes {
            if let SemanticOp::RemoveNode { target, .. } = item.op {
                let Some(base_idx) =
                    find_tracked_base_idx(&tracked_base, target, &header_transitions)
                else {
                    continue;
                };
                if !seen_remove_indices.insert(base_idx) {
                    continue;
                }
                let Some(unit) = resolve_base_unit(&units, &tracked_base[base_idx], &tracked_inserts)
                else {
                    continue;
                };
                let start = unit.trivia_start_byte;
                let mut end = unit.end_byte;

                // If inside a comma-separated container (or TS/TSX interface/enum body) and followed by ',', remove the trailing comma too.
                let is_comma_parent = tracked_base[base_idx]
                    .parent_idx
                    .is_some_and(|pi| {
                        tracked_base[pi].is_comma_separated_container
                            || matches!(lang, LanguageKind::TypeScript | LanguageKind::Tsx)
                    });
                if is_comma_parent
                    && let Some(comma_end) = find_trailing_comma_end(&live_src, end)
                {
                    end = comma_end;
                }

                let mut same_line_after_semi = false;
                if matches!(lang, LanguageKind::Go | LanguageKind::Python) {
                    if end <= live_src.len() && live_src[..end].ends_with(';') {
                        let rest = &live_src[end..];
                        let ws: usize = rest
                            .chars()
                            .take_while(|c| *c == ' ' || *c == '\t')
                            .map(|c| c.len_utf8())
                            .sum();
                        let after_ws = &rest[ws..];
                        if !after_ws.is_empty() && !after_ws.starts_with(['\n', '\r', '}', ')']) {
                            end += ws;
                            same_line_after_semi = true;
                        }
                    } else if let Some((semi_end, has_following_same_line)) =
                        find_trailing_semicolon_end(
                            &live_src,
                            end,
                            lang,
                            tracked_base[base_idx].parent_idx.is_some(),
                        )
                    {
                        end = semi_end;
                        same_line_after_semi = has_following_same_line;
                    }
                }
                removal_targets.push((base_idx, start, end, same_line_after_semi));
            }
        }
    }

    // Drop any removal span strictly contained within an outer container removal span.
    let mut filtered_removals: Vec<(usize, usize, usize, bool)> = Vec::new();
    for (idx, &(base_idx, s, e, same_line_semi)) in removal_targets.iter().enumerate() {
        let contained = removal_targets.iter().enumerate().any(|(j, &(_, os, oe, _))| {
            j != idx && os <= s && e <= oe && (os < s || e < oe)
        });
        if !contained {
            filtered_removals.push((base_idx, s, e, same_line_semi));
        }
    }
    filtered_removals.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.2.cmp(&a.2)));
    filtered_removals.dedup_by_key(|x| (x.1, x.2));
    let mut last_trim_start: Option<usize> = None;
    for (base_idx, start, mut end, mut same_line_semi) in filtered_removals {
        if let Some(prev_ts) = last_trim_start
            && end > prev_ts
        {
            end = prev_ts;
            same_line_semi = false;
        }
        if end <= live_src.len() && start <= end {
            let bytes = live_src.as_bytes();
            let mut line_scan = start;
            while line_scan > 0 && matches!(bytes[line_scan - 1], b' ' | b'\t') {
                line_scan -= 1;
            }
            let at_line_start =
                line_scan == 0 || matches!(bytes[line_scan - 1], b'\n' | b'\r');

            let (trim_start, trim_end, replacement) = if same_line_semi {
                let rep = if at_line_start {
                    line_indent_at(&live_src, end).to_string()
                } else {
                    String::new()
                };
                (start, end, rep)
            } else if !at_line_start {
                let mut trim_start = start;
                let mut trim_end = end;
                let mut rep = String::new();
                if line_scan > 0 && bytes[line_scan - 1] == b';' {
                    let mut before_semi = line_scan - 1;
                    while before_semi > 0 && matches!(bytes[before_semi - 1], b' ' | b'\t') {
                        before_semi -= 1;
                    }
                    trim_start = before_semi;
                    let mut after_ws = trim_end;
                    while after_ws < bytes.len() && matches!(bytes[after_ws], b' ' | b'\t') {
                        after_ws += 1;
                    }
                    if after_ws == bytes.len() || matches!(bytes[after_ws], b'\n' | b'\r') {
                        trim_end = after_ws;
                    }
                } else if lang == LanguageKind::Python
                    && line_scan > 0
                    && bytes[line_scan - 1] == b':'
                {
                    let mut after_ws = trim_end;
                    while after_ws < bytes.len() && matches!(bytes[after_ws], b' ' | b'\t') {
                        after_ws += 1;
                    }
                    if after_ws == bytes.len()
                        || matches!(bytes[after_ws], b'\n' | b'\r' | b'#')
                    {
                        rep = "pass".to_string();
                    }
                }
                (trim_start, trim_end, rep)
            } else {
                let (ts, te) = compute_removal_trim_span(&live_src, start, end);
                (ts, te, String::new())
            };
            last_trim_start = Some(last_trim_start.map_or(trim_start, |prev| prev.min(trim_start)));
            let parent_idx = tracked_base[base_idx].parent_idx;
            live_src.replace_range(trim_start..trim_end, &replacement);
            tracked_base[base_idx].removed = true;
            for tb in &mut tracked_base {
                if tb.live_span.start >= trim_start && tb.live_span.end <= trim_end {
                    tb.removed = true;
                }
            }
            adjust_spans_for_edit(
                trim_start,
                trim_end,
                replacement.len(),
                parent_idx,
                &mut tracked_base,
                &mut tracked_inserts,
            );
        }
    }

    if lang == LanguageKind::Python && !removes.is_empty() {
        repair_emptied_python_classes(&mut live_src, &mut tracked_base, &mut tracked_inserts)?;
    }

    // Phase E: Post-merge syntax check
    let final_parsed = parse_source(lang, &live_src, None)?;
    let errors = collect_syntax_errors(&final_parsed.tree);
    if !errors.is_empty() {
        let pa = group.first().map(|p| p.patch_hash).unwrap_or(Blake3Hash::ZERO);
        let pb = group.get(1).map(|p| p.patch_hash).unwrap_or(pa);
        let first_err = &errors[0];
        return Ok(Err(MergeOutcome::StructuralConflict {
            file: file_path.to_path_buf(),
            anchor: None,
            patch_a: pa,
            patch_b: pb,
            reason: format!(
                "post-merge CST syntax error at line {}, column {}",
                first_err.line, first_err.column
            ),
        }));
    }

    let mut touched_spans: Vec<Range<usize>> = Vec::new();
    for idx in touched_base_indices {
        let tb = &tracked_base[idx];
        if !tb.removed && tb.live_span.start < tb.live_span.end {
            touched_spans.push(tb.live_span.clone());
        }
    }
    for ins in &tracked_inserts {
        if ins.live_span.start < ins.live_span.end {
            touched_spans.push(ins.live_span.clone());
        }
    }

    Ok(Ok(TrackedFileApply {
        merged_src: live_src,
        touched_spans,
    }))
}

#[allow(clippy::too_many_arguments)]
fn apply_single_insert(
    lang: LanguageKind,
    tree: &tree_sitter::Tree,
    units: &[DeclUnit],
    live_src: &mut String,
    parent_anchor: &NodeAnchor,
    parent_base_idx: Option<usize>,
    effective_after: Option<ResolvedTarget>,
    header_tier: u8,
    full_text_with_trivia: &str,
    sibling_pending_texts: &[&str],
    tracked_base: &mut [TrackedBaseUnit],
    tracked_inserts: &mut [TrackedInsertUnit],
) -> Result<Range<usize>, CstError> {
    let is_file_root = parent_base_idx.is_none();
    let parent_unit = match parent_base_idx {
        None => None,
        Some(idx) => Some(
            resolve_base_unit(units, &tracked_base[idx], tracked_inserts).ok_or_else(|| {
                CstError::AnchorNotFound(format!(
                    "InsertMember parent_anchor '{}'",
                    parent_anchor.symbol_fqn
                ))
            })?,
        ),
    };
    let is_comma_sep = parent_unit.is_some_and(|u| u.is_comma_separated_container);

    let lf_count = live_src.matches('\n').count();
    let crlf_count = live_src.matches("\r\n").count();
    let nl = if crlf_count > 0 && crlf_count >= lf_count - crlf_count {
        "\r\n"
    } else {
        "\n"
    };

    if let Some(after_target) = effective_after {
        let after_unit = match after_target {
            ResolvedTarget::Base(idx) => {
                resolve_base_unit(units, &tracked_base[idx], tracked_inserts).ok_or_else(|| {
                    CstError::AnchorNotFound(format!(
                        "InsertMember after_anchor '{}'",
                        tracked_base[idx].base_anchor.symbol_fqn
                    ))
                })?
            }
            ResolvedTarget::Inserted(ins_idx) => {
                resolve_inserted_unit(units, parent_unit, &tracked_inserts[ins_idx]).ok_or_else(
                    || {
                        CstError::AnchorNotFound(format!(
                            "InsertMember after_anchor '{}'",
                            tracked_inserts[ins_idx].member_anchor.symbol_fqn
                        ))
                    },
                )?
            }
        };
        let mut after_end = after_unit.end_byte;
        if is_file_root && lang == LanguageKind::Go && header_tier >= 2 && after_unit.kind == SymbolKind::Import {
            let root = tree.root_node();
            let mut cursor = root.walk();
            for child in root.named_children(&mut cursor) {
                if child.kind() == "comment" {
                    continue;
                }
                if child.end_byte() <= after_end {
                    continue;
                }
                if child.kind() == "import_declaration" {
                    after_end = child.end_byte();
                } else {
                    break;
                }
            }
        }
        if is_file_root && lang == LanguageKind::Python && header_tier >= 1 {
            let root = tree.root_node();
            let mut cursor = root.walk();
            for child in root.named_children(&mut cursor) {
                if child.kind() == "comment" {
                    continue;
                }
                if child.end_byte() <= after_end {
                    continue;
                }
                if child.kind() == "future_import_statement" {
                    after_end = child.end_byte();
                } else {
                    break;
                }
            }
        }
        if is_comma_sep {
            if let Some(comma_end) = find_trailing_comma_end(live_src, after_end) {
                let insertion = if full_text_with_trivia.trim_end().ends_with(',') {
                    format!("{nl}{full_text_with_trivia}")
                } else {
                    format!("{nl}{full_text_with_trivia},")
                };
                live_src.insert_str(comma_end, &insertion);
                adjust_spans_for_edit(
                    comma_end,
                    comma_end,
                    insertion.len(),
                    parent_base_idx,
                    tracked_base,
                    tracked_inserts,
                );
                let text_start = comma_end + nl.len();
                return Ok(text_start..(text_start + full_text_with_trivia.len()));
            } else {
                live_src.insert(after_end, ',');
                adjust_spans_for_edit(
                    after_end,
                    after_end,
                    1,
                    parent_base_idx,
                    tracked_base,
                    tracked_inserts,
                );
                let insert_pos = skip_same_line_trailing_comment(live_src, after_end + 1);
                let insertion = format!("{nl}{full_text_with_trivia}");
                live_src.insert_str(insert_pos, &insertion);
                adjust_spans_for_edit(
                    insert_pos,
                    insert_pos,
                    insertion.len(),
                    parent_base_idx,
                    tracked_base,
                    tracked_inserts,
                );
                let text_start = insert_pos + nl.len();
                return Ok(text_start..(text_start + full_text_with_trivia.len()));
            }
        } else if lang == LanguageKind::Python {
            let member_indent = if let Some(pu) = parent_unit {
                resolve_python_member_indent(
                    tree.root_node(),
                    pu,
                    live_src,
                    full_text_with_trivia,
                    sibling_pending_texts,
                )
            } else {
                line_indent_at(live_src, after_unit.trivia_start_byte).to_string()
            };
            if let Some(pu) = parent_unit
                && let Some(body_range) = pu.body_range.as_ref()
            {
                let gaps = find_python_colon_row_inline_gaps(
                    tree.root_node(),
                    pu,
                    body_range.start,
                    live_src,
                );
                if !gaps.is_empty() {
                    let sep = format!("{nl}{member_indent}");
                    for (gap_start, gap_end) in gaps.into_iter().rev() {
                        live_src.replace_range(gap_start..gap_end, &sep);
                        adjust_spans_for_edit(
                            gap_start,
                            gap_end,
                            sep.len(),
                            parent_base_idx,
                            tracked_base,
                            tracked_inserts,
                        );
                        if after_end >= gap_end {
                            let delta = sep.len() as isize - (gap_end - gap_start) as isize;
                            after_end = (after_end as isize + delta) as usize;
                        }
                    }
                }
            }

            let insert_text = if parent_unit.is_some() {
                normalize_python_member_indent(full_text_with_trivia, &member_indent)
            } else {
                full_text_with_trivia.to_string()
            };

            let rest = &live_src[after_end..];
            let sep_len = scan_python_inline_sep(rest);
            let has_semi = rest[..sep_len].contains(';');
            let after_sep = &rest[sep_len..];
            let has_same_line_stmt =
                has_semi && !after_sep.is_empty() && !after_sep.starts_with(['\n', '\r', '#']);

            if has_same_line_stmt {
                let insertion = format!("{nl}{nl}{insert_text}{nl}{member_indent}");
                live_src.replace_range(after_end..(after_end + sep_len), &insertion);
                adjust_spans_for_edit(
                    after_end,
                    after_end + sep_len,
                    insertion.len(),
                    parent_base_idx,
                    tracked_base,
                    tracked_inserts,
                );
                let text_start = after_end + 2 * nl.len();
                return Ok(text_start..(text_start + insert_text.len()));
            }

            if has_semi {
                if after_sep.starts_with('#') {
                    live_src.replace_range(after_end..(after_end + sep_len), " ");
                    adjust_spans_for_edit(
                        after_end,
                        after_end + sep_len,
                        1,
                        parent_base_idx,
                        tracked_base,
                        tracked_inserts,
                    );
                } else {
                    live_src.replace_range(after_end..(after_end + sep_len), "");
                    adjust_spans_for_edit(
                        after_end,
                        after_end + sep_len,
                        0,
                        parent_base_idx,
                        tracked_base,
                        tracked_inserts,
                    );
                }
            }

            let insert_pos = skip_same_line_hash_comment(live_src, after_end);
            let insertion = format!("{nl}{nl}{insert_text}");
            live_src.insert_str(insert_pos, &insertion);
            adjust_spans_for_edit(
                insert_pos,
                insert_pos,
                insertion.len(),
                parent_base_idx,
                tracked_base,
                tracked_inserts,
            );
            let text_start = insert_pos + 2 * nl.len();
            return Ok(text_start..(text_start + insert_text.len()));
        } else {
            let mut insert_pos = skip_same_line_trailing_comment(live_src, after_end);
            let after_comment = &live_src[insert_pos..];
            let ws_before_semi: usize = after_comment
                .chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .map(|c| c.len_utf8())
                .sum();
            if after_comment[ws_before_semi..].starts_with([';', ',']) {
                insert_pos = skip_same_line_trailing_comment(
                    live_src,
                    insert_pos + ws_before_semi + 1,
                );
            }
            let rest = &live_src[insert_pos..];
            let after_ws = rest.trim_start_matches([' ', '\t']);
            let needs_trailing_nl =
                !after_ws.is_empty() && !after_ws.starts_with(['\n', '\r']);
            let mut insertion = format!("{nl}{nl}{full_text_with_trivia}");
            if needs_trailing_nl {
                insertion.push_str(nl);
                if (after_ws.starts_with('}') || after_ws.starts_with(')'))
                    && let Some(pu) = parent_unit
                {
                    insertion.push_str(line_indent_at(live_src, pu.trivia_start_byte));
                }
            }
            live_src.insert_str(insert_pos, &insertion);
            adjust_spans_for_edit(
                insert_pos,
                insert_pos,
                insertion.len(),
                parent_base_idx,
                tracked_base,
                tracked_inserts,
            );
            let text_start = insert_pos + 2 * nl.len();
            return Ok(text_start..(text_start + full_text_with_trivia.len()));
        }
    }

    // effective_after == None -> insert at start of container (or file root)
    if is_file_root {
        if let Some(insert_pos) =
            find_file_root_insert_pos(lang, tree.root_node(), units, live_src, header_tier)
        {
            let insertion = format!("{nl}{nl}{full_text_with_trivia}");
            live_src.insert_str(insert_pos, &insertion);
            adjust_spans_for_edit(
                insert_pos,
                insert_pos,
                insertion.len(),
                None,
                tracked_base,
                tracked_inserts,
            );
            let text_start = insert_pos + 2 * nl.len();
            return Ok(text_start..(text_start + full_text_with_trivia.len()));
        }
        if live_src.is_empty() {
            live_src.push_str(full_text_with_trivia);
            return Ok(0..full_text_with_trivia.len());
        } else {
            let insertion = format!("{full_text_with_trivia}{nl}{nl}");
            live_src.insert_str(0, &insertion);
            adjust_spans_for_edit(
                0,
                0,
                insertion.len(),
                None,
                tracked_base,
                tracked_inserts,
            );
            return Ok(0..full_text_with_trivia.len());
        }
    }

    let pu = parent_unit.expect("parent_unit present when not file root");
    let body_range = pu.body_range.as_ref().ok_or_else(|| {
        CstError::AnchorNotFound(format!(
            "container '{}' has no body_range",
            parent_anchor.symbol_fqn
        ))
    })?;
    let body_start = body_range.start;
    let pu_trivia_start = pu.trivia_start_byte;

    if lang == LanguageKind::Python {
        match find_python_class_insertion(tree.root_node(), pu, body_range, live_src) {
            PythonInsertAction::ReplacePass(pass_end) => {
                let mut scan = pass_end.min(live_src.len());
                let rest = &live_src[scan..];
                let ws1: usize = rest
                    .chars()
                    .take_while(|c| *c == ' ' || *c == '\t')
                    .map(|c| c.len_utf8())
                    .sum();
                let after_ws1 = &rest[ws1..];
                if after_ws1.starts_with(';') {
                    scan += ws1 + 1;
                    let after_semi = &live_src[scan..];
                    let ws2: usize = after_semi
                        .chars()
                        .take_while(|c| *c == ' ' || *c == '\t')
                        .map(|c| c.len_utf8())
                        .sum();
                    scan += ws2;
                } else if after_ws1.is_empty() || after_ws1.starts_with(['\n', '\r']) {
                    scan += ws1;
                }

                let comment_end = skip_same_line_hash_comment(live_src, scan);
                let comment_tail = live_src[scan..comment_end].trim_start_matches([' ', '\t']);
                let preserved_comment = if comment_tail.is_empty() {
                    String::new()
                } else {
                    format!(" {comment_tail}")
                };

                let member_indent = resolve_python_member_indent(
                    tree.root_node(),
                    pu,
                    live_src,
                    full_text_with_trivia,
                    sibling_pending_texts,
                );
                let insert_text =
                    normalize_python_member_indent(full_text_with_trivia, &member_indent);

                let insertion = format!("{preserved_comment}{nl}{insert_text}");
                live_src.replace_range(body_start..comment_end, &insertion);
                adjust_spans_for_edit(
                    body_start,
                    comment_end,
                    insertion.len(),
                    parent_base_idx,
                    tracked_base,
                    tracked_inserts,
                );
                let text_start = body_start + preserved_comment.len() + nl.len();
                Ok(text_start..(text_start + insert_text.len()))
            }
            PythonInsertAction::AfterDocstring {
                doc_start,
                mut doc_end,
                on_colon_row,
            } => {
                let member_indent = resolve_python_member_indent(
                    tree.root_node(),
                    pu,
                    live_src,
                    full_text_with_trivia,
                    sibling_pending_texts,
                );
                if on_colon_row {
                    let prefix_ins = format!("{nl}{member_indent}");
                    live_src.replace_range(body_start..doc_start, &prefix_ins);
                    adjust_spans_for_edit(
                        body_start,
                        doc_start,
                        prefix_ins.len(),
                        parent_base_idx,
                        tracked_base,
                        tracked_inserts,
                    );
                    let delta = prefix_ins.len() as isize - (doc_start - body_start) as isize;
                    doc_end = (doc_end as isize + delta) as usize;
                }
                let insert_text =
                    normalize_python_member_indent(full_text_with_trivia, &member_indent);
                let rest = &live_src[doc_end..];
                let sep_len = scan_python_inline_sep(rest);
                let after_sep = &rest[sep_len..];
                let has_same_line_stmt =
                    !after_sep.is_empty() && !after_sep.starts_with(['\n', '\r']);
                if has_same_line_stmt {
                    let insertion = format!("{nl}{insert_text}{nl}{member_indent}");
                    live_src.replace_range(doc_end..(doc_end + sep_len), &insertion);
                    adjust_spans_for_edit(
                        doc_end,
                        doc_end + sep_len,
                        insertion.len(),
                        parent_base_idx,
                        tracked_base,
                        tracked_inserts,
                    );
                    let text_start = doc_end + nl.len();
                    Ok(text_start..(text_start + insert_text.len()))
                } else {
                    let has_blank = after_sep.starts_with("\n\n") || after_sep.starts_with("\r\n\r\n");
                    let (nl_len, insertion) = if has_blank {
                        (2 * nl.len(), format!("{nl}{nl}{insert_text}"))
                    } else {
                        (nl.len(), format!("{nl}{insert_text}"))
                    };
                    live_src.replace_range(doc_end..(doc_end + sep_len), &insertion);
                    adjust_spans_for_edit(
                        doc_end,
                        doc_end + sep_len,
                        insertion.len(),
                        parent_base_idx,
                        tracked_base,
                        tracked_inserts,
                    );
                    let text_start = doc_end + nl_len;
                    Ok(text_start..(text_start + insert_text.len()))
                }
            }
            PythonInsertAction::AfterColon => {
                let member_indent = resolve_python_member_indent(
                    tree.root_node(),
                    pu,
                    live_src,
                    full_text_with_trivia,
                    sibling_pending_texts,
                );
                let insert_text =
                    normalize_python_member_indent(full_text_with_trivia, &member_indent);
                let insert_pos = skip_same_line_hash_comment(live_src, body_start);
                let rest = &live_src[insert_pos..];
                let sep_len = scan_python_inline_sep(rest);
                let after_sep = &rest[sep_len..];
                let has_same_line_stmt =
                    !after_sep.is_empty() && !after_sep.starts_with(['\n', '\r']);
                if has_same_line_stmt {
                    let insertion = format!("{nl}{insert_text}{nl}{member_indent}");
                    live_src.replace_range(insert_pos..(insert_pos + sep_len), &insertion);
                    adjust_spans_for_edit(
                        insert_pos,
                        insert_pos + sep_len,
                        insertion.len(),
                        parent_base_idx,
                        tracked_base,
                        tracked_inserts,
                    );
                    let text_start = insert_pos + nl.len();
                    Ok(text_start..(text_start + insert_text.len()))
                } else {
                    let insertion = format!("{nl}{insert_text}");
                    live_src.insert_str(insert_pos, &insertion);
                    adjust_spans_for_edit(
                        insert_pos,
                        insert_pos,
                        insertion.len(),
                        parent_base_idx,
                        tracked_base,
                        tracked_inserts,
                    );
                    let text_start = insert_pos + nl.len();
                    Ok(text_start..(text_start + insert_text.len()))
                }
            }
        }
    } else {
        // Brace/paren container: find opening '{' or '(' at body_range.start
        let open_end = (body_start + 1).min(live_src.len());
        let rest = &live_src[open_end..];
        let is_single_line_empty = rest.starts_with('}') || rest.starts_with(')');
        if is_single_line_empty {
            let indent = line_indent_at(live_src, pu_trivia_start).to_string();
            let insertion = format!("{nl}{full_text_with_trivia}{nl}{indent}");
            live_src.insert_str(open_end, &insertion);
            adjust_spans_for_edit(
                open_end,
                open_end,
                insertion.len(),
                parent_base_idx,
                tracked_base,
                tracked_inserts,
            );
            let text_start = open_end + nl.len();
            Ok(text_start..(text_start + full_text_with_trivia.len()))
        } else {
            let needs_trailing_nl = !rest.trim_start_matches([' ', '\t']).starts_with(['\n', '\r']);
            let mut insertion = if is_comma_sep && !full_text_with_trivia.trim_end().ends_with(',') {
                format!("{nl}{full_text_with_trivia},")
            } else {
                format!("{nl}{full_text_with_trivia}")
            };
            if needs_trailing_nl {
                insertion.push_str(nl);
            }
            live_src.insert_str(open_end, &insertion);
            adjust_spans_for_edit(
                open_end,
                open_end,
                insertion.len(),
                parent_base_idx,
                tracked_base,
                tracked_inserts,
            );
            let text_start = open_end + nl.len();
            Ok(text_start..(text_start + full_text_with_trivia.len()))
        }
    }
}

fn line_indent_at(source: &str, byte_idx: usize) -> &str {
    let bytes = source.as_bytes();
    let idx = byte_idx.min(bytes.len());
    let mut start = idx;
    while start > 0 && bytes[start - 1] != b'\n' && bytes[start - 1] != b'\r' {
        start -= 1;
    }
    let mut end = start;
    while end < bytes.len() && (bytes[end] == b' ' || bytes[end] == b'\t') {
        end += 1;
    }
    &source[start..end]
}

fn skip_same_line_trailing_comment(source: &str, pos: usize) -> usize {
    let pos = pos.min(source.len());
    let tail = &source[pos..];
    let mut ws_bytes = 0usize;
    for (t_off, t_ch) in tail.char_indices() {
        if t_ch == ' ' || t_ch == '\t' {
            ws_bytes = t_off + t_ch.len_utf8();
        } else {
            break;
        }
    }
    let after_ws = &tail[ws_bytes..];
    if after_ws.starts_with("//") {
        let line_len = after_ws
            .find(['\r', '\n'])
            .unwrap_or(after_ws.len());
        return pos + ws_bytes + line_len;
    }
    if after_ws.starts_with("/*")
        && let Some(close_idx) = after_ws[2..].find("*/")
    {
        let comment_end = 2 + close_idx + 2;
        if !after_ws[..comment_end].contains(['\r', '\n']) {
            let after_blk = &after_ws[comment_end..];
            let extra_ws: usize = after_blk
                .chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .map(|c| c.len_utf8())
                .sum();
            if after_blk[extra_ws..].starts_with("//") {
                let line_len = after_blk[extra_ws..]
                    .find(['\r', '\n'])
                    .unwrap_or(after_blk.len() - extra_ws);
                return pos + ws_bytes + comment_end + extra_ws + line_len;
            }
            return pos + ws_bytes + comment_end;
        }
    }
    pos
}

fn find_trailing_comma_end(source: &str, end_byte: usize) -> Option<usize> {
    let start = end_byte.min(source.len());
    let rest = &source[start..];
    for (offset, ch) in rest.char_indices() {
        if ch == ',' {
            let after_comma = start + offset + 1;
            return Some(skip_same_line_trailing_comment(source, after_comma));
        }
        if !ch.is_whitespace() {
            return None;
        }
    }
    None
}

fn find_trailing_semicolon_end(
    source: &str,
    end_byte: usize,
    lang: LanguageKind,
    has_parent: bool,
) -> Option<(usize, bool)> {
    let start = end_byte.min(source.len());
    let rest = &source[start..];
    let ws1: usize = rest
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .map(|c| c.len_utf8())
        .sum();
    if !rest[ws1..].starts_with(';') {
        return None;
    }
    let after_semi = start + ws1 + 1;
    let after_rest = &source[after_semi..];
    let ws2: usize = after_rest
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .map(|c| c.len_utf8())
        .sum();
    let tail = &after_rest[ws2..];
    let same_line_follows = match lang {
        LanguageKind::Python => !tail.is_empty() && !tail.starts_with(['\n', '\r', '#']),
        LanguageKind::Go => {
            !tail.is_empty()
                && !tail.starts_with(['\n', '\r', '}', ')'])
                && !tail.starts_with("//")
                && !tail.starts_with("/*")
        }
        _ => false,
    };
    if same_line_follows {
        Some((after_semi + ws2, true))
    } else if lang == LanguageKind::Go || (lang == LanguageKind::Python && has_parent) {
        Some((after_semi, false))
    } else {
        None
    }
}

fn scan_python_inline_sep(rest: &str) -> usize {
    let mut idx = 0usize;
    let bytes = rest.as_bytes();
    while idx < bytes.len() && matches!(bytes[idx], b' ' | b'\t') {
        idx += 1;
    }
    if idx < bytes.len() && bytes[idx] == b';' {
        idx += 1;
        while idx < bytes.len() && matches!(bytes[idx], b' ' | b'\t') {
            idx += 1;
        }
    }
    idx
}

fn python_explicit_member_indent(text: &str) -> &str {
    let mut first_non_empty_ws: Option<&str> = None;
    for line in text.lines() {
        if !line.trim().is_empty() {
            let ws_len: usize = line
                .chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .map(|c| c.len_utf8())
                .sum();
            let ws = &line[..ws_len];
            if first_non_empty_ws.is_none() {
                first_non_empty_ws = Some(ws);
            }
            if !line.trim_start().starts_with('#') {
                return ws;
            }
        }
    }
    first_non_empty_ws.unwrap_or("")
}

fn established_python_body_indent(
    root: tree_sitter::Node<'_>,
    pu: &DeclUnit,
    source: &str,
    pu_indent: &str,
) -> Option<String> {
    let body_range = pu.body_range.as_ref()?;
    let colon_row = byte_row_in_source(source, body_range.start.saturating_sub(1));
    if let Some(mut node) = root.descendant_for_byte_range(pu.decl_start_byte, pu.end_byte) {
        while node.kind() != "class_definition" || node.start_byte() != pu.decl_start_byte {
            let Some(p) = node.parent() else {
                break;
            };
            node = p;
        }
        if node.kind() == "class_definition"
            && node.start_byte() == pu.decl_start_byte
            && let Some(body) = node.child_by_field_name("body")
        {
            let mut comment_indent: Option<String> = None;
            let mut cursor = body.walk();
            for child in body.named_children(&mut cursor) {
                if child.start_position().row > colon_row {
                    let ws = line_indent_at(source, child.start_byte());
                    if ws.len() > pu_indent.len() && ws.starts_with(pu_indent) {
                        if child.kind() != "comment" {
                            return Some(ws.to_string());
                        } else if comment_indent.is_none() {
                            comment_indent = Some(ws.to_string());
                        }
                    }
                }
            }
            if comment_indent.is_some() {
                return comment_indent;
            }
        }
    }
    None
}

fn resolve_python_member_indent(
    root: tree_sitter::Node<'_>,
    pu: &DeclUnit,
    live_src: &str,
    current_text: &str,
    sibling_pending_texts: &[&str],
) -> String {
    let pu_indent = line_indent_at(live_src, pu.decl_start_byte);
    if let Some(existing) = established_python_body_indent(root, pu, live_src, pu_indent) {
        return existing;
    }
    for candidate in std::iter::once(current_text).chain(sibling_pending_texts.iter().copied()) {
        let ws = python_explicit_member_indent(candidate);
        if ws.len() > pu_indent.len() && ws.starts_with(pu_indent) {
            return ws.to_string();
        }
    }
    if pu_indent.contains('\t') {
        format!("{pu_indent}\t")
    } else {
        format!("{pu_indent}    ")
    }
}

fn detect_python_eol(live_src: &str, fallback_hint: &str) -> &'static str {
    let lf_count = live_src.matches('\n').count();
    let crlf_count = live_src.matches("\r\n").count();
    if (crlf_count > 0 && crlf_count >= lf_count - crlf_count)
        || (lf_count == 0 && fallback_hint.contains("\r\n"))
    {
        "\r\n"
    } else {
        "\n"
    }
}

#[allow(clippy::too_many_arguments)]
fn expand_python_inline_unit_to_line_start(
    live_src: &mut String,
    parsed: &mut ParsedFile,
    units: &mut Vec<DeclUnit>,
    logical_file_id: Blake3Hash,
    base_idx: usize,
    tracked_base: &mut [TrackedBaseUnit],
    tracked_inserts: &mut [TrackedInsertUnit],
    member_indent_hint: &str,
    sibling_hints: &[&str],
    eol_hint: &str,
) -> Result<(), CstError> {
    let nl = detect_python_eol(live_src, eol_hint);
    let parent_base_idx = tracked_base[base_idx].parent_idx;

    if let Some(p_idx) = parent_base_idx
        && let Some(pu) = resolve_base_unit(units, &tracked_base[p_idx], tracked_inserts)
        && let Some(pu_body) = pu.body_range.as_ref()
        && let Some(unit) = resolve_base_unit(units, &tracked_base[base_idx], tracked_inserts)
    {
        let colon_row = byte_row_in_source(live_src, pu_body.start.saturating_sub(1));
        let unit_row = byte_row_in_source(live_src, unit.decl_start_byte);
        if unit_row == colon_row {
            let gaps = find_python_colon_row_inline_gaps(
                parsed.tree.root_node(),
                pu,
                pu_body.start,
                live_src,
            );
            if !gaps.is_empty() {
                let member_indent = resolve_python_member_indent(
                    parsed.tree.root_node(),
                    pu,
                    live_src,
                    member_indent_hint,
                    sibling_hints,
                );
                let sep = format!("{nl}{member_indent}");
                for (gap_start, gap_end) in gaps.into_iter().rev() {
                    live_src.replace_range(gap_start..gap_end, &sep);
                    adjust_spans_for_edit(
                        gap_start,
                        gap_end,
                        sep.len(),
                        parent_base_idx,
                        tracked_base,
                        tracked_inserts,
                    );
                }
                *parsed = parse_source(LanguageKind::Python, live_src, None)?;
                *units = extract_decl_units(
                    LanguageKind::Python,
                    &parsed.tree,
                    live_src,
                    logical_file_id,
                );
            }
        }
    }

    if let Some(unit) = resolve_base_unit(units, &tracked_base[base_idx], tracked_inserts) {
        let hdr_start = unit.header_range.start;
        let at_line_start =
            hdr_start == 0 || matches!(live_src.as_bytes()[hdr_start - 1], b'\n' | b'\r');
        if !at_line_start {
            let bytes = live_src.as_bytes();
            let mut scan = hdr_start;
            while scan > 0 && matches!(bytes[scan - 1], b' ' | b'\t') {
                scan -= 1;
            }
            if scan > 0 && bytes[scan - 1] == b';' {
                let mut semi_gap_start = scan - 1;
                while semi_gap_start > 0 && matches!(bytes[semi_gap_start - 1], b' ' | b'\t') {
                    semi_gap_start -= 1;
                }
                let member_indent = if let Some(p_idx) = parent_base_idx
                    && let Some(pu) = resolve_base_unit(units, &tracked_base[p_idx], tracked_inserts)
                {
                    resolve_python_member_indent(
                        parsed.tree.root_node(),
                        pu,
                        live_src,
                        member_indent_hint,
                        sibling_hints,
                    )
                } else {
                    line_indent_at(live_src, hdr_start).to_string()
                };
                let sep = format!("{nl}{member_indent}");
                live_src.replace_range(semi_gap_start..hdr_start, &sep);
                adjust_spans_for_edit(
                    semi_gap_start,
                    hdr_start,
                    sep.len(),
                    parent_base_idx,
                    tracked_base,
                    tracked_inserts,
                );
                *parsed = parse_source(LanguageKind::Python, live_src, None)?;
                *units = extract_decl_units(
                    LanguageKind::Python,
                    &parsed.tree,
                    live_src,
                    logical_file_id,
                );
            }
        }
    }

    Ok(())
}

fn python_multiline_string_continuation_rows(text: &str) -> HashSet<usize> {
    let mut rows = HashSet::new();
    let wrapped = format!("if True:\n{text}");
    let Ok(parsed) = parse_source(LanguageKind::Python, &wrapped, None) else {
        return rows;
    };
    fn walk(node: tree_sitter::Node<'_>, rows: &mut HashSet<usize>) {
        if node.kind() == "string" {
            let sr = node.start_position().row;
            let er = node.end_position().row;
            if er > sr {
                for r in (sr + 1)..=er {
                    if r >= 1 {
                        rows.insert(r - 1);
                    }
                }
            }
            return;
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            walk(child, rows);
        }
    }
    walk(parsed.tree.root_node(), &mut rows);
    rows
}

fn normalize_python_lines_indent(
    text: &str,
    src_indent: &str,
    target_indent: &str,
    skip_rows: &HashSet<usize>,
) -> String {
    let mut out = String::with_capacity(text.len() + 32);
    let bytes = text.as_bytes();
    let mut pos = 0usize;
    let mut row_idx = 0usize;
    let convert_tabs = src_indent.contains('\t') && !target_indent.contains('\t');

    while pos < bytes.len() {
        let line_start = pos;
        while pos < bytes.len() && bytes[pos] != b'\n' && bytes[pos] != b'\r' {
            pos += 1;
        }
        let line_body = &text[line_start..pos];
        let eol_start = pos;
        if pos < bytes.len() {
            if bytes[pos] == b'\r' {
                pos += 1;
                if pos < bytes.len() && bytes[pos] == b'\n' {
                    pos += 1;
                }
            } else {
                pos += 1;
            }
        }
        let line_eol = &text[eol_start..pos];
        if skip_rows.contains(&row_idx) || line_body.trim().is_empty() {
            out.push_str(line_body);
        } else if let Some(rest) = line_body.strip_prefix(src_indent) {
            out.push_str(target_indent);
            if convert_tabs && rest.starts_with('\t') {
                let tab_count = rest.chars().take_while(|c| *c == '\t').count();
                for _ in 0..tab_count {
                    out.push_str("    ");
                }
                out.push_str(&rest[tab_count..]);
            } else {
                out.push_str(rest);
            }
        } else if !line_body.starts_with([' ', '\t']) && line_body.trim_start().starts_with('#') {
            out.push_str(target_indent);
            out.push_str(line_body);
        } else {
            out.push_str(line_body);
        }
        out.push_str(line_eol);
        row_idx += 1;
    }
    out
}

fn normalize_python_member_indent(full_text_with_trivia: &str, target_indent: &str) -> String {
    let src_indent = python_explicit_member_indent(full_text_with_trivia);
    if src_indent.is_empty() {
        if target_indent.is_empty() || full_text_with_trivia.starts_with([' ', '\t']) {
            return full_text_with_trivia.to_string();
        }
        let skip_rows = python_multiline_string_continuation_rows(full_text_with_trivia);
        return normalize_python_lines_indent(full_text_with_trivia, "", target_indent, &skip_rows);
    }
    let first_line_ws = full_text_with_trivia
        .lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| {
            let ws_len: usize = l
                .chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .map(|c| c.len_utf8())
                .sum();
            &l[..ws_len]
        })
        .unwrap_or("");
    if src_indent == target_indent && first_line_ws == target_indent {
        return full_text_with_trivia.to_string();
    }

    let skip_rows = python_multiline_string_continuation_rows(full_text_with_trivia);
    normalize_python_lines_indent(full_text_with_trivia, src_indent, target_indent, &skip_rows)
}

fn split_first_line(s: &str) -> Option<(&str, &str, &str)> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i] != b'\n' && bytes[i] != b'\r' {
        i += 1;
    }
    if i == bytes.len() {
        return None;
    }
    let eol_start = i;
    if bytes[i] == b'\r' {
        i += 1;
        if i < bytes.len() && bytes[i] == b'\n' {
            i += 1;
        }
    } else {
        i += 1;
    }
    Some((&s[..eol_start], &s[eol_start..i], &s[i..]))
}

fn python_block_body_indent_and_skip_rows(body_text: &str) -> (Option<String>, HashSet<usize>) {
    let mut skip_rows = HashSet::new();
    let Some((colon_tail, _, body_rest)) = split_first_line(body_text) else {
        return (None, skip_rows);
    };
    if !colon_tail.trim().is_empty() && !colon_tail.trim_start().starts_with('#') {
        return (None, skip_rows);
    }

    let wrapped = format!("def _loom_wrap():{body_text}");
    let mut block_stmt_ws: Option<String> = None;
    if let Ok(parsed) = parse_source(LanguageKind::Python, &wrapped, None) {
        let root = parsed.tree.root_node();
        fn walk_strings(node: tree_sitter::Node<'_>, rows: &mut HashSet<usize>) {
            if node.kind() == "string" {
                let sr = node.start_position().row;
                let er = node.end_position().row;
                if er > sr {
                    for r in (sr + 1)..=er {
                        if r >= 1 {
                            rows.insert(r - 1);
                        }
                    }
                }
                return;
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                walk_strings(child, rows);
            }
        }
        walk_strings(root, &mut skip_rows);

        let mut root_cursor = root.walk();
        if let Some(func_node) = root
            .named_children(&mut root_cursor)
            .find(|n| n.kind() == "function_definition")
            && let Some(body_node) = func_node.child_by_field_name("body")
        {
            let mut body_cursor = body_node.walk();
            if let Some(first_stmt) = body_node
                .named_children(&mut body_cursor)
                .find(|c| c.kind() != "comment" && c.start_position().row >= 1)
            {
                let ws = line_indent_at(&wrapped, first_stmt.start_byte());
                if !ws.is_empty() {
                    block_stmt_ws = Some(ws.to_string());
                }
            }
        }
    }

    if block_stmt_ws.is_none() {
        let fallback = python_explicit_member_indent(body_rest);
        if !fallback.is_empty() {
            block_stmt_ws = Some(fallback.to_string());
        }
    }

    (block_stmt_ws, skip_rows)
}

fn resolve_python_replace_body_target_indent(
    live_src: &str,
    unit: &DeclUnit,
    units: &[DeclUnit],
    old_live_body: &str,
    hdr_ws: &str,
    src_body_ws: &str,
) -> String {
    if let (Some(old_ws), _) = python_block_body_indent_and_skip_rows(old_live_body)
        && old_ws.len() > hdr_ws.len()
        && old_ws.starts_with(hdr_ws)
    {
        let ext = &old_ws[hdr_ws.len()..];
        let style_ok = if hdr_ws.contains('\t') {
            ext.chars().all(|c| c == '\t')
        } else {
            !old_ws.contains('\t')
        };
        if style_ok {
            return old_ws;
        }
    }

    if hdr_ws.contains('\t') {
        return format!("{hdr_ws}\t");
    }

    if !hdr_ws.is_empty() {
        if let Some(parent_anchor) = unit.parent_anchor.as_ref() {
            let parent_key = node_key(parent_anchor);
            if let Some(pu) = flatten_decl_units(units)
                .into_iter()
                .find(|u| node_key(&u.anchor) == parent_key)
            {
                let pu_ws = line_indent_at(live_src, pu.decl_start_byte);
                if hdr_ws.len() > pu_ws.len() && hdr_ws.starts_with(pu_ws) {
                    let step = &hdr_ws[pu_ws.len()..];
                    if !step.contains('\t') {
                        return format!("{hdr_ws}{step}");
                    }
                }
            }
        }
        return format!("{hdr_ws}    ");
    }

    src_body_ws.to_string()
}

fn normalize_python_replace_body<'a>(
    live_src: &str,
    unit: &DeclUnit,
    units: &[DeclUnit],
    old_live_body: &str,
    new_body_text: &'a str,
) -> std::borrow::Cow<'a, str> {
    let Some((colon_tail, first_eol, body_rest)) = split_first_line(new_body_text) else {
        return std::borrow::Cow::Borrowed(new_body_text);
    };
    if !colon_tail.trim().is_empty() && !colon_tail.trim_start().starts_with('#') {
        return std::borrow::Cow::Borrowed(new_body_text);
    }

    let (Some(src_body_ws), skip_rows) = python_block_body_indent_and_skip_rows(new_body_text)
    else {
        return std::borrow::Cow::Borrowed(new_body_text);
    };

    let hdr_ws = line_indent_at(live_src, unit.decl_start_byte);
    let line_start = live_src[..unit.decl_start_byte]
        .rfind(['\n', '\r'])
        .map_or(0, |p| p + 1);
    if unit.decl_start_byte != line_start + hdr_ws.len() {
        return std::borrow::Cow::Borrowed(new_body_text);
    }

    let target_body_ws = resolve_python_replace_body_target_indent(
        live_src,
        unit,
        units,
        old_live_body,
        hdr_ws,
        &src_body_ws,
    );

    if target_body_ws == src_body_ws {
        return std::borrow::Cow::Borrowed(new_body_text);
    }

    let normalized_rest =
        normalize_python_lines_indent(body_rest, &src_body_ws, &target_body_ws, &skip_rows);
    std::borrow::Cow::Owned(format!("{colon_tail}{first_eol}{normalized_rest}"))
}

fn compute_removal_trim_span(source: &str, start: usize, end: usize) -> (usize, usize) {
    let bytes = source.as_bytes();
    let mut new_end = end;
    let mut removed_trailing_nl = 0usize;
    for _ in 0..2 {
        if new_end < bytes.len() && bytes[new_end] == b'\r' {
            if new_end + 1 < bytes.len() && bytes[new_end + 1] == b'\n' {
                new_end += 2;
                removed_trailing_nl += 1;
            } else {
                new_end += 1;
                removed_trailing_nl += 1;
            }
        } else if new_end < bytes.len() && bytes[new_end] == b'\n' {
            new_end += 1;
            removed_trailing_nl += 1;
        } else {
            break;
        }
    }
    let mut new_start = start;
    if removed_trailing_nl == 1 && new_end == bytes.len() {
        // Trim one preceding blank newline if removing last item in file
        if new_start >= 2 && bytes[new_start - 2] == b'\r' && bytes[new_start - 1] == b'\n' {
            new_start -= 2;
        } else if new_start >= 1 && bytes[new_start - 1] == b'\n' {
            new_start -= 1;
        }
    }
    (new_start, new_end)
}

fn find_file_root_insert_pos(
    lang: LanguageKind,
    root: tree_sitter::Node<'_>,
    units: &[DeclUnit],
    source: &str,
    header_tier: u8,
) -> Option<usize> {
    let header_end = match lang {
        LanguageKind::Go => find_go_header_end_byte(root, source, header_tier),
        LanguageKind::Rust => find_rust_header_end_byte(root, source),
        LanguageKind::Python => find_python_header_end_byte(root, source, header_tier),
        _ => None,
    };
    let first_unit_trivia_start = units
        .first()
        .map(|u| u.trivia_start_byte)
        .unwrap_or(source.len());
    let header_limit = header_end.unwrap_or(0);
    let mut detached_end = None;
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.end_byte() <= header_limit {
            continue;
        }
        if child.start_byte() >= first_unit_trivia_start
            || child.end_byte() > first_unit_trivia_start
        {
            break;
        }
        if super::trivia::is_trivia_node(lang, child) {
            let mut end = child.end_byte().min(source.len());
            while end > child.start_byte() && matches!(source.as_bytes()[end - 1], b'\n' | b'\r') {
                end -= 1;
            }
            detached_end = Some(end);
        } else {
            break;
        }
    }
    detached_end.or(header_end)
}

fn find_go_header_end_byte(
    root: tree_sitter::Node<'_>,
    source: &str,
    header_tier: u8,
) -> Option<usize> {
    let mut last_end = None;
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        let kind = child.kind();
        if kind == "comment" {
            continue;
        }
        if kind == "package_clause" || (header_tier >= 2 && kind == "import_declaration") {
            let mut end =
                skip_same_line_trailing_comment(source, child.end_byte()).min(source.len());
            while end > child.start_byte() && matches!(source.as_bytes()[end - 1], b'\n' | b'\r') {
                end -= 1;
            }
            last_end = Some(end);
        } else if last_end.is_some() {
            break;
        }
    }
    last_end
}

fn find_rust_header_end_byte(root: tree_sitter::Node<'_>, source: &str) -> Option<usize> {
    let mut last_end = None;
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        let kind = child.kind();
        let is_inner = kind == "inner_attribute_item"
            || (matches!(kind, "line_comment" | "block_comment")
                && child.child_by_field_name("inner").is_some());
        if is_inner {
            let mut end = child.end_byte().min(source.len());
            if source[..end].ends_with('\r') && source[end..].starts_with('\n') {
                end -= 1;
            }
            last_end = Some(end);
        } else if !matches!(kind, "line_comment" | "block_comment" | "attribute_item") {
            break;
        }
    }
    last_end
}

enum PythonInsertAction {
    ReplacePass(usize),
    AfterDocstring {
        doc_start: usize,
        doc_end: usize,
        on_colon_row: bool,
    },
    AfterColon,
}

fn byte_row_in_source(source: &str, byte_idx: usize) -> usize {
    source[..byte_idx.min(source.len())]
        .bytes()
        .filter(|&b| b == b'\n')
        .count()
}

fn skip_same_line_hash_comment(source: &str, pos: usize) -> usize {
    let start = pos.min(source.len());
    let tail = &source[start..];
    let ws: usize = tail
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .map(|c| c.len_utf8())
        .sum();
    if tail[ws..].starts_with('#') {
        let line_len = tail[ws..].find(['\r', '\n']).unwrap_or(tail.len() - ws);
        start + ws + line_len
    } else {
        start
    }
}

fn is_python_docstring_stmt(stmt: tree_sitter::Node<'_>) -> bool {
    if stmt.kind() != "expression_statement" {
        return false;
    }
    let mut cursor = stmt.walk();
    let mut has_string = false;
    for child in stmt.named_children(&mut cursor) {
        if child.kind() == "string" {
            has_string = true;
        } else if child.kind() != "comment" {
            return false;
        }
    }
    has_string
}

fn collect_empty_python_class_colons(
    node: tree_sitter::Node<'_>,
    tracked_base: &[TrackedBaseUnit],
    live_src: &str,
    out: &mut Vec<(usize, String)>,
) {
    if node.kind() == "class_definition" {
        let outer_start = node
            .parent()
            .filter(|p| p.kind() == "decorated_definition")
            .map_or_else(|| node.start_byte(), |p| p.start_byte());
        let body_opt = node.child_by_field_name("body");
        let is_empty_body = match body_opt {
            None => true,
            Some(body) => {
                let mut cursor = body.walk();
                !body
                    .named_children(&mut cursor)
                    .any(|c| c.kind() != "comment")
            }
        };
        if is_empty_body
            && tracked_base.iter().any(|tb| {
                !tb.removed
                    && tb.is_container
                    && tb.live_span.start <= outer_start
                    && outer_start <= tb.live_span.end
            })
        {
            let colon_opt = {
                let limit = body_opt.map_or(node.end_byte(), |b| b.start_byte());
                let mut cursor = node.walk();
                let mut last_colon = None;
                for ch in node.children(&mut cursor) {
                    if ch.start_byte() >= limit {
                        break;
                    }
                    if ch.kind() == ":" {
                        last_colon = Some(ch.end_byte());
                    }
                }
                last_colon
            };
            if let Some(colon_end) = colon_opt {
                let after_comment = skip_same_line_hash_comment(live_src, colon_end);
                let tail = &live_src[after_comment..];
                let trailing_ws: usize = tail
                    .chars()
                    .take_while(|c| *c == ' ' || *c == '\t')
                    .map(|c| c.len_utf8())
                    .sum();
                let line_end = if tail[trailing_ws..].is_empty()
                    || tail[trailing_ws..].starts_with(['\n', '\r'])
                {
                    after_comment + trailing_ws
                } else {
                    after_comment
                };
                let indent = line_indent_at(live_src, node.start_byte());
                out.push((line_end, format!("{indent}    pass")));
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_empty_python_class_colons(child, tracked_base, live_src, out);
    }
}

fn repair_emptied_python_classes(
    live_src: &mut String,
    tracked_base: &mut [TrackedBaseUnit],
    tracked_inserts: &mut [TrackedInsertUnit],
) -> Result<(), CstError> {
    let parsed = parse_source(LanguageKind::Python, live_src, None)?;
    let mut empties = Vec::new();
    collect_empty_python_class_colons(parsed.tree.root_node(), tracked_base, live_src, &mut empties);
    if empties.is_empty() {
        return Ok(());
    }
    let lf_count = live_src.matches('\n').count();
    let crlf_count = live_src.matches("\r\n").count();
    let nl = if crlf_count > 0 && crlf_count >= lf_count - crlf_count {
        "\r\n"
    } else {
        "\n"
    };
    empties.sort_by_key(|b| std::cmp::Reverse(b.0));
    empties.dedup_by_key(|x| x.0);
    for (line_end, pass_line) in empties {
        let insertion = format!("{nl}{pass_line}");
        live_src.insert_str(line_end, &insertion);
        adjust_spans_for_edit(
            line_end,
            line_end,
            insertion.len(),
            None,
            tracked_base,
            tracked_inserts,
        );
    }
    Ok(())
}

fn find_python_header_end_byte(
    root: tree_sitter::Node<'_>,
    source: &str,
    header_tier: u8,
) -> Option<usize> {
    let mut last_end = None;
    let mut seen_docstring = false;
    let mut seen_import = false;
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        let kind = child.kind();
        if kind == "comment" {
            continue;
        }
        if !seen_docstring && !seen_import && is_python_docstring_stmt(child) {
            seen_docstring = true;
            let mut end = skip_same_line_hash_comment(source, child.end_byte()).min(source.len());
            while end > child.start_byte() && matches!(source.as_bytes()[end - 1], b'\n' | b'\r') {
                end -= 1;
            }
            last_end = Some(end);
            continue;
        }
        if header_tier >= 1 && kind == "future_import_statement" {
            seen_import = true;
            let mut end = skip_same_line_hash_comment(source, child.end_byte()).min(source.len());
            while end > child.start_byte() && matches!(source.as_bytes()[end - 1], b'\n' | b'\r') {
                end -= 1;
            }
            last_end = Some(end);
            continue;
        }
        if header_tier >= 2 && matches!(kind, "import_statement" | "import_from_statement") {
            seen_import = true;
            let mut end = skip_same_line_hash_comment(source, child.end_byte()).min(source.len());
            while end > child.start_byte() && matches!(source.as_bytes()[end - 1], b'\n' | b'\r') {
                end -= 1;
            }
            last_end = Some(end);
            continue;
        }
        break;
    }
    last_end
}

fn is_python_stub_stmt(stmt: tree_sitter::Node<'_>) -> bool {
    if matches!(stmt.kind(), "pass_statement" | "ellipsis") {
        return true;
    }
    if stmt.kind() != "expression_statement" {
        return false;
    }
    let mut cursor = stmt.walk();
    let mut saw_ellipsis = false;
    for child in stmt.named_children(&mut cursor) {
        if child.kind() == "ellipsis" {
            if saw_ellipsis {
                return false;
            }
            saw_ellipsis = true;
        } else if child.kind() != "comment" {
            return false;
        }
    }
    saw_ellipsis
}

fn find_python_class_insertion(
    root: tree_sitter::Node<'_>,
    pu: &DeclUnit,
    body_range: &Range<usize>,
    source: &str,
) -> PythonInsertAction {
    let Some(mut node) = root.descendant_for_byte_range(pu.decl_start_byte, pu.end_byte) else {
        return PythonInsertAction::AfterColon;
    };
    while node.kind() != "class_definition" || node.start_byte() != pu.decl_start_byte {
        let Some(p) = node.parent() else {
            return PythonInsertAction::AfterColon;
        };
        node = p;
    }
    let Some(body) = node.child_by_field_name("body") else {
        return PythonInsertAction::AfterColon;
    };
    let colon_row = byte_row_in_source(source, body_range.start.saturating_sub(1));
    let mut non_comment = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() != "comment" {
            non_comment.push(child);
        }
    }
    if non_comment.len() == 1
        && is_python_stub_stmt(non_comment[0])
        && non_comment[0].start_position().row == colon_row
    {
        return PythonInsertAction::ReplacePass(non_comment[0].end_byte());
    }
    if let Some(&first) = non_comment.first()
        && is_python_docstring_stmt(first)
    {
        return PythonInsertAction::AfterDocstring {
            doc_start: first.start_byte(),
            doc_end: skip_same_line_hash_comment(source, first.end_byte()),
            on_colon_row: first.start_position().row == colon_row,
        };
    }
    PythonInsertAction::AfterColon
}

fn find_python_colon_row_inline_gaps(
    root: tree_sitter::Node<'_>,
    pu: &DeclUnit,
    body_start: usize,
    source: &str,
) -> Vec<(usize, usize)> {
    let Some(mut node) = root.descendant_for_byte_range(pu.decl_start_byte, pu.end_byte) else {
        return Vec::new();
    };
    while node.start_byte() != pu.decl_start_byte || node.child_by_field_name("body").is_none() {
        let Some(p) = node.parent() else {
            return Vec::new();
        };
        node = p;
    }
    let Some(body) = node.child_by_field_name("body") else {
        return Vec::new();
    };
    let colon_row = byte_row_in_source(source, body_start.saturating_sub(1));
    let mut inline_stmts = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() == "comment" {
            continue;
        }
        if inline_stmts.is_empty() {
            if child.start_position().row == colon_row {
                inline_stmts.push(child);
            } else {
                break;
            }
        } else {
            let prev = *inline_stmts.last().unwrap();
            if child.start_position().row == prev.end_position().row {
                inline_stmts.push(child);
            } else {
                break;
            }
        }
    }
    if inline_stmts.is_empty() {
        return Vec::new();
    }
    let mut gaps = Vec::with_capacity(inline_stmts.len());
    let first_start = inline_stmts[0].start_byte();
    if body_start <= first_start {
        gaps.push((body_start, first_start));
    }
    for pair in inline_stmts.windows(2) {
        let prev_end = pair[0].end_byte();
        let next_start = pair[1].start_byte();
        if prev_end <= next_start {
            gaps.push((prev_end, next_start));
        }
    }
    gaps
}

// ============================================================================
// SemanticConflict Detection
// ============================================================================

fn strip_sig_overload_suffix(fqn: &str) -> &str {
    let fqn = fqn.strip_suffix("#added").unwrap_or(fqn);
    if let Some((prefix, suffix)) = fqn.rsplit_once("#sig:")
        && !suffix.is_empty()
        && suffix.bytes().all(|b| b.is_ascii_digit())
    {
        prefix
    } else {
        fqn
    }
}

fn strip_leaf_member_modifiers(fqn: &str) -> std::borrow::Cow<'_, str> {
    let (scope_opt, leaf) = match fqn.rsplit_once("::") {
        Some((p, l)) => (Some(p), l),
        None => (None, fqn),
    };
    let mut cur = leaf;
    if let Some(rest) = cur.strip_prefix("static:") {
        cur = rest;
    }
    if let Some(rest) = cur
        .strip_prefix("get:")
        .or_else(|| cur.strip_prefix("set:"))
        .or_else(|| cur.strip_prefix("del:"))
    {
        cur = rest;
    }
    if cur.len() == leaf.len() {
        std::borrow::Cow::Borrowed(fqn)
    } else if let Some(scope) = scope_opt {
        std::borrow::Cow::Owned(format!("{scope}::{cur}"))
    } else {
        std::borrow::Cow::Borrowed(cur)
    }
}

fn symbols_match(consumer_ref: &str, producer_fqn: &str) -> bool {
    let prod_no_sig = strip_sig_overload_suffix(producer_fqn);
    let prod_base = strip_leaf_member_modifiers(prod_no_sig);
    let prod = prod_base.as_ref();
    if consumer_ref == producer_fqn || consumer_ref == prod_no_sig || consumer_ref == prod {
        return true;
    }
    if prod.ends_with(&format!("::{consumer_ref}"))
        || prod.ends_with(&format!(".{consumer_ref}"))
    {
        return true;
    }
    if consumer_ref.ends_with(&format!("::{prod}"))
        || consumer_ref.ends_with(&format!(".{prod}"))
    {
        return true;
    }
    if !consumer_ref.contains("impl<") && prod.contains("impl<") {
        let c_norm = super::anchor::strip_rust_generic_args(consumer_ref);
        for cand in super::anchor::expand_rust_impl_fqn_candidates(prod) {
            for cref in [consumer_ref, c_norm.as_str()] {
                if cand == cref
                    || cand.ends_with(&format!("::{cref}"))
                    || cand.ends_with(&format!(".{cref}"))
                    || cref.ends_with(&format!("::{cand}"))
                    || cref.ends_with(&format!(".{cand}"))
                {
                    return true;
                }
            }
        }
    }
    false
}

fn breaking_delta_info(
    delta: &SymbolContractDelta,
    cross_file: bool,
) -> Option<(&str, String)> {
    match delta {
        SymbolContractDelta::RemovedSymbol { symbol_fqn, .. } => {
            Some((symbol_fqn, format!("symbol '{symbol_fqn}' was removed")))
        }
        SymbolContractDelta::SignatureChanged {
            symbol_fqn,
            old_signature,
            new_signature,
            ..
        } => Some((
            symbol_fqn,
            format!("signature of '{symbol_fqn}' changed from '{old_signature}' to '{new_signature}'"),
        )),
        SymbolContractDelta::VisibilityReduced { symbol_fqn, detail } => {
            if cross_file {
                Some((
                    symbol_fqn,
                    format!("visibility of '{symbol_fqn}' reduced ({detail})"),
                ))
            } else {
                None
            }
        }
        SymbolContractDelta::AddedSymbol { .. } => None,
    }
}

fn reconstruct_single_patch_symbols(
    base_files: &BTreeMap<PathBuf, Vec<u8>>,
    patch: &AstPatch,
    fid_to_base_path: &HashMap<Blake3Hash, PathBuf>,
) -> Result<(ExtractedSymbols, Vec<Range<usize>>), CstError> {
    let base_path = fid_to_base_path
        .get(&patch.logical_file_id)
        .cloned()
        .unwrap_or_else(|| patch.file_path.clone());
    let dest_path = patch
        .ops
        .iter()
        .find_map(|op| match op {
            SemanticOp::RenameOrMoveFile { new_path, .. } => Some(new_path.clone()),
            _ => None,
        })
        .unwrap_or_else(|| base_path.clone());

    let lang = LanguageKind::from_path(&dest_path);
    if !lang.is_code() {
        return Ok((ExtractedSymbols::default(), Vec::new()));
    }

    let base_bytes = base_files
        .get(&base_path)
        .map(|v| v.as_slice())
        .unwrap_or(&[]);
    let Ok(base_str) = std::str::from_utf8(base_bytes) else {
        return Ok((ExtractedSymbols::default(), Vec::new()));
    };

    let TrackedFileApply {
        merged_src: post_src,
        touched_spans,
    } = match apply_cst_ops_for_file_tracked(
        lang,
        patch.logical_file_id,
        &dest_path,
        base_str,
        &[patch],
    ) {
        Ok(Ok(res)) => res,
        _ => return Ok((ExtractedSymbols::default(), Vec::new())),
    };

    let parsed = parse_source(lang, &post_src, None)?;
    let syms = extract_file_symbols(&parsed, patch.logical_file_id, &dest_path, &patch.viewport_id, 0);

    Ok((syms, touched_spans))
}

fn ranges_overlap(a: &Range<usize>, b: &Range<usize>) -> bool {
    a.start < b.end && b.start < a.end
}

fn get_base_file_symbols<'a>(
    base_files: &BTreeMap<PathBuf, Vec<u8>>,
    base_path: &Path,
    base_syms_cache: &'a mut HashMap<PathBuf, ExtractedSymbols>,
) -> Option<&'a ExtractedSymbols> {
    if !base_syms_cache.contains_key(base_path) {
        let syms = if let Some(bytes) = base_files.get(base_path)
            && let Ok(src) = std::str::from_utf8(bytes)
        {
            let lang = LanguageKind::from_path(base_path);
            if lang.is_code()
                && let Ok(parsed) = parse_source(lang, src, None)
            {
                let fid = compute_logical_file_id(base_path);
                extract_file_symbols(&parsed, fid, base_path, "", 0)
            } else {
                ExtractedSymbols::default()
            }
        } else {
            ExtractedSymbols::default()
        };
        base_syms_cache.insert(base_path.to_path_buf(), syms);
    }
    base_syms_cache.get(base_path)
}

#[allow(clippy::too_many_arguments)]
fn resolve_effective_breaking_delta<'a>(
    producer: &AstPatch,
    delta: &'a SymbolContractDelta,
    cross_file: bool,
    consumer_logical_file_id: Option<Blake3Hash>,
    batch_patches: &[&AstPatch],
    cache: &'a HashMap<Blake3Hash, (ExtractedSymbols, Vec<Range<usize>>)>,
    base_files: &BTreeMap<PathBuf, Vec<u8>>,
    fid_to_base_path: &HashMap<Blake3Hash, PathBuf>,
    base_syms_cache: &mut HashMap<PathBuf, ExtractedSymbols>,
) -> Option<(&'a str, String, Option<&'a super::symbols::ProvidedSymbol>)> {
    if let SymbolContractDelta::RemovedSymbol {
        symbol_fqn,
        previous_hash,
    } = delta
    {
        for other in batch_patches {
            if other.patch_hash == producer.patch_hash
                || other.logical_file_id == producer.logical_file_id
            {
                continue;
            }
            let Some((other_syms, _)) = cache.get(&other.patch_hash) else {
                continue;
            };
            let Some(new_prov) = other_syms
                .provides
                .iter()
                .find(|p| p.anchor.symbol_fqn == *symbol_fqn)
            else {
                continue;
            };

            let added_in_delta = other.contract_deltas.iter().any(|d| {
                matches!(
                    d,
                    SymbolContractDelta::AddedSymbol { symbol_fqn: s, .. } if s == symbol_fqn
                )
            });
            let newly_introduced = if added_in_delta {
                true
            } else {
                let other_base_path = fid_to_base_path
                    .get(&other.logical_file_id)
                    .cloned()
                    .unwrap_or_else(|| other.file_path.clone());
                get_base_file_symbols(base_files, &other_base_path, base_syms_cache)
                    .is_none_or(|bs| !bs.provides.iter().any(|p| p.anchor.symbol_fqn == *symbol_fqn))
            };
            if !newly_introduced {
                continue;
            }

            let producer_base_path = fid_to_base_path
                .get(&producer.logical_file_id)
                .cloned()
                .unwrap_or_else(|| producer.file_path.clone());
            let old_prov_opt =
                get_base_file_symbols(base_files, &producer_base_path, base_syms_cache).and_then(
                    |bs| {
                        bs.provides
                            .iter()
                            .find(|p| p.anchor.symbol_fqn == *symbol_fqn)
                            .cloned()
                    },
                );

            let moved_cross_file =
                consumer_logical_file_id.is_none_or(|cfid| cfid != other.logical_file_id);
            if moved_cross_file
                && let Some(ref old_prov) = old_prov_opt
                && new_prov.visibility < old_prov.visibility
            {
                return Some((
                    symbol_fqn.as_str(),
                    format!(
                        "visibility of '{symbol_fqn}' reduced ({:?} -> {:?})",
                        old_prov.visibility, new_prov.visibility
                    ),
                    None,
                ));
            }

            if new_prov.anchor.decl_header_hash != *previous_hash {
                let old_sig = old_prov_opt
                    .as_ref()
                    .map_or("<unknown>", |op| op.normalized_signature.as_str());
                return Some((
                    symbol_fqn.as_str(),
                    format!(
                        "signature of '{symbol_fqn}' changed from '{old_sig}' to '{}'",
                        new_prov.normalized_signature
                    ),
                    Some(new_prov),
                ));
            }

            return None;
        }
    }

    breaking_delta_info(delta, cross_file).map(|(sym, reason)| (sym, reason, None))
}

fn detect_semantic_conflicts(
    base_files: &BTreeMap<PathBuf, Vec<u8>>,
    ordered: &[AstPatch],
    fid_to_base_path: &HashMap<Blake3Hash, PathBuf>,
) -> Result<Vec<MergeOutcome>, CstError> {
    let mut consumer_cache: HashMap<Blake3Hash, (ExtractedSymbols, Vec<Range<usize>>)> =
        HashMap::new();
    for p in ordered {
        let res = reconstruct_single_patch_symbols(base_files, p, fid_to_base_path)?;
        consumer_cache.insert(p.patch_hash, res);
    }
    let mut base_syms_cache: HashMap<PathBuf, ExtractedSymbols> = HashMap::new();
    let batch_refs: Vec<&AstPatch> = ordered.iter().collect();

    let mut raw_conflicts: Vec<MergeOutcome> = Vec::new();

    for producer in ordered {
        if producer.contract_deltas.is_empty() {
            continue;
        }
        let producer_provides = consumer_cache
            .get(&producer.patch_hash)
            .map(|(s, _)| &s.provides);

        for consumer in ordered {
            if consumer.patch_hash == producer.patch_hash {
                continue;
            }
            let cross_file = consumer.logical_file_id != producer.logical_file_id;
            let Some((consumer_syms, touched_spans)) = consumer_cache.get(&consumer.patch_hash)
            else {
                continue;
            };

            for delta in &producer.contract_deltas {
                let Some((sym_fqn, reason, moved_prov)) = resolve_effective_breaking_delta(
                    producer,
                    delta,
                    cross_file,
                    Some(consumer.logical_file_id),
                    &batch_refs,
                    &consumer_cache,
                    base_files,
                    fid_to_base_path,
                    &mut base_syms_cache,
                ) else {
                    continue;
                };

                // If consumer itself also removed or modified the exact same symbol, that's structural/convergent.
                let consumer_also_touched = consumer.contract_deltas.iter().any(|cd| match cd {
                    SymbolContractDelta::RemovedSymbol { symbol_fqn: csym, .. }
                    | SymbolContractDelta::SignatureChanged { symbol_fqn: csym, .. }
                    | SymbolContractDelta::VisibilityReduced { symbol_fqn: csym, .. } => {
                        csym == sym_fqn
                    }
                    _ => false,
                });
                if consumer_also_touched {
                    continue;
                }

                for c in &consumer_syms.consumes {
                    if c.in_error_subtree {
                        continue;
                    }
                    if !cross_file
                        && !touched_spans.iter().any(|sp| ranges_overlap(sp, &c.byte_range))
                    {
                        continue;
                    }
                    if !symbols_match(&c.symbol_ref, sym_fqn) {
                        continue;
                    }

                    // Extra precision for SignatureChanged (including across file rename) with call arity:
                    let arity_note = if let Some(call_arity) = c.call_arity {
                        let prov_opt = moved_prov.or_else(|| {
                            if matches!(delta, SymbolContractDelta::SignatureChanged { .. }) {
                                producer_provides.and_then(|provs| {
                                    provs.iter().find(|p| p.anchor.symbol_fqn == sym_fqn)
                                })
                            } else {
                                None
                            }
                        });
                        if let Some(prov) = prov_opt
                            && let Some((min_a, max_a)) = prov.arity
                        {
                            if call_arity < min_a || call_arity > max_a {
                                format!(" (call arity {call_arity} incompatible with {min_a}..={max_a})")
                            } else {
                                " (signature changed)".to_string()
                            }
                        } else {
                            String::new()
                        }
                    } else {
                        String::new()
                    };

                    raw_conflicts.push(MergeOutcome::SemanticConflict {
                        symbol_fqn: sym_fqn.to_string(),
                        producer_patch: producer.patch_hash,
                        consumer_patch: consumer.patch_hash,
                        detail: format!(
                            "{reason}{arity_note}; consumed at '{}' line {}",
                            c.location.file_path.display(),
                            c.location.line
                        ),
                    });
                    break;
                }
            }
        }
    }

    raw_conflicts.sort_by(|a, b| match (a, b) {
        (
            MergeOutcome::SemanticConflict {
                symbol_fqn: sa,
                producer_patch: pa1,
                consumer_patch: ca1,
                ..
            },
            MergeOutcome::SemanticConflict {
                symbol_fqn: sb,
                producer_patch: pa2,
                consumer_patch: ca2,
                ..
            },
        ) => (sa, pa1, ca1).cmp(&(sb, pa2, ca2)),
        _ => std::cmp::Ordering::Equal,
    });
    raw_conflicts.dedup();
    Ok(raw_conflicts)
}

/// Detects cross-viewport structural and semantic conflicts when two viewports
/// have diverged bases such that neither single base contains all target anchors
/// of both viewports.
pub fn detect_divergent_base_conflicts(
    base_a: &BTreeMap<PathBuf, Vec<u8>>,
    patches_a: &[AstPatch],
    base_b: &BTreeMap<PathBuf, Vec<u8>>,
    patches_b: &[AstPatch],
) -> Result<Vec<MergeOutcome>, CstError> {
    let mut conflicts: Vec<MergeOutcome> = Vec::new();
    let ordered_a = sort_and_dedup_patches(patches_a);
    let ordered_b = sort_and_dedup_patches(patches_b);

    let mut fid_to_path_a: HashMap<Blake3Hash, PathBuf> = base_a
        .keys()
        .map(|p| (compute_logical_file_id(p), p.clone()))
        .collect();
    for p in &ordered_a {
        fid_to_path_a.entry(p.logical_file_id).or_insert_with(|| {
            for op in &p.ops {
                if let SemanticOp::RenameOrMoveFile { old_path, .. } = op
                    && base_a.contains_key(old_path)
                {
                    return old_path.clone();
                }
            }
            p.file_path.clone()
        });
    }
    let mut fid_to_path_b: HashMap<Blake3Hash, PathBuf> = base_b
        .keys()
        .map(|p| (compute_logical_file_id(p), p.clone()))
        .collect();
    for p in &ordered_b {
        fid_to_path_b.entry(p.logical_file_id).or_insert_with(|| {
            for op in &p.ops {
                if let SemanticOp::RenameOrMoveFile { old_path, .. } = op
                    && base_b.contains_key(old_path)
                {
                    return old_path.clone();
                }
            }
            p.file_path.clone()
        });
    }

    // Cross-viewport RenameOrMoveFile collision detection
    let collect_side_renames =
        |ordered: &[AstPatch],
         fid_map: &HashMap<Blake3Hash, PathBuf>,
         side_conflicts: &mut Vec<MergeOutcome>| {
            let mut by_fid: BTreeMap<Blake3Hash, Vec<&AstPatch>> = BTreeMap::new();
            for p in ordered {
                let canonical_fid = p
                    .ops
                    .iter()
                    .find_map(|op| match op {
                        SemanticOp::RenameOrMoveFile { old_path, .. } => {
                            Some(compute_logical_file_id(old_path))
                        }
                        _ => None,
                    })
                    .unwrap_or(p.logical_file_id);
                by_fid.entry(canonical_fid).or_default().push(p);
            }
            let mut moved_from: HashSet<PathBuf> = HashSet::new();
            let mut state: BTreeMap<Blake3Hash, (PathBuf, PathBuf, Blake3Hash, bool)> =
                BTreeMap::new();
            for (fid, group) in by_fid {
                let rename_old_path = group.iter().find_map(|p| {
                    p.ops.iter().find_map(|op| match op {
                        SemanticOp::RenameOrMoveFile { old_path, .. } => Some(old_path.clone()),
                        _ => None,
                    })
                });
                let base_path = rename_old_path
                    .or_else(|| fid_map.get(&fid).cloned())
                    .unwrap_or_else(|| group[0].file_path.clone());
                let mut renames: Vec<(PathBuf, Blake3Hash)> = Vec::new();
                for p in &group {
                    for op in &p.ops {
                        if let SemanticOp::RenameOrMoveFile { new_path, .. } = op {
                            renames.push((new_path.clone(), p.patch_hash));
                        }
                    }
                }
                let has_rename = !renames.is_empty();
                let mut final_path = base_path.clone();
                if let Some((first_p, first_h)) = renames.first() {
                    final_path = first_p.clone();
                    for (other_p, other_h) in renames.iter().skip(1) {
                        if other_p != first_p {
                            side_conflicts.push(MergeOutcome::StructuralConflict {
                                file: base_path.clone(),
                                anchor: None,
                                patch_a: *first_h,
                                patch_b: *other_h,
                                reason: format!(
                                    "conflicting RenameOrMoveFile: '{}' vs '{}'",
                                    first_p.display(),
                                    other_p.display()
                                ),
                            });
                        }
                    }
                }
                if final_path != base_path {
                    moved_from.insert(base_path.clone());
                }
                let cur_h = renames.first().map(|x| x.1).unwrap_or(group[0].patch_hash);
                state.insert(fid, (base_path, final_path, cur_h, has_rename));
            }
            (state, moved_from)
        };

    let (ren_state_a, moved_from_a) =
        collect_side_renames(&ordered_a, &fid_to_path_a, &mut conflicts);
    let (ren_state_b, moved_from_b) =
        collect_side_renames(&ordered_b, &fid_to_path_b, &mut conflicts);
    let mut moved_from_all: HashSet<PathBuf> = moved_from_a;
    moved_from_all.extend(moved_from_b);

    for (fid, (base_pa, final_pa, ha, has_ren_a)) in &ren_state_a {
        if let Some((_, final_pb, hb, has_ren_b)) = ren_state_b.get(fid)
            && *has_ren_a
            && *has_ren_b
            && final_pa != final_pb
        {
            conflicts.push(MergeOutcome::StructuralConflict {
                file: base_pa.clone(),
                anchor: None,
                patch_a: *ha,
                patch_b: *hb,
                reason: format!(
                    "conflicting RenameOrMoveFile: '{}' vs '{}'",
                    final_pa.display(),
                    final_pb.display()
                ),
            });
        }
    }

    let mut targets_a: BTreeMap<PathBuf, (Blake3Hash, Blake3Hash)> = BTreeMap::new();
    for (&fid, (_, final_p, h, _)) in &ren_state_a {
        if let Some((prev_fid, prev_h)) = targets_a.get(final_p)
            && *prev_fid != fid
        {
            conflicts.push(MergeOutcome::StructuralConflict {
                file: final_p.clone(),
                anchor: None,
                patch_a: *prev_h,
                patch_b: *h,
                reason: format!(
                    "conflicting RenameOrMoveFile: multiple logical files moved/created at '{}'",
                    final_p.display()
                ),
            });
        } else {
            targets_a.insert(final_p.clone(), (fid, *h));
        }
    }
    let mut targets_b: BTreeMap<PathBuf, (Blake3Hash, Blake3Hash)> = BTreeMap::new();
    for (&fid, (_, final_p, h, _)) in &ren_state_b {
        if let Some((prev_fid, prev_h)) = targets_b.get(final_p)
            && *prev_fid != fid
        {
            conflicts.push(MergeOutcome::StructuralConflict {
                file: final_p.clone(),
                anchor: None,
                patch_a: *prev_h,
                patch_b: *h,
                reason: format!(
                    "conflicting RenameOrMoveFile: multiple logical files moved/created at '{}'",
                    final_p.display()
                ),
            });
        } else {
            targets_b.insert(final_p.clone(), (fid, *h));
        }
    }
    for (dest, (fid_a, ha)) in &targets_a {
        if let Some((fid_b, hb)) = targets_b.get(dest)
            && fid_a != fid_b
        {
            conflicts.push(MergeOutcome::StructuralConflict {
                file: dest.clone(),
                anchor: None,
                patch_a: *ha,
                patch_b: *hb,
                reason: format!(
                    "conflicting RenameOrMoveFile: multiple logical files moved/created at '{}'",
                    dest.display()
                ),
            });
        }
    }
    for (base_p, final_p, h, has_ren) in ren_state_a.values().chain(ren_state_b.values()) {
        if *has_ren
            && final_p != base_p
            && (base_a.contains_key(final_p) || base_b.contains_key(final_p))
            && !moved_from_all.contains(final_p)
        {
            conflicts.push(MergeOutcome::StructuralConflict {
                file: final_p.clone(),
                anchor: None,
                patch_a: *h,
                patch_b: *h,
                reason: format!(
                    "RenameOrMoveFile target '{}' already exists in base tree",
                    final_p.display()
                ),
            });
        }
    }

    let patch_dest_path = |p: &AstPatch, fid_map: &HashMap<Blake3Hash, PathBuf>| -> PathBuf {
        p.ops
            .iter()
            .find_map(|op| match op {
                SemanticOp::RenameOrMoveFile { new_path, .. } => Some(new_path.clone()),
                _ => None,
            })
            .or_else(|| fid_map.get(&p.logical_file_id).cloned())
            .unwrap_or_else(|| p.file_path.clone())
    };

    fn build_crdt_inputs<'a>(patches: &[&'a AstPatch]) -> Vec<crdt::CrdtPatchInput<'a>> {
        patches
            .iter()
            .map(|p| {
                let blobs: Vec<&[u8]> = p
                    .ops
                    .iter()
                    .filter_map(|op| match op {
                        SemanticOp::TextCrdtFallback {
                            automerge_changes, ..
                        } => Some(automerge_changes.as_slice()),
                        _ => None,
                    })
                    .collect();
                crdt::CrdtPatchInput {
                    patch_hash: p.patch_hash,
                    actor_id: &p.actor_id,
                    lamport_ts: p.lamport_ts,
                    parents: &p.parents,
                    blobs,
                }
            })
            .collect()
    }

    let mut crdt_pairs_checked: HashSet<(PathBuf, Blake3Hash)> = HashSet::new();
    let mut ancestor_map_cache: HashMap<(u8, PathBuf, Blake3Hash), HashMap<NodeKey, HashSet<NodeKey>>> =
        HashMap::new();
    for pa in &ordered_a {
        let dest_a = patch_dest_path(pa, &fid_to_path_a);
        let a_is_bin = pa.binary_manifest.is_some();
        let a_is_crdt = pa
            .ops
            .iter()
            .any(|op| matches!(op, SemanticOp::TextCrdtFallback { .. }));
        let a_is_cst = pa.ops.iter().any(|op| {
            matches!(
                op,
                SemanticOp::InsertMember { .. }
                    | SemanticOp::ReplaceBody { .. }
                    | SemanticOp::MutateSignature { .. }
                    | SemanticOp::RemoveNode { .. }
            )
        });
        for pb in &ordered_b {
            if pa.patch_hash == pb.patch_hash {
                continue;
            }
            let dest_b = patch_dest_path(pb, &fid_to_path_b);
            if pa.logical_file_id != pb.logical_file_id && dest_a != dest_b {
                continue;
            }
            let b_is_bin = pb.binary_manifest.is_some();
            let b_is_crdt = pb
                .ops
                .iter()
                .any(|op| matches!(op, SemanticOp::TextCrdtFallback { .. }));
            let b_is_cst = pb.ops.iter().any(|op| {
                matches!(
                    op,
                    SemanticOp::InsertMember { .. }
                        | SemanticOp::ReplaceBody { .. }
                        | SemanticOp::MutateSignature { .. }
                        | SemanticOp::RemoveNode { .. }
                )
            });

            if a_is_bin && b_is_bin {
                let ma = pa.binary_manifest.as_ref().unwrap();
                let mb = pb.binary_manifest.as_ref().unwrap();
                if ma.file_hash != mb.file_hash {
                    conflicts.push(MergeOutcome::StructuralConflict {
                        file: dest_a.clone(),
                        anchor: None,
                        patch_a: pa.patch_hash,
                        patch_b: pb.patch_hash,
                        reason: format!(
                            "divergent concurrent binary edits ({} vs {})",
                            ma.file_hash.short_hex(12),
                            mb.file_hash.short_hex(12)
                        ),
                    });
                }
                continue;
            }
            if (a_is_bin && (b_is_crdt || b_is_cst)) || (b_is_bin && (a_is_crdt || a_is_cst)) {
                conflicts.push(MergeOutcome::StructuralConflict {
                    file: dest_a.clone(),
                    anchor: None,
                    patch_a: pa.patch_hash,
                    patch_b: pb.patch_hash,
                    reason:
                        "cannot merge binary manifest with text/CST semantic ops on the same file"
                            .into(),
                });
                continue;
            }
            if (a_is_crdt && b_is_cst) || (b_is_crdt && a_is_cst) {
                conflicts.push(MergeOutcome::StructuralConflict {
                    file: dest_a.clone(),
                    anchor: None,
                    patch_a: pa.patch_hash,
                    patch_b: pb.patch_hash,
                    reason: "cannot merge TextCrdtFallback with CST semantic ops on the same file"
                        .into(),
                });
                continue;
            }
            if a_is_crdt && b_is_crdt && crdt_pairs_checked.insert((dest_a.clone(), pa.logical_file_id)) {
                let mut crdt_group_a: Vec<&AstPatch> = ordered_a
                    .iter()
                    .filter(|p| {
                        p.ops
                            .iter()
                            .any(|op| matches!(op, SemanticOp::TextCrdtFallback { .. }))
                            && (p.logical_file_id == pa.logical_file_id
                                || patch_dest_path(p, &fid_to_path_a) == dest_a)
                    })
                    .collect();
                crdt_group_a.sort_by_key(|p| (p.lamport_ts, p.patch_hash));
                crdt_group_a.dedup_by_key(|p| p.patch_hash);

                let mut crdt_group_b: Vec<&AstPatch> = ordered_b
                    .iter()
                    .filter(|p| {
                        p.ops
                            .iter()
                            .any(|op| matches!(op, SemanticOp::TextCrdtFallback { .. }))
                            && (p.logical_file_id == pb.logical_file_id
                                || patch_dest_path(p, &fid_to_path_b) == dest_a)
                    })
                    .collect();
                crdt_group_b.sort_by_key(|p| (p.lamport_ts, p.patch_hash));
                crdt_group_b.dedup_by_key(|p| p.patch_hash);

                let mut crdt_group: Vec<&AstPatch> = crdt_group_a
                    .iter()
                    .copied()
                    .chain(crdt_group_b.iter().copied())
                    .collect();
                crdt_group.sort_by_key(|p| (p.lamport_ts, p.patch_hash));
                crdt_group.dedup_by_key(|p| p.patch_hash);

                let inputs = build_crdt_inputs(&crdt_group);

                let path_a = fid_to_path_a
                    .get(&pa.logical_file_id)
                    .unwrap_or(&pa.file_path);
                let path_b = fid_to_path_b
                    .get(&pb.logical_file_id)
                    .unwrap_or(&pb.file_path);
                let base_bytes_a = base_a.get(path_a).map(|v| v.as_slice()).unwrap_or(&[]);
                let base_bytes_b = base_b.get(path_b).map(|v| v.as_slice()).unwrap_or(&[]);
                let base_str_a = std::str::from_utf8(base_bytes_a)
                    .map_err(|_| CstError::InvalidUtf8(dest_a.display().to_string()))?;
                let base_str_b = std::str::from_utf8(base_bytes_b)
                    .map_err(|_| CstError::InvalidUtf8(dest_b.display().to_string()))?;
                let sample_bytes = if !base_bytes_a.is_empty() {
                    base_bytes_a
                } else {
                    base_bytes_b
                };
                let lang = LanguageKind::detect(&dest_a, sample_bytes);
                match crdt::merge_crdt_patch_dag(base_str_a, &inputs) {
                    Ok(merged_text) => {
                        let (all_empty, delete_vs_modify) =
                            resolve_crdt_branch_tips(base_str_a, &inputs);
                        if merged_text.is_empty() && !base_str_a.is_empty() && all_empty {
                            // Convergent deletion
                        } else if delete_vs_modify {
                            conflicts.push(MergeOutcome::StructuralConflict {
                                file: dest_a.clone(),
                                anchor: None,
                                patch_a: pa.patch_hash,
                                patch_b: pb.patch_hash,
                                reason: format!(
                                    "delete-vs-modify conflict on '{}'",
                                    dest_a.display()
                                ),
                            });
                        } else if let Err(conflict) = crdt::validate_merged_non_code(
                            lang,
                            &dest_a,
                            merged_text,
                            pa.patch_hash,
                            pb.patch_hash,
                        )? {
                            conflicts.push(conflict);
                        }
                    }
                    Err(crdt::CrdtError::MissingDependencies(_))
                        if base_bytes_b != base_bytes_a =>
                    {
                        let inputs_a = build_crdt_inputs(&crdt_group_a);
                        let inputs_b = build_crdt_inputs(&crdt_group_b);
                        let post_a = match crdt::merge_crdt_patch_dag(base_str_a, &inputs_a) {
                            Ok(s) => s,
                            Err(err) => {
                                conflicts.push(MergeOutcome::StructuralConflict {
                                    file: dest_a.clone(),
                                    anchor: None,
                                    patch_a: pa.patch_hash,
                                    patch_b: pb.patch_hash,
                                    reason: format!(
                                        "divergent CRDT merge failed on '{}': {err}",
                                        dest_a.display()
                                    ),
                                });
                                continue;
                            }
                        };
                        let post_b = match crdt::merge_crdt_patch_dag(base_str_b, &inputs_b) {
                            Ok(s) => s,
                            Err(err) => {
                                conflicts.push(MergeOutcome::StructuralConflict {
                                    file: dest_a.clone(),
                                    anchor: None,
                                    patch_a: pa.patch_hash,
                                    patch_b: pb.patch_hash,
                                    reason: format!(
                                        "divergent CRDT merge failed on '{}': {err}",
                                        dest_a.display()
                                    ),
                                });
                                continue;
                            }
                        };
                        if post_a.is_empty() && post_b.is_empty() {
                            // Convergent deletion across divergent bases
                        } else if (post_a.is_empty() && !base_str_a.is_empty() && !post_b.is_empty())
                            || (post_b.is_empty()
                                && !base_str_b.is_empty()
                                && !post_a.is_empty())
                        {
                            conflicts.push(MergeOutcome::StructuralConflict {
                                file: dest_a.clone(),
                                anchor: None,
                                patch_a: pa.patch_hash,
                                patch_b: pb.patch_hash,
                                reason: format!(
                                    "delete-vs-modify conflict on '{}'",
                                    dest_a.display()
                                ),
                            });
                        } else if post_a == post_b {
                            if let Err(conflict) = crdt::validate_merged_non_code(
                                lang,
                                &dest_a,
                                post_a,
                                pa.patch_hash,
                                pb.patch_hash,
                            )? {
                                conflicts.push(conflict);
                            }
                        } else {
                            let common_base = if base_str_a.is_empty() {
                                base_str_b
                            } else {
                                base_str_a
                            };
                            let c_a = crdt::diff_text_to_crdt_changes(
                                common_base,
                                &post_a,
                                &pa.actor_id,
                                pa.lamport_ts,
                            )?;
                            let c_b = crdt::diff_text_to_crdt_changes(
                                common_base,
                                &post_b,
                                &pb.actor_id,
                                pb.lamport_ts,
                            )?;
                            match crdt::merge_non_code_file(
                                lang,
                                &dest_a,
                                common_base,
                                &[&c_a, &c_b],
                                pa.patch_hash,
                                pb.patch_hash,
                            ) {
                                Ok(Ok(_)) => {}
                                Ok(Err(conflict)) => conflicts.push(conflict),
                                Err(err) => conflicts.push(MergeOutcome::StructuralConflict {
                                    file: dest_a.clone(),
                                    anchor: None,
                                    patch_a: pa.patch_hash,
                                    patch_b: pb.patch_hash,
                                    reason: format!(
                                        "divergent CRDT merge failed on '{}': {err}",
                                        dest_a.display()
                                    ),
                                }),
                            }
                        }
                    }
                    Err(err) => conflicts.push(MergeOutcome::StructuralConflict {
                        file: dest_a.clone(),
                        anchor: None,
                        patch_a: pa.patch_hash,
                        patch_b: pb.patch_hash,
                        reason: format!(
                            "divergent CRDT merge failed on '{}': {err}",
                            dest_a.display()
                        ),
                    }),
                }
                continue;
            }
            let lang = LanguageKind::from_path(&dest_a);
            let path_a = fid_to_path_a
                .get(&pa.logical_file_id)
                .unwrap_or(&pa.file_path);
            let path_b = fid_to_path_b
                .get(&pb.logical_file_id)
                .unwrap_or(&pb.file_path);
            let anc_key_a = (0u8, path_a.clone(), pa.logical_file_id);
            if !ancestor_map_cache.contains_key(&anc_key_a) {
                let base_str_a = base_a
                    .get(path_a)
                    .and_then(|b| std::str::from_utf8(b).ok())
                    .unwrap_or("");
                ancestor_map_cache.insert(
                    anc_key_a.clone(),
                    build_base_ancestor_map(lang, pa.logical_file_id, base_str_a),
                );
            }
            let anc_key_b = (1u8, path_b.clone(), pb.logical_file_id);
            if !ancestor_map_cache.contains_key(&anc_key_b) {
                let base_str_b = base_b
                    .get(path_b)
                    .and_then(|b| std::str::from_utf8(b).ok())
                    .unwrap_or("");
                ancestor_map_cache.insert(
                    anc_key_b.clone(),
                    build_base_ancestor_map(lang, pb.logical_file_id, base_str_b),
                );
            }
            let anc_a = &ancestor_map_cache[&anc_key_a];
            let anc_b = &ancestor_map_cache[&anc_key_b];
            for op_a in &pa.ops {
                for op_b in &pb.ops {
                    if let Some(c) = check_op_pair_conflict(
                        lang,
                        &dest_a,
                        pa.patch_hash,
                        op_a,
                        pb.patch_hash,
                        op_b,
                        &[anc_a, anc_b],
                    ) {
                        conflicts.push(c);
                    }
                }
            }
        }
    }

    let mut cache_a: HashMap<Blake3Hash, (ExtractedSymbols, Vec<Range<usize>>)> = HashMap::new();
    for p in &ordered_a {
        cache_a.insert(
            p.patch_hash,
            reconstruct_single_patch_symbols(base_a, p, &fid_to_path_a)?,
        );
    }
    let mut cache_b: HashMap<Blake3Hash, (ExtractedSymbols, Vec<Range<usize>>)> = HashMap::new();
    for p in &ordered_b {
        cache_b.insert(
            p.patch_hash,
            reconstruct_single_patch_symbols(base_b, p, &fid_to_path_b)?,
        );
    }

    let mut base_syms_cache_a: HashMap<PathBuf, ExtractedSymbols> = HashMap::new();
    let mut base_syms_cache_b: HashMap<PathBuf, ExtractedSymbols> = HashMap::new();
    let refs_a: Vec<&AstPatch> = ordered_a.iter().collect();
    let refs_b: Vec<&AstPatch> = ordered_b.iter().collect();

    let mut raw_sem: Vec<MergeOutcome> = Vec::new();
    for (producers, p_cache, p_base, p_fid_map, p_refs, p_base_cache, consumers, c_cache) in [
        (
            &ordered_a,
            &cache_a,
            base_a,
            &fid_to_path_a,
            &refs_a,
            &mut base_syms_cache_a,
            &ordered_b,
            &cache_b,
        ),
        (
            &ordered_b,
            &cache_b,
            base_b,
            &fid_to_path_b,
            &refs_b,
            &mut base_syms_cache_b,
            &ordered_a,
            &cache_a,
        ),
    ] {
        for producer in producers {
            if producer.contract_deltas.is_empty() {
                continue;
            }
            let producer_provides = p_cache.get(&producer.patch_hash).map(|(s, _)| &s.provides);
            for consumer in consumers {
                if consumer.patch_hash == producer.patch_hash {
                    continue;
                }
                let cross_file = consumer.logical_file_id != producer.logical_file_id;
                let Some((consumer_syms, touched_spans)) = c_cache.get(&consumer.patch_hash)
                else {
                    continue;
                };
                for delta in &producer.contract_deltas {
                    let Some((sym_fqn, reason, moved_prov)) = resolve_effective_breaking_delta(
                        producer,
                        delta,
                        cross_file,
                        Some(consumer.logical_file_id),
                        p_refs,
                        p_cache,
                        p_base,
                        p_fid_map,
                        p_base_cache,
                    ) else {
                        continue;
                    };
                    let consumer_also_touched = consumer.contract_deltas.iter().any(|cd| match cd {
                        SymbolContractDelta::RemovedSymbol { symbol_fqn: csym, .. }
                        | SymbolContractDelta::SignatureChanged { symbol_fqn: csym, .. }
                        | SymbolContractDelta::VisibilityReduced { symbol_fqn: csym, .. } => {
                            csym == sym_fqn
                        }
                        _ => false,
                    });
                    if consumer_also_touched {
                        continue;
                    }
                    for c in &consumer_syms.consumes {
                        if c.in_error_subtree {
                            continue;
                        }
                        if !cross_file
                            && !touched_spans.iter().any(|sp| ranges_overlap(sp, &c.byte_range))
                        {
                            continue;
                        }
                        if !symbols_match(&c.symbol_ref, sym_fqn) {
                            continue;
                        }
                        let arity_note = if let Some(call_arity) = c.call_arity {
                            let prov_opt = moved_prov.or_else(|| {
                                if matches!(delta, SymbolContractDelta::SignatureChanged { .. }) {
                                    producer_provides.and_then(|provs| {
                                        provs.iter().find(|p| p.anchor.symbol_fqn == sym_fqn)
                                    })
                                } else {
                                    None
                                }
                            });
                            if let Some(prov) = prov_opt
                                && let Some((min_a, max_a)) = prov.arity
                            {
                                if call_arity < min_a || call_arity > max_a {
                                    format!(" (call arity {call_arity} incompatible with {min_a}..={max_a})")
                                } else {
                                    " (signature changed)".to_string()
                                }
                            } else {
                                String::new()
                            }
                        } else {
                            String::new()
                        };
                        raw_sem.push(MergeOutcome::SemanticConflict {
                            symbol_fqn: sym_fqn.to_string(),
                            producer_patch: producer.patch_hash,
                            consumer_patch: consumer.patch_hash,
                            detail: format!(
                                "{reason}{arity_note}; consumed at '{}' line {}",
                                c.location.file_path.display(),
                                c.location.line
                            ),
                        });
                        break;
                    }
                }
            }
        }
    }

    raw_sem.sort_by(|a, b| match (a, b) {
        (
            MergeOutcome::SemanticConflict {
                symbol_fqn: sa,
                producer_patch: pa1,
                consumer_patch: ca1,
                ..
            },
            MergeOutcome::SemanticConflict {
                symbol_fqn: sb,
                producer_patch: pa2,
                consumer_patch: ca2,
                ..
            },
        ) => (sa, pa1, ca1).cmp(&(sb, pa2, ca2)),
        _ => std::cmp::Ordering::Equal,
    });
    raw_sem.dedup();
    conflicts.extend(raw_sem);
    Ok(conflicts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cst::diff::{DiffContext, compute_logical_file_id, diff_source_to_patch};

    #[test]
    fn root_insert_member_preserves_detached_top_banner_comment() {
        let path = Path::new("src/lib.rs");
        let fid = compute_logical_file_id(path);
        let base_src = "// DETACHED_SECTION_BANNER: must survive target removal\n\n/// Attached doc comment for target_fn\n#[inline]\npub fn target_fn() -> i32 {\n    10\n}\n\npub fn keep_fn() -> i32 {\n    20\n}\n";
        let vp1_src = "// DETACHED_SECTION_BANNER: must survive target removal\n\n/// Top doc\npub fn new_top() -> i32 {\n    1\n}\n\n/// Attached doc comment for target_fn\n#[inline]\npub fn target_fn() -> i32 {\n    10\n}\n\npub fn keep_fn() -> i32 {\n    20\n}\n";
        let vp2_src = "// DETACHED_SECTION_BANNER: must survive target removal\n\npub fn keep_fn() -> i32 {\n    99\n}\n";

        let p1 = diff_source_to_patch(
            &DiffContext {
                logical_file_id: fid,
                file_path: path,
                language: LanguageKind::Rust,
                viewport_id: "vp1",
                actor_id: "a1",
                lamport_ts: 1,
                parents: Vec::new(),
            },
            base_src,
            vp1_src,
        )
        .unwrap();
        let p2 = diff_source_to_patch(
            &DiffContext {
                logical_file_id: fid,
                file_path: path,
                language: LanguageKind::Rust,
                viewport_id: "vp2",
                actor_id: "a2",
                lamport_ts: 2,
                parents: Vec::new(),
            },
            base_src,
            vp2_src,
        )
        .unwrap();

        let mut base_files = BTreeMap::new();
        base_files.insert(path.to_path_buf(), base_src.as_bytes().to_vec());
        let report = merge_patches(&base_files, &[p1, p2], None).unwrap();
        assert!(matches!(report.outcome, MergeOutcome::Clean { .. }));
        let MergedFileContent::Text(merged) = report.merged_files.get(path).unwrap() else {
            panic!("expected text");
        };
        let banner_pos = merged.find("DETACHED_SECTION_BANNER").unwrap();
        let new_top_pos = merged.find("pub fn new_top").unwrap();
        let keep_pos = merged.find("pub fn keep_fn").unwrap();
        assert!(
            banner_pos < new_top_pos && new_top_pos < keep_pos,
            "unexpected ordering:\n{merged}"
        );
        assert!(!merged.contains("target_fn"));
    }
}
