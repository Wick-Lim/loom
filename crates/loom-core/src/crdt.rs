use crate::model::{Blake3Hash, LanguageKind, MergeOutcome};
use automerge::transaction::Transactable;
use automerge::{ActorId, AutoCommit, ChangeHash, ObjId, ObjType, ReadDoc, ROOT};
use serde::de::DeserializeSeed;
use serde::Deserialize;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use thiserror::Error;

const GENESIS_ACTOR_BYTES: [u8; 16] = [0u8; 16];
const CONTENT_KEY: &str = "content";

#[derive(Debug, Error)]
pub enum CrdtError {
    #[error("automerge error: {0}")]
    Automerge(#[from] automerge::AutomergeError),
    #[error("missing root 'content' text object in automerge document")]
    MissingContentObject,
    #[error("CRDT patch has unmet causal dependencies: {0}")]
    MissingDependencies(String),
    #[error("CRDT patch causal parent graph contains a cycle")]
    CyclicDependencies,
}

pub fn genesis_actor_id() -> ActorId {
    ActorId::from(GENESIS_ACTOR_BYTES)
}

pub fn branch_actor_id(actor_label: &str, lamport_ts: u64, content_hash: Blake3Hash) -> ActorId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(actor_label.as_bytes());
    hasher.update(&lamport_ts.to_le_bytes());
    hasher.update(content_hash.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest.as_bytes()[..16]);
    // Ensure a branch actor is never identical to the genesis actor
    if bytes == GENESIS_ACTOR_BYTES {
        bytes[0] = 1;
    }
    ActorId::from(bytes)
}

pub fn init_text_doc(initial_text: &str) -> Result<AutoCommit, CrdtError> {
    let mut doc = AutoCommit::new().with_actor(genesis_actor_id());
    let text_id = doc.put_object(ROOT, CONTENT_KEY, ObjType::Text)?;
    if !initial_text.is_empty() {
        doc.update_text(&text_id, initial_text)?;
    }
    doc.commit();
    Ok(doc)
}

fn lookup_content_text_id(doc: &AutoCommit) -> Result<ObjId, CrdtError> {
    match doc.get(ROOT, CONTENT_KEY)? {
        Some((automerge::Value::Object(ObjType::Text), id)) => Ok(id),
        _ => Err(CrdtError::MissingContentObject),
    }
}

fn normalize_lf(s: &str) -> std::borrow::Cow<'_, str> {
    if s.contains("\r\n") {
        std::borrow::Cow::Owned(s.replace("\r\n", "\n"))
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}

fn is_crlf_dominant(s: &str) -> bool {
    let lf_count = s.matches('\n').count();
    let crlf_count = s.matches("\r\n").count();
    crlf_count > 0 && crlf_count == lf_count
}

fn restore_line_endings(lf_text: String, crlf_base: bool) -> String {
    if crlf_base {
        lf_text.replace('\n', "\r\n")
    } else {
        lf_text
    }
}

pub fn diff_text_to_crdt_changes(
    base_text: &str,
    modified_text: &str,
    actor_label: &str,
    lamport_ts: u64,
) -> Result<Vec<u8>, CrdtError> {
    let base_lf = normalize_lf(base_text);
    let mod_lf = normalize_lf(modified_text);
    let mut base_doc = init_text_doc(&base_lf)?;
    if base_lf == mod_lf {
        return Ok(Vec::new());
    }
    let base_heads = base_doc.get_heads();
    let content_hash = Blake3Hash::from_bytes(mod_lf.as_bytes());
    let actor = branch_actor_id(actor_label, lamport_ts, content_hash);
    let mut branch = base_doc.fork().with_actor(actor);
    let text_id = lookup_content_text_id(&branch)?;
    branch.update_text(&text_id, &mod_lf)?;
    branch.commit();
    Ok(branch.save_after(&base_heads))
}

