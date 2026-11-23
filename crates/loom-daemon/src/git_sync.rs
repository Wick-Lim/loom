use crate::capsule::collect_patch_symbols_vec;
use crate::radar::{collect_non_ignored_files, is_ignored_relative_path};
use crate::viewport_mgr::{
    capture_workspace_snapshot, load_workspace_snapshot, normalize_snapshot_key,
    snapshot_key_to_rel_path,
};
use crate::{DaemonContext, MutationRequest, WriterCommand};
use gix::bstr::BStr;
use gix::objs::Write as _;
use gix::objs::tree::EntryKind;
use gix::refs::transaction::{PreviousValue, RefEdit};
use loom_core::cas::CasStore;
use loom_core::cst::{
    DiffContext, MergedFileContent, compute_logical_file_id, diff_file_to_patch, merge_patches,
    parse_source,
};
use loom_core::model::{
    AstPatch, AstPatchRecord, Blake3Hash, CausalCapsule, GitMapEntry, GitSyncDirection,
    LanguageKind, MergeOutcome, NegativeEdge, OpKind, RadarConflictItem, RadarLevel, SemanticOp,
    ViewportRecord, ViewportState, WorkspaceSnapshot, now_ns,
};
use loom_core::rpc::{
    GitPullResult, GitPushResult, RpcError, RpcErrorCode, ViewportRebaseOutcome,
    ViewportRebaseStatus,
};
use loom_core::wal::StateEffect;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::oneshot;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoomGitNote {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capsule: Option<CausalCapsule>,
    #[serde(default)]
    pub negative_edges: Vec<NegativeEdge>,
}

fn open_gix_repo_with_fallback(
    path: &Path,
    agent_hint: Option<&str>,
) -> anyhow::Result<gix::Repository> {
    let opts = gix::open::Options::default().with(gix::sec::Trust::Full);
    let mut repo = gix::open_opts(path, opts)?;
    let need_author = repo.author().is_none();
    let need_committer = repo.committer().is_none();
    if need_author || need_committer {
        let name = agent_hint
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Project Loom");
        let mut cfg = repo.config_snapshot_mut();
        let mut overrides = Vec::new();
        if need_author {
            overrides.push(format!("gitoxide.author.nameFallback={name}"));
            overrides.push("gitoxide.author.emailFallback=loom@localhost".to_string());
        }
        if need_committer {
            overrides.push(format!("gitoxide.committer.nameFallback={name}"));
            overrides.push("gitoxide.committer.emailFallback=loom@localhost".to_string());
        }
        cfg.append_config(overrides, gix::config::Source::Api)?;
        cfg.commit()?;
    }
    Ok(repo)
}

fn looks_like_git_repo_dir(p: &Path) -> bool {
    (p.join("HEAD").is_file() && p.join("objects").is_dir()) || p.join(".git").exists()
}

fn resolve_local_bare_remote(
    repo_root: &Path,
    local_repo: &gix::Repository,
    remote_arg: Option<&str>,
    default_remote: &str,
    direction: gix::remote::Direction,
) -> Result<(String, PathBuf), RpcError> {
    let trimmed = remote_arg.map(str::trim).filter(|s| !s.is_empty());

    if let Some(raw) = trimmed {
        if raw.starts_with("file://") {
            let url = gix::url::parse(BStr::new(raw.as_bytes())).map_err(|e| {
                RpcError::new(
                    RpcErrorCode::InvalidRequest,
                    format!("invalid file:// remote URL `{raw}`: {e}"),
                )
            })?;
            let native = gix::path::to_native_path_on_windows(Cow::Borrowed(url.path.as_ref()))
                .map_err(|e| {
                    RpcError::new(
                        RpcErrorCode::InvalidRequest,
                        format!("invalid local remote path `{raw}`: {e}"),
                    )
                })?;
            let path = if native.is_absolute() {
                native.into_owned()
            } else {
                repo_root.join(native.as_ref())
            };
            return Ok((raw.to_string(), path));
        }

        if raw.contains("://") || raw.starts_with("git@") {
            return Err(RpcError::new(
                RpcErrorCode::InvalidState,
                format!(
                    "refusing non-local remote URL `{raw}`; Loom git sync targets local/file:// bare repositories only"
                ),
            ));
        }

        let direct = Path::new(raw);
        let candidate = if direct.is_absolute() {
            direct.to_path_buf()
        } else {
            repo_root.join(direct)
        };
        if looks_like_git_repo_dir(&candidate) {
            return Ok((raw.to_string(), candidate));
        }
    }

    let remote_name = trimmed.unwrap_or(default_remote);
    let remote = local_repo
        .try_find_remote(BStr::new(remote_name.as_bytes()))
        .transpose()
        .map_err(|e| {
            RpcError::new(
                RpcErrorCode::InvalidState,
                format!("failed to inspect git remote `{remote_name}`: {e}"),
            )
        })?
        .ok_or_else(|| {
            RpcError::new(
                RpcErrorCode::NotFound,
                format!(
                    "git remote `{remote_name}` not found (pass a local bare repository path or configure a file:// remote)"
                ),
            )
        })?;

    let url = remote.url(direction).ok_or_else(|| {
        RpcError::new(
            RpcErrorCode::InvalidState,
            format!("git remote `{remote_name}` has no configured URL"),
        )
    })?;

    if url.scheme != gix::url::Scheme::File {
        return Err(RpcError::new(
            RpcErrorCode::InvalidState,
            format!(
                "refusing non-local remote URL `{}` (scheme `{}`); Loom git sync targets local/file:// bare repositories only",
                url.to_bstring(),
                url.scheme
            ),
        ));
    }

    let native =
        gix::path::to_native_path_on_windows(Cow::Borrowed(url.path.as_ref())).map_err(|e| {
            RpcError::new(
                RpcErrorCode::InvalidState,
                format!("invalid file remote path for `{remote_name}`: {e}"),
            )
        })?;
    let path = if native.is_absolute() {
        native.into_owned()
    } else {
        repo_root.join(native.as_ref())
    };
    Ok((remote_name.to_string(), path))
}

fn resolve_branch_name(local_repo: &gix::Repository, branch_arg: Option<&str>) -> String {
    if let Some(b) = branch_arg.map(str::trim).filter(|s| !s.is_empty()) {
        return b.strip_prefix("refs/heads/").unwrap_or(b).to_string();
    }
    if let Ok(Some(head_name)) = local_repo.head_name() {
        let raw = head_name.as_bstr().to_string();
        if let Some(short) = raw.strip_prefix("refs/heads/") {
            return short.to_string();
        }
    }
    "main".to_string()
}

fn try_peel_ref_id(
    repo: &gix::Repository,
    ref_name: &str,
) -> Result<Option<gix::ObjectId>, RpcError> {
    match repo.try_find_reference(ref_name) {
        Ok(Some(mut r)) => {
            let id = r
                .peel_to_id()
                .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
            Ok(Some(id.detach()))
        }
        Ok(None) => Ok(None),
        Err(e) => Err(RpcError::new(RpcErrorCode::IoError, e.to_string())),
    }
}

fn is_ancestor(
    repo: &gix::Repository,
    ancestor: gix::ObjectId,
    descendant: gix::ObjectId,
) -> bool {
    if ancestor == descendant {
        return true;
    }
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();
    visited.insert(descendant);
    queue.push_back(descendant);
    while let Some(cur) = queue.pop_front() {
        let Ok(commit) = repo.find_commit(cur) else {
            continue;
        };
        for p in commit.parent_ids() {
            let pid = p.detach();
            if pid == ancestor {
                return true;
            }
            if visited.insert(pid) {
                queue.push_back(pid);
            }
        }
    }
    false
}

