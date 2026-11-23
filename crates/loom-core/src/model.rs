use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

pub const MAX_NEGATIVE_EDGE_LESSON_CHARS: usize = 500;

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("invalid blake3 hex hash (expected 64 hex chars): {0}")]
    InvalidHashHex(String),
    #[error("negative edge lesson summary exceeds {MAX_NEGATIVE_EDGE_LESSON_CHARS} characters (got {0})")]
    LessonTooLong(usize),
}

pub fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Blake3Hash(pub [u8; 32]);

impl Blake3Hash {
    pub const ZERO: Self = Self([0u8; 32]);

    pub fn from_bytes(data: &[u8]) -> Self {
        let digest = blake3::hash(data);
        Self(*digest.as_bytes())
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(64);
        for byte in &self.0 {
            use std::fmt::Write;
            let _ = write!(&mut out, "{byte:02x}");
        }
        out
    }

    pub fn short_hex(&self, len: usize) -> String {
        let hex = self.to_hex();
        hex.chars().take(len).collect()
    }

    pub fn from_hex(hex: &str) -> Result<Self, ModelError> {
        if hex.len() != 64 || !hex.is_ascii() {
            return Err(ModelError::InvalidHashHex(hex.to_string()));
        }
        let mut bytes = [0u8; 32];
        for (i, chunk) in hex.as_bytes().chunks_exact(2).enumerate() {
            let s = std::str::from_utf8(chunk)
                .map_err(|_| ModelError::InvalidHashHex(hex.to_string()))?;
            bytes[i] = u8::from_str_radix(s, 16)
                .map_err(|_| ModelError::InvalidHashHex(hex.to_string()))?;
        }
        Ok(Self(bytes))
    }
}

impl fmt::Debug for Blake3Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Blake3Hash({})", self.short_hex(16))
    }
}

impl fmt::Display for Blake3Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl FromStr for Blake3Hash {
    type Err = ModelError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_hex(s)
    }
}

impl Serialize for Blake3Hash {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Blake3Hash {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

// ============================================================================
// 1. VFS & Viewport (R1 / R2)
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CowBackendKind {
    ApfsClonefile,
    LinuxReflink,
    LinuxFuseOverlayFs,
    WindowsRefsBlockClone,
    WindowsProjFs,
    WindowsNtfsFallback,
    GenericCopyFallback,
}

impl fmt::Display for CowBackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::ApfsClonefile => "apfs_clonefile",
            Self::LinuxReflink => "linux_reflink",
            Self::LinuxFuseOverlayFs => "linux_fuse_overlayfs",
            Self::WindowsRefsBlockClone => "windows_refs_block_clone",
            Self::WindowsProjFs => "windows_projfs",
            Self::WindowsNtfsFallback => "windows_ntfs_fallback",
            Self::GenericCopyFallback => "generic_copy_fallback",
        };
        f.write_str(label)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ViewportState {
    #[default]
    Spawned,
    Active,
    Verifying,
    Sealed,
    Dropped,
    GcPending,
}

impl ViewportState {
    pub fn is_active(self) -> bool {
        matches!(self, Self::Spawned | Self::Active | Self::Verifying)
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        match self {
            Self::Spawned => matches!(next, Self::Active | Self::Dropped),
            Self::Active => matches!(next, Self::Verifying | Self::Sealed | Self::Dropped),
            Self::Verifying => matches!(next, Self::Active | Self::Sealed | Self::Dropped),
            Self::Sealed => matches!(next, Self::GcPending),
            Self::Dropped => matches!(next, Self::GcPending),
            Self::GcPending => false,
        }
    }
}

impl fmt::Display for ViewportState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::Spawned => "Spawned",
            Self::Active => "Active",
            Self::Verifying => "Verifying",
            Self::Sealed => "Sealed",
            Self::Dropped => "Dropped",
            Self::GcPending => "GC_Pending",
        };
        f.write_str(label)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewportMount {
    pub backend: CowBackendKind,
    pub viewport_root: PathBuf,
    pub mount_point: Option<PathBuf>,
    pub upper_dir: Option<PathBuf>,
    pub work_dir: Option<PathBuf>,
    pub hardlinked_files: u64,
    pub copied_files: u64,
    pub shared_cache_dirs: Vec<String>,
}