fn try_apply_blobs_on_text(
    base_text: &str,
    blobs: &[&[u8]],
) -> Result<Result<(AutoCommit, String), Vec<ChangeHash>>, CrdtError> {
    let base_lf = normalize_lf(base_text);
    let mut doc = init_text_doc(&base_lf)?;
    let mut current_text = base_lf.into_owned();
    let non_empty_count = blobs.iter().filter(|b| !b.is_empty()).count();
    let mut applied_idx = 0usize;
    for blob in blobs {
        if blob.is_empty() {
            continue;
        }
        let mut step = init_text_doc(&current_text)?;
        step.load_incremental(blob)?;
        let missing = step.get_missing_deps(&[]);
        if !missing.is_empty() {
            return Ok(Err(missing));
        }
        let step_text_id = lookup_content_text_id(&step)?;
        let next_text = step.text(&step_text_id)?;
        if non_empty_count == 1 {
            return Ok(Ok((step, next_text)));
        }
        if next_text != current_text {
            let actor = branch_actor_id(
                "crdt_step",
                applied_idx as u64,
                Blake3Hash::from_bytes(next_text.as_bytes()),
            );
            let mut forked = doc.fork().with_actor(actor);
            let text_id = lookup_content_text_id(&forked)?;
            forked.update_text(&text_id, &next_text)?;
            forked.commit();
            doc = forked;
            current_text = next_text;
        }
        applied_idx += 1;
    }
    Ok(Ok((doc, current_text)))
}

