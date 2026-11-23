use crate::radar::filter_snapshot;
use crate::{
    DaemonContext, WriterCommand, capsule, ensure_git_info_exclude, git_sync, inspect_git_repo,
    viewport_mgr,
};
use interprocess::local_socket::{
    ListenerOptions,
    tokio::{Listener, Stream},
};
use loom_core::cas::CasStore;
use loom_core::model::{OpKind, ViewportState};
use loom_core::rpc::{
    DaemonStatus, InitResult, OpLogView, PROTOCOL_VERSION, RadarStreamEvent, RpcError,
    RpcErrorCode, RpcMethod, RpcPayload, RpcRequest, RpcResponse, SocketDescriptor,
    pipe_full_path, pipe_short_name, read_frame, write_frame, write_sock_descriptor_atomic,
};
#[cfg(unix)]
use loom_core::rpc::sock_descriptor_path;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::{broadcast, oneshot, watch};

pub fn bind_listener(repo_root: &Path) -> anyhow::Result<(Listener, SocketDescriptor)> {
    #[cfg(windows)]
    {
        use interprocess::local_socket::{GenericNamespaced, ToNsName};
        let short = pipe_short_name(repo_root)?;
        let full = pipe_full_path(repo_root)?;
        let ns_name = short.as_str().to_ns_name::<GenericNamespaced>()?;
        let listener = ListenerOptions::new().name(ns_name).create_tokio()?;
        let desc = SocketDescriptor {
            transport: "named_pipe".to_string(),
            endpoint: full,
            pid: std::process::id(),
            protocol_version: PROTOCOL_VERSION,
        };
        write_sock_descriptor_atomic(repo_root, &desc)?;
        Ok((listener, desc))
    }

    #[cfg(unix)]
    {
        use interprocess::local_socket::{GenericFilePath, ToFsName};
        let sock_path = sock_descriptor_path(repo_root);
        if let Some(parent) = sock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if sock_path.exists() {
            let _ = std::fs::remove_file(&sock_path);
        }
        let fs_name = sock_path.as_path().to_fs_name::<GenericFilePath>()?;
        let listener = ListenerOptions::new().name(fs_name).create_tokio()?;
        let desc = SocketDescriptor {
            transport: "unix_socket".to_string(),
            endpoint: sock_path.to_string_lossy().into_owned(),
            pid: std::process::id(),
            protocol_version: PROTOCOL_VERSION,
        };
        Ok((listener, desc))
    }
}

pub async fn serve_connection(
    mut stream: Stream,
    ctx: Arc<DaemonContext>,
    shutdown_tx: watch::Sender<bool>,
) {
    let mut shutdown_rx = shutdown_tx.subscribe();
    let mut dispatched_any = false;
    loop {
        if *shutdown_rx.borrow_and_update() {
            if !dispatched_any
                && let Ok(Ok(Some(req))) = tokio::time::timeout(
                    std::time::Duration::from_millis(50),
                    read_frame::<_, RpcRequest>(&mut stream),
                )
                .await
                && !matches!(req.method, RpcMethod::RadarSubscribe { .. })
            {
                let resp = RpcResponse {
                    id: req.id,
                    result: Err(RpcError::new(
                        RpcErrorCode::AlreadyLocked,
                        "daemon shutting down due to idle timeout",
                    )),
                };
                let _ = write_frame(&mut stream, &resp).await;
            }
            break;
        }
        let frame_res = tokio::select! {
            res = read_frame::<_, RpcRequest>(&mut stream) => res,
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow_and_update() {
                    if !dispatched_any
                        && let Ok(Ok(Some(req))) = tokio::time::timeout(
                            std::time::Duration::from_millis(50),
                            read_frame::<_, RpcRequest>(&mut stream),
                        )
                        .await
                        && !matches!(req.method, RpcMethod::RadarSubscribe { .. })
                    {
                        let resp = RpcResponse {
                            id: req.id,
                            result: Err(RpcError::new(
                                RpcErrorCode::AlreadyLocked,
                                "daemon shutting down due to idle timeout",
                            )),
                        };
                        let _ = write_frame(&mut stream, &resp).await;
                    }
                    break;
                }
                continue;
            }
        };
        match frame_res {
            Ok(Some(req)) => {
                dispatched_any = true;
                ctx.idle_monitor.touch_activity();
                if let RpcMethod::RadarSubscribe { viewport_id } = req.method {
                    let rx = ctx.radar.subscribe();
                    let initial = ctx.radar.snapshot(viewport_id.as_deref()).await;
                    let resp = RpcResponse {
                        id: req.id,
                        result: Ok(RpcPayload::RadarSnapshot(initial)),
                    };
                    if write_frame(&mut stream, &resp).await.is_err() {
                        break;
                    }
                    drop(shutdown_tx);
                    serve_radar_stream(stream, ctx, rx, shutdown_rx, viewport_id).await;
                    break;
                }
                let is_shutdown = matches!(req.method, RpcMethod::Shutdown);
                let result = dispatch_method(req.method, &ctx).await;
                let resp = RpcResponse { id: req.id, result };
                if write_frame(&mut stream, &resp).await.is_err() {
                    break;
                }
                if is_shutdown {
                    let _ = shutdown_tx.send(true);
                    break;
                }
            }
            Ok(None) => break,
            Err(e) => {
                tracing::debug!(error = %e, "IPC connection read terminated");
                break;
            }
        }
    }
}

