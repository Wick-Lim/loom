use interprocess::local_socket::tokio::Stream;
use loom_core::rpc::{
    RadarSnapshot, RpcErrorCode, RpcMethod, RpcPayload, RpcRequest, RpcResponse,
    canonical_repo_root, read_frame, sock_descriptor_path, write_frame,
};
#[cfg(windows)]
use loom_core::rpc::{pipe_short_name, read_sock_descriptor};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{ReadHalf, WriteHalf};

pub fn is_loom_repo_root(dir: &Path) -> bool {
    let loom_dir = dir.join(".loom");
    loom_dir.join("config.toml").is_file() || loom_dir.join("state.redb").is_file()
}

pub fn discover_repo_root(explicit_repo: Option<&Path>, for_init: bool) -> anyhow::Result<PathBuf> {
    if for_init {
        let raw = match explicit_repo {
            Some(p) => {
                if !p.exists() {
                    std::fs::create_dir_all(p)?;
                }
                p.to_path_buf()
            }
            None => std::env::current_dir()?,
        };
        let root = canonical_repo_root(&raw)?;
        let loom_dir = root.join(".loom");
        std::fs::create_dir_all(&loom_dir)?;
        return Ok(root);
    }

    if let Some(p) = explicit_repo {
        let root = canonical_repo_root(p)?;
        anyhow::ensure!(
            is_loom_repo_root(&root),
            "{} is not an initialized Loom repository (run `loom init` first)",
            root.display()
        );
        return Ok(root);
    }

    let cwd = canonical_repo_root(&std::env::current_dir()?)?;
    for ancestor in cwd.ancestors() {
        if is_loom_repo_root(ancestor) {
            return Ok(ancestor.to_path_buf());
        }
    }

    anyhow::bail!("not inside a Loom repository (run `loom init` first)")
}

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
        let stream = Stream::connect(ns_name).await?;
        Ok(stream)
    }

    #[cfg(unix)]
    {
        use interprocess::local_socket::{GenericFilePath, ToFsName, tokio::prelude::*};
        let sock_path = sock_descriptor_path(repo_root);
        let fs_name = sock_path.as_path().to_fs_name::<GenericFilePath>()?;
        let stream = Stream::connect(fs_name).await?;
        Ok(stream)
    }
}

#[cfg(unix)]
struct SpawnedDaemonHandle(std::process::Child);

#[cfg(unix)]
impl SpawnedDaemonHandle {
    fn try_exit_code(&mut self) -> Option<i32> {
        match self.0.try_wait() {
            Ok(Some(status)) => Some(status.code().unwrap_or(1)),
            _ => None,
        }
    }
}

#[cfg(windows)]
struct SpawnedDaemonHandle(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
unsafe impl Send for SpawnedDaemonHandle {}

#[cfg(windows)]
impl Drop for SpawnedDaemonHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(self.0);
            }
        }
    }
}

#[cfg(windows)]
impl SpawnedDaemonHandle {
    fn try_exit_code(&mut self) -> Option<i32> {
        use windows_sys::Win32::Foundation::{INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
        if self.0.is_null() || self.0 == INVALID_HANDLE_VALUE {
            return None;
        }
        let wait = unsafe { WaitForSingleObject(self.0, 0) };
        if wait == WAIT_OBJECT_0 {
            let mut code: u32 = 0;
            if unsafe { GetExitCodeProcess(self.0, &mut code) } != 0 {
                return Some(code as i32);
            }
        }
        None
    }
}

fn daemon_log_tail(repo_root: &Path, max_lines: usize) -> Option<String> {
    let log_path = repo_root.join(".loom").join("daemon.log");
    let raw = std::fs::read_to_string(log_path).ok()?;
    let lines: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return None;
    }
    let start = lines.len().saturating_sub(max_lines);
    Some(lines[start..].join(" | "))
}

pub async fn connect_or_autostart(repo_root: &Path) -> anyhow::Result<Stream> {
    connect_or_autostart_with(repo_root, spawn_detached_daemon).await
}

