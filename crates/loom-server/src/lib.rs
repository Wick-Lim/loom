use bytes::Bytes;
use http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, WWW_AUTHENTICATE};
use http::{Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use loom_core::cas::{CasError, CasStore};
use loom_core::db::{StateDb, TABLE_CAPSULES, TABLE_NEGATIVE_INDEX, TABLE_OP_LOG, TABLE_PATCHES};
use loom_core::model::{
    AstPatchRecord, Blake3Hash, CausalCapsule, NegativeEdge, RepoMeta, WorkspaceSnapshot, now_ns,
};
use loom_core::remote::{
    CasHasRequest, CasHasResponse, REMOTE_PROTOCOL_VERSION, RemoteErrorBody, RemotePullRequest,
    RemotePullResponse, RemotePushRequest, RemotePushResponse, RemoteRefState, RemoteRepoInfo,
    collect_required_cas_hashes, constant_time_eq, validate_repo_slug,
};
use redb::ReadableTable;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::convert::Infallible;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use tempfile::NamedTempFile;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    pub data_dir: PathBuf,
    pub tokens: Vec<String>,
    pub allow_unauthenticated: bool,
    pub allow_auto_create: bool,
    pub zstd_level: i32,
    pub max_body_bytes: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 9418)),
            data_dir: PathBuf::from("./loom-data"),
            tokens: Vec::new(),
            allow_unauthenticated: false,
            allow_auto_create: true,
            zstd_level: 3,
            max_body_bytes: 64 * 1024 * 1024,
        }
    }
}

pub struct ServerHandle {
    pub local_addr: SocketAddr,
    shutdown_tx: Option<oneshot::Sender<()>>,
    task: JoinHandle<anyhow::Result<()>>,
}

impl ServerHandle {
    pub async fn shutdown(mut self) -> anyhow::Result<()> {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        self.task.await?
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct ServerRefsFile {
    refs: BTreeMap<String, RemoteRefState>,
}

struct ServerState {
    config: ServerConfig,
    repo_locks: StdMutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl ServerState {
    fn repo_lock(&self, slug: &str) -> Arc<Mutex<()>> {
        let mut guard = self.repo_locks.lock().expect("repo lock mutex poisoned");
        Arc::clone(
            guard
                .entry(slug.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    fn is_authorized(&self, req: &Request<Incoming>) -> bool {
        if self.config.allow_unauthenticated {
            return true;
        }
        let Some(raw_header) = req.headers().get(AUTHORIZATION) else {
            return false;
        };
        let Ok(header_str) = raw_header.to_str() else {
            return false;
        };
        let trimmed = header_str.trim();
        let token_part = trimmed
            .strip_prefix("Bearer ")
            .or_else(|| trimmed.strip_prefix("bearer "))
            .map(str::trim);
        let Some(provided) = token_part else {
            return false;
        };
        self.config
            .tokens
            .iter()
            .any(|expected| constant_time_eq(expected.as_bytes(), provided.as_bytes()))
    }
}

pub async fn start_server(mut config: ServerConfig) -> anyhow::Result<ServerHandle> {
    config.tokens = config
        .tokens
        .into_iter()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();

    if config.tokens.is_empty() && !config.allow_unauthenticated {
        anyhow::bail!(
            "refusing to start loom-server without any Bearer token (pass --token / set LOOM_SERVER_TOKEN, or pass --no-auth to explicitly disable authentication)"
        );
    }
    if !(1..=22).contains(&config.zstd_level) {
        anyhow::bail!("zstd_level {} must be in 1..=22", config.zstd_level);
    }
    if config.max_body_bytes < 1024 {
        anyhow::bail!("max_body_bytes must be >= 1024");
    }

    std::fs::create_dir_all(config.data_dir.join("repos"))?;
    let listener = TcpListener::bind(config.bind).await?;
    let local_addr = listener.local_addr()?;

    let state = Arc::new(ServerState {
        config,
        repo_locks: StdMutex::new(HashMap::new()),
    });

    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => {
                    break;
                }
                accept_res = listener.accept() => {
                    let (stream, _peer) = match accept_res {
                        Ok(pair) => pair,
                        Err(err) => {
                            tracing::warn!(error = %err, "loom-server accept error");
                            continue;
                        }
                    };
                    let io = TokioIo::new(stream);
                    let st = Arc::clone(&state);
                    tokio::spawn(async move {
                        let service = service_fn(move |req| {
                            let st = Arc::clone(&st);
                            async move { Ok::<_, Infallible>(handle_request(req, &st).await) }
                        });
                        if let Err(err) = http1::Builder::new().serve_connection(io, service).await {
                            tracing::debug!(error = %err, "loom-server HTTP connection ended");
                        }
                    });
                }
            }
        }
        Ok(())
    });

    Ok(ServerHandle {
        local_addr,
        shutdown_tx: Some(shutdown_tx),
        task,
    })
}

#[derive(Debug, PartialEq, Eq)]
enum Route {
    Healthz,
    RepoInfo { slug: String },
    CasHas { slug: String },
    CasBlob { slug: String, hash_hex: String },
    Push { slug: String },
    Pull { slug: String },
}

fn parse_route(path: &str) -> Result<Option<Route>, String> {
    if path == "/healthz" {
        return Ok(Some(Route::Healthz));
    }
    let Some(rest) = path.strip_prefix("/v1/repos/") else {
        return Ok(None);
    };
    if let Some(raw_slug) = rest.strip_suffix("/info") {
        let slug = validate_repo_slug(raw_slug)?;
        return Ok(Some(Route::RepoInfo { slug }));
    }
    if let Some(raw_slug) = rest.strip_suffix("/cas/has") {
        let slug = validate_repo_slug(raw_slug)?;
        return Ok(Some(Route::CasHas { slug }));
    }
    if let Some(raw_slug) = rest.strip_suffix("/push") {
        let slug = validate_repo_slug(raw_slug)?;
        return Ok(Some(Route::Push { slug }));
    }
    if let Some(raw_slug) = rest.strip_suffix("/pull") {
        let slug = validate_repo_slug(raw_slug)?;
        return Ok(Some(Route::Pull { slug }));
    }
    if let Some((raw_slug, hash_hex)) = rest.rsplit_once("/cas/") {
        let slug = validate_repo_slug(raw_slug)?;
        if hash_hex.is_empty() || hash_hex.contains('/') {
            return Err("invalid CAS hash path segment".to_string());
        }
        return Ok(Some(Route::CasBlob {
            slug,
            hash_hex: hash_hex.to_string(),
        }));
    }
    Ok(None)
}

fn json_response<T: Serialize>(status: StatusCode, body: &T) -> Response<Full<Bytes>> {
    let bytes = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(bytes)))
        .expect("valid JSON response")
}

