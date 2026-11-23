use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::{HarnessError, Result};

#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status: ExitStatus,
    pub exit_code: Option<i32>,
}

pub type CmdOutput = CommandOutput;

impl CommandOutput {
    pub fn from_output(
        program: PathBuf,
        args: Vec<String>,
        cwd: PathBuf,
        output: Output,
    ) -> Self {
        let exit_code = output.status.code();
        Self {
            program,
            args,
            cwd,
            stdout: output.stdout,
            stderr: output.stderr,
            status: output.status,
            exit_code,
        }
    }

    pub fn success(&self) -> bool {
        self.status.success()
    }

    pub fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_str(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }

    pub fn stdout_json(&self) -> Result<serde_json::Value> {
        Ok(serde_json::from_slice(&self.stdout)?)
    }

    #[track_caller]
    pub fn assert_success(&self) -> &Self {
        if !self.success() {
            panic!(
                "command failed (exit={:?})\n  program: {}\n  args: {:?}\n  cwd: {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
                self.exit_code,
                self.program.display(),
                self.args,
                self.cwd.display(),
                self.stdout_str(),
                self.stderr_str(),
            );
        }
        self
    }

    #[track_caller]
    pub fn assert_failure(&self) -> &Self {
        if self.success() {
            panic!(
                "command unexpectedly succeeded (exit={:?})\n  program: {}\n  args: {:?}\n  cwd: {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
                self.exit_code,
                self.program.display(),
                self.args,
                self.cwd.display(),
                self.stdout_str(),
                self.stderr_str(),
            );
        }
        self
    }
}

/// Handle to a background `loom` process. Auto-drains stdout/stderr to prevent pipe deadlock
/// and kills/reaps the child on `Drop` (also tracked by `TestRepo`).
pub struct BgChild {
    child: Arc<Mutex<Option<Child>>>,
    cached_status: Mutex<Option<ExitStatus>>,
    stdout_buf: Arc<Mutex<Vec<u8>>>,
    stderr_buf: Arc<Mutex<Vec<u8>>>,
    drain_handles: Mutex<Vec<JoinHandle<()>>>,
}

impl BgChild {
    pub(crate) fn from_child(mut child: Child) -> (Self, Arc<Mutex<Option<Child>>>) {
        let stdout_buf = Arc::new(Mutex::new(Vec::new()));
        let stderr_buf = Arc::new(Mutex::new(Vec::new()));
        let mut drain_handles = Vec::new();

        if let Some(out) = child.stdout.take() {
            drain_handles.push(spawn_drain(out, Arc::clone(&stdout_buf)));
        }
        if let Some(err) = child.stderr.take() {
            drain_handles.push(spawn_drain(err, Arc::clone(&stderr_buf)));
        }

        let shared = Arc::new(Mutex::new(Some(child)));
        let bg = Self {
            child: Arc::clone(&shared),
            cached_status: Mutex::new(None),
            stdout_buf,
            stderr_buf,
            drain_handles: Mutex::new(drain_handles),
        };
        (bg, shared)
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.lock().ok()?.as_ref().map(Child::id)
    }

    pub fn try_wait(&self) -> Result<Option<ExitStatus>> {
        {
            let cached = self.cached_status.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(status) = *cached {
                return Ok(Some(status));
            }
        }
        let mut guard = self.child.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_mut() {
            Some(c) => match c.try_wait()? {
                Some(status) => {
                    *guard = None;
                    *self.cached_status.lock().unwrap_or_else(|e| e.into_inner()) = Some(status);
                    Ok(Some(status))
                }
                None => Ok(None),
            },
            None => Ok(None),
        }
    }