fn copy_reachable_objects(
    src_repo: &gix::Repository,
    dst_repo: &gix::Repository,
    roots: &[gix::ObjectId],
) -> Result<usize, RpcError> {
    let empty_tree = gix::ObjectId::empty_tree(src_repo.object_hash());
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();
    for &r in roots {
        if r != empty_tree && !dst_repo.has_object(r) && visited.insert(r) {
            queue.push_back(r);
        }
    }

    let hash_kind = src_repo.object_hash();
    let mut ordered_objects: Vec<(gix::object::Kind, Vec<u8>)> = Vec::new();
    while let Some(oid) = queue.pop_front() {
        if oid == empty_tree || dst_repo.has_object(oid) {
            continue;
        }
        let obj = src_repo.find_object(oid).map_err(|e| {
            RpcError::new(
                RpcErrorCode::IoError,
                format!("failed to read git object {oid}: {e}"),
            )
        })?;
        match obj.kind {
            gix::object::Kind::Commit => {
                let iter = gix::objs::CommitRefIter::from_bytes(&obj.data, hash_kind);
                for tok in iter {
                    match tok {
                        Ok(gix::objs::commit::ref_iter::Token::Tree { id }) => {
                            if id != empty_tree && !dst_repo.has_object(id) && visited.insert(id) {
                                queue.push_back(id);
                            }
                        }
                        Ok(gix::objs::commit::ref_iter::Token::Parent { id }) => {
                            if !dst_repo.has_object(id) && visited.insert(id) {
                                queue.push_back(id);
                            }
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
            }
            gix::object::Kind::Tree => {
                let iter = gix::objs::TreeRefIter::from_bytes(&obj.data, hash_kind);
                for entry_res in iter {
                    let Ok(entry) = entry_res else {
                        break;
                    };
                    if !entry.mode.is_commit()
                        && entry.oid != empty_tree
                        && !dst_repo.has_object(entry.oid)
                        && visited.insert(entry.oid.to_owned())
                    {
                        queue.push_back(entry.oid.to_owned());
                    }
                }
            }
            gix::object::Kind::Tag => {
                let iter = gix::objs::TagRefIter::from_bytes(&obj.data, hash_kind);
                for tok in iter {
                    if let Ok(gix::objs::tag::ref_iter::Token::Target { id }) = tok
                        && !dst_repo.has_object(id)
                        && visited.insert(id)
                    {
                        queue.push_back(id);
                    }
                }
            }
            gix::object::Kind::Blob => {}
        }
        ordered_objects.push((obj.kind, obj.data.clone()));
    }

    let count = ordered_objects.len();
    for (kind, data) in ordered_objects.into_iter().rev() {
        dst_repo
            .write_buf(kind, &data)
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
    }
    Ok(count)
}

fn build_tree_from_snapshot_or_disk(
    repo: &gix::Repository,
    repo_root: &Path,
    ignore_dirs: &[String],
    cas: &CasStore,
    snapshot_hash: Option<Blake3Hash>,
) -> Result<gix::ObjectId, RpcError> {
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    if let Some(snap_hash) = snapshot_hash
        && let Ok(map) = load_workspace_snapshot(cas, snap_hash)
    {
        for (rel, blob_hash) in map {
            let key = normalize_snapshot_key(&rel);
            if key.is_empty() {
                continue;
            }
            let bytes = match cas.get_bytes(&blob_hash) {
                Ok(b) => b,
                Err(_) => std::fs::read(repo_root.join(&rel)).map_err(|e| {
                    RpcError::new(
                        RpcErrorCode::IoError,
                        format!("missing CAS blob and disk file for `{key}`: {e}"),
                    )
                })?,
            };
            files.insert(key, bytes);
        }
    } else {
        for rel in collect_non_ignored_files(repo_root, ignore_dirs) {
            let key = normalize_snapshot_key(&rel);
            if key.is_empty() {
                continue;
            }
            if let Ok(bytes) = std::fs::read(repo_root.join(&rel)) {
                files.insert(key, bytes);
            }
        }
    }

    let empty = gix::ObjectId::empty_tree(repo.object_hash());
    let mut editor = repo
        .edit_tree(empty)
        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
    for (rel_slash, bytes) in files {
        let blob_id = repo
            .write_blob(&bytes)
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?
            .detach();
        editor
            .upsert(rel_slash.as_str(), EntryKind::Blob, blob_id)
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
    }
    let tree_id = editor
        .write()
        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?
        .detach();
    Ok(tree_id)
}

fn format_capsule_commit_message(cap: &CausalCapsule) -> String {
    let title = cap.summary.trim();
    let title = if title.is_empty() {
        format!("Loom capsule {}", cap.capsule_id)
    } else {
        title.to_string()
    };
    let agent = if cap.agent_id.trim().is_empty() {
        "loom"
    } else {
        cap.agent_id.trim()
    };
    let verified_trailer = match &cap.verification_proof {
        Some(p) => {
            let status = if p.passed { "passed" } else { "failed" };
            let cmd = p.verify_cmd.as_deref().unwrap_or("none");
            format!("{status} ({cmd})")
        }
        None => "unverified (none)".to_string(),
    };
    format!(
        "{title}\n\nLoom-Capsule-Id: {}\nLoom-Agent: {agent}\nLoom-Verified: {verified_trailer}\n",
        cap.capsule_id
    )
}

fn read_notes_map(
    repo: &gix::Repository,
    notes_commit_oid: gix::ObjectId,
) -> Result<BTreeMap<gix::ObjectId, Vec<u8>>, RpcError> {
    let commit = repo
        .find_commit(notes_commit_oid)
        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
    let tree = commit
        .tree()
        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
    let entries = tree
        .traverse()
        .breadthfirst
        .files()
        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
    let mut out = BTreeMap::new();
    for entry in entries {
        let hex: String = entry
            .filepath
            .to_string()
            .chars()
            .filter(|c| *c != '/')
            .collect();
        if let Ok(target_oid) = gix::ObjectId::from_hex(hex.as_bytes())
            && let Ok(blob) = repo.find_blob(entry.oid)
        {
            out.insert(target_oid, blob.data.clone());
        }
    }
    Ok(out)
}

fn read_note_json(
    repo: &gix::Repository,
    notes_ref: &str,
    target_oid: gix::ObjectId,
) -> Option<LoomGitNote> {
    let mut platform = repo.notes().ok()?.with_refs([notes_ref]).ok()?;
    let notes = platform.get(target_oid).ok()?;
    for note in notes {
        if let Ok(parsed) = serde_json::from_slice::<LoomGitNote>(&note.blob.data) {
            return Some(parsed);
        }
    }
    None
}

fn merge_notes(base: Option<LoomGitNote>, incoming: LoomGitNote) -> LoomGitNote {
    match base {
        None => incoming,
        Some(mut cur) => {
            if cur.capsule.is_none() {
                cur.capsule = incoming.capsule;
            }
            let mut seen: HashSet<String> = cur
                .negative_edges
                .iter()
                .map(|e| e.edge_id.clone())
                .collect();
            for edge in incoming.negative_edges {
                if seen.insert(edge.edge_id.clone()) {
                    cur.negative_edges.push(edge);
                }
            }
            cur
        }
    }
}

fn sync_local_notes_with_remote(
    local_repo: &gix::Repository,
    bare_repo: &gix::Repository,
    notes_ref: &str,
    bare_notes_tip: gix::ObjectId,
) -> Result<(), RpcError> {
    let local_notes_tip = try_peel_ref_id(local_repo, notes_ref)?;
    if local_notes_tip == Some(bare_notes_tip) {
        return Ok(());
    }
    copy_reachable_objects(bare_repo, local_repo, &[bare_notes_tip])?;

    let notes_full = gix::refs::FullName::try_from(notes_ref)
        .map_err(|e| RpcError::new(RpcErrorCode::InvalidRequest, e.to_string()))?;
    let committer_ref = local_repo
        .committer()
        .transpose()
        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;

    let Some(lnt) = local_notes_tip else {
        local_repo
            .edit_references_as(
                [RefEdit::update(
                    notes_full,
                    bare_notes_tip,
                    PreviousValue::MustNotExist,
                    "loom: sync notes from remote",
                )],
                committer_ref,
            )
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
        return Ok(());
    };

    if is_ancestor(local_repo, bare_notes_tip, lnt) {
        return Ok(());
    }
    if is_ancestor(local_repo, lnt, bare_notes_tip) {
        local_repo
            .edit_references_as(
                [RefEdit::update(
                    notes_full,
                    bare_notes_tip,
                    PreviousValue::MustExistAndMatch(gix::refs::Target::Object(lnt)),
                    "loom: fast-forward notes from remote",
                )],
                committer_ref,
            )
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
        return Ok(());
    }

    // Diverged notes trees: reset local notes_ref to bare_notes_tip and replay local entries on top
    let local_entries = read_notes_map(local_repo, lnt)?;
    local_repo
        .edit_references_as(
            [RefEdit::update(
                notes_full.clone(),
                bare_notes_tip,
                PreviousValue::MustExistAndMatch(gix::refs::Target::Object(lnt)),
                "loom: rebase notes onto remote",
            )],
            committer_ref,
        )
        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;

    let mut platform = local_repo
        .notes()
        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
    for (target_oid, raw) in local_entries {
        let merged_bytes = if let Ok(local_note) = serde_json::from_slice::<LoomGitNote>(&raw) {
            let remote_note = read_note_json(local_repo, notes_ref, target_oid);
            let merged = merge_notes(remote_note, local_note);
            serde_json::to_vec_pretty(&merged)
                .map_err(|e| RpcError::new(RpcErrorCode::Internal, e.to_string()))?
        } else {
            raw
        };
        platform
            .replace_at_ref(notes_full.as_ref(), target_oid, &merged_bytes)
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
    }
    Ok(())
}

pub async fn git_push(
    ctx: &Arc<DaemonContext>,
    remote: Option<String>,
    branch: Option<String>,
) -> Result<GitPushResult, RpcError> {
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

    if !meta.colocate_git && !ctx.repo_root.join(".git").exists() {
        return Err(RpcError::new(
            RpcErrorCode::InvalidState,
            "repository is not git-colocated; run `loom init --colocate-git` in a git repository",
        ));
    }

    let seal_guard = ctx.trunk_seal_lock.lock().await;

    let mut capsules = ctx
        .db
        .list_capsules()
        .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
    capsules.sort_by(|a, b| {
        a.created_at_ns
            .cmp(&b.created_at_ns)
            .then_with(|| a.capsule_id.cmp(&b.capsule_id))
    });

    let mut unpushed_capsules = Vec::new();
    for cap in capsules {
        let mapped = ctx
            .db
            .get_git_map(&format!("cap:{}", cap.capsule_id))
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
        if cap.git_commit_oid.is_none() && mapped.is_none() {
            unpushed_capsules.push(cap);
        }
    }

    let mut all_edges = ctx
        .db
        .list_all_negative_edges()
        .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
    all_edges.sort_by(|a, b| {
        a.edge_seq
            .cmp(&b.edge_seq)
            .then_with(|| a.edge_id.cmp(&b.edge_id))
    });
    let mut unsynced_edges = Vec::new();
    for edge in all_edges {
        let mapped = ctx
            .db
            .get_git_map(&format!("note_edge:{}", edge.edge_id))
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
        if mapped.is_none() {
            unsynced_edges.push(edge);
        }
    }

    let repo_root = ctx.repo_root.clone();
    let cas_dir = ctx.loom_dir.join("cas");
    let zstd_level = ctx.config.zstd_level;
    let ignore_dirs = ctx.config.radar.ignore_dirs.clone();
    let default_remote = ctx.config.git.default_remote.clone();
    let notes_ref = ctx.config.git.notes_ref.clone();

    struct PushPrepOutcome {
        result: GitPushResult,
        updated_capsules: Vec<CausalCapsule>,
        git_map_entries: Vec<(String, GitMapEntry)>,
    }

    let prep = tokio::task::spawn_blocking(move || -> Result<PushPrepOutcome, RpcError> {
        let cas = CasStore::new(cas_dir, zstd_level)
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
        let local_repo = open_gix_repo_with_fallback(&repo_root, None).map_err(|e| {
            RpcError::new(
                RpcErrorCode::InvalidState,
                format!("failed to open local git repository: {e}"),
            )
        })?;
        let (remote_label, bare_path) = resolve_local_bare_remote(
            &repo_root,
            &local_repo,
            remote.as_deref(),
            &default_remote,
            gix::remote::Direction::Push,
        )?;
        let bare_repo = open_gix_repo_with_fallback(&bare_path, None).map_err(|e| {
            RpcError::new(
                RpcErrorCode::IoError,
                format!(
                    "failed to open bare remote repository `{}`: {e}",
                    bare_path.display()
                ),
            )
        })?;

        let branch_name = resolve_branch_name(&local_repo, branch.as_deref());
        let branch_ref = format!("refs/heads/{branch_name}");
        let notes_full_ref = gix::refs::FullName::try_from(notes_ref.as_str())
            .map_err(|e| RpcError::new(RpcErrorCode::InvalidRequest, e.to_string()))?;

        let orig_local_branch_tip = try_peel_ref_id(&local_repo, &branch_ref)?;
        let orig_local_notes_tip = try_peel_ref_id(&local_repo, &notes_ref)?;
        let mut parent_oid = orig_local_branch_tip;
        let remote_branch_tip = try_peel_ref_id(&bare_repo, &branch_ref)?;
        let remote_notes_tip = try_peel_ref_id(&bare_repo, &notes_ref)?;

        if let Some(rtip) = remote_branch_tip {
            let up_to_date_or_ahead = parent_oid
                .is_some_and(|ltip| ltip == rtip || is_ancestor(&local_repo, rtip, ltip));
            if !up_to_date_or_ahead {
                return Err(RpcError::new(
                    RpcErrorCode::Conflict,
                    format!(
                        "remote branch `{branch_name}` has unpulled commits; run `loom git pull` first"
                    ),
                ));
            }
        }

        let push_result: Result<PushPrepOutcome, RpcError> = (|| {
            if let Some(bnt) = remote_notes_tip {
                sync_local_notes_with_remote(&local_repo, &bare_repo, &notes_ref, bnt)?;
            }

            let now = now_ns();
            let mut commits_created = 0usize;
            let mut pushed_commit_oids = Vec::new();
            let mut pushed_capsule_ids = Vec::new();
            let mut updated_capsules = Vec::new();
            let mut git_map_entries = Vec::new();
            let mut edges_attached_to_oid: Option<String> = None;

            let cap_count = unpushed_capsules.len();
            for (idx, mut cap) in unpushed_capsules.into_iter().enumerate() {
                let tree_id = build_tree_from_snapshot_or_disk(
                    &local_repo,
                    &repo_root,
                    &ignore_dirs,
                    &cas,
                    cap.snapshot_cas_hash,
                )?;
                let msg = format_capsule_commit_message(&cap);
                let commit_id = local_repo
                    .commit(branch_ref.as_str(), &msg, tree_id, parent_oid)
                    .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?
                    .detach();
                let oid_hex = commit_id.to_hex().to_string();
                parent_oid = Some(commit_id);
                commits_created += 1;
                pushed_commit_oids.push(oid_hex.clone());
                pushed_capsule_ids.push(cap.capsule_id.clone());

                cap.git_commit_oid = Some(oid_hex.clone());
                let attach_edges = if idx + 1 == cap_count && !unsynced_edges.is_empty() {
                    edges_attached_to_oid = Some(oid_hex.clone());
                    unsynced_edges.clone()
                } else {
                    Vec::new()
                };
                let note = LoomGitNote {
                    schema_version: 1,
                    capsule: Some(cap.clone()),
                    negative_edges: attach_edges,
                };
                let note_bytes = serde_json::to_vec_pretty(&note)
                    .map_err(|e| RpcError::new(RpcErrorCode::Internal, e.to_string()))?;
                {
                    let mut notes_plat = local_repo
                        .notes()
                        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
                    notes_plat
                        .replace_at_ref(notes_full_ref.as_ref(), commit_id, &note_bytes)
                        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
                }

                let map_entry = GitMapEntry {
                    git_commit_oid: oid_hex.clone(),
                    capsule_id: Some(cap.capsule_id.clone()),
                    branch: branch_name.clone(),
                    direction: GitSyncDirection::Push,
                    notes_synced: true,
                    timestamp_ns: now,
                };
                git_map_entries.push((format!("oid:{oid_hex}"), map_entry.clone()));
                git_map_entries.push((format!("cap:{}", cap.capsule_id), map_entry));
                updated_capsules.push(cap);
            }

            if cap_count == 0 && !unsynced_edges.is_empty() {
                let tip_oid = match parent_oid {
                    Some(oid) => oid,
                    None => {
                        let tree_id = build_tree_from_snapshot_or_disk(
                            &local_repo,
                            &repo_root,
                            &ignore_dirs,
                            &cas,
                            None,
                        )?;
                        let msg = "Loom trunk snapshot\n\nLoom-Agent: loom\nLoom-Verified: unverified (none)\n";
                        let cid = local_repo
                            .commit(
                                branch_ref.as_str(),
                                msg,
                                tree_id,
                                std::iter::empty::<gix::ObjectId>(),
                            )
                            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?
                            .detach();
                        parent_oid = Some(cid);
                        commits_created += 1;
                        let oid_hex = cid.to_hex().to_string();
                        pushed_commit_oids.push(oid_hex.clone());
                        git_map_entries.push((
                            format!("oid:{oid_hex}"),
                            GitMapEntry {
                                git_commit_oid: oid_hex,
                                capsule_id: None,
                                branch: branch_name.clone(),
                                direction: GitSyncDirection::Push,
                                notes_synced: true,
                                timestamp_ns: now,
                            },
                        ));
                        cid
                    }
                };

                let existing = read_note_json(&local_repo, &notes_ref, tip_oid);
                let merged = merge_notes(
                    existing,
                    LoomGitNote {
                        schema_version: 1,
                        capsule: None,
                        negative_edges: unsynced_edges.clone(),
                    },
                );
                let note_bytes = serde_json::to_vec_pretty(&merged)
                    .map_err(|e| RpcError::new(RpcErrorCode::Internal, e.to_string()))?;
                let mut notes_plat = local_repo
                    .notes()
                    .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
                notes_plat
                    .replace_at_ref(notes_full_ref.as_ref(), tip_oid, &note_bytes)
                    .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
                edges_attached_to_oid = Some(tip_oid.to_hex().to_string());
            }

            if let Some(ref oid_hex) = edges_attached_to_oid {
                for edge in &unsynced_edges {
                    git_map_entries.push((
                        format!("note_edge:{}", edge.edge_id),
                        GitMapEntry {
                            git_commit_oid: oid_hex.clone(),
                            capsule_id: Some(edge.edge_id.clone()),
                            branch: branch_name.clone(),
                            direction: GitSyncDirection::Push,
                            notes_synced: true,
                            timestamp_ns: now,
                        },
                    ));
                }
            }

            let local_branch_tip = parent_oid;
            let local_notes_tip = try_peel_ref_id(&local_repo, &notes_ref)?;

            let mut copy_roots = Vec::new();
            if let Some(id) = local_branch_tip {
                copy_roots.push(id);
            }
            if let Some(id) = local_notes_tip {
                copy_roots.push(id);
            }
            if !copy_roots.is_empty() {
                copy_reachable_objects(&local_repo, &bare_repo, &copy_roots)?;
            }

            let mut edits = Vec::new();
            if let Some(new_tip) = local_branch_tip
                && remote_branch_tip != Some(new_tip)
            {
                let full_branch = gix::refs::FullName::try_from(branch_ref.as_str())
                    .map_err(|e| RpcError::new(RpcErrorCode::InvalidRequest, e.to_string()))?;
                let expected = match remote_branch_tip {
                    Some(old) => PreviousValue::MustExistAndMatch(gix::refs::Target::Object(old)),
                    None => PreviousValue::MustNotExist,
                };
                edits.push(RefEdit::update(
                    full_branch,
                    new_tip,
                    expected,
                    "loom git push",
                ));
            }

            if let Some(new_notes) = local_notes_tip
                && remote_notes_tip != Some(new_notes)
            {
                let expected = match remote_notes_tip {
                    Some(old) => PreviousValue::MustExistAndMatch(gix::refs::Target::Object(old)),
                    None => PreviousValue::MustNotExist,
                };
                edits.push(RefEdit::update(
                    notes_full_ref.clone(),
                    new_notes,
                    expected,
                    "loom git push notes",
                ));
            }

            if !edits.is_empty() {
                let bare_committer = bare_repo
                    .committer()
                    .transpose()
                    .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
                bare_repo
                    .edit_references_as(edits, bare_committer)
                    .map_err(|e| RpcError::new(RpcErrorCode::Conflict, e.to_string()))?;
            }

            let head_points_to_branch = local_repo
                .head_name()
                .ok()
                .flatten()
                .is_some_and(|h| h.as_bstr() == branch_ref.as_str());
            if commits_created > 0
                && head_points_to_branch
                && let Some(tip_oid) = local_branch_tip
                && let Ok(tip_commit) = local_repo.find_commit(tip_oid)
                && let Ok(tip_tree_id) = tip_commit.tree_id()
                && let Ok(mut idx) = local_repo.index_from_tree(&tip_tree_id.detach())
            {
                let _ = idx.write(Default::default());
            }

            let head_commit_oid = local_branch_tip.map(|id| id.to_hex().to_string());
            let notes_commit_oid = local_notes_tip.map(|id| id.to_hex().to_string());

            Ok(PushPrepOutcome {
                result: GitPushResult {
                    remote: remote_label,
                    remote_path: bare_path,
                    branch: branch_name,
                    commits_created,
                    pushed_commit_oids,
                    pushed_capsule_ids,
                    synced_negative_edges: if edges_attached_to_oid.is_some() {
                        unsynced_edges.len()
                    } else {
                        0
                    },
                    head_commit_oid,
                    notes_ref: notes_ref.clone(),
                    notes_commit_oid,
                },
                updated_capsules,
                git_map_entries,
            })
        })();

        if let Err(err) = push_result {
            let mut rollback_edits = Vec::new();
            let cur_branch_tip = try_peel_ref_id(&local_repo, &branch_ref).ok().flatten();
            if cur_branch_tip != orig_local_branch_tip
                && let Ok(full_branch) = gix::refs::FullName::try_from(branch_ref.as_str())
            {
                match orig_local_branch_tip {
                    Some(orig) => rollback_edits.push(RefEdit::update(
                        full_branch,
                        orig,
                        PreviousValue::Any,
                        "loom git push rollback",
                    )),
                    None => rollback_edits.push(RefEdit::delete(full_branch, PreviousValue::Any)),
                }
            }
            let cur_notes_tip = try_peel_ref_id(&local_repo, &notes_ref).ok().flatten();
            if cur_notes_tip != orig_local_notes_tip {
                match orig_local_notes_tip {
                    Some(orig) => rollback_edits.push(RefEdit::update(
                        notes_full_ref,
                        orig,
                        PreviousValue::Any,
                        "loom git push rollback notes",
                    )),
                    None => rollback_edits.push(RefEdit::delete(notes_full_ref, PreviousValue::Any)),
                }
            }
            if !rollback_edits.is_empty() {
                let local_committer = local_repo.committer().transpose().ok().flatten();
                let _ = local_repo.edit_references_as(rollback_edits, local_committer);
            }
            return Err(err);
        }

        push_result
    })
    .await
    .map_err(|e| RpcError::new(RpcErrorCode::Internal, e.to_string()))??;

    if !prep.updated_capsules.is_empty() || !prep.git_map_entries.is_empty() {
        let mut effects = Vec::new();
        for cap in prep.updated_capsules {
            effects.push(StateEffect::UpsertCapsule(cap));
        }
        for (key, entry) in prep.git_map_entries {
            effects.push(StateEffect::UpsertGitMap { key, entry });
        }
        let summary = format!(
            "Git push to {} ({}): {} commit(s), {} negative edge(s)",
            prep.result.remote,
            prep.result.branch,
            prep.result.commits_created,
            prep.result.synced_negative_edges
        );
        ctx.writer
            .mutate(MutationRequest {
                kind: OpKind::GitPush,
                viewport_id: None,
                target_id: prep.result.head_commit_oid.clone(),
                capsule_id: prep.result.pushed_capsule_ids.last().cloned(),
                touched_symbols: Vec::new(),
                summary,
                effect: StateEffect::Batch(effects),
            })
            .await?;
    }

    drop(seal_guard);
    Ok(prep.result)
}

fn collect_ancestors(repo: &gix::Repository, start: gix::ObjectId) -> HashSet<gix::ObjectId> {
    let mut ancestors = HashSet::new();
    let mut queue = VecDeque::new();
    ancestors.insert(start);
    queue.push_back(start);
    while let Some(cur) = queue.pop_front() {
        let Ok(commit) = repo.find_commit(cur) else {
            continue;
        };
        for p in commit.parent_ids() {
            let pid = p.detach();
            if ancestors.insert(pid) {
                queue.push_back(pid);
            }
        }
    }
    ancestors
}

fn collect_unmapped_commits_oldest_first(
    repo: &gix::Repository,
    tip: gix::ObjectId,
    stop_at: Option<gix::ObjectId>,
    already_mapped_oids: &HashSet<String>,
) -> Result<Vec<gix::ObjectId>, RpcError> {
    let stop_ancestors = stop_at.map(|s| collect_ancestors(repo, s)).unwrap_or_default();
    let mut visited = HashSet::new();
    let mut postorder = Vec::new();

    fn dfs(
        repo: &gix::Repository,
        oid: gix::ObjectId,
        stop_ancestors: &HashSet<gix::ObjectId>,
        already_mapped_oids: &HashSet<String>,
        visited: &mut HashSet<gix::ObjectId>,
        postorder: &mut Vec<gix::ObjectId>,
    ) -> Result<(), RpcError> {
        if stop_ancestors.contains(&oid) || !visited.insert(oid) {
            return Ok(());
        }
        let oid_hex = oid.to_hex().to_string();
        if already_mapped_oids.contains(&oid_hex) {
            return Ok(());
        }
        let commit = repo
            .find_commit(oid)
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
        let parents: Vec<gix::ObjectId> = commit.parent_ids().map(|p| p.detach()).collect();
        for p in parents {
            dfs(
                repo,
                p,
                stop_ancestors,
                already_mapped_oids,
                visited,
                postorder,
            )?;
        }
        postorder.push(oid);
        Ok(())
    }

    dfs(
        repo,
        tip,
        &stop_ancestors,
        already_mapped_oids,
        &mut visited,
        &mut postorder,
    )?;
    Ok(postorder)
}

fn parse_trailer_value(message: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    for line in message.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix(&prefix) {
            let val = rest.trim();
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
}

fn capture_tree_snapshot_to_cas(
    repo: &gix::Repository,
    tree_id: gix::ObjectId,
    ignore_dirs: &[String],
    cas: &CasStore,
) -> Result<Blake3Hash, RpcError> {
    let tree = repo
        .find_tree(tree_id)
        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
    let entries = tree
        .traverse()
        .breadthfirst
        .files()
        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
    let mut files = BTreeMap::new();
    for entry in entries {
        if !matches!(
            entry.mode.kind(),
            EntryKind::Blob | EntryKind::BlobExecutable
        ) {
            continue;
        }
        let rel_str = entry.filepath.to_string();
        let rel = snapshot_key_to_rel_path(&rel_str);
        if rel.as_os_str().is_empty() || is_ignored_relative_path(&rel, ignore_dirs) {
            continue;
        }
        let blob = repo
            .find_blob(entry.oid)
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
        let h = cas
            .put_bytes(&blob.data)
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
        files.insert(normalize_snapshot_key(&rel), h);
    }
    let snapshot = WorkspaceSnapshot { files };
    let encoded = rmp_serde::to_vec_named(&snapshot)
        .map_err(|e| RpcError::new(RpcErrorCode::Internal, e.to_string()))?;
    cas.put_bytes(&encoded)
        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))
}

fn sync_trunk_worktree_from_tree(
    repo: &gix::Repository,
    repo_root: &Path,
    old_tree_id: Option<gix::ObjectId>,
    new_tree_id: gix::ObjectId,
    ignore_dirs: &[String],
) -> Result<(), RpcError> {
    let mut old_paths: BTreeSet<PathBuf> = BTreeSet::new();
    if let Some(otid) = old_tree_id
        && let Ok(old_tree) = repo.find_tree(otid)
        && let Ok(entries) = old_tree.traverse().breadthfirst.files()
    {
        for entry in entries {
            let rel = snapshot_key_to_rel_path(&entry.filepath.to_string());
            if !rel.as_os_str().is_empty() && !is_ignored_relative_path(&rel, ignore_dirs) {
                old_paths.insert(rel);
            }
        }
    }

    let new_tree = repo
        .find_tree(new_tree_id)
        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
    let new_entries = new_tree
        .traverse()
        .breadthfirst
        .files()
        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;

    let mut new_paths: BTreeSet<PathBuf> = BTreeSet::new();
    let mut writes: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    for entry in new_entries {
        if !matches!(
            entry.mode.kind(),
            EntryKind::Blob | EntryKind::BlobExecutable
        ) {
            continue;
        }
        let rel = snapshot_key_to_rel_path(&entry.filepath.to_string());
        if rel.as_os_str().is_empty() || is_ignored_relative_path(&rel, ignore_dirs) {
            continue;
        }
        let blob = repo
            .find_blob(entry.oid)
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
        new_paths.insert(rel.clone());
        writes.push((rel, blob.data.clone()));
    }

    let deletes: Vec<PathBuf> = old_paths.difference(&new_paths).cloned().collect();

    #[cfg(windows)]
    {
        let delete_set: HashSet<&PathBuf> = deletes.iter().collect();
        let disk_rels = collect_non_ignored_files(repo_root, ignore_dirs);
        let retained_trunk: Vec<&PathBuf> = disk_rels
            .iter()
            .filter(|p| !delete_set.contains(*p))
            .collect();
        if let Some((_path, reason)) = crate::viewport_mgr::windows_find_case_collision(
            writes.iter().map(|(p, _)| p),
            retained_trunk,
        ) {
            return Err(RpcError::new(
                RpcErrorCode::Conflict,
                format!("pull aborted: {reason}"),
            ));
        }
    }

    crate::viewport_mgr::apply_workspace_deletes_and_writes(
        repo_root,
        &deletes,
        writes.iter().map(|(p, b)| (p, b.as_slice())),
    )?;

    if let Ok(mut idx) = repo.index_from_tree(&new_tree_id) {
        let _ = idx.write(Default::default());
    }
    Ok(())
}

fn read_tree_files_map(
    repo: &gix::Repository,
    tree_id: gix::ObjectId,
    ignore_dirs: &[String],
) -> Result<BTreeMap<PathBuf, Vec<u8>>, RpcError> {
    let tree = repo
        .find_tree(tree_id)
        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
    let entries = tree
        .traverse()
        .breadthfirst
        .files()
        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
    let mut out = BTreeMap::new();
    for entry in entries {
        if !matches!(
            entry.mode.kind(),
            EntryKind::Blob | EntryKind::BlobExecutable
        ) {
            continue;
        }
        let rel = snapshot_key_to_rel_path(&entry.filepath.to_string());
        if rel.as_os_str().is_empty() || is_ignored_relative_path(&rel, ignore_dirs) {
            continue;
        }
        let blob = repo
            .find_blob(entry.oid)
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
        out.insert(rel, blob.data.clone());
    }
    Ok(out)
}

fn load_snapshot_bytes_map(
    cas: &CasStore,
    snap_hash: Blake3Hash,
) -> Result<BTreeMap<PathBuf, Vec<u8>>, RpcError> {
    let map = load_workspace_snapshot(cas, snap_hash)?;
    let mut out = BTreeMap::new();
    for (rel, bh) in map {
        let bytes = cas
            .get_bytes(&bh)
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
        out.insert(rel, bytes);
    }
    Ok(out)
}

fn store_bytes_map_snapshot(
    cas: &CasStore,
    files: &BTreeMap<PathBuf, Vec<u8>>,
) -> Result<Blake3Hash, RpcError> {
    let mut snap_files = BTreeMap::new();
    for (rel, bytes) in files {
        let key = normalize_snapshot_key(rel);
        if key.is_empty() {
            continue;
        }
        let h = cas
            .put_bytes(bytes)
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
        snap_files.insert(key, h);
    }
    let snapshot = WorkspaceSnapshot { files: snap_files };
    let encoded = rmp_serde::to_vec_named(&snapshot)
        .map_err(|e| RpcError::new(RpcErrorCode::Internal, e.to_string()))?;
    cas.put_bytes(&encoded)
        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))
}

