pub mod capsule;
pub mod git_sync;
pub mod ipc;
pub mod mcp;
pub mod radar;
pub mod recovery;
pub mod remote_sync;
pub mod viewport_mgr;

use interprocess::local_socket::traits::tokio::Listener as _;
use loom_core::cas::CasStore;
use loom_core::config::LoomConfig;
use loom_core::db::{
    StateDb, TABLE_CAPSULES, TABLE_GIT_MAP, TABLE_OP_LOG, TABLE_SYMBOL_CONSUMES,
    TABLE_SYMBOL_PROVIDES, decode_value, encode_value,
};
use loom_core::model::{
    AstPatch, AstPatchRecord, Blake3Hash, GitMapEntry, GitSyncDirection, NegativeEdge, OpKind,
    OpLogEntry, RepoMeta, SymbolConsumeEntry, SymbolProvideEntry, ViewportMount, ViewportRecord,
    ViewportState, WorkspaceSnapshot, now_ns,
};
use loom_core::rpc::{
    GcResult, InitResult, RpcError, RpcErrorCode, SocketDescriptor, UndoResult,
    canonical_repo_root, lock_file_path, sock_descriptor_path,
};
use loom_core::wal::{InverseEffect, StateEffect, WalLog, WalPayload, WalRecordKind};
use redb::ReadableTable;
use std::collections::HashSet;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, mpsc, oneshot, watch};

// ============================================================================
// RepoLock (.loom/daemon.lock via std::fs::File::try_lock)
// ============================================================================

pub struct RepoLock {
    _file: File,
    path: PathBuf,
}

pub enum LockAcquireOutcome {
    Acquired(RepoLock),
    AlreadyRunning,
}

impl RepoLock {
    pub fn try_acquire(repo_root: &Path) -> anyhow::Result<LockAcquireOutcome> {
        let lock_path = lock_file_path(repo_root);
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;

        match file.try_lock() {
            Ok(()) => {
                file.set_len(0)?;
                file.seek(SeekFrom::Start(0))?;
                writeln!(file, "{}", std::process::id())?;
                file.flush()?;
                Ok(LockAcquireOutcome::Acquired(Self {
                    _file: file,
                    path: lock_path,
                }))
            }
            Err(TryLockError::WouldBlock) => Ok(LockAcquireOutcome::AlreadyRunning),
            Err(TryLockError::Error(e)) => Err(e.into()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

// ============================================================================
// Pure Rust .git Inspection & .git/info/exclude (NO git.exe shelling out)
// ============================================================================

#[derive(Debug, Clone)]
pub struct GitInspectInfo {
    pub git_dir: PathBuf,
    pub head_oid: Option<String>,
    pub branch: Option<String>,
}

pub fn resolve_git_dir(repo_root: &Path) -> anyhow::Result<PathBuf> {
    let dot_git = repo_root.join(".git");
    if dot_git.is_dir() {
        return Ok(dot_git);
    }
    if dot_git.is_file() {
        let content = std::fs::read_to_string(&dot_git)?;
        if let Some(rest) = content.trim().strip_prefix("gitdir:") {
            let candidate = PathBuf::from(rest.trim());
            let resolved = if candidate.is_absolute() {
                candidate
            } else {
                repo_root.join(candidate)
            };
            if resolved.is_dir() {
                return Ok(resolved);
            }
        }
    }
    anyhow::bail!("no .git repository found at {}", repo_root.display());
}

pub fn inspect_git_repo(repo_root: &Path) -> anyhow::Result<GitInspectInfo> {
    let git_dir = resolve_git_dir(repo_root)?;
    let head_path = git_dir.join("HEAD");
    if !head_path.is_file() {
        return Ok(GitInspectInfo {
            git_dir,
            head_oid: None,
            branch: None,
        });
    }

    let head_raw = std::fs::read_to_string(&head_path)?;
    let head_trimmed = head_raw.trim();

    if let Some(ref_name) = head_trimmed.strip_prefix("ref:") {
        let ref_name = ref_name.trim();
        let branch = ref_name
            .strip_prefix("refs/heads/")
            .map(str::to_string)
            .or_else(|| Some(ref_name.to_string()));
        let loose_ref_path = git_dir.join(ref_name);
        if loose_ref_path.is_file() {
            let oid = std::fs::read_to_string(&loose_ref_path)?.trim().to_string();
            if !oid.is_empty() {
                return Ok(GitInspectInfo {
                    git_dir,
                    head_oid: Some(oid),
                    branch,
                });
            }
        }
        let packed_refs_path = git_dir.join("packed-refs");
        if packed_refs_path.is_file() {
            let packed = std::fs::read_to_string(&packed_refs_path)?;
            for line in packed.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
                    continue;
                }
                if let Some((oid, r)) = line.split_once(' ')
                    && r.trim() == ref_name
                {
                    return Ok(GitInspectInfo {
                        git_dir,
                        head_oid: Some(oid.trim().to_string()),
                        branch,
                    });
                }
            }
        }
        Ok(GitInspectInfo {
            git_dir,
            head_oid: None,
            branch,
        })
    } else if (head_trimmed.len() == 40 || head_trimmed.len() == 64)
        && head_trimmed.bytes().all(|b| b.is_ascii_hexdigit())
    {
        Ok(GitInspectInfo {
            git_dir,
            head_oid: Some(head_trimmed.to_string()),
            branch: None,
        })
    } else {
        Ok(GitInspectInfo {
            git_dir,
            head_oid: None,
            branch: None,
        })
    }
}

pub fn ensure_git_info_exclude(git_dir: &Path) -> anyhow::Result<()> {
    let info_dir = git_dir.join("info");
    std::fs::create_dir_all(&info_dir)?;
    let exclude_path = info_dir.join("exclude");
    let existing = if exclude_path.is_file() {
        std::fs::read_to_string(&exclude_path)?
    } else {
        String::new()
    };

    let has_loom = existing
        .lines()
        .any(|line| matches!(line.trim(), ".loom" | ".loom/"));
    if has_loom {
        return Ok(());
    }

    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(".loom/\n");
    std::fs::write(&exclude_path, updated)?;
    Ok(())
}

// ============================================================================
// IdleMonitor
// ============================================================================

pub struct IdleMonitor {
    idle_timeout: Duration,
    active_viewports: Arc<AtomicUsize>,
    last_activity: Arc<Mutex<Instant>>,
    notify: Arc<Notify>,
}

impl IdleMonitor {
    pub fn new(idle_timeout: Duration, initial_active_viewports: usize) -> Self {
        Self {
            idle_timeout,
            active_viewports: Arc::new(AtomicUsize::new(initial_active_viewports)),
            last_activity: Arc::new(Mutex::new(Instant::now())),
            notify: Arc::new(Notify::new()),
        }
    }

    pub fn idle_timeout(&self) -> Duration {
        self.idle_timeout
    }

    pub fn touch_activity(&self) {
        if let Ok(mut guard) = self.last_activity.lock() {
            *guard = Instant::now();
        }
        self.notify.notify_one();
    }

    pub fn set_active_viewports(&self, count: usize) {
        self.active_viewports.store(count, Ordering::SeqCst);
        self.notify.notify_one();
    }

    pub fn active_viewports(&self) -> usize {
        self.active_viewports.load(Ordering::SeqCst)
    }

    pub fn idle_remaining_ms(&self) -> Option<u64> {
        if self.active_viewports.load(Ordering::SeqCst) > 0 {
            return None;
        }
        let elapsed = self
            .last_activity
            .lock()
            .map(|g| g.elapsed())
            .unwrap_or(Duration::ZERO);
        Some(self.idle_timeout.saturating_sub(elapsed).as_millis() as u64)
    }

    pub async fn wait_for_idle_shutdown(&self) {
        loop {
            if self.active_viewports.load(Ordering::SeqCst) > 0 {
                self.notify.notified().await;
                continue;
            }
            let last = self
                .last_activity
                .lock()
                .map(|g| *g)
                .unwrap_or_else(|_| Instant::now());
            let deadline = last + self.idle_timeout;
            tokio::select! {
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                    let current_last = self
                        .last_activity
                        .lock()
                        .map(|g| *g)
                        .unwrap_or_else(|_| Instant::now());
                    if self.active_viewports.load(Ordering::SeqCst) == 0
                        && Instant::now() >= current_last + self.idle_timeout
                    {
                        return;
                    }
                }
                _ = self.notify.notified() => {}
            }
        }
    }
}

// ============================================================================
// WriterActor (Single-Writer WAL -> redb Serializer)
// ============================================================================

#[derive(Debug, Clone)]
pub struct MutationRequest {
    pub kind: OpKind,
    pub viewport_id: Option<String>,
    pub target_id: Option<String>,
    pub capsule_id: Option<String>,
    pub touched_symbols: Vec<String>,
    pub summary: String,
    pub effect: StateEffect,
}

pub enum WriterCommand {
    InitRepo {
        colocate_git: bool,
        seeded_head_oid: Option<String>,
        branch: Option<String>,
        reply: oneshot::Sender<Result<InitResult, RpcError>>,
    },
    BeginSpawnViewport {
        vp: Box<ViewportRecord>,
        reply: oneshot::Sender<Result<(u64, u64, ViewportRecord), RpcError>>,
    },
    TransitionViewport {
        viewport_id: String,
        target_state: ViewportState,
        pid: Option<u32>,
        mount_override: Option<Option<ViewportMount>>,
        summary: Option<String>,
        reply: oneshot::Sender<Result<ViewportRecord, RpcError>>,
    },
    MarkViewportPruned {
        viewport_id: String,
        reply: oneshot::Sender<Result<ViewportRecord, RpcError>>,
    },
    ClaimViewportForSeal {
        viewport_id: String,
        reply: oneshot::Sender<Result<ViewportRecord, RpcError>>,
    },
    UpdateViewportBase {
        viewport_id: String,
        base_capsule_id: Option<String>,
        base_snapshot_cas: Option<Blake3Hash>,
        clear_patches: bool,
        op_kind: OpKind,
        summary: Option<String>,
        reply: oneshot::Sender<Result<ViewportRecord, RpcError>>,
    },
    Mutate {
        req: Box<MutationRequest>,
        reply: oneshot::Sender<Result<(u64, u64), RpcError>>,
    },
    RecordDeadend {
        edge: Box<NegativeEdge>,
        restore_base: Option<(Option<String>, Blake3Hash)>,
        reply: oneshot::Sender<Result<NegativeEdge, RpcError>>,
    },
    RecordAstPatch {
        viewport_id: String,
        patch: Box<AstPatch>,
        reply: oneshot::Sender<Result<Option<AstPatchRecord>, RpcError>>,
    },
    DropViewport {
        viewport_id: String,
        edge: Box<NegativeEdge>,
        extra_patches: Vec<AstPatch>,
        post_clean_snapshot: Option<Blake3Hash>,
        reply: oneshot::Sender<Result<(ViewportRecord, NegativeEdge), RpcError>>,
    },
    AppendUndo {
        op_seq: Option<u64>,
        reply: oneshot::Sender<Result<UndoResult, RpcError>>,
    },
    RunGc {
        prune_viewports: bool,
        cas_dir: PathBuf,
        viewports_dir: PathBuf,
        cache_dirs: Vec<String>,
        zstd_level: i32,
        reply: oneshot::Sender<Result<GcResult, RpcError>>,
    },
    SyncSymbolTables {
        remove_provides: Vec<(String, String)>,
        upsert_provides: Vec<SymbolProvideEntry>,
        remove_consumes: Vec<(String, String)>,
        upsert_consumes: Vec<SymbolConsumeEntry>,
        reply: oneshot::Sender<Result<(), RpcError>>,
    },
    ClearViewportSymbols {
        viewport_id: String,
        reply: oneshot::Sender<Result<(), RpcError>>,
    },
    CheckpointAndStop {
        reply: oneshot::Sender<Result<(), RpcError>>,
    },
}

#[derive(Clone)]
pub struct WriterHandle {
    tx: mpsc::Sender<WriterCommand>,
}

impl WriterHandle {
    pub async fn send(&self, cmd: WriterCommand) -> Result<(), mpsc::error::SendError<WriterCommand>> {
        self.tx.send(cmd).await
    }

