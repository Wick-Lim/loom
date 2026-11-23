use crate::{ipc, win32};
use anyhow::Context;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub struct BenchWorkspaceGuard {
    temp_dirs: Vec<PathBuf>,
    loom_repos: Vec<PathBuf>,
    tracked_pids: Arc<Mutex<BTreeSet<u32>>>,
    active_children: Vec<Child>,
    cleaned: bool,
}

impl BenchWorkspaceGuard {
    pub fn new() -> Self {
        Self {
            temp_dirs: Vec::new(),
            loom_repos: Vec::new(),
            tracked_pids: Arc::new(Mutex::new(BTreeSet::new())),
            active_children: Vec::new(),
            cleaned: false,
        }
    }

    pub fn create_temp_dir(&mut self, prefix: &str) -> anyhow::Result<PathBuf> {
        let now_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "loom_bench_{prefix}_{}_{now_ns}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create temp dir {}", dir.display()))?;
        self.temp_dirs.push(dir.clone());
        Ok(dir)
    }

    pub fn register_loom_repo(&mut self, repo_root: PathBuf) {
        if !self.loom_repos.contains(&repo_root) {
            self.loom_repos.push(repo_root);
        }
    }

    pub fn track_pid(&self, pid: u32) {
        if pid != 0
            && let Ok(mut guard) = self.tracked_pids.lock()
        {
            guard.insert(pid);
        }
    }

    #[allow(dead_code)]
    pub fn track_child(&mut self, child: Child) {
        self.track_pid(child.id());
        self.active_children.push(child);
    }

    pub fn shutdown_and_remove_repo(&mut self, repo_root: &Path, temp_dir: &Path) {
        if let Ok(Some(pid)) = ipc::shutdown_daemon(repo_root, 3000) {
            self.track_pid(pid);
        }
        if let Some(lock_pid) = ipc::read_lock_pid(repo_root) {
            self.track_pid(lock_pid);
            if !win32::wait_pid_exit(lock_pid, 1500) {
                win32::terminate_pid(lock_pid);
            }
        }
        let _ = remove_dir_all_forced(temp_dir);
        self.loom_repos.retain(|r| r != repo_root);
        self.temp_dirs.retain(|d| d != temp_dir);
    }

    fn execute_cleanup(&mut self) {
        if self.cleaned {
            return;
        }
        self.cleaned = true;

        // Stage 1: Kill & wait any active child processes
        for mut child in self.active_children.drain(..) {
            let _ = child.kill();
            let _ = child.wait();
        }

        // Stage 2: Graceful Named Pipe RpcMethod::Shutdown for all registered Loom repos
        let repos = std::mem::take(&mut self.loom_repos);
        for repo in &repos {
            if let Ok(Some(pid)) = ipc::shutdown_daemon(repo, 3000) {
                self.track_pid(pid);
            }
            if let Some(lock_pid) = ipc::read_lock_pid(repo) {
                self.track_pid(lock_pid);
            }
        }

        // Stage 3: Forceful PID termination fallback for any surviving tracked PIDs
        if let Ok(pids) = self.tracked_pids.lock() {
            for &pid in pids.iter() {
                if !win32::wait_pid_exit(pid, 100) {
                    win32::terminate_pid(pid);
                }
            }
        }

        // Stage 4: Forced %TEMP% directory removal (clearing read-only attributes on .git/objects)
        let dirs = std::mem::take(&mut self.temp_dirs);
        for dir in dirs.into_iter().rev() {
            let _ = remove_dir_all_forced(&dir);
        }
    }
}

impl Default for BenchWorkspaceGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for BenchWorkspaceGuard {
    fn drop(&mut self) {
        self.execute_cleanup();
    }
}

fn clear_readonly_recursive(path: &Path) {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        let mut perms = meta.permissions();
        if perms.readonly() {
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            let _ = std::fs::set_permissions(path, perms);
        }
        if meta.is_dir()
            && let Ok(entries) = std::fs::read_dir(path)
        {
            for entry in entries.flatten() {
                clear_readonly_recursive(&entry.path());
            }
        }
    }
}

pub fn remove_dir_all_forced(path: &Path) -> anyhow::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let mut last_err = None;
    for _ in 0..12 {
        clear_readonly_recursive(path);
        match std::fs::remove_dir_all(path) {
            Ok(()) => return Ok(()),
            Err(err) => {
                last_err = Some(err);
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
    if path.exists()
        && let Some(err) = last_err
    {
        return Err(err).with_context(|| format!("remove {}", path.display()));
    }
    Ok(())
}
