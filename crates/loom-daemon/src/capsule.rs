use crate::radar::{collect_non_ignored_files, is_ignored_relative_path};
use crate::viewport_mgr::{
    capture_workspace_snapshot, collect_patch_symbols, load_workspace_snapshot,
    normalize_snapshot_key, prepare_and_apply_3way_seal, snapshot_key_to_rel_path, to_summary,
};
use crate::{DaemonContext, MutationRequest, WriterCommand};
use loom_core::cas::CasStore;
use loom_core::cst::{
    DiffContext, compute_logical_file_id, diff_file_to_patch, extract_decl_units,
    flatten_decl_units, parse_source,
};
use loom_core::db::decode_value;
use loom_core::model::{
    AstPatch, AstPatchRecord, Blake3Hash, CausalCapsule, FailureKind, LanguageKind, NegativeEdge,
    OpKind, SemanticOp, VerificationProof, ViewportRecord, ViewportState, now_ns,
};
use loom_core::rpc::{DropResult, RpcError, RpcErrorCode, UndoResult};
use loom_core::wal::{InverseEffect, StateEffect};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::oneshot;

const VERIFICATION_PROOF_DOMAIN: &str = "project-loom-verification-proof-v1";

pub(crate) fn collect_patch_symbols_vec(patch: &AstPatch) -> Vec<String> {
    let mut set = BTreeSet::new();
    collect_patch_symbols(patch, &mut set);
    set.remove("<file>");
    set.into_iter().collect()
}

fn is_file_exact_case(root: &Path, rel_path: &Path) -> bool {
    if rel_path.as_os_str().is_empty() {
        return false;
    }
    let full_path = root.join(rel_path);
    let Ok(meta) = std::fs::symlink_metadata(&full_path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(windows)]
    {
        let mut cur = root.to_path_buf();
        for comp in rel_path.components() {
            let Component::Normal(expected) = comp else {
                return false;
            };
            let Ok(entries) = std::fs::read_dir(&cur) else {
                return false;
            };
            let mut matched = false;
            for entry in entries.flatten() {
                if entry.file_name() == expected {
                    matched = true;
                    break;
                }
            }
            if !matched {
                return false;
            }
            cur.push(expected);
        }
        true
    }
    #[cfg(not(windows))]
    {
        true
    }
}

fn write_len_prefixed_str(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
    buf.extend_from_slice(s.as_bytes());
}

#[allow(clippy::too_many_arguments)]
pub fn compute_verification_proof_mac(
    repo_id: &str,
    capsule_id: &str,
    viewport_id: &str,
    patch_ids: &[String],
    verify_cmd: Option<&str>,
    exit_code: i32,
    passed: bool,
    output_digest: Blake3Hash,
    duration_ms: u64,
    verified_at_ns: u64,
) -> Blake3Hash {
    let key = blake3::derive_key(VERIFICATION_PROOF_DOMAIN, repo_id.as_bytes());
    let mut payload = Vec::with_capacity(256);
    write_len_prefixed_str(&mut payload, capsule_id);
    write_len_prefixed_str(&mut payload, viewport_id);
    payload.extend_from_slice(&(patch_ids.len() as u32).to_le_bytes());
    for pid in patch_ids {
        write_len_prefixed_str(&mut payload, pid);
    }
    match verify_cmd {
        Some(cmd) => {
            payload.push(1);
            write_len_prefixed_str(&mut payload, cmd);
        }
        None => payload.push(0),
    }
    payload.extend_from_slice(&exit_code.to_le_bytes());
    payload.push(u8::from(passed));
    payload.extend_from_slice(output_digest.as_bytes());
    payload.extend_from_slice(&duration_ms.to_le_bytes());
    payload.extend_from_slice(&verified_at_ns.to_le_bytes());

    let mac = blake3::keyed_hash(&key, &payload);
    Blake3Hash(*mac.as_bytes())
}

pub fn verify_verification_proof(repo_id: &str, capsule: &CausalCapsule) -> bool {
    let Some(ref proof) = capsule.verification_proof else {
        return false;
    };
    let expected = compute_verification_proof_mac(
        repo_id,
        &capsule.capsule_id,
        &capsule.viewport_id,
        &capsule.patch_ids,
        proof.verify_cmd.as_deref(),
        proof.exit_code,
        proof.passed,
        proof.output_digest,
        proof.duration_ms,
        proof.verified_at_ns,
    );
    expected == proof.proof_hash
}