    pub async fn mutate(&self, req: MutationRequest) -> Result<(u64, u64), RpcError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(WriterCommand::Mutate {
                req: Box::new(req),
                reply: tx,
            })
            .await
            .map_err(|_| RpcError::new(RpcErrorCode::Internal, "writer actor closed"))?;
        rx.await
            .map_err(|_| RpcError::new(RpcErrorCode::Internal, "writer actor dropped reply"))?
    }

    pub async fn undo(&self, op_seq: Option<u64>) -> Result<UndoResult, RpcError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(WriterCommand::AppendUndo { op_seq, reply: tx })
            .await
            .map_err(|_| RpcError::new(RpcErrorCode::Internal, "writer actor closed"))?;
        rx.await
            .map_err(|_| RpcError::new(RpcErrorCode::Internal, "writer actor dropped reply"))?
    }

    pub async fn checkpoint_and_stop(&self) -> Result<(), RpcError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(WriterCommand::CheckpointAndStop { reply: tx })
            .await
            .map_err(|_| RpcError::new(RpcErrorCode::Internal, "writer actor closed"))?;
        rx.await
            .map_err(|_| RpcError::new(RpcErrorCode::Internal, "writer actor dropped reply"))?
    }
}

pub struct WriterActorState {
    db: Arc<StateDb>,
    wal: WalLog,
    checkpoint_ops: u64,
    ops_since_checkpoint: u64,
    idle_monitor: Arc<IdleMonitor>,
}

impl WriterActorState {
    pub fn spawn(
        db: Arc<StateDb>,
        wal: WalLog,
        checkpoint_ops: u64,
        idle_monitor: Arc<IdleMonitor>,
    ) -> (WriterHandle, std::thread::JoinHandle<()>) {
        let (tx, mut rx) = mpsc::channel::<WriterCommand>(256);
        let mut state = Self {
            db,
            wal,
            checkpoint_ops: checkpoint_ops.max(1),
            ops_since_checkpoint: 0,
            idle_monitor,
        };
        let join = std::thread::spawn(move || {
            while let Some(cmd) = rx.blocking_recv() {
                match cmd {
                    WriterCommand::InitRepo {
                        colocate_git,
                        seeded_head_oid,
                        branch,
                        reply,
                    } => {
                        let _ = reply.send(state.handle_init_repo(
                            colocate_git,
                            seeded_head_oid,
                            branch,
                        ));
                    }
                    WriterCommand::BeginSpawnViewport { vp, reply } => {
                        let _ = reply.send(state.handle_begin_spawn_viewport(*vp));
                    }
                    WriterCommand::TransitionViewport {
                        viewport_id,
                        target_state,
                        pid,
                        mount_override,
                        summary,
                        reply,
                    } => {
                        let _ = reply.send(state.handle_transition_viewport(
                            &viewport_id,
                            target_state,
                            pid,
                            mount_override,
                            summary,
                        ));
                    }
                    WriterCommand::MarkViewportPruned { viewport_id, reply } => {
                        let _ = reply.send(state.handle_mark_viewport_pruned(&viewport_id));
                    }
                    WriterCommand::ClaimViewportForSeal { viewport_id, reply } => {
                        let _ = reply.send(state.handle_claim_viewport_for_seal(&viewport_id));
                    }
                    WriterCommand::UpdateViewportBase {
                        viewport_id,
                        base_capsule_id,
                        base_snapshot_cas,
                        clear_patches,
                        op_kind,
                        summary,
                        reply,
                    } => {
                        let _ = reply.send(state.handle_update_viewport_base(
                            &viewport_id,
                            base_capsule_id,
                            base_snapshot_cas,
                            clear_patches,
                            op_kind,
                            summary,
                        ));
                    }
                    WriterCommand::Mutate { req, reply } => {
                        let _ = reply.send(state.handle_mutate(*req));
                    }
                    WriterCommand::RecordDeadend {
                        edge,
                        restore_base,
                        reply,
                    } => {
                        let _ = reply.send(state.handle_record_deadend(*edge, restore_base));
                    }
                    WriterCommand::RecordAstPatch {
                        viewport_id,
                        patch,
                        reply,
                    } => {
                        let _ = reply.send(state.handle_record_ast_patch(&viewport_id, *patch));
                    }
                    WriterCommand::DropViewport {
                        viewport_id,
                        edge,
                        extra_patches,
                        post_clean_snapshot,
                        reply,
                    } => {
                        let _ = reply.send(state.handle_drop_viewport(
                            &viewport_id,
                            *edge,
                            extra_patches,
                            post_clean_snapshot,
                        ));
                    }
                    WriterCommand::AppendUndo { op_seq, reply } => {
                        let _ = reply.send(state.handle_undo(op_seq));
                    }
                    WriterCommand::RunGc {
                        prune_viewports,
                        cas_dir,
                        viewports_dir,
                        cache_dirs,
                        zstd_level,
                        reply,
                    } => {
                        let _ = reply.send(state.handle_gc(
                            prune_viewports,
                            &cas_dir,
                            &viewports_dir,
                            &cache_dirs,
                            zstd_level,
                        ));
                    }
                    WriterCommand::SyncSymbolTables {
                        remove_provides,
                        upsert_provides,
                        remove_consumes,
                        upsert_consumes,
                        reply,
                    } => {
                        let _ = reply.send(state.handle_sync_symbol_tables(
                            &remove_provides,
                            &upsert_provides,
                            &remove_consumes,
                            &upsert_consumes,
                        ));
                    }
                    WriterCommand::ClearViewportSymbols { viewport_id, reply } => {
                        let _ = reply.send(state.handle_clear_viewport_symbols(&viewport_id));
                    }
                    WriterCommand::CheckpointAndStop { reply } => {
                        let res = state.force_checkpoint();
                        let _ = reply.send(res);
                        break;
                    }
                }
            }
        });
        (WriterHandle { tx }, join)
    }