fn error_response(status: StatusCode, code: &str, message: impl Into<String>) -> Response<Full<Bytes>> {
    json_response(
        status,
        &RemoteErrorBody {
            code: code.to_string(),
            message: message.into(),
            remote_head_capsule_id: None,
            missing_cas_hashes: Vec::new(),
        },
    )
}

async fn read_body_limited(
    mut body: Incoming,
    limit: usize,
) -> Result<Vec<u8>, Response<Full<Bytes>>> {
    let mut out = Vec::new();
    while let Some(frame_res) = body.frame().await {
        let frame = frame_res.map_err(|e| {
            error_response(StatusCode::BAD_REQUEST, "bad_body", format!("failed to read body: {e}"))
        })?;
        if let Ok(data) = frame.into_data() {
            if out.len().saturating_add(data.len()) > limit {
                return Err(error_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "payload_too_large",
                    format!("request body exceeds {limit} byte limit"),
                ));
            }
            out.extend_from_slice(&data);
        }
    }
    Ok(out)
}

async fn handle_request(req: Request<Incoming>, state: &Arc<ServerState>) -> Response<Full<Bytes>> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    let route = match parse_route(&path) {
        Ok(Some(r)) => r,
        Ok(None) => return error_response(StatusCode::NOT_FOUND, "not_found", "unknown endpoint"),
        Err(msg) => return error_response(StatusCode::BAD_REQUEST, "invalid_path", msg),
    };

    if route == Route::Healthz {
        if method != Method::GET {
            return error_response(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed", "expected GET");
        }
        return json_response(
            StatusCode::OK,
            &serde_json::json!({
                "status": "ok",
                "server": "loom-server",
                "version": env!("CARGO_PKG_VERSION"),
                "protocol_version": REMOTE_PROTOCOL_VERSION,
            }),
        );
    }

    if !state.is_authorized(&req) {
        let mut resp = error_response(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "missing or invalid Bearer token",
        );
        resp.headers_mut().insert(
            WWW_AUTHENTICATE,
            http::HeaderValue::from_static("Bearer realm=\"loom-server\""),
        );
        return resp;
    }

    match route {
        Route::Healthz => unreachable!(),
        Route::RepoInfo { slug } => {
            if method != Method::GET {
                return error_response(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed", "expected GET");
            }
            handle_repo_info(state, &slug).await
        }
        Route::CasHas { slug } => {
            if method != Method::POST {
                return error_response(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed", "expected POST");
            }
            let body = match read_body_limited(req.into_body(), state.config.max_body_bytes).await {
                Ok(b) => b,
                Err(resp) => return resp,
            };
            handle_cas_has(state, &slug, body).await
        }
        Route::CasBlob { slug, hash_hex } => match method {
            Method::PUT => {
                let is_zstd = req
                    .headers()
                    .get(CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|s| s.to_ascii_lowercase().contains("zstd"));
                let body = match read_body_limited(req.into_body(), state.config.max_body_bytes).await {
                    Ok(b) => b,
                    Err(resp) => return resp,
                };
                handle_cas_put(state, &slug, &hash_hex, is_zstd, body).await
            }
            Method::GET => {
                let wants_zstd = req
                    .headers()
                    .get(ACCEPT)
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|s| s.to_ascii_lowercase().contains("zstd"));
                handle_cas_get(state, &slug, &hash_hex, wants_zstd).await
            }
            _ => error_response(
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
                "expected GET or PUT",
            ),
        },
        Route::Push { slug } => {
            if method != Method::POST {
                return error_response(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed", "expected POST");
            }
            let body = match read_body_limited(req.into_body(), state.config.max_body_bytes).await {
                Ok(b) => b,
                Err(resp) => return resp,
            };
            handle_push(state, &slug, body).await
        }
        Route::Pull { slug } => {
            if method != Method::POST {
                return error_response(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed", "expected POST");
            }
            let body = match read_body_limited(req.into_body(), state.config.max_body_bytes).await {
                Ok(b) => b,
                Err(resp) => return resp,
            };
            handle_pull(state, &slug, body).await
        }
    }
}

fn repo_storage_dir(data_dir: &Path, slug: &str) -> PathBuf {
    let mut dir = data_dir.join("repos");
    for seg in slug.split('/') {
        dir.push(seg);
    }
    dir
}

fn repo_exists(repo_dir: &Path) -> bool {
    repo_dir.join("state.redb").is_file()
}

fn open_repo_db_and_cas(
    repo_dir: &Path,
    slug: &str,
    zstd_level: i32,
    create: bool,
) -> Result<(StateDb, CasStore, ServerRefsFile), (StatusCode, String, String)> {
    if !create && !repo_exists(repo_dir) {
        return Err((
            StatusCode::NOT_FOUND,
            "repo_not_found".to_string(),
            format!("remote repository `{slug}` does not exist"),
        ));
    }
    std::fs::create_dir_all(repo_dir).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "io_error".to_string(),
            e.to_string(),
        )
    })?;
    let cas = CasStore::new(repo_dir.join("cas"), zstd_level).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "cas_error".to_string(),
            e.to_string(),
        )
    })?;
    let db = StateDb::open_or_create(&repo_dir.join("state.redb")).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "db_error".to_string(),
            e.to_string(),
        )
    })?;
    let now = now_ns();
    let default_meta = RepoMeta {
        schema_version: 1,
        repo_id: format!("srv-{}", Blake3Hash::from_bytes(slug.as_bytes()).short_hex(16)),
        colocate_git: false,
        next_lsn: 1,
        next_op_seq: 1,
        next_edge_seq: 1,
        last_applied_lsn: 0,
        last_checkpoint_lsn: 0,
        lamport_clock: 0,
        head_capsule_id: None,
        created_at_ns: now,
        updated_at_ns: now,
    };
    db.init_repo_meta(&default_meta).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "db_error".to_string(),
            e.to_string(),
        )
    })?;

    let refs_path = repo_dir.join("refs.json");
    let refs = if refs_path.is_file() {
        let raw = std::fs::read(&refs_path).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "io_error".to_string(),
                e.to_string(),
            )
        })?;
        serde_json::from_slice(&raw).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "refs_corrupt".to_string(),
                e.to_string(),
            )
        })?
    } else {
        ServerRefsFile::default()
    };

    Ok((db, cas, refs))
}

