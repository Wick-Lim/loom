use crate::radar::collect_non_ignored_files;
use crate::{WriterCommand, DaemonContext};
use loom_core::cas::CasStore;
use loom_core::cst::{
    DiffContext, MergedFileContent, collect_syntax_errors, compute_logical_file_id,
    diff_file_to_patch, diff_file_to_patch_strict, extract_decl_units, merge_patches, parse_source,
};
use loom_core::db::symbol_matches_query;
use loom_core::model::{
    AstPatch, Blake3Hash, FailureKind, LanguageKind, MergeOutcome,
    NegativeEdge, OpKind, SemanticOp, SymbolContractDelta, ViewportRecord,
    ViewportState, WorkspaceSnapshot, now_ns,
};
use loom_core::rpc::{
    McpPreflightResult, RpcError, RpcErrorCode, ViewportSummary,
};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use tokio::sync::oneshot;

pub use crate::capsule::seal_capsule;

pub fn to_summary(vp: ViewportRecord) -> ViewportSummary {
    ViewportSummary {
        id: vp.id,
        name: vp.name,
        intent: vp.intent,
        agent_id: vp.agent_id,
        state: vp.state,
        path: vp.path,
        driver: vp.driver,
        pid: vp.pid,
        mount: vp.mount,
        base_op_seq: vp.base_op_seq,
        head_lsn: vp.head_lsn,
        created_at_ns: vp.created_at_ns,
        updated_at_ns: vp.updated_at_ns,
    }
}

pub(crate) fn normalize_snapshot_key(rel: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for comp in rel.components() {
        if let Component::Normal(s) = comp {
            parts.push(s.to_string_lossy().into_owned());
        }
    }
    parts.join("/")
}

pub(crate) fn snapshot_key_to_rel_path(key: &str) -> PathBuf {
    key.split('/').filter(|s| !s.is_empty()).collect()
}

pub(crate) fn capture_workspace_snapshot(
    root: &Path,
    ignore_dirs: &[String],
    cas: &CasStore,
) -> Result<Blake3Hash, RpcError> {
    let rels = collect_non_ignored_files(root, ignore_dirs);
    let mut files = BTreeMap::new();
    for rel in rels {
        let abs = root.join(&rel);
        let bytes = match std::fs::read(&abs) {
            Ok(b) => b,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(RpcError::new(
                    RpcErrorCode::IoError,
                    format!("failed to read `{}` for snapshot: {err}", abs.display()),
                ));
            }
        };
        let hash = cas
            .put_bytes(&bytes)
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
        let key = normalize_snapshot_key(&rel);
        if !key.is_empty() {
            files.insert(key, hash);
        }
    }
    let snapshot = WorkspaceSnapshot { files };
    let encoded = rmp_serde::to_vec_named(&snapshot)
        .map_err(|e| RpcError::new(RpcErrorCode::Internal, e.to_string()))?;
    cas.put_bytes(&encoded)
        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))
}

pub(crate) fn load_workspace_snapshot(
    cas: &CasStore,
    snapshot_hash: Blake3Hash,
) -> Result<BTreeMap<PathBuf, Blake3Hash>, RpcError> {
    let raw = cas
        .get_bytes(&snapshot_hash)
        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
    let snapshot: WorkspaceSnapshot = rmp_serde::from_slice(&raw)
        .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
    let mut out = BTreeMap::new();
    for (k, h) in snapshot.files {
        let rel = snapshot_key_to_rel_path(&k);
        if !rel.as_os_str().is_empty() {
            out.insert(rel, h);
        }
    }
    Ok(out)
}

pub(crate) fn collect_patch_symbols(patch: &AstPatch, out: &mut BTreeSet<String>) {
    for delta in &patch.contract_deltas {
        match delta {
            SymbolContractDelta::AddedSymbol { symbol_fqn, .. }
            | SymbolContractDelta::RemovedSymbol { symbol_fqn, .. }
            | SymbolContractDelta::SignatureChanged { symbol_fqn, .. }
            | SymbolContractDelta::VisibilityReduced { symbol_fqn, .. } => {
                out.insert(symbol_fqn.clone());
            }
        }
    }
    for op in &patch.ops {
        match op {
            SemanticOp::InsertMember {
                parent_anchor,
                member_anchor,
                ..
            } => {
                out.insert(parent_anchor.symbol_fqn.clone());
                out.insert(member_anchor.symbol_fqn.clone());
            }
            SemanticOp::ReplaceBody { target, .. }
            | SemanticOp::MutateSignature { target, .. }
            | SemanticOp::RemoveNode { target, .. } => {
                out.insert(target.symbol_fqn.clone());
            }
            SemanticOp::RenameOrMoveFile { .. } | SemanticOp::TextCrdtFallback { .. } => {}
        }
    }
}

