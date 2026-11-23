use interprocess::local_socket::tokio::Stream;
use loom_core::model::{CausalCapsule, FailureKind, NegativeEdge, RadarLevel, ViewportState};
use loom_core::rpc::{
    McpPreflightResult, RadarSnapshot, RpcErrorCode, RpcMethod, RpcPayload, RpcRequest,
    RpcResponse, read_frame, write_frame,
};
#[cfg(windows)]
use loom_core::rpc::{pipe_short_name, read_sock_descriptor};
#[cfg(unix)]
use loom_core::rpc::sock_descriptor_path;
use loom_daemon::mcp::{McpServerConfig, serve_mcp_stdio_with_streams, start_mcp_http_server};
use loom_daemon::{DaemonRunExit, StartDaemonOutcome, start_daemon};
use serde_json::Value;
use std::path::Path;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

async fn connect_test_ipc(repo_root: &Path) -> anyhow::Result<Stream> {
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
        Ok(Stream::connect(ns_name).await?)
    }
    #[cfg(unix)]
    {
        use interprocess::local_socket::{GenericFilePath, ToFsName, tokio::prelude::*};
        let sock_path = sock_descriptor_path(repo_root);
        let fs_name = sock_path.as_path().to_fs_name::<GenericFilePath>()?;
        Ok(Stream::connect(fs_name).await?)
    }
}

async fn send_rpc(
    repo_root: &Path,
    method: RpcMethod,
) -> Result<RpcPayload, (RpcErrorCode, String)> {
    let mut stream = connect_test_ipc(repo_root)
        .await
        .map_err(|e| (RpcErrorCode::IoError, e.to_string()))?;
    write_frame(&mut stream, &RpcRequest { id: 1, method })
        .await
        .map_err(|e| (RpcErrorCode::IoError, e.to_string()))?;
    let resp: RpcResponse = read_frame(&mut stream)
        .await
        .map_err(|e| (RpcErrorCode::IoError, e.to_string()))?
        .ok_or_else(|| (RpcErrorCode::IoError, "unexpected EOF".to_string()))?;
    resp.result.map_err(|e| (e.code, e.message))
}

async fn send_json_line(w: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>, val: &Value) {
    let s = format!("{}\n", val);
    w.write_all(s.as_bytes()).await.unwrap();
    w.flush().await.unwrap();
}

async fn recv_response_for_id(
    lines: &mut tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
    target_id: u64,
    captured_notifs: &mut Vec<Value>,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remain = deadline.saturating_duration_since(Instant::now());
        let line = tokio::time::timeout(remain, lines.next_line())
            .await
            .expect("timed out waiting for MCP stdio response")
            .unwrap()
            .expect("unexpected EOF on MCP stdio");
        let val: Value = serde_json::from_str(&line).unwrap();
        if val.get("id").and_then(|v| v.as_u64()) == Some(target_id) {
            return val;
        }
        if val.get("method").is_some() && val.get("id").is_none() {
            captured_notifs.push(val);
        }
    }
}

