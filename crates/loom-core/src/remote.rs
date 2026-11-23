use crate::cas::CasStore;
use crate::model::{
    AstPatchRecord, Blake3Hash, CausalCapsule, NegativeEdge, WorkspaceSnapshot,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const REMOTE_PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRemoteUrl {
    pub base_url: String,
    pub repo_slug: String,
    pub default_dir_name: String,
}

fn is_valid_slug_segment(seg: &str) -> bool {
    if seg.is_empty() || seg.len() > 64 || seg == "." || seg == ".." {
        return false;
    }
    let bytes = seg.as_bytes();
    if !bytes[0].is_ascii_alphanumeric() {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

pub fn parse_remote_repo_url(raw: &str) -> Result<ParsedRemoteUrl, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("remote URL cannot be empty".to_string());
    }
    if trimmed.contains('\\')
        || trimmed.contains('?')
        || trimmed.contains('#')
        || trimmed.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(format!(
            "invalid remote URL `{raw}`: unexpected query, fragment, backslash, or whitespace"
        ));
    }

    let (scheme, rest) = if let Some(r) = trimmed.strip_prefix("http://") {
        ("http", r)
    } else if let Some(r) = trimmed.strip_prefix("https://") {
        ("https", r)
    } else {
        return Err(format!(
            "unsupported remote URL `{raw}`; Loom native remote URLs must use http:// or https://"
        ));
    };

    let Some((authority, raw_path)) = rest.split_once('/') else {
        return Err(format!(
            "invalid remote URL `{raw}`: missing repository path (expected {scheme}://<host>/<repo>)"
        ));
    };
    if authority.is_empty() || authority.contains('@') {
        return Err(format!(
            "invalid remote URL `{raw}`: invalid authority `{authority}`"
        ));
    }

    let trimmed_path = raw_path.trim_matches('/');
    if trimmed_path.is_empty() {
        return Err(format!(
            "invalid remote URL `{raw}`: missing repository slug after host"
        ));
    }

    let mut segments: Vec<String> = trimmed_path
        .split('/')
        .map(|s| s.to_string())
        .collect();
    if segments.len() > 2 {
        return Err(format!(
            "invalid remote URL `{raw}`: repository slug must have at most 2 path segments (`<repo>` or `<owner>/<repo>`)"
        ));
    }

    if let Some(last) = segments.last_mut()
        && let Some(stripped) = last.strip_suffix(".loom")
    {
        *last = stripped.to_string();
    }

    for seg in &segments {
        if !is_valid_slug_segment(seg) {
            return Err(format!(
                "invalid repository slug segment `{seg}` in remote URL `{raw}`"
            ));
        }
    }

    let repo_slug = segments.join("/");
    let default_dir_name = segments
        .last()
        .cloned()
        .unwrap_or_else(|| "loom-repo".to_string());

    Ok(ParsedRemoteUrl {
        base_url: format!("{scheme}://{authority}"),
        repo_slug,
        default_dir_name,
    })
}

pub fn validate_repo_slug(slug: &str) -> Result<String, String> {
    let trimmed = slug.trim().trim_matches('/');
    if trimmed.is_empty() {
        return Err("repository slug cannot be empty".to_string());
    }
    let mut segments: Vec<String> = trimmed.split('/').map(|s| s.to_string()).collect();
    if segments.len() > 2 {
        return Err(format!(
            "invalid repository slug `{slug}`: expected `<repo>` or `<owner>/<repo>`"
        ));
    }
    if let Some(last) = segments.last_mut()
        && let Some(stripped) = last.strip_suffix(".loom")
    {
        *last = stripped.to_string();
    }
    for seg in &segments {
        if !is_valid_slug_segment(seg) {
            return Err(format!("invalid repository slug segment `{seg}`"));
        }
    }
    Ok(segments.join("/"))
}

pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    let len = a.len().min(b.len());
    for i in 0..len {
        diff |= usize::from(a[i] ^ b[i]);
    }
    diff == 0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RemoteRefState {
    pub head_capsule_id: Option<String>,
    pub head_snapshot_cas: Option<Blake3Hash>,
    pub updated_at_ns: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RemoteRepoInfo {
    pub repo: String,
    pub exists: bool,
    pub refs: std::collections::BTreeMap<String, RemoteRefState>,
    pub capsule_count: u64,
    pub patch_count: u64,
    pub negative_edge_count: u64,
    pub cas_object_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CasHasRequest {
    pub hashes: Vec<Blake3Hash>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CasHasResponse {
    pub missing: Vec<Blake3Hash>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RemotePushRequest {
    pub branch: String,
    pub expected_remote_head: Option<String>,
    pub force: bool,
    pub new_head_capsule_id: Option<String>,
    pub new_head_snapshot_cas: Option<Blake3Hash>,
    pub capsules: Vec<CausalCapsule>,
    pub patches: Vec<AstPatchRecord>,
    pub negative_edges: Vec<NegativeEdge>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RemotePushResponse {
    pub repo: String,
    pub branch: String,
    pub head_capsule_id: Option<String>,
    pub head_snapshot_cas: Option<Blake3Hash>,
    pub accepted_capsules: usize,
    pub accepted_patches: usize,
    pub accepted_negative_edges: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RemotePullRequest {
    pub branch: String,
    pub known_capsule_ids: Vec<String>,
    pub known_patch_ids: Vec<String>,
    pub known_negative_edge_ids: Vec<String>,
    pub known_cas_hashes: Vec<Blake3Hash>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RemotePullResponse {
    pub repo: String,
    pub branch: String,
    pub head_capsule_id: Option<String>,
    pub head_snapshot_cas: Option<Blake3Hash>,
    pub capsules: Vec<CausalCapsule>,
    pub patches: Vec<AstPatchRecord>,
    pub negative_edges: Vec<NegativeEdge>,
    pub required_cas_hashes: Vec<Blake3Hash>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RemoteErrorBody {
    pub code: String,
    pub message: String,
    pub remote_head_capsule_id: Option<String>,
    pub missing_cas_hashes: Vec<Blake3Hash>,
}

pub fn collect_required_cas_hashes(
    cas: &CasStore,
    root_snapshots: &[Blake3Hash],
    capsules: &[CausalCapsule],
    patches: &[AstPatchRecord],
) -> anyhow::Result<Vec<Blake3Hash>> {
    let mut hashes: BTreeSet<Blake3Hash> = BTreeSet::new();
    let mut required_snapshots: BTreeSet<Blake3Hash> = BTreeSet::new();

    for &snap_hash in root_snapshots {
        if snap_hash != Blake3Hash::ZERO {
            required_snapshots.insert(snap_hash);
        }
    }

    for cap in capsules {
        if let Some(snap_hash) = cap.snapshot_cas_hash
            && snap_hash != Blake3Hash::ZERO
            && (root_snapshots.contains(&snap_hash) || cas.contains(&snap_hash))
        {
            required_snapshots.insert(snap_hash);
        }
        if let Some(proof_hash) = cap.proof_cas_hash
            && proof_hash != Blake3Hash::ZERO
            && cas.contains(&proof_hash)
        {
            hashes.insert(proof_hash);
        }
    }

    for snap_hash in required_snapshots {
        let raw = cas.get_bytes(&snap_hash)?;
        let snapshot: WorkspaceSnapshot = rmp_serde::from_slice(&raw)?;
        hashes.insert(snap_hash);
        for file_hash in snapshot.files.values() {
            if *file_hash != Blake3Hash::ZERO {
                if !cas.contains(file_hash) {
                    anyhow::bail!(
                        "snapshot {} references missing CAS blob {}",
                        snap_hash.short_hex(16),
                        file_hash.short_hex(16)
                    );
                }
                hashes.insert(*file_hash);
            }
        }
    }

    for patch_rec in patches {
        if let Some(before) = patch_rec.patch.cas_before
            && before != Blake3Hash::ZERO
            && cas.contains(&before)
        {
            hashes.insert(before);
        }
        if let Some(after) = patch_rec.patch.cas_after
            && after != Blake3Hash::ZERO
            && cas.contains(&after)
        {
            hashes.insert(after);
        }
        if let Some(ref bm) = patch_rec.patch.binary_manifest {
            for ch in &bm.chunk_hashes {
                if *ch != Blake3Hash::ZERO && cas.contains(ch) {
                    hashes.insert(*ch);
                }
            }
        }
    }

    Ok(hashes.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    #[test]
    fn parse_remote_repo_url_accepts_valid_and_rejects_bad() {
        let one_seg = parse_remote_repo_url("http://127.0.0.1:9418/my-repo.loom/").unwrap();
        assert_eq!(one_seg.base_url, "http://127.0.0.1:9418");
        assert_eq!(one_seg.repo_slug, "my-repo");
        assert_eq!(one_seg.default_dir_name, "my-repo");

        let two_seg = parse_remote_repo_url("https://loom.example.com/wick-lim/loom").unwrap();
        assert_eq!(two_seg.base_url, "https://loom.example.com");
        assert_eq!(two_seg.repo_slug, "wick-lim/loom");
        assert_eq!(two_seg.default_dir_name, "loom");

        assert!(parse_remote_repo_url("file:///tmp/repo").is_err());
        assert!(parse_remote_repo_url("http://localhost:9418/").is_err());
        assert!(parse_remote_repo_url("http://localhost:9418/a/b/c").is_err());
        assert!(parse_remote_repo_url("http://localhost:9418/../evil").is_err());
        assert!(parse_remote_repo_url("http://localhost:9418/repo?foo=bar").is_err());
        assert!(parse_remote_repo_url("http://user:pass@localhost:9418/repo").is_err());
    }

    #[test]
    fn constant_time_eq_checks_exact_equality() {
        assert!(constant_time_eq(b"secret-token-123", b"secret-token-123"));
        assert!(!constant_time_eq(b"secret-token-123", b"secret-token-124"));
        assert!(!constant_time_eq(b"secret", b"secret-longer"));
    }

    #[test]
    fn collect_required_cas_hashes_walks_snapshots_and_patches() {
        let dir = TempDir::new().unwrap();
        let cas = CasStore::open_default(dir.path().join("cas")).unwrap();
        let f1 = cas.put_bytes(b"fn main() {}\n").unwrap();
        let f2 = cas.put_bytes(b"pub fn helper() {}\n").unwrap();
        let mut files = BTreeMap::new();
        files.insert("src/main.rs".to_string(), f1);
        files.insert("src/lib.rs".to_string(), f2);
        let snap = WorkspaceSnapshot { files };
        let snap_bytes = rmp_serde::to_vec_named(&snap).unwrap();
        let snap_hash = cas.put_bytes(&snap_bytes).unwrap();
        let proof_hash = cas.put_bytes(b"ok").unwrap();

        let cap = CausalCapsule {
            capsule_id: "cap-1".into(),
            snapshot_cas_hash: Some(snap_hash),
            proof_cas_hash: Some(proof_hash),
            ..Default::default()
        };

        let collected = collect_required_cas_hashes(&cas, &[snap_hash], &[cap], &[]).unwrap();
        assert!(collected.contains(&snap_hash));
        assert!(collected.contains(&f1));
        assert!(collected.contains(&f2));
        assert!(collected.contains(&proof_hash));
    }
}