fn save_refs_atomic(repo_dir: &Path, refs: &ServerRefsFile) -> Result<(), String> {
    let refs_path = repo_dir.join("refs.json");
    let json = serde_json::to_vec_pretty(refs).map_err(|e| e.to_string())?;
    let mut tmp = NamedTempFile::new_in(repo_dir).map_err(|e| e.to_string())?;
    tmp.write_all(&json).map_err(|e| e.to_string())?;
    tmp.as_file().sync_data().map_err(|e| e.to_string())?;
    tmp.persist(&refs_path).map_err(|e| e.to_string())?;
    Ok(())
}

fn validate_branch_name(branch: &str) -> Result<String, String> {
    let trimmed = branch.trim().strip_prefix("refs/heads/").unwrap_or(branch.trim());
    if trimmed.is_empty()
        || trimmed.contains("..")
        || trimmed.contains('\\')
        || trimmed.starts_with('/')
        || trimmed.ends_with('/')
        || trimmed.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(format!("invalid branch name `{branch}`"));
    }
    Ok(trimmed.to_string())
}

async fn handle_repo_info(state: &Arc<ServerState>, slug: &str) -> Response<Full<Bytes>> {
    let lock = state.repo_lock(slug);
    let _guard = lock.lock().await;
    let repo_dir = repo_storage_dir(&state.config.data_dir, slug);
    let slug_owned = slug.to_string();
    let zstd_level = state.config.zstd_level;

    let res = tokio::task::spawn_blocking(move || -> Result<RemoteRepoInfo, (StatusCode, String, String)> {
        if !repo_exists(&repo_dir) {
            return Ok(RemoteRepoInfo {
                repo: slug_owned,
                exists: false,
                ..Default::default()
            });
        }
        let (db, cas, refs) = open_repo_db_and_cas(&repo_dir, &slug_owned, zstd_level, false)?;
        let capsules = db.list_capsules().map_err(|e| {
            (StatusCode::INTERNAL_SERVER_ERROR, "db_error".into(), e.to_string())
        })?;
        let patches = db.list_patches().map_err(|e| {
            (StatusCode::INTERNAL_SERVER_ERROR, "db_error".into(), e.to_string())
        })?;
        let edges = db.list_all_negative_edges().map_err(|e| {
            (StatusCode::INTERNAL_SERVER_ERROR, "db_error".into(), e.to_string())
        })?;
        let cas_hashes = cas.list_hashes().map_err(|e| {
            (StatusCode::INTERNAL_SERVER_ERROR, "cas_error".into(), e.to_string())
        })?;
        Ok(RemoteRepoInfo {
            repo: slug_owned,
            exists: true,
            refs: refs.refs,
            capsule_count: capsules.len() as u64,
            patch_count: patches.len() as u64,
            negative_edge_count: edges.len() as u64,
            cas_object_count: cas_hashes.len() as u64,
        })
    })
    .await;

    match res {
        Ok(Ok(info)) => json_response(StatusCode::OK, &info),
        Ok(Err((st, code, msg))) => error_response(st, &code, msg),
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "join_error", e.to_string()),
    }
}

