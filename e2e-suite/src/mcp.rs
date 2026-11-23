use std::collections::{HashMap, VecDeque};
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::command::{spawn_drain, terminate_pid};
use crate::{HarnessError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpFraming {
    NewlineDelimited,
    ContentLength,
}

enum ReaderEvent {
    Message(Value),
    Eof,
    Error(String),
}

const MAX_MCP_FRAME_BYTES: usize = 16 * 1024 * 1024;

pub fn read_jsonrpc_frame<R: BufRead>(reader: &mut R) -> io::Result<Option<(McpFraming, Value)>> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim().trim_start_matches('\u{FEFF}').trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            let val: Value = serde_json::from_str(trimmed)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            return Ok(Some((McpFraming::NewlineDelimited, val)));
        }

        // LSP-style Content-Length framing
        let mut content_length: Option<usize> = parse_header_line(trimmed)?;

        // Read remaining headers until blank line
        loop {
            line.clear();
            let hn = reader.read_line(&mut line)?;
            if hn == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "EOF inside MCP Content-Length headers",
                ));
            }
            let htrim = line.trim();
            if htrim.is_empty() {
                break;
            }
            if let Some(next_len) = parse_header_line(htrim)? {
                match content_length {
                    Some(prev_len) if prev_len != next_len => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "conflicting Content-Length headers: {prev_len} vs {next_len}"
                            ),
                        ));
                    }
                    None => content_length = Some(next_len),
                    Some(_) => {}
                }
            }
        }

        let len = content_length.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "missing Content-Length header")
        })?;
        if len > MAX_MCP_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "MCP Content-Length {len} exceeds maximum allowed {MAX_MCP_FRAME_BYTES} bytes"
                ),
            ));
        }
        let mut body = vec![0u8; len];
        reader.read_exact(&mut body)?;
        let val: Value = serde_json::from_slice(&body)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        return Ok(Some((McpFraming::ContentLength, val)));
    }
}

fn parse_header_line(line: &str) -> io::Result<Option<usize>> {
    let (name, value) = line.split_once(':').ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected MCP frame/header line (missing ':'): {line}"),
        )
    })?;
    let name = name.trim();
    if name.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("empty MCP header name in line: {line}"),
        ));
    }
    if name.eq_ignore_ascii_case("content-length") {
        let len = value.trim().parse::<usize>().map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid Content-Length header '{}': {e}", value.trim()),
            )
        })?;
        Ok(Some(len))
    } else {
        Ok(None)
    }
}

pub fn write_jsonrpc_frame<W: Write>(
    writer: &mut W,
    framing: McpFraming,
    msg: &Value,
) -> io::Result<()> {
    let bytes = serde_json::to_vec(msg)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    match framing {
        McpFraming::NewlineDelimited => {
            writer.write_all(&bytes)?;
            writer.write_all(b"\n")?;
        }
        McpFraming::ContentLength => {
            write!(writer, "Content-Length: {}\r\n\r\n", bytes.len())?;
            writer.write_all(&bytes)?;
        }
    }
    writer.flush()
}

pub struct McpClient {
    writer: Option<Box<dyn Write + Send>>,
    rx: mpsc::Receiver<ReaderEvent>,
    framing: McpFraming,
    next_id: u64,
    default_timeout: Duration,
    pending_responses: HashMap<u64, Value>,
    pending_notifications: VecDeque<Value>,
    stderr_buf: Arc<Mutex<Vec<u8>>>,
    child: Arc<Mutex<Option<Child>>>,
    threads: Vec<JoinHandle<()>>,
}

impl McpClient {
    pub(crate) fn spawn(bin: &Path, cmd: &mut Command) -> Result<(Self, Arc<Mutex<Option<Child>>>)> {
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(|source| HarnessError::ProcessSpawn {
            program: bin.to_path_buf(),
            args: vec!["mcp".into(), "serve".into()],
            source,
        })?;

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let stderr_buf = Arc::new(Mutex::new(Vec::new()));
        let stderr_handle = spawn_drain(stderr, Arc::clone(&stderr_buf));

        let shared_child = Arc::new(Mutex::new(Some(child)));
        let mut client = Self::from_reader_writer(
            Box::new(BufReader::new(stdout)),
            Box::new(stdin),
            McpFraming::NewlineDelimited,
        );
        client.stderr_buf = stderr_buf;
        client.child = Arc::clone(&shared_child);
        client.threads.push(stderr_handle);
        Ok((client, shared_child))
    }