#[tokio::test]
async fn mcp_stdio_preinit_discover_32601_versions_tools_and_notifications() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn shared() -> i32 { 1 }\n").unwrap();

    let running = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    let vp1 = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "v1".into(),
            intent: "agent 1".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("{other:?}"),
    };
    let vp2 = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "v2".into(),
            intent: "agent 2".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("{other:?}"),
    };

    let (client_in, srv_out) = tokio::io::duplex(256 * 1024);
    let (srv_in, client_out) = tokio::io::duplex(256 * 1024);

    let cfg = McpServerConfig {
        repo_root: repo.to_path_buf(),
        default_viewport: Some("v1".into()),
    };
    let srv_task = tokio::spawn(async move { serve_mcp_stdio_with_streams(cfg, srv_in, srv_out).await });

    let (_, mut cw) = tokio::io::split(client_out);
    let (cr, _) = tokio::io::split(client_in);
    let mut lines = BufReader::new(cr).lines();
    let mut notifs = Vec::new();

    // 1. Pre-initialize `server/discover` must return JSON-RPC -32601 with matching id
    send_json_line(
        &mut cw,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "server/discover"
        }),
    )
    .await;
    let disc = recv_response_for_id(&mut lines, 1, &mut notifs).await;
    assert_eq!(disc["error"]["code"], -32601);

    // 2. Initialize negotiating "2024-11-05"
    send_json_line(
        &mut cw,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "test-client", "version": "0.1.0" }
            }
        }),
    )
    .await;
    let init_resp = recv_response_for_id(&mut lines, 2, &mut notifs).await;
    assert_eq!(init_resp["result"]["protocolVersion"], "2024-11-05");

    send_json_line(
        &mut cw,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }),
    )
    .await;

    // 3. tools/list lists all 4 tools
    send_json_line(
        &mut cw,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/list",
            "params": {}
        }),
    )
    .await;
    let list_resp = recv_response_for_id(&mut lines, 3, &mut notifs).await;
    let tool_names: Vec<&str> = list_resp["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for expected in [
        "loom_preflight_symbols",
        "loom_radar_poll",
        "loom_record_deadend",
        "loom_seal_capsule",
    ] {
        assert!(tool_names.contains(&expected), "missing tool {expected}");
    }

    // 4. loom_radar_poll
    send_json_line(
        &mut cw,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "loom_radar_poll",
                "arguments": {}
            }
        }),
    )
    .await;
    let poll_resp = recv_response_for_id(&mut lines, 4, &mut notifs).await;
    let snap: RadarSnapshot =
        serde_json::from_str(poll_resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(snap.viewports.len(), 1);
    assert_eq!(snap.viewports[0].viewport_name, "v1");

    // 5. loom_record_deadend rejects > 500 chars
    send_json_line(
        &mut cw,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "loom_record_deadend",
                "arguments": {
                    "touched_symbols": ["shared"],
                    "failure_kind": "CompileError",
                    "lesson_summary": "x".repeat(501)
                }
            }
        }),
    )
    .await;
    let bad_deadend = recv_response_for_id(&mut lines, 5, &mut notifs).await;
    assert_eq!(bad_deadend["result"]["isError"], true);

    // 6. loom_record_deadend persists valid <= 500 char NegativeEdge & advances next_edge_seq
    send_json_line(
        &mut cw,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": {
                "name": "loom_record_deadend",
                "arguments": {
                    "touched_symbols": ["shared"],
                    "failure_kind": "CompileError",
                    "lesson_summary": "avoid blocking inside shared()"
                }
            }
        }),
    )
    .await;
    let ok_deadend = recv_response_for_id(&mut lines, 6, &mut notifs).await;
    let edge: NegativeEdge =
        serde_json::from_str(ok_deadend["result"]["content"][0]["text"].as_str().unwrap())
            .unwrap();
    assert_eq!(edge.edge_seq, 1);
    assert_eq!(
        running.ctx.db.get_repo_meta().unwrap().unwrap().next_edge_seq,
        2
    );

    // 7. loom_preflight_symbols injects NegativeEdge warning
    send_json_line(
        &mut cw,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {
                "name": "loom_preflight_symbols",
                "arguments": {
                    "symbols": ["shared"]
                }
            }
        }),
    )
    .await;
    let preflight_resp = recv_response_for_id(&mut lines, 7, &mut notifs).await;
    let preflight: McpPreflightResult = serde_json::from_str(
        preflight_resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(preflight.negative_warnings.len(), 1);
    assert_eq!(
        preflight.negative_warnings[0].lesson_summary,
        "avoid blocking inside shared()"
    );

    // 8. Trigger RED conflict across v1 & v2 and verify push `notifications/loom/radar_conflict`
    std::fs::write(vp1.path.join("src/lib.rs"), "pub fn shared() -> i32 { 10 }\n").unwrap();
    std::fs::write(vp2.path.join("src/lib.rs"), "pub fn shared() -> i32 { 20 }\n").unwrap();

    let notif_deadline = Instant::now() + Duration::from_secs(4);
    let mut got_push = notifs
        .iter()
        .any(|n| n["method"] == "notifications/loom/radar_conflict");
    while !got_push && Instant::now() < notif_deadline {
        let remain = notif_deadline.saturating_duration_since(Instant::now());
        if let Ok(Ok(Some(line))) = tokio::time::timeout(remain, lines.next_line()).await {
            let val: Value = serde_json::from_str(&line).unwrap();
            if val.get("method").and_then(|m| m.as_str()) == Some("notifications/loom/radar_conflict")
            {
                got_push = true;
                break;
            }
        }
    }
    assert!(
        got_push,
        "expected notifications/loom/radar_conflict over stdio"
    );

    // 9. Seal v1 capsule with verify_cmd -> merges to trunk & sets head_capsule_id & Sealed
    send_json_line(
        &mut cw,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "tools/call",
            "params": {
                "name": "loom_seal_capsule",
                "arguments": {
                    "viewport": "v1",
                    "verify_cmd": "echo verify-ok",
                    "summary": "update shared to 10"
                }
            }
        }),
    )
    .await;
    let seal_resp = recv_response_for_id(&mut lines, 8, &mut notifs).await;
    let capsule: CausalCapsule =
        serde_json::from_str(seal_resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert!(capsule.verification_proof.as_ref().unwrap().passed);
    assert_eq!(
        std::fs::read_to_string(repo.join("src/lib.rs")).unwrap(),
        "pub fn shared() -> i32 { 10 }\n"
    );
    assert_eq!(
        running.ctx.db.get_repo_meta().unwrap().unwrap().head_capsule_id,
        Some(capsule.capsule_id.clone())
    );
    assert_eq!(
        running.ctx.db.get_viewport(&vp1.id).unwrap().unwrap().state,
        ViewportState::Sealed
    );

    drop(cw);
    srv_task.await.unwrap().unwrap();
    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

async fn post_http_json(
    addr: std::net::SocketAddr,
    session_id: Option<&str>,
    body: &Value,
) -> (u16, Option<String>, String) {
    let body_str = body.to_string();
    let mut req = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        addr.port(),
        body_str.len()
    );
    if let Some(sid) = session_id {
        req.push_str(&format!("Mcp-Session-Id: {sid}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(&body_str);

    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(req.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, raw_body) = text.split_once("\r\n\r\n").unwrap();
    let mut lines = head.lines();
    let status_line = lines.next().unwrap();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let mut sid_out = None;
    for h in lines {
        if let Some((k, v)) = h.split_once(':')
            && k.trim().eq_ignore_ascii_case("mcp-session-id")
        {
            sid_out = Some(v.trim().to_string());
        }
    }
    (status, sid_out, raw_body.to_string())
}

fn extract_json_from_http_or_sse(body: &str) -> Value {
    let trimmed = body.trim();
    if trimmed.starts_with('{') {
        return serde_json::from_str(trimmed).unwrap();
    }
    for line in trimmed.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            let d = data.trim();
            if d.starts_with('{') {
                return serde_json::from_str(d).unwrap();
            }
        }
    }
    panic!("no JSON payload found in HTTP response: {body:?}");
}

#[tokio::test]
async fn mcp_http_preinit_discover_32601_and_tool_call() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn greet() -> i32 { 1 }\n").unwrap();

    let running = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();
    send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "http-vp".into(),
            intent: "http test".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap();

    let cancel = CancellationToken::new();
    let (addr, http_task) = start_mcp_http_server(
        "127.0.0.1:0",
        McpServerConfig {
            repo_root: repo.to_path_buf(),
            default_viewport: Some("http-vp".into()),
        },
        cancel.clone(),
    )
    .await
    .unwrap();

    // 1. Pre-init `server/discover` -> HTTP 200 + JSON-RPC -32601
    let (st_disc, _, body_disc) = post_http_json(
        addr,
        None,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "server/discover"
        }),
    )
    .await;
    assert_eq!(st_disc, 200);
    let disc_json: Value = serde_json::from_str(&body_disc).unwrap();
    assert_eq!(disc_json["id"], 7);
    assert_eq!(disc_json["error"]["code"], -32601);

    // 2. Initialize negotiating "2025-11-25"
    let (st_init, sid_opt, body_init) = post_http_json(
        addr,
        None,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "http-test", "version": "0.1.0" }
            }
        }),
    )
    .await;
    assert_eq!(st_init, 200);
    let sid = sid_opt.expect("Mcp-Session-Id header");
    let init_val = extract_json_from_http_or_sse(&body_init);
    assert_eq!(init_val["result"]["protocolVersion"], "2025-11-25");

    let (st_notified, _, _) = post_http_json(
        addr,
        Some(&sid),
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }),
    )
    .await;
    assert!(st_notified == 200 || st_notified == 202);

    // 3. tools/call loom_radar_poll
    let (st_poll, _, body_poll) = post_http_json(
        addr,
        Some(&sid),
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "tools/call",
            "params": {
                "name": "loom_radar_poll",
                "arguments": {}
            }
        }),
    )
    .await;
    assert_eq!(st_poll, 200);
    let poll_val = extract_json_from_http_or_sse(&body_poll);
    let snap: RadarSnapshot =
        serde_json::from_str(poll_val["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(snap.viewports.len(), 1);
    assert_eq!(snap.viewports[0].viewport_name, "http-vp");

    cancel.cancel();
    http_task.await.unwrap().unwrap();
    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test]
async fn concurrent_record_deadend_allocates_distinct_monotonic_edge_seqs() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn shared() -> i32 { 1 }\n").unwrap();

    let running = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();
    let vp = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "v1".into(),
            intent: "concurrent deadend".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("{other:?}"),
    };

    let mut set = tokio::task::JoinSet::new();
    for i in 0..8usize {
        let repo_buf = repo.to_path_buf();
        let vp_id = vp.id.clone();
        set.spawn(async move {
            match send_rpc(
                &repo_buf,
                RpcMethod::McpRecordDeadend {
                    viewport_id: vp_id,
                    touched_symbols: vec!["shared".to_string()],
                    kind: "CompileError".to_string(),
                    lesson_summary: format!("concurrent lesson #{i}"),
                },
            )
            .await
            .unwrap()
            {
                RpcPayload::McpRecordDeadend(e) => e,
                other => panic!("unexpected payload: {other:?}"),
            }
        });
    }

    let mut seqs = Vec::new();
    while let Some(res) = set.join_next().await {
        seqs.push(res.unwrap().edge_seq);
    }
    seqs.sort_unstable();
    assert_eq!(seqs, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!(
        running.ctx.db.get_repo_meta().unwrap().unwrap().next_edge_seq,
        9
    );
    assert_eq!(
        running.ctx.db.query_negative_edges("shared").unwrap().len(),
        8
    );

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test]
async fn concurrent_seal_capsule_serializes_trunk_and_produces_linear_chain() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/a.rs"), "pub fn a() -> i32 { 1 }\n").unwrap();
    std::fs::write(repo.join("src/b.rs"), "pub fn b() -> i32 { 2 }\n").unwrap();

    let running = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    let vp1 = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "v1".into(),
            intent: "seal 1".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("{other:?}"),
    };
    let vp2 = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "v2".into(),
            intent: "seal 2".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("{other:?}"),
    };

    std::fs::write(vp1.path.join("src/a.rs"), "pub fn a() -> i32 { 11 }\n").unwrap();
    std::fs::write(vp2.path.join("src/b.rs"), "pub fn b() -> i32 { 22 }\n").unwrap();

    let (res1, res2) = tokio::join!(
        send_rpc(
            repo,
            RpcMethod::SealCapsule {
                viewport_id: vp1.id.clone(),
                verify_cmd: Some("echo seal-1".into()),
                summary: Some("seal v1".into()),
            },
        ),
        send_rpc(
            repo,
            RpcMethod::SealCapsule {
                viewport_id: vp2.id.clone(),
                verify_cmd: Some("echo seal-2".into()),
                summary: Some("seal v2".into()),
            },
        )
    );

    let cap1 = match res1.unwrap() {
        RpcPayload::CapsuleSeal(c) => c,
        other => panic!("{other:?}"),
    };
    let cap2 = match res2.unwrap() {
        RpcPayload::CapsuleSeal(c) => c,
        other => panic!("{other:?}"),
    };

    let (first, second) = if cap1.parent_capsule_ids.is_empty() {
        (cap1, cap2)
    } else {
        (cap2, cap1)
    };
    assert!(first.parent_capsule_ids.is_empty());
    assert_eq!(second.parent_capsule_ids, vec![first.capsule_id.clone()]);
    assert_eq!(
        running.ctx.db.get_repo_meta().unwrap().unwrap().head_capsule_id,
        Some(second.capsule_id.clone())
    );
    assert_eq!(
        running.ctx.db.get_viewport(&vp1.id).unwrap().unwrap().state,
        ViewportState::Sealed
    );
    assert_eq!(
        running.ctx.db.get_viewport(&vp2.id).unwrap().unwrap().state,
        ViewportState::Sealed
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("src/a.rs")).unwrap(),
        "pub fn a() -> i32 { 11 }\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("src/b.rs")).unwrap(),
        "pub fn b() -> i32 { 22 }\n"
    );

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test]
async fn seal_capsule_failing_verify_cmd_rolls_back_and_reregisters_with_radar() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn shared() -> i32 { 1 }\n").unwrap();

    let running = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();
    let vp1 = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "v1".into(),
            intent: "verify rollback".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("{other:?}"),
    };

    std::fs::write(vp1.path.join("src/lib.rs"), "pub fn shared() -> i32 { 99 }\n").unwrap();

    let (client_in, srv_out) = tokio::io::duplex(256 * 1024);
    let (srv_in, client_out) = tokio::io::duplex(256 * 1024);
    let cfg = McpServerConfig {
        repo_root: repo.to_path_buf(),
        default_viewport: Some("v1".into()),
    };
    let srv_task =
        tokio::spawn(async move { serve_mcp_stdio_with_streams(cfg, srv_in, srv_out).await });

    let (_, mut cw) = tokio::io::split(client_out);
    let (cr, _) = tokio::io::split(client_in);
    let mut lines = BufReader::new(cr).lines();
    let mut notifs = Vec::new();

    send_json_line(
        &mut cw,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "rollback-test", "version": "0.1.0" }
            }
        }),
    )
    .await;
    let _ = recv_response_for_id(&mut lines, 1, &mut notifs).await;
    send_json_line(
        &mut cw,
        &serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;

    send_json_line(
        &mut cw,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "loom_seal_capsule",
                "arguments": {
                    "viewport": "v1",
                    "verify_cmd": "exit 7"
                }
            }
        }),
    )
    .await;
    let seal_err = recv_response_for_id(&mut lines, 2, &mut notifs).await;
    assert_eq!(seal_err["result"]["isError"], true);

    assert_eq!(
        running.ctx.db.get_viewport(&vp1.id).unwrap().unwrap().state,
        ViewportState::Active
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("src/lib.rs")).unwrap(),
        "pub fn shared() -> i32 { 1 }\n"
    );
    assert!(
        running
            .ctx
            .db
            .get_repo_meta()
            .unwrap()
            .unwrap()
            .head_capsule_id
            .is_none()
    );

    send_json_line(
        &mut cw,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "loom_radar_poll",
                "arguments": {}
            }
        }),
    )
    .await;
    let poll_resp = recv_response_for_id(&mut lines, 3, &mut notifs).await;
    let snap: RadarSnapshot =
        serde_json::from_str(poll_resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(snap.viewports.len(), 1);
    assert_eq!(snap.viewports[0].viewport_id, vp1.id);
    assert_eq!(snap.viewports[0].state, ViewportState::Active);

    drop(cw);
    srv_task.await.unwrap().unwrap();
    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test]