pub fn validate_viewport_name(name: &str) -> Result<(), RpcError> {
    if name.is_empty() {
        return Err(RpcError::new(
            RpcErrorCode::InvalidRequest,
            "viewport name cannot be empty",
        ));
    }
    if name.chars().any(char::is_whitespace) {
        return Err(RpcError::new(
            RpcErrorCode::InvalidRequest,
            format!("viewport name `{name}` cannot contain whitespace"),
        ));
    }
    if name == "." || name == ".." || name.starts_with('.') {
        return Err(RpcError::new(
            RpcErrorCode::InvalidRequest,
            format!("viewport name `{name}` cannot start with `.` or be `.`/`..`"),
        ));
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        return Err(RpcError::new(
            RpcErrorCode::InvalidRequest,
            format!("viewport name `{name}` cannot contain path separators or NUL"),
        ));
    }
    if name
        .chars()
        .any(|c| c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
    {
        return Err(RpcError::new(
            RpcErrorCode::InvalidRequest,
            format!("viewport name `{name}` contains invalid filename characters"),
        ));
    }
    let upper = name.to_ascii_uppercase();
    let stem = upper.split('.').next().unwrap_or(&upper);
    if matches!(
        stem,
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    ) {
        return Err(RpcError::new(
            RpcErrorCode::InvalidRequest,
            format!("viewport name `{name}` uses a reserved OS device name"),
        ));
    }
    Ok(())
}

pub async fn spawn_viewport(
    ctx: &Arc<DaemonContext>,
    name: String,
    intent: String,
    _cmd: Vec<String>,
) -> Result<ViewportSummary, RpcError> {
    validate_viewport_name(&name)?;
    let _meta = ctx
        .db
        .get_repo_meta()
        .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
        .ok_or_else(|| {
            RpcError::new(
                RpcErrorCode::NotInitialized,
                "Loom repository is not initialized; run `loom init` first",
            )
        })?;

    let viewport_root = ctx.loom_dir.join("viewports").join(&name);
    if viewport_root.exists() {
        return Err(RpcError::new(
            RpcErrorCode::Conflict,
            format!(
                "viewport directory `{}` already exists on disk (run `loom gc --prune-viewports` if obsolete)",
                viewport_root.display()
            ),
        ));
    }

    let seal_guard = ctx.trunk_seal_lock.lock().await;

    let meta = ctx
        .db
        .get_repo_meta()
        .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
        .ok_or_else(|| {
            RpcError::new(
                RpcErrorCode::NotInitialized,
                "Loom repository is not initialized; run `loom init` first",
            )
        })?;

    let snap_root = ctx.repo_root.clone();
    let snap_ignore = ctx.config.radar.ignore_dirs.clone();
    let snap_cas_dir = ctx.loom_dir.join("cas");
    let snap_zstd = ctx.config.zstd_level;
    let base_snapshot_cas = tokio::task::spawn_blocking(move || {
        let cas = CasStore::new(snap_cas_dir, snap_zstd)
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
        capture_workspace_snapshot(&snap_root, &snap_ignore, &cas)
    })
    .await
    .map_err(|e| RpcError::new(RpcErrorCode::Internal, e.to_string()))??;

    let driver = loom_vfs::select_driver_with_config(
        ctx.config.vfs.driver,
        &ctx.repo_root,
        &ctx.config.vfs.cache_dirs,
    );
    let backend = driver.backend_kind();
    let now = now_ns();
    let vp_id = format!("vp-{}", uuid::Uuid::new_v4());
    let vp_record = ViewportRecord {
        id: vp_id.clone(),
        name: name.clone(),
        intent,
        agent_id: "cli".to_string(),
        state: ViewportState::Spawned,
        path: viewport_root.clone(),
        driver: backend,
        mount: None,
        pid: None,
        base_op_seq: 0,
        base_capsule_id: meta.head_capsule_id.clone(),
        base_snapshot_cas: Some(base_snapshot_cas),
        head_cas: None,
        head_lsn: 0,
        lamport_ts: 0,
        patch_ids: Vec::new(),
        created_at_ns: now,
        updated_at_ns: now,
        pruned_at_ns: None,
    };

    let (tx, rx) = oneshot::channel();
    ctx.writer
        .send(WriterCommand::BeginSpawnViewport {
            vp: Box::new(vp_record),
            reply: tx,
        })
        .await
        .map_err(|_| RpcError::new(RpcErrorCode::Internal, "writer actor closed"))?;
    let (_spawn_lsn, spawn_op_seq, spawned_vp) = rx
        .await
        .map_err(|_| RpcError::new(RpcErrorCode::Internal, "writer actor dropped reply"))??;

    let trunk_root = ctx.repo_root.clone();
    let vp_root_clone = viewport_root.clone();
    let cache_dirs = ctx.config.vfs.cache_dirs.clone();
    let vfs_pref = ctx.config.vfs.driver;

    let mat_res = tokio::task::spawn_blocking(move || {
        let drv = loom_vfs::select_driver_with_config(vfs_pref, &trunk_root, &cache_dirs);
        let cache_refs: Vec<&str> = cache_dirs.iter().map(String::as_str).collect();
        drv.materialize_viewport(&trunk_root, &vp_root_clone, &cache_refs)
    })
    .await;

    drop(seal_guard);

    let mount = match mat_res {
        Ok(Ok(m)) => m,
        Ok(Err(err)) => {
            rollback_failed_spawn(ctx, &spawned_vp, spawn_op_seq).await;
            return Err(RpcError::new(
                RpcErrorCode::IoError,
                format!("failed to materialize viewport `{name}`: {err}"),
            ));
        }
        Err(join_err) => {
            rollback_failed_spawn(ctx, &spawned_vp, spawn_op_seq).await;
            return Err(RpcError::new(
                RpcErrorCode::Internal,
                format!("viewport materialization task panicked: {join_err}"),
            ));
        }
    };

    let (tx, rx) = oneshot::channel();
    ctx.writer
        .send(WriterCommand::TransitionViewport {
            viewport_id: spawned_vp.id.clone(),
            target_state: ViewportState::Active,
            pid: None,
            mount_override: Some(Some(mount)),
            summary: Some(format!("Activate viewport `{name}`")),
            reply: tx,
        })
        .await
        .map_err(|_| RpcError::new(RpcErrorCode::Internal, "writer actor closed"))?;
    let active_vp = rx
        .await
        .map_err(|_| RpcError::new(RpcErrorCode::Internal, "writer actor dropped reply"))??;

    ctx.radar.register_viewport(&active_vp).await;

    Ok(to_summary(active_vp))
}

async fn rollback_failed_spawn(
    ctx: &Arc<DaemonContext>,
    vp: &ViewportRecord,
    spawn_op_seq: u64,
) {
    let vp_path = vp.path.clone();
    let backend = vp.driver;
    let cache_dirs = ctx.config.vfs.cache_dirs.clone();
    let _ = tokio::task::spawn_blocking(move || {
        let drv = loom_vfs::driver_for_backend(backend, &cache_dirs);
        let _ = drv.teardown_viewport(&vp_path, None);
    })
    .await;
    let _ = ctx.writer.undo(Some(spawn_op_seq)).await;
}

pub async fn rollback_failed_spawn_viewport(
    ctx: &Arc<DaemonContext>,
    viewport_id: &str,
) -> Result<ViewportSummary, RpcError> {
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

    let mut vp = ctx
        .db
        .get_viewport(viewport_id)
        .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
        .ok_or_else(|| {
            RpcError::new(
                RpcErrorCode::NotFound,
                format!("viewport `{viewport_id}` not found"),
            )
        })?;

    if vp.state == ViewportState::Sealed {
        return Err(RpcError::new(
            RpcErrorCode::InvalidState,
            format!(
                "cannot rollback failed spawn for viewport `{}` in state {}",
                vp.name, vp.state
            ),
        ));
    }

    if matches!(
        vp.state,
        ViewportState::Spawned | ViewportState::Active | ViewportState::Verifying
    ) {
        let (tx, rx) = oneshot::channel();
        ctx.writer
            .send(WriterCommand::TransitionViewport {
                viewport_id: vp.id.clone(),
                target_state: ViewportState::Dropped,
                pid: None,
                mount_override: None,
                summary: Some(format!("Drop failed-spawn viewport `{}`", vp.name)),
                reply: tx,
            })
            .await
            .map_err(|_| RpcError::new(RpcErrorCode::Internal, "writer actor closed"))?;
        vp = rx
            .await
            .map_err(|_| RpcError::new(RpcErrorCode::Internal, "writer actor dropped reply"))??;
    }

    ctx.radar.unregister_viewport(&vp.id).await;

    if vp.state == ViewportState::Dropped {
        let (tx, rx) = oneshot::channel();
        ctx.writer
            .send(WriterCommand::TransitionViewport {
                viewport_id: vp.id.clone(),
                target_state: ViewportState::GcPending,
                pid: None,
                mount_override: None,
                summary: Some(format!("Mark failed-spawn viewport `{}` GC_Pending", vp.name)),
                reply: tx,
            })
            .await
            .map_err(|_| RpcError::new(RpcErrorCode::Internal, "writer actor closed"))?;
        vp = rx
            .await
            .map_err(|_| RpcError::new(RpcErrorCode::Internal, "writer actor dropped reply"))??;
    }

    if vp.state == ViewportState::GcPending && vp.pruned_at_ns.is_some() {
        return Ok(to_summary(vp));
    }

    let viewports_dir = ctx.loom_dir.join("viewports");
    let canon_viewports_dir =
        std::fs::canonicalize(&viewports_dir).unwrap_or_else(|_| viewports_dir.clone());
    let path_in_viewports = vp.path.starts_with(&viewports_dir)
        || std::fs::canonicalize(&vp.path)
            .map(|c| c.starts_with(&canon_viewports_dir))
            .unwrap_or(false);

    let live_vps = ctx
        .db
        .list_viewports(false)
        .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
    let path_occupied = live_vps.iter().any(|other| {
        other.id != vp.id
            && (other.path == vp.path
                || match (
                    std::fs::canonicalize(&other.path),
                    std::fs::canonicalize(&vp.path),
                ) {
                    (Ok(ca), Ok(cb)) => ca == cb,
                    _ => false,
                })
    });

    if path_in_viewports && !path_occupied {
        let vp_path = vp.path.clone();
        let backend = vp.driver;
        let mount = vp.mount.clone();
        let cache_dirs = ctx.config.vfs.cache_dirs.clone();
        tokio::task::spawn_blocking(move || {
            let drv = loom_vfs::driver_for_backend(backend, &cache_dirs);
            drv.teardown_viewport(&vp_path, mount.as_ref())
        })
        .await
        .map_err(|e| RpcError::new(RpcErrorCode::Internal, e.to_string()))?
        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
    }

    if !path_occupied {
        let (tx, rx) = oneshot::channel();
        ctx.writer
            .send(WriterCommand::MarkViewportPruned {
                viewport_id: vp.id.clone(),
                reply: tx,
            })
            .await
            .map_err(|_| RpcError::new(RpcErrorCode::Internal, "writer actor closed"))?;
        vp = rx
            .await
            .map_err(|_| RpcError::new(RpcErrorCode::Internal, "writer actor dropped reply"))??;
    }

    Ok(to_summary(vp))
}

pub async fn transition_viewport(
    ctx: &Arc<DaemonContext>,
    viewport_id: String,
    target_state: ViewportState,
    pid: Option<u32>,
) -> Result<ViewportSummary, RpcError> {
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

    let (tx, rx) = oneshot::channel();
    ctx.writer
        .send(WriterCommand::TransitionViewport {
            viewport_id,
            target_state,
            pid,
            mount_override: None,
            summary: None,
            reply: tx,
        })
        .await
        .map_err(|_| RpcError::new(RpcErrorCode::Internal, "writer actor closed"))?;
    let updated = rx
        .await
        .map_err(|_| RpcError::new(RpcErrorCode::Internal, "writer actor dropped reply"))??;

    if updated.state.is_active() {
        ctx.radar.register_viewport(&updated).await;
    } else {
        ctx.radar.unregister_viewport(&updated.id).await;
    }

    Ok(to_summary(updated))
}

pub async fn restore_clean(
    ctx: &Arc<DaemonContext>,
    viewport_id: &str,
) -> Result<ViewportSummary, RpcError> {
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

    let seal_guard = ctx.trunk_seal_lock.lock().await;

    let meta = ctx
        .db
        .get_repo_meta()
        .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
        .ok_or_else(|| {
            RpcError::new(
                RpcErrorCode::NotInitialized,
                "Loom repository is not initialized; run `loom init` first",
            )
        })?;

    let vp = ctx
        .db
        .resolve_viewport(viewport_id)
        .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
        .ok_or_else(|| {
            RpcError::new(
                RpcErrorCode::NotFound,
                format!("viewport `{viewport_id}` not found"),
            )
        })?;

    if vp.state == ViewportState::Verifying {
        return Err(RpcError::new(
            RpcErrorCode::Conflict,
            format!("cannot restore viewport `{}` while sealing/verifying", vp.name),
        ));
    }
    if !vp.state.is_active() {
        return Err(RpcError::new(
            RpcErrorCode::InvalidState,
            format!(
                "cannot restore viewport `{}` in state {}",
                vp.name, vp.state
            ),
        ));
    }

    let trunk_root = ctx.repo_root.clone();
    let vp_path = vp.path.clone();
    let mount = vp.mount.clone();
    let backend = vp.driver;
    let cache_dirs = ctx.config.vfs.cache_dirs.clone();
    let ignore_dirs = ctx.config.radar.ignore_dirs.clone();
    let cas_dir = ctx.loom_dir.join("cas");
    let zstd_level = ctx.config.zstd_level;

    ctx.radar.set_suppressed(&vp.id, true).await;

    let res = tokio::task::spawn_blocking(move || -> Result<Blake3Hash, RpcError> {
        let cas = CasStore::new(cas_dir, zstd_level)
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
        let new_base_snapshot = capture_workspace_snapshot(&trunk_root, &ignore_dirs, &cas)?;
        let drv = loom_vfs::driver_for_backend(backend, &cache_dirs);
        let cache_refs: Vec<&str> = cache_dirs.iter().map(String::as_str).collect();
        drv.restore_clean_with_context(&trunk_root, &vp_path, &cache_refs, mount.as_ref())
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
        Ok(new_base_snapshot)
    })
    .await
    .map_err(|e| RpcError::new(RpcErrorCode::Internal, e.to_string()))
    .and_then(|r| r);

    let new_base_snapshot = match res {
        Ok(snap) => snap,
        Err(err) => {
            ctx.radar.set_suppressed(&vp.id, false).await;
            return Err(err);
        }
    };

    let (tx, rx) = oneshot::channel();
    if ctx
        .writer
        .send(WriterCommand::UpdateViewportBase {
            viewport_id: vp.id.clone(),
            base_capsule_id: meta.head_capsule_id,
            base_snapshot_cas: Some(new_base_snapshot),
            clear_patches: true,
            op_kind: OpKind::ViewportTransition,
            summary: None,
            reply: tx,
        })
        .await
        .is_err()
    {
        ctx.radar.set_suppressed(&vp.id, false).await;
        return Err(RpcError::new(RpcErrorCode::Internal, "writer actor closed"));
    }
    let updated_vp = match rx.await {
        Ok(Ok(u)) => u,
        Ok(Err(err)) => {
            ctx.radar.set_suppressed(&vp.id, false).await;
            return Err(err);
        }
        Err(_) => {
            ctx.radar.set_suppressed(&vp.id, false).await;
            return Err(RpcError::new(
                RpcErrorCode::Internal,
                "writer actor dropped reply",
            ));
        }
    };

    drop(seal_guard);
    ctx.radar.reset_viewport_clean(&updated_vp.id).await;
    Ok(to_summary(updated_vp))
}

pub fn preflight_symbols(
    ctx: &Arc<DaemonContext>,
    viewport_id: Option<&str>,
    symbols: &[String],
    files: &[PathBuf],
) -> Result<McpPreflightResult, RpcError> {
    let resolved_vp = match viewport_id.map(str::trim).filter(|s| !s.is_empty()) {
        Some(v) => Some(
            ctx.db
                .resolve_viewport(v)
                .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
                .map(|r| r.id)
                .unwrap_or_else(|| v.to_string()),
        ),
        None => None,
    };

    let all_provides = ctx
        .db
        .list_symbol_provides(None)
        .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
    let all_consumes = ctx
        .db
        .list_symbol_consumes(None)
        .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;

    let file_matches = |path: &PathBuf| -> bool {
        let p_norm = path.to_string_lossy().replace('\\', "/");
        files.iter().any(|f| {
            let f_raw = f.to_string_lossy().replace('\\', "/");
            let f_norm = f_raw.trim_start_matches("./");
            !f_norm.is_empty() && (p_norm == f_norm || p_norm.ends_with(&format!("/{f_norm}")))
        })
    };

    let matches_filter = |sym: &str, path_opt: Option<&PathBuf>, is_consume: bool| -> bool {
        if symbols.is_empty() && files.is_empty() {
            return true;
        }
        let sym_hit = symbols.iter().any(|q| {
            symbol_matches_query(sym, q)
                || (is_consume
                    && !q.trim().starts_with("file:")
                    && ((loom_core::db::is_unqualified_bare_symbol(sym)
                        || (!sym.contains("impl<") && q.contains("impl<")))
                        && symbol_matches_query(q, sym)))
        });
        let file_hit = path_opt.is_some_and(&file_matches);
        sym_hit || file_hit
    };

    let provides: Vec<_> = all_provides
        .into_iter()
        .filter(|e| matches_filter(&e.symbol_fqn, Some(&e.file_path), false))
        .collect();
    let consumes: Vec<_> = all_consumes
        .into_iter()
        .filter(|e| {
            if e.locations.is_empty() {
                matches_filter(&e.symbol_fqn, None, true)
            } else {
                e.locations
                    .iter()
                    .any(|l| matches_filter(&e.symbol_fqn, Some(&l.file_path), true))
            }
        })
        .collect();

    let mut expanded_symbols: Vec<String> = symbols.to_vec();
    if !files.is_empty() {
        let mut seen: BTreeSet<String> = expanded_symbols.iter().cloned().collect();
        for p in &provides {
            if file_matches(&p.file_path) && seen.insert(p.symbol_fqn.clone()) {
                expanded_symbols.push(p.symbol_fqn.clone());
            }
        }
        for c in &consumes {
            if c.locations.iter().any(|l| file_matches(&l.file_path))
                && seen.insert(c.symbol_fqn.clone())
            {
                expanded_symbols.push(c.symbol_fqn.clone());
            }
        }
    }

    let negative_warnings = ctx
        .db
        .query_negative_edges_matching(&expanded_symbols, files)
        .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;

    let mut radar = ctx.radar.latest_snapshot(resolved_vp.as_deref());
    let matching_edge_ids: BTreeSet<&str> = negative_warnings
        .iter()
        .map(|e| e.edge_id.as_str())
        .collect();
    for vp in &mut radar.viewports {
        vp.negative_warnings
            .retain(|e| matching_edge_ids.contains(e.edge_id.as_str()));
    }

    Ok(McpPreflightResult {
        viewport_id: resolved_vp,
        provides,
        consumes,
        radar,
        negative_warnings,
    })
}

pub async fn record_deadend(
    ctx: &Arc<DaemonContext>,
    viewport_id: &str,
    mut touched_symbols: Vec<String>,
    kind: &str,
    lesson_summary: String,
    restore_clean_flag: bool,
) -> Result<NegativeEdge, RpcError> {
    let trimmed = lesson_summary.trim();
    if trimmed.is_empty() {
        return Err(RpcError::new(
            RpcErrorCode::InvalidRequest,
            "lesson_summary cannot be empty",
        ));
    }
    if let Err(e) = NegativeEdge::validate_lesson(&lesson_summary) {
        return Err(RpcError::new(RpcErrorCode::InvalidRequest, e.to_string()));
    }
    let failure_kind = match Some(kind.trim()).filter(|s| !s.is_empty()) {
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

    let seal_guard = ctx.trunk_seal_lock.lock().await;

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
                "cannot record deadend for viewport `{}` while sealing/verifying",
                initial_vp.name
            ),
        ));
    }
    if !initial_vp.state.is_active() {
        return Err(RpcError::new(
            RpcErrorCode::InvalidState,
            format!(
                "cannot record deadend for viewport `{}` in state {}",
                initial_vp.name, initial_vp.state
            ),
        ));
    }

    touched_symbols.retain(|s| !s.trim().is_empty());
    let mut patch_hashes = Vec::new();
    let vp = if touched_symbols.is_empty() {
        // Flush any un-debounced edits before reading WAL patches and diffing against base snapshot
        ctx.radar.flush_viewport(&initial_vp.id).await;
        let fresh_vp = ctx
            .db
            .get_viewport(&initial_vp.id)
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
            .ok_or_else(|| {
                RpcError::new(
                    RpcErrorCode::NotFound,
                    format!("viewport `{}` no longer exists", initial_vp.id),
                )
            })?;
        if fresh_vp.state == ViewportState::Verifying {
            return Err(RpcError::new(
                RpcErrorCode::Conflict,
                format!(
                    "cannot record deadend for viewport `{}` while sealing/verifying",
                    fresh_vp.name
                ),
            ));
        }
        if !fresh_vp.state.is_active() {
            return Err(RpcError::new(
                RpcErrorCode::InvalidState,
                format!(
                    "cannot record deadend for viewport `{}` in state {}",
                    fresh_vp.name, fresh_vp.state
                ),
            ));
        }
        let (dirty_syms, collected_hashes, _) =
            crate::capsule::collect_dirty_symbols_and_patches(ctx, &fresh_vp, false).await?;
        touched_symbols = dirty_syms;
        patch_hashes = collected_hashes;
        fresh_vp
    } else {
        initial_vp
    };

    let restore_base = if restore_clean_flag {
        let meta = ctx
            .db
            .get_repo_meta()
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
            .ok_or_else(|| {
                RpcError::new(
                    RpcErrorCode::NotInitialized,
                    "Loom repository is not initialized; run `loom init` first",
                )
            })?;

        let trunk_root = ctx.repo_root.clone();
        let vp_path = vp.path.clone();
        let mount = vp.mount.clone();
        let backend = vp.driver;
        let cache_dirs = ctx.config.vfs.cache_dirs.clone();
        let ignore_dirs = ctx.config.radar.ignore_dirs.clone();
        let cas_dir = ctx.loom_dir.join("cas");
        let zstd_level = ctx.config.zstd_level;

        ctx.radar.set_suppressed(&vp.id, true).await;

        let res = tokio::task::spawn_blocking(move || -> Result<Blake3Hash, RpcError> {
            let cas = CasStore::new(cas_dir, zstd_level)
                .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
            let new_base_snapshot = capture_workspace_snapshot(&trunk_root, &ignore_dirs, &cas)?;
            let drv = loom_vfs::driver_for_backend(backend, &cache_dirs);
            let cache_refs: Vec<&str> = cache_dirs.iter().map(String::as_str).collect();
            drv.restore_clean_with_context(&trunk_root, &vp_path, &cache_refs, mount.as_ref())
                .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
            Ok(new_base_snapshot)
        })
        .await
        .map_err(|e| RpcError::new(RpcErrorCode::Internal, e.to_string()))
        .and_then(|r| r);

        match res {
            Ok(new_base_snapshot) => Some((meta.head_capsule_id, new_base_snapshot)),
            Err(err) => {
                ctx.radar.set_suppressed(&vp.id, false).await;
                return Err(err);
            }
        }
    } else {
        None
    };

    let edge = NegativeEdge {
        edge_id: format!("ne-{}", uuid::Uuid::new_v4()),
        edge_seq: 0,
        viewport_id: vp.id.clone(),
        intent: vp.intent.clone(),
        touched_symbols,
        failure_kind,
        lesson_summary,
        patch_hashes,
        source: "mcp_record_deadend".to_string(),
        created_at_ns: now_ns(),
    };

    let (tx, rx) = oneshot::channel();
    if ctx
        .writer
        .send(WriterCommand::RecordDeadend {
            edge: Box::new(edge),
            restore_base,
            reply: tx,
        })
        .await
        .is_err()
    {
        if restore_clean_flag {
            ctx.radar.set_suppressed(&vp.id, false).await;
        }
        return Err(RpcError::new(RpcErrorCode::Internal, "writer actor closed"));
    }
    let committed_edge = match rx.await {
        Ok(Ok(e)) => e,
        Ok(Err(err)) => {
            if restore_clean_flag {
                ctx.radar.set_suppressed(&vp.id, false).await;
            }
            return Err(err);
        }
        Err(_) => {
            if restore_clean_flag {
                ctx.radar.set_suppressed(&vp.id, false).await;
            }
            return Err(RpcError::new(
                RpcErrorCode::Internal,
                "writer actor dropped reply",
            ));
        }
    };

    drop(seal_guard);
    if restore_clean_flag {
        ctx.radar.reset_viewport_clean(&vp.id).await;
    } else {
        ctx.radar.reevaluate_all().await;
    }

    Ok(committed_edge)
}