async fn handle_cas_has(
    state: &Arc<ServerState>,
    slug: &str,
    body: Vec<u8>,
) -> Response<Full<Bytes>> {
    let req: CasHasRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, "invalid_json", e.to_string()),
    };

    let lock = state.repo_lock(slug);
    let _guard = lock.lock().await;
    let repo_dir = repo_storage_dir(&state.config.data_dir, slug);
    let slug_owned = slug.to_string();
    let zstd_level = state.config.zstd_level;
    let allow_create = state.config.allow_auto_create;

    let res = tokio::task::spawn_blocking(move || -> Result<CasHasResponse, (StatusCode, String, String)> {
        let (_db, cas, _refs) = open_repo_db_and_cas(&repo_dir, &slug_owned, zstd_level, allow_create)?;
        let mut missing = Vec::new();
        let mut seen = BTreeSet::new();
        for h in req.hashes {
            if seen.insert(h) && !cas.contains(&h) {
                missing.push(h);
            }
        }
        Ok(CasHasResponse { missing })
    })
    .await;

    match res {
        Ok(Ok(resp)) => json_response(StatusCode::OK, &resp),
        Ok(Err((st, code, msg))) => error_response(st, &code, msg),
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "join_error", e.to_string()),
    }
}

async fn handle_cas_put(
    state: &Arc<ServerState>,
    slug: &str,
    hash_hex: &str,
    is_zstd: bool,
    body: Vec<u8>,
) -> Response<Full<Bytes>> {
    let expected = match Blake3Hash::from_hex(hash_hex) {
        Ok(h) => h,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, "invalid_hash", e.to_string()),
    };

    let lock = state.repo_lock(slug);
    let _guard = lock.lock().await;
    let repo_dir = repo_storage_dir(&state.config.data_dir, slug);
    let slug_owned = slug.to_string();
    let zstd_level = state.config.zstd_level;
    let allow_create = state.config.allow_auto_create;

    let res = tokio::task::spawn_blocking(move || -> Result<bool, (StatusCode, String, String)> {
        let (_db, cas, _refs) = open_repo_db_and_cas(&repo_dir, &slug_owned, zstd_level, allow_create)?;
        if is_zstd {
            match cas.put_verified_compressed(&expected, &body) {
                Ok(created) => Ok(created),
                Err(CasError::HashMismatch { expected, actual }) => Err((
                    StatusCode::BAD_REQUEST,
                    "hash_mismatch".to_string(),
                    format!("CAS hash mismatch: expected {expected}, actual {actual}"),
                )),
                Err(CasError::Io(e)) => Err((
                    StatusCode::BAD_REQUEST,
                    "invalid_zstd".to_string(),
                    format!("invalid zstd CAS blob: {e}"),
                )),
                Err(e) => Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "cas_error".to_string(),
                    e.to_string(),
                )),
            }
        } else {
            let actual = Blake3Hash::from_bytes(&body);
            if actual != expected {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "hash_mismatch".to_string(),
                    format!("CAS hash mismatch: expected {expected}, actual {actual}"),
                ));
            }
            if cas.contains(&expected) {
                return Ok(false);
            }
            cas.put_bytes(&body).map_err(|e| {
                (StatusCode::INTERNAL_SERVER_ERROR, "cas_error".to_string(), e.to_string())
            })?;
            Ok(true)
        }
    })
    .await;

    match res {
        Ok(Ok(created)) => json_response(
            if created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            },
            &serde_json::json!({
                "hash": expected,
                "created": created,
            }),
        ),
        Ok(Err((st, code, msg))) => error_response(st, &code, msg),
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "join_error", e.to_string()),
    }
}