pub async fn seal_capsule(
    ctx: &Arc<DaemonContext>,
    viewport_id: &str,
    verify_cmd: Option<String>,
    summary: Option<String>,
) -> Result<CausalCapsule, RpcError> {
    let _ = ctx
        .db
        .get_repo_meta()
        .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
        .ok_or_else(|| {
            RpcError::new(
                RpcErrorCode::NotInitialized,
                "Loom repository is not initialized; run `loom init` first",
            )
        })?;

    // Atomically claim the viewport (`Spawned | Active -> Verifying`) so two concurrent
    // seal requests on the same viewport cannot both enter verification.
    let (tx, rx) = oneshot::channel();
    ctx.writer
        .send(WriterCommand::ClaimViewportForSeal {
            viewport_id: viewport_id.to_string(),
            reply: tx,
        })
        .await
        .map_err(|_| RpcError::new(RpcErrorCode::Internal, "writer actor closed"))?;
    let vp = rx
        .await
        .map_err(|_| RpcError::new(RpcErrorCode::Internal, "writer actor dropped reply"))??;

    let cas = match CasStore::new(ctx.loom_dir.join("cas"), ctx.config.zstd_level) {
        Ok(c) => c,
        Err(err) => {
            let _ = revert_verifying_to_active(ctx, &vp.id).await;
            return Err(RpcError::new(RpcErrorCode::IoError, err.to_string()));
        }
    };

    let verify_trimmed = verify_cmd
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let (mut verification_proof, proof_output_bytes) = if let Some(cmd_str) = verify_trimmed {
        let vp_dir = vp.path.clone();
        let cmd_exec = cmd_str.clone();
        let started = Instant::now();
        let output = match tokio::task::spawn_blocking(move || {
            #[cfg(windows)]
            let res = {
                use std::os::windows::process::CommandExt;
                std::process::Command::new("cmd")
                    .arg("/S")
                    .arg("/C")
                    .raw_arg(format!("\"{cmd_exec}\""))
                    .current_dir(&vp_dir)
                    .output()
            };
            #[cfg(unix)]
            let res = std::process::Command::new("sh")
                .args(["-c", &cmd_exec])
                .current_dir(&vp_dir)
                .output();
            res
        })
        .await
        {
            Ok(out) => out,
            Err(err) => {
                let _ = revert_verifying_to_active(ctx, &vp.id).await;
                return Err(RpcError::new(RpcErrorCode::Internal, err.to_string()));
            }
        };

        let duration_ms = started.elapsed().as_millis() as u64;
        let out = match output {
            Ok(o) => o,
            Err(err) => {
                let _ = revert_verifying_to_active(ctx, &vp.id).await;
                return Err(RpcError::new(
                    RpcErrorCode::IoError,
                    format!("failed to execute verify_cmd `{cmd_str}`: {err}"),
                ));
            }
        };

        let mut combined = out.stdout;
        if !out.stderr.is_empty() {
            if !combined.is_empty() {
                combined.push(b'\n');
            }
            combined.extend_from_slice(&out.stderr);
        }
        let exit_code = out.status.code().unwrap_or(-1);
        let passed = out.status.success();
        let output_digest = Blake3Hash::from_bytes(&combined);

        if !passed {
            let _ = revert_verifying_to_active(ctx, &vp.id).await;
            let snippet = String::from_utf8_lossy(&combined);
            return Err(RpcError::new(
                RpcErrorCode::InvalidState,
                format!(
                    "verification command `{cmd_str}` failed (exit {exit_code}): {}",
                    snippet.chars().take(400).collect::<String>()
                ),
            ));
        }

        (
            Some(VerificationProof {
                verify_cmd: Some(cmd_str),
                exit_code,
                passed,
                output_digest,
                proof_hash: Blake3Hash::ZERO,
                duration_ms,
                verified_at_ns: now_ns(),
            }),
            Some(combined),
        )
    } else {
        (None, None)
    };

    // Serialize trunk read/diff/merge/write + WriterActor commit across concurrent seals
    let seal_guard = ctx.trunk_seal_lock.lock().await;

    // Flush any un-debounced saves/deletes into the unstaged WAL AstPatch chain before sealing
    ctx.radar.flush_viewport(&vp.id).await;

    let mut fresh_vp = match ctx.db.get_viewport(&vp.id) {
        Ok(Some(v)) => v,
        Ok(None) => {
            return Err(RpcError::new(
                RpcErrorCode::NotFound,
                format!("viewport `{}` no longer exists", vp.id),
            ));
        }
        Err(err) => {
            drop(seal_guard);
            let _ = revert_verifying_to_active(ctx, &vp.id).await;
            return Err(RpcError::new(RpcErrorCode::StorageError, err.to_string()));
        }
    };
    if fresh_vp.state != ViewportState::Verifying {
        return Err(RpcError::new(
            RpcErrorCode::InvalidState,
            format!(
                "viewport `{}` transitioned out of Verifying ({}) prior to seal commit",
                fresh_vp.name, fresh_vp.state
            ),
        ));
    }

    let meta = match ctx.db.get_repo_meta() {
        Ok(Some(m)) => m,
        Ok(None) => {
            drop(seal_guard);
            let _ = revert_verifying_to_active(ctx, &fresh_vp.id).await;
            return Err(RpcError::new(
                RpcErrorCode::NotInitialized,
                "Loom repository is not initialized; run `loom init` first",
            ));
        }
        Err(err) => {
            drop(seal_guard);
            let _ = revert_verifying_to_active(ctx, &fresh_vp.id).await;
            return Err(RpcError::new(RpcErrorCode::StorageError, err.to_string()));
        }
    };

    let repo_root = ctx.repo_root.clone();
    let vp_for_merge = fresh_vp.clone();
    let ignore_dirs = ctx.config.radar.ignore_dirs.clone();
    let cas_dir = ctx.loom_dir.join("cas");
    let zstd_level = ctx.config.zstd_level;

    let prepared_res = tokio::task::spawn_blocking(move || {
        let cas = CasStore::new(cas_dir, zstd_level)
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
        prepare_and_apply_3way_seal(&repo_root, &vp_for_merge, &ignore_dirs, &cas)
    })
    .await
    .map_err(|e| RpcError::new(RpcErrorCode::Internal, e.to_string()))
    .and_then(|r| r);

    let prepared = match prepared_res {
        Ok(p) => p,
        Err(err) => {
            drop(seal_guard);
            let _ = revert_verifying_to_active(ctx, &fresh_vp.id).await;
            return Err(err);
        }
    };

    // Unregister from Radar BEFORE transitioning to Sealed
    ctx.radar.unregister_viewport(&fresh_vp.id).await;

    let now = now_ns();
    let capsule_id = format!("cap-{}", uuid::Uuid::new_v4());
    let mut effects = Vec::with_capacity(prepared.vp_patches.len() + 2);
    let base_lsn = meta.next_lsn.max(1);

    let prepared_file_keys: BTreeSet<String> = prepared
        .touched_files
        .iter()
        .map(|p| normalize_snapshot_key(p))
        .filter(|k| !k.is_empty())
        .collect();
    let mut latest_wal_cas_by_file: BTreeMap<String, Option<Blake3Hash>> = BTreeMap::new();
    let mut touched_files_set: BTreeSet<PathBuf> = prepared.touched_files.iter().cloned().collect();
    let mut touched_symbols_set: BTreeSet<String> =
        prepared.touched_symbols.iter().cloned().collect();
    let mut retained_patch_ids = Vec::with_capacity(fresh_vp.patch_ids.len());

    for pid in &fresh_vp.patch_ids {
        if let Ok(Some(rec)) = ctx.db.get_patch(pid) {
            let norm_key = normalize_snapshot_key(&rec.patch.file_path);
            let rename_match = rec.patch.ops.iter().any(|op| match op {
                SemanticOp::RenameOrMoveFile {
                    old_path,
                    new_path,
                    ..
                } => {
                    prepared_file_keys.contains(&normalize_snapshot_key(old_path))
                        || prepared_file_keys.contains(&normalize_snapshot_key(new_path))
                }
                _ => false,
            });
            if (!norm_key.is_empty() && prepared_file_keys.contains(&norm_key)) || rename_match {
                retained_patch_ids.push(pid.clone());
                if !norm_key.is_empty() {
                    latest_wal_cas_by_file.insert(norm_key.clone(), rec.patch.cas_after);
                    touched_files_set.insert(snapshot_key_to_rel_path(&norm_key));
                }
                collect_patch_symbols(&rec.patch, &mut touched_symbols_set);
            }
        }
    }
    fresh_vp.patch_ids = retained_patch_ids;

    let mut extra_idx = 0u64;
    for patch in prepared.vp_patches {
        let norm_key = normalize_snapshot_key(&patch.file_path);
        let disk_exists = is_file_exact_case(&fresh_vp.path, &patch.file_path);
        let already_captured = match latest_wal_cas_by_file.get(&norm_key) {
            Some(wal_cas_opt) => {
                (!disk_exists && wal_cas_opt.is_none())
                    || (disk_exists && *wal_cas_opt == patch.cas_after)
            }
            None => false,
        };
        if already_captured {
            continue;
        }
        let pid = format!("patch-{}", patch.patch_hash.short_hex(16));
        if !fresh_vp.patch_ids.contains(&pid) {
            fresh_vp.patch_ids.push(pid.clone());
        }
        let rec = AstPatchRecord {
            patch_id: pid,
            lsn: base_lsn + extra_idx,
            patch,
        };
        extra_idx += 1;
        effects.push(StateEffect::UpsertPatch(rec));
    }

    touched_symbols_set.remove("<file>");
    let touched_files: Vec<PathBuf> = touched_files_set.into_iter().collect();
    let touched_symbols: Vec<String> = touched_symbols_set.into_iter().collect();
    let patch_ids = fresh_vp.patch_ids.clone();

    if let Some(ref mut proof) = verification_proof {
        proof.proof_hash = compute_verification_proof_mac(
            &meta.repo_id,
            &capsule_id,
            &fresh_vp.id,
            &patch_ids,
            proof.verify_cmd.as_deref(),
            proof.exit_code,
            proof.passed,
            proof.output_digest,
            proof.duration_ms,
            proof.verified_at_ns,
        );
    }

    let proof_cas_hash = if let Some(ref bytes) = proof_output_bytes {
        match cas.put_bytes(bytes) {
            Ok(h) => Some(h),
            Err(err) => {
                drop(seal_guard);
                let _ = revert_verifying_to_active(ctx, &fresh_vp.id).await;
                return Err(RpcError::new(RpcErrorCode::IoError, err.to_string()));
            }
        }
    } else {
        None
    };

    let cap_summary = summary
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("Seal viewport `{}` ({})", fresh_vp.name, fresh_vp.intent));

    let capsule = CausalCapsule {
        capsule_id: capsule_id.clone(),
        viewport_id: fresh_vp.id.clone(),
        agent_id: fresh_vp.agent_id.clone(),
        summary: cap_summary.clone(),
        patch_ids,
        touched_files,
        touched_symbols: touched_symbols.clone(),
        verification_proof,
        parent_capsule_ids: meta.head_capsule_id.into_iter().collect(),
        snapshot_cas_hash: Some(prepared.post_seal_snapshot_cas),
        proof_cas_hash,
        git_commit_oid: None,
        created_at_ns: now,
    };

    fresh_vp.state = ViewportState::Sealed;
    fresh_vp.updated_at_ns = now;

    effects.push(StateEffect::RecordTrunkSnapshotTransition {
        pre_snapshot_cas: prepared.pre_seal_snapshot_cas,
        post_snapshot_cas: prepared.post_seal_snapshot_cas,
    });
    effects.push(StateEffect::UpsertCapsule(capsule.clone()));
    effects.push(StateEffect::UpsertViewport(fresh_vp.clone()));

    if let Err(err) = ctx
        .writer
        .mutate(MutationRequest {
            kind: OpKind::CapsuleSeal,
            viewport_id: Some(fresh_vp.id.clone()),
            target_id: Some(capsule_id.clone()),
            capsule_id: Some(capsule_id),
            touched_symbols,
            summary: cap_summary,
            effect: StateEffect::Batch(effects),
        })
        .await
    {
        drop(seal_guard);
        let _ = revert_verifying_to_active(ctx, &fresh_vp.id).await;
        return Err(err);
    }

    drop(seal_guard);
    ctx.radar.refresh_trunk_and_reevaluate().await;

    Ok(capsule)
}