    fn handle_init_repo(
        &mut self,
        colocate_git: bool,
        seeded_head_oid: Option<String>,
        branch: Option<String>,
    ) -> Result<InitResult, RpcError> {
        let existing = self
            .db
            .get_repo_meta()
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;

        if let Some(mut meta) = existing {
            if colocate_git && !meta.colocate_git {
                let now = now_ns();
                meta.colocate_git = true;
                meta.updated_at_ns = now;
                let wtx = self
                    .db
                    .begin_write()
                    .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
                {
                    let mut op_table = wtx
                        .open_table(TABLE_OP_LOG)
                        .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
                    StateDb::set_repo_meta_in_tx(&mut op_table, &meta)
                        .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
                    if let Some(oid) = seeded_head_oid.as_ref() {
                        let mut git_table = wtx
                            .open_table(TABLE_GIT_MAP)
                            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
                        let entry = GitMapEntry {
                            git_commit_oid: oid.clone(),
                            capsule_id: Some("genesis".to_string()),
                            branch: branch.clone().unwrap_or_else(|| "HEAD".to_string()),
                            direction: GitSyncDirection::InitSeed,
                            notes_synced: false,
                            timestamp_ns: now,
                        };
                        StateDb::upsert_git_map_in_tx(&mut git_table, &format!("oid:{oid}"), &entry)
                            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
                    }
                }
                wtx.commit()
                    .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
            }
            return Ok(InitResult {
                repo_root: PathBuf::new(),
                repo_id: meta.repo_id,
                colocate_git: meta.colocate_git,
                already_initialized: true,
                seeded_head_oid,
                branch,
            });
        }

        let now = now_ns();
        let lsn = self.wal.last_lsn() + 1;
        let op_seq = 1u64;
        let repo_id = uuid::Uuid::new_v4().to_string();
        let meta = RepoMeta {
            schema_version: 1,
            repo_id: repo_id.clone(),
            colocate_git,
            next_lsn: lsn + 1,
            next_op_seq: op_seq + 1,
            next_edge_seq: 1,
            last_applied_lsn: lsn,
            last_checkpoint_lsn: 0,
            lamport_clock: 1,
            head_capsule_id: None,
            created_at_ns: now,
            updated_at_ns: now,
        };
        let init_op = OpLogEntry {
            op_seq,
            lsn,
            kind: OpKind::RepoInit,
            viewport_id: None,
            target_id: Some(repo_id.clone()),
            capsule_id: None,
            touched_symbols: Vec::new(),
            summary: if colocate_git {
                "Initialized Loom repository (colocate-git)".to_string()
            } else {
                "Initialized Loom repository".to_string()
            },
            inverse_payload: None,
            undone_by_op: None,
            timestamp_ns: now,
        };
        let git_seed = if colocate_git {
            seeded_head_oid.as_ref().map(|oid| {
                (
                    format!("oid:{oid}"),
                    GitMapEntry {
                        git_commit_oid: oid.clone(),
                        capsule_id: Some("genesis".to_string()),
                        branch: branch.clone().unwrap_or_else(|| "HEAD".to_string()),
                        direction: GitSyncDirection::InitSeed,
                        notes_synced: false,
                        timestamp_ns: now,
                    },
                )
            })
        } else {
            None
        };

        let wal_payload = WalPayload::InitRepo {
            meta: meta.clone(),
            init_op: Some(init_op.clone()),
            git_seed: git_seed.clone(),
        };
        self.wal
            .append(WalRecordKind::Mutation, lsn, now, &wal_payload)
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;

        let wtx = self
            .db
            .begin_write()
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
        {
            let mut op_table = wtx
                .open_table(TABLE_OP_LOG)
                .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
            StateDb::set_repo_meta_in_tx(&mut op_table, &meta)
                .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
            StateDb::append_op_in_tx(&mut op_table, &init_op)
                .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
            if let Some((key, entry)) = &git_seed {
                let mut git_table = wtx
                    .open_table(TABLE_GIT_MAP)
                    .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
                StateDb::upsert_git_map_in_tx(&mut git_table, key, entry)
                    .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
            }
        }
        wtx.commit()
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;

        self.ops_since_checkpoint += 1;
        let _ = self.maybe_checkpoint();

        Ok(InitResult {
            repo_root: PathBuf::new(),
            repo_id,
            colocate_git,
            already_initialized: false,
            seeded_head_oid,
            branch,
        })
    }

    fn handle_begin_spawn_viewport(
        &mut self,
        mut vp: ViewportRecord,
    ) -> Result<(u64, u64, ViewportRecord), RpcError> {
        let meta = self
            .db
            .get_repo_meta()
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
            .ok_or_else(|| {
                RpcError::new(
                    RpcErrorCode::NotInitialized,
                    "repository metadata not initialized",
                )
            })?;

        if let Some(existing) = self
            .db
            .find_active_viewport_by_name(&vp.name)
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
        {
            return Err(RpcError::new(
                RpcErrorCode::Conflict,
                format!(
                    "active viewport `{}` ({}) already exists in state {}",
                    existing.name, existing.id, existing.state
                ),
            ));
        }

        if vp.path.exists() {
            return Err(RpcError::new(
                RpcErrorCode::Conflict,
                format!(
                    "viewport directory `{}` already exists on disk (run `loom gc --prune-viewports` if obsolete)",
                    vp.path.display()
                ),
            ));
        }

        let next_lsn = meta.next_lsn.max(self.wal.last_lsn() + 1);
        let next_op_seq = meta.next_op_seq.max(1);
        vp.state = ViewportState::Spawned;
        vp.base_op_seq = next_op_seq;
        vp.head_lsn = next_lsn;
        vp.lamport_ts = meta.lamport_clock + 1;

        let (lsn, op_seq) = self.handle_mutate(MutationRequest {
            kind: OpKind::ViewportSpawn,
            viewport_id: Some(vp.id.clone()),
            target_id: Some(vp.id.clone()),
            capsule_id: None,
            touched_symbols: Vec::new(),
            summary: format!("Spawn viewport `{}` ({})", vp.name, vp.driver),
            effect: StateEffect::UpsertViewport(vp.clone()),
        })?;

        Ok((lsn, op_seq, vp))
    }