async fn connect_or_autostart_with<F>(repo_root: &Path, mut spawn_fn: F) -> anyhow::Result<Stream>
where
    F: FnMut(&Path) -> anyhow::Result<SpawnedDaemonHandle>,
{
    if let Ok(stream) = connect_ipc(repo_root).await {
        return Ok(stream);
    }

    let mut primary_daemon = Some(spawn_fn(repo_root)?);
    let mut secondary_daemon: Option<SpawnedDaemonHandle> = None;

    let start = Instant::now();
    let deadline = start + Duration::from_secs(5);
    let backoffs = [5u64, 10, 20, 40, 80, 160, 250];
    let mut attempt = 0usize;
    let mut respawned = false;
    let mut last_spawn_at = start;

    loop {
        if let Ok(stream) = connect_ipc(repo_root).await {
            return Ok(stream);
        }
        if let Some(code) = primary_daemon.as_mut().and_then(|d| d.try_exit_code()) {
            if code != 0 {
                let tail = daemon_log_tail(repo_root, 5)
                    .map(|t| format!("; daemon.log: {t}"))
                    .unwrap_or_default();
                anyhow::bail!("daemon failed to start (exit code {code}){tail}");
            }
            primary_daemon = None;
        }
        if let Some(code) = secondary_daemon.as_mut().and_then(|d| d.try_exit_code()) {
            if code != 0 {
                let tail = daemon_log_tail(repo_root, 5)
                    .map(|t| format!("; daemon.log: {t}"))
                    .unwrap_or_default();
                anyhow::bail!("daemon failed to start (exit code {code}){tail}");
            }
            secondary_daemon = None;
        }
        if !respawned && start.elapsed() >= Duration::from_millis(100) {
            respawned = true;
            secondary_daemon = Some(spawn_fn(repo_root)?);
            last_spawn_at = Instant::now();
        } else if primary_daemon.is_none()
            && secondary_daemon.is_none()
            && last_spawn_at.elapsed() >= Duration::from_millis(80)
            && Instant::now() < deadline
        {
            secondary_daemon = Some(spawn_fn(repo_root)?);
            last_spawn_at = Instant::now();
        }
        if Instant::now() >= deadline {
            break;
        }
        let delay_ms = backoffs[attempt.min(backoffs.len() - 1)];
        attempt += 1;
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
    }

    match connect_ipc(repo_root).await {
        Ok(stream) => Ok(stream),
        Err(err) => {
            if let Some(tail) = daemon_log_tail(repo_root, 5) {
                Err(err.context(format!("daemon.log: {tail}")))
            } else {
                Err(err)
            }
        }
    }
}

#[cfg(unix)]
fn spawn_detached_daemon(repo_root: &Path) -> anyhow::Result<SpawnedDaemonHandle> {
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("__daemon").arg("--repo").arg(repo_root);

    if let Ok(ms) = std::env::var("LOOM_IDLE_TIMEOUT_MS")
        && !ms.trim().is_empty()
    {
        cmd.arg("--idle-timeout-ms").arg(ms.trim());
    }

    cmd.stdin(Stdio::null()).stdout(Stdio::null());
    let log_path = repo_root.join(".loom").join("daemon.log");
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(file) => {
            cmd.stderr(Stdio::from(file));
        }
        Err(_) => {
            cmd.stderr(Stdio::null());
        }
    }

    let child = cmd.spawn()?;
    Ok(SpawnedDaemonHandle(child))
}