fn extract_alnum_tokens(s: &str) -> BTreeSet<String> {
    let mut tokens = BTreeSet::new();
    let mut cur = String::new();
    for ch in s.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            cur.push(ch);
        } else if !cur.is_empty() {
            tokens.insert(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        tokens.insert(cur);
    }
    tokens
}

fn jaccard_similarity(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count();
    let union = a.len() + b.len() - inter;
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

fn extract_code_symbol_set(lang: LanguageKind, path: &Path, src: &str) -> BTreeSet<String> {
    if !lang.is_code() {
        return BTreeSet::new();
    }
    let Ok(parsed) = parse_source(lang, src, None) else {
        return BTreeSet::new();
    };
    let fid = compute_logical_file_id(path);
    let units = extract_decl_units(lang, &parsed.tree, src, fid);
    let mut out = BTreeSet::new();
    let mut stack = units;
    while let Some(u) = stack.pop() {
        out.insert(u.anchor.symbol_fqn);
        stack.extend(u.children);
    }
    out
}

fn rename_languages_compatible(a: LanguageKind, b: LanguageKind) -> bool {
    if a == LanguageKind::Binary || b == LanguageKind::Binary {
        return false;
    }
    a == b
        || matches!(
            (a, b),
            (LanguageKind::TypeScript, LanguageKind::Tsx)
                | (LanguageKind::Tsx, LanguageKind::TypeScript)
        )
}

pub(crate) fn detect_renames(
    deleted: &BTreeMap<PathBuf, Vec<u8>>,
    created: &BTreeMap<PathBuf, Vec<u8>>,
) -> Vec<(PathBuf, PathBuf)> {
    if deleted.is_empty() || created.is_empty() {
        return Vec::new();
    }

    let mut pairs: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut matched_old: HashSet<PathBuf> = HashSet::new();
    let mut matched_new: HashSet<PathBuf> = HashSet::new();

    // Pass 1: Exact non-empty BLAKE3 hash match
    let mut created_by_hash: BTreeMap<Blake3Hash, Vec<PathBuf>> = BTreeMap::new();
    for (new_path, new_bytes) in created {
        if new_bytes.is_empty() {
            continue;
        }
        created_by_hash
            .entry(Blake3Hash::from_bytes(new_bytes))
            .or_default()
            .push(new_path.clone());
    }

    let mut exact_candidates: Vec<(u8, PathBuf, PathBuf)> = Vec::new();
    for (old_path, old_bytes) in deleted {
        if old_bytes.is_empty() {
            continue;
        }
        let h = Blake3Hash::from_bytes(old_bytes);
        if let Some(new_paths) = created_by_hash.get(&h) {
            for new_path in new_paths {
                let rank = u8::from(old_path.file_name() != new_path.file_name());
                exact_candidates.push((rank, old_path.clone(), new_path.clone()));
            }
        }
    }
    exact_candidates.sort();
    for (_, old_path, new_path) in exact_candidates {
        if !matched_old.contains(&old_path) && !matched_new.contains(&new_path) {
            matched_old.insert(old_path.clone());
            matched_new.insert(new_path.clone());
            pairs.push((old_path, new_path));
        }
    }

    // Pass 2: Same-kind similarity >= 0.6 for remaining non-empty files
    struct FileSig {
        path: PathBuf,
        lang: LanguageKind,
        lines: BTreeSet<String>,
        tokens: BTreeSet<String>,
        symbols: BTreeSet<String>,
    }

    let build_sig = |path: &Path, bytes: &[u8]| -> Option<FileSig> {
        if bytes.is_empty() {
            return None;
        }
        let lang = LanguageKind::detect(path, bytes);
        if lang == LanguageKind::Binary {
            return None;
        }
        let text = std::str::from_utf8(bytes).ok()?;
        let lines: BTreeSet<String> = text
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        let tokens = extract_alnum_tokens(text);
        let symbols = extract_code_symbol_set(lang, path, text);
        Some(FileSig {
            path: path.to_path_buf(),
            lang,
            lines,
            tokens,
            symbols,
        })
    };

    let old_sigs: Vec<FileSig> = deleted
        .iter()
        .filter(|(p, _)| !matched_old.contains(*p))
        .filter_map(|(p, b)| build_sig(p, b))
        .collect();
    let new_sigs: Vec<FileSig> = created
        .iter()
        .filter(|(p, _)| !matched_new.contains(*p))
        .filter_map(|(p, b)| build_sig(p, b))
        .collect();

    let mut sim_candidates: Vec<(f64, u8, PathBuf, PathBuf)> = Vec::new();
    for old in &old_sigs {
        for new in &new_sigs {
            if !rename_languages_compatible(old.lang, new.lang) {
                continue;
            }
            let line_sim = jaccard_similarity(&old.lines, &new.lines);
            let token_sim = jaccard_similarity(&old.tokens, &new.tokens);
            let base_sim = line_sim.max(token_sim);
            let score = if !old.symbols.is_empty() || !new.symbols.is_empty() {
                0.7 * base_sim + 0.3 * jaccard_similarity(&old.symbols, &new.symbols)
            } else {
                base_sim
            };
            if score >= 0.6 {
                let rank = u8::from(old.path.file_name() != new.path.file_name());
                sim_candidates.push((score, rank, old.path.clone(), new.path.clone()));
            }
        }
    }
    sim_candidates.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
            .then_with(|| a.3.cmp(&b.3))
    });
    for (_, _, old_path, new_path) in sim_candidates {
        if !matched_old.contains(&old_path) && !matched_new.contains(&new_path) {
            matched_old.insert(old_path.clone());
            matched_new.insert(new_path.clone());
            pairs.push((old_path, new_path));
        }
    }

    pairs.sort();
    pairs
}

