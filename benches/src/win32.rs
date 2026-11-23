use anyhow::Context;
use serde::Serialize;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Copy, Serialize)]
pub struct ProcessMemorySample {
    pub working_set_bytes: u64,
    pub private_bytes: u64,
    pub peak_working_set_bytes: u64,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct FileLinkInfo {
    pub n_links: u32,
    pub file_index: u64,
}

#[cfg(windows)]
mod imp {
    use super::{FileLinkInfo, ProcessMemorySample};
    use anyhow::Context;
    use std::fs::File;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use std::process::Command;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
        PROCESS_VM_READ, TerminateProcess, WaitForSingleObject,
    };

    struct ProcessHandle(HANDLE);

    impl Drop for ProcessHandle {
        fn drop(&mut self) {
            if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
                unsafe {
                    let _ = CloseHandle(self.0);
                }
            }
        }
    }

    fn open_process(access: u32, pid: u32) -> anyhow::Result<ProcessHandle> {
        let handle = unsafe { OpenProcess(access, 0, pid) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("OpenProcess(pid={pid}) failed"));
        }
        Ok(ProcessHandle(handle))
    }

    pub fn sample_process_memory(pid: u32) -> anyhow::Result<ProcessMemorySample> {
        let handle = open_process(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, pid)?;
        let mut pmc: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
        pmc.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        let ok = unsafe {
            K32GetProcessMemoryInfo(
                handle.0,
                (&raw mut pmc).cast::<PROCESS_MEMORY_COUNTERS>(),
                pmc.cb,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("K32GetProcessMemoryInfo(pid={pid}) failed"));
        }
        Ok(ProcessMemorySample {
            working_set_bytes: pmc.WorkingSetSize as u64,
            private_bytes: pmc.PrivateUsage as u64,
            peak_working_set_bytes: pmc.PeakWorkingSetSize as u64,
        })
    }

    pub fn file_link_info(path: &Path) -> anyhow::Result<FileLinkInfo> {
        let file = File::open(path)
            .with_context(|| format!("open {} for link count check", path.display()))?;
        let raw_handle = file.as_raw_handle() as HANDLE;
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        let ok = unsafe { GetFileInformationByHandle(raw_handle, &mut info) };
        if ok == 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("GetFileInformationByHandle({}) failed", path.display()));
        }
        let file_index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
        Ok(FileLinkInfo {
            n_links: info.nNumberOfLinks,
            file_index,
        })
    }

    pub fn wait_pid_exit(pid: u32, timeout_ms: u32) -> bool {
        let Ok(handle) = open_process(PROCESS_SYNCHRONIZE, pid) else {
            return true;
        };
        let wait = unsafe { WaitForSingleObject(handle.0, timeout_ms) };
        wait == WAIT_OBJECT_0
    }

    pub fn terminate_pid(pid: u32) {
        if let Ok(handle) = open_process(PROCESS_SYNCHRONIZE | PROCESS_TERMINATE, pid) {
            let wait = unsafe { WaitForSingleObject(handle.0, 0) };
            if wait != WAIT_OBJECT_0 {
                unsafe {
                    let _ = TerminateProcess(handle.0, 1);
                    let _ = WaitForSingleObject(handle.0, 3000);
                }
            }
        }
        let _ = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .output();
    }
}

#[cfg(not(windows))]
mod imp {
    use super::{FileLinkInfo, ProcessMemorySample};
    use std::path::Path;

    pub fn sample_process_memory(_pid: u32) -> anyhow::Result<ProcessMemorySample> {
        anyhow::bail!("Win32 process memory sampling is only supported on Windows")
    }

    pub fn file_link_info(_path: &Path) -> anyhow::Result<FileLinkInfo> {
        anyhow::bail!("NTFS link inspection is only supported on Windows")
    }

    pub fn wait_pid_exit(_pid: u32, _timeout_ms: u32) -> bool {
        true
    }

    pub fn terminate_pid(_pid: u32) {}
}

pub use imp::{file_link_info, sample_process_memory, terminate_pid, wait_pid_exit};

pub fn query_host_summary() -> (String, String, u64) {
    let os_ver = Command::new("cmd")
        .args(["/C", "ver"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| std::env::consts::OS.to_string());

    let ps_out = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "$c = Get-CimInstance Win32_Processor | Select-Object -First 1; $m = Get-CimInstance Win32_ComputerSystem; \"$($c.Name)|$($m.TotalPhysicalMemory)\"",
        ])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    let (cpu_name, total_ram_bytes) = if let Some((cpu, mem)) = ps_out.split_once('|') {
        (
            cpu.trim().to_string(),
            mem.trim().parse::<u64>().unwrap_or(0),
        )
    } else {
        ("Unknown CPU".to_string(), 0)
    };

    (os_ver, cpu_name, total_ram_bytes)
}

pub fn run_git(cwd: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("failed to invoke git {:?}", args))?;
    if !output.status.success() {
        anyhow::bail!(
            "git {:?} failed (exit {:?}): {}",
            args,
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