#[cfg(windows)]
fn spawn_detached_daemon(repo_root: &Path) -> anyhow::Result<SpawnedDaemonHandle> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_APPEND_DATA, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_ALWAYS, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_NO_WINDOW, CreateProcessW, DeleteProcThreadAttributeList,
        EXTENDED_STARTUPINFO_PRESENT, InitializeProcThreadAttributeList,
        LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION,
        STARTF_USESTDHANDLES, STARTUPINFOEXW, UpdateProcThreadAttribute,
    };

    struct OwnedHandle(HANDLE);
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }

    struct ProcThreadAttrList {
        _buf: Vec<usize>,
        ptr: LPPROC_THREAD_ATTRIBUTE_LIST,
    }
    impl Drop for ProcThreadAttrList {
        fn drop(&mut self) {
            unsafe {
                DeleteProcThreadAttributeList(self.ptr);
            }
        }
    }

    fn open_inheritable_file(
        path: &Path,
        desired_access: u32,
        share_mode: u32,
        creation_disposition: u32,
    ) -> anyhow::Result<OwnedHandle> {
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: 1,
        };
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                desired_access,
                share_mode,
                &sa,
                creation_disposition,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(OwnedHandle(handle))
    }

    fn push_windows_arg(cmd: &mut Vec<u16>, arg: &std::ffi::OsStr) {
        let s = arg.to_string_lossy();
        let needs_quotes = s.is_empty() || s.chars().any(|c| matches!(c, ' ' | '\t' | '"'));
        if !needs_quotes {
            cmd.extend(arg.encode_wide());
            return;
        }
        cmd.push(b'"' as u16);
        let mut backslashes = 0usize;
        for unit in arg.encode_wide() {
            if unit == b'\\' as u16 {
                backslashes += 1;
            } else if unit == b'"' as u16 {
                for _ in 0..(backslashes * 2 + 1) {
                    cmd.push(b'\\' as u16);
                }
                backslashes = 0;
                cmd.push(b'"' as u16);
            } else {
                for _ in 0..backslashes {
                    cmd.push(b'\\' as u16);
                }
                backslashes = 0;
                cmd.push(unit);
            }
        }
        for _ in 0..(backslashes * 2) {
            cmd.push(b'\\' as u16);
        }
        cmd.push(b'"' as u16);
    }

    fn build_windows_command_line(args: &[OsString]) -> Vec<u16> {
        let mut cmd = Vec::new();
        for (idx, arg) in args.iter().enumerate() {
            if idx > 0 {
                cmd.push(b' ' as u16);
            }
            push_windows_arg(&mut cmd, arg);
        }
        cmd.push(0);
        cmd
    }

    let exe = std::env::current_exe()?;
    let exe_wide: Vec<u16> = exe
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let mut args: Vec<OsString> = vec![
        exe.as_os_str().to_os_string(),
        "__daemon".into(),
        "--repo".into(),
        repo_root.as_os_str().to_os_string(),
    ];
    if let Ok(ms) = std::env::var("LOOM_IDLE_TIMEOUT_MS")
        && !ms.trim().is_empty()
    {
        args.push("--idle-timeout-ms".into());
        args.push(ms.trim().into());
    }
    let mut cmd_wide = build_windows_command_line(&args);

    let nul_path = Path::new(r"\\.\NUL");
    let stdin_handle = open_inheritable_file(
        nul_path,
        GENERIC_READ,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        OPEN_EXISTING,
    )?;
    let stdout_handle = open_inheritable_file(
        nul_path,
        GENERIC_WRITE,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        OPEN_EXISTING,
    )?;
    let log_path = repo_root.join(".loom").join("daemon.log");
    let stderr_handle = match open_inheritable_file(
        &log_path,
        FILE_APPEND_DATA,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        OPEN_ALWAYS,
    ) {
        Ok(h) => h,
        Err(_) => open_inheritable_file(
            nul_path,
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            OPEN_EXISTING,
        )?,
    };

    let mut handle_list: [HANDLE; 3] = [stdin_handle.0, stdout_handle.0, stderr_handle.0];

    let mut attr_size: usize = 0;
    unsafe {
        InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut attr_size);
    }
    anyhow::ensure!(
        attr_size > 0,
        "InitializeProcThreadAttributeList returned zero size"
    );
    let word_count = attr_size.div_ceil(std::mem::size_of::<usize>());
    let mut attr_buf = vec![0usize; word_count];
    let attr_ptr = attr_buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
    let init_ok = unsafe { InitializeProcThreadAttributeList(attr_ptr, 1, 0, &mut attr_size) };
    if init_ok == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let attr_list = ProcThreadAttrList {
        _buf: attr_buf,
        ptr: attr_ptr,
    };

    let update_ok = unsafe {
        UpdateProcThreadAttribute(
            attr_list.ptr,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            handle_list.as_mut_ptr().cast(),
            std::mem::size_of_val(&handle_list),
            std::ptr::null_mut(),
            std::ptr::null(),
        )
    };
    if update_ok == 0 {
        return Err(std::io::Error::last_os_error().into());
    }

    let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = stdin_handle.0;
    si.StartupInfo.hStdOutput = stdout_handle.0;
    si.StartupInfo.hStdError = stderr_handle.0;
    si.lpAttributeList = attr_list.ptr;

    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let creation_flags = EXTENDED_STARTUPINFO_PRESENT | CREATE_NO_WINDOW;
    let spawn_ok = unsafe {
        CreateProcessW(
            exe_wide.as_ptr(),
            cmd_wide.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            creation_flags,
            std::ptr::null(),
            std::ptr::null(),
            &si.StartupInfo,
            &mut pi,
        )
    };
    if spawn_ok == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    unsafe {
        if !pi.hThread.is_null() {
            CloseHandle(pi.hThread);
        }
    }
    Ok(SpawnedDaemonHandle(pi.hProcess))
}

