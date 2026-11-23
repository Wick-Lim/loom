use crate::capsule::collect_patch_symbols_vec;
use crate::git_sync::{
    ViewportRebasePlan, compute_viewport_rebase_plan, load_snapshot_bytes_map, merge_3way_trees,
};
use crate::radar::{collect_non_ignored_files, is_ignored_relative_path};
use crate::viewport_mgr::capture_workspace_snapshot;
use crate::{DaemonContext, MutationRequest, WriterCommand};
use loom_core::cas::CasStore;
use loom_core::config::{LoomConfig, RemoteConfig};
use loom_core::model::{
    Blake3Hash, CausalCapsule, GitMapEntry, GitSyncDirection, OpKind, RadarConflictItem,
    RadarLevel, ViewportRecord, ViewportState, now_ns,
};
use loom_core::remote::{
    CasHasRequest, CasHasResponse, ParsedRemoteUrl, RemoteErrorBody, RemotePullRequest,
    RemotePullResponse, RemotePushRequest, RemotePushResponse, collect_required_cas_hashes,
    parse_remote_repo_url,
};
use loom_core::rpc::{
    NativeRemotePullResult, NativeRemotePushResult, RpcError, RpcErrorCode, ViewportRebaseOutcome,
    ViewportRebaseStatus,
};
use loom_core::wal::StateEffect;
use reqwest::StatusCode;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::oneshot;

fn resolve_remote_target(
    repo_root: &Path,
    config_fallback: &LoomConfig,
    remote_arg: Option<&str>,
) -> Result<(String, ParsedRemoteUrl, Option<RemoteConfig>), RpcError> {
    let cfg_path = repo_root.join(".loom").join("config.toml");
    let cfg = if cfg_path.is_file() {
        LoomConfig::load(&cfg_path).map_err(|e| RpcError::new(RpcErrorCode::InvalidState, e.to_string()))?
    } else {
        config_fallback.clone()
    };

    let raw = remote_arg
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(cfg.git.default_remote.as_str());

    if raw.starts_with("http://") || raw.starts_with("https://") {
        let parsed = parse_remote_repo_url(raw)
            .map_err(|msg| RpcError::new(RpcErrorCode::InvalidRequest, msg))?;
        return Ok((raw.to_string(), parsed, None));
    }

    let Some(remote_cfg) = cfg.remotes.get(raw).cloned() else {
        return Err(RpcError::new(
            RpcErrorCode::NotFound,
            format!(
                "remote `{raw}` is not configured in .loom/config.toml (run `loom remote add {raw} <url>` or pass an http(s):// URL)"
            ),
        ));
    };

    let parsed = parse_remote_repo_url(&remote_cfg.url)
        .map_err(|msg| RpcError::new(RpcErrorCode::InvalidState, msg))?;
    Ok((raw.to_string(), parsed, Some(remote_cfg)))
}