async fn handle_cas_get(
    state: &Arc<ServerState>,
    slug: &str,
    hash_hex: &str,
    wants_zstd: bool,
) -> Response<Full<Bytes>> {
    let expected = match Blake3Hash::from_hex(hash_hex) {
        Ok(h) => h,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, "invalid_hash", e.to_string()),
    };

    let repo_dir = repo_storage_dir(&state.config.data_dir, slug);
    let slug_owned = slug.to_string();
    let zstd_level = state.config.zstd_level;

    let res = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, (StatusCode, String, String)> {
        if !repo_exists(&repo_dir) {
            return Err((
                StatusCode::NOT_FOUND,
                "repo_not_found".to_string(),
                format!("remote repository `{slug_owned}` does not exist"),
            ));
        }
        let cas = CasStore::new(repo_dir.join("cas"), zstd_level).map_err(|e| {
            (StatusCode::INTERNAL_SERVER_ERROR, "cas_error".into(), e.to_string())
        })?;
        let out = if wants_zstd {
            cas.get_raw_compressed(&expected)
        } else {
            cas.get_bytes(&expected)
        };
        out.map_err(|e| match e {
            CasError::NotFound(_) => (
                StatusCode::NOT_FOUND,
                "cas_not_found".to_string(),
                e.to_string(),
            ),
            CasError::HashMismatch { .. } => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "cas_corrupt".to_string(),
                e.to_string(),
            ),
            other => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "cas_error".to_string(),
                other.to_string(),
            ),
        })
    })
    .await;

    match res {
        Ok(Ok(bytes)) => Response::builder()
            .status(StatusCode::OK)
            .header(
                CONTENT_TYPE,
                if wants_zstd {
                    "application/zstd"
                } else {
                    "application/octet-stream"
                },
            )
            .body(Full::new(Bytes::from(bytes)))
            .expect("valid binary response"),
        Ok(Err((st, code, msg))) => error_response(st, &code, msg),
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "join_error", e.to_string()),
    }
}