fn is_retryable_write_error(err: &anyhow::Error) -> bool {
    if let Some(io_err) = err.downcast_ref::<std::io::Error>() {
        if matches!(
            io_err.kind(),
            std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::UnexpectedEof
        ) {
            return true;
        }
        if io_err.raw_os_error() == Some(232) {
            return true;
        }
    }
    false
}

fn is_idempotent_for_retry(method: &RpcMethod) -> bool {
    match method {
        RpcMethod::Ping
        | RpcMethod::Status
        | RpcMethod::ListViewports { .. }
        | RpcMethod::Log { .. }
        | RpcMethod::Init { .. }
        | RpcMethod::Undo { op_seq: Some(_) }
        | RpcMethod::Shutdown
        | RpcMethod::RadarSnapshot { .. }
        | RpcMethod::McpPreflight { .. }
        | RpcMethod::McpPreflightEx { .. } => true,
        RpcMethod::RadarSubscribe { .. } => false,
        RpcMethod::Gc { .. }
        | RpcMethod::Undo { op_seq: None }
        | RpcMethod::SpawnViewport { .. }
        | RpcMethod::TransitionViewport { .. }
        | RpcMethod::RestoreViewportClean { .. }
        | RpcMethod::SealCapsule { .. }
        | RpcMethod::DropViewport { .. }
        | RpcMethod::McpRecordDeadend { .. }
        | RpcMethod::McpRecordDeadendEx { .. }
        | RpcMethod::GitPush { .. }
        | RpcMethod::GitPull { .. } => false,
        _ => false,
    }
}

