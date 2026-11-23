use bytes::Bytes;
use http::{Request, Response, StatusCode, header};
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use interprocess::local_socket::tokio::Stream;
use loom_core::model::NegativeEdge;
use loom_core::rpc::{
    RpcMethod, RpcPayload, RpcRequest, RpcResponse, read_frame, write_frame,
};
#[cfg(windows)]
use loom_core::rpc::{pipe_short_name, read_sock_descriptor};
#[cfg(unix)]
use loom_core::rpc::sock_descriptor_path;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, CustomNotification, InitializeRequestParams, InitializeResult,
    ProtocolVersion, ServerCapabilities, ServerConfig, ServerNotification,
};
use rmcp::service::{
    NotificationContext, Peer, RequestContext, ServiceError, serve_server_with_ct,
};
use rmcp::transport::streamable_http_server::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use std::borrow::Cow;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct McpServerConfig {
    pub repo_root: PathBuf,
    pub default_viewport: Option<String>,
}

#[derive(Clone)]
pub struct McpRuntime {
    config: McpServerConfig,
    peers: Arc<Mutex<Vec<Peer<RoleServer>>>>,
}

impl McpRuntime {
    pub fn new(config: McpServerConfig) -> Self {
        Self {
            config,
            peers: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub async fn register_peer(&self, peer: Peer<RoleServer>) {
        let mut guard = self.peers.lock().await;
        guard.push(peer);
    }

    pub fn spawn_radar_notifier(&self, cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
        let repo_root = self.config.repo_root.clone();
        let viewport_filter = self.config.default_viewport.clone();
        let peers = Arc::clone(&self.peers);

        tokio::spawn(async move {
            loop {
                if cancel.is_cancelled() || !repo_root.join(".loom").is_dir() {
                    cancel.cancel();
                    break;
                }
                let stream = tokio::select! {
                    _ = cancel.cancelled() => break,
                    res = connect_ipc_with_retry(&repo_root, 5) => {
                        match res {
                            Ok(s) => s,
                            Err(_) => {
                                tokio::select! {
                                    _ = cancel.cancelled() => break,
                                    _ = tokio::time::sleep(Duration::from_millis(250)) => continue,
                                }
                            }
                        }
                    }
                };

                let mut stream = stream;
                let sub_req = RpcRequest {
                    id: 1,
                    method: RpcMethod::RadarSubscribe {
                        viewport_id: viewport_filter.clone(),
                    },
                };
                if write_frame(&mut stream, &sub_req).await.is_err() {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    continue;
                }

                // Read initial snapshot response
                match read_frame::<_, RpcResponse>(&mut stream).await {
                    Ok(Some(RpcResponse {
                        result: Ok(RpcPayload::RadarSnapshot(snap)),
                        ..
                    })) => {
                        for vp in &snap.viewports {
                            if vp.level != loom_core::model::RadarLevel::Green {
                                let report = loom_core::model::RadarStatusReport {
                                    viewport_id: vp.viewport_id.clone(),
                                    viewport_name: vp.viewport_name.clone(),
                                    status: vp.level,
                                    pending_syntax: vp.pending_syntax,
                                    conflicts: vp.conflicts.clone(),
                                    pending_syntax_files: vp.pending_syntax_files.clone(),
                                    negative_warnings: vp.negative_warnings.clone(),
                                    updated_at_ms: vp.updated_at_ms,
                                };
                                broadcast_radar_notification(&peers, &report).await;
                            }
                        }
                    }
                    _ => {
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        continue;
                    }
                }

                // Stream RadarEvent updates
                loop {
                    let frame_res = tokio::select! {
                        _ = cancel.cancelled() => return,
                        r = read_frame::<_, RpcResponse>(&mut stream) => r,
                    };
                    match frame_res {
                        Ok(Some(RpcResponse {
                            result: Ok(RpcPayload::RadarEvent(evt)),
                            ..
                        })) => {
                            for report in &evt.escalated_notifications {
                                if report.status != loom_core::model::RadarLevel::Green {
                                    broadcast_radar_notification(&peers, report).await;
                                }
                            }
                        }
                        Ok(Some(_)) => {}
                        Ok(None) | Err(_) => break,
                    }
                }
            }
        })
    }
}

async fn broadcast_radar_notification(
    peers: &Arc<Mutex<Vec<Peer<RoleServer>>>>,
    report: &loom_core::model::RadarStatusReport,
) {
    let Ok(val) = serde_json::to_value(report) else {
        return;
    };
    let active_peers: Vec<Peer<RoleServer>> = {
        let guard = peers.lock().await;
        guard.clone()
    };
    if active_peers.is_empty() {
        return;
    }

    let mut closed_indices = Vec::new();
    for (idx, peer) in active_peers.iter().enumerate() {
        let notif = ServerNotification::CustomNotification(CustomNotification::new(
            "notifications/loom/radar_conflict",
            Some(val.clone()),
        ));
        if let Err(err) = peer.send_notification(notif).await
            && matches!(err, ServiceError::TransportClosed)
        {
            closed_indices.push(idx);
        }
    }

    if !closed_indices.is_empty() {
        let mut guard = peers.lock().await;
        for idx in closed_indices.into_iter().rev() {
            if idx < guard.len() {
                guard.remove(idx);
            }
        }
    }
}

// ============================================================================
// IPC Bridge to loomd
// ============================================================================

pub async fn connect_ipc(repo_root: &Path) -> anyhow::Result<Stream> {
    #[cfg(windows)]
    {
        use interprocess::local_socket::{GenericNamespaced, ToNsName, tokio::prelude::*};
        let pipe_short = match read_sock_descriptor(repo_root) {
            Ok(desc) => desc
                .endpoint
                .strip_prefix(r"\\.\pipe\")
                .unwrap_or(&desc.endpoint)
                .to_string(),
            Err(_) => pipe_short_name(repo_root)?,
        };
        let ns_name = pipe_short.as_str().to_ns_name::<GenericNamespaced>()?;
        Ok(Stream::connect(ns_name).await?)
    }
    #[cfg(unix)]
    {
        use interprocess::local_socket::{GenericFilePath, ToFsName, tokio::prelude::*};
        let sock_path = sock_descriptor_path(repo_root);
        let fs_name = sock_path.as_path().to_fs_name::<GenericFilePath>()?;
        Ok(Stream::connect(fs_name).await?)
    }
}

async fn connect_ipc_with_retry(repo_root: &Path, attempts: usize) -> anyhow::Result<Stream> {
    let mut last_err = None;
    for i in 0..attempts {
        match connect_ipc(repo_root).await {
            Ok(s) => return Ok(s),
            Err(e) => {
                last_err = Some(e);
                if i + 1 < attempts {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("failed to connect to loomd IPC")))
}

async fn call_daemon_rpc(repo_root: &Path, method: RpcMethod) -> Result<RpcPayload, String> {
    let mut stream = connect_ipc_with_retry(repo_root, 6)
        .await
        .map_err(|e| format!("failed to connect to loomd: {e}"))?;
    write_frame(&mut stream, &RpcRequest { id: 1, method })
        .await
        .map_err(|e| format!("failed to write RPC frame: {e}"))?;
    let resp: RpcResponse = read_frame(&mut stream)
        .await
        .map_err(|e| format!("failed to read RPC frame: {e}"))?
        .ok_or_else(|| "daemon closed IPC connection before responding".to_string())?;
    resp.result
        .map_err(|e| format!("[{:?}] {}", e.code, e.message))
}

// ============================================================================
// MCP Tool Schemas & ServerHandler
// ============================================================================

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PreflightSymbolsParams {
    /// Optional viewport name or ID (defaults to server --viewport if set)
    #[serde(default)]
    pub viewport: Option<String>,
    /// Symbol names or FQNs to inspect across active viewports and negative ledger
    #[serde(default)]
    pub symbols: Vec<String>,
    /// Optional relative file paths to filter provides/consumes and dead-end warnings
    #[serde(default)]
    pub files: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RadarPollParams {
    /// Optional viewport name or ID to filter the radar snapshot
    #[serde(default)]
    pub viewport: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecordDeadendParams {
    /// Viewport name or ID (defaults to server --viewport if set)
    #[serde(default)]
    pub viewport: Option<String>,
    /// Symbols touched by the failed approach
    #[serde(default)]
    pub touched_symbols: Vec<String>,
    /// Failure category (CompileError, TestFailure, SemanticConflict, StructuralConflict, PerformanceRegression, DesignDeadEnd, Deadlock, Other)
    #[serde(default)]
    pub failure_kind: Option<String>,
    /// Concise lesson summary (max 500 characters)
    pub lesson_summary: String,
    /// Whether to restore the viewport back to clean trunk after recording the dead-end (defaults to true)
    #[serde(default)]
    pub restore_clean: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SealCapsuleParams {
    /// Viewport name or ID (defaults to server --viewport if set)
    #[serde(default)]
    pub viewport: Option<String>,
    /// Optional shell verification command executed inside the viewport before sealing
    #[serde(default)]
    pub verify_cmd: Option<String>,
    /// Optional capsule summary message
    #[serde(default)]
    pub summary: Option<String>,
}

#[derive(Clone)]
pub struct LoomMcpServer {
    runtime: Arc<McpRuntime>,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl LoomMcpServer {
    pub fn new(runtime: Arc<McpRuntime>) -> Self {
        Self {
            runtime,
            tool_router: Self::tool_router(),
        }
    }

    fn resolve_viewport_arg(&self, explicit: Option<String>) -> Option<String> {
        explicit
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| self.runtime.config.default_viewport.clone())
    }

    #[tool(
        name = "loom_preflight_symbols",
        description = "Inspect active peer symbol provides/consumes, current merge radar status, and NegativeEdge failure warnings before editing."
    )]
    async fn loom_preflight_symbols(
        &self,
        Parameters(params): Parameters<PreflightSymbolsParams>,
    ) -> Result<CallToolResult, McpError> {
        let viewport_id = self.resolve_viewport_arg(params.viewport);
        let files: Vec<PathBuf> = params
            .files
            .unwrap_or_default()
            .into_iter()
            .map(PathBuf::from)
            .collect();
        match call_daemon_rpc(
            &self.runtime.config.repo_root,
            RpcMethod::McpPreflightEx {
                viewport_id,
                symbols: params.symbols,
                files,
            },
        )
        .await
        {
            Ok(RpcPayload::McpPreflight(res)) => Ok(structured_ok(&res)?),
            Ok(other) => Err(McpError::internal_error(
                format!("unexpected daemon RPC response: {other:?}"),
                None,
            )),
            Err(msg) => Ok(CallToolResult::error(vec![ContentBlock::text(msg)])),
        }
    }

    #[tool(
        name = "loom_radar_poll",
        description = "Poll the real-time Speculative Merge Radar snapshot (GREEN/YELLOW/RED status, pairwise N×N conflict matrix, PENDING_SYNTAX files, and negative warnings)."
    )]
    async fn loom_radar_poll(
        &self,
        Parameters(params): Parameters<RadarPollParams>,
    ) -> Result<CallToolResult, McpError> {
        let viewport_id = self.resolve_viewport_arg(params.viewport);
        match call_daemon_rpc(
            &self.runtime.config.repo_root,
            RpcMethod::RadarSnapshot { viewport_id },
        )
        .await
        {
            Ok(RpcPayload::RadarSnapshot(snap)) => Ok(structured_ok(&snap)?),
            Ok(other) => Err(McpError::internal_error(
                format!("unexpected daemon RPC response: {other:?}"),
                None,
            )),
            Err(msg) => Ok(CallToolResult::error(vec![ContentBlock::text(msg)])),
        }
    }

    #[tool(
        name = "loom_record_deadend",
        description = "Record a NegativeEdge failure lesson (<= 500 chars) into the shared negative_index and restore the viewport to clean trunk by default (set restore_clean=false to keep edits) so future agents avoid repeating the same dead end."
    )]
    async fn loom_record_deadend(
        &self,
        Parameters(params): Parameters<RecordDeadendParams>,
    ) -> Result<CallToolResult, McpError> {
        let trimmed = params.lesson_summary.trim();
        if trimmed.is_empty() {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "lesson_summary cannot be empty",
            )]));
        }
        if let Err(err) = NegativeEdge::validate_lesson(&params.lesson_summary) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(err.to_string())]));
        }
        let Some(viewport_id) = self.resolve_viewport_arg(params.viewport) else {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "viewport is required (pass `viewport` or start server with `--viewport`)",
            )]));
        };
        let kind = match params
            .failure_kind
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(raw) => {
                if let Err(msg) = raw.parse::<loom_core::model::FailureKind>() {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(msg)]));
                }
                raw.to_string()
            }
            None => "Other".to_string(),
        };

        let edge = match call_daemon_rpc(
            &self.runtime.config.repo_root,
            RpcMethod::McpRecordDeadendEx {
                viewport_id,
                touched_symbols: params.touched_symbols,
                kind,
                lesson_summary: params.lesson_summary,
                restore_clean: params.restore_clean.unwrap_or(true),
            },
        )
        .await
        {
            Ok(RpcPayload::McpRecordDeadend(e)) => e,
            Ok(other) => {
                return Err(McpError::internal_error(
                    format!("unexpected daemon RPC response: {other:?}"),
                    None,
                ));
            }
            Err(msg) => return Ok(CallToolResult::error(vec![ContentBlock::text(msg)])),
        };

        structured_ok(&edge)
    }

    #[tool(
        name = "loom_seal_capsule",
        description = "Optionally run a verification command inside the viewport, diff/merge CST changes to trunk, and seal a CausalCapsule."
    )]
    async fn loom_seal_capsule(
        &self,
        Parameters(params): Parameters<SealCapsuleParams>,
    ) -> Result<CallToolResult, McpError> {
        let Some(viewport_id) = self.resolve_viewport_arg(params.viewport) else {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "viewport is required (pass `viewport` or start server with `--viewport`)",
            )]));
        };
        match call_daemon_rpc(
            &self.runtime.config.repo_root,
            RpcMethod::SealCapsule {
                viewport_id,
                verify_cmd: params.verify_cmd,
                summary: params.summary,
            },
        )
        .await
        {
            Ok(RpcPayload::CapsuleSeal(cap)) => Ok(structured_ok(&cap)?),
            Ok(other) => Err(McpError::internal_error(
                format!("unexpected daemon RPC response: {other:?}"),
                None,
            )),
            Err(msg) => Ok(CallToolResult::error(vec![ContentBlock::text(msg)])),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for LoomMcpServer {
    fn get_info(&self) -> ServerConfig {
        let mut cfg = ServerConfig::default();
        cfg.protocol_version = ProtocolVersion::LATEST_WITH_INITIALIZE;
        cfg.capabilities = ServerCapabilities::builder().enable_tools().build();
        cfg.server_info.name = "loom-mcp".to_string();
        cfg.server_info.version = env!("CARGO_PKG_VERSION").to_string();
        cfg
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(ProtocolVersion::known_up_to(
            &ProtocolVersion::LATEST_WITH_INITIALIZE,
        ))
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, McpError> {
        context.peer.set_peer_info(request.clone());
        self.runtime.register_peer(context.peer.clone()).await;
        self.negotiate_initialize(&request)
    }

    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        if let Ok(RpcPayload::RadarSnapshot(snap)) = call_daemon_rpc(
            &self.runtime.config.repo_root,
            RpcMethod::RadarSnapshot {
                viewport_id: self.runtime.config.default_viewport.clone(),
            },
        )
        .await
        {
            for vp in &snap.viewports {
                if vp.level != loom_core::model::RadarLevel::Green {
                    let report = loom_core::model::RadarStatusReport {
                        viewport_id: vp.viewport_id.clone(),
                        viewport_name: vp.viewport_name.clone(),
                        status: vp.level,
                        pending_syntax: vp.pending_syntax,
                        conflicts: vp.conflicts.clone(),
                        pending_syntax_files: vp.pending_syntax_files.clone(),
                        negative_warnings: vp.negative_warnings.clone(),
                        updated_at_ms: vp.updated_at_ms,
                    };
                    if let Ok(val) = serde_json::to_value(&report) {
                        let notif = ServerNotification::CustomNotification(
                            CustomNotification::new(
                                "notifications/loom/radar_conflict",
                                Some(val),
                            ),
                        );
                        let _ = context.peer.send_notification(notif).await;
                    }
                }
            }
        }
    }
}

fn structured_ok<T: serde::Serialize>(val: &T) -> Result<CallToolResult, McpError> {
    let json_val = serde_json::to_value(val).map_err(|e| {
        McpError::internal_error(format!("failed to serialize tool result: {e}"), None)
    })?;
    Ok(CallToolResult::structured(json_val))
}

// ============================================================================
// Stdio & Streamable HTTP Transports (with pre-init `server/discover` -> -32601)
// ============================================================================

fn discover_method_not_found_json(id: &serde_json::Value) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -32601,
            "message": "Method not found: server/discover"
        }
    })
    .to_string()
}

