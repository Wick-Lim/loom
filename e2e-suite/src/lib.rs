pub mod command;
pub mod mcp;
pub mod repo;

pub use command::{BgChild, CmdOutput, CommandOutput};
pub use mcp::{read_jsonrpc_frame, write_jsonrpc_frame, McpClient, McpFraming};
pub use repo::{wait_for_radar_status, TestRepo};

use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};

pub type Result<T> = std::result::Result<T, HarnessError>;

#[derive(Debug)]
pub enum HarnessError {
    Io(std::io::Error),
    Json(serde_json::Error),
    TomlDe(toml::de::Error),
    TomlSer(toml::ser::Error),
    InvalidPath(String),
    ProcessSpawn {
        program: PathBuf,
        args: Vec<String>,
        source: std::io::Error,
    },
    JsonRpc {
        code: i64,
        message: String,
        data: Option<serde_json::Value>,
    },
    Protocol(String),
    Timeout {
        context: String,
        details: String,
    },
}

impl fmt::Display for HarnessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Json(e) => write!(f, "JSON error: {e}"),
            Self::TomlDe(e) => write!(f, "TOML deserialize error: {e}"),
            Self::TomlSer(e) => write!(f, "TOML serialize error: {e}"),
            Self::InvalidPath(msg) => write!(f, "invalid relative path: {msg}"),
            Self::ProcessSpawn {
                program,
                args,
                source,
            } => {
                write!(
                    f,
                    "failed to spawn `{}` {:?}: {source}",
                    program.display(),
                    args
                )
            }
            Self::JsonRpc {
                code,
                message,
                data,
            } => {
                write!(f, "JSON-RPC error ({code}): {message}; data={data:?}")
            }
            Self::Protocol(msg) => write!(f, "protocol error: {msg}"),
            Self::Timeout { context, details } => {
                write!(f, "timeout waiting for {context}: {details}")
            }
        }
    }
}

impl std::error::Error for HarnessError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Json(e) => Some(e),
            Self::TomlDe(e) => Some(e),
            Self::TomlSer(e) => Some(e),
            Self::ProcessSpawn { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<std::io::Error> for HarnessError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for HarnessError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

impl From<toml::de::Error> for HarnessError {
    fn from(e: toml::de::Error) -> Self {
        Self::TomlDe(e)
    }
}

impl From<toml::ser::Error> for HarnessError {
    fn from(e: toml::ser::Error) -> Self {
        Self::TomlSer(e)
    }
}

const LOOM_EXE_NAME: &str = if cfg!(windows) { "loom.exe" } else { "loom" };
const DEFAULT_WINDOWS_AGY: &str = r"C:\Users\ijh69\AppData\Local\agy\bin\agy.exe";

/// Workspace root containing `e2e-suite/`.
pub fn loom_repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("e2e-suite has parent repo root")
        .to_path_buf()
}

/// Resolves the `loom` binary path:
/// 1. `LOOM_BIN` env var (if set & non-empty)
/// 2. `<target_dir>/debug/loom[.exe]`
/// 3. `<target_dir>/release/loom[.exe]`
/// 4. `loom[.exe]` found on `PATH`
/// 5. Fallback `PathBuf::from("loom.exe" | "loom")`
pub fn loom_bin() -> PathBuf {
    let loom_env = std::env::var("LOOM_BIN").ok();
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| loom_repo_root().join("target"));
    let path_env = std::env::var_os("PATH");
    resolve_loom_bin_with(
        loom_env.as_deref(),
        &target_dir,
        path_env.as_deref(),
        |p| p.is_file(),
    )
}

pub fn resolve_loom_bin_with<F>(
    loom_bin_env: Option<&str>,
    target_dir: &Path,
    path_env: Option<&OsStr>,
    exists: F,
) -> PathBuf
where
    F: Fn(&Path) -> bool,
{
    if let Some(raw) = loom_bin_env.map(str::trim).filter(|s| !s.is_empty()) {
        return PathBuf::from(raw);
    }

    let debug = target_dir.join("debug").join(LOOM_EXE_NAME);
    if exists(&debug) {
        return debug;
    }

    let release = target_dir.join("release").join(LOOM_EXE_NAME);
    if exists(&release) {
        return release;
    }

    if let Some(found) = find_on_path(path_env, &[LOOM_EXE_NAME], &exists) {
        return found;
    }

    PathBuf::from(LOOM_EXE_NAME)
}

/// Resolves the `agy` CLI binary path if available:
/// 1. `AGY_BIN` env var (if set & non-empty)
/// 2. `C:\Users\ijh69\AppData\Local\agy\bin\agy.exe` (Windows)
/// 3. `agy[.exe/.cmd]` on `PATH`
pub fn agy_bin() -> Option<PathBuf> {
    let agy_env = std::env::var("AGY_BIN").ok();
    let default_win = if cfg!(windows) {
        Some(Path::new(DEFAULT_WINDOWS_AGY))
    } else {
        None
    };
    let path_env = std::env::var_os("PATH");
    resolve_agy_bin_with(
        agy_env.as_deref(),
        default_win,
        path_env.as_deref(),
        |p| p.is_file(),
    )
}

pub fn resolve_agy_bin_with<F>(
    agy_bin_env: Option<&str>,
    default_windows_path: Option<&Path>,
    path_env: Option<&OsStr>,
    exists: F,
) -> Option<PathBuf>
where
    F: Fn(&Path) -> bool,
{
    if let Some(raw) = agy_bin_env.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(raw));
    }

    if let Some(win_default) = default_windows_path {
        if exists(win_default) {
            return Some(win_default.to_path_buf());
        }
    }

    let candidates: &[&str] = if cfg!(windows) {
        &["agy.exe", "agy.cmd", "agy"]
    } else {
        &["agy"]
    };
    find_on_path(path_env, candidates, &exists)
}

fn find_on_path<F>(path_env: Option<&OsStr>, names: &[&str], exists: &F) -> Option<PathBuf>
where
    F: Fn(&Path) -> bool,
{
    let raw = path_env?;
    for dir in std::env::split_paths(raw) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for name in names {
            let candidate = dir.join(name);
            if exists(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn unit_loom_bin_fallback_when_absent() {
        let target = Path::new("nonexistent-target");
        let resolved = resolve_loom_bin_with(None, target, None, |_| false);
        assert_eq!(resolved, PathBuf::from(LOOM_EXE_NAME));
    }

    #[test]
    fn unit_loom_bin_ignores_whitespace_env() {
        let target = Path::new("target_dir");
        let debug = target.join("debug").join(LOOM_EXE_NAME);
        let resolved =
            resolve_loom_bin_with(Some("   "), target, None, |p| p == debug.as_path());
        assert_eq!(resolved, debug);
    }

    #[test]
    fn unit_agy_bin_none_when_absent() {
        let resolved = resolve_agy_bin_with(Some("  "), None, Some(&OsString::new()), |_| false);
        assert_eq!(resolved, None);
    }
}