impl Default for ViewportMount {
    fn default() -> Self {
        Self {
            backend: CowBackendKind::GenericCopyFallback,
            viewport_root: PathBuf::new(),
            mount_point: None,
            upper_dir: None,
            work_dir: None,
            hardlinked_files: 0,
            copied_files: 0,
            shared_cache_dirs: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewportRecord {
    pub id: String,
    pub name: String,
    pub intent: String,
    pub agent_id: String,
    pub state: ViewportState,
    pub path: PathBuf,
    pub driver: CowBackendKind,
    pub mount: Option<ViewportMount>,
    pub pid: Option<u32>,
    pub base_op_seq: u64,
    pub base_capsule_id: Option<String>,
    pub base_snapshot_cas: Option<Blake3Hash>,
    pub head_cas: Option<Blake3Hash>,
    pub head_lsn: u64,
    pub lamport_ts: u64,
    pub patch_ids: Vec<String>,
    pub created_at_ns: u64,
    pub updated_at_ns: u64,
    #[serde(default)]
    pub pruned_at_ns: Option<u64>,
}

impl Default for ViewportRecord {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            intent: String::new(),
            agent_id: String::new(),
            state: ViewportState::Spawned,
            path: PathBuf::new(),
            driver: CowBackendKind::GenericCopyFallback,
            mount: None,
            pid: None,
            base_op_seq: 0,
            base_capsule_id: None,
            base_snapshot_cas: None,
            head_cas: None,
            head_lsn: 0,
            lamport_ts: 0,
            patch_ids: Vec::new(),
            created_at_ns: 0,
            updated_at_ns: 0,
            pruned_at_ns: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct WorkspaceSnapshot {
    pub files: std::collections::BTreeMap<String, Blake3Hash>,
}

// ============================================================================
// 2. CST, Patches, Contracts, Merge & Binary CAS (R3)
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum LanguageKind {
    Rust,
    TypeScript,
    Tsx,
    Go,
    Python,
    Json,
    Yaml,
    Markdown,
    Binary,
    PlainText,
}

impl LanguageKind {
    pub fn from_path(path: &Path) -> Self {
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            return Self::PlainText;
        };
        match ext.to_ascii_lowercase().as_str() {
            "rs" => Self::Rust,
            "ts" | "mts" | "cts" => Self::TypeScript,
            "tsx" => Self::Tsx,
            "go" => Self::Go,
            "py" | "pyi" => Self::Python,
            "json" => Self::Json,
            "yaml" | "yml" => Self::Yaml,
            "md" | "markdown" => Self::Markdown,
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "pdf" | "wasm"
            | "exe" | "dll" | "so" | "dylib" | "bin" | "zip" | "tar" | "gz" | "zst"
            | "xz" | "7z" | "ttf" | "otf" | "woff" | "woff2" | "o" | "a" | "lib"
            | "rlib" | "pdb" | "class" | "jar" | "pyc" | "pyo" | "sqlite" | "db"
            | "redb" => Self::Binary,
            _ => Self::PlainText,
        }
    }

    pub fn detect(path: &Path, sample: &[u8]) -> Self {
        let by_ext = Self::from_path(path);
        if by_ext == Self::Binary {
            return Self::Binary;
        }
        if sample.contains(&0) {
            return Self::Binary;
        }
        if let Err(err) = std::str::from_utf8(sample) {
            // If there is an invalid UTF-8 sequence (not just a truncated multi-byte char at buffer end)
            // or if the sample is the complete file (< 8192 bytes), treat as binary.
            if err.error_len().is_some() || sample.len() < 8192 {
                return Self::Binary;
            }
        }
        by_ext
    }

    pub fn is_code(self) -> bool {
        matches!(
            self,
            Self::Rust | Self::TypeScript | Self::Tsx | Self::Go | Self::Python
        )
    }

    pub fn is_crdt_text(self) -> bool {
        matches!(
            self,
            Self::Json | Self::Yaml | Self::Markdown | Self::PlainText
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct NodeAnchor {
    pub logical_file_id: Blake3Hash,
    pub symbol_fqn: String,
    pub syntax_kind_id: u16,
    pub decl_header_hash: Blake3Hash,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SemanticOp {
    InsertMember {
        parent_anchor: NodeAnchor,
        member_anchor: NodeAnchor,
        full_text_with_trivia: String,
        #[serde(default)]
        after_anchor: Option<NodeAnchor>,
    },
    ReplaceBody {
        target: NodeAnchor,
        old_body_hash: Blake3Hash,
        new_body_text: String,
    },
    MutateSignature {
        target: NodeAnchor,
        old_header_hash: Blake3Hash,
        new_header_text: String,
        new_header_hash: Blake3Hash,
    },
    RemoveNode {
        target: NodeAnchor,
        removed_hash: Blake3Hash,
    },
    RenameOrMoveFile {
        logical_file_id: Blake3Hash,
        old_path: PathBuf,
        new_path: PathBuf,
    },
    TextCrdtFallback {
        logical_file_id: Blake3Hash,
        file_path: PathBuf,
        automerge_changes: Vec<u8>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "delta", rename_all = "snake_case")]
pub enum SymbolContractDelta {
    AddedSymbol {
        symbol_fqn: String,
        signature_hash: Blake3Hash,
        signature_text: String,
    },
    RemovedSymbol {
        symbol_fqn: String,
        previous_hash: Blake3Hash,
    },
    SignatureChanged {
        symbol_fqn: String,
        old_signature_hash: Blake3Hash,
        new_signature_hash: Blake3Hash,
        old_signature: String,
        new_signature: String,
    },
    VisibilityReduced {
        symbol_fqn: String,
        detail: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BinaryManifest {
    pub total_len: u64,
    pub chunk_size: u32,
    pub file_hash: Blake3Hash,
    pub chunk_hashes: Vec<Blake3Hash>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AstPatch {
    pub patch_hash: Blake3Hash,
    pub viewport_id: String,
    pub actor_id: String,
    pub lamport_ts: u64,
    pub parents: Vec<Blake3Hash>,
    pub logical_file_id: Blake3Hash,
    pub file_path: PathBuf,
    pub ops: Vec<SemanticOp>,
    pub contract_deltas: Vec<SymbolContractDelta>,
    pub cas_before: Option<Blake3Hash>,
    pub cas_after: Option<Blake3Hash>,
    pub binary_manifest: Option<BinaryManifest>,
    pub created_at_ns: u64,
}

impl AstPatch {
    pub fn compute_content_hash(&self) -> Blake3Hash {
        let mut canonical = self.clone();
        canonical.patch_hash = Blake3Hash::ZERO;
        canonical.created_at_ns = 0;
        let encoded = rmp_serde::to_vec_named(&canonical)
            .expect("AstPatch serialization for content hashing must succeed");
        Blake3Hash::from_bytes(&encoded)
    }

    pub fn finalize_hash(&mut self) {
        self.patch_hash = self.compute_content_hash();
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AstPatchRecord {
    pub patch_id: String,
    pub lsn: u64,
    pub patch: AstPatch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum MergeOutcome {
    Clean {
        #[serde(default)]
        merged_files: Vec<PathBuf>,
        #[serde(default)]
        applied_patches: Vec<Blake3Hash>,
    },
    StructuralConflict {
        file: PathBuf,
        #[serde(default)]
        anchor: Option<NodeAnchor>,
        patch_a: Blake3Hash,
        patch_b: Blake3Hash,
        reason: String,
    },
    SemanticConflict {
        symbol_fqn: String,
        producer_patch: Blake3Hash,
        consumer_patch: Blake3Hash,
        detail: String,
    },
}

// ============================================================================
// 3. Radar & Symbol Tables (R4)
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SymbolStatus {
    #[default]
    Ready,
    PendingSyntax,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SymbolProvideEntry {
    pub symbol_fqn: String,
    pub viewport_id: String,
    pub logical_file_id: Blake3Hash,
    pub file_path: PathBuf,
    pub node_anchor: Option<NodeAnchor>,
    pub signature_hash: Blake3Hash,
    pub signature_text: String,
    pub status: SymbolStatus,
    pub updated_at_ns: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SymbolLocation {
    pub logical_file_id: Blake3Hash,
    pub file_path: PathBuf,
    pub line: u32,
    pub column: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SymbolConsumeEntry {
    pub symbol_fqn: String,
    pub viewport_id: String,
    pub locations: Vec<SymbolLocation>,
    pub expected_signature_hash: Option<Blake3Hash>,
    pub updated_at_ns: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "UPPERCASE")]
pub enum RadarLevel {
    #[default]
    Green,
    Yellow,
    Red,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RadarConflictItem {
    pub peer_viewport_id: String,
    pub peer_viewport_name: String,
    pub level: RadarLevel,
    pub kind: String,
    pub file_path: Option<PathBuf>,
    pub symbol_fqn: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RadarStatusReport {
    pub viewport_id: String,
    pub viewport_name: String,
    pub status: RadarLevel,
    pub pending_syntax: bool,
    pub conflicts: Vec<RadarConflictItem>,
    pub pending_syntax_files: Vec<PathBuf>,
    pub negative_warnings: Vec<NegativeEdge>,
    pub updated_at_ms: u64,
}

// ============================================================================
// 4. Causal Capsules, Negative Edges, OpLog, RepoMeta, GitMap (R1 / R5)
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct VerificationProof {
    pub verify_cmd: Option<String>,
    pub exit_code: i32,
    pub passed: bool,
    pub output_digest: Blake3Hash,
    pub proof_hash: Blake3Hash,
    pub duration_ms: u64,
    pub verified_at_ns: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CausalCapsule {
    pub capsule_id: String,
    pub viewport_id: String,
    pub agent_id: String,
    pub summary: String,
    pub patch_ids: Vec<String>,
    pub touched_files: Vec<PathBuf>,
    pub touched_symbols: Vec<String>,
    pub verification_proof: Option<VerificationProof>,
    pub parent_capsule_ids: Vec<String>,
    pub snapshot_cas_hash: Option<Blake3Hash>,
    pub proof_cas_hash: Option<Blake3Hash>,
    pub git_commit_oid: Option<String>,
    pub created_at_ns: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum FailureKind {
    CompileError,
    TestFailure,
    SemanticConflict,
    StructuralConflict,
    PerformanceRegression,
    DesignDeadEnd,
    #[default]
    Other,
    Deadlock,
}

impl fmt::Display for FailureKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::CompileError => "CompileError",
            Self::TestFailure => "TestFailure",
            Self::SemanticConflict => "SemanticConflict",
            Self::StructuralConflict => "StructuralConflict",
            Self::PerformanceRegression => "PerformanceRegression",
            Self::DesignDeadEnd => "DesignDeadEnd",
            Self::Other => "Other",
            Self::Deadlock => "Deadlock",
        };
        f.write_str(s)
    }
}

impl std::str::FromStr for FailureKind {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let normalized: String = raw
            .trim()
            .chars()
            .filter(|c| *c != '_' && *c != '-' && !c.is_whitespace())
            .map(|c| c.to_ascii_lowercase())
            .collect();
        match normalized.as_str() {
            "compileerror" | "compile" => Ok(Self::CompileError),
            "testfailure" | "test" => Ok(Self::TestFailure),
            "semanticconflict" | "semantic" => Ok(Self::SemanticConflict),
            "structuralconflict" | "structural" => Ok(Self::StructuralConflict),
            "performanceregression" | "performance" | "perf" => Ok(Self::PerformanceRegression),
            "designdeadend" | "deadend" => Ok(Self::DesignDeadEnd),
            "deadlock" => Ok(Self::Deadlock),
            "other" => Ok(Self::Other),
            _ => Err(format!(
                "invalid FailureKind `{raw}`; expected one of: CompileError, TestFailure, SemanticConflict, StructuralConflict, PerformanceRegression, DesignDeadEnd, Deadlock, Other"
            )),
        }
    }
}

impl FailureKind {
    pub fn parse_lenient(raw: &str) -> Self {
        raw.parse().unwrap_or(Self::Other)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct NegativeEdge {
    pub edge_id: String,
    pub edge_seq: u64,
    pub viewport_id: String,
    pub intent: String,
    pub touched_symbols: Vec<String>,
    pub failure_kind: FailureKind,
    pub lesson_summary: String,
    pub patch_hashes: Vec<Blake3Hash>,
    pub source: String,
    pub created_at_ns: u64,
}

impl NegativeEdge {
    pub fn validate_lesson(summary: &str) -> Result<(), ModelError> {
        let len = summary.chars().count();
        if len > MAX_NEGATIVE_EDGE_LESSON_CHARS {
            return Err(ModelError::LessonTooLong(len));
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), ModelError> {
        Self::validate_lesson(&self.lesson_summary)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum OpKind {
    #[default]
    RepoInit,
    ViewportSpawn,
    ViewportTransition,
    AstPatch,
    CapsuleSeal,
    ViewportDrop,
    NegativeEdgeRecord,
    GitPush,
    GitPull,
    RemotePush,
    RemotePull,
    Undo,
    Gc,
}

impl fmt::Display for OpKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::RepoInit => "RepoInit",
            Self::ViewportSpawn => "ViewportSpawn",
            Self::ViewportTransition => "ViewportTransition",
            Self::AstPatch => "AstPatch",
            Self::CapsuleSeal => "CapsuleSeal",
            Self::ViewportDrop => "ViewportDrop",
            Self::NegativeEdgeRecord => "NegativeEdgeRecord",
            Self::GitPush => "GitPush",
            Self::GitPull => "GitPull",
            Self::RemotePush => "RemotePush",
            Self::RemotePull => "RemotePull",
            Self::Undo => "Undo",
            Self::Gc => "Gc",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct OpLogEntry {
    pub op_seq: u64,
    pub lsn: u64,
    pub kind: OpKind,
    pub viewport_id: Option<String>,
    pub target_id: Option<String>,
    pub capsule_id: Option<String>,
    pub touched_symbols: Vec<String>,
    pub summary: String,
    pub inverse_payload: Option<Vec<u8>>,
    pub undone_by_op: Option<u64>,
    pub timestamp_ns: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RepoMeta {
    pub schema_version: u32,
    pub repo_id: String,
    pub colocate_git: bool,
    pub next_lsn: u64,
    pub next_op_seq: u64,
    pub next_edge_seq: u64,
    pub last_applied_lsn: u64,
    pub last_checkpoint_lsn: u64,
    pub lamport_clock: u64,
    pub head_capsule_id: Option<String>,
    pub created_at_ns: u64,
    pub updated_at_ns: u64,
}

impl Default for RepoMeta {
    fn default() -> Self {
        Self {
            schema_version: 1,
            repo_id: String::new(),
            colocate_git: false,
            next_lsn: 1,
            next_op_seq: 1,
            next_edge_seq: 1,
            last_applied_lsn: 0,
            last_checkpoint_lsn: 0,
            lamport_clock: 0,
            head_capsule_id: None,
            created_at_ns: 0,
            updated_at_ns: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum GitSyncDirection {
    #[default]
    InitSeed,
    Push,
    Pull,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct GitMapEntry {
    pub git_commit_oid: String,
    pub capsule_id: Option<String>,
    pub branch: String,
    pub direction: GitSyncDirection,
    pub notes_synced: bool,
    pub timestamp_ns: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip_msgpack_and_json<T>(val: &T) -> T
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + fmt::Debug,
    {
        let encoded = rmp_serde::to_vec_named(val).expect("msgpack encode");
        let decoded: T = rmp_serde::from_slice(&encoded).expect("msgpack decode");
        assert_eq!(val, &decoded);

        let json_str = serde_json::to_string(val).expect("json encode");
        let json_decoded: T = serde_json::from_str(&json_str).expect("json decode");
        assert_eq!(val, &json_decoded);

        decoded
    }

    #[test]
    fn blake3_hash_hex_roundtrip() {
        let h = Blake3Hash::from_bytes(b"project loom");
        let hex = h.to_hex();
        assert_eq!(hex.len(), 64);
        assert_eq!(h.short_hex(16).len(), 16);
        let parsed = Blake3Hash::from_hex(&hex).unwrap();
        assert_eq!(h, parsed);
        assert!(Blake3Hash::from_hex("bad").is_err());
        assert!(Blake3Hash::from_hex(&"z".repeat(64)).is_err());
    }

    #[test]
    fn viewport_state_transitions_and_active_flag() {
        assert!(ViewportState::Spawned.is_active());
        assert!(ViewportState::Active.is_active());
        assert!(ViewportState::Verifying.is_active());
        assert!(!ViewportState::Sealed.is_active());
        assert!(!ViewportState::Dropped.is_active());
        assert!(!ViewportState::GcPending.is_active());

        assert!(ViewportState::Spawned.can_transition_to(ViewportState::Active));
        assert!(ViewportState::Active.can_transition_to(ViewportState::Verifying));
        assert!(ViewportState::Verifying.can_transition_to(ViewportState::Sealed));
        assert!(ViewportState::Sealed.can_transition_to(ViewportState::GcPending));
        assert!(ViewportState::Dropped.can_transition_to(ViewportState::GcPending));
        assert!(!ViewportState::Sealed.can_transition_to(ViewportState::Active));
        assert!(!ViewportState::GcPending.can_transition_to(ViewportState::Active));
    }

    #[test]
    fn negative_edge_lesson_500_char_boundary() {
        let ok_500 = "a".repeat(500);
        assert!(NegativeEdge::validate_lesson(&ok_500).is_ok());

        let too_long = "a".repeat(501);
        assert!(matches!(
            NegativeEdge::validate_lesson(&too_long),
            Err(ModelError::LessonTooLong(501))
        ));

        let unicode_500 = "가".repeat(500);
        assert!(NegativeEdge::validate_lesson(&unicode_500).is_ok());
    }

    #[test]
    fn all_r1_to_r5_models_roundtrip_msgpack_and_json() {
        let h1 = Blake3Hash::from_bytes(b"one");
        let h2 = Blake3Hash::from_bytes(b"two");

        let vp = ViewportRecord {
            id: "vp-1".into(),
            name: "agent-a".into(),
            intent: "refactor storage".into(),
            agent_id: "agy".into(),
            state: ViewportState::Active,
            path: PathBuf::from(".loom/viewports/vp-1"),
            driver: CowBackendKind::WindowsNtfsFallback,
            mount: Some(ViewportMount {
                backend: CowBackendKind::WindowsNtfsFallback,
                viewport_root: PathBuf::from(".loom/viewports/vp-1"),
                mount_point: None,
                upper_dir: None,
                work_dir: None,
                hardlinked_files: 10,
                copied_files: 4,
                shared_cache_dirs: vec!["target".into()],
            }),
            pid: Some(4242),
            base_op_seq: 1,
            base_capsule_id: Some("cap-0".into()),
            base_snapshot_cas: Some(h1),
            head_cas: Some(h2),
            head_lsn: 3,
            lamport_ts: 4,
            patch_ids: vec!["p-1".into()],
            created_at_ns: 100,
            updated_at_ns: 200,
            pruned_at_ns: None,
        };
        roundtrip_msgpack_and_json(&vp);

        let anchor = NodeAnchor {
            logical_file_id: h1,
            symbol_fqn: "crate::foo::bar".into(),
            syntax_kind_id: 42,
            decl_header_hash: h2,
        };
        let patch = AstPatchRecord {
            patch_id: "p-1".into(),
            lsn: 5,
            patch: AstPatch {
                patch_hash: h1,
                viewport_id: "vp-1".into(),
                actor_id: "agy".into(),
                lamport_ts: 7,
                parents: vec![h2],
                logical_file_id: h1,
                file_path: PathBuf::from("src/lib.rs"),
                ops: vec![
                    SemanticOp::InsertMember {
                        parent_anchor: anchor.clone(),
                        member_anchor: anchor.clone(),
                        full_text_with_trivia: "/// doc\nfn x() {}".into(),
                        after_anchor: None,
                    },
                    SemanticOp::ReplaceBody {
                        target: anchor.clone(),
                        old_body_hash: h1,
                        new_body_text: "{ 1 }".into(),
                    },
                    SemanticOp::MutateSignature {
                        target: anchor.clone(),
                        old_header_hash: h1,
                        new_header_text: "pub fn x() -> i32".into(),
                        new_header_hash: h2,
                    },
                    SemanticOp::RemoveNode {
                        target: anchor.clone(),
                        removed_hash: h2,
                    },
                    SemanticOp::RenameOrMoveFile {
                        logical_file_id: h1,
                        old_path: PathBuf::from("a.rs"),
                        new_path: PathBuf::from("b.rs"),
                    },
                    SemanticOp::TextCrdtFallback {
                        logical_file_id: h2,
                        file_path: PathBuf::from("README.md"),
                        automerge_changes: vec![1, 2, 3],
                    },
                ],
                contract_deltas: vec![
                    SymbolContractDelta::AddedSymbol {
                        symbol_fqn: "crate::x".into(),
                        signature_hash: h1,
                        signature_text: "pub fn x()".into(),
                    },
                    SymbolContractDelta::RemovedSymbol {
                        symbol_fqn: "crate::y".into(),
                        previous_hash: h2,
                    },
                    SymbolContractDelta::SignatureChanged {
                        symbol_fqn: "crate::z".into(),
                        old_signature_hash: h1,
                        new_signature_hash: h2,
                        old_signature: "fn z()".into(),
                        new_signature: "fn z(a: i32)".into(),
                    },
                    SymbolContractDelta::VisibilityReduced {
                        symbol_fqn: "crate::w".into(),
                        detail: "pub -> pub(crate)".into(),
                    },
                ],
                cas_before: Some(h1),
                cas_after: Some(h2),
                binary_manifest: Some(BinaryManifest {
                    total_len: 128,
                    chunk_size: 64,
                    file_hash: h1,
                    chunk_hashes: vec![h1, h2],
                }),
                created_at_ns: 999,
            },
        };
        roundtrip_msgpack_and_json(&patch);

        roundtrip_msgpack_and_json(&MergeOutcome::Clean {
            merged_files: vec![PathBuf::from("src/lib.rs")],
            applied_patches: vec![h1],
        });
        roundtrip_msgpack_and_json(&MergeOutcome::StructuralConflict {
            file: PathBuf::from("src/lib.rs"),
            anchor: Some(anchor.clone()),
            patch_a: h1,
            patch_b: h2,
            reason: "overlap".into(),
        });
        roundtrip_msgpack_and_json(&MergeOutcome::SemanticConflict {
            symbol_fqn: "crate::foo".into(),
            producer_patch: h1,
            consumer_patch: h2,
            detail: "arity mismatch".into(),
        });

        let provide = SymbolProvideEntry {
            symbol_fqn: "crate::foo".into(),
            viewport_id: "vp-1".into(),
            logical_file_id: h1,
            file_path: PathBuf::from("src/lib.rs"),
            node_anchor: Some(anchor),
            signature_hash: h2,
            signature_text: "pub fn foo()".into(),
            status: SymbolStatus::Ready,
            updated_at_ns: 10,
        };
        roundtrip_msgpack_and_json(&provide);

        let consume = SymbolConsumeEntry {
            symbol_fqn: "crate::foo".into(),
            viewport_id: "vp-2".into(),
            locations: vec![SymbolLocation {
                logical_file_id: h2,
                file_path: PathBuf::from("src/main.rs"),
                line: 12,
                column: 5,
            }],
            expected_signature_hash: Some(h2),
            updated_at_ns: 11,
        };
        roundtrip_msgpack_and_json(&consume);

        let neg = NegativeEdge {
            edge_id: "ne-1".into(),
            edge_seq: 1,
            viewport_id: "vp-1".into(),
            intent: "async lock".into(),
            touched_symbols: vec!["crate::foo".into()],
            failure_kind: FailureKind::CompileError,
            lesson_summary: "don't hold std Mutex across await".into(),
            patch_hashes: vec![h1],
            source: "loom_drop".into(),
            created_at_ns: 55,
        };
        roundtrip_msgpack_and_json(&neg);

        let radar = RadarStatusReport {
            viewport_id: "vp-1".into(),
            viewport_name: "agent-a".into(),
            status: RadarLevel::Yellow,
            pending_syntax: false,
            conflicts: vec![RadarConflictItem {
                peer_viewport_id: "vp-2".into(),
                peer_viewport_name: "agent-b".into(),
                level: RadarLevel::Yellow,
                kind: "signature_drift".into(),
                file_path: Some(PathBuf::from("src/lib.rs")),
                symbol_fqn: Some("crate::foo".into()),
                message: "peer modified signature".into(),
            }],
            pending_syntax_files: vec![],
            negative_warnings: vec![neg],
            updated_at_ms: 1234,
        };
        roundtrip_msgpack_and_json(&radar);

        let capsule = CausalCapsule {
            capsule_id: "cap-1".into(),
            viewport_id: "vp-1".into(),
            agent_id: "agy".into(),
            summary: "add foo".into(),
            patch_ids: vec!["p-1".into()],
            touched_files: vec![PathBuf::from("src/lib.rs")],
            touched_symbols: vec!["crate::foo".into()],
            verification_proof: Some(VerificationProof {
                verify_cmd: Some("cargo test".into()),
                exit_code: 0,
                passed: true,
                output_digest: h1,
                proof_hash: h2,
                duration_ms: 50,
                verified_at_ns: 99,
            }),
            parent_capsule_ids: vec![],
            snapshot_cas_hash: Some(h1),
            proof_cas_hash: Some(h2),
            git_commit_oid: None,
            created_at_ns: 100,
        };
        roundtrip_msgpack_and_json(&capsule);

        let op = OpLogEntry {
            op_seq: 1,
            lsn: 1,
            kind: OpKind::CapsuleSeal,
            viewport_id: Some("vp-1".into()),
            target_id: Some("cap-1".into()),
            capsule_id: Some("cap-1".into()),
            touched_symbols: vec!["crate::foo".into()],
            summary: "sealed".into(),
            inverse_payload: Some(vec![1, 2, 3]),
            undone_by_op: None,
            timestamp_ns: 123,
        };
        roundtrip_msgpack_and_json(&op);

        let meta = RepoMeta {
            schema_version: 1,
            repo_id: "repo-1".into(),
            colocate_git: true,
            next_lsn: 2,
            next_op_seq: 2,
            next_edge_seq: 1,
            last_applied_lsn: 1,
            last_checkpoint_lsn: 0,
            lamport_clock: 1,
            head_capsule_id: Some("cap-1".into()),
            created_at_ns: 10,
            updated_at_ns: 20,
        };
        roundtrip_msgpack_and_json(&meta);

        let git = GitMapEntry {
            git_commit_oid: "0".repeat(40),
            capsule_id: Some("cap-1".into()),
            branch: "main".into(),
            direction: GitSyncDirection::Push,
            notes_synced: true,
            timestamp_ns: 45,
        };
        roundtrip_msgpack_and_json(&git);

        let mut snap_files = std::collections::BTreeMap::new();
        snap_files.insert("src/lib.rs".to_string(), h1);
        let snap = WorkspaceSnapshot { files: snap_files };
        roundtrip_msgpack_and_json(&snap);
    }

    #[test]
    fn failure_kind_display_and_from_str_normalization() {
        assert_eq!(FailureKind::CompileError.to_string(), "CompileError");
        assert_eq!(FailureKind::DesignDeadEnd.to_string(), "DesignDeadEnd");

        assert_eq!("compile_error".parse::<FailureKind>().unwrap(), FailureKind::CompileError);
        assert_eq!("COMPILE-ERROR".parse::<FailureKind>().unwrap(), FailureKind::CompileError);
        assert_eq!("TestFailure".parse::<FailureKind>().unwrap(), FailureKind::TestFailure);
        assert_eq!("semantic_conflict".parse::<FailureKind>().unwrap(), FailureKind::SemanticConflict);
        assert_eq!("structural-conflict".parse::<FailureKind>().unwrap(), FailureKind::StructuralConflict);
        assert_eq!("performance_regression".parse::<FailureKind>().unwrap(), FailureKind::PerformanceRegression);
        assert_eq!("design_dead_end".parse::<FailureKind>().unwrap(), FailureKind::DesignDeadEnd);
        assert_eq!("deadend".parse::<FailureKind>().unwrap(), FailureKind::DesignDeadEnd);
        assert_eq!("dead-end".parse::<FailureKind>().unwrap(), FailureKind::DesignDeadEnd);
        assert_eq!(FailureKind::Deadlock.to_string(), "Deadlock");
        assert_eq!("Deadlock".parse::<FailureKind>().unwrap(), FailureKind::Deadlock);
        assert_eq!("deadlock".parse::<FailureKind>().unwrap(), FailureKind::Deadlock);
        assert_eq!("dead-lock".parse::<FailureKind>().unwrap(), FailureKind::Deadlock);
        assert_eq!("dead_lock".parse::<FailureKind>().unwrap(), FailureKind::Deadlock);
        assert_eq!("other".parse::<FailureKind>().unwrap(), FailureKind::Other);
        assert!("bogus_kind".parse::<FailureKind>().is_err());
        assert_eq!(FailureKind::parse_lenient("bogus_kind"), FailureKind::Other);
    }
}