fn rpc_id_key(id: &serde_json::Value) -> Option<String> {
    if id.is_string() || id.is_number() {
        Some(id.to_string())
    } else {
        None
    }
}

struct InFlightRequests {
    pending: std::sync::Mutex<std::collections::HashMap<String, usize>>,
    count_tx: tokio::sync::watch::Sender<usize>,
}

impl InFlightRequests {
    fn new() -> (Arc<Self>, tokio::sync::watch::Receiver<usize>) {
        let (count_tx, count_rx) = tokio::sync::watch::channel(0usize);
        let tracker = Arc::new(Self {
            pending: std::sync::Mutex::new(std::collections::HashMap::new()),
            count_tx,
        });
        (tracker, count_rx)
    }

    fn track(&self, id_key: String) {
        let mut guard = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        *guard.entry(id_key).or_insert(0) += 1;
        let total: usize = guard.values().sum();
        self.count_tx.send_replace(total);
    }

    fn untrack(&self, id_key: &str) {
        let mut guard = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(count) = guard.get_mut(id_key) {
            if *count <= 1 {
                guard.remove(id_key);
            } else {
                *count -= 1;
            }
            let total: usize = guard.values().sum();
            self.count_tx.send_replace(total);
        }
    }
}

async fn write_mcp_line<W: AsyncWrite + Unpin>(writer: &mut W, line: &str) -> bool {
    if writer.write_all(line.as_bytes()).await.is_err() {
        return false;
    }
    if !line.ends_with('\n') && writer.write_all(b"\n").await.is_err() {
        return false;
    }
    writer.flush().await.is_ok()
}