    fn handle_transition_viewport(
        &mut self,
        viewport_id: &str,
        target_state: ViewportState,
        pid: Option<u32>,
        mount_override: Option<Option<ViewportMount>>,
        summary: Option<String>,
    ) -> Result<ViewportRecord, RpcError> {
        let meta = self
            .db
            .get_repo_meta()
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
            .ok_or_else(|| {
                RpcError::new(
                    RpcErrorCode::NotInitialized,
                    "repository metadata not initialized",
                )
            })?;

        let mut vp = self
            .db
            .resolve_viewport(viewport_id)
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
            .ok_or_else(|| {
                RpcError::new(
                    RpcErrorCode::NotFound,
                    format!("viewport `{viewport_id}` not found"),
                )
            })?;

        let same_active_pid_update =
            vp.state == target_state && vp.state.is_active() && pid.is_some();
        if !same_active_pid_update && !vp.state.can_transition_to(target_state) {
            return Err(RpcError::new(
                RpcErrorCode::InvalidState,
                format!(
                    "invalid viewport state transition for `{}`: {} -> {}",
                    vp.name, vp.state, target_state
                ),
            ));
        }

        let next_lsn = meta.next_lsn.max(self.wal.last_lsn() + 1);
        let prev_state = vp.state;
        vp.state = target_state;
        if let Some(p) = pid {
            vp.pid = Some(p);
        }
        if let Some(m) = mount_override {
            vp.mount = m;
        }
        vp.head_lsn = next_lsn;
        vp.lamport_ts = meta.lamport_clock + 1;
        vp.updated_at_ns = now_ns();

        let summary_text = summary.unwrap_or_else(|| {
            format!(
                "Transition viewport `{}`: {} -> {}",
                vp.name, prev_state, target_state
            )
        });
        let _ = self.handle_mutate(MutationRequest {
            kind: OpKind::ViewportTransition,
            viewport_id: Some(vp.id.clone()),
            target_id: Some(vp.id.clone()),
            capsule_id: None,
            touched_symbols: Vec::new(),
            summary: summary_text,
            effect: StateEffect::UpsertViewport(vp.clone()),
        })?;

        Ok(vp)
    }

    fn handle_mark_viewport_pruned(
        &mut self,
        viewport_id: &str,
    ) -> Result<ViewportRecord, RpcError> {
        let mut vp = self
            .db
            .get_viewport(viewport_id)
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
            .ok_or_else(|| {
                RpcError::new(
                    RpcErrorCode::NotFound,
                    format!("viewport `{viewport_id}` not found"),
                )
            })?;

        if vp.state != ViewportState::GcPending {
            return Err(RpcError::new(
                RpcErrorCode::InvalidState,
                format!(
                    "cannot mark viewport `{}` pruned in state {}",
                    vp.name, vp.state
                ),
            ));
        }

        let ts = now_ns();
        vp.mount = None;
        vp.pruned_at_ns = Some(ts);
        vp.updated_at_ns = ts;

        let _ = self.handle_mutate(MutationRequest {
            kind: OpKind::Gc,
            viewport_id: Some(vp.id.clone()),
            target_id: Some(vp.id.clone()),
            capsule_id: None,
            touched_symbols: Vec::new(),
            summary: format!("Pruned failed-spawn viewport `{}`", vp.name),
            effect: StateEffect::UpsertViewport(vp.clone()),
        })?;

        Ok(vp)
    }

    fn handle_claim_viewport_for_seal(
        &mut self,
        viewport_id: &str,
    ) -> Result<ViewportRecord, RpcError> {
        let _ = self
            .db
            .get_repo_meta()
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
            .ok_or_else(|| {
                RpcError::new(
                    RpcErrorCode::NotInitialized,
                    "repository metadata not initialized",
                )
            })?;

        let vp = self
            .db
            .resolve_viewport(viewport_id)
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
            .ok_or_else(|| {
                RpcError::new(
                    RpcErrorCode::NotFound,
                    format!("viewport `{viewport_id}` not found"),
                )
            })?;

        match vp.state {
            ViewportState::Verifying => Err(RpcError::new(
                RpcErrorCode::Conflict,
                format!("viewport `{}` is already verifying/sealing", vp.name),
            )),
            ViewportState::Sealed | ViewportState::Dropped | ViewportState::GcPending => {
                Err(RpcError::new(
                    RpcErrorCode::InvalidState,
                    format!("cannot seal viewport `{}` in state {}", vp.name, vp.state),
                ))
            }
            ViewportState::Spawned => {
                let _ = self.handle_transition_viewport(
                    &vp.id,
                    ViewportState::Active,
                    None,
                    None,
                    Some(format!("Activate viewport `{}` prior to seal", vp.name)),
                )?;
                self.handle_transition_viewport(
                    &vp.id,
                    ViewportState::Verifying,
                    None,
                    None,
                    Some(format!("Verifying viewport `{}` for capsule seal", vp.name)),
                )
            }
            ViewportState::Active => self.handle_transition_viewport(
                &vp.id,
                ViewportState::Verifying,
                None,
                None,
                Some(format!("Verifying viewport `{}` for capsule seal", vp.name)),
            ),
        }
    }

    fn handle_update_viewport_base(
        &mut self,
        viewport_id: &str,
        base_capsule_id: Option<String>,
        base_snapshot_cas: Option<Blake3Hash>,
        clear_patches: bool,
        op_kind: OpKind,
        summary: Option<String>,
    ) -> Result<ViewportRecord, RpcError> {
        let meta = self
            .db
            .get_repo_meta()
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
            .ok_or_else(|| {
                RpcError::new(
                    RpcErrorCode::NotInitialized,
                    "repository metadata not initialized",
                )
            })?;

        let mut vp = self
            .db
            .resolve_viewport(viewport_id)
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
            .ok_or_else(|| {
                RpcError::new(
                    RpcErrorCode::NotFound,
                    format!("viewport `{viewport_id}` not found"),
                )
            })?;

        vp.base_capsule_id = base_capsule_id;
        vp.base_snapshot_cas = base_snapshot_cas;
        vp.base_op_seq = meta.next_op_seq.max(1);
        if clear_patches {
            vp.patch_ids.clear();
            vp.head_cas = None;
        }
        vp.updated_at_ns = now_ns();

        let summary_text = summary.unwrap_or_else(|| {
            format!("Refresh clean base snapshot for viewport `{}`", vp.name)
        });
        let _ = self.handle_mutate(MutationRequest {
            kind: op_kind,
            viewport_id: Some(vp.id.clone()),
            target_id: Some(vp.id.clone()),
            capsule_id: None,
            touched_symbols: Vec::new(),
            summary: summary_text,
            effect: StateEffect::UpsertViewport(vp.clone()),
        })?;

        Ok(vp)
    }

    fn handle_record_deadend(
        &mut self,
        mut edge: NegativeEdge,
        restore_base: Option<(Option<String>, Blake3Hash)>,
    ) -> Result<NegativeEdge, RpcError> {
        let meta = self
            .db
            .get_repo_meta()
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
            .ok_or_else(|| {
                RpcError::new(
                    RpcErrorCode::NotInitialized,
                    "repository metadata not initialized",
                )
            })?;

        let mut vp = self
            .db
            .resolve_viewport(&edge.viewport_id)
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
            .ok_or_else(|| {
                RpcError::new(
                    RpcErrorCode::NotFound,
                    format!("viewport `{}` not found", edge.viewport_id),
                )
            })?;

        if vp.state == ViewportState::Verifying {
            return Err(RpcError::new(
                RpcErrorCode::Conflict,
                format!(
                    "cannot record deadend for viewport `{}` while sealing/verifying",
                    vp.name
                ),
            ));
        }
        if !vp.state.is_active() {
            return Err(RpcError::new(
                RpcErrorCode::InvalidState,
                format!(
                    "cannot record deadend for viewport `{}` in state {}",
                    vp.name, vp.state
                ),
            ));
        }

        edge.edge_seq = meta.next_edge_seq.max(1);
        let summary = format!("Record deadend ({:?}): {}", edge.failure_kind, edge.lesson_summary);
        let effect = if let Some((base_capsule_id, base_snapshot_cas)) = restore_base {
            vp.base_capsule_id = base_capsule_id;
            vp.base_snapshot_cas = Some(base_snapshot_cas);
            vp.base_op_seq = meta.next_op_seq.max(1);
            vp.patch_ids.clear();
            vp.head_cas = None;
            vp.updated_at_ns = now_ns();
            StateEffect::Batch(vec![
                StateEffect::InsertNegativeEdge(edge.clone()),
                StateEffect::UpsertViewport(vp),
            ])
        } else {
            StateEffect::InsertNegativeEdge(edge.clone())
        };
        let _ = self.handle_mutate(MutationRequest {
            kind: OpKind::NegativeEdgeRecord,
            viewport_id: Some(edge.viewport_id.clone()),
            target_id: Some(edge.edge_id.clone()),
            capsule_id: None,
            touched_symbols: edge.touched_symbols.clone(),
            summary,
            effect,
        })?;
        Ok(edge)
    }