fn format_missing_deps(missing: &[ChangeHash]) -> String {
    missing
        .iter()
        .map(|h| h.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn merge_crdt_documents(base_text: &str, change_blobs: &[&[u8]]) -> Result<String, CrdtError> {
    let crlf_base = is_crlf_dominant(base_text);
    let base_lf = normalize_lf(base_text);
    let mut merged = init_text_doc(&base_lf)?;
    let mut resolved: Vec<(AutoCommit, String, Vec<ChangeHash>)> = Vec::new();
    let mut pending: Vec<(Blake3Hash, &[u8])> = change_blobs
        .iter()
        .copied()
        .filter(|b| !b.is_empty())
        .map(|b| (Blake3Hash::from_bytes(b), b))
        .collect();
    pending.sort_by_key(|(h, _)| *h);

    while !pending.is_empty() {
        let mut next_pending = Vec::new();
        let mut made_progress = false;

        for (blob_hash, blob) in pending {
            match try_apply_blobs_on_text(&base_lf, &[blob])? {
                Ok((mut branch, text)) => {
                    merged.merge(&mut branch)?;
                    let genesis_heads = init_text_doc(&text)?.get_heads();
                    resolved.push((branch, text, genesis_heads));
                    made_progress = true;
                }
                Err(missing) => {
                    if let Some((parent_doc, parent_text, _)) = resolved
                        .iter_mut()
                        .find(|(_, _, heads)| missing.iter().all(|h| heads.contains(h)))
                    {
                        let parent_text_owned = parent_text.clone();
                        match try_apply_blobs_on_text(&parent_text_owned, &[blob])? {
                            Ok((_, child_text)) => {
                                let actor = branch_actor_id("crdt_chain", 0, blob_hash);
                                let mut child_doc = parent_doc.fork().with_actor(actor);
                                let text_id = lookup_content_text_id(&child_doc)?;
                                if child_text != parent_text_owned {
                                    child_doc.update_text(&text_id, &child_text)?;
                                    child_doc.commit();
                                }
                                merged.merge(&mut child_doc)?;
                                let genesis_heads = init_text_doc(&child_text)?.get_heads();
                                resolved.push((child_doc, child_text, genesis_heads));
                                made_progress = true;
                            }
                            Err(_) => {
                                next_pending.push((blob_hash, blob));
                            }
                        }
                    } else {
                        next_pending.push((blob_hash, blob));
                    }
                }
            }
        }

        if !made_progress {
            let mut probe = init_text_doc(&base_lf)?;
            let (_, sample_blob) = next_pending[0];
            probe.load_incremental(sample_blob)?;
            let missing = probe.get_missing_deps(&[]);
            return Err(CrdtError::MissingDependencies(format_missing_deps(
                &missing,
            )));
        }
        pending = next_pending;
    }

    let text_id = lookup_content_text_id(&merged)?;
    Ok(restore_line_endings(merged.text(&text_id)?, crlf_base))
}

#[derive(Debug, Clone)]
pub struct CrdtPatchInput<'a> {
    pub patch_hash: Blake3Hash,
    pub actor_id: &'a str,
    pub lamport_ts: u64,
    pub parents: &'a [Blake3Hash],
    pub blobs: Vec<&'a [u8]>,
}

type ResolvedCrdtNode = (AutoCommit, String, Vec<ChangeHash>);

fn replay_blobs_onto_parent(
    parent_doc: &mut AutoCommit,
    parent_text: &str,
    input: &CrdtPatchInput<'_>,
) -> Result<Result<ResolvedCrdtNode, Vec<ChangeHash>>, CrdtError> {
    match try_apply_blobs_on_text(parent_text, &input.blobs)? {
        Ok((_, child_text)) => {
            let actor = branch_actor_id(input.actor_id, input.lamport_ts, input.patch_hash);
            let mut child_doc = parent_doc.fork().with_actor(actor);
            let child_text_id = lookup_content_text_id(&child_doc)?;
            if child_text != parent_text {
                child_doc.update_text(&child_text_id, &child_text)?;
                child_doc.commit();
            }
            let genesis_heads = init_text_doc(&child_text)?.get_heads();
            Ok(Ok((child_doc, child_text, genesis_heads)))
        }
        Err(missing) => Ok(Err(missing)),
    }
}

pub fn merge_crdt_patch_dag(
    base_text: &str,
    inputs: &[CrdtPatchInput<'_>],
) -> Result<String, CrdtError> {
    if inputs.is_empty() {
        return Ok(base_text.to_string());
    }
    let crlf_base = is_crlf_dominant(base_text);
    let base_lf = normalize_lf(base_text);

    let mut idx_by_hash: HashMap<Blake3Hash, usize> = HashMap::with_capacity(inputs.len());
    for (i, inp) in inputs.iter().enumerate() {
        idx_by_hash.insert(inp.patch_hash, i);
    }

    let mut in_group_parents: Vec<Vec<usize>> = vec![Vec::new(); inputs.len()];
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); inputs.len()];
    let mut in_deg: Vec<usize> = vec![0; inputs.len()];

    for (i, inp) in inputs.iter().enumerate() {
        let mut seen = HashSet::new();
        for p_hash in inp.parents {
            if let Some(&p_idx) = idx_by_hash.get(p_hash) {
                if p_idx == i {
                    return Err(CrdtError::CyclicDependencies);
                }
                if seen.insert(p_idx) {
                    in_group_parents[i].push(p_idx);
                    children[p_idx].push(i);
                    in_deg[i] += 1;
                }
            }
        }
    }

    let mut ready: BTreeSet<(u64, Blake3Hash, usize)> = BTreeSet::new();
    for (i, inp) in inputs.iter().enumerate() {
        if in_deg[i] == 0 {
            ready.insert((inp.lamport_ts, inp.patch_hash, i));
        }
    }

    let mut topo_order: Vec<usize> = Vec::with_capacity(inputs.len());
    while let Some((_, _, idx)) = ready.pop_first() {
        topo_order.push(idx);
        for &c_idx in &children[idx] {
            in_deg[c_idx] -= 1;
            if in_deg[c_idx] == 0 {
                ready.insert((inputs[c_idx].lamport_ts, inputs[c_idx].patch_hash, c_idx));
            }
        }
    }

    if topo_order.len() != inputs.len() {
        return Err(CrdtError::CyclicDependencies);
    }

    let mut resolved: Vec<Option<ResolvedCrdtNode>> = (0..inputs.len()).map(|_| None).collect();
    let mut deferred: Vec<usize> = Vec::new();
    let mut canonical_resolved_order: Vec<usize> = Vec::new();

    for &i in &topo_order {
        if !in_group_parents[i].is_empty() {
            if in_group_parents[i].iter().any(|&p| resolved[p].is_none()) {
                deferred.push(i);
                continue;
            }
            let p0 = in_group_parents[i][0];
            let mut parent_doc = resolved[p0].as_mut().unwrap().0.fork();
            for &pk in &in_group_parents[i][1..] {
                let mut other = resolved[pk].as_mut().unwrap().0.fork();
                parent_doc.merge(&mut other)?;
            }
            let text_id = lookup_content_text_id(&parent_doc)?;
            let parent_text = parent_doc.text(&text_id)?;
            match replay_blobs_onto_parent(&mut parent_doc, &parent_text, &inputs[i])? {
                Ok(node) => {
                    resolved[i] = Some(node);
                    canonical_resolved_order.push(i);
                }
                Err(missing) => {
                    return Err(CrdtError::MissingDependencies(format_missing_deps(
                        &missing,
                    )));
                }
            }
        } else {
            match try_apply_blobs_on_text(&base_lf, &inputs[i].blobs)? {
                Ok((doc, text)) => {
                    let genesis_heads = init_text_doc(&text)?.get_heads();
                    resolved[i] = Some((doc, text, genesis_heads));
                    canonical_resolved_order.push(i);
                }
                Err(missing) => {
                    let matching_parent = canonical_resolved_order
                        .iter()
                        .copied()
                        .filter(|&j| {
                            resolved[j]
                                .as_ref()
                                .is_some_and(|(_, _, heads)| missing.iter().all(|h| heads.contains(h)))
                        })
                        .min_by_key(|&j| (inputs[j].lamport_ts, inputs[j].patch_hash));

                    if let Some(j) = matching_parent {
                        let (mut parent_doc, parent_text, _) = resolved[j].take().unwrap();
                        let res =
                            replay_blobs_onto_parent(&mut parent_doc, &parent_text, &inputs[i])?;
                        let parent_heads = init_text_doc(&parent_text)?.get_heads();
                        resolved[j] = Some((parent_doc, parent_text, parent_heads));
                        match res {
                            Ok(node) => {
                                resolved[i] = Some(node);
                                canonical_resolved_order.push(i);
                            }
                            Err(m2) => {
                                return Err(CrdtError::MissingDependencies(format_missing_deps(
                                    &m2,
                                )));
                            }
                        }
                    } else {
                        deferred.push(i);
                    }
                }
            }
        }
    }

    while !deferred.is_empty() {
        let mut next_deferred = Vec::new();
        let mut made_progress = false;

        for i in deferred {
            if !in_group_parents[i].is_empty() {
                if in_group_parents[i].iter().any(|&p| resolved[p].is_none()) {
                    next_deferred.push(i);
                    continue;
                }
                let p0 = in_group_parents[i][0];
                let mut parent_doc = resolved[p0].as_mut().unwrap().0.fork();
                for &pk in &in_group_parents[i][1..] {
                    let mut other = resolved[pk].as_mut().unwrap().0.fork();
                    parent_doc.merge(&mut other)?;
                }
                let text_id = lookup_content_text_id(&parent_doc)?;
                let parent_text = parent_doc.text(&text_id)?;
                match replay_blobs_onto_parent(&mut parent_doc, &parent_text, &inputs[i])? {
                    Ok(node) => {
                        resolved[i] = Some(node);
                        canonical_resolved_order.push(i);
                        made_progress = true;
                    }
                    Err(missing) => {
                        return Err(CrdtError::MissingDependencies(format_missing_deps(
                            &missing,
                        )));
                    }
                }
            } else {
                let Err(missing) = try_apply_blobs_on_text(&base_lf, &inputs[i].blobs)? else {
                    unreachable!("deferred node previously had missing deps");
                };
                let matching_parent = canonical_resolved_order
                    .iter()
                    .copied()
                    .filter(|&j| {
                        resolved[j]
                            .as_ref()
                            .is_some_and(|(_, _, heads)| missing.iter().all(|h| heads.contains(h)))
                    })
                    .min_by_key(|&j| (inputs[j].lamport_ts, inputs[j].patch_hash));

                if let Some(j) = matching_parent {
                    let (mut parent_doc, parent_text, _) = resolved[j].take().unwrap();
                    let res =
                        replay_blobs_onto_parent(&mut parent_doc, &parent_text, &inputs[i])?;
                    let parent_heads = init_text_doc(&parent_text)?.get_heads();
                    resolved[j] = Some((parent_doc, parent_text, parent_heads));
                    match res {
                        Ok(node) => {
                            resolved[i] = Some(node);
                            canonical_resolved_order.push(i);
                            made_progress = true;
                        }
                        Err(m2) => {
                            return Err(CrdtError::MissingDependencies(format_missing_deps(
                                &m2,
                            )));
                        }
                    }
                } else {
                    next_deferred.push(i);
                }
            }
        }

        if !made_progress {
            let idx = next_deferred[0];
            let missing = match try_apply_blobs_on_text(&base_lf, &inputs[idx].blobs)? {
                Err(m) => m,
                Ok(_) => Vec::new(),
            };
            return Err(CrdtError::MissingDependencies(format_missing_deps(
                &missing,
            )));
        }
        deferred = next_deferred;
    }

    let mut merged = init_text_doc(&base_lf)?;
    for i in topo_order {
        if let Some((mut doc, _, _)) = resolved[i].take() {
            merged.merge(&mut doc)?;
        }
    }
    let text_id = lookup_content_text_id(&merged)?;
    Ok(restore_line_endings(merged.text(&text_id)?, crlf_base))
}