fn sync_trunk_worktree_from_map(
    repo: &gix::Repository,
    repo_root: &Path,
    old_tree_id: Option<gix::ObjectId>,
    index_tree_id: gix::ObjectId,
    files: &BTreeMap<PathBuf, Vec<u8>>,
    ignore_dirs: &[String],
) -> Result<(), RpcError> {
    let mut old_paths: BTreeSet<PathBuf> = BTreeSet::new();
    if let Some(otid) = old_tree_id
        && let Ok(old_tree) = repo.find_tree(otid)
        && let Ok(entries) = old_tree.traverse().breadthfirst.files()
    {
        for entry in entries {
            let rel = snapshot_key_to_rel_path(&entry.filepath.to_string());
            if !rel.as_os_str().is_empty() && !is_ignored_relative_path(&rel, ignore_dirs) {
                old_paths.insert(rel);
            }
        }
    }

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
        let disk_rels = collect_non_ignored_files(repo_root, ignore_dirs);
        let retained_trunk: Vec<&PathBuf> = disk_rels
            .iter()
            .filter(|p| !delete_set.contains(*p))
            .collect();
        if let Some((_path, reason)) = crate::viewport_mgr::windows_find_case_collision(
            writes.iter().map(|(p, _)| *p),
            retained_trunk,
        ) {
            return Err(RpcError::new(
                RpcErrorCode::Conflict,
                format!("pull aborted: {reason}"),
            ));
        }
    }

    crate::viewport_mgr::apply_workspace_deletes_and_writes(repo_root, &deletes, writes)?;

    if let Ok(mut idx) = repo.index_from_tree(&index_tree_id) {
        let _ = idx.write(Default::default());
    }
    Ok(())
}