    /// Testable constructor over any `BufRead + Send` and `Write + Send` (e.g. `std::io::pipe()`).
    pub fn from_reader_writer(
        mut reader: Box<dyn BufRead + Send>,
        writer: Box<dyn Write + Send>,
        framing: McpFraming,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let reader_thread = std::thread::spawn(move || loop {
            match read_jsonrpc_frame(&mut reader) {
                Ok(Some((_frame_kind, val))) => {
                    if tx.send(ReaderEvent::Message(val)).is_err() {
                        break;
                    }
                }
                Ok(None) => {
                    let _ = tx.send(ReaderEvent::Eof);
                    break;
                }
                Err(e) => {
                    let _ = tx.send(ReaderEvent::Error(e.to_string()));
                    break;
                }
            }
        });

        Self {
            writer: Some(writer),
            rx,
            framing,
            next_id: 1,
            default_timeout: Duration::from_secs(10),
            pending_responses: HashMap::new(),
            pending_notifications: VecDeque::new(),
            stderr_buf: Arc::new(Mutex::new(Vec::new())),
            child: Arc::new(Mutex::new(None)),
            threads: vec![reader_thread],
        }
    }

    pub fn with_framing(mut self, framing: McpFraming) -> Self {
        self.framing = framing;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.default_timeout = timeout;
        self
    }

    pub fn stderr_output(&self) -> String {
        let g = self.stderr_buf.lock().unwrap_or_else(|e| e.into_inner());
        String::from_utf8_lossy(&g).into_owned()
    }

    pub fn send_raw(&mut self, msg: &Value) -> Result<()> {
        let w = self
            .writer
            .as_mut()
            .ok_or_else(|| HarnessError::Protocol("MCP stdin already closed".into()))?;
        write_jsonrpc_frame(w, self.framing, msg)?;
        Ok(())
    }

    pub fn notify(&mut self, method: &str, params: Option<Value>) -> Result<()> {
        let mut msg = json!({ "jsonrpc": "2.0", "method": method });
        if let Some(p) = params {
            msg.as_object_mut().unwrap().insert("params".into(), p);
        }
        self.send_raw(&msg)
    }