pub fn validate_json_strict(text: &str) -> Result<(), String> {
    serde_json::from_str::<serde_json::Value>(text).map_err(|e| e.to_string())?;
    let mut de = serde_json::Deserializer::from_str(text);
    JsonUniqueKeyCheck
        .deserialize(&mut de)
        .map_err(|e| e.to_string())?;
    de.end().map_err(|e| e.to_string())?;
    Ok(())
}

pub fn validate_yaml_strict(text: &str) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err("empty or whitespace-only YAML document".to_string());
    }
    let mut doc_count = 0usize;
    for doc in serde_yaml_ng::Deserializer::from_str(text) {
        doc_count += 1;
        serde_yaml_ng::Value::deserialize(doc).map_err(|e| e.to_string())?;
    }
    if doc_count == 0 {
        return Err("empty or whitespace-only YAML document".to_string());
    }
    Ok(())
}

pub fn merge_non_code_file(
    lang: LanguageKind,
    file_path: &Path,
    base_text: &str,
    change_blobs: &[&[u8]],
    patch_a: Blake3Hash,
    patch_b: Blake3Hash,
) -> Result<Result<String, MergeOutcome>, CrdtError> {
    let merged = merge_crdt_documents(base_text, change_blobs)?;
    validate_merged_non_code(lang, file_path, merged, patch_a, patch_b)
}