    fn handle_record_ast_patch(
        &mut self,
        viewport_id: &str,
        patch: AstPatch,
    ) -> Result<Option<AstPatchRecord>, RpcError> {
        let meta = self
            .db
            .get_repo_meta()
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
            .ok_or_else(|| {
                RpcError::new(
                    RpcErrorCode::NotInitialized,
                    "repository metadata not initialized",
                )
            })?;

        let Some(mut vp) = self
            .db
            .resolve_viewport(viewport_id)
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
        else {
            return Ok(None);
        };
        if !vp.state.is_active() {
            return Ok(None);
        }

        let patch_id = format!("patch-{}", patch.patch_hash.short_hex(16));
        if vp.patch_ids.contains(&patch_id)
            && let Some(existing) = self
                .db
                .get_patch(&patch_id)
                .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
        {
            return Ok(Some(existing));
        }

        let next_lsn = meta.next_lsn.max(self.wal.last_lsn() + 1);
        let touched_symbols = capsule::collect_patch_symbols_vec(&patch);
        let mut rec = AstPatchRecord {
            patch_id: patch_id.clone(),
            lsn: next_lsn,
            patch,
        };

        vp.patch_ids.push(patch_id.clone());
        vp.head_cas = rec.patch.cas_after;
        vp.head_lsn = next_lsn;
        vp.lamport_ts = vp.lamport_ts.max(rec.patch.lamport_ts);
        vp.updated_at_ns = now_ns();

        let (lsn, _) = self.handle_mutate(MutationRequest {
            kind: OpKind::AstPatch,
            viewport_id: Some(vp.id.clone()),
            target_id: Some(patch_id),
            capsule_id: None,
            touched_symbols,
            summary: format!("Patch `{}` in `{}`", rec.patch.file_path.display(), vp.name),
            effect: StateEffect::Batch(vec![
                StateEffect::UpsertPatch(rec.clone()),
                StateEffect::UpsertViewport(vp),
            ]),
        })?;
        rec.lsn = lsn;
        Ok(Some(rec))
    }