fn resolve_bearer_token(
    explicit: Option<&str>,
    remote_cfg: Option<&RemoteConfig>,
) -> Option<String> {
    if let Some(tok) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(tok.to_string());
    }
    for env_key in ["LOOM_TOKEN", "LOOM_REMOTE_TOKEN"] {
        if let Ok(val) = std::env::var(env_key) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    if let Some(cfg) = remote_cfg {
        if let Some(ref env_name) = cfg.token_env
            && let Ok(val) = std::env::var(env_name)
        {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        if let Some(ref tok) = cfg.token {
            let trimmed = tok.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn resolve_branch(branch_arg: Option<&str>) -> Result<String, RpcError> {
    let raw = branch_arg
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("main");
    let stripped = raw.strip_prefix("refs/heads/").unwrap_or(raw);
    if stripped.is_empty()
        || stripped.contains("..")
        || stripped.contains('\\')
        || stripped.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(RpcError::new(
            RpcErrorCode::InvalidRequest,
            format!("invalid branch name `{raw}`"),
        ));
    }
    Ok(stripped.to_string())
}

fn remote_key(parsed: &ParsedRemoteUrl) -> String {
    format!("{}/{}", parsed.base_url.trim_end_matches('/'), parsed.repo_slug)
}

fn remote_api_url(parsed: &ParsedRemoteUrl, suffix: &str) -> String {
    format!(
        "{}/v1/repos/{}/{}",
        parsed.base_url.trim_end_matches('/'),
        parsed.repo_slug,
        suffix.trim_start_matches('/')
    )
}

fn apply_auth(req: reqwest::RequestBuilder, token: Option<&str>) -> reqwest::RequestBuilder {
    match token {
        Some(t) => req.bearer_auth(t),
        None => req,
    }
}

async fn map_http_error(resp: reqwest::Response, context: &str) -> RpcError {
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if let Ok(err_body) = serde_json::from_str::<RemoteErrorBody>(&text) {
        let code = match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => RpcErrorCode::InvalidState,
            StatusCode::NOT_FOUND => RpcErrorCode::NotFound,
            StatusCode::CONFLICT => RpcErrorCode::Conflict,
            StatusCode::BAD_REQUEST => RpcErrorCode::InvalidRequest,
            StatusCode::UNPROCESSABLE_ENTITY => RpcErrorCode::InvalidState,
            _ => RpcErrorCode::IoError,
        };
        let suffix = err_body
            .remote_head_capsule_id
            .as_deref()
            .map(|h| format!(" (remote head: {h})"))
            .unwrap_or_default();
        return RpcError::new(code, format!("{context} ({status}): {}{suffix}", err_body.message));
    }
    let code = match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => RpcErrorCode::InvalidState,
        StatusCode::NOT_FOUND => RpcErrorCode::NotFound,
        StatusCode::CONFLICT => RpcErrorCode::Conflict,
        _ => RpcErrorCode::IoError,
    };
    RpcError::new(code, format!("{context} failed with HTTP {status}: {text}"))
}

fn sync_trunk_worktree_direct(
    repo_root: &Path,
    files: &BTreeMap<PathBuf, Vec<u8>>,
    ignore_dirs: &[String],
) -> Result<(), RpcError> {
    let existing = collect_non_ignored_files(repo_root, ignore_dirs);
    let old_paths: BTreeSet<PathBuf> = existing.iter().cloned().collect();
    let mut new_paths: BTreeSet<PathBuf> = BTreeSet::new();
    let mut writes: Vec<(&PathBuf, &[u8])> = Vec::new();

    for (rel, bytes) in files {
        if rel.as_os_str().is_empty() || is_ignored_relative_path(rel, ignore_dirs) {
            continue;
        }
        new_paths.insert(rel.clone());
        writes.push((rel, bytes.as_slice()));
    }

    let deletes: Vec<PathBuf> = old_paths.difference(&new_paths).cloned().collect();

    #[cfg(windows)]
    {
        let delete_set: HashSet<&PathBuf> = deletes.iter().collect();
        let retained_trunk: Vec<&PathBuf> = existing
            .iter()
            .filter(|p| !delete_set.contains(*p))
            .collect();
        if let Some((_path, reason)) = crate::viewport_mgr::windows_find_case_collision(
            writes.iter().map(|(p, _)| *p),
            retained_trunk,
        ) {
            return Err(RpcError::new(
                RpcErrorCode::Conflict,
                format!("remote pull aborted: {reason}"),
            ));
        }
    }

    crate::viewport_mgr::apply_workspace_deletes_and_writes(repo_root, &deletes, writes)
}

pub async fn remote_push(
    ctx: &Arc<DaemonContext>,
    remote: Option<String>,
    branch: Option<String>,
    token: Option<String>,
    force: bool,
) -> Result<NativeRemotePushResult, RpcError> {
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

    let (remote_name, parsed, remote_cfg) =
        resolve_remote_target(&ctx.repo_root, &ctx.config, remote.as_deref())?;
    let branch_name = resolve_branch(branch.as_deref())?;
    let resolved_token = resolve_bearer_token(token.as_deref(), remote_cfg.as_ref());
    let r_key = remote_key(&parsed);
    let display_url = r_key.clone();

    let _seal_guard = ctx.trunk_seal_lock.lock().await;

    let cas_dir = ctx.loom_dir.join("cas");
    let zstd_level = ctx.config.zstd_level;
    let cas = CasStore::new(cas_dir, zstd_level)
        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;

    let mut capsules = ctx
        .db
        .list_capsules()
        .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
    capsules.sort_by(|a, b| {
        a.created_at_ns
            .cmp(&b.created_at_ns)
            .then_with(|| a.capsule_id.cmp(&b.capsule_id))
    });

    let mut patches = ctx
        .db
        .list_patches()
        .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
    patches.sort_by(|a, b| a.lsn.cmp(&b.lsn).then_with(|| a.patch_id.cmp(&b.patch_id)));

    let mut negative_edges = ctx
        .db
        .list_all_negative_edges()
        .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
    negative_edges.sort_by(|a, b| a.edge_seq.cmp(&b.edge_seq).then_with(|| a.edge_id.cmp(&b.edge_id)));

    let head_map_key = format!("loom_remote_head:{r_key}:{branch_name}");
    let expected_remote_head = ctx
        .db
        .get_git_map(&head_map_key)
        .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
        .and_then(|e| e.capsule_id);

    let new_head_capsule_id = meta.head_capsule_id.clone();
    let head_cap_snap = new_head_capsule_id
        .as_deref()
        .and_then(|hid| capsules.iter().find(|c| c.capsule_id == hid))
        .and_then(|c| c.snapshot_cas_hash);

    let new_head_snapshot_cas = if let Some(h) = head_cap_snap {
        Some(h)
    } else {
        let repo_root = ctx.repo_root.clone();
        let ignore_dirs = ctx.config.radar.ignore_dirs.clone();
        let cas_clone = cas.clone();
        let snap = tokio::task::spawn_blocking(move || {
            capture_workspace_snapshot(&repo_root, &ignore_dirs, &cas_clone)
        })
        .await
        .map_err(|e| RpcError::new(RpcErrorCode::Internal, e.to_string()))??;
        Some(snap)
    };

    let root_snaps: Vec<Blake3Hash> = new_head_snapshot_cas.into_iter().collect();
    let cas_closure = {
        let cas_clone = cas.clone();
        let caps_clone = capsules.clone();
        let patches_clone = patches.clone();
        tokio::task::spawn_blocking(move || {
            collect_required_cas_hashes(&cas_clone, &root_snaps, &caps_clone, &patches_clone)
        })
        .await
        .map_err(|e| RpcError::new(RpcErrorCode::Internal, e.to_string()))?
        .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
    };

    let client = reqwest::Client::new();
    let has_url = remote_api_url(&parsed, "cas/has");
    let has_resp = apply_auth(client.post(&has_url), resolved_token.as_deref())
        .json(&CasHasRequest {
            hashes: cas_closure,
        })
        .send()
        .await
        .map_err(|e| RpcError::new(RpcErrorCode::IoError, format!("failed to contact {has_url}: {e}")))?;

    if !has_resp.status().is_success() {
        return Err(map_http_error(has_resp, "CAS preflight failed").await);
    }
    let CasHasResponse { missing } = has_resp
        .json()
        .await
        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;

    let mut uploaded_cas_blobs = 0usize;
    for hash in missing {
        let raw_zst = cas
            .get_raw_compressed(&hash)
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
        let put_url = remote_api_url(&parsed, &format!("cas/{}", hash.to_hex()));
        let put_resp = apply_auth(client.put(&put_url), resolved_token.as_deref())
            .header("content-type", "application/zstd")
            .body(raw_zst)
            .send()
            .await
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, format!("CAS upload error: {e}")))?;
        if !put_resp.status().is_success() {
            return Err(map_http_error(put_resp, "CAS upload failed").await);
        }
        uploaded_cas_blobs += 1;
    }

    let push_url = remote_api_url(&parsed, "push");
    let push_req = RemotePushRequest {
        branch: branch_name.clone(),
        expected_remote_head,
        force,
        new_head_capsule_id: new_head_capsule_id.clone(),
        new_head_snapshot_cas,
        capsules: capsules.clone(),
        patches: patches.clone(),
        negative_edges: negative_edges.clone(),
    };
    let push_resp = apply_auth(client.post(&push_url), resolved_token.as_deref())
        .json(&push_req)
        .send()
        .await
        .map_err(|e| RpcError::new(RpcErrorCode::IoError, format!("remote push error: {e}")))?;

    if !push_resp.status().is_success() {
        return Err(map_http_error(push_resp, "remote push failed").await);
    }
    let ack: RemotePushResponse = push_resp
        .json()
        .await
        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;

    let now = now_ns();
    let mut effects = Vec::new();
    effects.push(StateEffect::UpsertGitMap {
        key: head_map_key,
        entry: GitMapEntry {
            git_commit_oid: ack
                .head_snapshot_cas
                .map(|h| h.to_hex())
                .unwrap_or_default(),
            capsule_id: ack.head_capsule_id.clone(),
            branch: branch_name.clone(),
            direction: GitSyncDirection::Push,
            notes_synced: true,
            timestamp_ns: now,
        },
    });
    for cap in &capsules {
        effects.push(StateEffect::UpsertGitMap {
            key: format!("loom_remote_cap:{}", cap.capsule_id),
            entry: GitMapEntry {
                git_commit_oid: cap.capsule_id.clone(),
                capsule_id: Some(cap.capsule_id.clone()),
                branch: branch_name.clone(),
                direction: GitSyncDirection::Push,
                notes_synced: true,
                timestamp_ns: now,
            },
        });
    }

    let pushed_capsule_ids: Vec<String> = capsules.iter().map(|c| c.capsule_id.clone()).collect();
    let mut touched_symbols = BTreeSet::new();
    for cap in &capsules {
        for sym in &cap.touched_symbols {
            touched_symbols.insert(sym.clone());
        }
    }

    let summary = format!(
        "Remote push to {display_url} ({branch_name}): {} capsule(s), {} CAS blob(s), {} negative edge(s)",
        pushed_capsule_ids.len(),
        uploaded_cas_blobs,
        negative_edges.len()
    );
    ctx.writer
        .mutate(MutationRequest {
            kind: OpKind::RemotePush,
            viewport_id: None,
            target_id: ack.head_capsule_id.clone(),
            capsule_id: ack.head_capsule_id.clone(),
            touched_symbols: touched_symbols.into_iter().collect(),
            summary,
            effect: StateEffect::Batch(effects),
        })
        .await?;

    Ok(NativeRemotePushResult {
        remote: remote_name,
        remote_url: display_url,
        branch: branch_name,
        head_capsule_id: ack.head_capsule_id,
        head_snapshot_cas: ack.head_snapshot_cas.map(|h| h.to_hex()),
        pushed_capsule_ids,
        uploaded_cas_blobs,
        pushed_patches: patches.len(),
        pushed_negative_edges: negative_edges.len(),
    })
}