pub async fn call_rpc(repo_root: &Path, method: RpcMethod) -> anyhow::Result<RpcPayload> {
    let mut eof_retried = false;
    for attempt in 0..8 {
        let mut stream = connect_or_autostart(repo_root).await?;
        let req = RpcRequest {
            id: 1,
            method: method.clone(),
        };
        if let Err(err) = write_frame(&mut stream, &req).await {
            if attempt < 7 && is_retryable_write_error(&err) {
                tokio::time::sleep(Duration::from_millis(40)).await;
                continue;
            }
            return Err(err);
        }
        match read_frame::<_, RpcResponse>(&mut stream).await {
            Ok(Some(resp)) => match resp.result {
                Ok(payload) => return Ok(payload),
                Err(err)
                    if err.code == RpcErrorCode::AlreadyLocked
                        && err.message.contains("shutting down due to idle timeout")
                        && attempt < 7 =>
                {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
                Err(err) => anyhow::bail!("[{:?}] {}", err.code, err.message),
            },
            Ok(None) => {
                if !eof_retried && is_idempotent_for_retry(&req.method) {
                    eof_retried = true;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    continue;
                }
                if attempt < 7 && !sock_descriptor_path(repo_root).exists() {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
                anyhow::bail!("daemon closed IPC connection before responding");
            }
            Err(err) => return Err(err),
        }
    }
    anyhow::bail!("daemon closed IPC connection before responding")
}

pub struct RadarSubscription {
    reader: ReadHalf<Stream>,
    writer: WriteHalf<Stream>,
    next_req_id: u64,
}

impl RadarSubscription {
    pub async fn next_event(&mut self) -> anyhow::Result<Option<RadarSnapshot>> {
        loop {
            match read_frame::<_, RpcResponse>(&mut self.reader).await? {
                Some(resp) => match resp.result {
                    Ok(RpcPayload::RadarEvent(ev)) => return Ok(Some(ev.snapshot)),
                    Ok(RpcPayload::RadarSnapshot(snap)) => return Ok(Some(snap)),
                    Ok(_) => continue,
                    Err(err) => anyhow::bail!("[{:?}] {}", err.code, err.message),
                },
                None => return Ok(None),
            }
        }
    }

    pub async fn request_refresh(&mut self, viewport_id: Option<String>) -> anyhow::Result<()> {
        let req_id = self.next_req_id;
        self.next_req_id += 1;
        let req = RpcRequest {
            id: req_id,
            method: RpcMethod::RadarSnapshot { viewport_id },
        };
        write_frame(&mut self.writer, &req).await
    }
}

pub async fn subscribe_radar(
    repo_root: &Path,
    viewport_id: Option<String>,
) -> anyhow::Result<(RadarSnapshot, RadarSubscription)> {
    for attempt in 0..2 {
        let mut stream = connect_or_autostart(repo_root).await?;
        let req = RpcRequest {
            id: 1,
            method: RpcMethod::RadarSubscribe {
                viewport_id: viewport_id.clone(),
            },
        };
        if let Err(err) = write_frame(&mut stream, &req).await {
            if attempt == 0 && is_retryable_write_error(&err) {
                tokio::time::sleep(Duration::from_millis(20)).await;
                continue;
            }
            return Err(err);
        }
        match read_frame::<_, RpcResponse>(&mut stream).await {
            Ok(Some(resp)) => match resp.result {
                Ok(RpcPayload::RadarSnapshot(snap)) => {
                    let (reader, writer) = tokio::io::split(stream);
                    return Ok((
                        snap,
                        RadarSubscription {
                            reader,
                            writer,
                            next_req_id: 2,
                        },
                    ));
                }
                Ok(other) => anyhow::bail!("unexpected RadarSubscribe response: {other:?}"),
                Err(err) => anyhow::bail!("[{:?}] {}", err.code, err.message),
            },
            Ok(None) => {
                if attempt == 0 {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    continue;
                }
                anyhow::bail!("daemon closed IPC connection before RadarSubscribe handshake");
            }
            Err(err) => return Err(err),
        }
    }
    anyhow::bail!("daemon closed IPC connection before RadarSubscribe handshake")
}

#[cfg(test)]
mod tests {
    use super::*;
    use interprocess::local_socket::ListenerOptions;
    use interprocess::local_socket::traits::tokio::Listener as _;
    use loom_core::rpc::{
        DaemonStatus, PROTOCOL_VERSION, SocketDescriptor, UndoResult, write_sock_descriptor_atomic,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;
    use tokio::io::AsyncWriteExt;

    async fn bind_test_listener(
        repo_root: &Path,
    ) -> interprocess::local_socket::tokio::Listener {
        #[cfg(windows)]
        {
            use interprocess::local_socket::{GenericNamespaced, ToNsName};
            use loom_core::rpc::pipe_full_path;
            let short = pipe_short_name(repo_root).unwrap();
            let full = pipe_full_path(repo_root).unwrap();
            let ns_name = short.as_str().to_ns_name::<GenericNamespaced>().unwrap();
            let listener = ListenerOptions::new().name(ns_name).create_tokio().unwrap();
            let desc = SocketDescriptor {
                transport: "named_pipe".into(),
                endpoint: full,
                pid: std::process::id(),
                protocol_version: PROTOCOL_VERSION,
            };
            write_sock_descriptor_atomic(repo_root, &desc).unwrap();
            listener
        }

        #[cfg(unix)]
        {
            use interprocess::local_socket::{GenericFilePath, ToFsName};
            let sock_path = sock_descriptor_path(repo_root);
            std::fs::create_dir_all(sock_path.parent().unwrap()).unwrap();
            let fs_name = sock_path.as_path().to_fs_name::<GenericFilePath>().unwrap();
            ListenerOptions::new().name(fs_name).create_tokio().unwrap()
        }
    }

    #[test]
    fn discover_repo_root_for_init_creates_loom_dir_without_precreating_config() {
        let tmp = TempDir::new().unwrap();
        let root = discover_repo_root(Some(tmp.path()), true).unwrap();
        assert!(root.join(".loom").is_dir());
        assert!(!root.join(".loom/config.toml").exists());
    }

    #[test]
    fn retryable_write_error_matches_closing_pipe_variants() {
        for kind in [
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::UnexpectedEof,
        ] {
            let err: anyhow::Error = std::io::Error::from(kind).into();
            assert!(is_retryable_write_error(&err));
        }
        let err_232: anyhow::Error = std::io::Error::from_raw_os_error(232).into();
        assert!(is_retryable_write_error(&err_232));

        let non_retryable: anyhow::Error = std::io::Error::from(std::io::ErrorKind::NotFound).into();
        assert!(!is_retryable_write_error(&non_retryable));
    }

    #[test]
    fn idempotent_for_retry_matrix() {
        assert!(is_idempotent_for_retry(&RpcMethod::Ping));
        assert!(is_idempotent_for_retry(&RpcMethod::Status));
        assert!(is_idempotent_for_retry(&RpcMethod::ListViewports {
            include_gc: false
        }));
        assert!(is_idempotent_for_retry(&RpcMethod::Log {
            capsules_only: false,
            symbol: None,
            limit: None,
        }));
        assert!(is_idempotent_for_retry(&RpcMethod::Init {
            colocate_git: false
        }));
        assert!(is_idempotent_for_retry(&RpcMethod::Undo {
            op_seq: Some(1)
        }));
        assert!(is_idempotent_for_retry(&RpcMethod::Shutdown));
        assert!(is_idempotent_for_retry(&RpcMethod::RadarSnapshot {
            viewport_id: None
        }));
        assert!(is_idempotent_for_retry(&RpcMethod::McpPreflight {
            viewport_id: "vp".into(),
            symbols: vec![],
        }));

        assert!(!is_idempotent_for_retry(&RpcMethod::Gc {
            prune_viewports: false
        }));
        assert!(!is_idempotent_for_retry(&RpcMethod::Undo { op_seq: None }));
        assert!(!is_idempotent_for_retry(&RpcMethod::SpawnViewport {
            name: "a".into(),
            intent: "b".into(),
            cmd: vec![],
        }));
        assert!(!is_idempotent_for_retry(&RpcMethod::TransitionViewport {
            viewport_id: "vp".into(),
            target_state: loom_core::model::ViewportState::Sealed,
            pid: None,
        }));
        assert!(!is_idempotent_for_retry(&RpcMethod::RestoreViewportClean {
            viewport_id: "vp".into(),
        }));
        assert!(!is_idempotent_for_retry(&RpcMethod::SealCapsule {
            viewport_id: "vp".into(),
            verify_cmd: None,
            summary: None,
        }));
        assert!(!is_idempotent_for_retry(&RpcMethod::DropViewport {
            viewport_id: "vp".into(),
            reason: "r".into(),
            kind: None,
        }));
        assert!(!is_idempotent_for_retry(&RpcMethod::McpRecordDeadend {
            viewport_id: "vp".into(),
            touched_symbols: vec![],
            kind: "k".into(),
            lesson_summary: "l".into(),
        }));
        assert!(!is_idempotent_for_retry(&RpcMethod::GitPush {
            remote: None,
            branch: None,
        }));
        assert!(!is_idempotent_for_retry(&RpcMethod::GitPull {
            remote: None,
            branch: None,
        }));
    }

    #[tokio::test]
    async fn call_rpc_retries_once_when_first_connection_closes_before_response() {
        let tmp = TempDir::new().unwrap();
        let repo_root = canonical_repo_root(tmp.path()).unwrap();
        let listener = bind_test_listener(&repo_root).await;

        let server = tokio::spawn(async move {
            // First connection: close immediately without replying (simulates closing-pipe race)
            let s1 = listener.accept().await.unwrap();
            drop(s1);

            // Second connection: read request and return Status
            let mut s2 = listener.accept().await.unwrap();
            let req: RpcRequest = read_frame(&mut s2).await.unwrap().unwrap();
            let resp = RpcResponse {
                id: req.id,
                result: Ok(RpcPayload::Status(DaemonStatus {
                    pid: 4242,
                    ..DaemonStatus::default()
                })),
            };
            write_frame(&mut s2, &resp).await.unwrap();

            // Third connection (for Undo { op_seq: Some(1) }): close immediately without replying
            let s3 = listener.accept().await.unwrap();
            drop(s3);

            // Fourth connection: read Undo { op_seq: Some(1) } and reply
            let mut s4 = listener.accept().await.unwrap();
            let req2: RpcRequest = read_frame(&mut s4).await.unwrap().unwrap();
            let resp2 = RpcResponse {
                id: req2.id,
                result: Ok(RpcPayload::Undo(UndoResult {
                    undone_op_seq: 1,
                    compensating_op_seq: 2,
                    summary: "undone op 1".into(),
                })),
            };
            write_frame(&mut s4, &resp2).await.unwrap();
        });

        let payload = call_rpc(&repo_root, RpcMethod::Status).await.unwrap();
        assert_eq!(
            payload,
            RpcPayload::Status(DaemonStatus {
                pid: 4242,
                ..DaemonStatus::default()
            })
        );

        let undo_payload = call_rpc(&repo_root, RpcMethod::Undo { op_seq: Some(1) })
            .await
            .unwrap();
        assert_eq!(
            undo_payload,
            RpcPayload::Undo(UndoResult {
                undone_op_seq: 1,
                compensating_op_seq: 2,
                summary: "undone op 1".into(),
            })
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn call_rpc_does_not_retry_after_partial_response_bytes() {
        let tmp = TempDir::new().unwrap();
        let repo_root = canonical_repo_root(tmp.path()).unwrap();
        let listener = bind_test_listener(&repo_root).await;

        let accept_count = Arc::new(AtomicUsize::new(0));
        let accept_count_srv = Arc::clone(&accept_count);
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        let server = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accepted = listener.accept() => {
                        let mut stream = accepted.unwrap();
                        accept_count_srv.fetch_add(1, Ordering::SeqCst);
                        let _req: Option<RpcRequest> = read_frame(&mut stream).await.unwrap();
                        // Send 1 byte of the 4-byte length prefix and close before completing frame.
                        stream.write_all(&[0x04]).await.unwrap();
                        stream.flush().await.unwrap();
                        drop(stream);
                    }
                }
            }
        });

        let res = call_rpc(&repo_root, RpcMethod::Status).await;
        let _ = shutdown_tx.send(());
        server.await.unwrap();

        assert!(res.is_err(), "truncated response frame must return an error");
        assert_eq!(
            accept_count.load(Ordering::SeqCst),
            1,
            "call_rpc must not retry after >= 1 response byte was received"
        );
    }

    #[tokio::test]
    async fn call_rpc_does_not_retry_non_idempotent_undo_latest_on_eof() {
        let tmp = TempDir::new().unwrap();
        let repo_root = canonical_repo_root(tmp.path()).unwrap();
        let listener = bind_test_listener(&repo_root).await;

        let accept_count = Arc::new(AtomicUsize::new(0));
        let accept_count_srv = Arc::clone(&accept_count);
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        let server = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accepted = listener.accept() => {
                        let mut stream = accepted.unwrap();
                        accept_count_srv.fetch_add(1, Ordering::SeqCst);
                        let req: RpcRequest = read_frame(&mut stream).await.unwrap().unwrap();
                        assert!(matches!(req.method, RpcMethod::Undo { op_seq: None }));
                        // Close cleanly (0 response bytes) after receiving the non-idempotent request.
                        drop(stream);
                    }
                }
            }
        });

        let res = call_rpc(&repo_root, RpcMethod::Undo { op_seq: None }).await;
        let _ = shutdown_tx.send(());
        server.await.unwrap();

        assert!(
            res.is_err(),
            "non-idempotent Undo {{ op_seq: None }} must fail on clean EOF"
        );
        assert_eq!(
            accept_count.load(Ordering::SeqCst),
            1,
            "call_rpc must not retry non-idempotent Undo {{ op_seq: None }}"
        );
    }

    #[tokio::test]
    async fn call_rpc_does_not_retry_non_idempotent_gc_and_spawn_on_eof() {
        for method in [
            RpcMethod::Gc {
                prune_viewports: true,
            },
            RpcMethod::SpawnViewport {
                name: "vp-test".into(),
                intent: "test".into(),
                cmd: vec![],
            },
        ] {
            let tmp = TempDir::new().unwrap();
            let repo_root = canonical_repo_root(tmp.path()).unwrap();
            let listener = bind_test_listener(&repo_root).await;

            let accept_count = Arc::new(AtomicUsize::new(0));
            let accept_count_srv = Arc::clone(&accept_count);
            let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

            let server = tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = &mut shutdown_rx => break,
                        accepted = listener.accept() => {
                            let mut stream = accepted.unwrap();
                            accept_count_srv.fetch_add(1, Ordering::SeqCst);
                            let _req: RpcRequest = read_frame(&mut stream).await.unwrap().unwrap();
                            drop(stream);
                        }
                    }
                }
            });

            let res = call_rpc(&repo_root, method).await;
            let _ = shutdown_tx.send(());
            server.await.unwrap();

            assert!(res.is_err(), "non-idempotent RPC must fail on clean EOF");
            assert_eq!(
                accept_count.load(Ordering::SeqCst),
                1,
                "call_rpc must not retry non-idempotent Gc/SpawnViewport on clean EOF"
            );
        }
    }

    fn spawn_exit_code_handle(code: i32) -> SpawnedDaemonHandle {
        #[cfg(windows)]
        {
            use std::os::windows::io::IntoRawHandle;
            let child = std::process::Command::new("cmd")
                .args(["/C", &format!("exit {code}")])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            SpawnedDaemonHandle(child.into_raw_handle() as windows_sys::Win32::Foundation::HANDLE)
        }
        #[cfg(unix)]
        {
            let child = std::process::Command::new("sh")
                .args(["-c", &format!("exit {code}")])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            SpawnedDaemonHandle(child)
        }
    }

    #[tokio::test]
    async fn connect_or_autostart_fails_fast_on_nonzero_daemon_exit() {
        let tmp = TempDir::new().unwrap();
        let repo_root = canonical_repo_root(tmp.path()).unwrap();
        std::fs::create_dir_all(repo_root.join(".loom")).unwrap();
        std::fs::write(
            repo_root.join(".loom").join("daemon.log"),
            "fatal: simulated startup error\n",
        )
        .unwrap();

        let started = Instant::now();
        let err = connect_or_autostart_with(&repo_root, |_| Ok(spawn_exit_code_handle(7)))
            .await
            .expect_err("non-zero daemon exit must fail");
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(2),
            "expected fast-fail under 2s, took {elapsed:?}"
        );
        let msg = format!("{err:#}");
        assert!(msg.contains("exit code 7"), "unexpected error: {msg}");
        assert!(
            msg.contains("simulated startup error"),
            "expected daemon.log tail in error: {msg}"
        );
    }

    #[tokio::test]
    async fn connect_or_autostart_converges_when_spawned_daemon_exits_zero() {
        let tmp = TempDir::new().unwrap();
        let repo_root = canonical_repo_root(tmp.path()).unwrap();
        let repo_srv = repo_root.clone();

        let server = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            let listener = bind_test_listener(&repo_srv).await;
            let _stream = listener.accept().await.unwrap();
        });

        let stream = connect_or_autostart_with(&repo_root, |_| Ok(spawn_exit_code_handle(0)))
            .await
            .expect("exit 0 concurrent autostart must converge to running listener");
        drop(stream);
        server.await.unwrap();
    }
}