pub fn merge_non_code_file_dag(
    lang: LanguageKind,
    file_path: &Path,
    base_text: &str,
    inputs: &[CrdtPatchInput<'_>],
    patch_a: Blake3Hash,
    patch_b: Blake3Hash,
) -> Result<Result<String, MergeOutcome>, CrdtError> {
    let merged = merge_crdt_patch_dag(base_text, inputs)?;
    validate_merged_non_code(lang, file_path, merged, patch_a, patch_b)
}

pub fn validate_merged_non_code(
    lang: LanguageKind,
    file_path: &Path,
    merged: String,
    patch_a: Blake3Hash,
    patch_b: Blake3Hash,
) -> Result<Result<String, MergeOutcome>, CrdtError> {
    match lang {
        LanguageKind::Json => {
            if let Err(reason) = validate_json_strict(&merged) {
                return Ok(Err(MergeOutcome::StructuralConflict {
                    file: file_path.to_path_buf(),
                    anchor: None,
                    patch_a,
                    patch_b,
                    reason: format!("post-merge JSON validation failed: {reason}"),
                }));
            }
        }
        LanguageKind::Yaml => {
            if let Err(reason) = validate_yaml_strict(&merged) {
                return Ok(Err(MergeOutcome::StructuralConflict {
                    file: file_path.to_path_buf(),
                    anchor: None,
                    patch_a,
                    patch_b,
                    reason: format!("post-merge YAML validation failed: {reason}"),
                }));
            }
        }
        _ => {}
    }
    Ok(Ok(merged))
}

struct JsonUniqueKeyCheck;

impl<'de> DeserializeSeed<'de> for JsonUniqueKeyCheck {
    type Value = ();

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_any(JsonUniqueKeyVisitor)
    }
}

struct JsonUniqueKeyVisitor;