pub async fn serve_mcp_stdio(config: McpServerConfig) -> anyhow::Result<()> {
    let (duplex_read, mut duplex_write) = tokio::io::duplex(256 * 1024);
    let (line_tx, mut line_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        use std::io::BufRead;
        let stdin = std::io::stdin();
        let mut lock = stdin.lock();
        loop {
            let mut buf = Vec::new();
            match lock.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if line_tx.send(buf).is_err() {
                        break;
                    }
                }
            }
        }
    });
    tokio::spawn(async move {
        while let Some(chunk) = line_rx.recv().await {
            if duplex_write.write_all(&chunk).await.is_err() {
                break;
            }
            let _ = duplex_write.flush().await;
        }
        let _ = duplex_write.shutdown().await;
    });
    let stdout = tokio::io::stdout();
    serve_mcp_stdio_with_streams(config, duplex_read, stdout).await
}

pub async fn serve_mcp_stdio_with_streams<R, W>(
    config: McpServerConfig,
    reader: R,
    mut writer: W,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let repo_loom_dir = config.repo_root.join(".loom");
    let cancel = CancellationToken::new();
    let runtime = Arc::new(McpRuntime::new(config));
    let notifier_task = runtime.spawn_radar_notifier(cancel.clone());
    let server = LoomMcpServer::new(Arc::clone(&runtime));

    let cancel_watchdog = cancel.clone();
    let watchdog_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel_watchdog.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_millis(200)) => {
                    if !repo_loom_dir.is_dir() {
                        cancel_watchdog.cancel();
                        break;
                    }
                }
            }
        }
    });

    let (rmcp_read, mut to_rmcp_write) = tokio::io::duplex(256 * 1024);
    let (from_rmcp_read, rmcp_write) = tokio::io::duplex(256 * 1024);
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let (in_flight, mut in_flight_rx) = InFlightRequests::new();
    let rmcp_cancel = cancel.child_token();

    let cancel_writer = cancel.clone();
    let mut writer_task = tokio::spawn(async move {
        loop {
            let line = tokio::select! {
                biased;
                l = out_rx.recv() => match l {
                    Some(l) => l,
                    None => break,
                },
                _ = cancel_writer.cancelled() => break,
            };
            if !write_mcp_line(&mut writer, &line).await {
                break;
            }
        }
        while let Ok(line) = out_rx.try_recv() {
            if !write_mcp_line(&mut writer, &line).await {
                break;
            }
        }
        let _ = writer.flush().await;
    });

    let out_tx_rmcp = out_tx.clone();
    let cancel_rmcp_out = cancel.clone();
    let in_flight_out = Arc::clone(&in_flight);
    let mut rmcp_out_task = tokio::spawn(async move {
        let mut lines = BufReader::new(from_rmcp_read).lines();
        loop {
            let line = tokio::select! {
                biased;
                r = lines.next_line() => match r {
                    Ok(Some(l)) => l,
                    _ => break,
                },
                _ = cancel_rmcp_out.cancelled() => break,
            };
            let resp_id_key = serde_json::from_str::<serde_json::Value>(&line)
                .ok()
                .filter(|v| v.get("result").is_some() || v.get("error").is_some())
                .and_then(|v| v.get("id").and_then(rpc_id_key));
            if out_tx_rmcp.send(line).is_err() {
                break;
            }
            if let Some(id_key) = resp_id_key {
                in_flight_out.untrack(&id_key);
            }
        }
    });

    let out_tx_discover = out_tx.clone();
    let cancel_stdin = cancel.clone();
    let rmcp_cancel_stdin = rmcp_cancel.clone();
    let in_flight_in = Arc::clone(&in_flight);
    drop(out_tx);
    let mut stdin_task = tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        let mut eof_reached = false;
        loop {
            let raw_line = tokio::select! {
                biased;
                r = lines.next_line() => match r {
                    Ok(Some(l)) => l,
                    Ok(None) => {
                        eof_reached = true;
                        break;
                    }
                    Err(_) => break,
                },
                _ = cancel_stdin.cancelled() => break,
                _ = rmcp_cancel_stdin.cancelled() => break,
            };
            let line = raw_line.trim_end_matches('\r');
            if line.trim().is_empty() {
                continue;
            }
            let parsed_val = serde_json::from_str::<serde_json::Value>(line).ok();
            if let Some(ref val) = parsed_val
                && val.get("method").and_then(|m| m.as_str()) == Some("server/discover")
            {
                if let Some(id) = val.get("id") {
                    let _ = out_tx_discover.send(discover_method_not_found_json(id));
                }
                continue;
            }
            let req_id_key = parsed_val
                .as_ref()
                .filter(|v| v.get("method").and_then(|m| m.as_str()).is_some())
                .and_then(|v| v.get("id").and_then(rpc_id_key));
            if let Some(ref k) = req_id_key {
                in_flight_in.track(k.clone());
            }
            if to_rmcp_write.write_all(line.as_bytes()).await.is_err()
                || to_rmcp_write.write_all(b"\n").await.is_err()
                || to_rmcp_write.flush().await.is_err()
            {
                if let Some(ref k) = req_id_key {
                    in_flight_in.untrack(k);
                }
                break;
            }
        }
        if eof_reached && !cancel_stdin.is_cancelled() && !rmcp_cancel_stdin.is_cancelled() {
            let grace = tokio::time::sleep(Duration::from_secs(2));
            tokio::pin!(grace);
            let wait_zero = in_flight_rx.wait_for(|&count| count == 0);
            tokio::select! {
                biased;
                _ = cancel_stdin.cancelled() => {},
                _ = rmcp_cancel_stdin.cancelled() => {},
                _ = wait_zero => {},
                _ = &mut grace => {
                    tracing::debug!("MCP stdio EOF grace timeout expired with pending request IDs");
                }
            }
        }
        let _ = to_rmcp_write.shutdown().await;
    });

    let running =
        serve_server_with_ct(server, (rmcp_read, rmcp_write), rmcp_cancel.clone()).await;
    match running {
        Ok(svc) => {
            let _ = svc.waiting().await;
        }
        Err(err) => {
            tracing::debug!(error = %err, "MCP stdio server exited before handshake");
            rmcp_cancel.cancel();
        }
    }

    if !cancel.is_cancelled() {
        // Normal completion (e.g., stdin EOF): let stdin_task finish dropping
        // `to_rmcp_write`/`out_tx_discover`, and drain `rmcp_out_task` + `writer_task`
        // before cancelling background notifier/watchdog tasks.
        if tokio::time::timeout(Duration::from_millis(500), &mut stdin_task)
            .await
            .is_err()
        {
            stdin_task.abort();
            let _ = stdin_task.await;
        }
        if tokio::time::timeout(Duration::from_secs(2), &mut rmcp_out_task)
            .await
            .is_err()
        {
            cancel.cancel();
            let _ = rmcp_out_task.await;
        }
        if tokio::time::timeout(Duration::from_secs(2), &mut writer_task)
            .await
            .is_err()
        {
            cancel.cancel();
            let _ = writer_task.await;
        }
        cancel.cancel();
    } else {
        cancel.cancel();
        stdin_task.abort();
        let _ = stdin_task.await;
        let _ = rmcp_out_task.await;
        let _ = writer_task.await;
    }
    let _ = notifier_task.await;
    let _ = watchdog_task.await;

    Ok(())
}