    fn handle_drop_viewport(
        &mut self,
        viewport_id: &str,
        mut edge: NegativeEdge,
        extra_patches: Vec<AstPatch>,
        post_clean_snapshot: Option<Blake3Hash>,
    ) -> Result<(ViewportRecord, NegativeEdge), RpcError> {
        let meta = self
            .db
            .get_repo_meta()
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
            .ok_or_else(|| {
                RpcError::new(
                    RpcErrorCode::NotInitialized,
                    "repository metadata not initialized",
                )
            })?;

        let mut vp = self
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
                format!("cannot drop viewport `{}` while verifying/sealing", vp.name),
            ));
        }
        if !vp.state.can_transition_to(ViewportState::Dropped) {
            return Err(RpcError::new(
                RpcErrorCode::InvalidState,
                format!("cannot drop viewport `{}` in state {}", vp.name, vp.state),
            ));
        }

        let next_lsn = meta.next_lsn.max(self.wal.last_lsn() + 1);
        edge.edge_seq = meta.next_edge_seq.max(1);

        let mut effects = Vec::with_capacity(extra_patches.len() + 2);
        for patch in extra_patches {
            let pid = format!("patch-{}", patch.patch_hash.short_hex(16));
            if !vp.patch_ids.contains(&pid) {
                vp.patch_ids.push(pid.clone());
                effects.push(StateEffect::UpsertPatch(AstPatchRecord {
                    patch_id: pid,
                    lsn: next_lsn,
                    patch,
                }));
            }
        }

        if let Some(snap) = post_clean_snapshot {
            vp.base_capsule_id = meta.head_capsule_id.clone();
            vp.base_snapshot_cas = Some(snap);
            vp.base_op_seq = meta.next_op_seq.max(1);
        }
        vp.state = ViewportState::Dropped;
        vp.head_lsn = next_lsn;
        vp.lamport_ts = meta.lamport_clock + 1;
        vp.updated_at_ns = now_ns();

        effects.push(StateEffect::InsertNegativeEdge(edge.clone()));
        effects.push(StateEffect::UpsertViewport(vp.clone()));

        let summary = format!(
            "Drop viewport `{}` ({}): {}",
            vp.name, edge.failure_kind, edge.lesson_summary
        );
        let _ = self.handle_mutate(MutationRequest {
            kind: OpKind::ViewportDrop,
            viewport_id: Some(vp.id.clone()),
            target_id: Some(edge.edge_id.clone()),
            capsule_id: None,
            touched_symbols: edge.touched_symbols.clone(),
            summary,
            effect: StateEffect::Batch(effects),
        })?;

        Ok((vp, edge))
    }

    fn handle_mutate(&mut self, req: MutationRequest) -> Result<(u64, u64), RpcError> {
        let MutationRequest {
            kind,
            viewport_id,
            target_id,
            capsule_id,
            touched_symbols,
            summary,
            mut effect,
        } = req;
        let mut meta = self
            .db
            .get_repo_meta()
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
            .ok_or_else(|| {
                RpcError::new(
                    RpcErrorCode::NotInitialized,
                    "repository metadata not initialized",
                )
            })?;

        let now = now_ns();
        let lsn = meta.next_lsn.max(self.wal.last_lsn() + 1);
        let op_seq = meta.next_op_seq.max(1);
        let mut next_edge_seq = meta.next_edge_seq.max(1);
        assign_effect_sequences(&mut effect, &mut next_edge_seq, lsn);

        let mut inverse = self
            .db
            .compute_inverse_effect(&effect)
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
        if kind == OpKind::CapsuleSeal {
            normalize_capsule_seal_inverse(&mut inverse);
        }
        normalize_destructive_clean_inverse(kind, &effect, &mut inverse);
        if kind == OpKind::CapsuleSeal
            && !matches!(inverse, InverseEffect::None)
        {
            let restore_head = InverseEffect::RestoreHeadCapsule(meta.head_capsule_id.clone());
            match &mut inverse {
                InverseEffect::Batch(items) => items.push(restore_head),
                _ => {
                    let prev = std::mem::take(&mut inverse);
                    inverse = InverseEffect::Batch(vec![prev, restore_head]);
                }
            }
        }
        let inverse_bytes = if matches!(inverse, InverseEffect::None)
            || matches!(
                kind,
                OpKind::GitPull
                    | OpKind::GitPush
                    | OpKind::RemotePull
                    | OpKind::RemotePush
                    | OpKind::Gc
            )
            || (kind != OpKind::CapsuleSeal
                && (capsule::inverse_has_verifying_restore(&inverse)
                    || capsule::effect_transitions_to_verifying(&effect)))
        {
            None
        } else {
            Some(
                encode_value(&inverse)
                    .map_err(|e| RpcError::new(RpcErrorCode::Internal, e.to_string()))?,
            )
        };
        let op = OpLogEntry {
            op_seq,
            lsn,
            kind,
            viewport_id,
            target_id,
            capsule_id,
            touched_symbols,
            summary,
            inverse_payload: inverse_bytes,
            undone_by_op: None,
            timestamp_ns: now,
        };

        let payload = WalPayload::Op {
            op: op.clone(),
            effect: effect.clone(),
        };
        self.wal
            .append(WalRecordKind::Mutation, lsn, now, &payload)
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;

        meta.next_lsn = lsn + 1;
        meta.next_op_seq = op_seq + 1;
        meta.last_applied_lsn = lsn;
        meta.lamport_clock += 1;
        meta.updated_at_ns = now;
        advance_meta_for_effect(&mut meta, kind, &effect);

        let wtx = self
            .db
            .begin_write()
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
        StateDb::apply_state_effect_in_wtx(&wtx, &effect)
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
        {
            let mut op_table = wtx
                .open_table(TABLE_OP_LOG)
                .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
            StateDb::append_op_in_tx(&mut op_table, &op)
                .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
            StateDb::set_repo_meta_in_tx(&mut op_table, &meta)
                .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
        }
        wtx.commit()
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;

        if let Ok(active_count) = self.db.count_active_viewports() {
            self.idle_monitor.set_active_viewports(active_count);
        }

        self.ops_since_checkpoint += 1;
        self.maybe_checkpoint()
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;

        Ok((lsn, op_seq))
    }

    fn handle_undo(&mut self, op_seq_opt: Option<u64>) -> Result<UndoResult, RpcError> {
        let mut meta = self
            .db
            .get_repo_meta()
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
            .ok_or_else(|| {
                RpcError::new(
                    RpcErrorCode::NotInitialized,
                    "repository metadata not initialized",
                )
            })?;

        let target = match op_seq_opt {
            Some(seq) => self
                .db
                .get_op(seq)
                .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
                .ok_or_else(|| {
                    RpcError::new(RpcErrorCode::NotFound, format!("op_seq {seq} not found"))
                })?,
            None => self
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
        let mut inverse: InverseEffect = decode_value(inv_bytes)
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
        if matches!(target.kind, OpKind::ViewportDrop | OpKind::NegativeEdgeRecord) {
            clear_restore_viewport_patch_state(&mut inverse);
        }

        capsule::validate_undo_target(&self.db, &target, &inverse)?;

        let now = now_ns();
        let lsn = meta.next_lsn.max(self.wal.last_lsn() + 1);
        let new_op_seq = meta.next_op_seq.max(1);
        let summary = format!("Undo op #{} ({})", target.op_seq, target.summary);
        let undo_op = OpLogEntry {
            op_seq: new_op_seq,
            lsn,
            kind: OpKind::Undo,
            viewport_id: target.viewport_id.clone(),
            target_id: Some(target.op_seq.to_string()),
            capsule_id: None,
            touched_symbols: target.touched_symbols.clone(),
            summary: summary.clone(),
            inverse_payload: None,
            undone_by_op: None,
            timestamp_ns: now,
        };

        let payload = WalPayload::Undo {
            undo_op: undo_op.clone(),
            target_op_seq: target.op_seq,
            inverse: inverse.clone(),
        };
        self.wal
            .append(WalRecordKind::Undo, lsn, now, &payload)
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;

        meta.next_lsn = lsn + 1;
        meta.next_op_seq = new_op_seq + 1;
        meta.last_applied_lsn = lsn;
        meta.lamport_clock += 1;
        meta.updated_at_ns = now;

        let wtx = self
            .db
            .begin_write()
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
        StateDb::apply_inverse_effect_in_wtx(&wtx, &inverse)
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
        apply_inverse_meta_updates(&wtx, &mut meta, &inverse)
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
        {
            let mut op_table = wtx
                .open_table(TABLE_OP_LOG)
                .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
            StateDb::append_op_in_tx(&mut op_table, &undo_op)
                .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
            StateDb::mark_op_undone_in_tx(&mut op_table, target.op_seq, new_op_seq)
                .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
            StateDb::set_repo_meta_in_tx(&mut op_table, &meta)
                .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
        }
        wtx.commit()
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;

        if let Ok(active_count) = self.db.count_active_viewports() {
            self.idle_monitor.set_active_viewports(active_count);
        }

        self.ops_since_checkpoint += 1;
        self.maybe_checkpoint()
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;

        Ok(UndoResult {
            undone_op_seq: target.op_seq,
            compensating_op_seq: new_op_seq,
            summary,
        })
    }

    fn handle_gc(
        &mut self,
        prune_viewports: bool,
        cas_dir: &Path,
        viewports_dir: &Path,
        cache_dirs: &[String],
        zstd_level: i32,
    ) -> Result<GcResult, RpcError> {
        let all_vps = self
            .db
            .list_viewports(true)
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;

        let now = now_ns();
        let mut to_transition = Vec::new();
        for mut vp in all_vps {
            if matches!(vp.state, ViewportState::Sealed | ViewportState::Dropped) {
                vp.state = ViewportState::GcPending;
                vp.updated_at_ns = now;
                to_transition.push(StateEffect::UpsertViewport(vp));
            }
        }
        let transitioned_viewports = to_transition.len();
        if !to_transition.is_empty() {
            let _ = self.handle_mutate(MutationRequest {
                kind: OpKind::Gc,
                viewport_id: None,
                target_id: None,
                capsule_id: None,
                touched_symbols: Vec::new(),
                summary: format!("Marked {transitioned_viewports} viewport(s) GC_Pending"),
                effect: StateEffect::Batch(to_transition),
            })?;
        }

        let mut pruned_viewport_dirs = 0usize;
        let mut pruned_viewport_ids = Vec::new();
        let mut pruned_viewport_names = Vec::new();
        if prune_viewports {
            let canon_viewports_dir = std::fs::canonicalize(viewports_dir)
                .unwrap_or_else(|_| viewports_dir.to_path_buf());
            let mut vps_now = self
                .db
                .list_viewports(true)
                .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
            vps_now.sort_by(|a, b| {
                b.created_at_ns
                    .cmp(&a.created_at_ns)
                    .then_with(|| b.id.cmp(&a.id))
            });

            let mut occupied_paths: HashSet<PathBuf> = HashSet::new();
            for v in &vps_now {
                if v.state != ViewportState::GcPending {
                    if !v.path.as_os_str().is_empty() {
                        occupied_paths.insert(v.path.clone());
                        if let Ok(canon) = std::fs::canonicalize(&v.path) {
                            occupied_paths.insert(canon);
                        }
                    }
                    occupied_paths.insert(viewports_dir.join(&v.id));
                }
            }

            let mut mount_clears = Vec::new();
            for mut vp in vps_now {
                if vp.state != ViewportState::GcPending || vp.pruned_at_ns.is_some() {
                    continue;
                }
                let candidate_by_id = viewports_dir.join(&vp.id);
                let target_opt = if candidate_by_id.exists() {
                    Some(candidate_by_id.clone())
                } else if vp.path.exists() {
                    let canon_vp =
                        std::fs::canonicalize(&vp.path).unwrap_or_else(|_| vp.path.clone());
                    if canon_vp.starts_with(&canon_viewports_dir)
                        || vp.path.starts_with(viewports_dir)
                    {
                        Some(vp.path.clone())
                    } else {
                        None
                    }
                } else {
                    None
                };

                if let Some(target_path) = target_opt {
                    let is_occupied = occupied_paths.contains(&target_path)
                        || std::fs::canonicalize(&target_path)
                            .map(|c| occupied_paths.contains(&c))
                            .unwrap_or(false);
                    if is_occupied {
                        continue;
                    }

                    let drv = loom_vfs::driver_for_backend(vp.driver, cache_dirs);
                    drv.teardown_viewport(&target_path, vp.mount.as_ref())
                        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
                    pruned_viewport_dirs += 1;
                    pruned_viewport_ids.push(vp.id.clone());
                    pruned_viewport_names.push(vp.name.clone());
                    let ts = now_ns();
                    vp.mount = None;
                    vp.pruned_at_ns = Some(ts);
                    vp.updated_at_ns = ts;
                    mount_clears.push(StateEffect::UpsertViewport(vp));
                } else {
                    let path_occupied = occupied_paths.contains(&candidate_by_id)
                        || (!vp.path.as_os_str().is_empty()
                            && (occupied_paths.contains(&vp.path)
                                || std::fs::canonicalize(&vp.path)
                                    .map(|c| occupied_paths.contains(&c))
                                    .unwrap_or(false)));
                    if !path_occupied {
                        let ts = now_ns();
                        vp.mount = None;
                        vp.pruned_at_ns = Some(ts);
                        vp.updated_at_ns = ts;
                        mount_clears.push(StateEffect::UpsertViewport(vp));
                    }
                }
            }

            if !mount_clears.is_empty() {
                let _ = self.handle_mutate(MutationRequest {
                    kind: OpKind::Gc,
                    viewport_id: None,
                    target_id: None,
                    capsule_id: None,
                    touched_symbols: Vec::new(),
                    summary: format!("Pruned {pruned_viewport_dirs} GC_Pending viewport dir(s)"),
                    effect: StateEffect::Batch(mount_clears),
                })?;
            }
        }

        let cas = CasStore::new(cas_dir, zstd_level)
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
        let mut referenced = self
            .db
            .collect_referenced_cas_hashes()
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;

        // Expand live WorkspaceSnapshot manifests referenced by non-GC viewports and capsules
        let mut manifest_hashes: HashSet<Blake3Hash> = HashSet::new();
        if let Ok(live_vps) = self.db.list_viewports(false) {
            for vp in live_vps {
                if let Some(h) = vp.base_snapshot_cas {
                    manifest_hashes.insert(h);
                }
            }
        }
        if let Ok(caps) = self.db.list_capsules() {
            for cap in caps {
                if let Some(h) = cap.snapshot_cas_hash {
                    manifest_hashes.insert(h);
                }
            }
        }
        if let Ok(op_snaps) = self.db.collect_unundone_trunk_snapshot_hashes() {
            manifest_hashes.extend(op_snaps);
        }
        for mh in manifest_hashes {
            if let Ok(raw) = cas.get_bytes(&mh)
                && let Ok(snap) = rmp_serde::from_slice::<WorkspaceSnapshot>(&raw)
            {
                referenced.extend(snap.files.values().copied());
            }
        }
        let mut removed_cas_blobs = 0usize;
        let mut reclaimed_cas_bytes = 0u64;
        for hash in cas
            .list_hashes()
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?
        {
            if !referenced.contains(&hash) {
                let size = std::fs::metadata(cas.object_path(&hash))
                    .map(|m| m.len())
                    .unwrap_or(0);
                if cas
                    .delete(&hash)
                    .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?
                {
                    removed_cas_blobs += 1;
                    reclaimed_cas_bytes += size;
                }
            }
        }

        let _ = self.force_checkpoint();
        let ckpt_lsn = self
            .db
            .get_repo_meta()
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
            .map_or(0, |m| m.last_checkpoint_lsn);
        let removed_wals = self
            .wal
            .compact_up_to(ckpt_lsn)
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;

        Ok(GcResult {
            transitioned_viewports,
            pruned_viewport_dirs,
            pruned_viewport_ids,
            pruned_viewport_names,
            removed_cas_blobs,
            reclaimed_cas_bytes,
            compacted_wal_segments: removed_wals.len(),
        })
    }

    fn handle_sync_symbol_tables(
        &self,
        remove_provides: &[(String, String)],
        upsert_provides: &[SymbolProvideEntry],
        remove_consumes: &[(String, String)],
        upsert_consumes: &[SymbolConsumeEntry],
    ) -> Result<(), RpcError> {
        let wtx = self
            .db
            .begin_write()
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
        {
            let mut sp = wtx
                .open_table(TABLE_SYMBOL_PROVIDES)
                .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
            for (sym, vp) in remove_provides {
                StateDb::remove_symbol_provide_in_tx(&mut sp, sym, vp)
                    .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
            }
            for entry in upsert_provides {
                StateDb::upsert_symbol_provide_in_tx(&mut sp, entry)
                    .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
            }
        }
        {
            let mut sc = wtx
                .open_table(TABLE_SYMBOL_CONSUMES)
                .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
            for (sym, vp) in remove_consumes {
                StateDb::remove_symbol_consume_in_tx(&mut sc, sym, vp)
                    .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
            }
            for entry in upsert_consumes {
                StateDb::upsert_symbol_consume_in_tx(&mut sc, entry)
                    .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
            }
        }
        wtx.commit()
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
        Ok(())
    }

    fn handle_clear_viewport_symbols(&self, viewport_id: &str) -> Result<(), RpcError> {
        let wtx = self
            .db
            .begin_write()
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
        StateDb::clear_viewport_symbols_in_wtx(&wtx, viewport_id)
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
        wtx.commit()
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
        Ok(())
    }

    fn maybe_checkpoint(&mut self) -> anyhow::Result<()> {
        if self.ops_since_checkpoint >= self.checkpoint_ops {
            self.force_checkpoint()
                .map_err(|e| anyhow::anyhow!(e.message))?;
        }
        Ok(())
    }

    fn force_checkpoint(&mut self) -> Result<(), RpcError> {
        let Some(mut meta) = self
            .db
            .get_repo_meta()
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?
        else {
            return Ok(());
        };
        if self.ops_since_checkpoint == 0 && meta.last_checkpoint_lsn >= meta.last_applied_lsn {
            return Ok(());
        }

        let ckpt_lsn = meta.next_lsn.max(self.wal.last_lsn() + 1);
        let last_op_seq = meta.next_op_seq.saturating_sub(1);
        let now = now_ns();
        self.wal
            .append_checkpoint(ckpt_lsn, now, meta.last_applied_lsn, last_op_seq)
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;

        meta.next_lsn = ckpt_lsn + 1;
        meta.last_applied_lsn = ckpt_lsn;
        meta.last_checkpoint_lsn = ckpt_lsn;
        meta.updated_at_ns = now;

        let wtx = self
            .db
            .begin_write()
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
        {
            let mut op_table = wtx
                .open_table(TABLE_OP_LOG)
                .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
            StateDb::set_repo_meta_in_tx(&mut op_table, &meta)
                .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
        }
        wtx.commit()
            .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;

        self.wal
            .compact_up_to(ckpt_lsn)
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
        self.ops_since_checkpoint = 0;
        Ok(())
    }
}