pub(crate) struct PreparedSeal {
    pub(crate) vp_patches: Vec<AstPatch>,
    pub(crate) touched_files: Vec<PathBuf>,
    pub(crate) touched_symbols: Vec<String>,
    pub(crate) pre_seal_snapshot_cas: Blake3Hash,
    pub(crate) post_seal_snapshot_cas: Blake3Hash,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_and_apply_3way_seal(
    repo_root: &Path,
    vp: &ViewportRecord,
    ignore_dirs: &[String],
    cas: &CasStore,
) -> Result<PreparedSeal, RpcError> {
    let base_hashes = match vp.base_snapshot_cas {
        Some(snap_hash) => Some(load_workspace_snapshot(cas, snap_hash)?),
        None => None,
    };

    let trunk_rels: BTreeSet<PathBuf> = collect_non_ignored_files(repo_root, ignore_dirs)
        .into_iter()
        .collect();
    let vp_rels: BTreeSet<PathBuf> = collect_non_ignored_files(&vp.path, ignore_dirs)
        .into_iter()
        .collect();
    let mut all_rels: BTreeSet<PathBuf> = trunk_rels.union(&vp_rels).cloned().collect();
    if let Some(ref base_map) = base_hashes {
        all_rels.extend(base_map.keys().cloned());
    }

    let mut merge_base_files: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
    let mut all_merge_patches: Vec<AstPatch> = Vec::new();
    let mut vp_patches: Vec<AstPatch> = Vec::new();
    let mut vp_only_writes: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
    let mut both_modified_paths: BTreeSet<PathBuf> = BTreeSet::new();
    let mut vp_deletes: Vec<PathBuf> = Vec::new();
    let mut touched_files: Vec<PathBuf> = Vec::new();
    let mut touched_symbols_set: BTreeSet<String> = BTreeSet::new();
    let mut handled_rels: BTreeSet<PathBuf> = BTreeSet::new();

    if let Some(ref base_map) = base_hashes {
        let mut trunk_deleted: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
        let mut trunk_created: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
        let mut vp_deleted: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
        let mut vp_created: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();

        for (rel, bh) in base_map {
            if !trunk_rels.contains(rel)
                && let Ok(bytes) = cas.get_bytes(bh)
            {
                trunk_deleted.insert(rel.clone(), bytes);
            }
            if !vp_rels.contains(rel)
                && let Ok(bytes) = cas.get_bytes(bh)
            {
                vp_deleted.insert(rel.clone(), bytes);
            }
        }
        for rel in &trunk_rels {
            if !base_map.contains_key(rel)
                && let Ok(bytes) = std::fs::read(repo_root.join(rel))
            {
                trunk_created.insert(rel.clone(), bytes);
            }
        }
        for rel in &vp_rels {
            if !base_map.contains_key(rel)
                && let Ok(bytes) = std::fs::read(vp.path.join(rel))
            {
                vp_created.insert(rel.clone(), bytes);
            }
        }

        let trunk_renames: BTreeMap<PathBuf, PathBuf> = detect_renames(&trunk_deleted, &trunk_created)
            .into_iter()
            .collect();
        let vp_renames: BTreeMap<PathBuf, PathBuf> = detect_renames(&vp_deleted, &vp_created)
            .into_iter()
            .collect();

        for (old_p, new_p) in &trunk_renames {
            handled_rels.insert(old_p.clone());
            handled_rels.insert(new_p.clone());
        }
        for (old_p, new_p) in &vp_renames {
            handled_rels.insert(old_p.clone());
            handled_rels.insert(new_p.clone());
        }

        let all_rename_olds: BTreeSet<PathBuf> = trunk_renames
            .keys()
            .chain(vp_renames.keys())
            .cloned()
            .collect();

        for old_path in all_rename_olds {
            let b_hash = base_map[&old_path];
            let b_bytes = cas
                .get_bytes(&b_hash)
                .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
            let fid = compute_logical_file_id(&old_path);

            match (trunk_renames.get(&old_path), vp_renames.get(&old_path)) {
                (Some(t_new), Some(v_new)) => {
                    let t_bytes = std::fs::read(repo_root.join(t_new)).map_err(|e| {
                        RpcError::new(
                            RpcErrorCode::IoError,
                            format!("failed to read trunk rename target `{}`: {e}", t_new.display()),
                        )
                    })?;
                    let v_bytes = std::fs::read(vp.path.join(v_new)).map_err(|e| {
                        RpcError::new(
                            RpcErrorCode::IoError,
                            format!("failed to read viewport rename target `{}`: {e}", v_new.display()),
                        )
                    })?;
                    let t_lang = LanguageKind::detect(t_new, &t_bytes);
                    let v_lang = LanguageKind::detect(v_new, &v_bytes);
                    let trunk_ctx = DiffContext {
                        logical_file_id: fid,
                        file_path: t_new,
                        language: t_lang,
                        viewport_id: "__trunk__",
                        actor_id: "__trunk__",
                        lamport_ts: 1,
                        parents: Vec::new(),
                    };
                    let vp_ctx = DiffContext {
                        logical_file_id: fid,
                        file_path: v_new,
                        language: v_lang,
                        viewport_id: &vp.id,
                        actor_id: &vp.agent_id,
                        lamport_ts: vp.lamport_ts.max(2),
                        parents: Vec::new(),
                    };
                    let trunk_patch = diff_file_to_patch(
                        &trunk_ctx,
                        &b_bytes,
                        &t_bytes,
                        Some(&old_path),
                        Some(cas),
                    )
                    .map_err(|err| {
                        RpcError::new(
                            RpcErrorCode::InvalidState,
                            format!("failed to diff trunk `{}` during seal: {err}", t_new.display()),
                        )
                    })?;
                    let vp_patch = if v_lang.is_code() {
                        diff_file_to_patch_strict(
                            &vp_ctx,
                            &b_bytes,
                            &v_bytes,
                            Some(&old_path),
                            Some(cas),
                        )
                    } else {
                        diff_file_to_patch(
                            &vp_ctx,
                            &b_bytes,
                            &v_bytes,
                            Some(&old_path),
                            Some(cas),
                        )
                    }
                    .map_err(|err| {
                        RpcError::new(
                            RpcErrorCode::InvalidState,
                            format!("failed to diff `{}` during seal: {err}", v_new.display()),
                        )
                    })?;

                    merge_base_files.insert(old_path.clone(), b_bytes);
                    touched_files.push(v_new.clone());
                    collect_patch_symbols(&vp_patch, &mut touched_symbols_set);
                    all_merge_patches.push(trunk_patch);
                    all_merge_patches.push(vp_patch.clone());
                    vp_patches.push(vp_patch);
                    if t_new == v_new && t_bytes != v_bytes {
                        both_modified_paths.insert(t_new.clone());
                    }
                }
                (None, Some(v_new)) => {
                    let Some(t_old_bytes) = trunk_rels
                        .contains(&old_path)
                        .then(|| std::fs::read(repo_root.join(&old_path)).ok())
                        .flatten()
                    else {
                        return Err(RpcError::new(
                            RpcErrorCode::Conflict,
                            format!(
                                "delete-vs-rename conflict on `{}` during seal",
                                old_path.display()
                            ),
                        ));
                    };
                    let v_bytes = std::fs::read(vp.path.join(v_new)).map_err(|e| {
                        RpcError::new(
                            RpcErrorCode::IoError,
                            format!("failed to read viewport rename target `{}`: {e}", v_new.display()),
                        )
                    })?;
                    let v_lang = LanguageKind::detect(v_new, &v_bytes);
                    let vp_ctx = DiffContext {
                        logical_file_id: fid,
                        file_path: v_new,
                        language: v_lang,
                        viewport_id: &vp.id,
                        actor_id: &vp.agent_id,
                        lamport_ts: vp.lamport_ts.max(2),
                        parents: Vec::new(),
                    };
                    let vp_patch = if v_lang.is_code() {
                        diff_file_to_patch_strict(
                            &vp_ctx,
                            &b_bytes,
                            &v_bytes,
                            Some(&old_path),
                            Some(cas),
                        )
                    } else {
                        diff_file_to_patch(
                            &vp_ctx,
                            &b_bytes,
                            &v_bytes,
                            Some(&old_path),
                            Some(cas),
                        )
                    }
                    .map_err(|err| {
                        RpcError::new(
                            RpcErrorCode::InvalidState,
                            format!("failed to diff `{}` during seal: {err}", v_new.display()),
                        )
                    })?;

                    touched_files.push(old_path.clone());
                    touched_files.push(v_new.clone());
                    collect_patch_symbols(&vp_patch, &mut touched_symbols_set);
                    vp_patches.push(vp_patch.clone());

                    let t_at_target = trunk_rels
                        .contains(v_new)
                        .then(|| std::fs::read(repo_root.join(v_new)).ok())
                        .flatten();
                    let trunk_collides_target =
                        t_at_target.is_some() && !trunk_renames.contains_key(v_new);
                    if let Some(t_target_bytes) = t_at_target.as_ref().filter(|_| trunk_collides_target)
                    {
                        if let Some(base_target_hash) = base_map.get(v_new) {
                            let base_target_bytes = cas
                                .get_bytes(base_target_hash)
                                .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
                            merge_base_files.insert(v_new.clone(), base_target_bytes);
                        } else {
                            let t_fid = compute_logical_file_id(v_new);
                            let t_lang = LanguageKind::detect(v_new, t_target_bytes);
                            let t_ctx = DiffContext {
                                logical_file_id: t_fid,
                                file_path: v_new,
                                language: t_lang,
                                viewport_id: "__trunk__",
                                actor_id: "__trunk__",
                                lamport_ts: 1,
                                parents: Vec::new(),
                            };
                            let t_create_patch =
                                diff_file_to_patch(&t_ctx, &[], t_target_bytes, None, Some(cas))
                                    .map_err(|err| {
                                        RpcError::new(
                                            RpcErrorCode::InvalidState,
                                            format!(
                                                "failed to diff trunk `{}` during seal: {err}",
                                                v_new.display()
                                            ),
                                        )
                                    })?;
                            all_merge_patches.push(t_create_patch);
                        }
                    }

                    let trunk_modified_old = Blake3Hash::from_bytes(&t_old_bytes) != b_hash;
                    if trunk_modified_old {
                        let t_lang = LanguageKind::detect(&old_path, &t_old_bytes);
                        let trunk_ctx = DiffContext {
                            logical_file_id: fid,
                            file_path: &old_path,
                            language: t_lang,
                            viewport_id: "__trunk__",
                            actor_id: "__trunk__",
                            lamport_ts: 1,
                            parents: Vec::new(),
                        };
                        let trunk_patch = diff_file_to_patch(
                            &trunk_ctx,
                            &b_bytes,
                            &t_old_bytes,
                            None,
                            Some(cas),
                        )
                        .map_err(|err| {
                            RpcError::new(
                                RpcErrorCode::InvalidState,
                                format!(
                                    "failed to diff trunk `{}` during seal: {err}",
                                    old_path.display()
                                ),
                            )
                        })?;
                        merge_base_files.insert(old_path.clone(), b_bytes);
                        all_merge_patches.push(trunk_patch);
                        all_merge_patches.push(vp_patch);
                        both_modified_paths.insert(v_new.clone());
                        vp_deletes.push(old_path);
                    } else {
                        if v_lang.is_code() || trunk_collides_target {
                            merge_base_files.insert(old_path.clone(), b_bytes);
                            all_merge_patches.push(vp_patch);
                        }
                        vp_only_writes.insert(v_new.clone(), v_bytes);
                        vp_deletes.push(old_path);
                    }
                }
                (Some(t_new), None) => {
                    let Some(v_old_bytes) = vp_rels
                        .contains(&old_path)
                        .then(|| std::fs::read(vp.path.join(&old_path)).ok())
                        .flatten()
                    else {
                        return Err(RpcError::new(
                            RpcErrorCode::Conflict,
                            format!(
                                "rename-vs-delete conflict on `{}` during seal",
                                old_path.display()
                            ),
                        ));
                    };
                    let t_bytes = std::fs::read(repo_root.join(t_new)).map_err(|e| {
                        RpcError::new(
                            RpcErrorCode::IoError,
                            format!("failed to read trunk rename target `{}`: {e}", t_new.display()),
                        )
                    })?;
                    let t_lang = LanguageKind::detect(t_new, &t_bytes);
                    let trunk_ctx = DiffContext {
                        logical_file_id: fid,
                        file_path: t_new,
                        language: t_lang,
                        viewport_id: "__trunk__",
                        actor_id: "__trunk__",
                        lamport_ts: 1,
                        parents: Vec::new(),
                    };
                    let trunk_patch = diff_file_to_patch(
                        &trunk_ctx,
                        &b_bytes,
                        &t_bytes,
                        Some(&old_path),
                        Some(cas),
                    )
                    .map_err(|err| {
                        RpcError::new(
                            RpcErrorCode::InvalidState,
                            format!("failed to diff trunk `{}` during seal: {err}", t_new.display()),
                        )
                    })?;

                    let v_at_target = vp_rels
                        .contains(t_new)
                        .then(|| std::fs::read(vp.path.join(t_new)).ok())
                        .flatten();
                    let vp_collides_target =
                        v_at_target.is_some() && !vp_renames.contains_key(t_new);
                    if let Some(v_target_bytes) = v_at_target.as_ref().filter(|_| vp_collides_target)
                    {
                        let v_fid = compute_logical_file_id(t_new);
                        let v_lang = LanguageKind::detect(t_new, v_target_bytes);
                        let v_ctx = DiffContext {
                            logical_file_id: v_fid,
                            file_path: t_new,
                            language: v_lang,
                            viewport_id: &vp.id,
                            actor_id: &vp.agent_id,
                            lamport_ts: vp.lamport_ts.max(2),
                            parents: Vec::new(),
                        };
                        let v_create_patch = if v_lang.is_code() {
                            diff_file_to_patch_strict(&v_ctx, &[], v_target_bytes, None, Some(cas))
                        } else {
                            diff_file_to_patch(&v_ctx, &[], v_target_bytes, None, Some(cas))
                        }
                        .map_err(|err| {
                            RpcError::new(
                                RpcErrorCode::InvalidState,
                                format!("failed to diff `{}` during seal: {err}", t_new.display()),
                            )
                        })?;
                        all_merge_patches.push(v_create_patch);
                    }

                    let vp_modified_old = Blake3Hash::from_bytes(&v_old_bytes) != b_hash;
                    if vp_modified_old {
                        let v_lang = LanguageKind::detect(&old_path, &v_old_bytes);
                        let vp_ctx = DiffContext {
                            logical_file_id: fid,
                            file_path: &old_path,
                            language: v_lang,
                            viewport_id: &vp.id,
                            actor_id: &vp.agent_id,
                            lamport_ts: vp.lamport_ts.max(2),
                            parents: Vec::new(),
                        };
                        let vp_patch = if v_lang.is_code() {
                            diff_file_to_patch_strict(
                                &vp_ctx,
                                &b_bytes,
                                &v_old_bytes,
                                None,
                                Some(cas),
                            )
                        } else {
                            diff_file_to_patch(&vp_ctx, &b_bytes, &v_old_bytes, None, Some(cas))
                        }
                        .map_err(|err| {
                            RpcError::new(
                                RpcErrorCode::InvalidState,
                                format!(
                                    "failed to diff `{}` during seal: {err}",
                                    old_path.display()
                                ),
                            )
                        })?;
                        merge_base_files.insert(old_path.clone(), b_bytes);
                        touched_files.push(t_new.clone());
                        collect_patch_symbols(&vp_patch, &mut touched_symbols_set);
                        all_merge_patches.push(trunk_patch);
                        all_merge_patches.push(vp_patch.clone());
                        vp_patches.push(vp_patch);
                        both_modified_paths.insert(t_new.clone());
                    } else if t_lang.is_code() || vp_collides_target {
                        merge_base_files.insert(old_path.clone(), b_bytes);
                        all_merge_patches.push(trunk_patch);
                    }
                }
                (None, None) => {}
            }
        }
    }

    for rel in all_rels {
        if handled_rels.contains(&rel) {
            continue;
        }
        let trunk_path = repo_root.join(&rel);
        let vp_path = vp.path.join(&rel);
        let t_opt = trunk_rels
            .contains(&rel)
            .then(|| std::fs::read(&trunk_path).ok())
            .flatten();
        let v_opt = vp_rels
            .contains(&rel)
            .then(|| std::fs::read(&vp_path).ok())
            .flatten();

        let b_hash_opt = match &base_hashes {
            Some(map) => map.get(&rel).copied(),
            None => t_opt.as_ref().map(|b| Blake3Hash::from_bytes(b)),
        };

        let t_same_as_base = match (&t_opt, b_hash_opt) {
            (None, None) => true,
            (Some(tb), Some(bh)) => Blake3Hash::from_bytes(tb) == bh,
            _ => false,
        };
        let v_same_as_base = match (&v_opt, b_hash_opt) {
            (None, None) => true,
            (Some(vb), Some(bh)) => Blake3Hash::from_bytes(vb) == bh,
            _ => false,
        };

        if t_same_as_base && v_same_as_base {
            // Case 1: neither trunk nor viewport modified `rel`.
            continue;
        }

        let fid = compute_logical_file_id(&rel);

        if !t_same_as_base && v_same_as_base {
            // Case 2: only trunk changed `rel`. Keep trunk untouched; if code, include
            // `trunk_patch` in `all_merge_patches` for cross-file SemanticConflict checks.
            if LanguageKind::from_path(&rel).is_code() {
                let b_bytes = match b_hash_opt {
                    Some(bh) => Some(
                        cas.get_bytes(&bh)
                            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?,
                    ),
                    None => None,
                };
                let b_slice = b_bytes.as_deref().unwrap_or(&[]);
                let t_slice = t_opt.as_deref().unwrap_or(&[]);
                let lang = LanguageKind::detect(
                    &rel,
                    if t_slice.is_empty() { b_slice } else { t_slice },
                );
                let trunk_ctx = DiffContext {
                    logical_file_id: fid,
                    file_path: &rel,
                    language: lang,
                    viewport_id: "__trunk__",
                    actor_id: "__trunk__",
                    lamport_ts: 1,
                    parents: Vec::new(),
                };
                let trunk_patch =
                    diff_file_to_patch(&trunk_ctx, b_slice, t_slice, None, Some(cas)).map_err(
                        |err| {
                            RpcError::new(
                                RpcErrorCode::InvalidState,
                                format!("failed to diff trunk `{}` during seal: {err}", rel.display()),
                            )
                        },
                    )?;
                if let Some(bb) = b_bytes {
                    merge_base_files.insert(rel.clone(), bb);
                }
                all_merge_patches.push(trunk_patch);
            }
            continue;
        }

        if t_same_as_base && !v_same_as_base {
            // Case 3: only viewport changed `rel`.
            touched_files.push(rel.clone());
            let b_slice = t_opt.as_deref().unwrap_or(&[]);
            let v_slice = v_opt.as_deref().unwrap_or(&[]);
            let lang = LanguageKind::detect(
                &rel,
                if v_slice.is_empty() { b_slice } else { v_slice },
            );
            let vp_ctx = DiffContext {
                logical_file_id: fid,
                file_path: &rel,
                language: lang,
                viewport_id: &vp.id,
                actor_id: &vp.agent_id,
                lamport_ts: vp.lamport_ts.max(2),
                parents: Vec::new(),
            };
            let vp_patch = if v_opt.is_some() && lang.is_code() {
                diff_file_to_patch_strict(&vp_ctx, b_slice, v_slice, None, Some(cas))
            } else {
                diff_file_to_patch(&vp_ctx, b_slice, v_slice, None, Some(cas))
            }
            .map_err(|err| {
                RpcError::new(
                    RpcErrorCode::InvalidState,
                    format!("failed to diff `{}` during seal: {err}", rel.display()),
                )
            })?;
            collect_patch_symbols(&vp_patch, &mut touched_symbols_set);
            vp_patches.push(vp_patch.clone());
            if lang.is_code() {
                if let Some(tb) = t_opt {
                    merge_base_files.insert(rel.clone(), tb);
                }
                all_merge_patches.push(vp_patch);
            }
            if let Some(vb) = v_opt {
                vp_only_writes.insert(rel, vb);
            } else {
                vp_deletes.push(rel);
            }
            continue;
        }

        // Case 4: both trunk and viewport changed `rel` relative to base.
        if t_opt == v_opt {
            // Convergent edit or both deleted; record viewport patch if any, no trunk write needed.
            if v_opt.is_some() || b_hash_opt.is_some() {
                let b_bytes = match b_hash_opt {
                    Some(bh) => cas
                        .get_bytes(&bh)
                        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?,
                    None => Vec::new(),
                };
                let v_slice = v_opt.as_deref().unwrap_or(&[]);
                let lang = LanguageKind::detect(
                    &rel,
                    if v_slice.is_empty() {
                        &b_bytes
                    } else {
                        v_slice
                    },
                );
                let vp_ctx = DiffContext {
                    logical_file_id: fid,
                    file_path: &rel,
                    language: lang,
                    viewport_id: &vp.id,
                    actor_id: &vp.agent_id,
                    lamport_ts: vp.lamport_ts.max(2),
                    parents: Vec::new(),
                };
                let vp_patch = if v_opt.is_some() && lang.is_code() {
                    diff_file_to_patch_strict(&vp_ctx, &b_bytes, v_slice, None, Some(cas))
                } else {
                    diff_file_to_patch(&vp_ctx, &b_bytes, v_slice, None, Some(cas))
                }
                .map_err(|err| {
                    RpcError::new(
                        RpcErrorCode::InvalidState,
                        format!("failed to diff `{}` during seal: {err}", rel.display()),
                    )
                })?;
                touched_files.push(rel.clone());
                collect_patch_symbols(&vp_patch, &mut touched_symbols_set);
                vp_patches.push(vp_patch);
            }
            continue;
        }

        if b_hash_opt.is_some() && (t_opt.is_none() ^ v_opt.is_none()) {
            return Err(RpcError::new(
                RpcErrorCode::Conflict,
                format!(
                    "delete-vs-modify conflict on `{}` during seal",
                    rel.display()
                ),
            ));
        }

        let b_bytes = match b_hash_opt {
            Some(bh) => cas
                .get_bytes(&bh)
                .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?,
            None => Vec::new(),
        };
        let t_slice = t_opt.as_deref().unwrap_or(&[]);
        let v_slice = v_opt.as_deref().unwrap_or(&[]);

        let trunk_lang = LanguageKind::detect(
            &rel,
            if t_slice.is_empty() {
                &b_bytes
            } else {
                t_slice
            },
        );
        let vp_lang = LanguageKind::detect(
            &rel,
            if v_slice.is_empty() {
                &b_bytes
            } else {
                v_slice
            },
        );

        let trunk_ctx = DiffContext {
            logical_file_id: fid,
            file_path: &rel,
            language: trunk_lang,
            viewport_id: "__trunk__",
            actor_id: "__trunk__",
            lamport_ts: 1,
            parents: Vec::new(),
        };
        let vp_ctx = DiffContext {
            logical_file_id: fid,
            file_path: &rel,
            language: vp_lang,
            viewport_id: &vp.id,
            actor_id: &vp.agent_id,
            lamport_ts: vp.lamport_ts.max(2),
            parents: Vec::new(),
        };

        let trunk_patch =
            diff_file_to_patch(&trunk_ctx, &b_bytes, t_slice, None, Some(cas)).map_err(|err| {
                RpcError::new(
                    RpcErrorCode::InvalidState,
                    format!("failed to diff trunk `{}` during seal: {err}", rel.display()),
                )
            })?;
        let vp_patch = if v_opt.is_some() && vp_lang.is_code() {
            diff_file_to_patch_strict(&vp_ctx, &b_bytes, v_slice, None, Some(cas))
        } else {
            diff_file_to_patch(&vp_ctx, &b_bytes, v_slice, None, Some(cas))
        }
        .map_err(|err| {
            RpcError::new(
                RpcErrorCode::InvalidState,
                format!("failed to diff `{}` during seal: {err}", rel.display()),
            )
        })?;

        merge_base_files.insert(rel.clone(), b_bytes);
        touched_files.push(rel.clone());
        collect_patch_symbols(&vp_patch, &mut touched_symbols_set);
        all_merge_patches.push(trunk_patch);
        all_merge_patches.push(vp_patch.clone());
        vp_patches.push(vp_patch);
        both_modified_paths.insert(rel);
    }

    let report = if !all_merge_patches.is_empty() {
        let rep = merge_patches(&merge_base_files, &all_merge_patches, Some(cas)).map_err(
            |err| {
                RpcError::new(
                    RpcErrorCode::Conflict,
                    format!("CST merge failed during seal: {err}"),
                )
            },
        )?;
        if !matches!(rep.outcome, MergeOutcome::Clean { .. }) {
            return Err(RpcError::new(
                RpcErrorCode::Conflict,
                format!("merge conflict during seal: {:?}", rep.outcome),
            ));
        }
        Some(rep)
    } else {
        None
    };

    let mut pending_writes: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    for (rel, bytes) in vp_only_writes {
        pending_writes.push((rel, bytes));
    }
    if let Some(ref rep) = report {
        for rel in &both_modified_paths {
            let content = rep.merged_files.get(rel).ok_or_else(|| {
                RpcError::new(
                    RpcErrorCode::Internal,
                    format!("missing merged content for `{}`", rel.display()),
                )
            })?;
            let bytes = match content {
                MergedFileContent::Text(s) => s.as_bytes().to_vec(),
                MergedFileContent::Binary(manifest) => cas
                    .get_chunked_binary(manifest)
                    .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?,
            };
            pending_writes.push((rel.clone(), bytes));
        }
    }

    #[cfg(windows)]
    {
        let delete_set: HashSet<&PathBuf> = vp_deletes.iter().collect();
        let retained_trunk: Vec<&PathBuf> = trunk_rels
            .iter()
            .filter(|p| !delete_set.contains(*p))
            .collect();
        if let Some((_path, reason)) = windows_find_case_collision(
            pending_writes.iter().map(|(p, _)| p),
            retained_trunk,
        ) {
            return Err(RpcError::new(
                RpcErrorCode::Conflict,
                format!("seal aborted: {reason}"),
            ));
        }
    }

    for (rel, bytes) in &pending_writes {
        let lang = LanguageKind::from_path(rel);
        if lang.is_code() {
            let text = std::str::from_utf8(bytes).map_err(|_| {
                RpcError::new(
                    RpcErrorCode::InvalidState,
                    format!("post-merge UTF-8 validation failed for `{}`", rel.display()),
                )
            })?;
            let parsed = parse_source(lang, text, None).map_err(|e| {
                RpcError::new(
                    RpcErrorCode::InvalidState,
                    format!("post-merge parse failed for `{}`: {e}", rel.display()),
                )
            })?;
            let errors = collect_syntax_errors(&parsed.tree);
            if let Some(first_err) = errors.first() {
                return Err(RpcError::new(
                    RpcErrorCode::InvalidState,
                    format!(
                        "post-merge syntax error in `{}` at line {}, column {}",
                        rel.display(),
                        first_err.line,
                        first_err.column
                    ),
                ));
            }
        } else if lang == LanguageKind::Json {
            let text = std::str::from_utf8(bytes).map_err(|_| {
                RpcError::new(
                    RpcErrorCode::Conflict,
                    format!("post-merge UTF-8 validation failed for `{}`", rel.display()),
                )
            })?;
            if let Err(reason) = loom_core::crdt::validate_json_strict(text) {
                return Err(RpcError::new(
                    RpcErrorCode::Conflict,
                    format!(
                        "post-merge JSON validation failed for `{}`: {reason}",
                        rel.display()
                    ),
                ));
            }
        } else if lang == LanguageKind::Yaml {
            let text = std::str::from_utf8(bytes).map_err(|_| {
                RpcError::new(
                    RpcErrorCode::Conflict,
                    format!("post-merge UTF-8 validation failed for `{}`", rel.display()),
                )
            })?;
            if let Err(reason) = loom_core::crdt::validate_yaml_strict(text) {
                return Err(RpcError::new(
                    RpcErrorCode::Conflict,
                    format!(
                        "post-merge YAML validation failed for `{}`: {reason}",
                        rel.display()
                    ),
                ));
            }
        }
    }

    let pre_seal_snapshot_cas = capture_workspace_snapshot(repo_root, ignore_dirs, cas)?;

    apply_workspace_deletes_and_writes(
        repo_root,
        &vp_deletes,
        pending_writes.iter().map(|(p, b)| (p, b.as_slice())),
    )?;

    let post_seal_snapshot_cas = capture_workspace_snapshot(repo_root, ignore_dirs, cas)?;

    Ok(PreparedSeal {
        vp_patches,
        touched_files,
        touched_symbols: touched_symbols_set.into_iter().collect(),
        pre_seal_snapshot_cas,
        post_seal_snapshot_cas,
    })
}

fn prune_empty_parents(target: &Path, root: &Path) {
    let mut cur = target.parent();
    while let Some(dir) = cur {
        if dir == root || !dir.starts_with(root) {
            break;
        }
        if std::fs::remove_dir(dir).is_err() {
            break;
        }
        cur = dir.parent();
    }
}

fn rel_path_lookup_key(path: &Path) -> String {
    let parts: Vec<String> = path
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => {
                let s = s.to_string_lossy();
                #[cfg(windows)]
                {
                    Some(s.to_lowercase())
                }
                #[cfg(not(windows))]
                {
                    Some(s.into_owned())
                }
            }
            _ => None,
        })
        .collect();
    parts.join("/")
}

