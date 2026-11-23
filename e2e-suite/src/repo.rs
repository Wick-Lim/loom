use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

use crate::command::{kill_shared_child, terminate_pid, BgChild, CommandOutput};
use crate::mcp::McpClient;
use crate::{loom_bin, HarnessError, Result};

pub struct TestRepo {
    dir: Option<TempDir>,
    bare_remotes: Mutex<Vec<TempDir>>,
    bg_children: Mutex<Vec<Arc<Mutex<Option<Child>>>>>,
    loom_roots: Mutex<HashSet<PathBuf>>,
}

impl TestRepo {
    pub fn new() -> Result<Self> {
        let dir = tempfile::Builder::new().prefix("loom-e2e-").tempdir()?;
        let mut roots = HashSet::new();
        roots.insert(dir.path().to_path_buf());
        Ok(Self {
            dir: Some(dir),
            bare_remotes: Mutex::new(Vec::new()),
            bg_children: Mutex::new(Vec::new()),
            loom_roots: Mutex::new(roots),
        })
    }

    pub fn root(&self) -> &Path {
        self.dir
            .as_ref()
            .expect("TestRepo accessed after drop")
            .path()
    }

    pub fn try_viewport_dir(&self, name: &str) -> Result<PathBuf> {
        if name.is_empty()
            || name.contains('/')
            || name.contains('\\')
            || name.contains('\0')
        {
            return Err(HarnessError::InvalidPath(format!(
                "invalid viewport name: {name:?}"
            )));
        }
        let mut components = Path::new(name).components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(_)), None) => {
                Ok(self.root().join(".loom").join("viewports").join(name))
            }
            _ => Err(HarnessError::InvalidPath(format!(
                "viewport name must be a single clean relative component: {name:?}"
            ))),
        }
    }

    #[track_caller]
    pub fn viewport_dir(&self, name: &str) -> PathBuf {
        self.try_viewport_dir(name)
            .unwrap_or_else(|e| panic!("{e}"))
    }

    pub fn path(&self, rel: impl AsRef<Path>) -> Result<PathBuf> {
        let rel = rel.as_ref();
        if rel.is_absolute() {
            return Err(HarnessError::InvalidPath(format!(
                "expected relative path, got {}",
                rel.display()
            )));
        }
        for c in rel.components() {
            if matches!(
                c,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            ) {
                return Err(HarnessError::InvalidPath(format!(
                    "path traversal not allowed: {}",
                    rel.display()
                )));
            }
        }
        Ok(self.root().join(rel))
    }

    pub fn write_file(&self, rel: impl AsRef<Path>, content: &str) -> Result<()> {
        self.write_bytes(rel, content.as_bytes())
    }

    pub fn read_file(&self, rel: impl AsRef<Path>) -> Result<String> {
        let p = self.path(rel)?;
        Ok(fs::read_to_string(p)?)
    }

    pub fn write_bytes(&self, rel: impl AsRef<Path>, bytes: &[u8]) -> Result<()> {
        let p = self.path(rel)?;
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(p, bytes)?;
        Ok(())
    }

    pub fn read_bytes(&self, rel: impl AsRef<Path>) -> Result<Vec<u8>> {
        let p = self.path(rel)?;
        Ok(fs::read(p)?)
    }

    pub fn exists(&self, rel: impl AsRef<Path>) -> bool {
        self.path(rel).map(|p| p.exists()).unwrap_or(false)
    }

    pub fn daemon_pid(&self) -> Option<u32> {
        daemon_pid_at(self.root())
    }

    pub fn git(&self, args: &[&str]) -> CommandOutput {
        self.git_in(self.root(), args)
    }

    pub fn git_in(&self, dir: &Path, args: &[&str]) -> CommandOutput {
        let mut cmd = Command::new("git");
        cmd.args(args)
            .current_dir(dir)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = cmd
            .output()
            .unwrap_or_else(|e| panic!("failed to spawn git {:?} in {}: {e}", args, dir.display()));
        CommandOutput::from_output(
            PathBuf::from("git"),
            args.iter().map(|s| (*s).to_string()).collect(),
            dir.to_path_buf(),
            output,
        )
    }

    pub fn init_git_repo(&self) -> Result<()> {
        self.git(&["init", "-b", "main"]).assert_success();
        self.git(&["config", "user.name", "Loom E2E"])
            .assert_success();
        self.git(&["config", "user.email", "loom-e2e@example.com"])
            .assert_success();
        self.git(&["config", "init.defaultBranch", "main"])
            .assert_success();
        self.git(&["config", "core.autocrlf", "false"])
            .assert_success();

        self.git(&["add", "--", ".", ":!.loom"]).assert_success();
        let status = self.git(&["status", "--porcelain"]);
        status.assert_success();
        if status.stdout_str().trim().is_empty() {
            self.write_file("README.md", "# Loom E2E Test Repo\n")?;
            self.git(&["add", "README.md"]).assert_success();
        }
        self.git(&["commit", "-m", "chore: initial commit"])
            .assert_success();
        Ok(())
    }

    pub fn init_bare_remote(&self) -> Result<PathBuf> {
        let remote_dir = tempfile::Builder::new().prefix("loom-e2e-bare-").tempdir()?;
        let path = remote_dir.path().to_path_buf();
        self.git_in(&path, &["init", "--bare", "-b", "main"])
            .assert_success();
        self.bare_remotes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(remote_dir);
        Ok(path)
    }

    #[track_caller]
    pub fn loom(&self, args: &[&str]) -> CommandOutput {
        self.loom_in_with_env(self.root(), args, &[])
    }

    #[track_caller]
    pub fn loom_in(&self, dir: &Path, args: &[&str]) -> CommandOutput {
        self.loom_in_with_env(dir, args, &[])
    }

    #[track_caller]
    pub fn loom_with_env(&self, args: &[&str], envs: &[(&str, &str)]) -> CommandOutput {
        self.loom_in_with_env(self.root(), args, envs)
    }

    #[track_caller]
    pub fn loom_in_with_env(
        &self,
        dir: &Path,
        args: &[&str],
        envs: &[(&str, &str)],
    ) -> CommandOutput {
        self.try_loom_in_with_env(dir, args, envs)
            .unwrap_or_else(|e| panic!("{e}"))
    }

    fn record_loom_roots(&self, dir: &Path) {
        let mut roots = self.loom_roots.lock().unwrap_or_else(|e| e.into_inner());
        roots.insert(dir.to_path_buf());
        for ancestor in dir.ancestors() {
            if ancestor.join(".loom").is_dir() {
                roots.insert(ancestor.to_path_buf());
            }
        }
    }

    pub fn try_loom_in_with_env(
        &self,
        dir: &Path,
        args: &[&str],
        envs: &[(&str, &str)],
    ) -> Result<CommandOutput> {
        self.record_loom_roots(dir);
        let bin = loom_bin();
        let mut cmd = Command::new(&bin);
        cmd.args(args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let output = cmd.output().map_err(|source| HarnessError::ProcessSpawn {
            program: bin.clone(),
            args: args.iter().map(|s| (*s).to_string()).collect(),
            source,
        })?;
        self.record_loom_roots(dir);
        Ok(CommandOutput::from_output(
            bin,
            args.iter().map(|s| (*s).to_string()).collect(),
            dir.to_path_buf(),
            output,
        ))
    }

    pub fn spawn_loom_bg(
        &self,
        args: &[&str],
        cwd: Option<&Path>,
        envs: &[(&str, &str)],
    ) -> Result<BgChild> {
        let dir = cwd.unwrap_or_else(|| self.root());
        self.record_loom_roots(dir);
        let bin = loom_bin();
        let mut cmd = Command::new(&bin);
        cmd.args(args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let child = cmd.spawn().map_err(|source| HarnessError::ProcessSpawn {
            program: bin,
            args: args.iter().map(|s| (*s).to_string()).collect(),
            source,
        })?;
        let (bg, shared) = BgChild::from_child(child);
        self.bg_children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(shared);
        Ok(bg)
    }

    pub fn spawn_mcp_stdio(&self, cwd: Option<&Path>) -> Result<McpClient> {
        let dir = cwd.unwrap_or_else(|| self.root());
        self.record_loom_roots(dir);
        let bin = loom_bin();
        let mut cmd = Command::new(&bin);
        cmd.args(["mcp", "serve"]).current_dir(dir);
        let (client, shared_child) = McpClient::spawn(&bin, &mut cmd)?;
        self.bg_children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(shared_child);
        Ok(client)
    }

    pub fn wait_for_radar_status<F>(
        &self,
        viewport_root: &Path,
        timeout: Duration,
        predicate: F,
    ) -> Result<Value>
    where
        F: FnMut(&Value) -> bool,
    {
        wait_for_radar_status(viewport_root, timeout, predicate)
    }
}

pub fn wait_for_radar_status<F>(
    viewport_root: &Path,
    timeout: Duration,
    mut predicate: F,
) -> Result<Value>
where
    F: FnMut(&Value) -> bool,
{
    let status_path =
        if viewport_root.file_name().and_then(|s| s.to_str()) == Some("RADAR_STATUS.json") {
            viewport_root.to_path_buf()
        } else {
            viewport_root.join(".loom").join("RADAR_STATUS.json")
        };

    let deadline = Instant::now() + timeout;
    let mut last_valid: Option<Value> = None;
    let mut last_err: Option<String>;

    loop {
        match fs::read_to_string(&status_path) {
            Ok(text) => match serde_json::from_str::<Value>(&text) {
                Ok(val) => {
                    if predicate(&val) {
                        return Ok(val);
                    }
                    last_valid = Some(val);
                    last_err = None;
                }
                Err(e) => {
                    last_err = Some(format!("JSON parse error: {e}"));
                }
            },
            Err(e) => {
                last_err = Some(format!("read error: {e}"));
            }
        }

        let now = Instant::now();
        if now >= deadline {
            return Err(HarnessError::Timeout {
                context: format!("RADAR_STATUS.json at {}", status_path.display()),
                details: format!("last_valid={last_valid:?}, last_err={last_err:?}"),
            });
        }
        std::thread::sleep(Duration::from_millis(20).min(deadline - now));
    }
}

impl Drop for TestRepo {
    fn drop(&mut self) {
        // 1. Kill all tracked background & MCP child processes
        if let Ok(mut children) = self.bg_children.lock() {
            for shared in children.drain(..) {
                kill_shared_child(&shared);
            }
        }

        // 2. Kill any auto-spawned loomd recorded in .loom/daemon.sock and/or .loom/daemon.pid
        if let Ok(roots) = self.loom_roots.lock() {
            let mut expanded = HashSet::new();
            for root in roots.iter() {
                expanded.insert(root.clone());
                for ancestor in root.ancestors() {
                    if ancestor.join(".loom").is_dir() {
                        expanded.insert(ancestor.to_path_buf());
                    }
                }
            }
            for root in expanded {
                kill_loomd_at_root(&root);
            }
        }

        // 3. Remove TempDir with bounded retry (up to 20 x 50ms) for Windows file-handle release latency
        if let Some(dir) = self.dir.take() {
            remove_tempdir_with_retry(dir);
        }

        if let Ok(mut remotes) = self.bare_remotes.lock() {
            for remote in remotes.drain(..) {
                remove_tempdir_with_retry(remote);
            }
        }
    }
}

fn remove_tempdir_with_retry(dir: TempDir) {
    let path = dir.path().to_path_buf();
    if dir.close().is_err() {
        for _ in 0..20 {
            if !path.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
            let _ = clear_readonly_recursive(&path);
            if fs::remove_dir_all(&path).is_ok() {
                break;
            }
        }
    }
}

pub(crate) fn daemon_pids_at(root: &Path) -> Vec<u32> {
    let mut pids = Vec::new();
    let mut seen = HashSet::new();

    let sock_path = root.join(".loom").join("daemon.sock");
    if let Ok(text) = fs::read_to_string(&sock_path) {
        if let Ok(val) = serde_json::from_str::<Value>(&text) {
            if let Some(pid) = val.get("pid").and_then(Value::as_u64) {
                if let Ok(pid_u32) = u32::try_from(pid) {
                    if pid_u32 > 0 && seen.insert(pid_u32) {
                        pids.push(pid_u32);
                    }
                }
            }
        }
    }

    let pid_path = root.join(".loom").join("daemon.pid");
    if let Ok(text) = fs::read_to_string(&pid_path) {
        if let Ok(pid) = text.trim().parse::<u32>() {
            if pid > 0 && seen.insert(pid) {
                pids.push(pid);
            }
        }
    }

    pids
}

pub(crate) fn daemon_pid_at(root: &Path) -> Option<u32> {
    daemon_pids_at(root).into_iter().next()
}

fn kill_loomd_at_root(root: &Path) {
    let self_pid = std::process::id();
    for pid in daemon_pids_at(root) {
        if pid != 0 && pid != self_pid {
            terminate_pid(pid);
        }
    }
}

fn clear_readonly_recursive(path: &Path) -> std::io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    if let Ok(meta) = fs::symlink_metadata(path) {
        let mut perms = meta.permissions();
        if perms.readonly() {
            perms.set_readonly(false);
            let _ = fs::set_permissions(path, perms);
        }
        if meta.is_dir() {
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    let _ = clear_readonly_recursive(&entry.path());
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_testrepo_viewport_dir_and_daemon_pid() {
        let repo = TestRepo::new().expect("create TestRepo");
        assert_eq!(
            repo.viewport_dir("vp1"),
            repo.root().join(".loom").join("viewports").join("vp1")
        );
        assert_eq!(repo.daemon_pid(), None);

        repo.write_file(
            ".loom/daemon.sock",
            r#"{"transport":"named_pipe","endpoint":"\\\\.\\pipe\\loom-abc","pid":424242,"protocol_version":1}"#,
        )
        .unwrap();
        repo.write_file(".loom/daemon.pid", "434343\n").unwrap();
        assert_eq!(repo.daemon_pid(), Some(424242));
        assert_eq!(daemon_pids_at(repo.root()), vec![424242, 434343]);

        // Duplicate PID in both files is deduplicated
        repo.write_file(".loom/daemon.pid", "424242").unwrap();
        assert_eq!(daemon_pids_at(repo.root()), vec![424242]);

        // Remove synthetic PID files before Drop so taskkill is not invoked on fake PIDs
        fs::remove_file(repo.path(".loom/daemon.sock").unwrap()).unwrap();
        fs::remove_file(repo.path(".loom/daemon.pid").unwrap()).unwrap();
        assert_eq!(repo.daemon_pid(), None);
    }

    #[test]
    fn unit_viewport_dir_rejects_traversal_and_invalid_names() {
        let repo = TestRepo::new().expect("create TestRepo");
        assert!(repo.try_viewport_dir("feat-a").is_ok());

        for bad in ["", ".", "..", "../escape", "a/b", r"a\b", "/abs"] {
            assert!(
                matches!(repo.try_viewport_dir(bad), Err(HarnessError::InvalidPath(_))),
                "expected InvalidPath for {bad:?}"
            );
        }
        if cfg!(windows) {
            assert!(matches!(
                repo.try_viewport_dir("C:rel"),
                Err(HarnessError::InvalidPath(_))
            ));
        }
    }

    #[test]
    fn unit_init_git_repo_handles_empty_subdir() {
        let repo = TestRepo::new().expect("create TestRepo");
        fs::create_dir_all(repo.path("src").unwrap()).unwrap();
        repo.init_git_repo().expect("init_git_repo with empty src/");
        assert!(repo.exists("README.md"));
        let log = repo.git(&["log", "--oneline"]);
        log.assert_success();
        assert!(log.stdout_str().contains("initial commit"));
    }
}