async fn revert_verifying_to_active(
    ctx: &Arc<DaemonContext>,
    viewport_id: &str,
) -> Result<(), RpcError> {
    let (tx, rx) = oneshot::channel();
    ctx.writer
        .send(WriterCommand::TransitionViewport {
            viewport_id: viewport_id.to_string(),
            target_state: ViewportState::Active,
            pid: None,
            mount_override: None,
            summary: Some("Revert failed seal verification back to Active".to_string()),
            reply: tx,
        })
        .await
        .map_err(|_| RpcError::new(RpcErrorCode::Internal, "writer actor closed"))?;
    let updated = rx
        .await
        .map_err(|_| RpcError::new(RpcErrorCode::Internal, "writer actor dropped reply"))??;
    ctx.radar.register_viewport(&updated).await;
    Ok(())
}

pub(crate) async fn collect_dirty_symbols_and_patches(
    ctx: &Arc<DaemonContext>,
    vp: &ViewportRecord,
    fallback_to_all_provides: bool,
) -> Result<(Vec<String>, Vec<Blake3Hash>, Vec<AstPatch>), RpcError> {
    let mut patch_hashes: Vec<Blake3Hash> = Vec::new();
    let mut seen_patch_hashes: BTreeSet<Blake3Hash> = BTreeSet::new();
    let mut touched_symbols_set: BTreeSet<String> = BTreeSet::new();
    let mut latest_wal_cas_by_file: BTreeMap<String, Option<Blake3Hash>> = BTreeMap::new();
    let mut wal_modified_files: BTreeSet<String> = BTreeSet::new();

    for pid in &vp.patch_ids {
        if let Ok(Some(rec)) = ctx.db.get_patch(pid) {
            if seen_patch_hashes.insert(rec.patch.patch_hash) {
                patch_hashes.push(rec.patch.patch_hash);
            }
            let norm_key = normalize_snapshot_key(&rec.patch.file_path);
            if !norm_key.is_empty() {
                latest_wal_cas_by_file.insert(norm_key.clone(), rec.patch.cas_after);
                wal_modified_files.insert(norm_key);
            }
            collect_patch_symbols(&rec.patch, &mut touched_symbols_set);
        }
    }
    touched_symbols_set.remove("<file>");

    let vp_provides = ctx
        .db
        .list_symbol_provides_for_viewport(&vp.id)
        .unwrap_or_default();

    let trunk_root = ctx.repo_root.clone();
    let vp_clone = vp.clone();
    let ignore_dirs = ctx.config.radar.ignore_dirs.clone();
    let cas_dir = ctx.loom_dir.join("cas");
    let zstd_level = ctx.config.zstd_level;
    let vp_provides_clone = vp_provides.clone();
    let wal_cas_snapshot = latest_wal_cas_by_file.clone();

    let (disk_symbols, extra_patches) = tokio::task::spawn_blocking(move || -> Result<(Vec<String>, Vec<AstPatch>), RpcError> {
        let cas = CasStore::new(cas_dir, zstd_level)
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
        let base_map = match vp_clone.base_snapshot_cas {
            Some(snap_hash) => load_workspace_snapshot(&cas, snap_hash)?,
            None => {
                let trunk_snap = capture_workspace_snapshot(&trunk_root, &ignore_dirs, &cas)?;
                load_workspace_snapshot(&cas, trunk_snap)?
            }
        };

        let vp_rels: BTreeSet<PathBuf> = collect_non_ignored_files(&vp_clone.path, &ignore_dirs)
            .into_iter()
            .collect();
        let mut all_rels: BTreeSet<PathBuf> = base_map.keys().cloned().collect();
        all_rels.extend(vp_rels.iter().cloned());

        let mut symbols_out: BTreeSet<String> = BTreeSet::new();
        let mut extra_patches_out: Vec<AstPatch> = Vec::new();
        let mut next_ts = vp_clone.lamport_ts.max(1);

        for rel in all_rels {
            let v_opt = if vp_rels.contains(&rel) {
                std::fs::read(vp_clone.path.join(&rel)).ok()
            } else {
                None
            };
            let b_hash_opt = base_map.get(&rel).copied();
            let same_as_base = match (&v_opt, b_hash_opt) {
                (None, None) => true,
                (Some(vb), Some(bh)) => Blake3Hash::from_bytes(vb) == bh,
                _ => false,
            };
            if same_as_base {
                continue;
            }

            let b_bytes = match b_hash_opt {
                Some(bh) => cas.get_bytes(&bh).unwrap_or_default(),
                None => Vec::new(),
            };
            let v_slice = v_opt.as_deref().unwrap_or(&[]);
            let fid = compute_logical_file_id(&rel);
            let lang = LanguageKind::detect(&rel, if v_slice.is_empty() { &b_bytes } else { v_slice });
            next_ts += 1;
            let ctx = DiffContext {
                logical_file_id: fid,
                file_path: &rel,
                language: lang,
                viewport_id: &vp_clone.id,
                actor_id: &vp_clone.agent_id,
                lamport_ts: next_ts,
                parents: Vec::new(),
            };

            let mut file_syms: BTreeSet<String> = BTreeSet::new();
            let norm_key = normalize_snapshot_key(&rel);

            if let Ok(mut patch) = diff_file_to_patch(&ctx, &b_bytes, v_slice, None, Some(&cas)) {
                if v_opt.is_none() {
                    patch.cas_after = None;
                    patch.finalize_hash();
                }
                collect_patch_symbols(&patch, &mut file_syms);
                file_syms.remove("<file>");

                let has_crdt_fallback = patch
                    .ops
                    .iter()
                    .any(|op| matches!(op, SemanticOp::TextCrdtFallback { .. }));

                if lang.is_code() && (file_syms.is_empty() || has_crdt_fallback) {
                    if let Ok(old_str) = std::str::from_utf8(&b_bytes)
                        && !old_str.is_empty()
                        && let Ok(parsed) = parse_source(lang, old_str, None)
                    {
                        let units = extract_decl_units(lang, &parsed.tree, old_str, fid);
                        for u in flatten_decl_units(&units) {
                            if u.anchor.symbol_fqn != "<file>" {
                                file_syms.insert(u.anchor.symbol_fqn.clone());
                            }
                        }
                    }
                    if let Ok(new_str) = std::str::from_utf8(v_slice)
                        && !new_str.is_empty()
                        && let Ok(parsed) = parse_source(lang, new_str, None)
                    {
                        let units = extract_decl_units(lang, &parsed.tree, new_str, fid);
                        for u in flatten_decl_units(&units) {
                            if u.anchor.symbol_fqn != "<file>" {
                                file_syms.insert(u.anchor.symbol_fqn.clone());
                            }
                        }
                    }
                }

                let disk_exists = v_opt.is_some();
                let already_captured = match wal_cas_snapshot.get(&norm_key) {
                    Some(wal_cas_opt) => {
                        (!disk_exists && wal_cas_opt.is_none())
                            || (disk_exists && *wal_cas_opt == patch.cas_after)
                    }
                    None => false,
                };
                if !already_captured {
                    extra_patches_out.push(patch);
                }
            }

            if file_syms.is_empty() {
                for p in &vp_provides_clone {
                    if normalize_snapshot_key(&p.file_path) == norm_key {
                        file_syms.insert(p.symbol_fqn.clone());
                    }
                }
            }

            if file_syms.is_empty() && !norm_key.is_empty() {
                file_syms.insert(format!("file:{norm_key}"));
            }
            symbols_out.extend(file_syms);
        }

        Ok((symbols_out.into_iter().collect(), extra_patches_out))
    })
    .await
    .map_err(|e| RpcError::new(RpcErrorCode::Internal, e.to_string()))??;

    for sym in disk_symbols {
        touched_symbols_set.insert(sym);
    }
    for patch in &extra_patches {
        if seen_patch_hashes.insert(patch.patch_hash) {
            patch_hashes.push(patch.patch_hash);
        }
    }
    if touched_symbols_set.is_empty() {
        if fallback_to_all_provides {
            for p in &vp_provides {
                touched_symbols_set.insert(p.symbol_fqn.clone());
            }
        }
        if touched_symbols_set.is_empty() {
            for f in wal_modified_files {
                touched_symbols_set.insert(format!("file:{f}"));
            }
        }
        if touched_symbols_set.is_empty() {
            touched_symbols_set.insert("__repo__".to_string());
        }
    }

    Ok((
        touched_symbols_set.into_iter().collect(),
        patch_hashes,
        extra_patches,
    ))
}