#[cfg(windows)]
pub(crate) fn windows_case_fold_path(path: &Path) -> String {
    rel_path_lookup_key(path)
}

#[cfg(windows)]
pub(crate) fn windows_find_case_collision<'a, W, R>(
    writes: W,
    retained: R,
) -> Option<(PathBuf, String)>
where
    W: IntoIterator<Item = &'a PathBuf>,
    R: IntoIterator<Item = &'a PathBuf>,
{
    use std::collections::HashMap;

    let mut dir_prefixes: HashMap<String, PathBuf> = HashMap::new();
    let mut retained_files: HashMap<String, PathBuf> = HashMap::new();

    for ret in retained {
        let fkey = rel_path_lookup_key(ret);
        retained_files.insert(fkey, ret.clone());
        let mut cur = ret.parent();
        while let Some(dir) = cur {
            if dir.as_os_str().is_empty() {
                break;
            }
            dir_prefixes
                .entry(rel_path_lookup_key(dir))
                .or_insert_with(|| dir.to_path_buf());
            cur = dir.parent();
        }
    }

    let mut seen_write_files: HashMap<String, PathBuf> = HashMap::new();
    for w in writes {
        let wkey = rel_path_lookup_key(w);
        if let Some(prev) = seen_write_files.get(&wkey)
            && prev != w
        {
            return Some((
                w.clone(),
                format!(
                    "case-colliding writes `{}` and `{}`",
                    prev.display(),
                    w.display()
                ),
            ));
        }
        if let Some(existing) = retained_files.get(&wkey)
            && existing != w
        {
            return Some((
                w.clone(),
                format!(
                    "case-colliding path `{}` conflicts with existing `{}`",
                    w.display(),
                    existing.display()
                ),
            ));
        }

        let mut cur = w.parent();
        while let Some(dir) = cur {
            if dir.as_os_str().is_empty() {
                break;
            }
            let dkey = rel_path_lookup_key(dir);
            if let Some(existing_dir) = dir_prefixes.get(&dkey) {
                if existing_dir != dir {
                    return Some((
                        w.clone(),
                        format!(
                            "directory casing `{}` for `{}` conflicts with existing `{}`",
                            dir.display(),
                            w.display(),
                            existing_dir.display()
                        ),
                    ));
                }
            } else {
                dir_prefixes.insert(dkey, dir.to_path_buf());
            }
            cur = dir.parent();
        }

        seen_write_files.insert(wkey, w.clone());
    }

    None
}