async fn server_discover_empty_params_string_id_and_stdio_eof_flush() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn f() -> i32 { 1 }\n").unwrap();

    let running = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    // 1. HTTP `server/discover` with `"params": {}` and string `"id": "abc"`
    let cancel = CancellationToken::new();
    let (addr, http_task) = start_mcp_http_server(
        "127.0.0.1:0",
        McpServerConfig {
            repo_root: repo.to_path_buf(),
            default_viewport: None,
        },
        cancel.clone(),
    )
    .await
    .unwrap();
    let (st, _, body) = post_http_json(
        addr,
        None,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": "abc",
            "method": "server/discover",
            "params": {}
        }),
    )
    .await;
    assert_eq!(st, 200);
    let http_disc: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(http_disc["id"], "abc");
    assert_eq!(http_disc["error"]["code"], -32601);
    cancel.cancel();
    http_task.await.unwrap().unwrap();

    // 2. Single-shot piped stdio: send discover + initialize + tools/list, then close stdin immediately!
    let (client_in, srv_out) = tokio::io::duplex(256 * 1024);
    let (srv_in, client_out) = tokio::io::duplex(256 * 1024);
    let cfg = McpServerConfig {
        repo_root: repo.to_path_buf(),
        default_viewport: None,
    };
    let srv_task =
        tokio::spawn(async move { serve_mcp_stdio_with_streams(cfg, srv_in, srv_out).await });

    let (_, mut cw) = tokio::io::split(client_out);
    send_json_line(
        &mut cw,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": "abc",
            "method": "server/discover",
            "params": {}
        }),
    )
    .await;
    send_json_line(
        &mut cw,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": "init-1",
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "oneshot", "version": "0.1.0" }
            }
        }),
    )
    .await;
    send_json_line(
        &mut cw,
        &serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;
    send_json_line(
        &mut cw,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": "list-1",
            "method": "tools/list",
            "params": {}
        }),
    )
    .await;
    // Close stdin immediately without waiting for responses first
    cw.shutdown().await.unwrap();
    drop(cw);

    let (cr, _) = tokio::io::split(client_in);
    let mut lines = BufReader::new(cr).lines();
    let mut responses = Vec::new();
    while let Some(line) = tokio::time::timeout(Duration::from_secs(3), lines.next_line())
        .await
        .expect("timed out draining stdio output")
        .unwrap()
    {
        responses.push(serde_json::from_str::<Value>(&line).unwrap());
    }
    srv_task.await.unwrap().unwrap();

    let disc_resp = responses
        .iter()
        .find(|v| v.get("id").and_then(|id| id.as_str()) == Some("abc"))
        .expect("expected server/discover response before EOF");
    assert_eq!(disc_resp["error"]["code"], -32601);

    let list_resp = responses
        .iter()
        .find(|v| v.get("id").and_then(|id| id.as_str()) == Some("list-1"))
        .expect("expected tools/list response flushed before EOF");
    assert!(list_resp["result"]["tools"].as_array().unwrap().len() >= 4);

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test]
async fn file_only_preflight_symbols_surfaces_matching_negative_edge_warnings() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("src/lib.rs"),
        "pub fn shared_target() -> i32 { 1 }\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("src/other.rs"),
        "pub fn unrelated() -> i32 { 2 }\n",
    )
    .unwrap();

    let running = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();
    let vp1 = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "v1".into(),
            intent: "deadend author".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("{other:?}"),
    };
    send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "v2".into(),
            intent: "preflight caller".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap();

    // Record deadend touching only symbol name (no `file:` prefix)
    send_rpc(
        repo,
        RpcMethod::McpRecordDeadend {
            viewport_id: vp1.id.clone(),
            touched_symbols: vec!["shared_target".to_string()],
            kind: "DesignDeadEnd".to_string(),
            lesson_summary: "do not cache shared_target without lock".to_string(),
        },
    )
    .await
    .unwrap();

    // File-only query for `src/lib.rs` with `symbols: []` must expand symbols in `src/lib.rs`
    // and return the NegativeEdge warning for `shared_target`.
    let res = match send_rpc(
        repo,
        RpcMethod::McpPreflightEx {
            viewport_id: Some("v2".into()),
            symbols: vec![],
            files: vec!["src/lib.rs".into()],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::McpPreflight(r) => r,
        other => panic!("{other:?}"),
    };
    assert!(
        res.provides
            .iter()
            .any(|p| p.symbol_fqn.ends_with("shared_target"))
    );
    assert_eq!(res.negative_warnings.len(), 1);
    assert_eq!(
        res.negative_warnings[0].lesson_summary,
        "do not cache shared_target without lock"
    );

    // File-only query for `src/other.rs` must NOT include `shared_target` deadend.
    let unrelated = match send_rpc(
        repo,
        RpcMethod::McpPreflightEx {
            viewport_id: Some("v2".into()),
            symbols: vec![],
            files: vec!["src/other.rs".into()],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::McpPreflight(r) => r,
        other => panic!("{other:?}"),
    };
    assert!(unrelated.negative_warnings.is_empty());

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test]
async fn concurrent_4_way_disjoint_seal_preserves_all_trunk_writes() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    for f in ["a", "b", "c", "d"] {
        std::fs::write(
            repo.join(format!("src/{f}.rs")),
            format!("pub fn {f}() -> i32 {{ 1 }}\n"),
        )
        .unwrap();
    }

    let running = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    let mut vps = Vec::new();
    for f in ["a", "b", "c", "d"] {
        let vp = match send_rpc(
            repo,
            RpcMethod::SpawnViewport {
                name: format!("v_{f}"),
                intent: format!("edit {f}"),
                cmd: vec![],
            },
        )
        .await
        .unwrap()
        {
            RpcPayload::Viewport(v) => v,
            other => panic!("{other:?}"),
        };
        std::fs::write(
            vp.path.join(format!("src/{f}.rs")),
            format!("pub fn {f}() -> i32 {{ 100 }}\n"),
        )
        .unwrap();
        vps.push(vp);
    }

    let (r0, r1, r2, r3) = tokio::join!(
        send_rpc(
            repo,
            RpcMethod::SealCapsule {
                viewport_id: vps[0].id.clone(),
                verify_cmd: None,
                summary: Some("seal a".into()),
            }
        ),
        send_rpc(
            repo,
            RpcMethod::SealCapsule {
                viewport_id: vps[1].id.clone(),
                verify_cmd: None,
                summary: Some("seal b".into()),
            }
        ),
        send_rpc(
            repo,
            RpcMethod::SealCapsule {
                viewport_id: vps[2].id.clone(),
                verify_cmd: None,
                summary: Some("seal c".into()),
            }
        ),
        send_rpc(
            repo,
            RpcMethod::SealCapsule {
                viewport_id: vps[3].id.clone(),
                verify_cmd: None,
                summary: Some("seal d".into()),
            }
        ),
    );

    let mut caps = Vec::new();
    for res in [r0, r1, r2, r3] {
        match res.unwrap() {
            RpcPayload::CapsuleSeal(c) => caps.push(c),
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(caps.len(), 4);
    for f in ["a", "b", "c", "d"] {
        assert_eq!(
            std::fs::read_to_string(repo.join(format!("src/{f}.rs"))).unwrap(),
            format!("pub fn {f}() -> i32 {{ 100 }}\n"),
            "trunk file src/{f}.rs was clobbered"
        );
    }

    // Verify linear 4-capsule chain
    let all_caps = running.ctx.db.list_capsules().unwrap();
    assert_eq!(all_caps.len(), 4);
    let head = running
        .ctx
        .db
        .get_repo_meta()
        .unwrap()
        .unwrap()
        .head_capsule_id
        .unwrap();
    let mut chain = Vec::new();
    let mut cur = Some(head);
    while let Some(cid) = cur {
        chain.push(cid.clone());
        let cap = all_caps.iter().find(|c| c.capsule_id == cid).unwrap();
        cur = cap.parent_capsule_ids.first().cloned();
    }
    assert_eq!(chain.len(), 4);

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test]
async fn overlapping_sequential_seals_preserve_edits_cst_insertions_new_files_deletions_and_crdt() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/a.rs"), "pub fn a() -> i32 { 1 }\n").unwrap();
    std::fs::write(repo.join("src/b.rs"), "pub fn b() -> i32 { 2 }\n").unwrap();
    std::fs::write(
        repo.join("src/lib.rs"),
        "pub fn base_fn() -> i32 {\n    0\n}\n",
    )
    .unwrap();
    std::fs::write(repo.join("src/obsolete.rs"), "pub fn old() {}\n").unwrap();
    std::fs::write(repo.join("config.json"), "{\n  \"a\": 1,\n  \"b\": 2\n}\n").unwrap();

    let running = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    let vp1 = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "v1".into(),
            intent: "feature 1".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("{other:?}"),
    };
    let vp2 = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "v2".into(),
            intent: "feature 2".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("{other:?}"),
    };

    // v1: edit src/a.rs, insert added_by_v1 in src/lib.rs, create src/new_v1.rs, delete src/obsolete.rs, update config.json["a"]
    std::fs::write(vp1.path.join("src/a.rs"), "pub fn a() -> i32 { 11 }\n").unwrap();
    std::fs::write(
        vp1.path.join("src/lib.rs"),
        "pub fn base_fn() -> i32 {\n    0\n}\n\npub fn added_by_v1() -> i32 {\n    1\n}\n",
    )
    .unwrap();
    std::fs::write(
        vp1.path.join("src/new_v1.rs"),
        "pub fn new_v1() -> i32 { 111 }\n",
    )
    .unwrap();
    std::fs::remove_file(vp1.path.join("src/obsolete.rs")).unwrap();
    std::fs::write(
        vp1.path.join("config.json"),
        "{\n  \"a\": 11,\n  \"b\": 2\n}\n",
    )
    .unwrap();

    // v2: edit src/b.rs, insert added_by_v2 in src/lib.rs, create src/new_v2.rs, update config.json["b"]
    std::fs::write(vp2.path.join("src/b.rs"), "pub fn b() -> i32 { 22 }\n").unwrap();
    std::fs::write(
        vp2.path.join("src/lib.rs"),
        "pub fn base_fn() -> i32 {\n    0\n}\n\npub fn added_by_v2() -> i32 {\n    2\n}\n",
    )
    .unwrap();
    std::fs::write(
        vp2.path.join("src/new_v2.rs"),
        "pub fn new_v2() -> i32 { 222 }\n",
    )
    .unwrap();
    std::fs::write(
        vp2.path.join("config.json"),
        "{\n  \"a\": 1,\n  \"b\": 22\n}\n",
    )
    .unwrap();

    send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp1.id.clone(),
            verify_cmd: None,
            summary: Some("seal v1".into()),
        },
    )
    .await
    .unwrap();
    send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp2.id.clone(),
            verify_cmd: None,
            summary: Some("seal v2".into()),
        },
    )
    .await
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(repo.join("src/a.rs")).unwrap(),
        "pub fn a() -> i32 { 11 }\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("src/b.rs")).unwrap(),
        "pub fn b() -> i32 { 22 }\n"
    );
    let lib_src = std::fs::read_to_string(repo.join("src/lib.rs")).unwrap();
    assert!(lib_src.contains("added_by_v1"), "lib.rs missing added_by_v1:\n{lib_src}");
    assert!(lib_src.contains("added_by_v2"), "lib.rs missing added_by_v2:\n{lib_src}");
    assert_eq!(
        std::fs::read_to_string(repo.join("src/new_v1.rs")).unwrap(),
        "pub fn new_v1() -> i32 { 111 }\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("src/new_v2.rs")).unwrap(),
        "pub fn new_v2() -> i32 { 222 }\n"
    );
    assert!(!repo.join("src/obsolete.rs").exists());
    let cfg_json: Value =
        serde_json::from_str(&std::fs::read_to_string(repo.join("config.json")).unwrap()).unwrap();
    assert_eq!(cfg_json["a"], 11);
    assert_eq!(cfg_json["b"], 22);

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test]
async fn seal_capsule_3way_conflict_and_delete_vs_modify_abort_cleanly() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("src/lib.rs"),
        "pub fn shared() -> i32 {\n    1\n}\n",
    )
    .unwrap();
    std::fs::write(repo.join("src/del.rs"), "pub fn doomed() -> i32 { 1 }\n").unwrap();

    let running = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    // Part 1: 3-way ReplaceBody conflict on shared()
    let vp1 = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "v1".into(),
            intent: "edit shared 1".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("{other:?}"),
    };
    let vp2 = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "v2".into(),
            intent: "edit shared 2".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("{other:?}"),
    };

    std::fs::write(
        vp1.path.join("src/lib.rs"),
        "pub fn shared() -> i32 {\n    10\n}\n",
    )
    .unwrap();
    std::fs::write(
        vp2.path.join("src/lib.rs"),
        "pub fn shared() -> i32 {\n    20\n}\n",
    )
    .unwrap();

    send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp1.id.clone(),
            verify_cmd: None,
            summary: Some("v1 wins".into()),
        },
    )
    .await
    .unwrap();

    let err2 = send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp2.id.clone(),
            verify_cmd: None,
            summary: Some("v2 conflicts".into()),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err2.0, RpcErrorCode::Conflict);
    assert_eq!(
        std::fs::read_to_string(repo.join("src/lib.rs")).unwrap(),
        "pub fn shared() -> i32 {\n    10\n}\n"
    );
    assert_eq!(
        running.ctx.db.get_viewport(&vp2.id).unwrap().unwrap().state,
        ViewportState::Active
    );
    let snap = match send_rpc(
        repo,
        RpcMethod::RadarSnapshot {
            viewport_id: Some(vp2.id.clone()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::RadarSnapshot(s) => s,
        other => panic!("{other:?}"),
    };
    assert!(
        snap.viewports
            .iter()
            .any(|v| v.viewport_id == vp2.id && v.state == ViewportState::Active),
        "conflicted viewport must be re-registered in Radar"
    );

    // Part 2: delete-vs-modify conflict on src/del.rs
    let vp1b = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "v1b".into(),
            intent: "delete del.rs".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("{other:?}"),
    };
    let vp2b = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "v2b".into(),
            intent: "modify del.rs".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("{other:?}"),
    };

    std::fs::remove_file(vp1b.path.join("src/del.rs")).unwrap();
    std::fs::write(
        vp2b.path.join("src/del.rs"),
        "pub fn doomed() -> i32 { 999 }\n",
    )
    .unwrap();

    send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp1b.id.clone(),
            verify_cmd: None,
            summary: Some("delete del.rs".into()),
        },
    )
    .await
    .unwrap();
    assert!(!repo.join("src/del.rs").exists());

    let del_err = send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp2b.id.clone(),
            verify_cmd: None,
            summary: Some("modify deleted file".into()),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(del_err.0, RpcErrorCode::Conflict);
    assert!(!repo.join("src/del.rs").exists());
    assert_eq!(
        running.ctx.db.get_viewport(&vp2b.id).unwrap().unwrap().state,
        ViewportState::Active
    );

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test]
async fn restore_clean_refreshes_base_snapshot_for_subsequent_seal() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/a.rs"), "pub fn a() -> i32 { 1 }\n").unwrap();
    std::fs::write(repo.join("src/b.rs"), "pub fn b() -> i32 { 2 }\n").unwrap();

    let running = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    let vp1 = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "v1".into(),
            intent: "edit a".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("{other:?}"),
    };
    let vp2 = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "v2".into(),
            intent: "edit b after restore".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("{other:?}"),
    };

    std::fs::write(vp1.path.join("src/a.rs"), "pub fn a() -> i32 { 11 }\n").unwrap();
    let cap1 = match send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp1.id.clone(),
            verify_cmd: None,
            summary: Some("seal v1".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::CapsuleSeal(c) => c,
        other => panic!("{other:?}"),
    };

    // Dirty vp2 then restore clean; vp2 must pick up cap1 as its new base_capsule_id & base_snapshot_cas
    std::fs::write(vp2.path.join("src/a.rs"), "pub fn a() -> i32 { 999 }\n").unwrap();
    send_rpc(
        repo,
        RpcMethod::RestoreViewportClean {
            viewport_id: vp2.id.clone(),
        },
    )
    .await
    .unwrap();

    let vp2_refreshed = running.ctx.db.get_viewport(&vp2.id).unwrap().unwrap();
    assert_eq!(vp2_refreshed.base_capsule_id, Some(cap1.capsule_id.clone()));
    assert_eq!(
        std::fs::read_to_string(vp2.path.join("src/a.rs")).unwrap(),
        "pub fn a() -> i32 { 11 }\n"
    );

    std::fs::write(vp2.path.join("src/b.rs"), "pub fn b() -> i32 { 22 }\n").unwrap();
    send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp2.id.clone(),
            verify_cmd: None,
            summary: Some("seal v2".into()),
        },
    )
    .await
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(repo.join("src/a.rs")).unwrap(),
        "pub fn a() -> i32 { 11 }\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("src/b.rs")).unwrap(),
        "pub fn b() -> i32 { 22 }\n"
    );

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test]
async fn same_viewport_concurrent_double_seal_rejected() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/a.rs"), "pub fn a() -> i32 { 1 }\n").unwrap();

    let running = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    let vp1 = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "v1".into(),
            intent: "double seal".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("{other:?}"),
    };
    std::fs::write(vp1.path.join("src/a.rs"), "pub fn a() -> i32 { 77 }\n").unwrap();

    #[cfg(windows)]
    let slow_cmd = "powershell -NoProfile -Command \"Start-Sleep -Milliseconds 200\"";
    #[cfg(unix)]
    let slow_cmd = "sleep 0.2";

    let (r1, r2) = tokio::join!(
        send_rpc(
            repo,
            RpcMethod::SealCapsule {
                viewport_id: vp1.id.clone(),
                verify_cmd: Some(slow_cmd.to_string()),
                summary: Some("seal first".into()),
            }
        ),
        async {
            tokio::time::sleep(Duration::from_millis(40)).await;
            send_rpc(
                repo,
                RpcMethod::SealCapsule {
                    viewport_id: vp1.id.clone(),
                    verify_cmd: None,
                    summary: Some("seal second".into()),
                },
            )
            .await
        }
    );

    let ok_count = [r1.is_ok(), r2.is_ok()].into_iter().filter(|b| *b).count();
    assert_eq!(ok_count, 1, "expected exactly 1 seal to succeed: r1={r1:?}, r2={r2:?}");
    let err = r1.err().or_else(|| r2.err()).unwrap();
    assert!(
        matches!(err.0, RpcErrorCode::Conflict | RpcErrorCode::InvalidState),
        "unexpected error code for double seal: {err:?}"
    );
    assert_eq!(running.ctx.db.list_capsules().unwrap().len(), 1);
    assert_eq!(
        running.ctx.db.get_viewport(&vp1.id).unwrap().unwrap().state,
        ViewportState::Sealed
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("src/a.rs")).unwrap(),
        "pub fn a() -> i32 { 77 }\n"
    );

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test]
async fn stdio_eof_with_unanswered_unknown_method_bounds_grace_timeout() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn f() -> i32 { 1 }\n").unwrap();

    let running = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    let (client_in, srv_out) = tokio::io::duplex(256 * 1024);
    let (srv_in, client_out) = tokio::io::duplex(256 * 1024);
    let cfg = McpServerConfig {
        repo_root: repo.to_path_buf(),
        default_viewport: None,
    };
    let srv_task =
        tokio::spawn(async move { serve_mcp_stdio_with_streams(cfg, srv_in, srv_out).await });

    let (_, mut cw) = tokio::io::split(client_out);
    send_json_line(
        &mut cw,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": "init-1",
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "grace-test", "version": "0.1.0" }
            }
        }),
    )
    .await;
    send_json_line(
        &mut cw,
        &serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;
    send_json_line(
        &mut cw,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": "poll-1",
            "method": "tools/call",
            "params": {
                "name": "loom_radar_poll",
                "arguments": {}
            }
        }),
    )
    .await;
    // Unrecognized method produces id: null error in rmcp, so "bad-1" stays untracked until bounded grace timeout expires.
    send_json_line(
        &mut cw,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": "bad-1",
            "method": "unknown/noop",
            "params": {}
        }),
    )
    .await;
    cw.shutdown().await.unwrap();
    drop(cw);

    let (cr, _) = tokio::io::split(client_in);
    let mut lines = BufReader::new(cr).lines();
    let mut responses = Vec::new();
    while let Some(line) = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
        .await
        .expect("timed out draining stdio output with unanswered id")
        .unwrap()
    {
        responses.push(serde_json::from_str::<Value>(&line).unwrap());
    }
    tokio::time::timeout(Duration::from_secs(5), srv_task)
        .await
        .expect("serve_mcp_stdio_with_streams hung past grace timeout")
        .unwrap()
        .unwrap();

    assert!(
        responses
            .iter()
            .any(|v| v.get("id").and_then(|id| id.as_str()) == Some("poll-1")),
        "expected poll-1 response flushed before EOF"
    );

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test]
async fn mcp_record_deadend_accepts_deadlock_and_rejects_invalid_kind() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn auth() -> i32 { 1 }\n").unwrap();

    let running = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    let vp = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "v1".into(),
            intent: "deadlock test".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("{other:?}"),
    };

    let (client_in, srv_out) = tokio::io::duplex(256 * 1024);
    let (srv_in, client_out) = tokio::io::duplex(256 * 1024);
    let cfg = McpServerConfig {
        repo_root: repo.to_path_buf(),
        default_viewport: Some(vp.name.clone()),
    };
    let srv_task =
        tokio::spawn(async move { serve_mcp_stdio_with_streams(cfg, srv_in, srv_out).await });

    let (cr, _) = tokio::io::split(client_in);
    let (_, mut cw) = tokio::io::split(client_out);
    let mut lines = BufReader::new(cr).lines();
    let mut notifs = Vec::new();

    send_json_line(
        &mut cw,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "deadlock-test", "version": "0.1.0" }
            }
        }),
    )
    .await;
    let _ = recv_response_for_id(&mut lines, 1, &mut notifs).await;
    send_json_line(
        &mut cw,
        &serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;

    // 1. Valid "Deadlock" failure_kind succeeds
    send_json_line(
        &mut cw,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "loom_record_deadend",
                "arguments": {
                    "touched_symbols": ["auth"],
                    "failure_kind": "Deadlock",
                    "lesson_summary": "mutex inversion on auth_cache"
                }
            }
        }),
    )
    .await;
    let ok_resp = recv_response_for_id(&mut lines, 2, &mut notifs).await;
    assert_ne!(ok_resp["result"]["isError"], true, "unexpected error: {ok_resp}");
    let edge: NegativeEdge =
        serde_json::from_str(ok_resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(edge.failure_kind, loom_core::model::FailureKind::Deadlock);

    // 2. Invalid failure_kind via MCP returns isError=true and does not pollute negative_index
    send_json_line(
        &mut cw,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "loom_record_deadend",
                "arguments": {
                    "touched_symbols": ["auth"],
                    "failure_kind": "NotARealFailureKind_XYZ",
                    "lesson_summary": "should be rejected"
                }
            }
        }),
    )
    .await;
    let bad_resp = recv_response_for_id(&mut lines, 3, &mut notifs).await;
    assert_eq!(bad_resp["result"]["isError"], true, "expected isError: {bad_resp}");
    assert_eq!(running.ctx.db.list_all_negative_edges().unwrap().len(), 1);

    cw.shutdown().await.unwrap();
    drop(cw);
    srv_task.await.unwrap().unwrap();

    // 3. Direct RPC McpRecordDeadend also strictly rejects invalid kind
    let rpc_err = send_rpc(
        repo,
        RpcMethod::McpRecordDeadend {
            viewport_id: vp.id.clone(),
            touched_symbols: vec!["auth".into()],
            kind: "NotARealFailureKind_XYZ".into(),
            lesson_summary: "direct rpc invalid kind".into(),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(rpc_err.0, RpcErrorCode::InvalidRequest);
    assert_eq!(running.ctx.db.list_all_negative_edges().unwrap().len(), 1);

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test]
async fn mcp_record_deadend_default_restore_clean_reverts_dirty_files_and_clears_radar_red() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn shared() -> i32 { 1 }\n").unwrap();

    let running = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    let vp1 = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "v1".into(),
            intent: "deadend author".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("{other:?}"),
    };
    let vp2 = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "v2".into(),
            intent: "peer observer".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("{other:?}"),
    };

    let (client_in, srv_out) = tokio::io::duplex(256 * 1024);
    let (srv_in, client_out) = tokio::io::duplex(256 * 1024);
    let cfg = McpServerConfig {
        repo_root: repo.to_path_buf(),
        default_viewport: Some("v1".into()),
    };
    let srv_task =
        tokio::spawn(async move { serve_mcp_stdio_with_streams(cfg, srv_in, srv_out).await });

    let (cr, _) = tokio::io::split(client_in);
    let (_, mut cw) = tokio::io::split(client_out);
    let mut lines = BufReader::new(cr).lines();
    let mut notifs = Vec::new();

    send_json_line(
        &mut cw,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "deadend-default-test", "version": "0.1.0" }
            }
        }),
    )
    .await;
    let _ = recv_response_for_id(&mut lines, 1, &mut notifs).await;
    send_json_line(
        &mut cw,
        &serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;

    // Dirty tracked file + create untracked file in v1, and edit same symbol in v2 to trigger RED
    std::fs::write(vp1.path.join("src/lib.rs"), "pub fn shared() -> i32 { 999 }\n").unwrap();
    std::fs::write(vp1.path.join("src/scratch.rs"), "pub fn temp() {}\n").unwrap();
    std::fs::write(vp2.path.join("src/lib.rs"), "pub fn shared() -> i32 { 20 }\n").unwrap();

    let mut next_id = 2u64;
    let red_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let id = next_id;
        next_id += 1;
        send_json_line(
            &mut cw,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": {
                    "name": "loom_radar_poll",
                    "arguments": { "viewport": "v2" }
                }
            }),
        )
        .await;
        let resp = recv_response_for_id(&mut lines, id, &mut notifs).await;
        let snap: RadarSnapshot =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        if snap.viewports.iter().any(|v| v.viewport_name == "v2" && v.level == RadarLevel::Red) {
            break;
        }
        assert!(
            Instant::now() < red_deadline,
            "timed out waiting for v2 Radar RED conflict: {snap:?}"
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    }

    // Call loom_record_deadend WITHOUT restore_clean -> must restore clean by default
    let deadend_id = next_id;
    next_id += 1;
    send_json_line(
        &mut cw,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": deadend_id,
            "method": "tools/call",
            "params": {
                "name": "loom_record_deadend",
                "arguments": {
                    "touched_symbols": ["shared"],
                    "failure_kind": "DesignDeadEnd",
                    "lesson_summary": "avoid 999 on shared"
                }
            }
        }),
    )
    .await;
    let deadend_resp = recv_response_for_id(&mut lines, deadend_id, &mut notifs).await;
    assert_ne!(deadend_resp["result"]["isError"], true, "unexpected error: {deadend_resp}");
    let edge: NegativeEdge =
        serde_json::from_str(deadend_resp["result"]["content"][0]["text"].as_str().unwrap())
            .unwrap();
    assert_eq!(edge.failure_kind, FailureKind::DesignDeadEnd);
    assert_eq!(edge.lesson_summary, "avoid 999 on shared");

    // Verify v1 tracked file reverted, untracked file removed, and v1 remains Active with cleared patches
    assert_eq!(
        std::fs::read_to_string(vp1.path.join("src/lib.rs")).unwrap(),
        "pub fn shared() -> i32 { 1 }\n"
    );
    assert!(
        !vp1.path.join("src/scratch.rs").exists(),
        "untracked scratch.rs should be removed by default restore_clean"
    );
    let vp1_db = running.ctx.db.get_viewport(&vp1.id).unwrap().unwrap();
    assert_eq!(vp1_db.state, ViewportState::Active);
    assert!(vp1_db.patch_ids.is_empty());
    assert!(vp1_db.head_cas.is_none());

    // Verify Radar RED clears on v1 and v2 (default_viewport=v1 filters `{}` to v1, so poll v1 and v2 explicitly)
    let green_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let id1 = next_id;
        next_id += 1;
        send_json_line(
            &mut cw,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": id1,
                "method": "tools/call",
                "params": {
                    "name": "loom_radar_poll",
                    "arguments": { "viewport": "v1" }
                }
            }),
        )
        .await;
        let resp1 = recv_response_for_id(&mut lines, id1, &mut notifs).await;
        let snap1: RadarSnapshot =
            serde_json::from_str(resp1["result"]["content"][0]["text"].as_str().unwrap()).unwrap();

        let id2 = next_id;
        next_id += 1;
        send_json_line(
            &mut cw,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": id2,
                "method": "tools/call",
                "params": {
                    "name": "loom_radar_poll",
                    "arguments": { "viewport": "v2" }
                }
            }),
        )
        .await;
        let resp2 = recv_response_for_id(&mut lines, id2, &mut notifs).await;
        let snap2: RadarSnapshot =
            serde_json::from_str(resp2["result"]["content"][0]["text"].as_str().unwrap()).unwrap();

        let v1_green = snap1
            .viewports
            .iter()
            .any(|v| v.viewport_name == "v1" && v.level == RadarLevel::Green && v.conflicts.is_empty());
        let v2_green = snap2
            .viewports
            .iter()
            .any(|v| v.viewport_name == "v2" && v.level == RadarLevel::Green && v.conflicts.is_empty());
        if v1_green && v2_green {
            break;
        }
        assert!(
            Instant::now() < green_deadline,
            "timed out waiting for Radar RED to clear after loom_record_deadend: v1={snap1:?}, v2={snap2:?}"
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    }

    // Verify NegativeEdge persisted & returned by loom_preflight_symbols on v2
    assert_eq!(running.ctx.db.list_all_negative_edges().unwrap().len(), 1);
    let preflight_id = next_id;
    send_json_line(
        &mut cw,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": preflight_id,
            "method": "tools/call",
            "params": {
                "name": "loom_preflight_symbols",
                "arguments": {
                    "viewport": "v2",
                    "symbols": ["shared"]
                }
            }
        }),
    )
    .await;
    let preflight_resp = recv_response_for_id(&mut lines, preflight_id, &mut notifs).await;
    let preflight: McpPreflightResult =
        serde_json::from_str(preflight_resp["result"]["content"][0]["text"].as_str().unwrap())
            .unwrap();
    assert_eq!(preflight.negative_warnings.len(), 1);
    assert_eq!(
        preflight.negative_warnings[0].lesson_summary,
        "avoid 999 on shared"
    );

    cw.shutdown().await.unwrap();
    drop(cw);
    srv_task.await.unwrap().unwrap();
    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test]