async fn serve_radar_stream(
    stream: Stream,
    ctx: Arc<DaemonContext>,
    mut rx: broadcast::Receiver<RadarStreamEvent>,
    mut shutdown_rx: watch::Receiver<bool>,
    viewport_filter: Option<String>,
) {
    let (mut reader, mut writer) = tokio::io::split(stream);
    loop {
        if *shutdown_rx.borrow_and_update() {
            break;
        }
        tokio::select! {
            ev_res = rx.recv() => {
                match ev_res {
                    Ok(mut event) => {
                        if let Some(ref filter) = viewport_filter {
                            event.snapshot = filter_snapshot(event.snapshot, Some(filter.as_str()));
                            event.escalated_notifications.retain(|n| n.viewport_id == *filter || n.viewport_name == *filter);
                        }
                        let resp = RpcResponse {
                            id: 0,
                            result: Ok(RpcPayload::RadarEvent(event)),
                        };
                        if write_frame(&mut writer, &resp).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let snapshot = ctx.radar.snapshot(viewport_filter.as_deref()).await;
                        let event = RadarStreamEvent {
                            snapshot,
                            changed_viewports: Vec::new(),
                            escalated_notifications: Vec::new(),
                        };
                        let resp = RpcResponse {
                            id: 0,
                            result: Ok(RpcPayload::RadarEvent(event)),
                        };
                        if write_frame(&mut writer, &resp).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            req_res = read_frame::<_, RpcRequest>(&mut reader) => {
                match req_res {
                    Ok(Some(in_req)) => {
                        ctx.idle_monitor.touch_activity();
                        let method = match in_req.method {
                            RpcMethod::RadarSnapshot { viewport_id } => RpcMethod::RadarSnapshot {
                                viewport_id: viewport_id.or_else(|| viewport_filter.clone()),
                            },
                            other => other,
                        };
                        let result = dispatch_method(method, &ctx).await;
                        let resp = RpcResponse { id: in_req.id, result };
                        if write_frame(&mut writer, &resp).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) | Err(_) => break,
                }
            }
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow_and_update() {
                    break;
                }
            }
        }
    }
}

async fn dispatch_method(
    method: RpcMethod,
    ctx: &Arc<DaemonContext>,
) -> Result<RpcPayload, RpcError> {
    match method {
        RpcMethod::Ping => Ok(RpcPayload::Pong {
            protocol_version: PROTOCOL_VERSION,
            pid: std::process::id(),
        }),
        RpcMethod::Shutdown => Ok(RpcPayload::ShutdownAck),
        RpcMethod::Init { colocate_git } => handle_init(ctx, colocate_git).await,
        RpcMethod::Status => handle_status(ctx),
        RpcMethod::ListViewports { include_gc } => handle_list_viewports(ctx, include_gc),
        RpcMethod::Log {
            capsules_only,
            symbol,
            limit,
        } => handle_log(ctx, capsules_only, symbol.as_deref(), limit),
        RpcMethod::Undo { op_seq } => handle_undo(ctx, op_seq).await,
        RpcMethod::Gc { prune_viewports } => handle_gc(ctx, prune_viewports).await,
        RpcMethod::SpawnViewport { name, intent, cmd } => {
            viewport_mgr::spawn_viewport(ctx, name, intent, cmd)
                .await
                .map(RpcPayload::Viewport)
        }
        RpcMethod::TransitionViewport {
            viewport_id,
            target_state,
            pid,
        } => viewport_mgr::transition_viewport(ctx, viewport_id, target_state, pid)
            .await
            .map(RpcPayload::Viewport),
        RpcMethod::RollbackFailedSpawn { viewport_id } => {
            viewport_mgr::rollback_failed_spawn_viewport(ctx, &viewport_id)
                .await
                .map(RpcPayload::Viewport)
        }
        RpcMethod::RestoreViewportClean { viewport_id } => {
            viewport_mgr::restore_clean(ctx, &viewport_id)
                .await
                .map(RpcPayload::Viewport)
        }
        RpcMethod::SealCapsule {
            viewport_id,
            verify_cmd,
            summary,
        } => capsule::seal_capsule(ctx, &viewport_id, verify_cmd, summary)
            .await
            .map(RpcPayload::CapsuleSeal),
        RpcMethod::RadarSnapshot { viewport_id } => {
            let _ = require_repo_meta(ctx)?;
            let resolved_filter = match viewport_id.as_deref() {
                Some(v) => ctx
                    .db
                    .resolve_viewport(v)
                    .ok()
                    .flatten()
                    .map(|r| r.id)
                    .or(viewport_id),
                None => None,
            };
            let snap = ctx.radar.snapshot(resolved_filter.as_deref()).await;
            Ok(RpcPayload::RadarSnapshot(snap))
        }
        RpcMethod::McpPreflight {
            viewport_id,
            symbols,
        } => {
            let _ = require_repo_meta(ctx)?;
            viewport_mgr::preflight_symbols(ctx, Some(&viewport_id), &symbols, &[])
                .map(RpcPayload::McpPreflight)
        }
        RpcMethod::McpPreflightEx {
            viewport_id,
            symbols,
            files,
        } => {
            let _ = require_repo_meta(ctx)?;
            viewport_mgr::preflight_symbols(ctx, viewport_id.as_deref(), &symbols, &files)
                .map(RpcPayload::McpPreflight)
        }
        RpcMethod::McpRecordDeadend {
            viewport_id,
            touched_symbols,
            kind,
            lesson_summary,
        } => viewport_mgr::record_deadend(
            ctx,
            &viewport_id,
            touched_symbols,
            &kind,
            lesson_summary,
            false,
        )
        .await
        .map(RpcPayload::McpRecordDeadend),
        RpcMethod::McpRecordDeadendEx {
            viewport_id,
            touched_symbols,
            kind,
            lesson_summary,
            restore_clean,
        } => viewport_mgr::record_deadend(
            ctx,
            &viewport_id,
            touched_symbols,
            &kind,
            lesson_summary,
            restore_clean,
        )
        .await
        .map(RpcPayload::McpRecordDeadend),
        RpcMethod::DropViewport {
            viewport_id,
            reason,
            kind,
        } => capsule::drop_viewport(ctx, &viewport_id, reason, kind)
            .await
            .map(RpcPayload::Drop),
        RpcMethod::GitPush { remote, branch } => git_sync::git_push(ctx, remote, branch)
            .await
            .map(RpcPayload::GitPush),
        RpcMethod::GitPull { remote, branch } => git_sync::git_pull(ctx, remote, branch)
            .await
            .map(RpcPayload::GitPull),
        RpcMethod::RemotePush {
            remote,
            branch,
            token,
            force,
        } => crate::remote_sync::remote_push(ctx, remote, branch, token, force)
            .await
            .map(RpcPayload::RemotePush),
        RpcMethod::RemotePull {
            remote,
            branch,
            token,
        } => crate::remote_sync::remote_pull(ctx, remote, branch, token)
            .await
            .map(RpcPayload::RemotePull),
        #[allow(unreachable_patterns)]
        _ => Err(RpcError::new(
            RpcErrorCode::NotImplemented,
            "unknown RPC method",
        )),
    }
}

async fn handle_init(ctx: &Arc<DaemonContext>, colocate_git: bool) -> Result<RpcPayload, RpcError> {
    let mut seeded_head_oid = None;
    let mut branch = None;

    if colocate_git {
        let info = inspect_git_repo(&ctx.repo_root).map_err(|e| {
            RpcError::new(
                RpcErrorCode::InvalidState,
                format!("--colocate-git failed: {e}"),
            )
        })?;
        ensure_git_info_exclude(&info.git_dir).map_err(|e| {
            RpcError::new(
                RpcErrorCode::IoError,
                format!("failed to update .git/info/exclude: {e}"),
            )
        })?;
        seeded_head_oid = info.head_oid;
        branch = info.branch;
    }

    // Ensure skeleton dirs exist
    for sub in ["wal", "cas", "viewports"] {
        std::fs::create_dir_all(ctx.loom_dir.join(sub))
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
    }
    let cfg_path = ctx.loom_dir.join("config.toml");
    if !cfg_path.exists() {
        let mut cfg = ctx.config.clone();
        cfg.colocate_git = colocate_git;
        cfg.save(&cfg_path)
            .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
    } else if colocate_git
        && !ctx.config.colocate_git
        && let Ok(mut cfg) = loom_core::config::LoomConfig::load(&cfg_path)
    {
        cfg.colocate_git = true;
        let _ = cfg.save(&cfg_path);
    }

    let (tx, rx) = oneshot::channel();
    ctx.writer
        .send(WriterCommand::InitRepo {
            colocate_git,
            seeded_head_oid,
            branch,
            reply: tx,
        })
        .await
        .map_err(|_| RpcError::new(RpcErrorCode::Internal, "writer actor closed"))?;

    let mut init_res: InitResult = rx
        .await
        .map_err(|_| RpcError::new(RpcErrorCode::Internal, "writer actor dropped reply"))??;
    init_res.repo_root = ctx.repo_root.clone();
    Ok(RpcPayload::Init(init_res))
}

fn require_repo_meta(ctx: &Arc<DaemonContext>) -> Result<loom_core::model::RepoMeta, RpcError> {
    match ctx.db.get_repo_meta() {
        Ok(Some(meta)) => Ok(meta),
        Ok(None) => Err(RpcError::new(
            RpcErrorCode::NotInitialized,
            "Loom repository is not initialized; run `loom init` first",
        )),
        Err(e) => Err(RpcError::new(RpcErrorCode::StorageError, e.to_string())),
    }
}

fn handle_status(ctx: &Arc<DaemonContext>) -> Result<RpcPayload, RpcError> {
    let meta = require_repo_meta(ctx)?;
    let mut viewports = ctx
        .db
        .list_viewports(true)
        .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
    viewports.sort_by(|a, b| {
        a.created_at_ns
            .cmp(&b.created_at_ns)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.id.cmp(&b.id))
    });
    let active_viewports = viewports.iter().filter(|v| v.state.is_active()).count();
    let total_viewports = viewports.len();
    let non_gc_summaries = viewports
        .into_iter()
        .filter(|v| v.state != ViewportState::GcPending)
        .map(viewport_mgr::to_summary)
        .collect();
    let default_driver = Some(
        loom_vfs::select_driver_with_config(
            ctx.config.vfs.driver,
            &ctx.repo_root,
            &ctx.config.vfs.cache_dirs,
        )
        .backend_kind(),
    );
    let cas = CasStore::new(ctx.loom_dir.join("cas"), ctx.config.zstd_level)
        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;
    let cas_stats = cas
        .stats()
        .map_err(|e| RpcError::new(RpcErrorCode::IoError, e.to_string()))?;

    let status = DaemonStatus {
        pid: std::process::id(),
        transport: ctx.descriptor.transport.clone(),
        endpoint: ctx.descriptor.endpoint.clone(),
        protocol_version: PROTOCOL_VERSION,
        uptime_ms: ctx.started_at.elapsed().as_millis() as u64,
        idle_timeout_ms: ctx.idle_monitor.idle_timeout().as_millis() as u64,
        idle_remaining_ms: ctx.idle_monitor.idle_remaining_ms(),
        active_viewports,
        total_viewports,
        default_driver,
        viewports: non_gc_summaries,
        latest_op_seq: meta.next_op_seq.saturating_sub(1),
        last_checkpoint_lsn: meta.last_checkpoint_lsn,
        last_applied_lsn: meta.last_applied_lsn,
        cas_object_count: cas_stats.object_count,
        cas_total_bytes: cas_stats.compressed_bytes,
        colocate_git: meta.colocate_git,
    };
    Ok(RpcPayload::Status(status))
}

fn handle_list_viewports(
    ctx: &Arc<DaemonContext>,
    include_gc: bool,
) -> Result<RpcPayload, RpcError> {
    let _ = require_repo_meta(ctx)?;
    let mut vps = ctx
        .db
        .list_viewports(include_gc)
        .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
    vps.sort_by(|a, b| {
        a.created_at_ns
            .cmp(&b.created_at_ns)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.id.cmp(&b.id))
    });
    let summaries = vps.into_iter().map(viewport_mgr::to_summary).collect();
    Ok(RpcPayload::Viewports(summaries))
}

