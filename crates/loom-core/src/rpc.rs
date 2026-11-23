use crate::model::{
    CausalCapsule, CowBackendKind, NegativeEdge, OpKind, RadarConflictItem, RadarLevel,
    RadarStatusReport, SymbolConsumeEntry, SymbolProvideEntry, ViewportMount, ViewportState,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::io::Write;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SocketDescriptor {
    pub transport: String,
    pub endpoint: String,
    pub pid: u32,
    pub protocol_version: u32,
}

pub fn canonical_repo_root(path: &Path) -> anyhow::Result<PathBuf> {
    if path.exists() {
        Ok(std::fs::canonicalize(path)?)
    } else if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

pub fn repo_endpoint_hash16(repo_root: &Path) -> anyhow::Result<String> {
    let canon = canonical_repo_root(repo_root)?;
    let canon_str = canon.to_string_lossy();
    let normalized = if cfg!(windows) {
        canon_str.to_lowercase()
    } else {
        canon_str.into_owned()
    };
    let hash = blake3::hash(normalized.as_bytes());
    let hex = hash.to_hex();
    Ok(hex.as_str()[..16].to_string())
}

pub fn pipe_short_name(repo_root: &Path) -> anyhow::Result<String> {
    Ok(format!("loom-{}", repo_endpoint_hash16(repo_root)?))
}

pub fn pipe_full_path(repo_root: &Path) -> anyhow::Result<String> {
    Ok(format!(r"\\.\pipe\{}", pipe_short_name(repo_root)?))
}

pub fn sock_descriptor_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".loom").join("daemon.sock")
}

pub fn lock_file_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".loom").join("daemon.lock")
}

pub fn write_sock_descriptor_atomic(
    repo_root: &Path,
    desc: &SocketDescriptor,
) -> anyhow::Result<()> {
    let sock_path = sock_descriptor_path(repo_root);
    let loom_dir = sock_path
        .parent()
        .expect("sock_descriptor_path always has .loom parent");
    std::fs::create_dir_all(loom_dir)?;

    let tmp_path = loom_dir.join(format!("daemon.sock.tmp.{}", std::process::id()));
    let json = serde_json::to_vec_pretty(desc)?;
    {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp_path)?;
        f.write_all(&json)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp_path, &sock_path)?;
    Ok(())
}