fn verify_server_cas_completeness(
    cas: &CasStore,
    req: &RemotePushRequest,
) -> Result<Vec<Blake3Hash>, String> {
    let mut missing: BTreeSet<Blake3Hash> = BTreeSet::new();
    let mut snap_hashes: BTreeSet<Blake3Hash> = BTreeSet::new();

    if let Some(h) = req.new_head_snapshot_cas
        && h != Blake3Hash::ZERO
    {
        snap_hashes.insert(h);
    }
    for cap in &req.capsules {
        if let Some(h) = cap.snapshot_cas_hash
            && h != Blake3Hash::ZERO
        {
            snap_hashes.insert(h);
        }
        if let Some(p) = cap.proof_cas_hash
            && p != Blake3Hash::ZERO
            && !cas.contains(&p)
        {
            missing.insert(p);
        }
    }

    for snap_hash in snap_hashes {
        if !cas.contains(&snap_hash) {
            missing.insert(snap_hash);
            continue;
        }
        let raw = cas.get_bytes(&snap_hash).map_err(|e| e.to_string())?;
        let snapshot: WorkspaceSnapshot =
            rmp_serde::from_slice(&raw).map_err(|e| format!("invalid snapshot manifest: {e}"))?;
        for fh in snapshot.files.values() {
            if *fh != Blake3Hash::ZERO && !cas.contains(fh) {
                missing.insert(*fh);
            }
        }
    }

    Ok(missing.into_iter().collect())
}

async fn handle_push(
    state: &Arc<ServerState>,
    slug: &str,
    body: Vec<u8>,
) -> Response<Full<Bytes>> {
    let req: RemotePushRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, "invalid_json", e.to_string()),
    };
    let branch = match validate_branch_name(&req.branch) {
        Ok(b) => b,
        Err(msg) => return error_response(StatusCode::BAD_REQUEST, "invalid_branch", msg),
    };
    for edge in &req.negative_edges {
        if let Err(err) = edge.validate() {
            return error_response(StatusCode::BAD_REQUEST, "invalid_negative_edge", err.to_string());
        }
    }

    let lock = state.repo_lock(slug);
    let _guard = lock.lock().await;
    let repo_dir = repo_storage_dir(&state.config.data_dir, slug);
    let slug_owned = slug.to_string();
    let zstd_level = state.config.zstd_level;
    let allow_create = state.config.allow_auto_create;

    let res = tokio::task::spawn_blocking(move || -> Result<RemotePushResponse, (StatusCode, RemoteErrorBody)> {
        let (db, cas, mut refs) =
            open_repo_db_and_cas(&repo_dir, &slug_owned, zstd_level, allow_create).map_err(
                |(st, code, msg)| {
                    (
                        st,
                        RemoteErrorBody {
                            code,
                            message: msg,
                            ..Default::default()
                        },
                    )
                },
            )?;

        let current_ref = refs.refs.get(&branch).cloned().unwrap_or_default();
        if !req.force && req.expected_remote_head != current_ref.head_capsule_id {
            return Err((
                StatusCode::CONFLICT,
                RemoteErrorBody {
                    code: "non_fast_forward".to_string(),
                    message: format!(
                        "non-fast-forward push on `{branch}`: expected remote head {:?}, actual {:?}",
                        req.expected_remote_head, current_ref.head_capsule_id
                    ),
                    remote_head_capsule_id: current_ref.head_capsule_id,
                    missing_cas_hashes: Vec::new(),
                },
            ));
        }

        let missing_cas = verify_server_cas_completeness(&cas, &req).map_err(|msg| {
            (
                StatusCode::BAD_REQUEST,
                RemoteErrorBody {
                    code: "invalid_snapshot".to_string(),
                    message: msg,
                    ..Default::default()
                },
            )
        })?;
        if !missing_cas.is_empty() {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                RemoteErrorBody {
                    code: "missing_cas".to_string(),
                    message: format!("{} required CAS blob(s) missing on server", missing_cas.len()),
                    remote_head_capsule_id: current_ref.head_capsule_id,
                    missing_cas_hashes: missing_cas,
                },
            ));
        }

        let mut existing_edge_ids: HashSet<String> = db
            .list_all_negative_edges()
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    RemoteErrorBody {
                        code: "db_error".to_string(),
                        message: e.to_string(),
                        ..Default::default()
                    },
                )
            })?
            .into_iter()
            .map(|e| e.edge_id)
            .collect();

        let now = now_ns();
        let mut accepted_capsules = 0usize;
        let mut accepted_patches = 0usize;
        let mut accepted_negative_edges = 0usize;

        let wtx = db.begin_write().map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                RemoteErrorBody {
                    code: "db_error".to_string(),
                    message: e.to_string(),
                    ..Default::default()
                },
            )
        })?;

        {
            let mut op_table = wtx.open_table(TABLE_OP_LOG).map_err(db_err)?;
            let mut meta = StateDb::get_repo_meta_in_tx(&op_table)
                .map_err(db_err)?
                .unwrap_or_default();
            let mut cap_table = wtx.open_table(TABLE_CAPSULES).map_err(db_err)?;
            let mut patch_table = wtx.open_table(TABLE_PATCHES).map_err(db_err)?;
            let mut neg_table = wtx.open_table(TABLE_NEGATIVE_INDEX).map_err(db_err)?;

            for cap in &req.capsules {
                if cap_table.get(cap.capsule_id.as_str()).map_err(db_err)?.is_none() {
                    StateDb::upsert_capsule_in_tx(&mut cap_table, cap).map_err(db_err)?;
                    accepted_capsules += 1;
                }
            }
            for patch in &req.patches {
                if patch_table
                    .get(patch.patch_id.as_str())
                    .map_err(db_err)?
                    .is_none()
                {
                    StateDb::upsert_patch_in_tx(&mut patch_table, patch).map_err(db_err)?;
                    accepted_patches += 1;
                }
            }
            for raw_edge in &req.negative_edges {
                if !existing_edge_ids.insert(raw_edge.edge_id.clone()) {
                    continue;
                }
                let mut edge = raw_edge.clone();
                let seq = meta.next_edge_seq.max(1);
                meta.next_edge_seq = seq + 1;
                edge.edge_seq = seq;
                if edge.touched_symbols.is_empty() {
                    edge.touched_symbols.push("__repo__".to_string());
                }
                let dedup_syms: BTreeSet<String> = edge.touched_symbols.iter().cloned().collect();
                for sym in dedup_syms {
                    StateDb::insert_negative_edge_in_tx(&mut neg_table, &sym, &edge).map_err(db_err)?;
                }
                accepted_negative_edges += 1;
            }

            if let Some(ref new_head) = req.new_head_capsule_id {
                meta.head_capsule_id = Some(new_head.clone());
            }
            meta.updated_at_ns = now;
            StateDb::set_repo_meta_in_tx(&mut op_table, &meta).map_err(db_err)?;
        }
        wtx.commit().map_err(db_err)?;

        let final_head_capsule = req
            .new_head_capsule_id
            .clone()
            .or(current_ref.head_capsule_id);
        let final_head_snap = req
            .new_head_snapshot_cas
            .or(current_ref.head_snapshot_cas);

        refs.refs.insert(
            branch.clone(),
            RemoteRefState {
                head_capsule_id: final_head_capsule.clone(),
                head_snapshot_cas: final_head_snap,
                updated_at_ns: now,
            },
        );
        save_refs_atomic(&repo_dir, &refs).map_err(|msg| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                RemoteErrorBody {
                    code: "io_error".to_string(),
                    message: msg,
                    ..Default::default()
                },
            )
        })?;

        Ok(RemotePushResponse {
            repo: slug_owned,
            branch,
            head_capsule_id: final_head_capsule,
            head_snapshot_cas: final_head_snap,
            accepted_capsules,
            accepted_patches,
            accepted_negative_edges,
        })
    })
    .await;

    match res {
        Ok(Ok(resp)) => json_response(StatusCode::OK, &resp),
        Ok(Err((st, body))) => json_response(st, &body),
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "join_error", e.to_string()),
    }
}