fn dir_has_non_deleted_entries_keys(
    dir: &Path,
    root: &Path,
    delete_keys: &HashSet<String>,
) -> Result<bool, RpcError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(RpcError::new(RpcErrorCode::IoError, e.to_string())),
    };
    for entry in entries {
        let entry = entry.map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
        if file_type.is_dir() {
            if dir_has_non_deleted_entries_keys(&path, root, delete_keys)? {
                return Ok(true);
            }
        } else {
            let Ok(rel) = path.strip_prefix(root) else {
                return Ok(true);
            };
            if !delete_keys.contains(&rel_path_lookup_key(rel)) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

#[allow(dead_code)]
pub(crate) fn dir_has_non_deleted_entries(
    dir: &Path,
    root: &Path,
    delete_set: &HashSet<PathBuf>,
) -> Result<bool, RpcError> {
    let delete_keys: HashSet<String> = delete_set.iter().map(|p| rel_path_lookup_key(p)).collect();
    dir_has_non_deleted_entries_keys(dir, root, &delete_keys)
}

fn find_write_path_conflict_keys(
    root: &Path,
    rel: &Path,
    delete_keys: &HashSet<String>,
) -> Result<Option<String>, RpcError> {
    if let Some(parent) = rel.parent() {
        let mut cur = PathBuf::new();
        for comp in parent.components() {
            cur.push(comp);
            let abs = root.join(&cur);
            match std::fs::symlink_metadata(&abs) {
                Ok(meta) if !meta.is_dir() => {
                    if !delete_keys.contains(&rel_path_lookup_key(&cur)) {
                        return Ok(Some(format!(
                            "cannot write `{}`: ancestor `{}` exists as a non-directory",
                            rel.display(),
                            cur.display()
                        )));
                    }
                }
                Ok(_) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) => {}
                Err(e) => return Err(RpcError::new(RpcErrorCode::IoError, e.to_string())),
            }
        }
    }

    let target = root.join(rel);
    match std::fs::symlink_metadata(&target) {
        Ok(meta) if meta.is_dir() => {
            if dir_has_non_deleted_entries_keys(&target, root, delete_keys)? {
                return Ok(Some(format!(
                    "cannot write `{}`: target exists as a non-empty directory",
                    rel.display()
                )));
            }
        }
        Ok(_) => {}
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) => {}
        Err(e) => return Err(RpcError::new(RpcErrorCode::IoError, e.to_string())),
    }

    Ok(None)
}