pub async fn drop_viewport(
    ctx: &Arc<DaemonContext>,
    viewport_id: &str,
    reason: String,
    kind: Option<String>,
) -> Result<DropResult, RpcError> {
    let trimmed = reason.trim();
    if trimmed.is_empty() {
        return Err(RpcError::new(
            RpcErrorCode::InvalidRequest,
            "reason cannot be empty",
        ));
    }
    if let Err(e) = NegativeEdge::validate_lesson(&reason) {
        return Err(RpcError::new(RpcErrorCode::InvalidRequest, e.to_string()));
    }

    let failure_kind = match kind.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => raw
            .parse::<FailureKind>()
            .map_err(|msg| RpcError::new(RpcErrorCode::InvalidRequest, msg))?,
        None => FailureKind::Other,
    };

    let _ = ctx
        .db
        .get_repo_meta()
        .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
        .ok_or_else(|| {
            RpcError::new(
                RpcErrorCode::NotInitialized,
                "Loom repository is not initialized; run `loom init` first",
            )
        })?;

    let initial_vp = ctx
        .db
        .resolve_viewport(viewport_id)
        .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
        .ok_or_else(|| {
            RpcError::new(
                RpcErrorCode::NotFound,
                format!("viewport `{viewport_id}` not found"),
            )
        })?;

    if initial_vp.state == ViewportState::Verifying {
        return Err(RpcError::new(
            RpcErrorCode::Conflict,
            format!(
                "cannot drop viewport `{}` while verifying/sealing",
                initial_vp.name
            ),
        ));
    }
    if !initial_vp.state.is_active() {
        return Err(RpcError::new(
            RpcErrorCode::InvalidState,
            format!(
                "cannot drop viewport `{}` in state {}",
                initial_vp.name, initial_vp.state
            ),
        ));
    }

    let seal_guard = ctx.trunk_seal_lock.lock().await;

    // Flush any un-debounced edits before reading WAL patches and diffing
    ctx.radar.flush_viewport(&initial_vp.id).await;

    let vp = ctx
        .db
        .get_viewport(&initial_vp.id)
        .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
        .ok_or_else(|| {
            RpcError::new(
                RpcErrorCode::NotFound,
                format!("viewport `{}` no longer exists", initial_vp.id),
            )
        })?;

    if vp.state == ViewportState::Verifying {
        return Err(RpcError::new(
            RpcErrorCode::Conflict,
            format!("cannot drop viewport `{}` while verifying/sealing", vp.name),
        ));
    }
    if !vp.state.is_active() {
        return Err(RpcError::new(
            RpcErrorCode::InvalidState,
            format!("cannot drop viewport `{}` in state {}", vp.name, vp.state),
        ));
    }

    let (touched_symbols, patch_hashes, extra_patches) =
        collect_dirty_symbols_and_patches(ctx, &vp, true).await?;

    // Unregister from Radar BEFORE touching disk on Windows (releases ReadDirectoryChangesW handles)
    ctx.radar.unregister_viewport(&vp.id).await;

    let trunk_root = ctx.repo_root.clone();
    let vp_path = vp.path.clone();
    let mount = vp.mount.clone();
    let backend = vp.driver;
    let cache_dirs = ctx.config.vfs.cache_dirs.clone();
    let ignore_dirs = ctx.config.radar.ignore_dirs.clone();
    let cas_dir = ctx.loom_dir.join("cas");
    let zstd_level = ctx.config.zstd_level;

    let restore_res = tokio::task::spawn_blocking(move || -> Result<Blake3Hash, RpcError> {
        let cas = CasStore::new(cas_dir, zstd_level)
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
        let post_clean_snapshot = capture_workspace_snapshot(&trunk_root, &ignore_dirs, &cas)?;
        let drv = loom_vfs::driver_for_backend(backend, &cache_dirs);
        let cache_refs: Vec<&str> = cache_dirs.iter().map(String::as_str).collect();
        drv.restore_clean_with_context(&trunk_root, &vp_path, &cache_refs, mount.as_ref())
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
        let stale_radar = vp_path.join(".loom").join("RADAR_STATUS.json");
        if stale_radar.is_file() {
            let _ = std::fs::remove_file(&stale_radar);
        }
        Ok(post_clean_snapshot)
    })
    .await
    .map_err(|e| RpcError::new(RpcErrorCode::Internal, e.to_string()))
    .and_then(|r| r);

    let post_clean_snapshot = match restore_res {
        Ok(snap) => snap,
        Err(err) => {
            drop(seal_guard);
            ctx.radar.register_viewport(&vp).await;
            return Err(err);
        }
    };

    let edge = NegativeEdge {
        edge_id: format!("ne-{}", uuid::Uuid::new_v4()),
        edge_seq: 0,
        viewport_id: vp.id.clone(),
        intent: vp.intent.clone(),
        touched_symbols,
        failure_kind,
        lesson_summary: reason,
        patch_hashes,
        source: "loom_drop".to_string(),
        created_at_ns: now_ns(),
    };

    let (tx, rx) = oneshot::channel();
    if ctx
        .writer
        .send(WriterCommand::DropViewport {
            viewport_id: vp.id.clone(),
            edge: Box::new(edge),
            extra_patches,
            post_clean_snapshot: Some(post_clean_snapshot),
            reply: tx,
        })
        .await
        .is_err()
    {
        drop(seal_guard);
        return Err(RpcError::new(RpcErrorCode::Internal, "writer actor closed"));
    }

    let (updated_vp, committed_edge) = rx
        .await
        .map_err(|_| RpcError::new(RpcErrorCode::Internal, "writer actor dropped reply"))??;

    drop(seal_guard);
    ctx.radar.reevaluate_all().await;

    Ok(DropResult {
        viewport: to_summary(updated_vp),
        negative_edge: committed_edge,
    })
}