fn db_err<E: std::fmt::Display>(err: E) -> (StatusCode, RemoteErrorBody) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        RemoteErrorBody {
            code: "db_error".to_string(),
            message: err.to_string(),
            ..Default::default()
        },
    )
}

fn sort_capsules_topologically(mut capsules: Vec<CausalCapsule>) -> Vec<CausalCapsule> {
    capsules.sort_by(|a, b| {
        a.created_at_ns
            .cmp(&b.created_at_ns)
            .then_with(|| a.capsule_id.cmp(&b.capsule_id))
    });
    if capsules.len() <= 1 {
        return capsules;
    }

    let by_id: HashMap<String, CausalCapsule> = capsules
        .into_iter()
        .map(|c| (c.capsule_id.clone(), c))
        .collect();

    let mut in_degree: HashMap<String, usize> = HashMap::new();
    let mut children: HashMap<String, Vec<String>> = HashMap::new();
    for (id, cap) in &by_id {
        let mut deg = 0usize;
        for p in &cap.parent_capsule_ids {
            if by_id.contains_key(p) && p != id {
                deg += 1;
                children.entry(p.clone()).or_default().push(id.clone());
            }
        }
        in_degree.insert(id.clone(), deg);
    }

    let mut ready: BTreeSet<(u64, String)> = by_id
        .iter()
        .filter(|(id, _)| in_degree.get(*id).copied().unwrap_or(0) == 0)
        .map(|(id, cap)| (cap.created_at_ns, id.clone()))
        .collect();

    let mut ordered = Vec::with_capacity(by_id.len());
    let mut emitted = HashSet::new();

    while let Some((_, cid)) = ready.pop_first() {
        if !emitted.insert(cid.clone()) {
            continue;
        }
        if let Some(cap) = by_id.get(&cid) {
            ordered.push(cap.clone());
        }
        if let Some(ch_list) = children.get(&cid) {
            for ch in ch_list {
                if let Some(deg) = in_degree.get_mut(ch) {
                    *deg = deg.saturating_sub(1);
                    if *deg == 0
                        && let Some(ch_cap) = by_id.get(ch)
                    {
                        ready.insert((ch_cap.created_at_ns, ch.clone()));
                    }
                }
            }
        }
    }

    if ordered.len() < by_id.len() {
        let mut rest: Vec<CausalCapsule> = by_id
            .into_iter()
            .filter(|(id, _)| !emitted.contains(id))
            .map(|(_, c)| c)
            .collect();
        rest.sort_by(|a, b| {
            a.created_at_ns
                .cmp(&b.created_at_ns)
                .then_with(|| a.capsule_id.cmp(&b.capsule_id))
        });
        ordered.extend(rest);
    }

    ordered
}