pub async fn start_mcp_http_server(
    addr: &str,
    config: McpServerConfig,
    cancel: CancellationToken,
) -> anyhow::Result<(SocketAddr, tokio::task::JoinHandle<anyhow::Result<()>>)> {
    let listener = TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;

    let runtime = Arc::new(McpRuntime::new(config));
    let notifier_task = runtime.spawn_radar_notifier(cancel.clone());

    let mut http_cfg = rmcp::transport::streamable_http_server::StreamableHttpServerConfig::default();
    http_cfg.cancellation_token = cancel.clone();

    let runtime_for_factory = Arc::clone(&runtime);
    let mcp_service = StreamableHttpService::new(
        move || Ok(LoomMcpServer::new(Arc::clone(&runtime_for_factory))),
        Arc::new(LocalSessionManager::default()),
        http_cfg,
    );

    let handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                accept_res = listener.accept() => {
                    let (tcp_stream, _) = match accept_res {
                        Ok(pair) => pair,
                        Err(e) => {
                            tracing::warn!(error = %e, "MCP HTTP accept error");
                            continue;
                        }
                    };
                    let svc = mcp_service.clone();
                    let conn_cancel = cancel.clone();
                    tokio::spawn(async move {
                        let io = TokioIo::new(tcp_stream);
                        let hyper_svc = service_fn(move |req: Request<Incoming>| {
                            let svc = svc.clone();
                            async move { handle_http_request(svc, req).await }
                        });
                        let conn = http1::Builder::new().serve_connection(io, hyper_svc);
                        tokio::pin!(conn);
                        tokio::select! {
                            _ = conn_cancel.cancelled() => {}
                            res = &mut conn => {
                                if let Err(err) = res {
                                    tracing::debug!(error = %err, "MCP HTTP connection closed");
                                }
                            }
                        }
                    });
                }
            }
        }
        notifier_task.abort();
        Ok(())
    });

    Ok((local_addr, handle))
}