pub(crate) fn extract_trunk_snapshot_transition(
    inverse: &InverseEffect,
) -> Option<(Blake3Hash, Blake3Hash)> {
    match inverse {
        InverseEffect::RestoreTrunkSnapshot {
            pre_snapshot_cas,
            post_snapshot_cas,
        } => Some((*pre_snapshot_cas, *post_snapshot_cas)),
        InverseEffect::Batch(items) => items
            .iter()
            .rev()
            .find_map(extract_trunk_snapshot_transition),
        _ => None,
    }
}

fn is_safe_trunk_rel(rel: &Path) -> bool {
    let mut comps = rel.components();
    let Some(first) = comps.next() else {
        return false;
    };
    if let Component::Normal(os) = first {
        if os == ".loom" || os == ".git" {
            return false;
        }
    } else {
        return false;
    }
    comps.all(|c| matches!(c, Component::Normal(_)))
}

pub(crate) fn collect_inverse_viewport_effects(
    inverse: &InverseEffect,
    deletes: &mut Vec<String>,
    restores: &mut Vec<ViewportRecord>,
) {
    match inverse {
        InverseEffect::DeleteViewport(id) => deletes.push(id.clone()),
        InverseEffect::RestoreViewport(vp) => restores.push(vp.clone()),
        InverseEffect::Batch(items) => {
            for item in items {
                collect_inverse_viewport_effects(item, deletes, restores);
            }
        }
        _ => {}
    }
}