fn merge_3way_trees(
    base_files: &BTreeMap<PathBuf, Vec<u8>>,
    remote_files: &BTreeMap<PathBuf, Vec<u8>>,
    local_files: &BTreeMap<PathBuf, Vec<u8>>,
    capsule_id: &str,
    branch_name: &str,
    cas: &CasStore,
) -> Result<(BTreeMap<PathBuf, Vec<u8>>, Blake3Hash), RpcError> {
    let mut all_rels: BTreeSet<PathBuf> = base_files.keys().cloned().collect();
    all_rels.extend(remote_files.keys().cloned());
    all_rels.extend(local_files.keys().cloned());

    let mut merged: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
    let mut merge_base_files: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
    let mut all_merge_patches: Vec<AstPatch> = Vec::new();
    let mut both_modified_paths: BTreeSet<PathBuf> = BTreeSet::new();

    for rel in all_rels {
        let b_opt = base_files.get(&rel);
        let r_opt = remote_files.get(&rel);
        let l_opt = local_files.get(&rel);

        let r_same = r_opt == b_opt;
        let l_same = l_opt == b_opt;

        if r_same && l_same {
            if let Some(b) = b_opt {
                merged.insert(rel, b.clone());
            }
            continue;
        }

        let fid = compute_logical_file_id(&rel);

        if !r_same && l_same {
            if let Some(r) = r_opt {
                let lang = LanguageKind::detect(&rel, r);
                if lang.is_code() {
                    let b_slice = b_opt.map(Vec::as_slice).unwrap_or(&[]);
                    let r_ctx = DiffContext {
                        logical_file_id: fid,
                        file_path: &rel,
                        language: lang,
                        viewport_id: "__remote__",
                        actor_id: "__remote__",
                        lamport_ts: 1,
                        parents: Vec::new(),
                    };
                    if let Ok(r_patch) = diff_file_to_patch(&r_ctx, b_slice, r, None, Some(cas)) {
                        merge_base_files.insert(rel.clone(), b_slice.to_vec());
                        all_merge_patches.push(r_patch);
                    }
                }
                merged.insert(rel, r.clone());
            } else if let Some(b) = b_opt {
                let lang = LanguageKind::detect(&rel, b);
                if lang.is_code() {
                    let r_ctx = DiffContext {
                        logical_file_id: fid,
                        file_path: &rel,
                        language: lang,
                        viewport_id: "__remote__",
                        actor_id: "__remote__",
                        lamport_ts: 1,
                        parents: Vec::new(),
                    };
                    if let Ok(r_patch) = diff_file_to_patch(&r_ctx, b, &[], None, Some(cas)) {
                        merge_base_files.insert(rel.clone(), b.clone());
                        all_merge_patches.push(r_patch);
                    }
                }
            }
            continue;
        }

        if r_same && !l_same {
            if let Some(l) = l_opt {
                let lang = LanguageKind::detect(&rel, l);
                if lang.is_code() {
                    let b_slice = b_opt.map(Vec::as_slice).unwrap_or(&[]);
                    let l_ctx = DiffContext {
                        logical_file_id: fid,
                        file_path: &rel,
                        language: lang,
                        viewport_id: capsule_id,
                        actor_id: capsule_id,
                        lamport_ts: 2,
                        parents: Vec::new(),
                    };
                    if let Ok(l_patch) = diff_file_to_patch(&l_ctx, b_slice, l, None, Some(cas)) {
                        merge_base_files.insert(rel.clone(), b_slice.to_vec());
                        all_merge_patches.push(l_patch);
                    }
                }
                merged.insert(rel, l.clone());
            } else if let Some(b) = b_opt {
                let lang = LanguageKind::detect(&rel, b);
                if lang.is_code() {
                    let l_ctx = DiffContext {
                        logical_file_id: fid,
                        file_path: &rel,
                        language: lang,
                        viewport_id: capsule_id,
                        actor_id: capsule_id,
                        lamport_ts: 2,
                        parents: Vec::new(),
                    };
                    if let Ok(l_patch) = diff_file_to_patch(&l_ctx, b, &[], None, Some(cas)) {
                        merge_base_files.insert(rel.clone(), b.clone());
                        all_merge_patches.push(l_patch);
                    }
                }
            }
            continue;
        }

        // Both remote and local unpushed capsule changed `rel`.
        if r_opt == l_opt {
            if let Some(r) = r_opt {
                merged.insert(rel, r.clone());
            }
            continue;
        }

        if b_opt.is_some() && (r_opt.is_none() ^ l_opt.is_none()) {
            return Err(RpcError::new(
                RpcErrorCode::Conflict,
                format!(
                    "pull aborted: delete-vs-modify conflict on `{}` between unpushed capsule `{capsule_id}` and remote `{branch_name}`",
                    rel.display()
                ),
            ));
        }

        let b_bytes = b_opt.map(Vec::as_slice).unwrap_or(&[]);
        let r_slice = r_opt.map(Vec::as_slice).unwrap_or(&[]);
        let l_slice = l_opt.map(Vec::as_slice).unwrap_or(&[]);

        let r_lang = LanguageKind::detect(&rel, if r_slice.is_empty() { b_bytes } else { r_slice });
        let l_lang = LanguageKind::detect(&rel, if l_slice.is_empty() { b_bytes } else { l_slice });

        if r_lang.is_code()
            && let Ok(r_str) = std::str::from_utf8(r_slice)
            && !r_str.is_empty()
            && parse_source(r_lang, r_str, None).is_ok_and(|p| p.tree.root_node().has_error())
        {
            return Err(RpcError::new(
                RpcErrorCode::Conflict,
                format!(
                    "pull aborted: remote `{branch_name}` has syntax errors in `{}` conflicting with unpushed capsule `{capsule_id}`",
                    rel.display()
                ),
            ));
        }
        if l_lang.is_code()
            && let Ok(l_str) = std::str::from_utf8(l_slice)
            && !l_str.is_empty()
            && parse_source(l_lang, l_str, None).is_ok_and(|p| p.tree.root_node().has_error())
        {
            return Err(RpcError::new(
                RpcErrorCode::Conflict,
                format!(
                    "pull aborted: unpushed capsule `{capsule_id}` has syntax errors in `{}` conflicting with remote `{branch_name}`",
                    rel.display()
                ),
            ));
        }

        let r_ctx = DiffContext {
            logical_file_id: fid,
            file_path: &rel,
            language: r_lang,
            viewport_id: "__remote__",
            actor_id: "__remote__",
            lamport_ts: 1,
            parents: Vec::new(),
        };
        let l_ctx = DiffContext {
            logical_file_id: fid,
            file_path: &rel,
            language: l_lang,
            viewport_id: capsule_id,
            actor_id: capsule_id,
            lamport_ts: 2,
            parents: Vec::new(),
        };

        let r_patch = diff_file_to_patch(&r_ctx, b_bytes, r_slice, None, Some(cas)).map_err(|e| {
            RpcError::new(
                RpcErrorCode::Conflict,
                format!(
                    "pull aborted: failed to diff remote `{}` for unpushed capsule `{capsule_id}`: {e}",
                    rel.display()
                ),
            )
        })?;
        let l_patch = diff_file_to_patch(&l_ctx, b_bytes, l_slice, None, Some(cas)).map_err(|e| {
            RpcError::new(
                RpcErrorCode::Conflict,
                format!(
                    "pull aborted: failed to diff unpushed capsule `{capsule_id}` on `{}`: {e}",
                    rel.display()
                ),
            )
        })?;

        merge_base_files.insert(rel.clone(), b_bytes.to_vec());
        all_merge_patches.push(r_patch);
        all_merge_patches.push(l_patch);
        both_modified_paths.insert(rel);
    }

    if !all_merge_patches.is_empty() {
        let report = merge_patches(&merge_base_files, &all_merge_patches, Some(cas)).map_err(|e| {
            RpcError::new(
                RpcErrorCode::Conflict,
                format!(
                    "pull aborted: CST merge failed between unpushed capsule `{capsule_id}` and remote `{branch_name}`: {e}"
                ),
            )
        })?;
        if !matches!(report.outcome, MergeOutcome::Clean { .. }) {
            let details: Vec<String> = report
                .conflicts
                .iter()
                .map(|c| match c {
                    MergeOutcome::StructuralConflict { file, reason, .. } => {
                        format!("{}: {reason}", file.display())
                    }
                    MergeOutcome::SemanticConflict {
                        symbol_fqn, detail, ..
                    } => format!("{symbol_fqn}: {detail}"),
                    MergeOutcome::Clean { .. } => String::new(),
                })
                .filter(|s| !s.is_empty())
                .collect();
            return Err(RpcError::new(
                RpcErrorCode::Conflict,
                format!(
                    "pull aborted: merge conflict between unpushed capsule `{capsule_id}` and remote `{branch_name}` ({})",
                    details.join("; ")
                ),
            ));
        }
        for rel in both_modified_paths {
            let content = report.merged_files.get(&rel).ok_or_else(|| {
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
            merged.insert(rel, bytes);
        }
    }

    #[cfg(windows)]
    {
        if let Some((_path, reason)) = crate::viewport_mgr::windows_find_case_collision(
            merged.keys(),
            std::iter::empty::<&PathBuf>(),
        ) {
            return Err(RpcError::new(
                RpcErrorCode::Conflict,
                format!(
                    "pull aborted: {reason} between unpushed capsule `{capsule_id}` and remote `{branch_name}`"
                ),
            ));
        }
    }

    let snap_hash = store_bytes_map_snapshot(cas, &merged)?;
    Ok((merged, snap_hash))
}

enum ViewportRebasePlan {
    Clean {
        writes: BTreeMap<PathBuf, Vec<u8>>,
        deletes: Vec<PathBuf>,
        updated_files: usize,
    },
    Conflict(Vec<RadarConflictItem>),
}

#[allow(clippy::too_many_arguments)]
fn compute_viewport_rebase_plan(
    vp: &ViewportRecord,
    old_trunk_snapshot_cas: Blake3Hash,
    new_trunk_snapshot_cas: Blake3Hash,
    branch_name: &str,
    ignore_dirs: &[String],
    cas: &CasStore,
) -> Result<ViewportRebasePlan, RpcError> {
    let base_snap = vp.base_snapshot_cas.unwrap_or(old_trunk_snapshot_cas);
    let base_map = load_workspace_snapshot(cas, base_snap)?;
    let trunk_map = load_workspace_snapshot(cas, new_trunk_snapshot_cas)?;
    let vp_rels: BTreeSet<PathBuf> = collect_non_ignored_files(&vp.path, ignore_dirs)
        .into_iter()
        .collect();

    let mut all_rels: BTreeSet<PathBuf> = base_map.keys().cloned().collect();
    all_rels.extend(trunk_map.keys().cloned());
    all_rels.extend(vp_rels.iter().cloned());

    let mut merge_base_files: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
    let mut all_merge_patches: Vec<AstPatch> = Vec::new();
    let mut both_modified_paths: BTreeSet<PathBuf> = BTreeSet::new();
    let mut writes: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
    let mut deletes: Vec<PathBuf> = Vec::new();
    let mut conflicts: Vec<RadarConflictItem> = Vec::new();
    let peer_label = format!("git:{branch_name}");

    for rel in all_rels {
        let b_hash_opt = base_map.get(&rel).copied();
        let t_hash_opt = trunk_map.get(&rel).copied();
        let v_opt = if vp_rels.contains(&rel) {
            std::fs::read(vp.path.join(&rel)).ok()
        } else {
            None
        };

        let t_same_as_base = t_hash_opt == b_hash_opt;
        let v_same_as_base = match (&v_opt, b_hash_opt) {
            (None, None) => true,
            (Some(vb), Some(bh)) => Blake3Hash::from_bytes(vb) == bh,
            _ => false,
        };

        if t_same_as_base && v_same_as_base {
            continue;
        }

        let fid = compute_logical_file_id(&rel);

        if !t_same_as_base && v_same_as_base {
            match t_hash_opt {
                Some(th) => {
                    let t_bytes = cas
                        .get_bytes(&th)
                        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
                    if LanguageKind::detect(&rel, &t_bytes).is_code() {
                        let b_bytes = match b_hash_opt {
                            Some(bh) => cas.get_bytes(&bh).unwrap_or_default(),
                            None => Vec::new(),
                        };
                        let lang = LanguageKind::detect(&rel, &t_bytes);
                        let t_ctx = DiffContext {
                            logical_file_id: fid,
                            file_path: &rel,
                            language: lang,
                            viewport_id: "__trunk__",
                            actor_id: "__trunk__",
                            lamport_ts: 1,
                            parents: Vec::new(),
                        };
                        if let Ok(t_patch) =
                            diff_file_to_patch(&t_ctx, &b_bytes, &t_bytes, None, Some(cas))
                        {
                            merge_base_files.insert(rel.clone(), b_bytes);
                            all_merge_patches.push(t_patch);
                        }
                    }
                    writes.insert(rel, t_bytes);
                }
                None => {
                    if let Some(bh) = b_hash_opt
                        && let Ok(b_bytes) = cas.get_bytes(&bh)
                        && LanguageKind::detect(&rel, &b_bytes).is_code()
                    {
                        let lang = LanguageKind::detect(&rel, &b_bytes);
                        let t_ctx = DiffContext {
                            logical_file_id: fid,
                            file_path: &rel,
                            language: lang,
                            viewport_id: "__trunk__",
                            actor_id: "__trunk__",
                            lamport_ts: 1,
                            parents: Vec::new(),
                        };
                        if let Ok(t_patch) =
                            diff_file_to_patch(&t_ctx, &b_bytes, &[], None, Some(cas))
                        {
                            merge_base_files.insert(rel.clone(), b_bytes);
                            all_merge_patches.push(t_patch);
                        }
                    }
                    deletes.push(rel);
                }
            }
            continue;
        }

        if t_same_as_base && !v_same_as_base {
            let b_bytes = match b_hash_opt {
                Some(bh) => cas.get_bytes(&bh).unwrap_or_default(),
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
            if lang.is_code() {
                let v_ctx = DiffContext {
                    logical_file_id: fid,
                    file_path: &rel,
                    language: lang,
                    viewport_id: &vp.id,
                    actor_id: &vp.agent_id,
                    lamport_ts: vp.lamport_ts.max(2),
                    parents: Vec::new(),
                };
                if let Ok(v_patch) = diff_file_to_patch(&v_ctx, &b_bytes, v_slice, None, Some(cas))
                {
                    merge_base_files.insert(rel.clone(), b_bytes);
                    all_merge_patches.push(v_patch);
                }
            }
            continue;
        }

        // Case 4: both trunk and viewport changed `rel` relative to base.
        let t_opt = match t_hash_opt {
            Some(th) => Some(
                cas.get_bytes(&th)
                    .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?,
            ),
            None => None,
        };
        if t_opt == v_opt {
            continue;
        }
        if b_hash_opt.is_some() && (t_opt.is_none() ^ v_opt.is_none()) {
            conflicts.push(RadarConflictItem {
                peer_viewport_id: String::new(),
                peer_viewport_name: peer_label.clone(),
                level: RadarLevel::Red,
                kind: "rebase_conflict".to_string(),
                file_path: Some(rel.clone()),
                symbol_fqn: None,
                message: format!(
                    "delete-vs-modify rebase conflict on `{}` against `{peer_label}`",
                    rel.display()
                ),
            });
            continue;
        }

        let b_bytes = match b_hash_opt {
            Some(bh) => cas.get_bytes(&bh).unwrap_or_default(),
            None => Vec::new(),
        };
        let t_slice = t_opt.as_deref().unwrap_or(&[]);
        let v_slice = v_opt.as_deref().unwrap_or(&[]);
        let t_lang = LanguageKind::detect(&rel, if t_slice.is_empty() { &b_bytes } else { t_slice });
        let v_lang = LanguageKind::detect(&rel, if v_slice.is_empty() { &b_bytes } else { v_slice });

        if v_lang.is_code()
            && let Ok(v_str) = std::str::from_utf8(v_slice)
            && !v_str.is_empty()
            && parse_source(v_lang, v_str, None).is_ok_and(|p| p.tree.root_node().has_error())
        {
            conflicts.push(RadarConflictItem {
                peer_viewport_id: String::new(),
                peer_viewport_name: peer_label.clone(),
                level: RadarLevel::Red,
                kind: "rebase_conflict".to_string(),
                file_path: Some(rel.clone()),
                symbol_fqn: None,
                message: format!(
                    "viewport `{}` has syntax errors on `{}` modified in `{peer_label}`",
                    vp.name,
                    rel.display()
                ),
            });
            continue;
        }

        let t_ctx = DiffContext {
            logical_file_id: fid,
            file_path: &rel,
            language: t_lang,
            viewport_id: "__trunk__",
            actor_id: "__trunk__",
            lamport_ts: 1,
            parents: Vec::new(),
        };
        let v_ctx = DiffContext {
            logical_file_id: fid,
            file_path: &rel,
            language: v_lang,
            viewport_id: &vp.id,
            actor_id: &vp.agent_id,
            lamport_ts: vp.lamport_ts.max(2),
            parents: Vec::new(),
        };

        let t_patch = match diff_file_to_patch(&t_ctx, &b_bytes, t_slice, None, Some(cas)) {
            Ok(p) => p,
            Err(err) => {
                conflicts.push(RadarConflictItem {
                    peer_viewport_id: String::new(),
                    peer_viewport_name: peer_label.clone(),
                    level: RadarLevel::Red,
                    kind: "rebase_conflict".to_string(),
                    file_path: Some(rel.clone()),
                    symbol_fqn: None,
                    message: format!("failed to diff trunk `{}` during rebase: {err}", rel.display()),
                });
                continue;
            }
        };
        let v_patch = match diff_file_to_patch(&v_ctx, &b_bytes, v_slice, None, Some(cas)) {
            Ok(p) => p,
            Err(err) => {
                conflicts.push(RadarConflictItem {
                    peer_viewport_id: String::new(),
                    peer_viewport_name: peer_label.clone(),
                    level: RadarLevel::Red,
                    kind: "rebase_conflict".to_string(),
                    file_path: Some(rel.clone()),
                    symbol_fqn: None,
                    message: format!("failed to diff viewport `{}` during rebase: {err}", rel.display()),
                });
                continue;
            }
        };

        merge_base_files.insert(rel.clone(), b_bytes);
        all_merge_patches.push(t_patch);
        all_merge_patches.push(v_patch);
        both_modified_paths.insert(rel);
    }

    if conflicts.is_empty() && !all_merge_patches.is_empty() {
        match merge_patches(&merge_base_files, &all_merge_patches, Some(cas)) {
            Ok(report) => {
                for c in &report.conflicts {
                    match c {
                        MergeOutcome::StructuralConflict {
                            file,
                            anchor,
                            reason,
                            ..
                        } => {
                            conflicts.push(RadarConflictItem {
                                peer_viewport_id: String::new(),
                                peer_viewport_name: peer_label.clone(),
                                level: RadarLevel::Red,
                                kind: "rebase_conflict".to_string(),
                                file_path: Some(file.clone()),
                                symbol_fqn: anchor.as_ref().map(|a| a.symbol_fqn.clone()),
                                message: reason.clone(),
                            });
                        }
                        MergeOutcome::SemanticConflict {
                            symbol_fqn, detail, ..
                        } => {
                            conflicts.push(RadarConflictItem {
                                peer_viewport_id: String::new(),
                                peer_viewport_name: peer_label.clone(),
                                level: RadarLevel::Red,
                                kind: "rebase_conflict".to_string(),
                                file_path: None,
                                symbol_fqn: Some(symbol_fqn.clone()),
                                message: detail.clone(),
                            });
                        }
                        MergeOutcome::Clean { .. } => {}
                    }
                }
                if conflicts.is_empty() {
                    for rel in both_modified_paths {
                        if let Some(content) = report.merged_files.get(&rel) {
                            let bytes = match content {
                                MergedFileContent::Text(s) => s.as_bytes().to_vec(),
                                MergedFileContent::Binary(manifest) => cas
                                    .get_chunked_binary(manifest)
                                    .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?,
                            };
                            writes.insert(rel, bytes);
                        }
                    }
                }
            }
            Err(err) => {
                conflicts.push(RadarConflictItem {
                    peer_viewport_id: String::new(),
                    peer_viewport_name: peer_label.clone(),
                    level: RadarLevel::Red,
                    kind: "rebase_conflict".to_string(),
                    file_path: both_modified_paths.iter().next().cloned(),
                    symbol_fqn: None,
                    message: format!("rebase CST merge error: {err}"),
                });
            }
        }
    }

    if conflicts.is_empty() {
        let delete_set: HashSet<PathBuf> = deletes.iter().cloned().collect();
        for rel in writes.keys() {
            if let Some(reason) =
                crate::viewport_mgr::find_write_path_conflict(&vp.path, rel, &delete_set)?
            {
                conflicts.push(RadarConflictItem {
                    peer_viewport_id: String::new(),
                    peer_viewport_name: peer_label.clone(),
                    level: RadarLevel::Red,
                    kind: "rebase_conflict".to_string(),
                    file_path: Some(rel.clone()),
                    symbol_fqn: None,
                    message: format!("path-type rebase conflict against `{peer_label}`: {reason}"),
                });
            }
        }
        #[cfg(windows)]
        {
            let fold_key = |p: &Path| -> String {
                crate::viewport_mgr::windows_case_fold_path(p)
            };
            let delete_case_keys: HashSet<String> = deletes.iter().map(|p| fold_key(p)).collect();
            let mut vp_case_map: std::collections::HashMap<String, &PathBuf> =
                std::collections::HashMap::new();
            let mut vp_dir_map: std::collections::HashMap<String, PathBuf> =
                std::collections::HashMap::new();
            for vrel in &vp_rels {
                let k = fold_key(vrel);
                if !delete_case_keys.contains(&k) {
                    vp_case_map.insert(k, vrel);
                    let mut cur = vrel.parent();
                    while let Some(dir) = cur {
                        if dir.as_os_str().is_empty() {
                            break;
                        }
                        vp_dir_map
                            .entry(fold_key(dir))
                            .or_insert_with(|| dir.to_path_buf());
                        cur = dir.parent();
                    }
                }
            }
            let mut write_case_map: std::collections::HashMap<String, &PathBuf> =
                std::collections::HashMap::new();
            let mut write_dir_map: std::collections::HashMap<String, PathBuf> =
                std::collections::HashMap::new();
            for rel in writes.keys() {
                let k = fold_key(rel);
                if let Some(prev) = write_case_map.insert(k.clone(), rel) {
                    conflicts.push(RadarConflictItem {
                        peer_viewport_id: String::new(),
                        peer_viewport_name: peer_label.clone(),
                        level: RadarLevel::Red,
                        kind: "rebase_conflict".to_string(),
                        file_path: Some(rel.clone()),
                        symbol_fqn: None,
                        message: format!(
                            "case-colliding rebase writes `{}` and `{}` against `{peer_label}`",
                            prev.display(),
                            rel.display()
                        ),
                    });
                } else if let Some(existing) = vp_case_map.get(&k)
                    && *existing != rel
                {
                    conflicts.push(RadarConflictItem {
                        peer_viewport_id: String::new(),
                        peer_viewport_name: peer_label.clone(),
                        level: RadarLevel::Red,
                        kind: "rebase_conflict".to_string(),
                        file_path: Some(rel.clone()),
                        symbol_fqn: None,
                        message: format!(
                            "case-colliding rebase conflict between `{}` and viewport `{}` against `{peer_label}`",
                            rel.display(),
                            existing.display()
                        ),
                    });
                }

                let mut cur = rel.parent();
                while let Some(dir) = cur {
                    if dir.as_os_str().is_empty() {
                        break;
                    }
                    let dkey = fold_key(dir);
                    if let Some(existing_vp_dir) = vp_dir_map.get(&dkey)
                        && existing_vp_dir != dir
                    {
                        conflicts.push(RadarConflictItem {
                            peer_viewport_id: String::new(),
                            peer_viewport_name: peer_label.clone(),
                            level: RadarLevel::Red,
                            kind: "rebase_conflict".to_string(),
                            file_path: Some(rel.clone()),
                            symbol_fqn: None,
                            message: format!(
                                "directory-casing rebase conflict between `{}` (`{}`) and viewport directory `{}` against `{peer_label}`",
                                rel.display(),
                                dir.display(),
                                existing_vp_dir.display()
                            ),
                        });
                        break;
                    }
                    if let Some(existing_write_dir) = write_dir_map.get(&dkey) {
                        if existing_write_dir != dir {
                            conflicts.push(RadarConflictItem {
                                peer_viewport_id: String::new(),
                                peer_viewport_name: peer_label.clone(),
                                level: RadarLevel::Red,
                                kind: "rebase_conflict".to_string(),
                                file_path: Some(rel.clone()),
                                symbol_fqn: None,
                                message: format!(
                                    "directory-casing rebase conflict between `{}` (`{}`) and `{}` against `{peer_label}`",
                                    rel.display(),
                                    dir.display(),
                                    existing_write_dir.display()
                                ),
                            });
                            break;
                        }
                    } else {
                        write_dir_map.insert(dkey, dir.to_path_buf());
                    }
                    cur = dir.parent();
                }
            }
        }
    }

    if !conflicts.is_empty() {
        return Ok(ViewportRebasePlan::Conflict(conflicts));
    }

    let updated_files = writes.len() + deletes.len();
    Ok(ViewportRebasePlan::Clean {
        writes,
        deletes,
        updated_files,
    })
}

fn rollback_git_refs(
    local_repo: &gix::Repository,
    branch_ref: &str,
    orig_local_branch_tip: Option<gix::ObjectId>,
    notes_ref: &str,
    orig_local_notes_tip: Option<gix::ObjectId>,
) {
    let mut rollback_edits = Vec::new();
    let cur_branch_tip = try_peel_ref_id(local_repo, branch_ref).ok().flatten();
    if cur_branch_tip != orig_local_branch_tip
        && let Ok(full_branch) = gix::refs::FullName::try_from(branch_ref)
    {
        match orig_local_branch_tip {
            Some(orig) => rollback_edits.push(RefEdit::update(
                full_branch,
                orig,
                PreviousValue::Any,
                "loom git pull rollback",
            )),
            None => rollback_edits.push(RefEdit::delete(full_branch, PreviousValue::Any)),
        }
    }
    let cur_notes_tip = try_peel_ref_id(local_repo, notes_ref).ok().flatten();
    if cur_notes_tip != orig_local_notes_tip
        && let Ok(notes_full_ref) = gix::refs::FullName::try_from(notes_ref)
    {
        match orig_local_notes_tip {
            Some(orig) => rollback_edits.push(RefEdit::update(
                notes_full_ref,
                orig,
                PreviousValue::Any,
                "loom git pull rollback notes",
            )),
            None => rollback_edits.push(RefEdit::delete(notes_full_ref, PreviousValue::Any)),
        }
    }
    if !rollback_edits.is_empty() {
        let local_committer = local_repo.committer().transpose().ok().flatten();
        let _ = local_repo.edit_references_as(rollback_edits, local_committer);
    }
}

pub async fn git_pull(
    ctx: &Arc<DaemonContext>,
    remote: Option<String>,
    branch: Option<String>,
) -> Result<GitPullResult, RpcError> {
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

    if !meta.colocate_git && !ctx.repo_root.join(".git").exists() {
        return Err(RpcError::new(
            RpcErrorCode::InvalidState,
            "repository is not git-colocated; run `loom init --colocate-git` in a git repository",
        ));
    }

    let seal_guard = ctx.trunk_seal_lock.lock().await;

    let existing_git_maps = ctx
        .db
        .list_git_maps()
        .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
    let mut mapped_oids: HashSet<String> = HashSet::new();
    let mut mapped_note_edges: HashSet<String> = HashSet::new();
    let mut mapped_capsule_ids: HashSet<String> = HashSet::new();
    for (k, entry) in existing_git_maps {
        if let Some(oid) = k.strip_prefix("oid:") {
            mapped_oids.insert(oid.to_string());
        } else if let Some(eid) = k.strip_prefix("note_edge:") {
            mapped_note_edges.insert(eid.to_string());
        } else if let Some(cid) = k.strip_prefix("cap:") {
            mapped_capsule_ids.insert(cid.to_string());
        }
        let _ = entry;
    }
    let existing_edges = ctx
        .db
        .list_all_negative_edges()
        .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
    let mut existing_edge_ids: HashSet<String> =
        existing_edges.into_iter().map(|e| e.edge_id).collect();
    let mut all_capsules = ctx
        .db
        .list_capsules()
        .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
    all_capsules.sort_by(|a, b| {
        a.created_at_ns
            .cmp(&b.created_at_ns)
            .then_with(|| a.capsule_id.cmp(&b.capsule_id))
    });
    let existing_capsules: HashSet<String> = all_capsules
        .iter()
        .map(|c| c.capsule_id.clone())
        .collect();
    let capsule_parents: std::collections::HashMap<String, Vec<String>> = all_capsules
        .iter()
        .map(|c| (c.capsule_id.clone(), c.parent_capsule_ids.clone()))
        .collect();
    let unpushed_capsules: Vec<CausalCapsule> = all_capsules
        .iter()
        .filter(|cap| cap.git_commit_oid.is_none() && !mapped_capsule_ids.contains(&cap.capsule_id))
        .cloned()
        .collect();
    let unpushed_ids: HashSet<String> = unpushed_capsules
        .iter()
        .map(|c| c.capsule_id.clone())
        .collect();
    let pulled_base_cap_id = {
        let mut found: Option<String> = None;
        if let Some(ref head_id) = meta.head_capsule_id {
            let mut queue = std::collections::VecDeque::new();
            let mut visited = HashSet::new();
            queue.push_back(head_id.clone());
            while let Some(cid) = queue.pop_front() {
                if !visited.insert(cid.clone()) {
                    continue;
                }
                if !unpushed_ids.contains(&cid) {
                    if existing_capsules.contains(&cid) {
                        found = Some(cid);
                        break;
                    }
                    continue;
                }
                if let Some(parents) = capsule_parents.get(&cid) {
                    for p in parents {
                        if !visited.contains(p) {
                            queue.push_back(p.clone());
                        }
                    }
                }
            }
        }
        found.or_else(|| {
            all_capsules
                .iter()
                .rev()
                .find(|c| !unpushed_ids.contains(&c.capsule_id))
                .map(|c| c.capsule_id.clone())
        })
    };

    let repo_root = ctx.repo_root.clone();
    let cas_dir = ctx.loom_dir.join("cas");
    let zstd_level = ctx.config.zstd_level;
    let ignore_dirs = ctx.config.radar.ignore_dirs.clone();
    let default_remote = ctx.config.git.default_remote.clone();
    let notes_ref = ctx.config.git.notes_ref.clone();
    let base_lsn = meta.next_lsn.max(1);

    struct PullPrepOutcome {
        result: GitPullResult,
        effects: Vec<StateEffect>,
        touched_symbols: Vec<String>,
        pulled_summaries: Vec<String>,
        last_pulled_capsule_id: Option<String>,
        new_head_capsule_id: Option<String>,
        old_trunk_snapshot_cas: Blake3Hash,
        new_trunk_snapshot_cas: Blake3Hash,
        branch_ref: String,
        notes_ref: String,
        orig_local_branch_tip: Option<gix::ObjectId>,
        orig_local_notes_tip: Option<gix::ObjectId>,
    }

    let prep = tokio::task::spawn_blocking(move || -> Result<PullPrepOutcome, RpcError> {
        let cas = CasStore::new(cas_dir, zstd_level)
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
        let local_repo = open_gix_repo_with_fallback(&repo_root, None).map_err(|e| {
            RpcError::new(
                RpcErrorCode::InvalidState,
                format!("failed to open local git repository: {e}"),
            )
        })?;
        let (remote_label, bare_path) = resolve_local_bare_remote(
            &repo_root,
            &local_repo,
            remote.as_deref(),
            &default_remote,
            gix::remote::Direction::Fetch,
        )?;
        let bare_repo = open_gix_repo_with_fallback(&bare_path, None).map_err(|e| {
            RpcError::new(
                RpcErrorCode::IoError,
                format!(
                    "failed to open bare remote repository `{}`: {e}",
                    bare_path.display()
                ),
            )
        })?;

        let branch_name = resolve_branch_name(&local_repo, branch.as_deref());
        let branch_ref = format!("refs/heads/{branch_name}");

        let remote_branch_tip = try_peel_ref_id(&bare_repo, &branch_ref)?;
        let remote_notes_tip = try_peel_ref_id(&bare_repo, &notes_ref)?;
        let old_local_tip = try_peel_ref_id(&local_repo, &branch_ref)?;
        let orig_local_notes_tip = try_peel_ref_id(&local_repo, &notes_ref)?;

        let old_trunk_snapshot_cas = capture_workspace_snapshot(&repo_root, &ignore_dirs, &cas)?;

        // Copy reachable objects into local ODB first (read-only w.r.t. refs/trunk/StateDb)
        // so `is_ancestor` and 3-way merge checks can inspect `remote_branch_tip`.
        let mut copy_roots = Vec::new();
        if let Some(id) = remote_branch_tip {
            copy_roots.push(id);
        }
        if let Some(id) = remote_notes_tip {
            copy_roots.push(id);
        }
        if !copy_roots.is_empty() {
            copy_reachable_objects(&bare_repo, &local_repo, &copy_roots)?;
        }

        // Check branch relationship before mutating refs, notes, disk, or StateDb.
        let (branch_advanced, active_branch_tip) = match (old_local_tip, remote_branch_tip) {
            (_, None) => (false, old_local_tip),
            (None, Some(rtip)) => (true, Some(rtip)),
            (Some(ltip), Some(rtip)) => {
                if ltip == rtip || is_ancestor(&local_repo, rtip, ltip) {
                    (false, Some(ltip))
                } else if is_ancestor(&local_repo, ltip, rtip) {
                    (true, Some(rtip))
                } else {
                    return Err(RpcError::new(
                        RpcErrorCode::Conflict,
                        format!(
                            "local branch `{branch_name}` ({ltip}) and remote ({rtip}) have diverged"
                        ),
                    ));
                }
            }
        };

        // If local repo has unpushed sealed capsules AND remote is ahead, run a non-destructive
        // 3-way merge BEFORE mutating refs/heads/<branch>, refs/notes/loom, trunk disk, or StateDb.
        let mut rebased_unpushed_capsules: Vec<CausalCapsule> = Vec::new();
        let mut merged_trunk_override: Option<(BTreeMap<PathBuf, Vec<u8>>, Blake3Hash)> = None;
        if branch_advanced
            && !unpushed_capsules.is_empty()
            && let Some(rtip) = remote_branch_tip
        {
            let rtip_tree_id = local_repo
                .find_commit(rtip)
                .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?
                .tree_id()
                .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?
                .detach();
            let remote_files = read_tree_files_map(&local_repo, rtip_tree_id, &ignore_dirs)?;
            let base_files = match old_local_tip {
                Some(ltip) => {
                    let ltip_tree_id = local_repo
                        .find_commit(ltip)
                        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?
                        .tree_id()
                        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?
                        .detach();
                    read_tree_files_map(&local_repo, ltip_tree_id, &ignore_dirs)?
                }
                None => BTreeMap::new(),
            };
            let fallback_local_files = load_snapshot_bytes_map(&cas, old_trunk_snapshot_cas)?;
            for mut cap in unpushed_capsules {
                let local_files = match cap.snapshot_cas_hash {
                    Some(sh) => load_snapshot_bytes_map(&cas, sh)?,
                    None => fallback_local_files.clone(),
                };
                let (merged_files, merged_snap) = merge_3way_trees(
                    &base_files,
                    &remote_files,
                    &local_files,
                    &cap.capsule_id,
                    &branch_name,
                    &cas,
                )?;
                cap.snapshot_cas_hash = Some(merged_snap);
                rebased_unpushed_capsules.push(cap);
                merged_trunk_override = Some((merged_files, merged_snap));
            }
        }

        let pull_res = (|| -> Result<PullPrepOutcome, RpcError> {
            if let Some(bnt) = remote_notes_tip {
                sync_local_notes_with_remote(&local_repo, &bare_repo, &notes_ref, bnt)?;
            }

            let updated_trunk = branch_advanced && active_branch_tip.is_some();

        let mut effects = Vec::new();
        let mut pulled_commit_oids = Vec::new();
        let mut restored_capsule_ids = Vec::new();
        let mut synthesized_capsule_ids = Vec::new();
        let mut restored_negative_edges = 0usize;
        let mut all_touched_symbols = BTreeSet::new();
        let mut pulled_summaries = Vec::new();
        let mut last_pulled_capsule_id: Option<String> = None;
        let mut prev_cap_id = pulled_base_cap_id.clone();
        let now = now_ns();
        let mut lsn_cursor = base_lsn;

        if let Some(tip) = active_branch_tip {
            let unmapped = collect_unmapped_commits_oldest_first(
                &local_repo,
                tip,
                old_local_tip,
                &mapped_oids,
            )?;

            for commit_oid in unmapped {
                let oid_hex = commit_oid.to_hex().to_string();
                pulled_commit_oids.push(oid_hex.clone());
                let commit = local_repo
                    .find_commit(commit_oid)
                    .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
                let new_tree = commit
                    .tree()
                    .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;

                let note_opt = read_note_json(&local_repo, &notes_ref, commit_oid);
                if let Some(note) = note_opt {
                    if let Some(mut cap) = note.capsule {
                        cap.git_commit_oid = Some(oid_hex.clone());
                        let need_snap = cap.snapshot_cas_hash.is_none_or(|h| !cas.contains(&h));
                        if need_snap {
                            let snap_hash = capture_tree_snapshot_to_cas(
                                &local_repo,
                                new_tree.id,
                                &ignore_dirs,
                                &cas,
                            )?;
                            cap.snapshot_cas_hash = Some(snap_hash);
                        }
                        for sym in &cap.touched_symbols {
                            all_touched_symbols.insert(sym.clone());
                        }
                        let cap_id = cap.capsule_id.clone();
                        let cap_summary_line = cap
                            .summary
                            .lines()
                            .map(str::trim)
                            .find(|s| !s.is_empty())
                            .map(ToOwned::to_owned)
                            .unwrap_or_else(|| {
                                let msg = String::from_utf8_lossy(commit.message_raw_sloppy());
                                msg.lines()
                                    .map(str::trim)
                                    .find(|s| !s.is_empty())
                                    .unwrap_or(&cap_id)
                                    .to_string()
                            });
                        pulled_summaries.push(cap_summary_line);
                        if !existing_capsules.contains(&cap_id) {
                            effects.push(StateEffect::UpsertCapsule(cap));
                            restored_capsule_ids.push(cap_id.clone());
                        }
                        last_pulled_capsule_id = Some(cap_id.clone());
                        prev_cap_id = Some(cap_id.clone());
                        let entry = GitMapEntry {
                            git_commit_oid: oid_hex.clone(),
                            capsule_id: Some(cap_id.clone()),
                            branch: branch_name.clone(),
                            direction: GitSyncDirection::Pull,
                            notes_synced: true,
                            timestamp_ns: now,
                        };
                        effects.push(StateEffect::UpsertGitMap {
                            key: format!("oid:{oid_hex}"),
                            entry: entry.clone(),
                        });
                        effects.push(StateEffect::UpsertGitMap {
                            key: format!("cap:{cap_id}"),
                            entry,
                        });
                    } else {
                        // Note exists without capsule -> treat commit itself as external
                        let msg = String::from_utf8_lossy(commit.message_raw_sloppy()).into_owned();
                        let author_name = commit
                            .author()
                            .ok()
                            .map(|a| String::from_utf8_lossy(a.name.as_ref()).into_owned())
                            .unwrap_or_else(|| "git".to_string());
                        let summary_line = msg
                            .lines()
                            .map(str::trim)
                            .find(|s| !s.is_empty())
                            .unwrap_or("External git commit")
                            .to_string();
                        let cap_id = parse_trailer_value(&msg, "Loom-Capsule-Id")
                            .unwrap_or_else(|| format!("cap-ext-{}", &oid_hex[..16]));
                        let snap_hash = capture_tree_snapshot_to_cas(
                            &local_repo,
                            new_tree.id,
                            &ignore_dirs,
                            &cas,
                        )?;
                        pulled_summaries.push(summary_line.clone());
                        let cap = CausalCapsule {
                            capsule_id: cap_id.clone(),
                            viewport_id: "git-pull".to_string(),
                            agent_id: author_name,
                            summary: summary_line,
                            patch_ids: Vec::new(),
                            touched_files: Vec::new(),
                            touched_symbols: Vec::new(),
                            verification_proof: None,
                            parent_capsule_ids: prev_cap_id.clone().into_iter().collect(),
                            snapshot_cas_hash: Some(snap_hash),
                            proof_cas_hash: None,
                            git_commit_oid: Some(oid_hex.clone()),
                            created_at_ns: now,
                        };
                        effects.push(StateEffect::UpsertCapsule(cap));
                        synthesized_capsule_ids.push(cap_id.clone());
                        last_pulled_capsule_id = Some(cap_id.clone());
                        prev_cap_id = Some(cap_id.clone());
                        let entry = GitMapEntry {
                            git_commit_oid: oid_hex.clone(),
                            capsule_id: Some(cap_id.clone()),
                            branch: branch_name.clone(),
                            direction: GitSyncDirection::Pull,
                            notes_synced: true,
                            timestamp_ns: now,
                        };
                        effects.push(StateEffect::UpsertGitMap {
                            key: format!("oid:{oid_hex}"),
                            entry: entry.clone(),
                        });
                        effects.push(StateEffect::UpsertGitMap {
                            key: format!("cap:{cap_id}"),
                            entry,
                        });
                    }

                    for mut edge in note.negative_edges {
                        if !existing_edge_ids.contains(&edge.edge_id)
                            && !mapped_note_edges.contains(&edge.edge_id)
                        {
                            existing_edge_ids.insert(edge.edge_id.clone());
                            mapped_note_edges.insert(edge.edge_id.clone());
                            edge.edge_seq = 0;
                            if edge.touched_symbols.is_empty() {
                                edge.touched_symbols.push("__repo__".to_string());
                            }
                            for sym in &edge.touched_symbols {
                                all_touched_symbols.insert(sym.clone());
                            }
                            effects.push(StateEffect::UpsertGitMap {
                                key: format!("note_edge:{}", edge.edge_id),
                                entry: GitMapEntry {
                                    git_commit_oid: oid_hex.clone(),
                                    capsule_id: Some(edge.edge_id.clone()),
                                    branch: branch_name.clone(),
                                    direction: GitSyncDirection::Pull,
                                    notes_synced: true,
                                    timestamp_ns: now,
                                },
                            });
                            effects.push(StateEffect::InsertNegativeEdge(edge));
                            restored_negative_edges += 1;
                        }
                    }
                } else {
                    // External non-Loom commit: diff_tree_to_tree -> reverse-synthesize AstPatch + CausalCapsule
                    let parent_tree = commit
                        .parent_ids()
                        .next()
                        .and_then(|pid| local_repo.find_commit(pid).ok())
                        .and_then(|pc| pc.tree().ok());
                    let diff_opts = gix::diff::Options::default();
                    let changes = local_repo
                        .diff_tree_to_tree(parent_tree.as_ref(), Some(&new_tree), Some(diff_opts))
                        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;

                    let msg = String::from_utf8_lossy(commit.message_raw_sloppy()).into_owned();
                    let author_name = commit
                        .author()
                        .ok()
                        .map(|a| String::from_utf8_lossy(a.name.as_ref()).into_owned())
                        .unwrap_or_else(|| "git".to_string());
                    let summary_line = msg
                        .lines()
                        .map(str::trim)
                        .find(|s| !s.is_empty())
                        .unwrap_or("External git commit")
                        .to_string();
                    let cap_id = parse_trailer_value(&msg, "Loom-Capsule-Id")
                        .unwrap_or_else(|| format!("cap-ext-{}", &oid_hex[..16]));
                    pulled_summaries.push(summary_line.clone());

                    let mut patch_ids = Vec::new();
                    let mut touched_files = Vec::new();
                    let mut cap_syms = BTreeSet::new();
                    let mut last_patch_hash: Option<Blake3Hash> = None;

                    for change in changes {
                        let (rel_bstr, old_id_opt, new_id_opt, entry_mode) = match change {
                            gix::object::tree::diff::ChangeDetached::Addition {
                                location,
                                entry_mode,
                                id,
                                ..
                            } => (location, None, Some(id), entry_mode),
                            gix::object::tree::diff::ChangeDetached::Deletion {
                                location,
                                entry_mode,
                                id,
                                ..
                            } => (location, Some(id), None, entry_mode),
                            gix::object::tree::diff::ChangeDetached::Modification {
                                location,
                                previous_id,
                                entry_mode,
                                id,
                                ..
                            } => (location, Some(previous_id), Some(id), entry_mode),
                            gix::object::tree::diff::ChangeDetached::Rewrite {
                                location,
                                source_id,
                                entry_mode,
                                id,
                                ..
                            } => (location, Some(source_id), Some(id), entry_mode),
                        };
                        if !matches!(
                            entry_mode.kind(),
                            EntryKind::Blob | EntryKind::BlobExecutable
                        ) {
                            continue;
                        }
                        let rel = snapshot_key_to_rel_path(&rel_bstr.to_string());
                        if rel.as_os_str().is_empty()
                            || is_ignored_relative_path(&rel, &ignore_dirs)
                        {
                            continue;
                        }
                        let old_bytes = old_id_opt
                            .and_then(|id| local_repo.find_blob(id).ok())
                            .map(|b| b.data.clone())
                            .unwrap_or_default();
                        let new_bytes = new_id_opt
                            .and_then(|id| local_repo.find_blob(id).ok())
                            .map(|b| b.data.clone())
                            .unwrap_or_default();
                        let fid = compute_logical_file_id(&rel);
                        let lang = LanguageKind::detect(
                            &rel,
                            if new_bytes.is_empty() {
                                &old_bytes
                            } else {
                                &new_bytes
                            },
                        );
                        let dctx = DiffContext {
                            logical_file_id: fid,
                            file_path: &rel,
                            language: lang,
                            viewport_id: "git-pull",
                            actor_id: &author_name,
                            lamport_ts: lsn_cursor,
                            parents: last_patch_hash.into_iter().collect(),
                        };

                        let mut patch = match diff_file_to_patch(
                            &dctx,
                            &old_bytes,
                            &new_bytes,
                            None,
                            Some(&cas),
                        ) {
                            Ok(p) => p,
                            Err(_) => {
                                let cas_before = cas.put_bytes(&old_bytes).ok();
                                let cas_after = if new_id_opt.is_some() {
                                    cas.put_bytes(&new_bytes).ok()
                                } else {
                                    None
                                };
                                let mut fallback = AstPatch {
                                    patch_hash: Blake3Hash::ZERO,
                                    viewport_id: "git-pull".to_string(),
                                    actor_id: author_name.clone(),
                                    lamport_ts: lsn_cursor,
                                    parents: last_patch_hash.into_iter().collect(),
                                    logical_file_id: fid,
                                    file_path: rel.clone(),
                                    ops: vec![SemanticOp::TextCrdtFallback {
                                        logical_file_id: fid,
                                        file_path: rel.clone(),
                                        automerge_changes: Vec::new(),
                                    }],
                                    contract_deltas: Vec::new(),
                                    cas_before,
                                    cas_after,
                                    binary_manifest: None,
                                    created_at_ns: now,
                                };
                                fallback.finalize_hash();
                                fallback
                            }
                        };
                        if new_id_opt.is_none() {
                            patch.cas_after = None;
                            patch.finalize_hash();
                        }
                        last_patch_hash = Some(patch.patch_hash);
                        for s in collect_patch_symbols_vec(&patch) {
                            cap_syms.insert(s.clone());
                            all_touched_symbols.insert(s);
                        }
                        touched_files.push(rel);
                        let pid = format!("patch-{}", patch.patch_hash.short_hex(16));
                        patch_ids.push(pid.clone());
                        effects.push(StateEffect::UpsertPatch(AstPatchRecord {
                            patch_id: pid,
                            lsn: lsn_cursor,
                            patch,
                        }));
                        lsn_cursor += 1;
                    }

                    let snap_hash = capture_tree_snapshot_to_cas(
                        &local_repo,
                        new_tree.id,
                        &ignore_dirs,
                        &cas,
                    )?;
                    let cap = CausalCapsule {
                        capsule_id: cap_id.clone(),
                        viewport_id: "git-pull".to_string(),
                        agent_id: author_name,
                        summary: summary_line,
                        patch_ids,
                        touched_files,
                        touched_symbols: cap_syms.into_iter().collect(),
                        verification_proof: None,
                        parent_capsule_ids: prev_cap_id.clone().into_iter().collect(),
                        snapshot_cas_hash: Some(snap_hash),
                        proof_cas_hash: None,
                        git_commit_oid: Some(oid_hex.clone()),
                        created_at_ns: now,
                    };
                    if !existing_capsules.contains(&cap_id) {
                        effects.push(StateEffect::UpsertCapsule(cap));
                        synthesized_capsule_ids.push(cap_id.clone());
                    }
                    last_pulled_capsule_id = Some(cap_id.clone());
                    prev_cap_id = Some(cap_id.clone());
                    let entry = GitMapEntry {
                        git_commit_oid: oid_hex.clone(),
                        capsule_id: Some(cap_id.clone()),
                        branch: branch_name.clone(),
                        direction: GitSyncDirection::Pull,
                        notes_synced: false,
                        timestamp_ns: now,
                    };
                    effects.push(StateEffect::UpsertGitMap {
                        key: format!("oid:{oid_hex}"),
                        entry: entry.clone(),
                    });
                    effects.push(StateEffect::UpsertGitMap {
                        key: format!("cap:{cap_id}"),
                        entry,
                    });
                }
            }
        }

        // Also scan all notes in refs/notes/loom for NegativeEdges attached to already-pulled commits
        if let Some(lnt) = try_peel_ref_id(&local_repo, &notes_ref)?
            && let Ok(note_map) = read_notes_map(&local_repo, lnt)
        {
            for (target_oid, raw) in note_map {
                let Ok(note) = serde_json::from_slice::<LoomGitNote>(&raw) else {
                    continue;
                };
                let target_hex = target_oid.to_hex().to_string();
                for mut edge in note.negative_edges {
                    if !existing_edge_ids.contains(&edge.edge_id)
                        && !mapped_note_edges.contains(&edge.edge_id)
                    {
                        existing_edge_ids.insert(edge.edge_id.clone());
                        mapped_note_edges.insert(edge.edge_id.clone());
                        edge.edge_seq = 0;
                        if edge.touched_symbols.is_empty() {
                            edge.touched_symbols.push("__repo__".to_string());
                        }
                        for sym in &edge.touched_symbols {
                            all_touched_symbols.insert(sym.clone());
                        }
                        effects.push(StateEffect::UpsertGitMap {
                            key: format!("note_edge:{}", edge.edge_id),
                            entry: GitMapEntry {
                                git_commit_oid: target_hex.clone(),
                                capsule_id: Some(edge.edge_id.clone()),
                                branch: branch_name.clone(),
                                direction: GitSyncDirection::Pull,
                                notes_synced: true,
                                timestamp_ns: now,
                            },
                        });
                        effects.push(StateEffect::InsertNegativeEdge(edge));
                        restored_negative_edges += 1;
                    }
                }
            }
        }

        if !rebased_unpushed_capsules.is_empty() {
            let pulled_tip_cap = prev_cap_id.clone();
            let rebased_ids: HashSet<String> = rebased_unpushed_capsules
                .iter()
                .map(|c| c.capsule_id.clone())
                .collect();
            for (idx, mut cap) in rebased_unpushed_capsules.into_iter().enumerate() {
                if idx == 0 {
                    if let Some(ref p_id) = pulled_tip_cap {
                        if pulled_tip_cap != pulled_base_cap_id
                            && let Some(ref base_id) = pulled_base_cap_id
                        {
                            cap.parent_capsule_ids.retain(|id| id != base_id);
                        }
                        if !rebased_ids.contains(p_id) && !cap.parent_capsule_ids.contains(p_id) {
                            cap.parent_capsule_ids.push(p_id.clone());
                        }
                    }
                } else {
                    let self_id = cap.capsule_id.clone();
                    cap.parent_capsule_ids.retain(|id| id != &self_id);
                }
                prev_cap_id = Some(cap.capsule_id.clone());
                effects.push(StateEffect::UpsertCapsule(cap));
            }
        }

        let new_trunk_snapshot_cas = if updated_trunk && let Some(new_tip) = active_branch_tip {
            let old_tree_id = old_local_tip
                .and_then(|oid| local_repo.find_commit(oid).ok())
                .and_then(|c| c.tree_id().ok())
                .map(|id| id.detach());
            let new_tree_id = local_repo
                .find_commit(new_tip)
                .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?
                .tree_id()
                .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?
                .detach();
            let snap = if let Some((merged_files, merged_snap_cas)) = merged_trunk_override {
                sync_trunk_worktree_from_map(
                    &local_repo,
                    &repo_root,
                    old_tree_id,
                    new_tree_id,
                    &merged_files,
                    &ignore_dirs,
                )?;
                merged_snap_cas
            } else {
                sync_trunk_worktree_from_tree(
                    &local_repo,
                    &repo_root,
                    old_tree_id,
                    new_tree_id,
                    &ignore_dirs,
                )?;
                capture_workspace_snapshot(&repo_root, &ignore_dirs, &cas)?
            };

            let full_branch = gix::refs::FullName::try_from(branch_ref.as_str())
                .map_err(|e| RpcError::new(RpcErrorCode::InvalidRequest, e.to_string()))?;
            let committer_ref = local_repo
                .committer()
                .transpose()
                .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
            let expected = match old_local_tip {
                Some(ltip) => PreviousValue::MustExistAndMatch(gix::refs::Target::Object(ltip)),
                None => PreviousValue::MustNotExist,
            };
            local_repo
                .edit_references_as(
                    [RefEdit::update(
                        full_branch,
                        new_tip,
                        expected,
                        "loom git pull",
                    )],
                    committer_ref,
                )
                .map_err(|e| RpcError::new(RpcErrorCode::Conflict, e.to_string()))?;

            snap
        } else {
            old_trunk_snapshot_cas
        };

        Ok(PullPrepOutcome {
            result: GitPullResult {
                remote: remote_label,
                remote_path: bare_path,
                branch: branch_name,
                head_commit_oid: active_branch_tip.map(|id| id.to_hex().to_string()),
                pulled_commit_oids,
                restored_capsule_ids,
                synthesized_capsule_ids,
                restored_negative_edges,
                updated_trunk,
                rebased_viewports: Vec::new(),
            },
            effects,
            touched_symbols: all_touched_symbols.into_iter().collect(),
            pulled_summaries,
            last_pulled_capsule_id,
            new_head_capsule_id: prev_cap_id,
            old_trunk_snapshot_cas,
            new_trunk_snapshot_cas,
            branch_ref: branch_ref.clone(),
            notes_ref: notes_ref.clone(),
            orig_local_branch_tip: old_local_tip,
            orig_local_notes_tip,
        })
        })();

        if let Err(err) = pull_res {
            rollback_git_refs(
                &local_repo,
                &branch_ref,
                old_local_tip,
                &notes_ref,
                orig_local_notes_tip,
            );
            return Err(err);
        }

        pull_res
    })
    .await
    .map_err(|e| RpcError::new(RpcErrorCode::Internal, e.to_string()))??;

    let mut result = prep.result;

    if !prep.effects.is_empty() {
        let base_summary = format!(
            "Git pull from {} ({}): {} commit(s), {} restored, {} external, {} negative edge(s)",
            result.remote,
            result.branch,
            result.pulled_commit_oids.len(),
            result.restored_capsule_ids.len(),
            result.synthesized_capsule_ids.len(),
            result.restored_negative_edges
        );
        let summary = if prep.pulled_summaries.is_empty() {
            base_summary
        } else {
            const MAX_SHOWN: usize = 32;
            let shown: Vec<&str> = prep
                .pulled_summaries
                .iter()
                .take(MAX_SHOWN)
                .map(String::as_str)
                .collect();
            if prep.pulled_summaries.len() > MAX_SHOWN {
                format!(
                    "{base_summary} [{}; ... (+{} more)]",
                    shown.join("; "),
                    prep.pulled_summaries.len() - MAX_SHOWN
                )
            } else {
                format!("{base_summary} [{}]", shown.join("; "))
            }
        };
        if let Err(err) = ctx
            .writer
            .mutate(MutationRequest {
                kind: OpKind::GitPull,
                viewport_id: None,
                target_id: result.head_commit_oid.clone(),
                capsule_id: prep.last_pulled_capsule_id.clone(),
                touched_symbols: prep.touched_symbols,
                summary,
                effect: StateEffect::Batch(prep.effects),
            })
            .await
        {
            let rr = ctx.repo_root.clone();
            let br = prep.branch_ref;
            let nr = prep.notes_ref;
            let obt = prep.orig_local_branch_tip;
            let ont = prep.orig_local_notes_tip;
            let _ = tokio::task::spawn_blocking(move || {
                if let Ok(local_repo) = open_gix_repo_with_fallback(&rr, None) {
                    rollback_git_refs(&local_repo, &br, obt, &nr, ont);
                }
            })
            .await;
            return Err(err);
        }
    }

    if result.updated_trunk {
        let active_vps: Vec<ViewportRecord> = ctx
            .db
            .list_viewports(false)
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
            .into_iter()
            .filter(|v| v.state.is_active())
            .collect();

        for vp in active_vps {
            if vp.state == ViewportState::Verifying || !vp.path.is_dir() {
                result.rebased_viewports.push(ViewportRebaseStatus {
                    viewport_id: vp.id,
                    viewport_name: vp.name,
                    outcome: ViewportRebaseOutcome::Skipped,
                    updated_files: 0,
                    conflicts: Vec::new(),
                });
                continue;
            }

            let vp_clone = vp.clone();
            let old_snap = prep.old_trunk_snapshot_cas;
            let new_snap = prep.new_trunk_snapshot_cas;
            let branch_for_rebase = result.branch.clone();
            let ignore_dirs = ctx.config.radar.ignore_dirs.clone();
            let cas_dir = ctx.loom_dir.join("cas");
            let zstd_level = ctx.config.zstd_level;

            let plan = tokio::task::spawn_blocking(move || {
                let cas = CasStore::new(cas_dir, zstd_level)
                    .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
                compute_viewport_rebase_plan(
                    &vp_clone,
                    old_snap,
                    new_snap,
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
                                peer_viewport_name: format!("git:{}", result.branch),
                                level: RadarLevel::Red,
                                kind: "rebase_conflict".to_string(),
                                file_path: None,
                                symbol_fqn: None,
                                message: err.message,
                            }];
                            ctx.radar
                                .set_external_conflicts(&vp.id, conflicts.clone())
                                .await;
                            result.rebased_viewports.push(ViewportRebaseStatus {
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
                            base_capsule_id: prep.new_head_capsule_id.clone(),
                            base_snapshot_cas: Some(prep.new_trunk_snapshot_cas),
                            clear_patches: false,
                            op_kind: OpKind::GitPull,
                            summary: Some(format!(
                                "Rebase viewport `{}` base snapshot onto git pull",
                                vp.name
                            )),
                            reply: tx,
                        })
                        .await
                        .map_err(|_| {
                            RpcError::new(RpcErrorCode::Internal, "writer actor closed")
                        })?;
                    let _ = rx.await.map_err(|_| {
                        RpcError::new(RpcErrorCode::Internal, "writer actor dropped reply")
                    })??;

                    ctx.radar.reset_viewport_clean(&vp.id).await;

                    result.rebased_viewports.push(ViewportRebaseStatus {
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
                    result.rebased_viewports.push(ViewportRebaseStatus {
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
    Ok(result)
}
