use crate::win32;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::{ErrorKind, Read, Write};
use std::path::Path;
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SocketDescriptor {
    pub transport: String,
    pub endpoint: String,
    pub pid: u32,
    pub protocol_version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    pub id: u64,
    pub method: RpcMethod,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RpcMethod {
    Shutdown,
}

pub fn encode_shutdown_frame(id: u64) -> anyhow::Result<Vec<u8>> {
    let req = RpcRequest {
        id,
        method: RpcMethod::Shutdown,
    };
    let payload = rmp_serde::to_vec_named(&req)?;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

pub fn read_sock_descriptor(repo_root: &Path) -> anyhow::Result<Option<SocketDescriptor>> {
    let sock_path = repo_root.join(".loom").join("daemon.sock");
    if !sock_path.is_file() {
        return Ok(None);
    }
    let bytes = std::fs::read(&sock_path)
        .with_context(|| format!("read {}", sock_path.display()))?;
    let desc: SocketDescriptor = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse {}", sock_path.display()))?;
    Ok(Some(desc))
}

pub fn read_lock_pid(repo_root: &Path) -> Option<u32> {
    let lock_path = repo_root.join(".loom").join("daemon.lock");
    let content = std::fs::read_to_string(lock_path).ok()?;
    content.lines().next()?.trim().parse::<u32>().ok()
}

pub fn shutdown_daemon(repo_root: &Path, wait_timeout_ms: u32) -> anyhow::Result<Option<u32>> {
    let Some(desc) = read_sock_descriptor(repo_root)? else {
        return Ok(read_lock_pid(repo_root));
    };

    let frame = encode_shutdown_frame(1)?;
    let mut opened = None;
    for attempt in 0..10 {
        match OpenOptions::new()
            .read(true)
            .write(true)
            .open(&desc.endpoint)
        {
            Ok(pipe) => {
                opened = Some(pipe);
                break;
            }
            Err(err) if err.kind() == ErrorKind::NotFound => break,
            Err(err) => {
                if attempt == 9 {
                    return Err(err)
                        .with_context(|| format!("open Named Pipe {}", desc.endpoint));
                }
                thread::sleep(Duration::from_millis(25));
            }
        }
    }

    if let Some(mut pipe) = opened {
        pipe.write_all(&frame)
            .with_context(|| format!("write Shutdown frame to {}", desc.endpoint))?;
        let _ = pipe.flush();

        // Drain the ShutdownAck frame if available before the daemon closes the pipe.
        let mut len_buf = [0u8; 4];
        if pipe.read_exact(&mut len_buf).is_ok() {
            let resp_len = u32::from_le_bytes(len_buf) as usize;
            if resp_len > 0 && resp_len <= 64 * 1024 {
                let mut resp_buf = vec![0u8; resp_len];
                let _ = pipe.read_exact(&mut resp_buf);
            }
        }
    }

    let _ = win32::wait_pid_exit(desc.pid, wait_timeout_ms);
    Ok(Some(desc.pid))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_frame_wire_encoding_matches_rpc_contract() {
        let frame = encode_shutdown_frame(42).expect("encode shutdown frame");
        assert!(frame.len() > 4);
        let len = u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
        assert_eq!(len, frame.len() - 4);

        let req: RpcRequest = rmp_serde::from_slice(&frame[4..]).expect("decode RpcRequest");
        assert_eq!(req.id, 42);
        assert!(matches!(req.method, RpcMethod::Shutdown));
    }
}