async fn mcp_record_deadend_explicit_restore_clean_false_preserves_dirty_files() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn calc() -> i32 { 1 }\n").unwrap();

    let running = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    let vp1 = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "v1".into(),
            intent: "keep dirty files".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("{other:?}"),
    };
    let _vp2 = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "v2".into(),
            intent: "preflight observer".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("{other:?}"),
    };

    let (client_in, srv_out) = tokio::io::duplex(256 * 1024);
    let (srv_in, client_out) = tokio::io::duplex(256 * 1024);
    let cfg = McpServerConfig {
        repo_root: repo.to_path_buf(),
        default_viewport: Some("v1".into()),
    };
    let srv_task =
        tokio::spawn(async move { serve_mcp_stdio_with_streams(cfg, srv_in, srv_out).await });

    let (cr, _) = tokio::io::split(client_in);
    let (_, mut cw) = tokio::io::split(client_out);
    let mut lines = BufReader::new(cr).lines();
    let mut notifs = Vec::new();

    send_json_line(
        &mut cw,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "deadend-optout-test", "version": "0.1.0" }
            }
        }),
    )
    .await;
    let _ = recv_response_for_id(&mut lines, 1, &mut notifs).await;
    send_json_line(
        &mut cw,
        &serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;

    std::fs::write(vp1.path.join("src/lib.rs"), "pub fn calc() -> i32 { 777 }\n").unwrap();
    std::fs::write(vp1.path.join("src/notes.rs"), "pub fn scratch() {}\n").unwrap();

    send_json_line(
        &mut cw,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "loom_record_deadend",
                "arguments": {
                    "touched_symbols": ["calc"],
                    "failure_kind": "TestFailure",
                    "lesson_summary": "keep dirty files for inspection",
                    "restore_clean": false
                }
            }
        }),
    )
    .await;
    let deadend_resp = recv_response_for_id(&mut lines, 2, &mut notifs).await;
    assert_ne!(deadend_resp["result"]["isError"], true, "unexpected error: {deadend_resp}");
    let edge: NegativeEdge =
        serde_json::from_str(deadend_resp["result"]["content"][0]["text"].as_str().unwrap())
            .unwrap();
    assert_eq!(edge.failure_kind, FailureKind::TestFailure);

    // Dirty tracked & untracked files must be preserved
    assert_eq!(
        std::fs::read_to_string(vp1.path.join("src/lib.rs")).unwrap(),
        "pub fn calc() -> i32 { 777 }\n"
    );
    assert_eq!(
        std::fs::read_to_string(vp1.path.join("src/notes.rs")).unwrap(),
        "pub fn scratch() {}\n"
    );
    assert_eq!(
        running.ctx.db.get_viewport(&vp1.id).unwrap().unwrap().state,
        ViewportState::Active
    );

    send_json_line(
        &mut cw,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "loom_preflight_symbols",
                "arguments": {
                    "viewport": "v2",
                    "symbols": ["calc"]
                }
            }
        }),
    )
    .await;
    let preflight_resp = recv_response_for_id(&mut lines, 3, &mut notifs).await;
    let preflight: McpPreflightResult =
        serde_json::from_str(preflight_resp["result"]["content"][0]["text"].as_str().unwrap())
            .unwrap();
    assert_eq!(preflight.negative_warnings.len(), 1);
    assert_eq!(
        preflight.negative_warnings[0].lesson_summary,
        "keep dirty files for inspection"
    );

    cw.shutdown().await.unwrap();
    drop(cw);
    srv_task.await.unwrap().unwrap();
    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test]