async fn handle_pull(
    state: &Arc<ServerState>,
    slug: &str,
    body: Vec<u8>,
) -> Response<Full<Bytes>> {
    let req: RemotePullRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, "invalid_json", e.to_string()),
    };
    let branch = match validate_branch_name(&req.branch) {
        Ok(b) => b,
        Err(msg) => return error_response(StatusCode::BAD_REQUEST, "invalid_branch", msg),
    };

    let lock = state.repo_lock(slug);
    let _guard = lock.lock().await;
    let repo_dir = repo_storage_dir(&state.config.data_dir, slug);
    let slug_owned = slug.to_string();
    let zstd_level = state.config.zstd_level;

    let res = tokio::task::spawn_blocking(move || -> Result<RemotePullResponse, (StatusCode, String, String)> {
        let (db, cas, refs) = open_repo_db_and_cas(&repo_dir, &slug_owned, zstd_level, false)?;
        let ref_state = refs.refs.get(&branch).cloned().unwrap_or_default();

        let known_caps: HashSet<String> = req.known_capsule_ids.into_iter().collect();
        let known_patches: HashSet<String> = req.known_patch_ids.into_iter().collect();
        let known_edges: HashSet<String> = req.known_negative_edge_ids.into_iter().collect();
        let known_cas: HashSet<Blake3Hash> = req.known_cas_hashes.into_iter().collect();

        let capsules_raw: Vec<CausalCapsule> = db
            .list_capsules()
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, "db_error".into(), e.to_string()))?
            .into_iter()
            .filter(|c| !known_caps.contains(&c.capsule_id))
            .collect();
        let capsules = sort_capsules_topologically(capsules_raw);

        let mut patches: Vec<AstPatchRecord> = db
            .list_patches()
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, "db_error".into(), e.to_string()))?
            .into_iter()
            .filter(|p| !known_patches.contains(&p.patch_id))
            .collect();
        patches.sort_by(|a, b| a.lsn.cmp(&b.lsn).then_with(|| a.patch_id.cmp(&b.patch_id)));

        let mut negative_edges: Vec<NegativeEdge> = db
            .list_all_negative_edges()
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, "db_error".into(), e.to_string()))?
            .into_iter()
            .filter(|e| !known_edges.contains(&e.edge_id))
            .collect();
        negative_edges.sort_by(|a, b| a.edge_seq.cmp(&b.edge_seq).then_with(|| a.edge_id.cmp(&b.edge_id)));

        let mut root_snaps = Vec::new();
        if let Some(h) = ref_state.head_snapshot_cas {
            root_snaps.push(h);
        }
        let all_required_cas = collect_required_cas_hashes(&cas, &root_snaps, &capsules, &patches)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, "cas_error".into(), e.to_string()))?;
        let required_cas_hashes: Vec<Blake3Hash> = all_required_cas
            .into_iter()
            .filter(|h| !known_cas.contains(h))
            .collect();

        Ok(RemotePullResponse {
            repo: slug_owned,
            branch,
            head_capsule_id: ref_state.head_capsule_id,
            head_snapshot_cas: ref_state.head_snapshot_cas,
            capsules,
            patches,
            negative_edges,
            required_cas_hashes,
        })
    })
    .await;

    match res {
        Ok(Ok(resp)) => json_response(StatusCode::OK, &resp),
        Ok(Err((st, code, msg))) => error_response(st, &code, msg),
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "join_error", e.to_string()),
    }
}