impl<'de> serde::de::Visitor<'de> for JsonUniqueKeyVisitor {
    type Value = ();

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a JSON value without duplicate object keys")
    }

    fn visit_bool<E: serde::de::Error>(self, _v: bool) -> Result<(), E> {
        Ok(())
    }

    fn visit_i64<E: serde::de::Error>(self, _v: i64) -> Result<(), E> {
        Ok(())
    }

    fn visit_u64<E: serde::de::Error>(self, _v: u64) -> Result<(), E> {
        Ok(())
    }

    fn visit_f64<E: serde::de::Error>(self, _v: f64) -> Result<(), E> {
        Ok(())
    }

    fn visit_str<E: serde::de::Error>(self, _v: &str) -> Result<(), E> {
        Ok(())
    }

    fn visit_string<E: serde::de::Error>(self, _v: String) -> Result<(), E> {
        Ok(())
    }

    fn visit_none<E: serde::de::Error>(self) -> Result<(), E> {
        Ok(())
    }

    fn visit_unit<E: serde::de::Error>(self) -> Result<(), E> {
        Ok(())
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while seq.next_element_seed(JsonUniqueKeyCheck)?.is_some() {}
        Ok(())
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let mut seen = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !seen.insert(key.clone()) {
                return Err(serde::de::Error::custom(format!(
                    "duplicate JSON key '{key}'"
                )));
            }
            map.next_value_seed(JsonUniqueKeyCheck)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_crdt_commutative_and_associative_merge() {
        let base = "# Title\n\n## Section A\nalpha\n\n## Section B\nbeta\n";
        let m1 = "# Title\n\n## Section A\nalpha updated by agent 1\n\n## Section B\nbeta\n";
        let m2 = "# Title\n\n## Section A\nalpha\n\n## Section B\nbeta updated by agent 2\n";
        let m3 = "# Title (Final)\n\n## Section A\nalpha\n\n## Section B\nbeta\n";

        let c1 = diff_text_to_crdt_changes(base, m1, "agent-1", 1).unwrap();
        let c2 = diff_text_to_crdt_changes(base, m2, "agent-2", 2).unwrap();
        let c3 = diff_text_to_crdt_changes(base, m3, "agent-3", 3).unwrap();

        let r12 = merge_crdt_documents(base, &[&c1, &c2]).unwrap();
        let r21 = merge_crdt_documents(base, &[&c2, &c1]).unwrap();
        assert_eq!(r12, r21);
        assert!(r12.contains("alpha updated by agent 1"));
        assert!(r12.contains("beta updated by agent 2"));

        let r123 = merge_crdt_documents(base, &[&c1, &c2, &c3]).unwrap();
        let r321 = merge_crdt_documents(base, &[&c3, &c2, &c1]).unwrap();
        let r231 = merge_crdt_documents(base, &[&c2, &c3, &c1]).unwrap();
        assert_eq!(r123, r321);
        assert_eq!(r123, r231);
        assert!(r123.contains("# Title (Final)"));
        assert!(r123.contains("alpha updated by agent 1"));
        assert!(r123.contains("beta updated by agent 2"));
    }

    #[test]
    fn json_crdt_valid_merge_and_duplicate_or_malformed_conflict() {
        let base = "{\n  \"host\": \"localhost\",\n  \"port\": 8080\n}\n";
        let e1 = "{\n  \"host\": \"0.0.0.0\",\n  \"port\": 8080\n}\n";
        let e2 = "{\n  \"host\": \"localhost\",\n  \"port\": 9090\n}\n";

        let c1 = diff_text_to_crdt_changes(base, e1, "a1", 1).unwrap();
        let c2 = diff_text_to_crdt_changes(base, e2, "a2", 2).unwrap();
        let pa = Blake3Hash::from_bytes(b"pa");
        let pb = Blake3Hash::from_bytes(b"pb");

        let merged = merge_non_code_file(
            LanguageKind::Json,
            Path::new("config.json"),
            base,
            &[&c1, &c2],
            pa,
            pb,
        )
        .unwrap()
        .expect("disjoint JSON edits must merge cleanly");
        let parsed: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(parsed["host"], "0.0.0.0");
        assert_eq!(parsed["port"], 9090);

        // Concurrent insertion of the same key at the same spot triggers duplicate key or syntax conflict
        let dup_base = "{\n  \"a\": 1\n}\n";
        let d1 = "{\n  \"a\": 1,\n  \"k\": 10\n}\n";
        let d2 = "{\n  \"a\": 1,\n  \"k\": 20\n}\n";
        let dc1 = diff_text_to_crdt_changes(dup_base, d1, "a1", 1).unwrap();
        let dc2 = diff_text_to_crdt_changes(dup_base, d2, "a2", 2).unwrap();
        let res = merge_non_code_file(
            LanguageKind::Json,
            Path::new("dup.json"),
            dup_base,
            &[&dc1, &dc2],
            pa,
            pb,
        )
        .unwrap();
        assert!(matches!(res, Err(MergeOutcome::StructuralConflict { .. })));

        // Direct duplicate key validator test
        assert!(validate_json_strict(r#"{"nested": {"x": 1, "x": 2}}"#).is_err());
        assert!(validate_json_strict(r#"{"nested": {"x": 1, "y": 2}}"#).is_ok());
    }

    #[test]
    fn yaml_crdt_valid_merge_and_structural_conflict() {
        let base = "service:\n  name: loom\n  port: 7000\n";
        let y1 = "service:\n  name: loomd\n  port: 7000\n";
        let y2 = "service:\n  name: loom\n  port: 7443\n";
        let c1 = diff_text_to_crdt_changes(base, y1, "a1", 1).unwrap();
        let c2 = diff_text_to_crdt_changes(base, y2, "a2", 2).unwrap();
        let pa = Blake3Hash::from_bytes(b"pa");
        let pb = Blake3Hash::from_bytes(b"pb");

        let merged = merge_non_code_file(
            LanguageKind::Yaml,
            Path::new("loom.yaml"),
            base,
            &[&c1, &c2],
            pa,
            pb,
        )
        .unwrap()
        .expect("disjoint YAML edits must merge cleanly");
        assert!(merged.contains("name: loomd"));
        assert!(merged.contains("port: 7443"));

        // Duplicate key in YAML -> StructuralConflict
        let bad1 = "service:\n  name: loom\n  port: 7000\n  workers: 4\n";
        let bad2 = "service:\n  name: loom\n  port: 7000\n  workers: 8\n";
        let bc1 = diff_text_to_crdt_changes(base, bad1, "a1", 1).unwrap();
        let bc2 = diff_text_to_crdt_changes(base, bad2, "a2", 2).unwrap();
        let conflict = merge_non_code_file(
            LanguageKind::Yaml,
            Path::new("loom.yaml"),
            base,
            &[&bc1, &bc2],
            pa,
            pb,
        )
        .unwrap();
        assert!(matches!(
            conflict,
            Err(MergeOutcome::StructuralConflict { .. })
        ));
    }

    #[test]
    fn sequential_chained_crdt_blobs_and_missing_deps_rejected() {
        let base = "# Doc\n\nA: 1\n\nB: 2\n";
        let v1 = "# Doc\n\nA: 10\n\nB: 2\n";
        let v2 = "# Doc\n\nA: 10\n\nB: 20\n";

        let c1 = diff_text_to_crdt_changes(base, v1, "a1", 1).unwrap();
        let c2 = diff_text_to_crdt_changes(v1, v2, "a1", 2).unwrap();

        assert_eq!(merge_crdt_documents(base, &[&c1, &c2]).unwrap(), v2);
        assert_eq!(merge_crdt_documents(base, &[&c2, &c1]).unwrap(), v2);

        // Missing parent blob must error rather than silently dropping c2
        assert!(matches!(
            merge_crdt_documents(base, &[&c2]),
            Err(CrdtError::MissingDependencies(_))
        ));
    }

    #[test]
    fn branched_crdt_chain_is_permutation_invariant_across_all_6_orders() {
        let base = "end\n";
        let mid = "mid_0\nend\n";
        let branch_a = "mid_0\nbranchA_0\nend\n";
        let branch_b = "mid_0\nbranchB_0\nend\n";

        let c1 = diff_text_to_crdt_changes(base, mid, "a1", 1).unwrap();
        let c2a = diff_text_to_crdt_changes(mid, branch_a, "a2", 2).unwrap();
        let c2b = diff_text_to_crdt_changes(mid, branch_b, "a3", 2).unwrap();

        let perms: [&[&[u8]]; 6] = [
            &[&c1, &c2a, &c2b],
            &[&c1, &c2b, &c2a],
            &[&c2a, &c1, &c2b],
            &[&c2a, &c2b, &c1],
            &[&c2b, &c1, &c2a],
            &[&c2b, &c2a, &c1],
        ];
        let results: Vec<String> = perms
            .iter()
            .map(|p| merge_crdt_documents(base, p).unwrap())
            .collect();
        for r in &results[1..] {
            assert_eq!(r, &results[0], "diverged across permutations: {results:?}");
        }
        assert!(results[0].contains("mid_0"));
        assert!(results[0].contains("branchA_0"));
        assert!(results[0].contains("branchB_0"));
        assert!(results[0].contains("end"));
    }

    #[test]
    fn self_referencing_crdt_patch_rejected_as_cyclic_dependencies() {
        let base = "# Doc\n\nA: 1\n";
        let v1 = "# Doc\n\nA: 10\n";
        let c1 = diff_text_to_crdt_changes(base, v1, "a1", 1).unwrap();
        let self_hash = Blake3Hash::from_bytes(b"self_cycle_patch");
        let parents = [self_hash];
        let inp = CrdtPatchInput {
            patch_hash: self_hash,
            actor_id: "a1",
            lamport_ts: 1,
            parents: &parents,
            blobs: vec![c1.as_slice()],
        };
        assert!(matches!(
            merge_crdt_patch_dag(base, &[inp]),
            Err(CrdtError::CyclicDependencies)
        ));
    }

    #[test]
    fn crlf_vs_lf_concurrent_markdown_merge_preserves_lines() {
        let base_lf = "# Specification\n\n## Section A\nInitial A\n\n## Section B\nInitial B\n";
        let vp1_crlf =
            "# Specification\r\n\r\n## Section A\r\nUpdated A from vp1 (CRLF)\r\n\r\n## Section B\r\nInitial B\r\n";
        let vp2_lf =
            "# Specification\n\n## Section A\nInitial A\n\n## Section B\nUpdated B from vp2 (LF)\n";

        let c1 = diff_text_to_crdt_changes(base_lf, vp1_crlf, "vp1", 1).unwrap();
        let c2 = diff_text_to_crdt_changes(base_lf, vp2_lf, "vp2", 2).unwrap();

        let merged = merge_crdt_documents(base_lf, &[&c1, &c2]).unwrap();
        assert!(!merged.contains('\r'));
        assert!(merged.contains("Updated A from vp1 (CRLF)"));
        assert!(merged.contains("Updated B from vp2 (LF)"));

        // CRLF-dominant base restores CRLF line endings on merge
        let base_crlf = base_lf.replace('\n', "\r\n");
        let c1_b = diff_text_to_crdt_changes(&base_crlf, vp1_crlf, "vp1", 1).unwrap();
        let c2_b = diff_text_to_crdt_changes(&base_crlf, vp2_lf, "vp2", 2).unwrap();
        let merged_crlf = merge_crdt_documents(&base_crlf, &[&c1_b, &c2_b]).unwrap();
        assert!(merged_crlf.contains("Updated A from vp1 (CRLF)\r\n"));
        assert!(merged_crlf.contains("Updated B from vp2 (LF)\r\n"));
    }

    #[test]
    fn validate_yaml_strict_rejects_empty_and_whitespace_and_validates_multidoc() {
        assert!(validate_yaml_strict("").is_err());
        assert!(validate_yaml_strict("   \n\t\r\n  ").is_err());
        assert!(validate_yaml_strict("# comment only\n").is_ok());
        assert!(validate_yaml_strict("service:\n  port: 8080\n").is_ok());
        assert!(validate_yaml_strict("a: 1\n---\nb: 2\n").is_ok());
        assert!(validate_yaml_strict("a: 1\n---\nb: [unclosed\n").is_err());

        // Merging a YAML truncation to empty triggers post-merge StructuralConflict
        let base = "service:\n  port: 8080\n";
        let c_empty = diff_text_to_crdt_changes(base, "", "vp1", 1).unwrap();
        let pa = Blake3Hash::from_bytes(b"pa");
        let pb = Blake3Hash::from_bytes(b"pb");
        let res = merge_non_code_file(
            LanguageKind::Yaml,
            Path::new("config.yaml"),
            base,
            &[&c_empty],
            pa,
            pb,
        )
        .unwrap();
        assert!(matches!(res, Err(MergeOutcome::StructuralConflict { .. })));
    }
}