fn handle_log(
    ctx: &Arc<DaemonContext>,
    capsules_only: bool,
    symbol: Option<&str>,
    limit: Option<usize>,
) -> Result<RpcPayload, RpcError> {
    let _ = require_repo_meta(ctx)?;
    let entries = ctx
        .db
        .list_ops(capsules_only, symbol, limit)
        .map_err(|e| RpcError::new(RpcErrorCode::StorageError, e.to_string()))?;
    let mut views = Vec::with_capacity(entries.len());
    for e in entries {
        let capsule_summary = if let Some(cap_id) = e.capsule_id.as_deref().or_else(|| {
            (e.kind == OpKind::CapsuleSeal)
                .then_some(e.target_id.as_deref())
                .flatten()
        }) {
            ctx.db
                .get_capsule(cap_id)
                .ok()
                .flatten()
                .map(|c| c.summary)
        } else {
            None
        };
        views.push(OpLogView {
            op_seq: e.op_seq,
            lsn: e.lsn,
            kind: e.kind,
            viewport_id: e.viewport_id,
            target_id: e.target_id,
            summary: e.summary,
            undone_by_op: e.undone_by_op,
            timestamp_ns: e.timestamp_ns,
            capsule_summary,
            touched_symbols: e.touched_symbols,
        });
    }
    Ok(RpcPayload::Log(views))
}

