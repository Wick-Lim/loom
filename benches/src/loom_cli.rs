use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DaemonStatus {
    pub pid: u32,
    pub transport: String,
    pub endpoint: String,
    pub protocol_version: u32,
    pub uptime_ms: u64,
    pub idle_timeout_ms: u64,
    pub active_viewports: usize,
    pub total_viewports: usize,
    pub default_driver: Option<String>,
    pub cas_object_count: u64,
    pub cas_total_bytes: u64,
    pub colocate_git: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ViewportMount {
    pub backend: String,
    pub viewport_root: PathBuf,
    pub hardlinked_files: u64,
    pub copied_files: u64,
    pub shared_cache_dirs: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ViewportSummary {
    pub id: String,
    pub name: String,
    pub intent: String,
    pub state: String,
    pub path: PathBuf,
    pub driver: String,
    pub pid: Option<u32>,
    pub mount: Option<ViewportMount>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RadarConflictItem {
    pub peer_viewport_id: String,
    pub peer_viewport_name: String,
    pub level: String,
    pub kind: String,
    pub file_path: Option<PathBuf>,
    pub symbol_fqn: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RadarStatusReport {
    pub viewport_id: String,
    pub viewport_name: String,
    pub status: String,
    pub pending_syntax: bool,
    pub conflicts: Vec<RadarConflictItem>,
    pub pending_syntax_files: Vec<PathBuf>,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RadarBatchMetrics {
    pub batch_mode: bool,
    pub effective_debounce_ms: u64,
    pub batch_switches: u64,
    pub coalesced_events: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RadarViewportSummary {
    pub viewport_id: String,
    pub viewport_name: String,
    pub level: String,
    pub pending_syntax: bool,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RadarSnapshot {
    pub sequence: u64,
    pub batch_mode: bool,
    pub debounce_ms: u64,
    pub metrics: RadarBatchMetrics,
    pub viewports: Vec<RadarViewportSummary>,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CausalCapsule {
    pub capsule_id: String,
    pub viewport_id: String,
    pub summary: String,
    pub patch_ids: Vec<String>,
    pub touched_files: Vec<PathBuf>,
    pub touched_symbols: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GcResult {
    pub transitioned_viewports: usize,
    pub pruned_viewport_dirs: usize,
    pub removed_cas_blobs: usize,
    pub reclaimed_cas_bytes: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
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

#[derive(Clone)]
pub struct LoomCli {
    bin: PathBuf,
    idle_timeout_ms: u64,
}

impl LoomCli {
    pub fn new(bin: PathBuf, idle_timeout_ms: u64) -> Self {
        Self {
            bin,
            idle_timeout_ms,
        }
    }

    fn run_json<T: serde::de::DeserializeOwned>(
        &self,
        repo_root: &Path,
        args: &[&str],
    ) -> anyhow::Result<T> {
        let mut cmd = Command::new(&self.bin);
        cmd.arg("--repo")
            .arg(repo_root)
            .arg("--json")
            .args(args)
            .current_dir(repo_root)
            .env("LOOM_IDLE_TIMEOUT_MS", self.idle_timeout_ms.to_string());
        let out = cmd
            .output()
            .with_context(|| format!("invoke {} {:?}", self.bin.display(), args))?;
        if !out.status.success() {
            anyhow::bail!(
                "loom {:?} failed (exit {:?}): stdout=`{}` stderr=`{}`",
                args,
                out.status.code(),
                String::from_utf8_lossy(&out.stdout).trim(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        serde_json::from_slice(&out.stdout).with_context(|| {
            format!(
                "parse JSON output of loom {:?}: `{}`",
                args,
                String::from_utf8_lossy(&out.stdout)
            )
        })
    }

    pub fn init_colocate_git(&self, repo_root: &Path) -> anyhow::Result<serde_json::Value> {
        self.run_json(repo_root, &["init", "--colocate-git"])
    }

    pub fn status(&self, repo_root: &Path) -> anyhow::Result<DaemonStatus> {
        self.run_json(repo_root, &["status"])
    }

    pub fn spawn_timed(
        &self,
        repo_root: &Path,
        name: &str,
        intent: &str,
    ) -> anyhow::Result<(ViewportSummary, Duration)> {
        let t0 = Instant::now();
        let vp: ViewportSummary =
            self.run_json(repo_root, &["spawn", "-n", name, "-m", intent])?;
        Ok((vp, t0.elapsed()))
    }

    pub fn seal(
        &self,
        repo_root: &Path,
        viewport_name: &str,
        summary: &str,
        verify_cmd: Option<&str>,
    ) -> anyhow::Result<CausalCapsule> {
        let mut args = vec!["seal", "--viewport", viewport_name, "-m", summary];
        if let Some(cmd) = verify_cmd {
            args.push("-v");
            args.push(cmd);
        }
        self.run_json(repo_root, &args)
    }

    pub fn drop_viewport(
        &self,
        repo_root: &Path,
        viewport_name: &str,
        kind: &str,
        reason: &str,
    ) -> anyhow::Result<serde_json::Value> {
        self.run_json(
            repo_root,
            &[
                "drop",
                "--viewport",
                viewport_name,
                "--kind",
                kind,
                "--reason",
                reason,
            ],
        )
    }

    pub fn gc_prune(&self, repo_root: &Path) -> anyhow::Result<GcResult> {
        self.run_json(repo_root, &["gc", "--prune-viewports"])
    }

    pub fn radar_snapshot(&self, repo_root: &Path) -> anyhow::Result<RadarSnapshot> {
        self.run_json(repo_root, &["radar"])
    }

    pub fn git_push(&self, repo_root: &Path, remote: &str, branch: &str) -> anyhow::Result<GitPushResult> {
        self.run_json(repo_root, &["git", "push", remote, branch])
    }
}