fn paths_occupy_same_dir(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

pub(crate) fn inverse_has_verifying_restore(inverse: &InverseEffect) -> bool {
    match inverse {
        InverseEffect::RestoreViewport(vp) => vp.state == ViewportState::Verifying,
        InverseEffect::Batch(items) => items.iter().any(inverse_has_verifying_restore),
        _ => false,
    }
}

pub(crate) fn effect_transitions_to_verifying(effect: &StateEffect) -> bool {
    match effect {
        StateEffect::UpsertViewport(vp) => vp.state == ViewportState::Verifying,
        StateEffect::Batch(items) => items.iter().any(effect_transitions_to_verifying),
        _ => false,
    }
}

pub(crate) fn validate_undo_target(
    db: &loom_core::db::StateDb,
    target: &loom_core::model::OpLogEntry,
    inverse: &InverseEffect,
) -> Result<(), RpcError> {
    if target.kind == OpKind::CapsuleSeal
        && let Some(cap_id) = target
            .capsule_id
            .as_deref()
            .or(target.target_id.as_deref())
    {
        let pushed_in_cap = db
            .get_capsule(cap_id)
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
            .is_some_and(|c| c.git_commit_oid.is_some());
        let pushed_in_map = db
            .get_git_map(&format!("cap:{cap_id}"))
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
            .is_some();
        if pushed_in_cap || pushed_in_map {
            return Err(RpcError::new(
                RpcErrorCode::Conflict,
                format!(
                    "cannot undo capsule seal op #{}: capsule `{cap_id}` has already been pushed to git",
                    target.op_seq
                ),
            ));
        }
    }

    let trunk_transition = extract_trunk_snapshot_transition(inverse);
    if (target.kind == OpKind::CapsuleSeal || trunk_transition.is_some())
        && let Some(later) = db
            .later_unundone_trunk_op(target.op_seq)
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
    {
        return Err(RpcError::new(
            RpcErrorCode::Conflict,
            format!(
                "cannot undo capsule seal op #{} while later trunk op #{} is still applied; undo later ops first",
                target.op_seq, later.op_seq
            ),
        ));
    }

    let mut delete_vp_ids = Vec::new();
    let mut restore_vps = Vec::new();
    collect_inverse_viewport_effects(inverse, &mut delete_vp_ids, &mut restore_vps);
    if delete_vp_ids.is_empty() && restore_vps.is_empty() {
        return Ok(());
    }

    let all_vps = db
        .list_viewports(true)
        .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;

    for id in &delete_vp_ids {
        if let Some(later) = db
            .later_unundone_undoable_viewport_op(target.op_seq, id)
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
        {
            return Err(RpcError::new(
                RpcErrorCode::Conflict,
                format!(
                    "cannot undo op #{}: viewport `{id}` has later un-undone op #{}; undo later ops first",
                    target.op_seq, later.op_seq
                ),
            ));
        }
        if let Some(current) = all_vps.iter().find(|v| v.id == *id)
            && (matches!(
                current.state,
                ViewportState::Sealed | ViewportState::Verifying | ViewportState::GcPending
            ) || current.pruned_at_ns.is_some())
        {
            return Err(RpcError::new(
                RpcErrorCode::InvalidState,
                format!(
                    "cannot undo op #{}: viewport `{}` is in non-undoable state {}",
                    target.op_seq, current.name, current.state
                ),
            ));
        }
    }

    for vp in &restore_vps {
        if vp.state == ViewportState::Verifying {
            return Err(RpcError::new(
                RpcErrorCode::InvalidState,
                format!(
                    "cannot undo op #{}: cannot restore viewport `{}` into transient Verifying state",
                    target.op_seq, vp.name
                ),
            ));
        }
        let current = all_vps.iter().find(|v| v.id == vp.id);
        if let Some(cur) = current
            && cur.state == ViewportState::Verifying
        {
            return Err(RpcError::new(
                RpcErrorCode::Conflict,
                format!(
                    "cannot undo op #{}: viewport `{}` is currently verifying/sealing",
                    target.op_seq, cur.name
                ),
            ));
        }
        let is_pruned_or_gc = current
            .is_some_and(|cur| cur.state == ViewportState::GcPending || cur.pruned_at_ns.is_some());
        if is_pruned_or_gc && (vp.state.is_active() || target.kind != OpKind::CapsuleSeal) {
            return Err(RpcError::new(
                RpcErrorCode::InvalidState,
                format!(
                    "cannot undo op #{}: viewport `{}` was already garbage-collected/pruned",
                    target.op_seq, vp.name
                ),
            ));
        }
        if target.kind != OpKind::CapsuleSeal {
            if let Some(cur) = current {
                if matches!(cur.state, ViewportState::Sealed | ViewportState::Dropped)
                    && target.kind != OpKind::ViewportDrop
                {
                    return Err(RpcError::new(
                        RpcErrorCode::InvalidState,
                        format!(
                            "cannot undo op #{}: viewport `{}` is already {} and cannot be resurrected",
                            target.op_seq, vp.name, cur.state
                        ),
                    ));
                }
                if target.kind == OpKind::ViewportDrop && cur.state != ViewportState::Dropped {
                    return Err(RpcError::new(
                        RpcErrorCode::InvalidState,
                        format!(
                            "cannot undo drop op #{}: viewport `{}` is in state {} (expected Dropped)",
                            target.op_seq, vp.name, cur.state
                        ),
                    ));
                }
            }
            if let Some(later) = db
                .later_unundone_undoable_viewport_op(target.op_seq, &vp.id)
                .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
            {
                return Err(RpcError::new(
                    RpcErrorCode::Conflict,
                    format!(
                        "cannot undo op #{}: viewport `{}` has later un-undone op #{}; undo later ops first",
                        target.op_seq, vp.name, later.op_seq
                    ),
                ));
            }
            if let Some(clean_op) = db
                .later_destructive_clean_for_viewport(target.op_seq, &vp.id)
                .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
            {
                return Err(RpcError::new(
                    RpcErrorCode::Conflict,
                    format!(
                        "cannot undo op #{}: viewport `{}` was destructively cleaned by later op #{} ({}); pre-clean state cannot be restored",
                        target.op_seq, vp.name, clean_op.op_seq, clean_op.summary
                    ),
                ));
            }
        }
        if vp.state.is_active() {
            if !vp.path.is_dir() {
                return Err(RpcError::new(
                    RpcErrorCode::InvalidState,
                    format!(
                        "cannot undo op #{}: viewport `{}` directory `{}` no longer exists",
                        target.op_seq,
                        vp.name,
                        vp.path.display()
                    ),
                ));
            }
            if let Some(other) = all_vps.iter().find(|other| {
                other.id != vp.id
                    && other.state.is_active()
                    && paths_occupy_same_dir(&other.path, &vp.path)
            }) {
                return Err(RpcError::new(
                    RpcErrorCode::Conflict,
                    format!(
                        "cannot undo op #{}: viewport path `{}` is already occupied by active viewport `{}`",
                        target.op_seq,
                        vp.path.display(),
                        other.name
                    ),
                ));
            }
        }
    }
    Ok(())
}

struct RollbackSnapshotRequest<'a> {
    root: &'a Path,
    pre_snapshot_cas: Blake3Hash,
    post_snapshot_cas: Blake3Hash,
    cas_dir: &'a Path,
    zstd_level: i32,
    ignore_dirs: &'a [String],
    op_seq: u64,
    workspace_label: &'a str,
}