async fn mcp_record_deadend_rejects_empty_501_unknown_and_non_active_viewports() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn item() -> i32 { 1 }\n").unwrap();

    let running = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    let vp_active = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "v_active".into(),
            intent: "active".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("{other:?}"),
    };
    let vp_sealed = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "v_sealed".into(),
            intent: "sealed".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("{other:?}"),
    };
    let vp_dropped = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "v_dropped".into(),
            intent: "dropped".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("{other:?}"),
    };
    let _vp_empty_syms = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "v_empty_syms".into(),
            intent: "empty syms".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("{other:?}"),
    };

    std::fs::write(vp_active.path.join("src/lib.rs"), "pub fn item() -> i32 { 555 }\n").unwrap();
    send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp_sealed.id.clone(),
            verify_cmd: None,
            summary: Some("seal".into()),
        },
    )
    .await
    .unwrap();
    send_rpc(
        repo,
        RpcMethod::DropViewport {
            viewport_id: vp_dropped.id.clone(),
            reason: "drop baseline".into(),
            kind: None,
        },
    )
    .await
    .unwrap();

    let base_edges = running.ctx.db.list_all_negative_edges().unwrap().len();
    assert_eq!(base_edges, 1);

    let (client_in, srv_out) = tokio::io::duplex(256 * 1024);
    let (srv_in, client_out) = tokio::io::duplex(256 * 1024);
    let cfg = McpServerConfig {
        repo_root: repo.to_path_buf(),
        default_viewport: None,
    };
    let srv_task =
        tokio::spawn(async move { serve_mcp_stdio_with_streams(cfg, srv_in, srv_out).await });

    let (cr, _) = tokio::io::split(client_in);
    let (_, mut cw) = tokio::io::split(client_out);
    let mut lines = BufReader::new(cr).lines();
    let mut notifs = Vec::new();

    send_json_line(
        &mut cw,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "deadend-validation-test", "version": "0.1.0" }
            }
        }),
    )
    .await;
    let _ = recv_response_for_id(&mut lines, 1, &mut notifs).await;
    send_json_line(
        &mut cw,
        &serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;

    let invalid_cases = vec![
        (
            2u64,
            serde_json::json!({
                "touched_symbols": ["item"],
                "lesson_summary": "missing viewport"
            }),
            "missing viewport without --viewport",
        ),
        (
            3u64,
            serde_json::json!({
                "viewport": "v_active",
                "touched_symbols": ["item"],
                "lesson_summary": ""
            }),
            "empty lesson_summary",
        ),
        (
            4u64,
            serde_json::json!({
                "viewport": "v_active",
                "touched_symbols": ["item"],
                "lesson_summary": "   \t\n  "
            }),
            "whitespace lesson_summary",
        ),
        (
            5u64,
            serde_json::json!({
                "viewport": "v_active",
                "touched_symbols": ["item"],
                "lesson_summary": "가".repeat(501)
            }),
            "501-char Unicode lesson_summary",
        ),
        (
            6u64,
            serde_json::json!({
                "viewport": "nonexistent-vp",
                "touched_symbols": ["item"],
                "lesson_summary": "unknown viewport"
            }),
            "unknown viewport",
        ),
        (
            7u64,
            serde_json::json!({
                "viewport": "v_sealed",
                "touched_symbols": ["item"],
                "lesson_summary": "cannot record on sealed"
            }),
            "Sealed viewport",
        ),
        (
            8u64,
            serde_json::json!({
                "viewport": "v_dropped",
                "touched_symbols": ["item"],
                "lesson_summary": "cannot record on dropped"
            }),
            "Dropped viewport",
        ),
    ];

    for (id, args, label) in invalid_cases {
        send_json_line(
            &mut cw,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": {
                    "name": "loom_record_deadend",
                    "arguments": args
                }
            }),
        )
        .await;
        let resp = recv_response_for_id(&mut lines, id, &mut notifs).await;
        assert_eq!(
            resp["result"]["isError"], true,
            "expected isError=true for {label}: {resp}"
        );
    }

    // Rejected calls must NOT persist any NegativeEdge and must NOT touch dirty files on v_active
    assert_eq!(
        running.ctx.db.list_all_negative_edges().unwrap().len(),
        base_edges
    );
    assert_eq!(
        std::fs::read_to_string(vp_active.path.join("src/lib.rs")).unwrap(),
        "pub fn item() -> i32 { 555 }\n"
    );

    // 500 Unicode chars on active viewport succeeds and restores clean by default
    let lesson_500 = "가".repeat(500);
    send_json_line(
        &mut cw,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "tools/call",
            "params": {
                "name": "loom_record_deadend",
                "arguments": {
                    "viewport": "v_active",
                    "touched_symbols": ["item"],
                    "lesson_summary": lesson_500
                }
            }
        }),
    )
    .await;
    let ok_500 = recv_response_for_id(&mut lines, 9, &mut notifs).await;
    assert_ne!(ok_500["result"]["isError"], true, "unexpected error for 500 chars: {ok_500}");
    let edge_500: NegativeEdge =
        serde_json::from_str(ok_500["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(edge_500.lesson_summary.chars().count(), 500);
    assert_eq!(
        std::fs::read_to_string(vp_active.path.join("src/lib.rs")).unwrap(),
        "pub fn item() -> i32 { 1 }\n"
    );

    // Empty/blank touched_symbols falls back cleanly without inserting blank keys
    send_json_line(
        &mut cw,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": {
                "name": "loom_record_deadend",
                "arguments": {
                    "viewport": "v_empty_syms",
                    "touched_symbols": ["", "   "],
                    "lesson_summary": "fallback symbol test"
                }
            }
        }),
    )
    .await;
    let ok_empty_syms = recv_response_for_id(&mut lines, 10, &mut notifs).await;
    assert_ne!(
        ok_empty_syms["result"]["isError"], true,
        "unexpected error for empty touched_symbols: {ok_empty_syms}"
    );
    let edge_fallback: NegativeEdge = serde_json::from_str(
        ok_empty_syms["result"]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert!(!edge_fallback.touched_symbols.is_empty());
    assert!(
        edge_fallback
            .touched_symbols
            .iter()
            .all(|s| !s.trim().is_empty()),
        "blank symbol leaked into touched_symbols: {:?}",
        edge_fallback.touched_symbols
    );

    cw.shutdown().await.unwrap();
    drop(cw);
    srv_task.await.unwrap().unwrap();

    let count_after_mcp = running.ctx.db.list_all_negative_edges().unwrap().len();
    assert_eq!(count_after_mcp, base_edges + 2);

    // Direct RPC McpRecordDeadend must also enforce exact validation & state parity
    let err_empty = send_rpc(
        repo,
        RpcMethod::McpRecordDeadend {
            viewport_id: vp_active.id.clone(),
            touched_symbols: vec!["item".into()],
            kind: "Other".into(),
            lesson_summary: "   ".into(),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err_empty.0, RpcErrorCode::InvalidRequest);

    let err_501 = send_rpc(
        repo,
        RpcMethod::McpRecordDeadend {
            viewport_id: vp_active.id.clone(),
            touched_symbols: vec!["item".into()],
            kind: "Other".into(),
            lesson_summary: "가".repeat(501),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err_501.0, RpcErrorCode::InvalidRequest);

    let err_unknown = send_rpc(
        repo,
        RpcMethod::McpRecordDeadend {
            viewport_id: "nonexistent-vp".into(),
            touched_symbols: vec!["item".into()],
            kind: "Other".into(),
            lesson_summary: "unknown".into(),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err_unknown.0, RpcErrorCode::NotFound);

    let err_sealed = send_rpc(
        repo,
        RpcMethod::McpRecordDeadend {
            viewport_id: vp_sealed.id.clone(),
            touched_symbols: vec!["item".into()],
            kind: "Other".into(),
            lesson_summary: "sealed".into(),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err_sealed.0, RpcErrorCode::InvalidState);

    let err_dropped = send_rpc(
        repo,
        RpcMethod::McpRecordDeadend {
            viewport_id: vp_dropped.id.clone(),
            touched_symbols: vec!["item".into()],
            kind: "Other".into(),
            lesson_summary: "dropped".into(),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err_dropped.0, RpcErrorCode::InvalidState);

    assert_eq!(
        running.ctx.db.list_all_negative_edges().unwrap().len(),
        count_after_mcp
    );

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test]
async fn mcp_record_deadend_then_retry_and_seal_picks_up_newer_trunk() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/a.rs"), "pub fn a() -> i32 { 1 }\n").unwrap();
    std::fs::write(repo.join("src/b.rs"), "pub fn b() -> i32 { 2 }\n").unwrap();

    let running = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    let vp1 = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "v1".into(),
            intent: "retry a after deadend".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("{other:?}"),
    };
    let vp2 = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "v2".into(),
            intent: "advance trunk b".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("{other:?}"),
    };

    // v1 makes a deadend edit to src/a.rs, while v2 seals an unrelated edit to src/b.rs onto trunk
    std::fs::write(vp1.path.join("src/a.rs"), "pub fn a() -> i32 { 999 }\n").unwrap();
    std::fs::write(vp2.path.join("src/b.rs"), "pub fn b() -> i32 { 22 }\n").unwrap();
    let cap_v2 = match send_rpc(
        repo,
        RpcMethod::SealCapsule {
            viewport_id: vp2.id.clone(),
            verify_cmd: None,
            summary: Some("seal b=22".into()),
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::CapsuleSeal(c) => c,
        other => panic!("{other:?}"),
    };

    let (client_in, srv_out) = tokio::io::duplex(256 * 1024);
    let (srv_in, client_out) = tokio::io::duplex(256 * 1024);
    let cfg = McpServerConfig {
        repo_root: repo.to_path_buf(),
        default_viewport: Some("v1".into()),
    };
    let srv_task =
        tokio::spawn(async move { serve_mcp_stdio_with_streams(cfg, srv_in, srv_out).await });

    let (cr, _) = tokio::io::split(client_in);
    let (_, mut cw) = tokio::io::split(client_out);
    let mut lines = BufReader::new(cr).lines();
    let mut notifs = Vec::new();

    send_json_line(
        &mut cw,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "deadend-retry-seal", "version": "0.1.0" }
            }
        }),
    )
    .await;
    let _ = recv_response_for_id(&mut lines, 1, &mut notifs).await;
    send_json_line(
        &mut cw,
        &serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;

    // v1 records deadend (omitting restore_clean) -> picks up cap_v2's newer trunk & clears 999
    send_json_line(
        &mut cw,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "loom_record_deadend",
                "arguments": {
                    "touched_symbols": ["a"],
                    "failure_kind": "DesignDeadEnd",
                    "lesson_summary": "999 exceeds domain bounds"
                }
            }
        }),
    )
    .await;
    let deadend_resp = recv_response_for_id(&mut lines, 2, &mut notifs).await;
    assert_ne!(deadend_resp["result"]["isError"], true, "unexpected error: {deadend_resp}");

    let vp1_refreshed = running.ctx.db.get_viewport(&vp1.id).unwrap().unwrap();
    assert_eq!(vp1_refreshed.state, ViewportState::Active);
    assert_eq!(
        vp1_refreshed.base_capsule_id,
        Some(cap_v2.capsule_id.clone())
    );
    assert_eq!(
        std::fs::read_to_string(vp1.path.join("src/a.rs")).unwrap(),
        "pub fn a() -> i32 { 1 }\n"
    );
    assert_eq!(
        std::fs::read_to_string(vp1.path.join("src/b.rs")).unwrap(),
        "pub fn b() -> i32 { 22 }\n"
    );

    // v1 retries with a valid fix and seals cleanly
    std::fs::write(vp1.path.join("src/a.rs"), "pub fn a() -> i32 { 11 }\n").unwrap();
    send_json_line(
        &mut cw,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "loom_seal_capsule",
                "arguments": {
                    "summary": "fix a=11"
                }
            }
        }),
    )
    .await;
    let seal_resp = recv_response_for_id(&mut lines, 3, &mut notifs).await;
    assert_ne!(seal_resp["result"]["isError"], true, "unexpected seal error: {seal_resp}");
    let cap_v1: CausalCapsule =
        serde_json::from_str(seal_resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(cap_v1.parent_capsule_ids, vec![cap_v2.capsule_id]);
    assert_eq!(
        running.ctx.db.get_viewport(&vp1.id).unwrap().unwrap().state,
        ViewportState::Sealed
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("src/a.rs")).unwrap(),
        "pub fn a() -> i32 { 11 }\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("src/b.rs")).unwrap(),
        "pub fn b() -> i32 { 22 }\n"
    );

    cw.shutdown().await.unwrap();
    drop(cw);
    srv_task.await.unwrap().unwrap();
    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mcp_record_deadend_is_atomic_under_concurrent_seal_and_verifying_state() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn v() -> i32 { 1 }\n").unwrap();

    let running = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    let mut expected_edges = 0usize;
    for i in 0..10 {
        let vp = match send_rpc(
            repo,
            RpcMethod::SpawnViewport {
                name: format!("race-{i}"),
                intent: "atomicity".into(),
                cmd: vec![],
            },
        )
        .await
        .unwrap()
        {
            RpcPayload::Viewport(v) => v,
            other => panic!("{other:?}"),
        };
        std::fs::write(
            vp.path.join("src/lib.rs"),
            format!("pub fn v() -> i32 {{ {} }}\n", 10 + i),
        )
        .unwrap();

        let (deadend_res, seal_res) = tokio::join!(
            send_rpc(
                repo,
                RpcMethod::McpRecordDeadendEx {
                    viewport_id: vp.id.clone(),
                    touched_symbols: vec!["v".into()],
                    kind: "TestFailure".into(),
                    lesson_summary: format!("deadend-{i}"),
                    restore_clean: true,
                }
            ),
            send_rpc(
                repo,
                RpcMethod::SealCapsule {
                    viewport_id: vp.id.clone(),
                    verify_cmd: None,
                    summary: Some(format!("seal-{i}")),
                }
            )
        );
        match (deadend_res, seal_res) {
            (Err((code, _)), Ok(_)) => {
                assert!(
                    matches!(code, RpcErrorCode::Conflict | RpcErrorCode::InvalidState),
                    "iter {i}: expected Conflict or InvalidState when seal wins, got {code:?}"
                );
            }
            (Ok(_), Ok(_)) => {
                // If McpRecordDeadendEx won trunk_seal_lock before SealCapsule claimed Verifying,
                // it leaves the viewport Active and SealCapsule may legitimately run afterward.
                // Atomicity requires NegativeEdgeRecord committed strictly before Verifying/CapsuleSeal.
                let ops = running.ctx.db.list_ops(false, None, None).unwrap();
                let neg_op = ops
                    .iter()
                    .find(|e| {
                        e.kind == loom_core::model::OpKind::NegativeEdgeRecord
                            && e.viewport_id.as_deref() == Some(vp.id.as_str())
                    })
                    .expect("NegativeEdgeRecord op must exist");
                let verify_op = ops
                    .iter()
                    .find(|e| {
                        e.kind == loom_core::model::OpKind::ViewportTransition
                            && e.viewport_id.as_deref() == Some(vp.id.as_str())
                            && e.summary.contains("Verifying viewport")
                    })
                    .expect("Verifying ViewportTransition op must exist");
                let seal_op = ops
                    .iter()
                    .find(|e| {
                        e.kind == loom_core::model::OpKind::CapsuleSeal
                            && e.viewport_id.as_deref() == Some(vp.id.as_str())
                    })
                    .expect("CapsuleSeal op must exist");
                assert!(
                    neg_op.op_seq < verify_op.op_seq && verify_op.op_seq < seal_op.op_seq,
                    "iter {i}: McpRecordDeadendEx must not commit during/after Verifying (neg={}, verify={}, seal={})",
                    neg_op.op_seq,
                    verify_op.op_seq,
                    seal_op.op_seq
                );
                expected_edges += 1;
            }
            (Ok(_), Err(_)) => {
                expected_edges += 1;
            }
            (Err(de), Err(se)) => {
                panic!("iter {i}: one of deadend or seal must succeed: deadend={de:?}, seal={se:?}");
            }
        }
    }

    assert_eq!(
        running.ctx.db.list_all_negative_edges().unwrap().len(),
        expected_edges
    );

    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test]