    pub fn request(&mut self, method: &str, params: Option<Value>, timeout: Duration) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let mut msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
        });
        if let Some(p) = params {
            msg.as_object_mut().unwrap().insert("params".into(), p);
        }
        self.send_raw(&msg)?;
        let resp = self.wait_for_response(id, timeout)?;
        if let Some(err) = resp.get("error") {
            return Err(HarnessError::JsonRpc {
                code: err.get("code").and_then(Value::as_i64).unwrap_or(-1),
                message: err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown JSON-RPC error")
                    .to_string(),
                data: err.get("data").cloned(),
            });
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    pub fn initialize(&mut self) -> Result<Value> {
        self.initialize_with_version("2024-11-05")
    }

    pub fn initialize_with_version(&mut self, protocol_version: &str) -> Result<Value> {
        self.request(
            "initialize",
            Some(json!({
                "protocolVersion": protocol_version,
                "capabilities": {},
                "clientInfo": { "name": "loom-e2e-harness", "version": "0.1.0" }
            })),
            self.default_timeout,
        )
    }

    pub fn send_initialized(&mut self) -> Result<()> {
        self.notify("notifications/initialized", None)
    }

    pub fn handshake(&mut self) -> Result<Value> {
        let res = self.initialize()?;
        self.send_initialized()?;
        Ok(res)
    }

    pub fn tools_list(&mut self) -> Result<Value> {
        self.request("tools/list", Some(json!({})), self.default_timeout)
    }

    pub fn tools_call(&mut self, name: &str, arguments: Value) -> Result<Value> {
        self.request(
            "tools/call",
            Some(json!({ "name": name, "arguments": arguments })),
            self.default_timeout,
        )
    }

    pub fn read_until_notification(&mut self, method: &str, timeout: Duration) -> Result<Value> {
        self.read_notification_matching(timeout, |msg| {
            msg.get("method").and_then(Value::as_str) == Some(method)
        })
    }

    pub fn read_notification_matching<F>(
        &mut self,
        timeout: Duration,
        mut predicate: F,
    ) -> Result<Value>
    where
        F: FnMut(&Value) -> bool,
    {
        if let Some(pos) = self.pending_notifications.iter().position(|n| predicate(n)) {
            return Ok(self.pending_notifications.remove(pos).unwrap());
        }

        let deadline = Instant::now() + timeout;
        loop {
            loop {
                match self.rx.try_recv() {
                    Ok(ReaderEvent::Message(msg)) => {
                        if let Some(resp_id) = response_id(&msg) {
                            self.pending_responses.insert(resp_id, msg);
                        } else if predicate(&msg) {
                            return Ok(msg);
                        } else {
                            self.pending_notifications.push_back(msg);
                        }
                    }
                    Ok(ReaderEvent::Eof) => {
                        return Err(HarnessError::Protocol(format!(
                            "MCP server closed stdout unexpectedly; stderr={}",
                            self.stderr_output()
                        )));
                    }
                    Ok(ReaderEvent::Error(e)) => {
                        return Err(HarnessError::Protocol(format!(
                            "MCP reader error: {e}; stderr={}",
                            self.stderr_output()
                        )));
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        return Err(HarnessError::Protocol(
                            "MCP reader channel disconnected".into(),
                        ));
                    }
                }
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(HarnessError::Timeout {
                    context: "MCP notification".into(),
                    details: format!(
                        "buffered_notifications={}, stderr={}",
                        self.pending_notifications.len(),
                        self.stderr_output()
                    ),
                });
            }

            match self.rx.recv_timeout(remaining) {
                Ok(ReaderEvent::Message(msg)) => {
                    if let Some(resp_id) = response_id(&msg) {
                        self.pending_responses.insert(resp_id, msg);
                    } else if predicate(&msg) {
                        return Ok(msg);
                    } else {
                        self.pending_notifications.push_back(msg);
                    }
                }
                Ok(ReaderEvent::Eof) => {
                    return Err(HarnessError::Protocol(format!(
                        "MCP server closed stdout unexpectedly; stderr={}",
                        self.stderr_output()
                    )));
                }
                Ok(ReaderEvent::Error(e)) => {
                    return Err(HarnessError::Protocol(format!(
                        "MCP reader error: {e}; stderr={}",
                        self.stderr_output()
                    )));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(HarnessError::Timeout {
                        context: "MCP notification".into(),
                        details: format!(
                            "buffered_notifications={}, stderr={}",
                            self.pending_notifications.len(),
                            self.stderr_output()
                        ),
                    });
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(HarnessError::Protocol(
                        "MCP reader channel disconnected".into(),
                    ));
                }
            }
        }
    }

    fn wait_for_response(&mut self, id: u64, timeout: Duration) -> Result<Value> {
        if let Some(resp) = self.pending_responses.remove(&id) {
            return Ok(resp);
        }

        let deadline = Instant::now() + timeout;
        loop {
            loop {
                match self.rx.try_recv() {
                    Ok(ReaderEvent::Message(msg)) => {
                        if let Some(resp_id) = response_id(&msg) {
                            if resp_id == id {
                                return Ok(msg);
                            }
                            self.pending_responses.insert(resp_id, msg);
                        } else if is_null_id_error(&msg) {
                            return Ok(msg);
                        } else {
                            self.pending_notifications.push_back(msg);
                        }
                    }
                    Ok(ReaderEvent::Eof) => {
                        return Err(HarnessError::Protocol(format!(
                            "MCP server closed stdout before response id={id}; stderr={}",
                            self.stderr_output()
                        )));
                    }
                    Ok(ReaderEvent::Error(e)) => {
                        return Err(HarnessError::Protocol(format!(
                            "MCP reader error: {e}; stderr={}",
                            self.stderr_output()
                        )));
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        return Err(HarnessError::Protocol(
                            "MCP reader channel disconnected".into(),
                        ));
                    }
                }
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(HarnessError::Timeout {
                    context: format!("MCP response id={id}"),
                    details: format!("stderr={}", self.stderr_output()),
                });
            }

            match self.rx.recv_timeout(remaining) {
                Ok(ReaderEvent::Message(msg)) => {
                    if let Some(resp_id) = response_id(&msg) {
                        if resp_id == id {
                            return Ok(msg);
                        }
                        self.pending_responses.insert(resp_id, msg);
                    } else if is_null_id_error(&msg) {
                        return Ok(msg);
                    } else {
                        self.pending_notifications.push_back(msg);
                    }
                }
                Ok(ReaderEvent::Eof) => {
                    return Err(HarnessError::Protocol(format!(
                        "MCP server closed stdout before response id={id}; stderr={}",
                        self.stderr_output()
                    )));
                }
                Ok(ReaderEvent::Error(e)) => {
                    return Err(HarnessError::Protocol(format!(
                        "MCP reader error: {e}; stderr={}",
                        self.stderr_output()
                    )));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(HarnessError::Timeout {
                        context: format!("MCP response id={id}"),
                        details: format!("stderr={}", self.stderr_output()),
                    });
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(HarnessError::Protocol(
                        "MCP reader channel disconnected".into(),
                    ));
                }
            }
        }
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        // Close stdin first so server exits cleanly on EOF if cooperative
        self.writer = None;
        let had_child = {
            let mut guard = self.child.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(mut child) = guard.take() {
                let deadline = Instant::now() + Duration::from_millis(200);
                let exited = loop {
                    match child.try_wait() {
                        Ok(Some(_)) => break true,
                        Ok(None) if Instant::now() < deadline => {
                            std::thread::sleep(Duration::from_millis(15));
                        }
                        _ => break false,
                    }
                };
                if !exited {
                    terminate_pid(child.id());
                    let _ = child.kill();
                    let _ = child.wait();
                }
                true
            } else {
                false
            }
        };

        // Bound thread join wait so open pipe writers (from_reader_writer) or
        // inherited grandchild pipe handles never deadlock Drop.
        let grace = if had_child {
            Duration::from_millis(150)
        } else {
            Duration::from_millis(15)
        };
        let join_deadline = Instant::now() + grace;
        while !self.threads.iter().all(JoinHandle::is_finished)
            && Instant::now() < join_deadline
        {
            std::thread::sleep(Duration::from_millis(2));
        }
        for h in self.threads.drain(..) {
            if h.is_finished() {
                let _ = h.join();
            }
        }
    }
}