async fn handle_undo(ctx: &Arc<DaemonContext>, op_seq: Option<u64>) -> Result<RpcPayload, RpcError> {
    let _ = require_repo_meta(ctx)?;
    let res = capsule::undo_op(ctx, op_seq).await?;
    Ok(RpcPayload::Undo(res))
}

async fn handle_gc(
    ctx: &Arc<DaemonContext>,
    prune_viewports: bool,
) -> Result<RpcPayload, RpcError> {
    let _ = require_repo_meta(ctx)?;
    let _seal_guard = ctx.trunk_seal_lock.lock().await;
    // Unwatch inactive viewport directories BEFORE teardown_viewport runs on Windows.
    ctx.radar.unregister_inactive_viewports().await;
    // Flush & pause active Radar CAS writes until RunGc mark-and-sweep completes.
    let _radar_quiesce = ctx.radar.quiesce_for_gc().await;
    let (tx, rx) = oneshot::channel();
    ctx.writer
        .send(WriterCommand::RunGc {
            prune_viewports,
            cas_dir: ctx.loom_dir.join("cas"),
            viewports_dir: ctx.loom_dir.join("viewports"),
            cache_dirs: ctx.config.vfs.cache_dirs.clone(),
            zstd_level: ctx.config.zstd_level,
            reply: tx,
        })
        .await
        .map_err(|_| RpcError::new(RpcErrorCode::Internal, "writer actor closed"))?;
    let res = rx
        .await
        .map_err(|_| RpcError::new(RpcErrorCode::Internal, "writer actor dropped reply"))??;
    Ok(RpcPayload::Gc(res))
}