    pub fn wait_timeout(&self, timeout: Duration) -> Result<ExitStatus> {
        {
            let cached = self.cached_status.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(status) = *cached {
                drop(cached);
                self.join_drains_bounded(Duration::from_millis(50));
                return Ok(status);
            }
        }

        let deadline = Instant::now() + timeout;
        loop {
            {
                let mut guard = self.child.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(c) = guard.as_mut() {
                    if let Some(status) = c.try_wait()? {
                        *guard = None;
                        *self.cached_status.lock().unwrap_or_else(|e| e.into_inner()) = Some(status);
                        drop(guard);
                        self.join_drains_bounded(Duration::from_millis(50));
                        return Ok(status);
                    }
                } else {
                    return Err(HarnessError::Protocol("BgChild already reaped".into()));
                }
            }
            if Instant::now() >= deadline {
                return Err(HarnessError::Timeout {
                    context: "background child exit".into(),
                    details: format!("stdout={}\nstderr={}", self.stdout_str(), self.stderr_str()),
                });
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn join_drains_bounded(&self, grace: Duration) {
        let mut handles = self.drain_handles.lock().unwrap_or_else(|e| e.into_inner());
        let deadline = Instant::now() + grace;
        while !handles.iter().all(JoinHandle::is_finished) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        let remaining = std::mem::take(&mut *handles);
        for h in remaining {
            if h.is_finished() {
                let _ = h.join();
            } else {
                handles.push(h);
            }
        }
    }

    pub fn stdout_str(&self) -> String {
        let guard = self.stdout_buf.lock().unwrap_or_else(|e| e.into_inner());
        String::from_utf8_lossy(&guard).into_owned()
    }

    pub fn stderr_str(&self) -> String {
        let guard = self.stderr_buf.lock().unwrap_or_else(|e| e.into_inner());
        String::from_utf8_lossy(&guard).into_owned()
    }

    pub fn kill(&mut self) {
        kill_shared_child(&self.child);
        self.join_drains_bounded(Duration::from_millis(150));
        self.drain_handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}

impl Drop for BgChild {
    fn drop(&mut self) {
        self.kill();
    }
}

pub(crate) fn spawn_drain<R: Read + Send + 'static>(
    mut reader: R,
    buf: Arc<Mutex<Vec<u8>>>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut guard = buf.lock().unwrap_or_else(|e| e.into_inner());
                    guard.extend_from_slice(&chunk[..n]);
                }
            }
        }
    })
}

pub(crate) fn kill_shared_child(shared: &Arc<Mutex<Option<Child>>>) {
    let mut guard = shared.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(mut child) = guard.take() {
        if let Ok(None) = child.try_wait() {
            terminate_pid(child.id());
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub(crate) fn terminate_pid(pid: u32) {
    if pid == 0 || pid == std::process::id() {
        return;
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        std::thread::sleep(Duration::from_millis(50));
        let _ = Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_command_output_json_and_assertions() {
        #[cfg(windows)]
        let output = Command::new("cmd")
            .args(["/C", "echo", "[1,2,3]"])
            .output()
            .expect("spawn cmd");
        #[cfg(not(windows))]
        let output = Command::new("sh")
            .args(["-c", "echo '[1,2,3]'"])
            .output()
            .expect("spawn sh");

        let cmd_out: CmdOutput = CommandOutput::from_output(
            PathBuf::from("test"),
            vec![],
            PathBuf::from("."),
            output,
        );
        cmd_out.assert_success();
        assert_eq!(cmd_out.exit_code(), Some(0));
        let parsed = cmd_out.stdout_json().expect("valid json");
        assert_eq!(parsed, serde_json::json!([1, 2, 3]));

        #[cfg(windows)]
        let fail_output = Command::new("cmd")
            .args(["/C", "exit", "7"])
            .output()
            .expect("spawn cmd exit 7");
        #[cfg(not(windows))]
        let fail_output = Command::new("sh")
            .args(["-c", "exit 7"])
            .output()
            .expect("spawn sh exit 7");

        let fail_out = CommandOutput::from_output(
            PathBuf::from("test"),
            vec![],
            PathBuf::from("."),
            fail_output,
        );
        fail_out.assert_failure();
        assert_eq!(fail_out.exit_code(), Some(7));
    }

    #[test]
    fn unit_bg_child_caches_exit_status_and_drains_output() {
        #[cfg(windows)]
        let child = Command::new("cmd")
            .args(["/C", "echo", "hello-bg"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn cmd");
        #[cfg(not(windows))]
        let child = Command::new("sh")
            .args(["-c", "echo hello-bg"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn sh");

        let (bg, _shared) = BgChild::from_child(child);
        let status = bg.wait_timeout(Duration::from_secs(3)).expect("wait_timeout");
        assert!(status.success());
        assert_eq!(bg.try_wait().unwrap(), Some(status));
        assert_eq!(bg.try_wait().unwrap(), Some(status));
        assert!(bg.stdout_str().contains("hello-bg"));
    }
}