fn response_id(msg: &Value) -> Option<u64> {
    // JSON-RPC responses have `id` and either `result` or `error` (no `method`)
    if msg.get("method").is_some() {
        return None;
    }
    msg.get("id").and_then(Value::as_u64)
}

fn is_null_id_error(msg: &Value) -> bool {
    msg.get("method").is_none()
        && msg.get("id") == Some(&Value::Null)
        && msg.get("error").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn unit_jsonrpc_rejects_malformed_header() {
        let mut cursor = Cursor::new(b"not-a-header\n".to_vec());
        let err = read_jsonrpc_frame(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn unit_jsonrpc_eof_returns_none() {
        let mut cursor = Cursor::new(Vec::<u8>::new());
        assert!(read_jsonrpc_frame(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn unit_jsonrpc_strips_leading_utf8_bom() {
        let payload = "\u{FEFF}{\"jsonrpc\":\"2.0\",\"id\":1}\n\u{FEFF}Content-Length: 2\r\n\r\n{}";
        let mut cursor = Cursor::new(payload.as_bytes().to_vec());

        let (f1, v1) = read_jsonrpc_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(f1, McpFraming::NewlineDelimited);
        assert_eq!(v1, json!({"jsonrpc": "2.0", "id": 1}));

        let (f2, v2) = read_jsonrpc_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(f2, McpFraming::ContentLength);
        assert_eq!(v2, json!({}));
    }

    #[test]
    fn unit_jsonrpc_rejects_non_colon_header_inside_block() {
        let raw = b"Content-Length: 2\r\nbad-header-no-colon\r\n\r\n{}";
        let mut cursor = Cursor::new(raw.to_vec());
        let err = read_jsonrpc_frame(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn unit_jsonrpc_rejects_conflicting_duplicate_content_length() {
        let conflict = b"Content-Length: 2\r\nContent-Length: 25\r\n\r\n{}";
        let mut cursor = Cursor::new(conflict.to_vec());
        let err = read_jsonrpc_frame(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        let identical = b"Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}";
        let mut cursor_ok = Cursor::new(identical.to_vec());
        let (f, v) = read_jsonrpc_frame(&mut cursor_ok).unwrap().unwrap();
        assert_eq!(f, McpFraming::ContentLength);
        assert_eq!(v, json!({}));
    }

    #[test]
    fn unit_jsonrpc_rejects_oversized_content_length() {
        let huge = b"Content-Length: 4294967296\r\n\r\n";
        let mut cursor = Cursor::new(huge.to_vec());
        let err = read_jsonrpc_frame(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn unit_mcp_client_null_id_error_and_zero_timeout_queued_drain() {
        let (c2s_read, c2s_write) = std::io::pipe().unwrap();
        let (s2c_read, mut s2c_write) = std::io::pipe().unwrap();

        let mut client = McpClient::from_reader_writer(
            Box::new(BufReader::new(s2c_read)),
            Box::new(c2s_write),
            McpFraming::NewlineDelimited,
        );

        // 1. Pre-queue a notification in rx and verify Duration::ZERO drains it
        let queued_notif = json!({
            "jsonrpc": "2.0",
            "method": "notifications/loom/radar_conflict",
            "params": {"seq": 7}
        });
        write_jsonrpc_frame(&mut s2c_write, McpFraming::NewlineDelimited, &queued_notif).unwrap();

        // Give reader_thread a brief moment to move the line into mpsc rx
        let start = Instant::now();
        let mut found = None;
        while start.elapsed() < Duration::from_secs(1) {
            match client.read_notification_matching(Duration::ZERO, |v| {
                v.get("params").and_then(|p| p.get("seq")) == Some(&json!(7))
            }) {
                Ok(val) => {
                    found = Some(val);
                    break;
                }
                Err(HarnessError::Timeout { .. }) => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        assert_eq!(found.expect("Duration::ZERO should drain queued rx message"), queued_notif);

        // 1b. Pre-queue a response in rx and verify wait_for_response(Duration::ZERO) drains it
        let queued_resp = json!({
            "jsonrpc": "2.0",
            "id": 99,
            "result": {"queued": true}
        });
        write_jsonrpc_frame(&mut s2c_write, McpFraming::NewlineDelimited, &queued_resp).unwrap();
        let start = Instant::now();
        let mut found_resp = None;
        while start.elapsed() < Duration::from_secs(1) {
            match client.wait_for_response(99, Duration::ZERO) {
                Ok(val) => {
                    found_resp = Some(val);
                    break;
                }
                Err(HarnessError::Timeout { .. }) => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        assert_eq!(
            found_resp.expect("wait_for_response(Duration::ZERO) should drain queued rx message"),
            queued_resp
        );

        // 2. Verify "id": null JSON-RPC error routes to active request() as HarnessError::JsonRpc
        let server = std::thread::spawn(move || {
            let mut reader = BufReader::new(c2s_read);
            if let Ok(Some((_f, _req))) = read_jsonrpc_frame(&mut reader) {
                let err_resp = json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": { "code": -32700, "message": "Parse error" }
                });
                write_jsonrpc_frame(&mut s2c_write, McpFraming::NewlineDelimited, &err_resp).unwrap();
            }
        });

        let err = client
            .request("tools/call", Some(json!({})), Duration::from_secs(2))
            .unwrap_err();
        match err {
            HarnessError::JsonRpc { code, message, .. } => {
                assert_eq!(code, -32700);
                assert_eq!(message, "Parse error");
            }
            other => panic!("expected HarnessError::JsonRpc, got {other:?}"),
        }

        drop(client);
        server.join().unwrap();
    }
}