async fn mcp_record_deadend_empty_symbols_flushes_and_captures_unflushed_new_symbol() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/base.rs"), "pub fn existing_base() -> i32 { 1 }\n").unwrap();

    let running = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    let vp = match send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "v-dead".into(),
            intent: "try doomed algo".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap()
    {
        RpcPayload::Viewport(v) => v,
        other => panic!("{other:?}"),
    };

    let (client_in, srv_out) = tokio::io::duplex(256 * 1024);
    let (srv_in, client_out) = tokio::io::duplex(256 * 1024);
    let cfg = McpServerConfig {
        repo_root: repo.to_path_buf(),
        default_viewport: Some("v-dead".into()),
    };
    let srv_task =
        tokio::spawn(async move { serve_mcp_stdio_with_streams(cfg, srv_in, srv_out).await });

    let (cr, _) = tokio::io::split(client_in);
    let (_, mut cw) = tokio::io::split(client_out);
    let mut lines = BufReader::new(cr).lines();
    let mut notifs = Vec::new();

    send_json_line(
        &mut cw,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "deadend-flush-test", "version": "0.1.0" }
            }
        }),
    )
    .await;
    let _ = recv_response_for_id(&mut lines, 1, &mut notifs).await;
    send_json_line(
        &mut cw,
        &serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;

    // Write new file and immediately call loom_record_deadend with empty touched_symbols
    std::fs::write(vp.path.join("src/doomed.rs"), "pub fn doomed_algo() -> i32 { 42 }\n").unwrap();
    send_json_line(
        &mut cw,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "loom_record_deadend",
                "arguments": {
                    "touched_symbols": [],
                    "failure_kind": "DesignDeadEnd",
                    "lesson_summary": "doomed_algo overflows"
                }
            }
        }),
    )
    .await;
    let resp = recv_response_for_id(&mut lines, 2, &mut notifs).await;
    assert_ne!(resp["result"]["isError"], true, "unexpected error: {resp}");
    let edge: NegativeEdge =
        serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert!(
        edge.touched_symbols.iter().any(|s| s.contains("doomed_algo")),
        "expected doomed_algo in touched_symbols, got {:?}",
        edge.touched_symbols
    );
    assert!(
        !edge.touched_symbols.iter().any(|s| s.contains("existing_base")),
        "untouched symbol existing_base must not pollute NegativeEdge: {:?}",
        edge.touched_symbols
    );
    assert!(!vp.path.join("src/doomed.rs").exists());

    cw.shutdown().await.unwrap();
    drop(cw);
    srv_task.await.unwrap().unwrap();
    send_rpc(repo, RpcMethod::Shutdown).await.unwrap();
    assert_eq!(running.wait().await.unwrap(), DaemonRunExit::ShutdownRequested);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn record_deadend_empty_touched_symbols_scopes_to_dirty_files_only() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().to_path_buf();
    std::fs::create_dir_all(repo.join(".loom")).unwrap();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/a.rs"), "pub fn alpha() -> i32 { 1 }\n").unwrap();
    std::fs::write(repo.join("src/b.rs"), "pub fn beta() -> i32 { 2 }\n").unwrap();

    let running = match start_daemon(&repo, Some(60_000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("unexpected AlreadyRunning"),
    };
    let _ = send_rpc(&repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();
    let RpcPayload::Viewport(vp1) = send_rpc(
        &repo,
        RpcMethod::SpawnViewport {
            name: "vp1".into(),
            intent: "edit alpha".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap() else {
        panic!("expected viewport");
    };

    std::fs::write(
        vp1.path.join("src/a.rs"),
        "pub fn alpha() -> i32 { 1 }\npub fn newly_added_deadend() -> i32 { 9 }\n",
    )
    .unwrap();

    let RpcPayload::McpRecordDeadend(edge) = send_rpc(
        &repo,
        RpcMethod::McpRecordDeadend {
            viewport_id: vp1.id.clone(),
            touched_symbols: vec![],
            kind: "CompileError".into(),
            lesson_summary: "broken alpha extension".into(),
        },
    )
    .await
    .unwrap() else {
        panic!("expected NegativeEdge");
    };

    assert!(
        !edge.touched_symbols.iter().any(|s| s == "beta"),
        "untouched symbol `beta` must not pollute NegativeEdge: {:?}",
        edge.touched_symbols
    );
    assert!(
        edge.touched_symbols
            .iter()
            .any(|s| s == "newly_added_deadend"),
        "newly added dirty symbol must be captured: {:?}",
        edge.touched_symbols
    );

    let _ = send_rpc(&repo, RpcMethod::Shutdown).await;
    let _ = running.wait().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_stdio_delivers_preexisting_radar_conflict_after_initialize() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().to_path_buf();
    std::fs::create_dir_all(repo.join(".loom")).unwrap();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("src/lib.rs"),
        "pub fn shared() -> i32 {\n    1\n}\n",
    )
    .unwrap();

    let running = match start_daemon(&repo, Some(60_000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("unexpected AlreadyRunning"),
    };
    let ctx = std::sync::Arc::clone(&running.ctx);
    let _ = send_rpc(&repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    let RpcPayload::Viewport(vp1) = send_rpc(
        &repo,
        RpcMethod::SpawnViewport {
            name: "vp1".into(),
            intent: "edit 1".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap() else {
        panic!("expected viewport");
    };
    let RpcPayload::Viewport(vp2) = send_rpc(
        &repo,
        RpcMethod::SpawnViewport {
            name: "vp2".into(),
            intent: "edit 2".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap() else {
        panic!("expected viewport");
    };

    std::fs::write(
        vp1.path.join("src/lib.rs"),
        "pub fn shared() -> i32 {\n    10\n}\n",
    )
    .unwrap();
    std::fs::write(
        vp2.path.join("src/lib.rs"),
        "pub fn shared() -> i32 {\n    20\n}\n",
    )
    .unwrap();
    ctx.radar.flush_viewport(&vp1.id).await;
    ctx.radar.flush_viewport(&vp2.id).await;

    let (client_read, server_write) = tokio::io::duplex(64 * 1024);
    let (server_read, mut client_write) = tokio::io::duplex(64 * 1024);
    let cfg = McpServerConfig {
        repo_root: repo.clone(),
        default_viewport: Some("vp1".into()),
    };
    let srv = tokio::spawn(async move {
        serve_mcp_stdio_with_streams(cfg, server_read, server_write).await
    });

    tokio::time::sleep(Duration::from_millis(120)).await;
    let init_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "0.1" }
        }
    });
    client_write
        .write_all(format!("{init_req}\n").as_bytes())
        .await
        .unwrap();
    let mut reader = BufReader::new(client_read).lines();
    let init_line = reader.next_line().await.unwrap().unwrap();
    let init_val: Value = serde_json::from_str(&init_line).unwrap();
    assert_eq!(init_val["id"], 1);
    client_write
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .unwrap();

    let notif_line = tokio::time::timeout(Duration::from_secs(2), reader.next_line())
        .await
        .expect("timed out waiting for pre-existing radar_conflict notification")
        .unwrap()
        .unwrap();
    let notif: Value = serde_json::from_str(&notif_line).unwrap();
    assert_eq!(notif["method"], "notifications/loom/radar_conflict");

    drop(client_write);
    let _ = srv.await;
    let _ = send_rpc(&repo, RpcMethod::Shutdown).await;
    let _ = running.wait().await;
}

#[tokio::test]
async fn preflight_matches_bare_consumer_deadend_with_static_provides_without_false_instance_cross_match()
{
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::create_dir_all(repo.join("app")).unwrap();
    std::fs::write(
        repo.join("src/user.ts"),
        concat!(
            "export class User {\n",
            "    static parse(raw: string): User {\n",
            "        return new User(raw);\n",
            "    }\n",
            "    parse(raw: string): number {\n",
            "        return raw.length;\n",
            "    }\n",
            "}\n",
        ),
    )
    .unwrap();
    std::fs::write(
        repo.join("src/caller.ts"),
        "import { User } from './user';\nexport function run(): User {\n    return User.parse('x');\n}\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("src/static_only.ts"),
        "export class StaticOnly {\n    static parse(raw: string): string {\n        return raw;\n    }\n}\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("app/prop.py"),
        concat!(
            "class Temp:\n",
            "    @property\n",
            "    def c(self):\n",
            "        return self._c\n",
            "\n",
            "    @c.setter\n",
            "    def c(self, v):\n",
            "        self._c = v\n",
        ),
    )
    .unwrap();

    let running = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();

    let RpcPayload::Viewport(vp1) = send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "v1".into(),
            intent: "bare deadend".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap() else {
        panic!("expected viewport");
    };
    let RpcPayload::Viewport(vp2) = send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "v2".into(),
            intent: "instance deadend".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap() else {
        panic!("expected viewport");
    };
    let RpcPayload::Viewport(vp3) = send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "v3".into(),
            intent: "getter deadend".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap() else {
        panic!("expected viewport");
    };
    send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "v4".into(),
            intent: "preflight observer".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap();

    // Flush v4 so symbol_provides and symbol_consumes are indexed in state.redb
    running.ctx.radar.flush_viewport("v4").await;

    send_rpc(
        repo,
        RpcMethod::McpRecordDeadendEx {
            viewport_id: vp1.id.clone(),
            touched_symbols: vec!["parse".into()],
            kind: "DesignDeadEnd".into(),
            lesson_summary: "bare parse consumer deadend".into(),
            restore_clean: false,
        },
    )
    .await
    .unwrap();
    send_rpc(
        repo,
        RpcMethod::McpRecordDeadendEx {
            viewport_id: vp2.id.clone(),
            touched_symbols: vec!["User::parse".into()],
            kind: "DesignDeadEnd".into(),
            lesson_summary: "instance User::parse deadend".into(),
            restore_clean: false,
        },
    )
    .await
    .unwrap();
    send_rpc(
        repo,
        RpcMethod::McpRecordDeadendEx {
            viewport_id: vp3.id.clone(),
            touched_symbols: vec!["Temp::get:c".into()],
            kind: "DesignDeadEnd".into(),
            lesson_summary: "getter Temp::get:c deadend".into(),
            restore_clean: false,
        },
    )
    .await
    .unwrap();

    // 1. Querying User::static:parse returns static provides, bare consume `"parse"`, and bare `"parse"` deadend,
    //    while NOT returning instance `"User::parse"` deadend or provide.
    let RpcPayload::McpPreflight(static_res) = send_rpc(
        repo,
        RpcMethod::McpPreflightEx {
            viewport_id: Some("v4".into()),
            symbols: vec!["User::static:parse".into()],
            files: vec![],
        },
    )
    .await
    .unwrap() else {
        panic!("expected McpPreflight");
    };
    assert!(
        static_res
            .provides
            .iter()
            .any(|p| p.symbol_fqn == "User::static:parse")
    );
    assert!(
        !static_res
            .provides
            .iter()
            .any(|p| p.symbol_fqn == "User::parse")
    );
    assert!(
        static_res
            .consumes
            .iter()
            .any(|c| c.symbol_fqn == "parse")
    );
    assert!(
        static_res
            .negative_warnings
            .iter()
            .any(|w| w.lesson_summary == "bare parse consumer deadend"),
        "expected bare consumer deadend warning, got {:?}",
        static_res.negative_warnings
    );
    assert!(
        !static_res
            .negative_warnings
            .iter()
            .any(|w| w.lesson_summary == "instance User::parse deadend"),
        "User::static:parse must not match User::parse deadend: {:?}",
        static_res.negative_warnings
    );

    // 2. Querying Temp::set:c must NOT return Temp::get:c deadend or provide.
    let RpcPayload::McpPreflight(setter_res) = send_rpc(
        repo,
        RpcMethod::McpPreflightEx {
            viewport_id: Some("v4".into()),
            symbols: vec!["Temp::set:c".into()],
            files: vec![],
        },
    )
    .await
    .unwrap() else {
        panic!("expected McpPreflight");
    };
    assert!(
        setter_res
            .provides
            .iter()
            .any(|p| p.symbol_fqn == "Temp::set:c")
    );
    assert!(
        !setter_res
            .provides
            .iter()
            .any(|p| p.symbol_fqn == "Temp::get:c")
    );
    assert!(
        !setter_res
            .negative_warnings
            .iter()
            .any(|w| w.lesson_summary == "getter Temp::get:c deadend"),
        "Temp::set:c must not match Temp::get:c deadend: {:?}",
        setter_res.negative_warnings
    );

    // 3. File-only preflight on `src/static_only.ts` expands `StaticOnly::static:parse` and
    //    matches the unqualified `"parse"` deadend without falsely matching `"User::parse"`.
    let RpcPayload::McpPreflight(file_res) = send_rpc(
        repo,
        RpcMethod::McpPreflightEx {
            viewport_id: Some("v4".into()),
            symbols: vec![],
            files: vec!["src/static_only.ts".into()],
        },
    )
    .await
    .unwrap() else {
        panic!("expected McpPreflight");
    };
    assert!(
        file_res
            .negative_warnings
            .iter()
            .any(|w| w.lesson_summary == "bare parse consumer deadend"),
        "file expansion of StaticOnly::static:parse must match bare parse deadend: {:?}",
        file_res.negative_warnings
    );
    assert!(
        !file_res
            .negative_warnings
            .iter()
            .any(|w| w.lesson_summary == "instance User::parse deadend"),
        "StaticOnly::static:parse must not match User::parse deadend: {:?}",
        file_res.negative_warnings
    );

    let _ = send_rpc(repo, RpcMethod::Shutdown).await;
    let _ = running.wait().await;
}

#[tokio::test]
async fn preflight_file_pseudo_symbol_does_not_match_short_symbol_name_in_provides_or_consumes() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("src/lib.rs"),
        concat!(
            "pub fn rs() -> i32 {\n",
            "    42\n",
            "}\n",
            "pub fn caller() -> i32 {\n",
            "    rs()\n",
            "}\n",
        ),
    )
    .unwrap();

    let running = match start_daemon(repo, Some(5000)).await.unwrap() {
        StartDaemonOutcome::Started(r) => r,
        StartDaemonOutcome::AlreadyRunning => panic!("daemon should start"),
    };
    send_rpc(repo, RpcMethod::Init { colocate_git: false })
        .await
        .unwrap();
    send_rpc(
        repo,
        RpcMethod::SpawnViewport {
            name: "vp-rs".into(),
            intent: "preflight file guard".into(),
            cmd: vec![],
        },
    )
    .await
    .unwrap();

    running.ctx.radar.flush_viewport("vp-rs").await;

    let RpcPayload::McpPreflight(control) = send_rpc(
        repo,
        RpcMethod::McpPreflightEx {
            viewport_id: Some("vp-rs".into()),
            symbols: vec!["rs".into()],
            files: vec![],
        },
    )
    .await
    .unwrap() else {
        panic!("expected McpPreflight");
    };
    assert!(
        control.provides.iter().any(|p| p.symbol_fqn == "rs"),
        "control query for `rs` must find provides `rs`: {:?}",
        control.provides
    );
    assert!(
        control.consumes.iter().any(|c| c.symbol_fqn == "rs"),
        "control query for `rs` must find consumes `rs`: {:?}",
        control.consumes
    );

    let RpcPayload::McpPreflight(res) = send_rpc(
        repo,
        RpcMethod::McpPreflightEx {
            viewport_id: Some("vp-rs".into()),
            symbols: vec!["file:src/foo.rs".into()],
            files: vec![],
        },
    )
    .await
    .unwrap() else {
        panic!("expected McpPreflight");
    };
    assert!(
        res.provides.is_empty(),
        "file:src/foo.rs must not match provides symbol `rs`: {:?}",
        res.provides
    );
    assert!(
        res.consumes.is_empty(),
        "file:src/foo.rs must not match consumes symbol `rs`: {:?}",
        res.consumes
    );

    let _ = send_rpc(repo, RpcMethod::Shutdown).await;
    let _ = running.wait().await;
}