fn rollback_workspace_snapshot(req: RollbackSnapshotRequest<'_>) -> Result<(), RpcError> {
    let RollbackSnapshotRequest {
        root,
        pre_snapshot_cas,
        post_snapshot_cas,
        cas_dir,
        zstd_level,
        ignore_dirs,
        op_seq,
        workspace_label,
    } = req;
    let cas = CasStore::new(cas_dir.to_path_buf(), zstd_level)
        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
    let pre_map = load_workspace_snapshot(&cas, pre_snapshot_cas)?;
    let post_map = load_workspace_snapshot(&cas, post_snapshot_cas)?;
    let disk_rels: BTreeSet<PathBuf> = collect_non_ignored_files(root, ignore_dirs)
        .into_iter()
        .collect();

    #[cfg(windows)]
    let disk_by_lower: BTreeMap<String, Vec<PathBuf>> = {
        let mut map: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
        for dr in &disk_rels {
            map.entry(normalize_snapshot_key(dr).to_lowercase())
                .or_default()
                .push(dr.clone());
        }
        map
    };

    let mut all_rels: BTreeSet<PathBuf> = pre_map.keys().cloned().collect();
    all_rels.extend(post_map.keys().cloned());

    let mut staged_writes: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    let mut staged_deletes: Vec<PathBuf> = Vec::new();

    for rel in &all_rels {
        if !is_safe_trunk_rel(rel) {
            continue;
        }
        let pre_h = pre_map.get(rel).copied();
        let post_h = post_map.get(rel).copied();
        if pre_h == post_h {
            continue;
        }
        let exact_on_disk = disk_rels.contains(rel)
            || (is_ignored_relative_path(rel, ignore_dirs) && is_file_exact_case(root, rel));
        let cur_h = if exact_on_disk {
            let target_path = root.join(rel);
            match std::fs::symlink_metadata(&target_path) {
                Ok(meta) if meta.is_file() => {
                    let bytes = std::fs::read(&target_path)
                        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
                    Some(Blake3Hash::from_bytes(&bytes))
                }
                Ok(_) => None,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    None
                }
                Err(e) => return Err(RpcError::new(RpcErrorCode::IoError, e.to_string())),
            }
        } else {
            #[cfg(windows)]
            {
                let folded = normalize_snapshot_key(rel).to_lowercase();
                if let Some(collisions) = disk_by_lower.get(&folded)
                    && let Some(unmanaged) = collisions.iter().find(|dr| !all_rels.contains(*dr))
                {
                    return Err(RpcError::new(
                        RpcErrorCode::Conflict,
                        format!(
                            "cannot undo op #{op_seq}: {workspace_label} file `{}` was modified externally",
                            unmanaged.display()
                        ),
                    ));
                }
            }
            None
        };
        if cur_h != post_h && cur_h != pre_h {
            return Err(RpcError::new(
                RpcErrorCode::Conflict,
                format!(
                    "cannot undo op #{op_seq}: {workspace_label} file `{}` was modified externally",
                    rel.display()
                ),
            ));
        }
        if let Some(h) = pre_h {
            let bytes = cas
                .get_bytes(&h)
                .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
            staged_writes.push((rel.clone(), bytes));
        } else if exact_on_disk {
            staged_deletes.push(rel.clone());
        }
    }

    crate::viewport_mgr::apply_workspace_deletes_and_writes(
        root,
        &staged_deletes,
        staged_writes.iter().map(|(p, b)| (p, b.as_slice())),
    )?;
    Ok(())
}