pub async fn remote_pull(
    ctx: &Arc<DaemonContext>,
    remote: Option<String>,
    branch: Option<String>,
    token: Option<String>,
) -> Result<NativeRemotePullResult, RpcError> {
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

    let (remote_name, parsed, remote_cfg) =
        resolve_remote_target(&ctx.repo_root, &ctx.config, remote.as_deref())?;
    let branch_name = resolve_branch(branch.as_deref())?;
    let resolved_token = resolve_bearer_token(token.as_deref(), remote_cfg.as_ref());
    let r_key = remote_key(&parsed);
    let display_url = r_key.clone();

    let seal_guard = ctx.trunk_seal_lock.lock().await;

    let cas_dir = ctx.loom_dir.join("cas");
    let zstd_level = ctx.config.zstd_level;
    let cas = CasStore::new(cas_dir, zstd_level)
        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;

    let local_capsules = ctx
        .db
        .list_capsules()
        .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
    let known_capsule_ids: Vec<String> = local_capsules
        .iter()
        .map(|c| c.capsule_id.clone())
        .collect();
    let local_patches = ctx
        .db
        .list_patches()
        .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
    let known_patch_ids: Vec<String> = local_patches
        .iter()
        .map(|p| p.patch_id.clone())
        .collect();
    let local_edges = ctx
        .db
        .list_all_negative_edges()
        .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
    let known_negative_edge_ids: Vec<String> =
        local_edges.iter().map(|e| e.edge_id.clone()).collect();
    let known_cas_hashes = cas
        .list_hashes()
        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;

    let existing_git_maps = ctx
        .db
        .list_git_maps()
        .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
    let mut synced_remote_caps: HashSet<String> = HashSet::new();
    let head_map_key = format!("loom_remote_head:{r_key}:{branch_name}");
    let mut prev_remote_head: Option<String> = None;
    let mut prev_remote_snap: Option<Blake3Hash> = None;
    for (k, entry) in existing_git_maps {
        if let Some(cid) = k.strip_prefix("loom_remote_cap:") {
            synced_remote_caps.insert(cid.to_string());
        } else if k == head_map_key {
            prev_remote_head = entry.capsule_id;
            prev_remote_snap =
                Blake3Hash::from_hex(&entry.git_commit_oid).ok().filter(|h| *h != Blake3Hash::ZERO);
        }
    }

    let client = reqwest::Client::new();
    let pull_url = remote_api_url(&parsed, "pull");
    let pull_resp = apply_auth(client.post(&pull_url), resolved_token.as_deref())
        .json(&RemotePullRequest {
            branch: branch_name.clone(),
            known_capsule_ids,
            known_patch_ids,
            known_negative_edge_ids,
            known_cas_hashes,
        })
        .send()
        .await
        .map_err(|e| RpcError::new(RpcErrorCode::IoError, format!("remote pull error: {e}")))?;

    if !pull_resp.status().is_success() {
        return Err(map_http_error(pull_resp, "remote pull failed").await);
    }
    let resp: RemotePullResponse = pull_resp
        .json()
        .await
        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;

    let mut downloaded_cas_blobs = 0usize;
    for hash in &resp.required_cas_hashes {
        if cas.contains(hash) {
            continue;
        }
        let get_url = remote_api_url(&parsed, &format!("cas/{}", hash.to_hex()));
        let blob_resp = apply_auth(client.get(&get_url), resolved_token.as_deref())
            .header("accept", "application/zstd")
            .send()
            .await
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, format!("CAS download error: {e}")))?;
        if !blob_resp.status().is_success() {
            return Err(map_http_error(blob_resp, "CAS download failed").await);
        }
        let zst_bytes = blob_resp
            .bytes()
            .await
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
        if cas
            .put_verified_compressed(hash, &zst_bytes)
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
        {
            downloaded_cas_blobs += 1;
        }
    }

    let repo_root = ctx.repo_root.clone();
    let ignore_dirs = ctx.config.radar.ignore_dirs.clone();
    let cas_clone = cas.clone();
    let old_trunk_snapshot_cas = tokio::task::spawn_blocking(move || {
        capture_workspace_snapshot(&repo_root, &ignore_dirs, &cas_clone)
    })
    .await
    .map_err(|e| RpcError::new(RpcErrorCode::Internal, e.to_string()))??;

    let mut existing_cap_map: HashMap<String, CausalCapsule> = local_capsules
        .iter()
        .map(|c| (c.capsule_id.clone(), c.clone()))
        .collect();
    for cap in &resp.capsules {
        existing_cap_map.insert(cap.capsule_id.clone(), cap.clone());
    }

    let pulled_ids: HashSet<String> = resp.capsules.iter().map(|c| c.capsule_id.clone()).collect();
    let mut unpushed_local: Vec<CausalCapsule> = local_capsules
        .iter()
        .filter(|c| {
            !synced_remote_caps.contains(&c.capsule_id)
                && !pulled_ids.contains(&c.capsule_id)
                && Some(&c.capsule_id) != resp.head_capsule_id.as_ref()
        })
        .cloned()
        .collect();
    unpushed_local.sort_by(|a, b| {
        a.created_at_ns
            .cmp(&b.created_at_ns)
            .then_with(|| a.capsule_id.cmp(&b.capsule_id))
    });

    let remote_head_changed = resp.head_capsule_id != prev_remote_head
        || (resp.head_capsule_id.is_none()
            && resp.head_snapshot_cas.is_some()
            && resp.head_snapshot_cas != Some(old_trunk_snapshot_cas));

    let mut updated_trunk = false;
    let mut new_trunk_snapshot_cas = old_trunk_snapshot_cas;
    let mut final_head_capsule: Option<CausalCapsule> = None;
    let mut sync_files_target: Option<BTreeMap<PathBuf, Vec<u8>>> = None;

    if let Some(remote_snap) = resp.head_snapshot_cas
        && remote_head_changed
    {
        if unpushed_local.is_empty() {
            let remote_files = load_snapshot_bytes_map(&cas, remote_snap)?;
            sync_files_target = Some(remote_files);
            new_trunk_snapshot_cas = remote_snap;
            updated_trunk = true;
            if let Some(ref rh) = resp.head_capsule_id {
                final_head_capsule = existing_cap_map.get(rh).cloned();
            }
        } else {
            let base_snap = prev_remote_head
                .as_ref()
                .and_then(|prev_id| existing_cap_map.get(prev_id))
                .and_then(|prev_cap| prev_cap.snapshot_cas_hash)
                .or(prev_remote_snap);
            let base_files = if let Some(prev_snap) = base_snap {
                load_snapshot_bytes_map(&cas, prev_snap)?
            } else {
                BTreeMap::new()
            };
            let remote_files = load_snapshot_bytes_map(&cas, remote_snap)?;
            let local_files = load_snapshot_bytes_map(&cas, old_trunk_snapshot_cas)?;
            let local_head_id = meta
                .head_capsule_id
                .clone()
                .unwrap_or_else(|| "local-head".to_string());
            let (merged_files, merged_snap) = merge_3way_trees(
                &base_files,
                &remote_files,
                &local_files,
                &local_head_id,
                &branch_name,
                &cas,
            )?;

            let mut touched_files: Vec<PathBuf> = merged_files.keys().cloned().collect();
            touched_files.sort();
            let mut parents = Vec::new();
            if let Some(lh) = meta.head_capsule_id.clone() {
                parents.push(lh);
            }
            if let Some(rh) = resp.head_capsule_id.clone()
                && !parents.contains(&rh)
            {
                parents.push(rh);
            }
            let now = now_ns();
            let mut merge_seed = Vec::new();
            merge_seed.extend_from_slice(merged_snap.as_bytes());
            for p in &parents {
                merge_seed.extend_from_slice(p.as_bytes());
            }
            let merge_cap_id = format!("cap-{}", Blake3Hash::from_bytes(&merge_seed).short_hex(16));
            let merge_cap = CausalCapsule {
                capsule_id: merge_cap_id,
                viewport_id: "remote-pull".to_string(),
                agent_id: "loom-remote".to_string(),
                summary: format!("Merge remote `{remote_name}/{branch_name}` into local trunk"),
                patch_ids: Vec::new(),
                touched_files,
                touched_symbols: Vec::new(),
                verification_proof: None,
                parent_capsule_ids: parents,
                snapshot_cas_hash: Some(merged_snap),
                proof_cas_hash: None,
                git_commit_oid: None,
                created_at_ns: now,
            };

            sync_files_target = Some(merged_files);
            new_trunk_snapshot_cas = merged_snap;
            final_head_capsule = Some(merge_cap);
            updated_trunk = true;
        }
    }

    if let Some(ref target_files) = sync_files_target {
        let rr = ctx.repo_root.clone();
        let ig = ctx.config.radar.ignore_dirs.clone();
        let tf = target_files.clone();
        tokio::task::spawn_blocking(move || sync_trunk_worktree_direct(&rr, &tf, &ig))
            .await
            .map_err(|e| RpcError::new(RpcErrorCode::Internal, e.to_string()))??;
    }

    let now = now_ns();
    let mut effects = Vec::new();
    let mut touched_symbols = BTreeSet::new();

    for patch_rec in &resp.patches {
        for s in collect_patch_symbols_vec(&patch_rec.patch) {
            touched_symbols.insert(s);
        }
        effects.push(StateEffect::UpsertPatch(patch_rec.clone()));
    }

    let pulled_capsule_ids: Vec<String> = resp
        .capsules
        .iter()
        .map(|c| c.capsule_id.clone())
        .collect();
    for cap in &resp.capsules {
        for sym in &cap.touched_symbols {
            touched_symbols.insert(sym.clone());
        }
        effects.push(StateEffect::UpsertCapsule(cap.clone()));
        effects.push(StateEffect::UpsertGitMap {
            key: format!("loom_remote_cap:{}", cap.capsule_id),
            entry: GitMapEntry {
                git_commit_oid: cap.capsule_id.clone(),
                capsule_id: Some(cap.capsule_id.clone()),
                branch: branch_name.clone(),
                direction: GitSyncDirection::Pull,
                notes_synced: true,
                timestamp_ns: now,
            },
        });
    }

    let mut existing_edge_ids: HashSet<String> =
        local_edges.into_iter().map(|e| e.edge_id).collect();
    let mut restored_negative_edges = 0usize;
    for mut edge in resp.negative_edges {
        if !existing_edge_ids.insert(edge.edge_id.clone()) {
            continue;
        }
        edge.edge_seq = 0;
        if edge.touched_symbols.is_empty() {
            edge.touched_symbols.push("__repo__".to_string());
        }
        for sym in &edge.touched_symbols {
            touched_symbols.insert(sym.clone());
        }
        effects.push(StateEffect::InsertNegativeEdge(edge));
        restored_negative_edges += 1;
    }

    if let Some(ref head_cap) = final_head_capsule {
        effects.push(StateEffect::UpsertCapsule(head_cap.clone()));
    }

    if updated_trunk {
        effects.push(StateEffect::RecordTrunkSnapshotTransition {
            pre_snapshot_cas: old_trunk_snapshot_cas,
            post_snapshot_cas: new_trunk_snapshot_cas,
        });
    }

    if resp.head_capsule_id.is_some()
        || resp.head_snapshot_cas.is_some()
        || !resp.capsules.is_empty()
        || updated_trunk
    {
        effects.push(StateEffect::UpsertGitMap {
            key: head_map_key,
            entry: GitMapEntry {
                git_commit_oid: resp
                    .head_snapshot_cas
                    .map(|h| h.to_hex())
                    .unwrap_or_default(),
                capsule_id: resp.head_capsule_id.clone(),
                branch: branch_name.clone(),
                direction: GitSyncDirection::Pull,
                notes_synced: true,
                timestamp_ns: now,
            },
        });
    }

    let result_head_cap = final_head_capsule
        .as_ref()
        .map(|c| c.capsule_id.clone())
        .or_else(|| resp.head_capsule_id.clone())
        .or(meta.head_capsule_id);

    if !effects.is_empty() {
        let summary = format!(
            "Remote pull from {display_url} ({branch_name}): {} capsule(s), {} CAS blob(s), {} negative edge(s)",
            pulled_capsule_ids.len(),
            downloaded_cas_blobs,
            restored_negative_edges
        );
        ctx.writer
            .mutate(MutationRequest {
                kind: OpKind::RemotePull,
                viewport_id: None,
                target_id: result_head_cap.clone(),
                capsule_id: result_head_cap.clone(),
                touched_symbols: touched_symbols.into_iter().collect(),
                summary,
                effect: StateEffect::Batch(effects),
            })
            .await?;
    }

    let mut rebased_viewports = Vec::new();
    if updated_trunk {
        let active_vps: Vec<ViewportRecord> = ctx
            .db
            .list_viewports(false)
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
            .into_iter()
            .filter(|v| v.state.is_active())
            .collect();

        for vp in active_vps {
            if vp.state == ViewportState::Verifying || !vp.path.is_dir() {
                rebased_viewports.push(ViewportRebaseStatus {
                    viewport_id: vp.id,
                    viewport_name: vp.name,
                    outcome: ViewportRebaseOutcome::Skipped,
                    updated_files: 0,
                    conflicts: Vec::new(),
                });
                continue;
            }

            let vp_clone = vp.clone();
            let branch_for_rebase = branch_name.clone();
            let ignore_dirs = ctx.config.radar.ignore_dirs.clone();
            let cas_dir = ctx.loom_dir.join("cas");

            let plan = tokio::task::spawn_blocking(move || {
                let cas = CasStore::new(cas_dir, zstd_level)
                    .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
                compute_viewport_rebase_plan(
                    &vp_clone,
                    old_trunk_snapshot_cas,
                    new_trunk_snapshot_cas,
                    &branch_for_rebase,
                    &ignore_dirs,
                    &cas,
                )
            })
            .await
            .map_err(|e| RpcError::new(RpcErrorCode::Internal, e.to_string()))??;

            match plan {
                ViewportRebasePlan::Clean {
                    writes,
                    deletes,
                    updated_files,
                } => {
                    ctx.radar.set_suppressed(&vp.id, true).await;
                    let vp_path = vp.path.clone();
                    let write_res = tokio::task::spawn_blocking(move || -> Result<(), RpcError> {
                        crate::viewport_mgr::apply_workspace_deletes_and_writes(
                            &vp_path,
                            &deletes,
                            writes.iter().map(|(p, b)| (p, b.as_slice())),
                        )
                    })
                    .await
                    .map_err(|e| RpcError::new(RpcErrorCode::Internal, e.to_string()))
                    .and_then(|r| r);

                    if let Err(err) = write_res {
                        ctx.radar.set_suppressed(&vp.id, false).await;
                        if err.code == RpcErrorCode::Conflict {
                            let conflicts = vec![RadarConflictItem {
                                peer_viewport_id: String::new(),
                                peer_viewport_name: format!("remote:{branch_name}"),
                                level: RadarLevel::Red,
                                kind: "rebase_conflict".to_string(),
                                file_path: None,
                                symbol_fqn: None,
                                message: err.message,
                            }];
                            ctx.radar
                                .set_external_conflicts(&vp.id, conflicts.clone())
                                .await;
                            rebased_viewports.push(ViewportRebaseStatus {
                                viewport_id: vp.id,
                                viewport_name: vp.name,
                                outcome: ViewportRebaseOutcome::Conflict,
                                updated_files: 0,
                                conflicts,
                            });
                            continue;
                        }
                        drop(seal_guard);
                        return Err(err);
                    }

                    let (tx, rx) = oneshot::channel();
                    ctx.writer
                        .send(WriterCommand::UpdateViewportBase {
                            viewport_id: vp.id.clone(),
                            base_capsule_id: result_head_cap.clone(),
                            base_snapshot_cas: Some(new_trunk_snapshot_cas),
                            clear_patches: false,
                            op_kind: OpKind::RemotePull,
                            summary: Some(format!(
                                "Rebase viewport `{}` base snapshot onto remote pull",
                                vp.name
                            )),
                            reply: tx,
                        })
                        .await
                        .map_err(|_| RpcError::new(RpcErrorCode::Internal, "writer actor closed"))?;
                    let _ = rx
                        .await
                        .map_err(|_| RpcError::new(RpcErrorCode::Internal, "writer actor dropped reply"))??;

                    ctx.radar.reset_viewport_clean(&vp.id).await;
                    rebased_viewports.push(ViewportRebaseStatus {
                        viewport_id: vp.id,
                        viewport_name: vp.name,
                        outcome: ViewportRebaseOutcome::Clean,
                        updated_files,
                        conflicts: Vec::new(),
                    });
                }
                ViewportRebasePlan::Conflict(conflicts) => {
                    ctx.radar
                        .set_external_conflicts(&vp.id, conflicts.clone())
                        .await;
                    rebased_viewports.push(ViewportRebaseStatus {
                        viewport_id: vp.id,
                        viewport_name: vp.name,
                        outcome: ViewportRebaseOutcome::Conflict,
                        updated_files: 0,
                        conflicts,
                    });
                }
            }
        }
    }

    drop(seal_guard);
    ctx.radar.refresh_trunk_and_reevaluate().await;

    Ok(NativeRemotePullResult {
        remote: remote_name,
        remote_url: display_url,
        branch: branch_name,
        head_capsule_id: result_head_cap,
        head_snapshot_cas: Some(new_trunk_snapshot_cas.to_hex()),
        pulled_capsule_ids,
        downloaded_cas_blobs,
        pulled_patches: resp.patches.len(),
        restored_negative_edges,
        updated_trunk,
        rebased_viewports,
    })
}