pub fn read_sock_descriptor(repo_root: &Path) -> anyhow::Result<SocketDescriptor> {
    let sock_path = sock_descriptor_path(repo_root);
    let bytes = std::fs::read(&sock_path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

// ============================================================================
// RPC Protocol Types
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    pub id: u64,
    pub method: RpcMethod,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum RpcMethod {
    // R1
    Ping,
    Init {
        colocate_git: bool,
    },
    Status,
    ListViewports {
        include_gc: bool,
    },
    Log {
        capsules_only: bool,
        symbol: Option<String>,
        limit: Option<usize>,
    },
    Undo {
        op_seq: Option<u64>,
    },
    Gc {
        prune_viewports: bool,
    },
    Shutdown,

    // R2-R5 Extensibility Hooks
    SpawnViewport {
        name: String,
        intent: String,
        cmd: Vec<String>,
    },
    TransitionViewport {
        viewport_id: String,
        target_state: ViewportState,
        #[serde(default)]
        pid: Option<u32>,
    },
    RollbackFailedSpawn {
        viewport_id: String,
    },
    RestoreViewportClean {
        viewport_id: String,
    },
    SealCapsule {
        viewport_id: String,
        verify_cmd: Option<String>,
        summary: Option<String>,
    },
    DropViewport {
        viewport_id: String,
        reason: String,
        kind: Option<String>,
    },
    RadarSnapshot {
        viewport_id: Option<String>,
    },
    RadarSubscribe {
        viewport_id: Option<String>,
    },
    McpPreflight {
        viewport_id: String,
        symbols: Vec<String>,
    },
    McpPreflightEx {
        viewport_id: Option<String>,
        symbols: Vec<String>,
        #[serde(default)]
        files: Vec<PathBuf>,
    },
    McpRecordDeadend {
        viewport_id: String,
        touched_symbols: Vec<String>,
        kind: String,
        lesson_summary: String,
    },
    McpRecordDeadendEx {
        viewport_id: String,
        touched_symbols: Vec<String>,
        kind: String,
        lesson_summary: String,
        #[serde(default)]
        restore_clean: bool,
    },
    GitPush {
        remote: Option<String>,
        branch: Option<String>,
    },
    GitPull {
        remote: Option<String>,
        branch: Option<String>,
    },
    RemotePush {
        remote: Option<String>,
        branch: Option<String>,
        #[serde(default)]
        token: Option<String>,
        #[serde(default)]
        force: bool,
    },
    RemotePull {
        remote: Option<String>,
        branch: Option<String>,
        #[serde(default)]
        token: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    pub id: u64,
    pub result: Result<RpcPayload, RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RpcError {
    pub code: RpcErrorCode,
    pub message: String,
    pub details: Option<String>,
}

impl RpcError {
    pub fn new(code: RpcErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum RpcErrorCode {
    InvalidRequest,
    NotInitialized,
    AlreadyLocked,
    NotFound,
    InvalidState,
    Conflict,
    IoError,
    StorageError,
    NotImplemented,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub enum RpcPayload {
    Pong { protocol_version: u32, pid: u32 },
    Init(InitResult),
    Status(DaemonStatus),
    Viewport(ViewportSummary),
    Viewports(Vec<ViewportSummary>),
    Log(Vec<OpLogView>),
    Undo(UndoResult),
    Gc(GcResult),
    ShutdownAck,
    RadarSnapshot(RadarSnapshot),
    RadarEvent(RadarStreamEvent),
    McpPreflight(McpPreflightResult),
    McpRecordDeadend(NegativeEdge),
    CapsuleSeal(CausalCapsule),
    Drop(DropResult),
    GitPush(GitPushResult),
    GitPull(GitPullResult),
    RemotePush(NativeRemotePushResult),
    RemotePull(NativeRemotePullResult),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct InitResult {
    pub repo_root: PathBuf,
    pub repo_id: String,
    pub colocate_git: bool,
    pub already_initialized: bool,
    pub seeded_head_oid: Option<String>,
    pub branch: Option<String>,
}

impl Default for InitResult {
    fn default() -> Self {
        Self {
            repo_root: PathBuf::new(),
            repo_id: String::new(),
            colocate_git: false,
            already_initialized: false,
            seeded_head_oid: None,
            branch: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct DaemonStatus {
    pub pid: u32,
    pub transport: String,
    pub endpoint: String,
    pub protocol_version: u32,
    pub uptime_ms: u64,
    pub idle_timeout_ms: u64,
    pub idle_remaining_ms: Option<u64>,
    pub active_viewports: usize,
    pub total_viewports: usize,
    pub default_driver: Option<CowBackendKind>,
    pub viewports: Vec<ViewportSummary>,
    pub latest_op_seq: u64,
    pub last_checkpoint_lsn: u64,
    pub last_applied_lsn: u64,
    pub cas_object_count: u64,
    pub cas_total_bytes: u64,
    pub colocate_git: bool,
}

impl Default for DaemonStatus {
    fn default() -> Self {
        Self {
            pid: 0,
            transport: String::new(),
            endpoint: String::new(),
            protocol_version: PROTOCOL_VERSION,
            uptime_ms: 0,
            idle_timeout_ms: 0,
            idle_remaining_ms: None,
            active_viewports: 0,
            total_viewports: 0,
            default_driver: None,
            viewports: Vec::new(),
            latest_op_seq: 0,
            last_checkpoint_lsn: 0,
            last_applied_lsn: 0,
            cas_object_count: 0,
            cas_total_bytes: 0,
            colocate_git: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ViewportSummary {
    pub id: String,
    pub name: String,
    pub intent: String,
    pub agent_id: String,
    pub state: ViewportState,
    pub path: PathBuf,
    pub driver: CowBackendKind,
    pub pid: Option<u32>,
    pub mount: Option<ViewportMount>,
    pub base_op_seq: u64,
    pub head_lsn: u64,
    pub created_at_ns: u64,
    pub updated_at_ns: u64,
}

impl Default for ViewportSummary {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            intent: String::new(),
            agent_id: String::new(),
            state: ViewportState::Spawned,
            path: PathBuf::new(),
            driver: CowBackendKind::GenericCopyFallback,
            pid: None,
            mount: None,
            base_op_seq: 0,
            head_lsn: 0,
            created_at_ns: 0,
            updated_at_ns: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct OpLogView {
    pub op_seq: u64,
    pub lsn: u64,
    pub kind: OpKind,
    pub viewport_id: Option<String>,
    pub target_id: Option<String>,
    pub summary: String,
    pub undone_by_op: Option<u64>,
    pub timestamp_ns: u64,
    pub capsule_summary: Option<String>,
    pub touched_symbols: Vec<String>,
}

impl Default for OpLogView {
    fn default() -> Self {
        Self {
            op_seq: 0,
            lsn: 0,
            kind: OpKind::RepoInit,
            viewport_id: None,
            target_id: None,
            summary: String::new(),
            undone_by_op: None,
            timestamp_ns: 0,
            capsule_summary: None,
            touched_symbols: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UndoResult {
    pub undone_op_seq: u64,
    pub compensating_op_seq: u64,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct GcResult {
    pub transitioned_viewports: usize,
    pub pruned_viewport_dirs: usize,
    pub pruned_viewport_ids: Vec<String>,
    pub pruned_viewport_names: Vec<String>,
    pub removed_cas_blobs: usize,
    pub reclaimed_cas_bytes: u64,
    pub compacted_wal_segments: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct RadarBatchMetrics {
    pub batch_mode: bool,
    pub effective_debounce_ms: u64,
    pub batch_switches: u64,
    pub coalesced_events: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct RadarViewportSummary {
    pub viewport_id: String,
    pub viewport_name: String,
    pub intent: String,
    pub state: ViewportState,
    pub level: RadarLevel,
    pub pending_syntax: bool,
    pub pending_syntax_files: Vec<PathBuf>,
    pub conflicts: Vec<RadarConflictItem>,
    pub negative_warnings: Vec<NegativeEdge>,
    pub dirty_files: Vec<PathBuf>,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct RadarMatrixCell {
    pub viewport_a_id: String,
    pub viewport_a_name: String,
    pub viewport_b_id: String,
    pub viewport_b_name: String,
    pub level: RadarLevel,
    pub conflicts: Vec<RadarConflictItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct RadarSnapshot {
    pub sequence: u64,
    pub repo_root: PathBuf,
    pub batch_mode: bool,
    pub debounce_ms: u64,
    pub metrics: RadarBatchMetrics,
    pub viewports: Vec<RadarViewportSummary>,
    pub matrix: Vec<RadarMatrixCell>,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct RadarStreamEvent {
    pub snapshot: RadarSnapshot,
    pub changed_viewports: Vec<String>,
    pub escalated_notifications: Vec<RadarStatusReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct McpPreflightResult {
    pub viewport_id: Option<String>,
    pub provides: Vec<SymbolProvideEntry>,
    pub consumes: Vec<SymbolConsumeEntry>,
    pub radar: RadarSnapshot,
    pub negative_warnings: Vec<NegativeEdge>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct DropResult {
    pub viewport: ViewportSummary,
    pub negative_edge: NegativeEdge,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct GitPushResult {
    pub remote: String,
    pub remote_path: PathBuf,
    pub branch: String,
    pub commits_created: usize,
    pub pushed_commit_oids: Vec<String>,
    pub pushed_capsule_ids: Vec<String>,
    pub synced_negative_edges: usize,
    pub head_commit_oid: Option<String>,
    pub notes_ref: String,
    pub notes_commit_oid: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ViewportRebaseOutcome {
    #[default]
    Clean,
    Conflict,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct ViewportRebaseStatus {
    pub viewport_id: String,
    pub viewport_name: String,
    pub outcome: ViewportRebaseOutcome,
    pub updated_files: usize,
    pub conflicts: Vec<RadarConflictItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct GitPullResult {
    pub remote: String,
    pub remote_path: PathBuf,
    pub branch: String,
    pub head_commit_oid: Option<String>,
    pub pulled_commit_oids: Vec<String>,
    pub restored_capsule_ids: Vec<String>,
    pub synthesized_capsule_ids: Vec<String>,
    pub restored_negative_edges: usize,
    pub updated_trunk: bool,
    pub rebased_viewports: Vec<ViewportRebaseStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct NativeRemotePushResult {
    pub remote: String,
    pub remote_url: String,
    pub branch: String,
    pub head_capsule_id: Option<String>,
    pub head_snapshot_cas: Option<String>,
    pub pushed_capsule_ids: Vec<String>,
    pub uploaded_cas_blobs: usize,
    pub pushed_patches: usize,
    pub pushed_negative_edges: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct NativeRemotePullResult {
    pub remote: String,
    pub remote_url: String,
    pub branch: String,
    pub head_capsule_id: Option<String>,
    pub head_snapshot_cas: Option<String>,
    pub pulled_capsule_ids: Vec<String>,
    pub downloaded_cas_blobs: usize,
    pub pulled_patches: usize,
    pub restored_negative_edges: usize,
    pub updated_trunk: bool,
    pub rebased_viewports: Vec<ViewportRebaseStatus>,
}

// ============================================================================
// Length-Prefixed Wire Codec ([u32_le][rmp_serde::to_vec_named])
// ============================================================================

pub fn encode_frame<T: Serialize>(msg: &T) -> anyhow::Result<Vec<u8>> {
    let payload = rmp_serde::to_vec_named(msg)?;
    anyhow::ensure!(
        !payload.is_empty() && payload.len() <= MAX_FRAME_BYTES,
        "RPC frame size {} exceeds 64 MiB limit",
        payload.len()
    );
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg: &impl Serialize,
) -> anyhow::Result<()> {
    let bytes = encode_frame(msg)?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn read_frame<R: AsyncRead + Unpin, T: DeserializeOwned>(
    reader: &mut R,
) -> anyhow::Result<Option<T>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf[..1]).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    reader.read_exact(&mut len_buf[1..]).await?;
    let len = u32::from_le_bytes(len_buf) as usize;
    anyhow::ensure!(
        len > 0 && len <= MAX_FRAME_BYTES,
        "invalid RPC frame length: {len}"
    );
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    Ok(Some(rmp_serde::from_slice(&buf)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn rpc_frame_roundtrip_all_r1_variants() {
        let requests = vec![
            RpcRequest {
                id: 1,
                method: RpcMethod::Ping,
            },
            RpcRequest {
                id: 2,
                method: RpcMethod::Init { colocate_git: true },
            },
            RpcRequest {
                id: 3,
                method: RpcMethod::Status,
            },
            RpcRequest {
                id: 4,
                method: RpcMethod::ListViewports { include_gc: true },
            },
            RpcRequest {
                id: 5,
                method: RpcMethod::Log {
                    capsules_only: true,
                    symbol: Some("crate::foo".into()),
                    limit: Some(10),
                },
            },
            RpcRequest {
                id: 6,
                method: RpcMethod::Undo { op_seq: Some(2) },
            },
            RpcRequest {
                id: 7,
                method: RpcMethod::Gc {
                    prune_viewports: true,
                },
            },
            RpcRequest {
                id: 8,
                method: RpcMethod::Shutdown,
            },
            RpcRequest {
                id: 9,
                method: RpcMethod::SpawnViewport {
                    name: "agent-a".into(),
                    intent: "test".into(),
                    cmd: vec!["echo".into()],
                },
            },
            RpcRequest {
                id: 10,
                method: RpcMethod::TransitionViewport {
                    viewport_id: "vp-1".into(),
                    target_state: ViewportState::Verifying,
                    pid: Some(1234),
                },
            },
            RpcRequest {
                id: 11,
                method: RpcMethod::RestoreViewportClean {
                    viewport_id: "vp-1".into(),
                },
            },
        ];

        let mut buf = Vec::new();
        for req in &requests {
            write_frame(&mut buf, req).await.unwrap();
        }

        let mut cursor = std::io::Cursor::new(buf);
        for expected in &requests {
            let decoded: RpcRequest = read_frame(&mut cursor).await.unwrap().unwrap();
            assert_eq!(decoded.id, expected.id);
        }
        assert!(read_frame::<_, RpcRequest>(&mut cursor).await.unwrap().is_none());

        let responses = vec![
            RpcResponse {
                id: 1,
                result: Ok(RpcPayload::Pong {
                    protocol_version: 1,
                    pid: 1234,
                }),
            },
            RpcResponse {
                id: 2,
                result: Ok(RpcPayload::Init(InitResult::default())),
            },
            RpcResponse {
                id: 3,
                result: Ok(RpcPayload::Status(DaemonStatus::default())),
            },
            RpcResponse {
                id: 4,
                result: Ok(RpcPayload::Viewports(vec![])),
            },
            RpcResponse {
                id: 5,
                result: Ok(RpcPayload::Log(vec![])),
            },
            RpcResponse {
                id: 6,
                result: Ok(RpcPayload::Undo(UndoResult {
                    undone_op_seq: 1,
                    compensating_op_seq: 2,
                    summary: "undone".into(),
                })),
            },
            RpcResponse {
                id: 7,
                result: Ok(RpcPayload::Gc(GcResult::default())),
            },
            RpcResponse {
                id: 8,
                result: Ok(RpcPayload::ShutdownAck),
            },
            RpcResponse {
                id: 9,
                result: Ok(RpcPayload::Viewport(ViewportSummary {
                    id: "vp-1".into(),
                    name: "agent-a".into(),
                    intent: "test".into(),
                    agent_id: "cli".into(),
                    state: ViewportState::Active,
                    path: PathBuf::from(".loom/viewports/agent-a"),
                    driver: CowBackendKind::WindowsNtfsFallback,
                    pid: Some(1234),
                    mount: Some(ViewportMount::default()),
                    base_op_seq: 2,
                    head_lsn: 3,
                    created_at_ns: 10,
                    updated_at_ns: 20,
                })),
            },
        ];

        let mut resp_buf = Vec::new();
        for resp in &responses {
            write_frame(&mut resp_buf, resp).await.unwrap();
        }
        let mut rcursor = std::io::Cursor::new(resp_buf);
        for expected in &responses {
            let decoded: RpcResponse = read_frame(&mut rcursor).await.unwrap().unwrap();
            assert_eq!(decoded.id, expected.id);
        }
    }

    #[tokio::test]
    async fn rpc_frame_64mib_guard() {
        let big = vec![0u8; MAX_FRAME_BYTES + 1];
        assert!(encode_frame(&big).is_err());

        // Forged header > 64 MiB must fail before allocating
        let forged_len = (MAX_FRAME_BYTES as u32) + 1;
        let mut forged = Vec::from(forged_len.to_le_bytes());
        forged.extend_from_slice(&[1, 2, 3]);
        let mut cursor = std::io::Cursor::new(forged);
        let err = read_frame::<_, RpcRequest>(&mut cursor).await.unwrap_err();
        assert!(err.to_string().contains("invalid RPC frame length"));
    }

    #[tokio::test]
    async fn rpc_frame_eof_and_truncated() {
        let mut empty = std::io::Cursor::new(Vec::<u8>::new());
        assert!(read_frame::<_, RpcRequest>(&mut empty).await.unwrap().is_none());

        let mut trunc_header = std::io::Cursor::new(vec![4u8, 0]);
        assert!(read_frame::<_, RpcRequest>(&mut trunc_header).await.is_err());

        let mut trunc_body = std::io::Cursor::new(vec![10u8, 0, 0, 0, 1, 2]);
        assert!(read_frame::<_, RpcRequest>(&mut trunc_body).await.is_err());
    }

    #[test]
    fn windows_pipe_name_and_descriptor_roundtrip() {
        let dir = TempDir::new().unwrap();
        let short = pipe_short_name(dir.path()).unwrap();
        assert!(short.starts_with("loom-"));
        assert!(!short.contains(r"\\.\pipe\"));
        assert_eq!(short.len(), 5 + 16);

        let full = pipe_full_path(dir.path()).unwrap();
        assert_eq!(full, format!(r"\\.\pipe\{short}"));

        let desc = SocketDescriptor {
            transport: "named_pipe".into(),
            endpoint: full,
            pid: 999,
            protocol_version: PROTOCOL_VERSION,
        };
        write_sock_descriptor_atomic(dir.path(), &desc).unwrap();
        let loaded = read_sock_descriptor(dir.path()).unwrap();
        assert_eq!(desc, loaded);
    }
}