async fn handle_http_request(
    service: StreamableHttpService<LoomMcpServer, LocalSessionManager>,
    req: Request<Incoming>,
) -> Result<Response<BoxBody<Bytes, Infallible>>, Infallible> {
    if req.method() == http::Method::POST {
        let (parts, body) = req.into_parts();
        const MAX_BODY: usize = 4 * 1024 * 1024;
        let collected = match http_body_util::Limited::new(body, MAX_BODY).collect().await {
            Ok(c) => c.to_bytes(),
            Err(_) => {
                let resp = Response::builder()
                    .status(StatusCode::PAYLOAD_TOO_LARGE)
                    .body(
                        Full::new(Bytes::from_static(b"request body too large"))
                            .map_err(|never| match never {})
                            .boxed(),
                    )
                    .unwrap();
                return Ok(resp);
            }
        };

        if let Ok(val) = serde_json::from_slice::<serde_json::Value>(&collected)
            && val.get("method").and_then(|m| m.as_str()) == Some("server/discover")
        {
            let body_bytes = if let Some(id) = val.get("id") {
                Bytes::from(discover_method_not_found_json(id))
            } else {
                Bytes::from_static(b"")
            };
            let resp = Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(
                    Full::new(body_bytes)
                        .map_err(|never| match never {})
                        .boxed(),
                )
                .unwrap();
            return Ok(resp);
        }

        let reconstructed = Request::from_parts(parts, Full::new(collected));
        return Ok(service.handle(reconstructed).await);
    }

    Ok(service.handle(req).await)
}

pub async fn serve_mcp_http(addr: &str, config: McpServerConfig) -> anyhow::Result<()> {
    let cancel = CancellationToken::new();
    let (bound_addr, handle) = start_mcp_http_server(addr, config, cancel.clone()).await?;
    tracing::info!(addr = %bound_addr, "loom MCP Streamable HTTP server listening");

    let cancel_sig = cancel.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        cancel_sig.cancel();
    });

    handle.await??;
    Ok(())
}