pub(crate) fn find_write_path_conflict(
    root: &Path,
    rel: &Path,
    delete_set: &HashSet<PathBuf>,
) -> Result<Option<String>, RpcError> {
    let delete_keys: HashSet<String> = delete_set.iter().map(|p| rel_path_lookup_key(p)).collect();
    find_write_path_conflict_keys(root, rel, &delete_keys)
}

pub(crate) fn apply_workspace_deletes_and_writes<'a, I>(
    root: &Path,
    deletes: &[PathBuf],
    writes: I,
) -> Result<(), RpcError>
where
    I: IntoIterator<Item = (&'a PathBuf, &'a [u8])>,
{
    let writes_vec: Vec<(&PathBuf, &[u8])> = writes.into_iter().collect();
    let delete_keys: HashSet<String> = deletes.iter().map(|p| rel_path_lookup_key(p)).collect();
    let write_keys: HashSet<String> = writes_vec
        .iter()
        .map(|(p, _)| rel_path_lookup_key(p))
        .collect();

    for (rel, _) in &writes_vec {
        if let Some(parent) = rel.parent() {
            let mut cur = PathBuf::new();
            for comp in parent.components() {
                cur.push(comp);
                if write_keys.contains(&rel_path_lookup_key(&cur)) {
                    return Err(RpcError::new(
                        RpcErrorCode::Conflict,
                        format!(
                            "cannot write `{}`: ancestor `{}` is also written as a file",
                            rel.display(),
                            cur.display()
                        ),
                    ));
                }
            }
        }
        if let Some(reason) = find_write_path_conflict_keys(root, rel, &delete_keys)? {
            return Err(RpcError::new(RpcErrorCode::Conflict, reason));
        }
    }

    for del_rel in deletes {
        let target = root.join(del_rel);
        match std::fs::symlink_metadata(&target) {
            Ok(meta) if !meta.is_dir() => {
                if let Err(e) = std::fs::remove_file(&target)
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    return Err(RpcError::new(RpcErrorCode::IoError, e.to_string()));
                }
            }
            Ok(_) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) => {}
            Err(e) => return Err(RpcError::new(RpcErrorCode::IoError, e.to_string())),
        }
        prune_empty_parents(&target, root);
    }

    let empty_deletes: HashSet<String> = HashSet::new();
    for (rel, bytes) in writes_vec {
        let target = root.join(rel);
        if let Some(reason) = find_write_path_conflict_keys(root, rel, &empty_deletes)? {
            return Err(RpcError::new(RpcErrorCode::Conflict, reason));
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
        }
        if std::fs::symlink_metadata(&target).is_ok_and(|m| m.is_dir()) {
            std::fs::remove_dir_all(&target)
                .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
        }
        std::fs::write(&target, bytes)
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_core::model::{CowBackendKind, ViewportState};
    use tempfile::TempDir;

    #[test]
    fn detect_renames_pairs_exact_non_empty_and_edited_and_skips_empty() {
        let mut deleted = BTreeMap::new();
        let mut created = BTreeMap::new();

        deleted.insert(PathBuf::from("empty_a.rs"), Vec::new());
        created.insert(PathBuf::from("empty_b.rs"), Vec::new());
        deleted.insert(
            PathBuf::from("src/old.rs"),
            b"pub fn shared() -> i32 {\n    7\n}\n".to_vec(),
        );
        created.insert(
            PathBuf::from("src/alpha.rs"),
            b"pub fn shared() -> i32 {\n    7\n}\n".to_vec(),
        );
        deleted.insert(
            PathBuf::from("src/edit_old.rs"),
            b"pub fn compute(a: i32, b: i32) -> i32 {\n    let sum = a + b;\n    sum * 2\n}\n"
                .to_vec(),
        );
        created.insert(
            PathBuf::from("src/edit_new.rs"),
            b"pub fn compute(a: i32, b: i32) -> i32 {\n    let sum = a + b;\n    sum * 3\n}\n"
                .to_vec(),
        );

        let pairs = detect_renames(&deleted, &created);
        assert_eq!(
            pairs,
            vec![
                (PathBuf::from("src/edit_old.rs"), PathBuf::from("src/edit_new.rs")),
                (PathBuf::from("src/old.rs"), PathBuf::from("src/alpha.rs")),
            ]
        );
    }

    #[test]
    fn prepare_seal_rejects_broken_rust_empty_json_and_divergent_renames() {
        let tmp = TempDir::new().unwrap();
        let repo_root = tmp.path().join("repo");
        let vp1_root = tmp.path().join("vp1");
        let vp2_root = tmp.path().join("vp2");
        let cas = CasStore::new(tmp.path().join("cas"), 3).unwrap();

        std::fs::create_dir_all(repo_root.join("src")).unwrap();
        std::fs::write(repo_root.join("src/lib.rs"), "pub fn ok() -> i32 { 1 }\n").unwrap();
        std::fs::write(repo_root.join("data.json"), "{\"a\": 1}\n").unwrap();
        std::fs::write(repo_root.join("src/old.rs"), "pub fn shared() -> i32 { 7 }\n").unwrap();

        let base_snap = capture_workspace_snapshot(&repo_root, &[".loom".into()], &cas).unwrap();

        // Broken Rust syntax rejected without mutating trunk
        std::fs::create_dir_all(vp1_root.join("src")).unwrap();
        std::fs::write(
            vp1_root.join("src/lib.rs"),
            "pub fn broken( -> i32 { let x = ;;; }\n",
        )
        .unwrap();
        std::fs::write(vp1_root.join("data.json"), "{\"a\": 1}\n").unwrap();
        std::fs::write(vp1_root.join("src/old.rs"), "pub fn shared() -> i32 { 7 }\n").unwrap();

        let vp1 = ViewportRecord {
            id: "vp-1".into(),
            name: "vp1".into(),
            agent_id: "a1".into(),
            state: ViewportState::Verifying,
            path: vp1_root.clone(),
            driver: CowBackendKind::GenericCopyFallback,
            base_snapshot_cas: Some(base_snap),
            lamport_ts: 2,
            ..ViewportRecord::default()
        };
        let err_syntax = prepare_and_apply_3way_seal(&repo_root, &vp1, &[".loom".into()], &cas)
            .err()
            .expect("broken syntax must be rejected");
        assert_eq!(err_syntax.code, RpcErrorCode::InvalidState);

        // 0-byte data.json rejected without mutating trunk
        std::fs::write(vp1_root.join("src/lib.rs"), "pub fn ok() -> i32 { 1 }\n").unwrap();
        std::fs::write(vp1_root.join("data.json"), "").unwrap();
        let err_json = prepare_and_apply_3way_seal(&repo_root, &vp1, &[".loom".into()], &cas)
            .err()
            .expect("0-byte json must be rejected");
        assert_eq!(err_json.code, RpcErrorCode::Conflict);
        assert_eq!(
            std::fs::read_to_string(repo_root.join("data.json")).unwrap(),
            "{\"a\": 1}\n"
        );

        // Divergent rename: vp2 renames src/old.rs -> src/alpha.rs and seals cleanly,
        // then vp1 renames src/old.rs -> src/beta.rs and is rejected with Conflict
        std::fs::create_dir_all(vp2_root.join("src")).unwrap();
        std::fs::write(vp2_root.join("src/lib.rs"), "pub fn ok() -> i32 { 1 }\n").unwrap();
        std::fs::write(vp2_root.join("data.json"), "{\"a\": 1}\n").unwrap();
        std::fs::write(vp2_root.join("src/alpha.rs"), "pub fn shared() -> i32 { 7 }\n").unwrap();
        let vp2 = ViewportRecord {
            id: "vp-2".into(),
            name: "vp2".into(),
            agent_id: "a2".into(),
            state: ViewportState::Verifying,
            path: vp2_root,
            driver: CowBackendKind::GenericCopyFallback,
            base_snapshot_cas: Some(base_snap),
            lamport_ts: 2,
            ..ViewportRecord::default()
        };
        prepare_and_apply_3way_seal(&repo_root, &vp2, &[".loom".into()], &cas).unwrap();
        assert!(repo_root.join("src/alpha.rs").is_file());
        assert!(!repo_root.join("src/old.rs").exists());

        std::fs::write(vp1_root.join("data.json"), "{\"a\": 1}\n").unwrap();
        std::fs::remove_file(vp1_root.join("src/old.rs")).unwrap();
        std::fs::write(vp1_root.join("src/beta.rs"), "pub fn shared() -> i32 { 7 }\n").unwrap();
        let err_rename = prepare_and_apply_3way_seal(&repo_root, &vp1, &[".loom".into()], &cas)
            .err()
            .expect("divergent rename must conflict");
        assert_eq!(err_rename.code, RpcErrorCode::Conflict);
    }

    #[test]
    fn prepare_seal_rejects_empty_and_whitespace_yaml_without_mutating_trunk() {
        let tmp = TempDir::new().unwrap();
        let repo_root = tmp.path().join("repo");
        let vp1_root = tmp.path().join("vp1");
        let cas = CasStore::new(tmp.path().join("cas"), 3).unwrap();

        let orig_yaml = "service:\n  port: 8080\n";
        std::fs::create_dir_all(&repo_root).unwrap();
        std::fs::write(repo_root.join("config.yaml"), orig_yaml).unwrap();
        std::fs::create_dir_all(&vp1_root).unwrap();
        std::fs::write(vp1_root.join("config.yaml"), orig_yaml).unwrap();

        let base_snap = capture_workspace_snapshot(&repo_root, &[".loom".into()], &cas).unwrap();
        let vp1 = ViewportRecord {
            id: "vp-yaml".into(),
            name: "vp1".into(),
            agent_id: "a1".into(),
            state: ViewportState::Verifying,
            path: vp1_root.clone(),
            driver: CowBackendKind::GenericCopyFallback,
            base_snapshot_cas: Some(base_snap),
            lamport_ts: 2,
            ..ViewportRecord::default()
        };

        // 0-byte config.yaml rejected without mutating trunk
        std::fs::write(vp1_root.join("config.yaml"), "").unwrap();
        let err_empty = prepare_and_apply_3way_seal(&repo_root, &vp1, &[".loom".into()], &cas)
            .err()
            .expect("0-byte yaml must be rejected");
        assert_eq!(err_empty.code, RpcErrorCode::Conflict);
        assert_eq!(
            std::fs::read_to_string(repo_root.join("config.yaml")).unwrap(),
            orig_yaml
        );

        // Whitespace-only config.yaml rejected without mutating trunk
        std::fs::write(vp1_root.join("config.yaml"), "   \n  \n\t\n").unwrap();
        let err_ws = prepare_and_apply_3way_seal(&repo_root, &vp1, &[".loom".into()], &cas)
            .err()
            .expect("whitespace-only yaml must be rejected");
        assert_eq!(err_ws.code, RpcErrorCode::Conflict);
        assert_eq!(
            std::fs::read_to_string(repo_root.join("config.yaml")).unwrap(),
            orig_yaml
        );
    }
}