fn assign_effect_sequences(effect: &mut StateEffect, next_edge_seq: &mut u64, lsn: u64) {
    match effect {
        StateEffect::InsertNegativeEdge(edge) => {
            if edge.edge_seq == 0 {
                edge.edge_seq = *next_edge_seq;
                *next_edge_seq += 1;
            } else {
                *next_edge_seq = (*next_edge_seq).max(edge.edge_seq + 1);
            }
        }
        StateEffect::UpsertPatch(rec) => {
            rec.lsn = lsn;
        }
        StateEffect::Batch(effects) => {
            for eff in effects {
                assign_effect_sequences(eff, next_edge_seq, lsn);
            }
        }
        _ => {}
    }
}

pub(crate) fn normalize_capsule_seal_inverse(inverse: &mut InverseEffect) {
    match inverse {
        InverseEffect::RestoreViewport(vp) => {
            if vp.state == ViewportState::Verifying {
                vp.state = ViewportState::Dropped;
            }
        }
        InverseEffect::Batch(items) => {
            for item in items {
                normalize_capsule_seal_inverse(item);
            }
        }
        _ => {}
    }
}

fn collect_effect_upserted_viewports<'a>(
    effect: &'a StateEffect,
    out: &mut std::collections::HashMap<&'a str, &'a ViewportRecord>,
) {
    match effect {
        StateEffect::UpsertViewport(vp) => {
            out.insert(vp.id.as_str(), vp);
        }
        StateEffect::Batch(items) => {
            for item in items {
                collect_effect_upserted_viewports(item, out);
            }
        }
        _ => {}
    }
}

pub(crate) fn clear_restore_viewport_patch_state(inverse: &mut InverseEffect) {
    match inverse {
        InverseEffect::RestoreViewport(vp) => {
            vp.patch_ids.clear();
            vp.head_cas = None;
        }
        InverseEffect::Batch(items) => {
            for item in items {
                clear_restore_viewport_patch_state(item);
            }
        }
        _ => {}
    }
}

fn sync_restore_viewport_from_clean_effect(
    inverse: &mut InverseEffect,
    upserted: &std::collections::HashMap<&str, &ViewportRecord>,
    sync_base: bool,
) {
    match inverse {
        InverseEffect::RestoreViewport(old_vp) => {
            if let Some(new_vp) = upserted.get(old_vp.id.as_str()) {
                old_vp.patch_ids.clear();
                old_vp.head_cas = None;
                if sync_base {
                    old_vp.base_capsule_id = new_vp.base_capsule_id.clone();
                    old_vp.base_snapshot_cas = new_vp.base_snapshot_cas;
                    old_vp.base_op_seq = new_vp.base_op_seq;
                }
            }
        }
        InverseEffect::Batch(items) => {
            for item in items {
                sync_restore_viewport_from_clean_effect(item, upserted, sync_base);
            }
        }
        _ => {}
    }
}

pub(crate) fn normalize_destructive_clean_inverse(
    kind: OpKind,
    effect: &StateEffect,
    inverse: &mut InverseEffect,
) {
    match kind {
        OpKind::ViewportDrop | OpKind::NegativeEdgeRecord => {
            let mut upserted = std::collections::HashMap::new();
            collect_effect_upserted_viewports(effect, &mut upserted);
            if !upserted.is_empty() {
                sync_restore_viewport_from_clean_effect(inverse, &upserted, true);
            }
        }
        OpKind::ViewportTransition => {
            let mut upserted = std::collections::HashMap::new();
            collect_effect_upserted_viewports(effect, &mut upserted);
            upserted.retain(|_, vp| vp.patch_ids.is_empty() && vp.head_cas.is_none());
            if !upserted.is_empty() {
                sync_restore_viewport_from_clean_effect(inverse, &upserted, false);
            }
        }
        _ => {}
    }
}

fn extract_restore_head_capsule(inverse: &InverseEffect) -> Option<Option<String>> {
    match inverse {
        InverseEffect::RestoreHeadCapsule(prev) => Some(prev.clone()),
        InverseEffect::Batch(items) => items.iter().rev().find_map(extract_restore_head_capsule),
        _ => None,
    }
}