pub async fn undo_op(
    ctx: &Arc<DaemonContext>,
    op_seq: Option<u64>,
) -> Result<UndoResult, RpcError> {
    let _seal_guard = ctx.trunk_seal_lock.lock().await;

    let _ = ctx
        .db
        .get_repo_meta()
        .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
        .ok_or_else(|| {
            RpcError::new(
                RpcErrorCode::NotInitialized,
                "Loom repository is not initialized; run `loom init` first",
            )
        })?;

    let target = match op_seq {
        Some(seq) => ctx
            .db
            .get_op(seq)
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
            .ok_or_else(|| {
                RpcError::new(RpcErrorCode::NotFound, format!("op_seq {seq} not found"))
            })?,
        None => ctx
            .db
            .latest_undoable_op()
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
            .ok_or_else(|| {
                RpcError::new(RpcErrorCode::NotFound, "no undoable operation found")
            })?,
    };

    if target.kind == OpKind::Undo {
        return Err(RpcError::new(
            RpcErrorCode::InvalidState,
            format!("op_seq {} is already an Undo operation", target.op_seq),
        ));
    }
    if target.undone_by_op.is_some() {
        return Err(RpcError::new(
            RpcErrorCode::InvalidState,
            format!("op_seq {} has already been undone", target.op_seq),
        ));
    }
    let inv_bytes = target.inverse_payload.as_ref().ok_or_else(|| {
        RpcError::new(
            RpcErrorCode::InvalidState,
            format!("op_seq {} does not support undo", target.op_seq),
        )
    })?;
    let inverse: InverseEffect = decode_value(inv_bytes)
        .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;

    let mut delete_vp_ids = Vec::new();
    let mut restore_vps = Vec::new();
    collect_inverse_viewport_effects(&inverse, &mut delete_vp_ids, &mut restore_vps);

    if target.kind == OpKind::ViewportTransition {
        for old_vp in &restore_vps {
            if let Ok(Some(cur_vp)) = ctx.db.get_viewport(&old_vp.id)
                && cur_vp.state.is_active()
                && old_vp.state.is_active()
                && cur_vp.base_snapshot_cas != old_vp.base_snapshot_cas
            {
                ctx.radar.flush_viewport(&cur_vp.id).await;
            }
        }
    }

    validate_undo_target(&ctx.db, &target, &inverse)?;

    let mut pre_undo_vps: HashMap<String, ViewportRecord> = HashMap::new();
    for vp in &restore_vps {
        if let Ok(Some(cur)) = ctx.db.get_viewport(&vp.id) {
            pre_undo_vps.insert(cur.id.clone(), cur);
        }
    }

    let mut vps_to_teardown = Vec::new();
    for id in &delete_vp_ids {
        if let Ok(Some(vp)) = ctx.db.get_viewport(id) {
            vps_to_teardown.push(vp);
        }
    }

    let trunk_transition = extract_trunk_snapshot_transition(&inverse);
    if let Some((pre_snapshot_cas, post_snapshot_cas)) = trunk_transition {
        let repo_root = ctx.repo_root.clone();
        let cas_dir = ctx.loom_dir.join("cas");
        let zstd_level = ctx.config.zstd_level;
        let ignore_dirs = ctx.config.radar.ignore_dirs.clone();
        let op_seq = target.op_seq;

        tokio::task::spawn_blocking(move || -> Result<(), RpcError> {
            rollback_workspace_snapshot(RollbackSnapshotRequest {
                root: &repo_root,
                pre_snapshot_cas,
                post_snapshot_cas,
                cas_dir: &cas_dir,
                zstd_level,
                ignore_dirs: &ignore_dirs,
                op_seq,
                workspace_label: "trunk",
            })
        })
        .await
        .map_err(|e| RpcError::new(RpcErrorCode::Internal, e.to_string()))??;
    }

    let mut suppressed_vp_ids: HashSet<String> = HashSet::new();
    if target.kind == OpKind::ViewportTransition {
        let mut vp_rollbacks: Vec<(String, PathBuf, Blake3Hash, Blake3Hash)> = Vec::new();
        for old_vp in &restore_vps {
            if let Some(cur_vp) = pre_undo_vps.get(&old_vp.id)
                && cur_vp.state.is_active()
                && old_vp.state.is_active()
                && let (Some(pre_snap), Some(post_snap)) =
                    (old_vp.base_snapshot_cas, cur_vp.base_snapshot_cas)
                && pre_snap != post_snap
            {
                vp_rollbacks.push((cur_vp.id.clone(), cur_vp.path.clone(), pre_snap, post_snap));
            }
        }
        for (vp_id, vp_path, pre_snap, post_snap) in vp_rollbacks {
            ctx.radar.set_suppressed(&vp_id, true).await;
            suppressed_vp_ids.insert(vp_id.clone());
            let cas_dir = ctx.loom_dir.join("cas");
            let zstd_level = ctx.config.zstd_level;
            let ignore_dirs = ctx.config.radar.ignore_dirs.clone();
            let op_seq = target.op_seq;
            let rollback_res = tokio::task::spawn_blocking(move || -> Result<(), RpcError> {
                rollback_workspace_snapshot(RollbackSnapshotRequest {
                    root: &vp_path,
                    pre_snapshot_cas: pre_snap,
                    post_snapshot_cas: post_snap,
                    cas_dir: &cas_dir,
                    zstd_level,
                    ignore_dirs: &ignore_dirs,
                    op_seq,
                    workspace_label: "viewport",
                })
            })
            .await
            .map_err(|e| RpcError::new(RpcErrorCode::Internal, e.to_string()))
            .and_then(|r| r);
            if let Err(err) = rollback_res {
                for id in &suppressed_vp_ids {
                    ctx.radar.set_suppressed(id, false).await;
                }
                return Err(err);
            }
        }
    }

    let res = match ctx.writer.undo(Some(target.op_seq)).await {
        Ok(r) => r,
        Err(err) => {
            for id in &suppressed_vp_ids {
                ctx.radar.set_suppressed(id, false).await;
            }
            return Err(err);
        }
    };

    let viewports_dir = ctx.loom_dir.join("viewports");
    let remaining_vps = ctx.db.list_viewports(true).unwrap_or_default();
    for vp in vps_to_teardown {
        ctx.radar.unregister_viewport(&vp.id).await;
        let path_still_used = remaining_vps.iter().any(|other| other.path == vp.path);
        if !path_still_used && vp.path.starts_with(&viewports_dir) && vp.path.exists() {
            let vp_path = vp.path.clone();
            let mount = vp.mount.clone();
            let driver = vp.driver;
            let cache_dirs = ctx.config.vfs.cache_dirs.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let drv = loom_vfs::driver_for_backend(driver, &cache_dirs);
                let _ = drv.teardown_viewport(&vp_path, mount.as_ref());
            })
            .await;
        }
    }
    for id in &delete_vp_ids {
        ctx.radar.unregister_viewport(id).await;
    }

    for vp in restore_vps {
        let cur = ctx.db.get_viewport(&vp.id).ok().flatten().unwrap_or(vp);
        if cur.state.is_active() {
            let needs_reset = target.kind == OpKind::NegativeEdgeRecord
                || suppressed_vp_ids.contains(&cur.id)
                || pre_undo_vps
                    .get(&cur.id)
                    .is_some_and(|prev| prev.base_snapshot_cas != cur.base_snapshot_cas);
            ctx.radar.register_viewport(&cur).await;
            if needs_reset {
                ctx.radar.reset_viewport_clean(&cur.id).await;
            }
        } else {
            ctx.radar.unregister_viewport(&cur.id).await;
        }
    }

    if trunk_transition.is_some() {
        ctx.radar.refresh_trunk_and_reevaluate().await;
    }

    Ok(res)
}