pub(crate) fn apply_inverse_meta_updates(
    wtx: &redb::WriteTransaction,
    meta: &mut RepoMeta,
    inverse: &InverseEffect,
) -> anyhow::Result<()> {
    if let Some(prev_head) = extract_restore_head_capsule(inverse) {
        let cap_table = wtx.open_table(TABLE_CAPSULES)?;
        let head_deleted = match meta.head_capsule_id.as_deref() {
            Some(cur) => cap_table.get(cur)?.is_none(),
            None => false,
        };
        if head_deleted {
            meta.head_capsule_id = match prev_head {
                Some(ref pid) if cap_table.get(pid.as_str())?.is_some() => Some(pid.clone()),
                _ => None,
            };
        }
    }
    Ok(())
}

pub(crate) fn advance_meta_for_effect(
    meta: &mut RepoMeta,
    op_kind: OpKind,
    effect: &StateEffect,
) {
    match effect {
        StateEffect::InsertNegativeEdge(edge) => {
            meta.next_edge_seq = meta.next_edge_seq.max(edge.edge_seq + 1);
        }
        StateEffect::UpsertCapsule(cap) => {
            if matches!(
                op_kind,
                OpKind::CapsuleSeal | OpKind::GitPull | OpKind::RemotePull
            ) {
                meta.head_capsule_id = Some(cap.capsule_id.clone());
            }
        }
        StateEffect::Batch(effects) => {
            for eff in effects {
                advance_meta_for_effect(meta, op_kind, eff);
            }
        }
        _ => {}
    }
}

// ============================================================================
// Daemon Entrypoint & Context
// ============================================================================

pub struct DaemonContext {
    pub repo_root: PathBuf,
    pub loom_dir: PathBuf,
    pub config: LoomConfig,
    pub db: Arc<StateDb>,
    pub writer: WriterHandle,
    pub idle_monitor: Arc<IdleMonitor>,
    pub radar: radar::RadarHandle,
    pub descriptor: SocketDescriptor,
    pub started_at: Instant,
    pub trunk_seal_lock: tokio::sync::Mutex<()>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonRunExit {
    AlreadyRunning,
    IdleTimeout,
    ShutdownRequested,
}

pub struct RunningDaemon {
    pub ctx: Arc<DaemonContext>,
    task: tokio::task::JoinHandle<anyhow::Result<DaemonRunExit>>,
}

impl RunningDaemon {
    pub async fn wait(self) -> anyhow::Result<DaemonRunExit> {
        let Self { ctx, task } = self;
        drop(ctx);
        task.await?
    }
}

pub enum StartDaemonOutcome {
    Started(RunningDaemon),
    AlreadyRunning,
}

pub async fn start_daemon(
    repo_root: impl AsRef<Path>,
    idle_timeout_override_ms: Option<u64>,
) -> anyhow::Result<StartDaemonOutcome> {
    let repo_root = canonical_repo_root(repo_root.as_ref())?;
    let loom_dir = repo_root.join(".loom");
    std::fs::create_dir_all(&loom_dir)?;

    // 1. Acquire exclusive RepoLock BEFORE touching socket/DB/WAL.
    let repo_lock = match RepoLock::try_acquire(&repo_root)? {
        LockAcquireOutcome::Acquired(lock) => lock,
        LockAcquireOutcome::AlreadyRunning => {
            tracing::info!(repo = %repo_root.display(), "another loomd instance holds lock; exiting 0");
            return Ok(StartDaemonOutcome::AlreadyRunning);
        }
    };

    // 2. Load config
    let config_path = loom_dir.join("config.toml");
    let mut config = if config_path.is_file() {
        LoomConfig::load(&config_path)?
    } else {
        LoomConfig::default()
    };
    if let Some(ms) = idle_timeout_override_ms {
        config.idle_timeout_ms = Some(ms);
    }
    let idle_timeout = config.idle_timeout();

    // 3. Startup crash recovery (WAL replay + torn-tail truncation + checkpoint)
    let outcome = recovery::recover(&loom_dir, &config)?;
    let db = Arc::new(outcome.db);
    let active_vps = db.count_active_viewports()?;
    let idle_monitor = Arc::new(IdleMonitor::new(idle_timeout, active_vps));

    // 4. Spawn single-writer actor
    let (writer, writer_thread) = WriterActorState::spawn(
        Arc::clone(&db),
        outcome.wal,
        config.wal_checkpoint_ops,
        Arc::clone(&idle_monitor),
    );

    // 4b. Start Speculative Merge Radar and bootstrap active viewports from recovered DB
    let radar = radar::RadarHandle::start(
        repo_root.clone(),
        loom_dir.clone(),
        config.clone(),
        Arc::clone(&db),
        writer.clone(),
    )?;
    radar.bootstrap_from_db().await;

    // 5. Bind IPC listener
    let (listener, descriptor) = ipc::bind_listener(&repo_root)?;

    let ctx = Arc::new(DaemonContext {
        repo_root: repo_root.clone(),
        loom_dir: loom_dir.clone(),
        config,
        db,
        writer: writer.clone(),
        idle_monitor: Arc::clone(&idle_monitor),
        radar: radar.clone(),
        descriptor,
        started_at: Instant::now(),
        trunk_seal_lock: tokio::sync::Mutex::new(()),
    });

    let ctx_loop = Arc::clone(&ctx);
    let task = tokio::spawn(async move {
        let _repo_lock = repo_lock;
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let mut conn_tasks = tokio::task::JoinSet::new();

        let exit_reason = loop {
            tokio::select! {
                accept_res = listener.accept() => {
                    match accept_res {
                        Ok(stream) => {
                            let c = Arc::clone(&ctx_loop);
                            let stx = shutdown_tx.clone();
                            conn_tasks.spawn(async move {
                                ipc::serve_connection(stream, c, stx).await;
                            });
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "IPC accept error");
                        }
                    }
                }
                Some(_) = conn_tasks.join_next(), if !conn_tasks.is_empty() => {}
                _ = idle_monitor.wait_for_idle_shutdown() => {
                    break DaemonRunExit::IdleTimeout;
                }
                res = shutdown_rx.changed() => {
                    if res.is_ok() && *shutdown_rx.borrow() {
                        break DaemonRunExit::ShutdownRequested;
                    }
                }
            }
        };

        // Clean shutdown: remove daemon.sock first so clients know shutdown began,
        // signal active connections, drain any connection already waiting in the
        // OS pipe/socket accept queue so it receives an explicit AlreadyLocked error,
        // then drop the listener and drain connection tasks.
        let _ = std::fs::remove_file(sock_descriptor_path(&repo_root));
        let _ = shutdown_tx.send(true);
        while let Ok(Ok(stream)) =
            tokio::time::timeout(Duration::from_millis(5), listener.accept()).await
        {
            let c = Arc::clone(&ctx_loop);
            let stx = shutdown_tx.clone();
            conn_tasks.spawn(async move {
                ipc::serve_connection(stream, c, stx).await;
            });
        }
        drop(shutdown_tx);
        drop(listener);
        let drain_res = tokio::time::timeout(Duration::from_secs(2), async {
            while conn_tasks.join_next().await.is_some() {}
        })
        .await;
        if drain_res.is_err() {
            conn_tasks.abort_all();
            while conn_tasks.join_next().await.is_some() {}
        }
        radar.shutdown().await;
        let _ = writer.checkpoint_and_stop().await;
        drop(writer);
        let _ = tokio::task::spawn_blocking(move || {
            let _ = writer_thread.join();
        })
        .await;
        drop(ctx_loop);
        drop(_repo_lock);

        Ok(exit_reason)
    });

    Ok(StartDaemonOutcome::Started(RunningDaemon { ctx, task }))
}

pub async fn run_daemon(
    repo_root: impl AsRef<Path>,
    idle_timeout_override_ms: Option<u64>,
) -> anyhow::Result<DaemonRunExit> {
    match start_daemon(repo_root, idle_timeout_override_ms).await? {
        StartDaemonOutcome::AlreadyRunning => Ok(DaemonRunExit::AlreadyRunning),
        StartDaemonOutcome::Started(running) => running.wait().await,
    }
}

pub fn run_cli_from_args() -> anyhow::Result<()> {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let mut repo = PathBuf::from(".");
    let mut idle_timeout_ms = None;
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--repo" && i + 1 < args.len() {
            repo = PathBuf::from(&args[i + 1]);
            i += 2;
        } else if args[i] == "--idle-timeout-ms" && i + 1 < args.len() {
            idle_timeout_ms = args[i + 1].to_string_lossy().parse::<u64>().ok();
            i += 2;
        } else {
            i += 1;
        }
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let _ = run_daemon(repo, idle_timeout_ms).await?;
        Ok::<(), anyhow::Error>(())
    })
}
