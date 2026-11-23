use crate::{WriterCommand, WriterHandle};
use loom_core::cas::CasStore;
use loom_core::config::LoomConfig;
use loom_core::cst::{
    CstError, DiffContext, ExtractedSymbols, compute_logical_file_id,
    detect_divergent_base_conflicts, diff_file_to_patch, extract_file_symbols, merge_patches,
    parse_source_incremental, ts_language,
};
use loom_core::db::{StateDb, symbol_matches_query};
use loom_core::model::{
    AstPatch, Blake3Hash, LanguageKind, MergeOutcome, NegativeEdge, RadarConflictItem,
    RadarLevel, RadarStatusReport, SemanticOp, SymbolConsumeEntry, SymbolContractDelta,
    SymbolProvideEntry, SymbolStatus, ViewportRecord, WorkspaceSnapshot, now_ns,
};
use loom_core::rpc::{
    RadarBatchMetrics, RadarMatrixCell, RadarSnapshot, RadarStreamEvent, RadarViewportSummary,
};
use notify::{Event, EventHandler, RecursiveMode, RecommendedWatcher, Watcher};
use notify_debouncer_full::{
    Config as DebouncerConfig, DebounceEventResult, Debouncer, FileIdCache, RecommendedCache,
    WatchRoot, new_debouncer_opt,
};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tempfile::NamedTempFile;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tree_sitter::{Parser, Tree};

// ============================================================================
// Path Normalization & Pre-Debounce Filter
// ============================================================================

pub fn strip_windows_verbatim_prefix(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path.to_path_buf()
    }
}

fn normalize_path_key(path: &Path) -> String {
    let stripped = strip_windows_verbatim_prefix(path);
    let mut s = stripped.to_string_lossy().replace('\\', "/");
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
}

fn component_ignored(name: &str, ignore_dirs: &[String]) -> bool {
    if name == ".loom" || name == ".git" {
        return true;
    }
    if cfg!(windows) {
        ignore_dirs.iter().any(|ig| ig.eq_ignore_ascii_case(name))
    } else {
        ignore_dirs.iter().any(|ig| ig == name)
    }
}

pub fn is_ignored_relative_path(rel: &Path, ignore_dirs: &[String]) -> bool {
    for comp in rel.components() {
        if let Component::Normal(os) = comp
            && let Some(s) = os.to_str()
            && component_ignored(s, ignore_dirs)
        {
            return true;
        }
    }
    false
}

fn normalize_rel_path(rel: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in rel.components() {
        if let Component::Normal(os) = comp {
            out.push(os);
        }
    }
    out
}

fn relative_by_component_skip(stripped_abs: &Path, stripped_root: &Path) -> PathBuf {
    let skip_count = stripped_root.components().count();
    let mut out = PathBuf::new();
    for comp in stripped_abs.components().skip(skip_count) {
        if let Component::Normal(os) = comp {
            out.push(os);
        }
    }
    out
}

#[inline]
fn run_blocking_inline<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    if let Ok(handle) = tokio::runtime::Handle::try_current()
        && handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread
    {
        tokio::task::block_in_place(f)
    } else {
        f()
    }
}

#[derive(Debug)]
struct WatcherFilterState {
    ignore_dirs: Vec<String>,
    watched_roots: RwLock<Vec<(String, PathBuf)>>,
    raw_timestamps: Mutex<VecDeque<Instant>>,
    window_dur: Duration,
}

impl WatcherFilterState {
    fn new(ignore_dirs: Vec<String>, window_dur: Duration) -> Self {
        Self {
            ignore_dirs,
            watched_roots: RwLock::new(Vec::new()),
            raw_timestamps: Mutex::new(VecDeque::with_capacity(64)),
            window_dur,
        }
    }

    fn add_watched_root(&self, root: &Path) {
        let key = normalize_path_key(root);
        let clean = strip_windows_verbatim_prefix(root);
        if let Ok(mut guard) = self.watched_roots.write()
            && !guard.iter().any(|(k, _)| k == &key)
        {
            guard.push((key, clean));
        }
    }

    fn remove_watched_root(&self, root: &Path) {
        let key = normalize_path_key(root);
        if let Ok(mut guard) = self.watched_roots.write() {
            guard.retain(|(k, _)| k != &key);
        }
    }

    fn relative_to_watched_root(&self, abs_path: &Path) -> Option<PathBuf> {
        let norm_abs = normalize_path_key(abs_path);
        let stripped_abs = strip_windows_verbatim_prefix(abs_path);
        let guard = self.watched_roots.read().ok()?;
        let mut best: Option<(&str, &PathBuf)> = None;
        for (root_key, root_buf) in guard.iter() {
            if norm_abs == *root_key
                || (norm_abs.starts_with(root_key.as_str())
                    && norm_abs.as_bytes().get(root_key.len()) == Some(&b'/'))
            {
                let is_better = best.is_none_or(|(bk, _)| root_key.len() > bk.len());
                if is_better {
                    best = Some((root_key.as_str(), root_buf));
                }
            }
        }
        let (best_key, best_root) = best?;
        if norm_abs.len() == best_key.len() {
            return Some(PathBuf::new());
        }
        if let Ok(rel) = stripped_abs.strip_prefix(best_root) {
            return Some(normalize_rel_path(rel));
        }
        let by_comp = relative_by_component_skip(&stripped_abs, best_root);
        if !by_comp.as_os_str().is_empty() {
            return Some(by_comp);
        }
        let rel_str = &norm_abs[best_key.len() + 1..];
        Some(PathBuf::from(rel_str))
    }

    fn is_ignored_path(&self, abs_path: &Path) -> bool {
        if let Some(rel) = self.relative_to_watched_root(abs_path) {
            if rel.as_os_str().is_empty() {
                return false;
            }
            return is_ignored_relative_path(&rel, &self.ignore_dirs);
        }
        // Fallback before root registration or outside known roots: check tail components.
        for comp in abs_path.components() {
            if let Component::Normal(os) = comp
                && let Some(s) = os.to_str()
                && component_ignored(s, &self.ignore_dirs)
            {
                return true;
            }
        }
        false
    }

    fn record_raw_event(&self, now: Instant) {
        if let Ok(mut q) = self.raw_timestamps.lock() {
            while let Some(&front) = q.front() {
                if now.saturating_duration_since(front) > self.window_dur {
                    q.pop_front();
                } else {
                    break;
                }
            }
            q.push_back(now);
        }
    }

    fn recent_raw_count(&self, now: Instant) -> usize {
        if let Ok(mut q) = self.raw_timestamps.lock() {
            while let Some(&front) = q.front() {
                if now.saturating_duration_since(front) > self.window_dur {
                    q.pop_front();
                } else {
                    break;
                }
            }
            q.len()
        } else {
            0
        }
    }
}

thread_local! {
    static PENDING_WATCHER_FILTER: RefCell<Option<Arc<WatcherFilterState>>> = const { RefCell::new(None) };
}

struct FilteredFileIdCache {
    inner: RecommendedCache,
    filter: Arc<WatcherFilterState>,
}

impl FilteredFileIdCache {
    fn new(filter: Arc<WatcherFilterState>) -> Self {
        Self {
            inner: RecommendedCache::new(),
            filter,
        }
    }
}

impl FileIdCache for FilteredFileIdCache {
    fn cached_file_id(&self, path: &Path) -> Option<impl AsRef<notify_debouncer_full::file_id::FileId>> {
        self.inner.cached_file_id(path)
    }

    fn add_path(&mut self, path: &Path, recursive_mode: RecursiveMode) {
        if self.filter.is_ignored_path(path) {
            return;
        }
        self.inner.add_single_path(path);
        if recursive_mode == RecursiveMode::NonRecursive {
            let Ok(rd) = std::fs::read_dir(path) else {
                return;
            };
            for entry in rd.flatten() {
                let p = entry.path();
                if !self.filter.is_ignored_path(&p) {
                    self.inner.add_single_path(&p);
                }
            }
            return;
        }
        let mut stack = vec![path.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in rd.flatten() {
                let p = entry.path();
                if self.filter.is_ignored_path(&p) {
                    continue;
                }
                self.inner.add_single_path(&p);
                if let Ok(ft) = entry.file_type()
                    && ft.is_dir()
                    && !ft.is_symlink()
                {
                    stack.push(p);
                }
            }
        }
    }

    fn add_single_path(&mut self, path: &Path) {
        if !self.filter.is_ignored_path(path) {
            self.inner.add_single_path(path);
        }
    }

    fn remove_path(&mut self, path: &Path) {
        self.inner.remove_path(path);
    }

    fn rescan(&mut self, roots: &[WatchRoot]) {
        for root in roots {
            self.add_path(&root.path, root.recursive_mode);
        }
    }
}

struct FilteringEventHandler<F> {
    inner: F,
    filter: Arc<WatcherFilterState>,
}

impl<F: EventHandler> EventHandler for FilteringEventHandler<F> {
    fn handle_event(&mut self, event: notify::Result<Event>) {
        match event {
            Ok(mut ev) => {
                if ev.need_rescan() {
                    self.filter.record_raw_event(Instant::now());
                    self.inner.handle_event(Ok(ev));
                    return;
                }
                ev.paths.retain(|p| !self.filter.is_ignored_path(p));
                if ev.paths.is_empty() {
                    return;
                }
                self.filter.record_raw_event(Instant::now());
                self.inner.handle_event(Ok(ev));
            }
            Err(err) => {
                self.inner.handle_event(Err(err));
            }
        }
    }
}

struct FilteredWatcher {
    inner: RecommendedWatcher,
}

impl Watcher for FilteredWatcher {
    fn new<F: EventHandler>(event_handler: F, config: notify::Config) -> notify::Result<Self>
    where
        Self: Sized,
    {
        let filter = PENDING_WATCHER_FILTER
            .with(|slot| slot.borrow_mut().take())
            .unwrap_or_else(|| {
                Arc::new(WatcherFilterState::new(
                    LoomConfig::default().radar.ignore_dirs,
                    Duration::from_millis(200),
                ))
            });
        let wrapped = FilteringEventHandler {
            inner: event_handler,
            filter,
        };
        let inner = RecommendedWatcher::new(wrapped, config)?;
        Ok(Self { inner })
    }

    fn watch(&mut self, path: &Path, recursive_mode: RecursiveMode) -> notify::Result<()> {
        self.inner.watch(path, recursive_mode)
    }

    fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
        self.inner.unwatch(path)
    }

    fn configure(&mut self, option: notify::Config) -> notify::Result<bool> {
        self.inner.configure(option)
    }

    fn watched_paths(&self) -> notify::Result<Vec<notify::WatchedPath>> {
        self.inner.watched_paths()
    }

    fn kind() -> notify::WatcherKind
    where
        Self: Sized,
    {
        RecommendedWatcher::kind()
    }
}

// ============================================================================
// Radar Public Handle & Commands
// ============================================================================

enum RadarCommand {
    Bootstrap,
    RegisterViewport {
        vp: Box<ViewportRecord>,
        reply: oneshot::Sender<()>,
    },
    UnregisterViewport {
        viewport_id: String,
        reply: oneshot::Sender<()>,
    },
    UnregisterInactive {
        reply: oneshot::Sender<()>,
    },
    SetSuppressed {
        viewport_id: String,
        suppressed: bool,
        reply: oneshot::Sender<()>,
    },
    ResetClean {
        viewport_id: String,
        reply: oneshot::Sender<()>,
    },
    RefreshTrunkAndReeval {
        reply: oneshot::Sender<()>,
    },
    ReevaluateAll {
        reply: oneshot::Sender<()>,
    },
    FlushViewport {
        viewport_id: String,
        reply: oneshot::Sender<()>,
    },
    SetExternalConflicts {
        viewport_id: String,
        conflicts: Vec<RadarConflictItem>,
        reply: oneshot::Sender<()>,
    },
    Snapshot {
        viewport_id: Option<String>,
        reply: oneshot::Sender<RadarSnapshot>,
    },
    QuiesceForGc {
        ack: oneshot::Sender<()>,
        release: oneshot::Receiver<()>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

pub struct RadarGcGuard {
    release_tx: Option<oneshot::Sender<()>>,
}

impl Drop for RadarGcGuard {
    fn drop(&mut self) {
        if let Some(tx) = self.release_tx.take() {
            let _ = tx.send(());
        }
    }
}

#[derive(Clone)]
pub struct RadarHandle {
    cmd_tx: mpsc::Sender<RadarCommand>,
    broadcast_tx: broadcast::Sender<RadarStreamEvent>,
    watch_rx: watch::Receiver<RadarSnapshot>,
    actor_join: Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl RadarHandle {
    pub fn start(
        repo_root: PathBuf,
        loom_dir: PathBuf,
        config: LoomConfig,
        db: Arc<StateDb>,
        writer: WriterHandle,
    ) -> anyhow::Result<Self> {
        let radar_cfg = config.radar.normalized();
        let initial_snapshot = RadarSnapshot {
            sequence: 0,
            repo_root: repo_root.clone(),
            batch_mode: false,
            debounce_ms: radar_cfg.debounce_ms,
            metrics: RadarBatchMetrics {
                batch_mode: false,
                effective_debounce_ms: radar_cfg.debounce_ms,
                batch_switches: 0,
                coalesced_events: 0,
            },
            viewports: Vec::new(),
            matrix: Vec::new(),
            updated_at_ms: now_ms(),
        };

        let (broadcast_tx, _) = broadcast::channel::<RadarStreamEvent>(512);
        let (watch_tx, watch_rx) = watch::channel(initial_snapshot);
        let (cmd_tx, cmd_rx) = mpsc::channel::<RadarCommand>(128);

        let window_dur = Duration::from_millis(radar_cfg.batch_window_ms.max(100));
        let filter_state = Arc::new(WatcherFilterState::new(
            radar_cfg.ignore_dirs.clone(),
            window_dur,
        ));
        let (ev_tx, ev_rx) = mpsc::unbounded_channel::<DebounceEventResult>();

        let debounce_ms = radar_cfg.debounce_ms.max(1);
        let tick_ms = (debounce_ms / 4).max(1);
        let debouncer_cfg = DebouncerConfig::default()
            .with_timeout(Duration::from_millis(debounce_ms))
            .with_tick_rate(Duration::from_millis(tick_ms))
            .with_notify_config(
                notify::Config::default().with_event_kinds(notify::EventKindMask::CORE),
            );

        PENDING_WATCHER_FILTER.with(|slot| {
            *slot.borrow_mut() = Some(Arc::clone(&filter_state));
        });
        let debouncer_res = new_debouncer_opt::<_, FilteredWatcher, FilteredFileIdCache>(
            debouncer_cfg,
            ev_tx,
            FilteredFileIdCache::new(Arc::clone(&filter_state)),
        );
        PENDING_WATCHER_FILTER.with(|slot| {
            *slot.borrow_mut() = None;
        });
        let debouncer = debouncer_res?;

        let cas = CasStore::new(loom_dir.join("cas"), config.zstd_level)?;
        let actor = RadarActor {
            repo_root,
            config,
            db,
            writer,
            cas,
            debouncer: Some(debouncer),
            filter_state,
            parsers: HashMap::new(),
            trunk_files: BTreeMap::new(),
            trunk_rev: 1,
            viewports: BTreeMap::new(),
            pair_cache: HashMap::new(),
            trunk_cache: HashMap::new(),
            sequence: 0,
            batch_mode: false,
            batch_switches: 0,
            coalesced_events: 0,
            batch_started_at: None,
            batch_quiet_deadline: None,
            pending_paths: BTreeMap::new(),
            broadcast_tx: broadcast_tx.clone(),
            watch_tx,
        };

        let actor_join = tokio::spawn(actor.run(cmd_rx, ev_rx));

        Ok(Self {
            cmd_tx,
            broadcast_tx,
            watch_rx,
            actor_join: Arc::new(std::sync::Mutex::new(Some(actor_join))),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<RadarStreamEvent> {
        self.broadcast_tx.subscribe()
    }

    pub fn latest_snapshot(&self, viewport_filter: Option<&str>) -> RadarSnapshot {
        let snap = self.watch_rx.borrow().clone();
        filter_snapshot(snap, viewport_filter)
    }

    pub async fn snapshot(&self, viewport_filter: Option<&str>) -> RadarSnapshot {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(RadarCommand::Snapshot {
                viewport_id: viewport_filter.map(str::to_string),
                reply: tx,
            })
            .await
            .is_ok()
            && let Ok(snap) = rx.await
        {
            return snap;
        }
        self.latest_snapshot(viewport_filter)
    }

    pub async fn bootstrap_from_db(&self) {
        let _ = self.cmd_tx.send(RadarCommand::Bootstrap).await;
    }

    pub async fn register_viewport(&self, vp: &ViewportRecord) {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(RadarCommand::RegisterViewport {
                vp: Box::new(vp.clone()),
                reply: tx,
            })
            .await
            .is_ok()
        {
            let _ = rx.await;
        }
    }

    pub async fn unregister_viewport(&self, viewport_id: &str) {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(RadarCommand::UnregisterViewport {
                viewport_id: viewport_id.to_string(),
                reply: tx,
            })
            .await
            .is_ok()
        {
            let _ = rx.await;
        }
    }

    pub async fn unregister_inactive_viewports(&self) {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(RadarCommand::UnregisterInactive { reply: tx })
            .await
            .is_ok()
        {
            let _ = rx.await;
        }
    }

    pub async fn set_suppressed(&self, viewport_id: &str, suppressed: bool) {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(RadarCommand::SetSuppressed {
                viewport_id: viewport_id.to_string(),
                suppressed,
                reply: tx,
            })
            .await
            .is_ok()
        {
            let _ = rx.await;
        }
    }

    pub async fn reset_viewport_clean(&self, viewport_id: &str) {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(RadarCommand::ResetClean {
                viewport_id: viewport_id.to_string(),
                reply: tx,
            })
            .await
            .is_ok()
        {
            let _ = rx.await;
        }
    }

    pub async fn refresh_trunk_and_reevaluate(&self) {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(RadarCommand::RefreshTrunkAndReeval { reply: tx })
            .await
            .is_ok()
        {
            let _ = rx.await;
        }
    }

    pub async fn reevaluate_all(&self) {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(RadarCommand::ReevaluateAll { reply: tx })
            .await
            .is_ok()
        {
            let _ = rx.await;
        }
    }

    pub async fn flush_viewport(&self, viewport_id: &str) {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(RadarCommand::FlushViewport {
                viewport_id: viewport_id.to_string(),
                reply: tx,
            })
            .await
            .is_ok()
        {
            let _ = rx.await;
        }
    }

    pub async fn set_external_conflicts(
        &self,
        viewport_id: &str,
        conflicts: Vec<RadarConflictItem>,
    ) {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(RadarCommand::SetExternalConflicts {
                viewport_id: viewport_id.to_string(),
                conflicts,
                reply: tx,
            })
            .await
            .is_ok()
        {
            let _ = rx.await;
        }
    }

    pub async fn quiesce_for_gc(&self) -> RadarGcGuard {
        let (ack_tx, ack_rx) = oneshot::channel();
        let (rel_tx, rel_rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(RadarCommand::QuiesceForGc {
                ack: ack_tx,
                release: rel_rx,
            })
            .await
            .is_err()
        {
            return RadarGcGuard { release_tx: None };
        }
        if ack_rx.await.is_err() {
            return RadarGcGuard { release_tx: None };
        }
        RadarGcGuard {
            release_tx: Some(rel_tx),
        }
    }

    pub async fn shutdown(&self) {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(RadarCommand::Shutdown { reply: tx })
            .await
            .is_ok()
        {
            let _ = rx.await;
        }
        let join = self.actor_join.lock().ok().and_then(|mut g| g.take());
        if let Some(jh) = join {
            let _ = jh.await;
        }
    }
}

// ============================================================================
// Internal Radar State & Incremental Shadow Merge Engine
// ============================================================================

struct CachedFileState {
    content_hash: Blake3Hash,
    current_source: String,
    tree: Option<Tree>,
    last_valid_source: Option<String>,
    last_valid_extracted: Option<ExtractedSymbols>,
}

struct ViewportShadow {
    vp: ViewportRecord,
    suppressed: bool,
    watched: bool,
    base_files: Option<BTreeMap<PathBuf, Blake3Hash>>,
    files: BTreeMap<PathBuf, CachedFileState>,
    pending_syntax_files: BTreeSet<PathBuf>,
    file_patches: BTreeMap<PathBuf, AstPatch>,
    persisted_files: BTreeMap<PathBuf, Blake3Hash>,
    last_patch_hash: Option<Blake3Hash>,
    persisted_lamport_ts: u64,
    pending_wal_patches: Vec<AstPatch>,
    external_conflicts: Vec<RadarConflictItem>,
    provides_by_file: BTreeMap<PathBuf, Vec<SymbolProvideEntry>>,
    consumes_by_file: BTreeMap<PathBuf, Vec<SymbolConsumeEntry>>,
    published_provides: HashMap<String, SymbolProvideEntry>,
    published_consumes: HashMap<String, SymbolConsumeEntry>,
    rev: u64,
    has_written_status: bool,
    last_status: RadarLevel,
    last_conflicts: Vec<RadarConflictItem>,
    last_warnings: Vec<NegativeEdge>,
    last_pending_files: Vec<PathBuf>,
}

impl ViewportShadow {
    fn new(vp: ViewportRecord) -> Self {
        let initial_ts = vp.lamport_ts;
        Self {
            vp,
            suppressed: false,
            watched: false,
            base_files: None,
            files: BTreeMap::new(),
            pending_syntax_files: BTreeSet::new(),
            file_patches: BTreeMap::new(),
            persisted_files: BTreeMap::new(),
            last_patch_hash: None,
            persisted_lamport_ts: initial_ts,
            pending_wal_patches: Vec::new(),
            external_conflicts: Vec::new(),
            provides_by_file: BTreeMap::new(),
            consumes_by_file: BTreeMap::new(),
            published_provides: HashMap::new(),
            published_consumes: HashMap::new(),
            rev: 1,
            has_written_status: false,
            last_status: RadarLevel::Green,
            last_conflicts: Vec::new(),
            last_warnings: Vec::new(),
            last_pending_files: Vec::new(),
        }
    }

    fn all_consumes(&self) -> Vec<&SymbolConsumeEntry> {
        self.consumes_by_file.values().flatten().collect()
    }

    fn dirty_files(&self) -> Vec<PathBuf> {
        self.file_patches.keys().cloned().collect()
    }

    fn base_bytes(
        &self,
        rel: &Path,
        trunk_files: &BTreeMap<PathBuf, Option<Vec<u8>>>,
        cas: &CasStore,
    ) -> Option<Vec<u8>> {
        match &self.base_files {
            Some(map) => map.get(rel).and_then(|h| cas.get_bytes(h).ok()),
            None => trunk_files.get(rel).and_then(|o| o.clone()),
        }
    }

    fn touched_symbols(&self) -> Vec<String> {
        let mut syms: BTreeSet<String> = BTreeSet::new();
        for (file_path, patch) in &self.file_patches {
            let mut file_syms: Vec<String> = Vec::new();
            for delta in &patch.contract_deltas {
                match delta {
                    SymbolContractDelta::AddedSymbol { symbol_fqn, .. }
                    | SymbolContractDelta::RemovedSymbol { symbol_fqn, .. }
                    | SymbolContractDelta::SignatureChanged { symbol_fqn, .. }
                    | SymbolContractDelta::VisibilityReduced { symbol_fqn, .. } => {
                        if symbol_fqn != "<file>" {
                            file_syms.push(symbol_fqn.clone());
                        }
                    }
                }
            }
            for op in &patch.ops {
                match op {
                    SemanticOp::InsertMember {
                        parent_anchor,
                        member_anchor,
                        ..
                    } => {
                        if parent_anchor.symbol_fqn != "<file>" {
                            file_syms.push(parent_anchor.symbol_fqn.clone());
                        }
                        if member_anchor.symbol_fqn != "<file>" {
                            file_syms.push(member_anchor.symbol_fqn.clone());
                        }
                    }
                    SemanticOp::ReplaceBody { target, .. }
                    | SemanticOp::MutateSignature { target, .. }
                    | SemanticOp::RemoveNode { target, .. } => {
                        if target.symbol_fqn != "<file>" {
                            file_syms.push(target.symbol_fqn.clone());
                        }
                    }
                    SemanticOp::RenameOrMoveFile { .. } | SemanticOp::TextCrdtFallback { .. } => {
                    }
                }
            }
            if file_syms.is_empty() {
                if let Some(entries) = self.provides_by_file.get(file_path) {
                    for p in entries {
                        if p.symbol_fqn != "<file>" {
                            syms.insert(p.symbol_fqn.clone());
                        }
                    }
                }
            } else {
                syms.extend(file_syms);
            }
        }
        syms.into_iter().collect()
    }
}

struct CachedPairEval {
    rev_a: u64,
    rev_b: u64,
    level: RadarLevel,
    conflicts_for_a: Vec<RadarConflictItem>,
    conflicts_for_b: Vec<RadarConflictItem>,
}

struct CachedTrunkEval {
    vp_rev: u64,
    trunk_rev: u64,
    conflicts: Vec<RadarConflictItem>,
}

struct RadarActor {
    repo_root: PathBuf,
    config: LoomConfig,
    db: Arc<StateDb>,
    writer: WriterHandle,
    cas: CasStore,
    debouncer: Option<Debouncer<FilteredWatcher, FilteredFileIdCache>>,
    filter_state: Arc<WatcherFilterState>,
    parsers: HashMap<LanguageKind, Parser>,
    trunk_files: BTreeMap<PathBuf, Option<Vec<u8>>>,
    trunk_rev: u64,
    viewports: BTreeMap<String, ViewportShadow>,
    pair_cache: HashMap<(String, String), CachedPairEval>,
    trunk_cache: HashMap<String, CachedTrunkEval>,
    sequence: u64,
    batch_mode: bool,
    batch_switches: u64,
    coalesced_events: u64,
    batch_started_at: Option<Instant>,
    batch_quiet_deadline: Option<Instant>,
    pending_paths: BTreeMap<(String, PathBuf), bool>,
    broadcast_tx: broadcast::Sender<RadarStreamEvent>,
    watch_tx: watch::Sender<RadarSnapshot>,
}

impl RadarActor {
    async fn run(
        mut self,
        mut cmd_rx: mpsc::Receiver<RadarCommand>,
        mut ev_rx: mpsc::UnboundedReceiver<DebounceEventResult>,
    ) {
        loop {
            let batch_deadline = if self.batch_mode {
                let batch_window =
                    Duration::from_millis(self.config.radar.normalized().batch_window_ms);
                let max_coalesce = batch_window * 2;
                let cap_deadline = self
                    .batch_started_at
                    .map(|s| s + max_coalesce)
                    .unwrap_or_else(|| Instant::now() + batch_window);
                let quiet_deadline = self
                    .batch_quiet_deadline
                    .unwrap_or_else(|| Instant::now() + batch_window);
                Some(quiet_deadline.min(cap_deadline))
            } else {
                None
            };

            tokio::select! {
                cmd_opt = cmd_rx.recv() => {
                    match cmd_opt {
                        Some(RadarCommand::Bootstrap) => {
                            self.handle_bootstrap().await;
                        }
                        Some(RadarCommand::RegisterViewport { vp, reply }) => {
                            self.handle_register_viewport(*vp).await;
                            let _ = reply.send(());
                        }
                        Some(RadarCommand::UnregisterViewport { viewport_id, reply }) => {
                            self.handle_unregister_viewport(&viewport_id).await;
                            let _ = reply.send(());
                        }
                        Some(RadarCommand::UnregisterInactive { reply }) => {
                            self.handle_unregister_inactive().await;
                            let _ = reply.send(());
                        }
                        Some(RadarCommand::SetSuppressed { viewport_id, suppressed, reply }) => {
                            if let Some(vp) = self.viewports.get_mut(&viewport_id) {
                                vp.suppressed = suppressed;
                            }
                            let _ = reply.send(());
                        }
                        Some(RadarCommand::ResetClean { viewport_id, reply }) => {
                            self.handle_reset_clean(&viewport_id).await;
                            let _ = reply.send(());
                        }
                        Some(RadarCommand::RefreshTrunkAndReeval { reply }) => {
                            self.trunk_files.clear();
                            self.pair_cache.clear();
                            self.trunk_cache.clear();
                            self.trunk_rev += 1;
                            self.rescan_all_active_viewports().await;
                            let _ = reply.send(());
                        }
                        Some(RadarCommand::ReevaluateAll { reply }) => {
                            self.reevaluate_and_publish(Vec::new()).await;
                            let _ = reply.send(());
                        }
                        Some(RadarCommand::FlushViewport { viewport_id, reply }) => {
                            self.handle_flush_viewport(&viewport_id).await;
                            let _ = reply.send(());
                        }
                        Some(RadarCommand::SetExternalConflicts {
                            viewport_id,
                            conflicts,
                            reply,
                        }) => {
                            self.handle_set_external_conflicts(&viewport_id, conflicts)
                                .await;
                            let _ = reply.send(());
                        }
                        Some(RadarCommand::Snapshot { viewport_id, reply }) => {
                            if !self.pending_paths.is_empty() {
                                self.flush_pending_paths().await;
                            }
                            let snap = filter_snapshot(self.build_snapshot(), viewport_id.as_deref());
                            let _ = reply.send(snap);
                        }
                        Some(RadarCommand::QuiesceForGc { ack, release }) => {
                            if !self.pending_paths.is_empty() {
                                self.flush_pending_paths().await;
                            }
                            let _ = ack.send(());
                            let _ = release.await;
                        }
                        Some(RadarCommand::Shutdown { reply }) => {
                            self.handle_shutdown();
                            let _ = reply.send(());
                            break;
                        }
                        None => {
                            self.handle_shutdown();
                            break;
                        }
                    }
                }
                ev_opt = ev_rx.recv() => {
                    match ev_opt {
                        Some(res) => {
                            self.handle_debounced_result(res).await;
                        }
                        None => break,
                    }
                }
                _ = async {
                    if let Some(dl) = batch_deadline {
                        tokio::time::sleep_until(tokio::time::Instant::from_std(dl)).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {
                    self.flush_batch_timer().await;
                }
            }
        }
    }

    fn handle_shutdown(&mut self) {
        let roots: Vec<PathBuf> = self
            .viewports
            .values()
            .filter(|v| v.watched)
            .map(|v| v.vp.path.clone())
            .collect();
        if let Some(ref mut deb) = self.debouncer {
            for r in &roots {
                let _ = deb.unwatch(r);
                self.filter_state.remove_watched_root(r);
            }
        }
        if let Some(deb) = self.debouncer.take() {
            deb.stop();
        }
    }

    async fn handle_bootstrap(&mut self) {
        let Ok(vps) = self.db.list_viewports(false) else {
            return;
        };
        let active: Vec<ViewportRecord> = vps
            .into_iter()
            .filter(|v| v.state.is_active() && v.path.is_dir())
            .collect();
        for vp in active {
            self.register_viewport_inner(vp, false).await;
        }
        let all_ids: Vec<String> = self.viewports.keys().cloned().collect();
        self.reevaluate_and_publish(all_ids).await;
    }

    async fn handle_register_viewport(&mut self, vp: ViewportRecord) {
        let id = vp.id.clone();
        self.register_viewport_inner(vp, true).await;
        self.reevaluate_and_publish(vec![id]).await;
    }

    async fn register_viewport_inner(&mut self, vp: ViewportRecord, _publish: bool) {
        if !vp.state.is_active() || !vp.path.is_dir() {
            return;
        }
        let id = vp.id.clone();
        let vp_path = vp.path.clone();
        let mut already_watched = false;
        if let Some(existing) = self.viewports.get_mut(&id) {
            already_watched = existing.watched;
            if existing.vp.base_snapshot_cas == vp.base_snapshot_cas {
                existing.vp = vp;
                if !already_watched {
                    self.watch_path(&vp_path);
                    if let Some(v) = self.viewports.get_mut(&id) {
                        v.watched = true;
                    }
                }
                return;
            }
            self.viewports.remove(&id);
            self.pending_paths.retain(|(vid, _), _| vid != &id);
            self.pair_cache.retain(|(a, b), _| a != &id && b != &id);
            self.trunk_cache.remove(&id);
        }

        let mut shadow = ViewportShadow::new(vp);
        self.seed_viewport_persisted_state(&mut shadow);
        let mut candidate_rels: BTreeSet<PathBuf> = shadow.persisted_files.keys().cloned().collect();
        let has_base_files = if let Some(ref base_map) = shadow.base_files {
            candidate_rels.extend(base_map.keys().cloned());
            true
        } else {
            candidate_rels.extend(
                self.trunk_files
                    .iter()
                    .filter_map(|(p, v)| v.as_ref().map(|_| p.clone())),
            );
            false
        };
        if !already_watched {
            self.watch_path(&vp_path);
        }
        shadow.watched = true;
        self.viewports.insert(id.clone(), shadow);

        run_blocking_inline(|| {
            if !has_base_files {
                candidate_rels.extend(collect_non_ignored_files(
                    &self.repo_root,
                    &self.config.radar.ignore_dirs,
                ));
            }
            candidate_rels.extend(collect_non_ignored_files(
                &vp_path,
                &self.config.radar.ignore_dirs,
            ));
            for rel in candidate_rels {
                self.process_viewport_file(&id, &rel);
            }
        });
        self.flush_wal_patches_for_viewport(&id).await;
        self.sync_viewport_symbols_to_db(&id).await;
    }

    fn seed_viewport_persisted_state(&self, shadow: &mut ViewportShadow) {
        shadow.persisted_files.clear();
        shadow.base_files = None;
        shadow.last_patch_hash = None;
        shadow.persisted_lamport_ts = shadow.vp.lamport_ts;
        shadow.pending_wal_patches.clear();

        if let Some(snap_hash) = shadow.vp.base_snapshot_cas
            && let Ok(raw) = self.cas.get_bytes(&snap_hash)
            && let Ok(snapshot) = rmp_serde::from_slice::<WorkspaceSnapshot>(&raw)
        {
            let mut base_map = BTreeMap::new();
            for (k, h) in snapshot.files {
                let rel: PathBuf = k.split('/').filter(|s| !s.is_empty()).collect();
                if !rel.as_os_str().is_empty() {
                    shadow.persisted_files.insert(rel.clone(), h);
                    base_map.insert(rel, h);
                }
            }
            shadow.base_files = Some(base_map);
        } else {
            let trunk_rels =
                collect_non_ignored_files(&self.repo_root, &self.config.radar.ignore_dirs);
            for rel in trunk_rels {
                if let Ok(bytes) = std::fs::read(self.repo_root.join(&rel)) {
                    shadow
                        .persisted_files
                        .insert(rel, Blake3Hash::from_bytes(&bytes));
                }
            }
        }

        for pid in &shadow.vp.patch_ids {
            if let Ok(Some(rec)) = self.db.get_patch(pid) {
                let rel: PathBuf = rec
                    .patch
                    .file_path
                    .to_string_lossy()
                    .replace('\\', "/")
                    .split('/')
                    .filter(|s| !s.is_empty())
                    .collect();
                if let Some(after) = rec.patch.cas_after {
                    shadow.persisted_files.insert(rel, after);
                } else {
                    shadow.persisted_files.remove(&rel);
                }
                shadow.last_patch_hash = Some(rec.patch.patch_hash);
                shadow.persisted_lamport_ts =
                    shadow.persisted_lamport_ts.max(rec.patch.lamport_ts);
            }
        }
    }

    async fn flush_wal_patches_for_viewport(&mut self, vp_id: &str) {
        let pending = self
            .viewports
            .get_mut(vp_id)
            .map(|s| std::mem::take(&mut s.pending_wal_patches))
            .unwrap_or_default();
        for patch in pending {
            let (tx, rx) = oneshot::channel();
            if self
                .writer
                .send(WriterCommand::RecordAstPatch {
                    viewport_id: vp_id.to_string(),
                    patch: Box::new(patch),
                    reply: tx,
                })
                .await
                .is_ok()
                && let Ok(Ok(Some(rec))) = rx.await
                && let Some(shadow) = self.viewports.get_mut(vp_id)
            {
                if !shadow.vp.patch_ids.contains(&rec.patch_id) {
                    shadow.vp.patch_ids.push(rec.patch_id);
                }
                shadow.vp.lamport_ts = shadow.vp.lamport_ts.max(rec.patch.lamport_ts);
                shadow.vp.head_cas = rec.patch.cas_after;
                shadow.vp.head_lsn = rec.lsn;
            }
        }
    }

    fn watch_path(&mut self, vp_path: &Path) {
        self.filter_state.add_watched_root(vp_path);
        if let Some(ref mut deb) = self.debouncer
            && let Err(e) = deb.watch(vp_path, RecursiveMode::Recursive)
        {
            tracing::warn!(path = %vp_path.display(), error = %e, "radar watch failed");
        }
    }

    fn unwatch_path(&mut self, vp_path: &Path) {
        if let Some(ref mut deb) = self.debouncer {
            let _ = deb.unwatch(vp_path);
        }
        self.filter_state.remove_watched_root(vp_path);
    }

    async fn handle_unregister_viewport(&mut self, viewport_id: &str) {
        let resolved_id = if self.viewports.contains_key(viewport_id) {
            viewport_id.to_string()
        } else if let Ok(Some(rec)) = self.db.resolve_viewport(viewport_id) {
            rec.id
        } else {
            viewport_id.to_string()
        };

        if let Some(shadow) = self.viewports.remove(&resolved_id) {
            if shadow.watched {
                self.unwatch_path(&shadow.vp.path);
            }
            self.pending_paths
                .retain(|(vid, _), _| vid != &resolved_id);
            self.pair_cache
                .retain(|(a, b), _| a != &resolved_id && b != &resolved_id);
            self.trunk_cache.remove(&resolved_id);
            self.clear_viewport_symbols_in_db(&resolved_id).await;
            self.reevaluate_and_publish(vec![resolved_id]).await;
        }
    }

    async fn handle_unregister_inactive(&mut self) {
        let mut to_remove = Vec::new();
        for id in self.viewports.keys() {
            let active_in_db = self
                .db
                .get_viewport(id)
                .ok()
                .flatten()
                .is_some_and(|v| v.state.is_active());
            if !active_in_db {
                to_remove.push(id.clone());
            }
        }
        // Also ensure any directory on disk for inactive viewports is unwatched
        if let Ok(all_vps) = self.db.list_viewports(true) {
            for vp in all_vps {
                if !vp.state.is_active() {
                    self.unwatch_path(&vp.path);
                }
            }
        }
        if !to_remove.is_empty() {
            for id in &to_remove {
                if let Some(shadow) = self.viewports.remove(id)
                    && shadow.watched
                {
                    self.unwatch_path(&shadow.vp.path);
                }
                self.pending_paths.retain(|(vid, _), _| vid != id);
                self.pair_cache.retain(|(a, b), _| a != id && b != id);
                self.trunk_cache.remove(id);
                self.clear_viewport_symbols_in_db(id).await;
            }
            self.reevaluate_and_publish(to_remove).await;
        }
    }

    async fn handle_reset_clean(&mut self, viewport_id: &str) {
        let Some(vp_path) = self.viewports.get(viewport_id).map(|s| s.vp.path.clone()) else {
            return;
        };
        let fresh_vp = self.db.get_viewport(viewport_id).ok().flatten();
        if let Some(mut shadow) = self.viewports.remove(viewport_id) {
            if let Some(vp) = fresh_vp {
                shadow.vp = vp;
            }
            shadow.files.clear();
            shadow.pending_syntax_files.clear();
            shadow.file_patches.clear();
            shadow.external_conflicts.clear();
            shadow.provides_by_file.clear();
            shadow.consumes_by_file.clear();
            shadow.rev += 1;
            shadow.suppressed = false;
            self.seed_viewport_persisted_state(&mut shadow);
            self.viewports.insert(viewport_id.to_string(), shadow);
        }
        self.pending_paths
            .retain(|(vid, _), _| vid != viewport_id);
        self.pair_cache
            .retain(|(a, b), _| a != viewport_id && b != viewport_id);
        self.trunk_cache.remove(viewport_id);

        run_blocking_inline(|| {
            let mut candidate_rels: BTreeSet<PathBuf> = collect_non_ignored_files(
                &vp_path,
                &self.config.radar.ignore_dirs,
            )
            .into_iter()
            .collect();
            if let Some(shadow) = self.viewports.get(viewport_id) {
                candidate_rels.extend(shadow.persisted_files.keys().cloned());
                if let Some(ref base_map) = shadow.base_files {
                    candidate_rels.extend(base_map.keys().cloned());
                }
            }
            for rel in candidate_rels {
                self.process_viewport_file(viewport_id, &rel);
            }
        });
        self.flush_wal_patches_for_viewport(viewport_id).await;
        self.sync_viewport_symbols_to_db(viewport_id).await;
        self.reevaluate_and_publish(vec![viewport_id.to_string()])
            .await;
    }

    async fn rescan_all_active_viewports(&mut self) {
        let ids: Vec<String> = self.viewports.keys().cloned().collect();
        for id in &ids {
            let scanned = run_blocking_inline(|| {
                let candidate_rels = if let Some(shadow) = self.viewports.get_mut(id) {
                    let vp_path = shadow.vp.path.clone();
                    shadow.files.clear();
                    shadow.pending_syntax_files.clear();
                    shadow.file_patches.clear();
                    shadow.provides_by_file.clear();
                    shadow.consumes_by_file.clear();
                    shadow.rev += 1;
                    let mut candidate_rels: BTreeSet<PathBuf> =
                        shadow.persisted_files.keys().cloned().collect();
                    if let Some(ref base_map) = shadow.base_files {
                        candidate_rels.extend(base_map.keys().cloned());
                    } else {
                        candidate_rels.extend(
                            self.trunk_files
                                .iter()
                                .filter_map(|(p, v)| v.as_ref().map(|_| p.clone())),
                        );
                        candidate_rels.extend(collect_non_ignored_files(
                            &self.repo_root,
                            &self.config.radar.ignore_dirs,
                        ));
                    }
                    candidate_rels.extend(collect_non_ignored_files(
                        &vp_path,
                        &self.config.radar.ignore_dirs,
                    ));
                    Some(candidate_rels)
                } else {
                    None
                };
                if let Some(candidate_rels) = candidate_rels {
                    for rel in candidate_rels {
                        self.process_viewport_file(id, &rel);
                    }
                    true
                } else {
                    false
                }
            });
            if scanned {
                self.flush_wal_patches_for_viewport(id).await;
                self.sync_viewport_symbols_to_db(id).await;
            }
        }
        self.reevaluate_and_publish(ids).await;
    }

    async fn handle_flush_viewport(&mut self, viewport_id: &str) {
        let resolved_id = if self.viewports.contains_key(viewport_id) {
            viewport_id.to_string()
        } else if let Ok(Some(rec)) = self.db.resolve_viewport(viewport_id) {
            rec.id
        } else {
            return;
        };

        let Some(vp_path) = self.viewports.get(&resolved_id).map(|s| s.vp.path.clone()) else {
            return;
        };

        let mut candidate_rels: BTreeSet<PathBuf> = BTreeSet::new();
        let keys_to_remove: Vec<(String, PathBuf)> = self
            .pending_paths
            .keys()
            .filter(|(vid, _)| vid == &resolved_id)
            .cloned()
            .collect();
        for key in keys_to_remove {
            self.pending_paths.remove(&key);
            candidate_rels.insert(key.1);
        }

        let mut has_base_files = false;
        if let Some(s) = self.viewports.get(&resolved_id) {
            for p in s
                .persisted_files
                .keys()
                .chain(s.files.keys())
                .chain(s.file_patches.keys())
                .chain(s.pending_syntax_files.iter())
                .chain(s.provides_by_file.keys())
                .chain(s.consumes_by_file.keys())
            {
                candidate_rels.insert(p.clone());
            }
            if let Some(ref base_map) = s.base_files {
                has_base_files = true;
                candidate_rels.extend(base_map.keys().cloned());
            }
        }
        if !has_base_files {
            candidate_rels.extend(
                self.trunk_files
                    .iter()
                    .filter_map(|(p, v)| v.as_ref().map(|_| p.clone())),
            );
        }

        let ignore_dirs = self.config.radar.ignore_dirs.clone();
        let disk_rels = run_blocking_inline(|| {
            let mut rels = collect_non_ignored_files(&vp_path, &ignore_dirs);
            if !has_base_files {
                rels.extend(collect_non_ignored_files(&self.repo_root, &ignore_dirs));
            }
            rels
        });
        candidate_rels.extend(disk_rels);

        let any_changed = run_blocking_inline(|| {
            let mut changed = false;
            for rel in candidate_rels {
                if self.process_viewport_file(&resolved_id, &rel) {
                    changed = true;
                }
            }
            changed
        });

        self.flush_wal_patches_for_viewport(&resolved_id).await;
        if any_changed {
            self.sync_viewport_symbols_to_db(&resolved_id).await;
            self.reevaluate_and_publish(vec![resolved_id]).await;
        }
    }

    async fn handle_set_external_conflicts(
        &mut self,
        viewport_id: &str,
        conflicts: Vec<RadarConflictItem>,
    ) {
        let resolved_id = if self.viewports.contains_key(viewport_id) {
            viewport_id.to_string()
        } else if let Ok(Some(rec)) = self.db.resolve_viewport(viewport_id) {
            rec.id
        } else {
            return;
        };

        if let Some(shadow) = self.viewports.get_mut(&resolved_id) {
            shadow.external_conflicts = conflicts;
            shadow.rev += 1;
        } else {
            return;
        }
        self.reevaluate_and_publish(vec![resolved_id]).await;
    }

    async fn handle_debounced_result(&mut self, res: DebounceEventResult) {
        let events = match res {
            Ok(evs) => evs,
            Err(errs) => {
                for err in errs {
                    tracing::warn!(error = %err, "radar file watcher error");
                }
                return;
            }
        };
        if events.is_empty() {
            return;
        }

        let cfg = self.config.radar.normalized();
        let now = Instant::now();
        let mut rescan_requested = false;
        let mut batch_items: Vec<(String, PathBuf)> = Vec::new();

        for dev in &events {
            if dev.event.need_rescan() {
                rescan_requested = true;
            }
            for p in &dev.event.paths {
                if let Some((vp_id, rel)) = self.map_path_to_viewport(p) {
                    if rel.as_os_str().is_empty() {
                        continue;
                    }
                    if !is_ignored_relative_path(&rel, &cfg.ignore_dirs) {
                        batch_items.push((vp_id, rel));
                    }
                }
            }
        }

        if batch_items.is_empty() && !rescan_requested {
            return;
        }

        let raw_rate = self.filter_state.recent_raw_count(now);
        let should_batch = rescan_requested
            || batch_items.len() >= cfg.batch_threshold_events
            || raw_rate >= cfg.batch_threshold_events;

        if rescan_requested {
            let scanned = run_blocking_inline(|| {
                let mut found = Vec::new();
                for (id, shadow) in &self.viewports {
                    if shadow.suppressed {
                        continue;
                    }
                    let mut rels: BTreeSet<PathBuf> =
                        collect_non_ignored_files(&shadow.vp.path, &cfg.ignore_dirs)
                            .into_iter()
                            .collect();
                    rels.extend(shadow.persisted_files.keys().cloned());
                    rels.extend(shadow.files.keys().cloned());
                    if let Some(ref base_map) = shadow.base_files {
                        rels.extend(base_map.keys().cloned());
                    }
                    for rel in rels {
                        found.push((id.clone(), rel));
                    }
                }
                found
            });
            for (id, rel) in scanned {
                self.pending_paths.insert((id, rel), true);
            }
        }

        for (vp_id, rel) in batch_items {
            if self.viewports.get(&vp_id).is_some_and(|v| v.suppressed) {
                continue;
            }
            self.pending_paths.insert((vp_id, rel), true);
        }

        if self.batch_mode || should_batch {
            let batch_window = Duration::from_millis(cfg.batch_window_ms);
            if !self.batch_mode {
                self.batch_mode = true;
                self.batch_switches += 1;
                self.batch_started_at = Some(now);
            }
            self.coalesced_events += events.len() as u64;
            self.batch_quiet_deadline = Some(now + batch_window);
            let snap = self.build_snapshot();
            let _ = self.watch_tx.send(snap);
            return;
        }

        self.flush_pending_paths().await;
    }

    async fn flush_batch_timer(&mut self) {
        self.flush_pending_paths().await;
        let cfg = self.config.radar.normalized();
        let now = Instant::now();
        let raw_rate = self.filter_state.recent_raw_count(now);
        let low_watermark = (cfg.batch_threshold_events / 2).max(1);
        if raw_rate < low_watermark {
            self.batch_mode = false;
            self.batch_started_at = None;
            self.batch_quiet_deadline = None;
            let snap = self.build_snapshot();
            let _ = self.watch_tx.send(snap.clone());
            let _ = self.broadcast_tx.send(RadarStreamEvent {
                snapshot: snap,
                changed_viewports: Vec::new(),
                escalated_notifications: Vec::new(),
            });
        } else {
            let batch_window = Duration::from_millis(cfg.batch_window_ms);
            self.batch_started_at = Some(now);
            self.batch_quiet_deadline = Some(now + batch_window);
        }
    }

    async fn flush_pending_paths(&mut self) {
        if self.pending_paths.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.pending_paths);
        let touched_vps: BTreeSet<String> = run_blocking_inline(|| {
            let mut touched = BTreeSet::new();
            for ((vp_id, rel), _) in pending {
                if self.viewports.get(&vp_id).is_some_and(|v| !v.suppressed)
                    && self.process_viewport_file(&vp_id, &rel)
                {
                    touched.insert(vp_id);
                }
            }
            touched
        });
        for vp_id in &touched_vps {
            self.flush_wal_patches_for_viewport(vp_id).await;
            self.sync_viewport_symbols_to_db(vp_id).await;
        }
        if !touched_vps.is_empty() {
            self.reevaluate_and_publish(touched_vps.into_iter().collect())
                .await;
        }
    }

    fn map_path_to_viewport(&self, abs_path: &Path) -> Option<(String, PathBuf)> {
        let norm_abs = normalize_path_key(abs_path);
        let stripped_abs = strip_windows_verbatim_prefix(abs_path);
        let mut best: Option<(&str, &ViewportShadow, usize)> = None;
        for (id, shadow) in &self.viewports {
            let root_key = normalize_path_key(&shadow.vp.path);
            if norm_abs == root_key
                || (norm_abs.starts_with(&root_key)
                    && norm_abs.as_bytes().get(root_key.len()) == Some(&b'/'))
            {
                let is_better = best.is_none_or(|(_, _, len)| root_key.len() > len);
                if is_better {
                    best = Some((id.as_str(), shadow, root_key.len()));
                }
            }
        }
        let (id, shadow, root_len) = best?;
        let vp_id = id.to_string();
        if norm_abs.len() == root_len {
            return Some((vp_id, PathBuf::new()));
        }
        let stripped_root = strip_windows_verbatim_prefix(&shadow.vp.path);
        let rel = if let Ok(r) = stripped_abs.strip_prefix(&stripped_root) {
            normalize_rel_path(r)
        } else {
            let by_comp = relative_by_component_skip(&stripped_abs, &stripped_root);
            if !by_comp.as_os_str().is_empty() {
                by_comp
            } else {
                PathBuf::from(&norm_abs[root_len + 1..])
            }
        };
        Some((vp_id, rel))
    }

    fn load_trunk_bytes(&mut self, rel_path: &Path) -> Option<Vec<u8>> {
        if let Some(cached) = self.trunk_files.get(rel_path) {
            return cached.clone();
        }
        let data = if is_file_exact_case(&self.repo_root, rel_path) {
            std::fs::read(self.repo_root.join(rel_path)).ok()
        } else {
            None
        };
        self.trunk_files.insert(rel_path.to_path_buf(), data.clone());
        data
    }

    fn get_or_init_parser(&mut self, lang: LanguageKind) -> Option<&mut Parser> {
        if let std::collections::hash_map::Entry::Vacant(entry) = self.parsers.entry(lang) {
            let ts_lang = ts_language(lang).ok()?;
            let mut p = Parser::new();
            p.set_language(&ts_lang).ok()?;
            entry.insert(p);
        }
        self.parsers.get_mut(&lang)
    }

    fn process_viewport_file(&mut self, vp_id: &str, rel_path: &Path) -> bool {
        let Some(vp_root) = self.viewports.get(vp_id).map(|s| s.vp.path.clone()) else {
            return false;
        };
        let full_path = vp_root.join(rel_path);
        if full_path.is_dir() {
            let ignore_dirs = self.config.radar.ignore_dirs.clone();
            let children = collect_non_ignored_files(&full_path, &ignore_dirs);
            let mut changed = false;
            if !rel_path.as_os_str().is_empty()
                && self.process_single_viewport_file(vp_id, rel_path)
            {
                changed = true;
            }
            for child_rel in children {
                let joined = rel_path.join(child_rel);
                if self.process_single_viewport_file(vp_id, &joined) {
                    changed = true;
                }
            }
            if let Some(shadow) = self.viewports.get(vp_id) {
                let tracked = collect_tracked_descendants(shadow, rel_path);
                for desc in tracked {
                    if !is_file_exact_case(&vp_root, &desc)
                        && self.process_single_viewport_file(vp_id, &desc)
                    {
                        changed = true;
                    }
                }
            }
            return changed;
        }

        let mut changed = self.process_single_viewport_file(vp_id, rel_path);
        if !full_path.is_dir()
            && let Some(shadow) = self.viewports.get(vp_id)
        {
            let tracked = collect_tracked_descendants(shadow, rel_path);
            for desc in tracked {
                if !is_file_exact_case(&vp_root, &desc)
                    && self.process_single_viewport_file(vp_id, &desc)
                {
                    changed = true;
                }
            }
        }
        changed
    }

    fn process_single_viewport_file(&mut self, vp_id: &str, rel_path: &Path) -> bool {
        let Some(vp_root) = self.viewports.get(vp_id).map(|s| s.vp.path.clone()) else {
            return false;
        };
        let full_path = vp_root.join(rel_path);

        enum ShadowBaseLookup {
            FromSnapshot(Option<Blake3Hash>),
            FallbackTrunk,
        }
        let base_lookup = match self.viewports.get(vp_id) {
            Some(s) => match &s.base_files {
                Some(bf) => ShadowBaseLookup::FromSnapshot(bf.get(rel_path).copied()),
                None => ShadowBaseLookup::FallbackTrunk,
            },
            None => return false,
        };
        let base_bytes = match base_lookup {
            ShadowBaseLookup::FromSnapshot(Some(h)) => self.cas.get_bytes(&h).ok(),
            ShadowBaseLookup::FromSnapshot(None) => None,
            ShadowBaseLookup::FallbackTrunk => self.load_trunk_bytes(rel_path),
        };
        let file_bytes = if !is_file_exact_case(&vp_root, rel_path) {
            None
        } else {
            match std::fs::read(&full_path) {
                Ok(b) => Some(b),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(_) => return false,
            }
        };

        // Handle deleted file
        let Some(bytes) = file_bytes else {
            let mut changed = false;
            if let Some(shadow) = self.viewports.get_mut(vp_id) {
                if shadow.files.remove(rel_path).is_some() {
                    changed = true;
                }
                if shadow.pending_syntax_files.remove(rel_path) {
                    changed = true;
                }
                if shadow.provides_by_file.remove(rel_path).is_some() {
                    changed = true;
                }
                if shadow.consumes_by_file.remove(rel_path).is_some() {
                    changed = true;
                }
                if let Some(base) = base_bytes {
                    let fid = compute_logical_file_id(rel_path);
                    let lang = LanguageKind::detect(rel_path, &base);
                    let ctx = DiffContext {
                        logical_file_id: fid,
                        file_path: rel_path,
                        language: lang,
                        viewport_id: vp_id,
                        actor_id: &shadow.vp.agent_id,
                        lamport_ts: shadow.vp.lamport_ts.max(1),
                        parents: Vec::new(),
                    };
                    if let Ok(patch) = diff_file_to_patch(&ctx, &base, &[], None, Some(&self.cas))
                    {
                        shadow.file_patches.insert(rel_path.to_path_buf(), patch);
                        changed = true;
                    }
                } else if shadow.file_patches.remove(rel_path).is_some() {
                    changed = true;
                }
                if let Some(prev_hash) = shadow.persisted_files.remove(rel_path) {
                    let prev_bytes = self.cas.get_bytes(&prev_hash).unwrap_or_default();
                    let fid = compute_logical_file_id(rel_path);
                    let lang = LanguageKind::detect(rel_path, &prev_bytes);
                    let next_ts = shadow.persisted_lamport_ts.max(shadow.vp.lamport_ts) + 1;
                    let parents: Vec<Blake3Hash> = shadow.last_patch_hash.into_iter().collect();
                    let wal_ctx = DiffContext {
                        logical_file_id: fid,
                        file_path: rel_path,
                        language: lang,
                        viewport_id: vp_id,
                        actor_id: &shadow.vp.agent_id,
                        lamport_ts: next_ts,
                        parents,
                    };
                    if let Ok(mut wal_patch) =
                        diff_file_to_patch(&wal_ctx, &prev_bytes, &[], None, Some(&self.cas))
                    {
                        wal_patch.cas_after = None;
                        wal_patch.finalize_hash();
                        shadow.last_patch_hash = Some(wal_patch.patch_hash);
                        shadow.persisted_lamport_ts = next_ts;
                        shadow.pending_wal_patches.push(wal_patch);
                    }
                    changed = true;
                }
                if changed {
                    shadow.rev += 1;
                }
            }
            return changed;
        };

        let content_hash = Blake3Hash::from_bytes(&bytes);
        let lang = LanguageKind::detect(rel_path, &bytes);
        let fid = compute_logical_file_id(rel_path);

        // Quick skip if unchanged
        if let Some(shadow) = self.viewports.get(vp_id)
            && let Some(prev) = shadow.files.get(rel_path)
            && prev.content_hash == content_hash
        {
            return false;
        }

        if lang.is_code()
            && let Ok(new_src) = std::str::from_utf8(&bytes)
        {
            let prev_state = self
                .viewports
                .get_mut(vp_id)
                .and_then(|s| s.files.remove(rel_path));
            let (old_src, mut old_tree, last_valid_source, last_valid_extracted) = match prev_state
            {
                Some(st) => (
                    st.current_source,
                    st.tree,
                    st.last_valid_source,
                    st.last_valid_extracted,
                ),
                None => (String::new(), None, None, None),
            };

            let parsed_res = {
                let parser = self.get_or_init_parser(lang);
                parse_source_incremental(lang, &old_src, new_src, old_tree.as_mut(), parser)
            };

            let Ok(inc) = parsed_res else {
                return false;
            };

            let has_err = inc.parsed.tree.root_node().has_error();
            let now = now_ns();
            let extracted = extract_file_symbols(&inc.parsed, fid, rel_path, vp_id, now);

            let (new_last_valid_src, new_last_valid_ext, effective_ext, pending_syntax) =
                if !has_err {
                    (
                        Some(new_src.to_string()),
                        Some(extracted.clone()),
                        extracted.clone(),
                        false,
                    )
                } else {
                    let effective = merge_pending_syntax_symbols(
                        last_valid_extracted.as_ref(),
                        &extracted,
                    );
                    (
                        last_valid_source.clone(),
                        last_valid_extracted.clone(),
                        effective,
                        true,
                    )
                };

            let new_cached = CachedFileState {
                content_hash,
                current_source: new_src.to_string(),
                tree: Some(inc.parsed.tree),
                last_valid_source: new_last_valid_src,
                last_valid_extracted: new_last_valid_ext,
            };

            let Some(shadow) = self.viewports.get_mut(vp_id) else {
                return false;
            };
            shadow.files.insert(rel_path.to_path_buf(), new_cached);
            shadow
                .provides_by_file
                .insert(rel_path.to_path_buf(), effective_ext.provide_entries);
            shadow
                .consumes_by_file
                .insert(rel_path.to_path_buf(), effective_ext.consume_entries);

            if pending_syntax {
                shadow.pending_syntax_files.insert(rel_path.to_path_buf());
                // Retain last valid patch in `shadow.file_patches` (never emit CRDT fallback or false RemoveNode!)
            } else {
                shadow.pending_syntax_files.remove(rel_path);
                let base_slice = base_bytes.as_deref().unwrap_or(&[]);
                if base_slice == bytes.as_slice() {
                    shadow.file_patches.remove(rel_path);
                } else {
                    let ctx = DiffContext {
                        logical_file_id: fid,
                        file_path: rel_path,
                        language: lang,
                        viewport_id: vp_id,
                        actor_id: &shadow.vp.agent_id,
                        lamport_ts: shadow.vp.lamport_ts.max(1),
                        parents: Vec::new(),
                    };
                    if let Ok(patch) =
                        diff_file_to_patch(&ctx, base_slice, &bytes, None, Some(&self.cas))
                    {
                        if patch.ops.is_empty() && patch.contract_deltas.is_empty() {
                            shadow.file_patches.remove(rel_path);
                        } else {
                            shadow.file_patches.insert(rel_path.to_path_buf(), patch);
                        }
                    }
                }
                let prev_hash_opt = shadow.persisted_files.get(rel_path).copied();
                if prev_hash_opt != Some(content_hash) {
                    let prev_bytes = prev_hash_opt
                        .and_then(|h| self.cas.get_bytes(&h).ok())
                        .unwrap_or_default();
                    let next_ts = shadow.persisted_lamport_ts.max(shadow.vp.lamport_ts) + 1;
                    let parents: Vec<Blake3Hash> = shadow.last_patch_hash.into_iter().collect();
                    let wal_ctx = DiffContext {
                        logical_file_id: fid,
                        file_path: rel_path,
                        language: lang,
                        viewport_id: vp_id,
                        actor_id: &shadow.vp.agent_id,
                        lamport_ts: next_ts,
                        parents,
                    };
                    if let Ok(wal_patch) =
                        diff_file_to_patch(&wal_ctx, &prev_bytes, &bytes, None, Some(&self.cas))
                    {
                        shadow
                            .persisted_files
                            .insert(rel_path.to_path_buf(), content_hash);
                        shadow.last_patch_hash = Some(wal_patch.patch_hash);
                        shadow.persisted_lamport_ts = next_ts;
                        shadow.pending_wal_patches.push(wal_patch);
                    }
                }
            }
            shadow.rev += 1;
            return true;
        }

        // Non-code CRDT (.json/.yaml/.md/.txt) or Binary file
        let Some(shadow) = self.viewports.get_mut(vp_id) else {
            return false;
        };
        shadow.pending_syntax_files.remove(rel_path);
        shadow.provides_by_file.remove(rel_path);
        shadow.consumes_by_file.remove(rel_path);
        shadow.files.insert(
            rel_path.to_path_buf(),
            CachedFileState {
                content_hash,
                current_source: String::new(),
                tree: None,
                last_valid_source: None,
                last_valid_extracted: None,
            },
        );

        let base_slice = base_bytes.as_deref().unwrap_or(&[]);
        if base_slice == bytes.as_slice() {
            shadow.file_patches.remove(rel_path);
        } else {
            let ctx = DiffContext {
                logical_file_id: fid,
                file_path: rel_path,
                language: lang,
                viewport_id: vp_id,
                actor_id: &shadow.vp.agent_id,
                lamport_ts: shadow.vp.lamport_ts.max(1),
                parents: Vec::new(),
            };
            if let Ok(patch) = diff_file_to_patch(&ctx, base_slice, &bytes, None, Some(&self.cas))
            {
                if patch.ops.is_empty()
                    && patch.contract_deltas.is_empty()
                    && patch.binary_manifest.is_none()
                {
                    shadow.file_patches.remove(rel_path);
                } else {
                    shadow.file_patches.insert(rel_path.to_path_buf(), patch);
                }
            }
        }
        let prev_hash_opt = shadow.persisted_files.get(rel_path).copied();
        if prev_hash_opt != Some(content_hash) {
            let prev_bytes = prev_hash_opt
                .and_then(|h| self.cas.get_bytes(&h).ok())
                .unwrap_or_default();
            let next_ts = shadow.persisted_lamport_ts.max(shadow.vp.lamport_ts) + 1;
            let parents: Vec<Blake3Hash> = shadow.last_patch_hash.into_iter().collect();
            let wal_ctx = DiffContext {
                logical_file_id: fid,
                file_path: rel_path,
                language: lang,
                viewport_id: vp_id,
                actor_id: &shadow.vp.agent_id,
                lamport_ts: next_ts,
                parents,
            };
            if let Ok(wal_patch) =
                diff_file_to_patch(&wal_ctx, &prev_bytes, &bytes, None, Some(&self.cas))
            {
                shadow
                    .persisted_files
                    .insert(rel_path.to_path_buf(), content_hash);
                shadow.last_patch_hash = Some(wal_patch.patch_hash);
                shadow.persisted_lamport_ts = next_ts;
                shadow.pending_wal_patches.push(wal_patch);
            }
        }
        shadow.rev += 1;
        true
    }

    async fn sync_viewport_symbols_to_db(&mut self, vp_id: &str) {
        let Some(shadow) = self.viewports.get(vp_id) else {
            return;
        };

        let next_provides = aggregate_provide_entries(shadow.provides_by_file.values());

        let mut next_consumes: HashMap<String, SymbolConsumeEntry> = HashMap::new();
        for list in shadow.consumes_by_file.values() {
            for entry in list {
                next_consumes
                    .entry(entry.symbol_fqn.clone())
                    .and_modify(|cur| {
                        cur.locations.extend(entry.locations.iter().cloned());
                        cur.updated_at_ns = cur.updated_at_ns.max(entry.updated_at_ns);
                    })
                    .or_insert_with(|| entry.clone());
            }
        }

        let mut remove_provides = Vec::new();
        for old_sym in shadow.published_provides.keys() {
            if !next_provides.contains_key(old_sym) {
                remove_provides.push((old_sym.clone(), vp_id.to_string()));
            }
        }
        let mut upsert_provides = Vec::new();
        for (sym, entry) in &next_provides {
            if shadow.published_provides.get(sym) != Some(entry) {
                upsert_provides.push(entry.clone());
            }
        }

        let mut remove_consumes = Vec::new();
        for old_sym in shadow.published_consumes.keys() {
            if !next_consumes.contains_key(old_sym) {
                remove_consumes.push((old_sym.clone(), vp_id.to_string()));
            }
        }
        let mut upsert_consumes = Vec::new();
        for (sym, entry) in &next_consumes {
            if shadow.published_consumes.get(sym) != Some(entry) {
                upsert_consumes.push(entry.clone());
            }
        }

        if remove_provides.is_empty()
            && upsert_provides.is_empty()
            && remove_consumes.is_empty()
            && upsert_consumes.is_empty()
        {
            return;
        }

        if let Some(shadow_mut) = self.viewports.get_mut(vp_id) {
            shadow_mut.published_provides = next_provides;
            shadow_mut.published_consumes = next_consumes;
        }

        let (tx, rx) = oneshot::channel();
        if self
            .writer
            .send(WriterCommand::SyncSymbolTables {
                remove_provides,
                upsert_provides,
                remove_consumes,
                upsert_consumes,
                reply: tx,
            })
            .await
            .is_ok()
        {
            let _ = rx.await;
        }
    }

    async fn clear_viewport_symbols_in_db(&self, vp_id: &str) {
        let (tx, rx) = oneshot::channel();
        if self
            .writer
            .send(WriterCommand::ClearViewportSymbols {
                viewport_id: vp_id.to_string(),
                reply: tx,
            })
            .await
            .is_ok()
        {
            let _ = rx.await;
        }
    }

    async fn reevaluate_and_publish(&mut self, changed_hint: Vec<String>) {
        let ids: Vec<String> = self.viewports.keys().cloned().collect();
        let mut per_vp_conflicts: BTreeMap<String, Vec<RadarConflictItem>> = run_blocking_inline(|| {
            let mut needed_trunk_paths: BTreeSet<PathBuf> = collect_non_ignored_files(
                &self.repo_root,
                &self.config.radar.ignore_dirs,
            )
            .into_iter()
            .collect();
            for id in &ids {
                if let Some(s) = self.viewports.get(id) {
                    for path in s.file_patches.keys() {
                        needed_trunk_paths.insert(path.clone());
                    }
                    if let Some(ref bf) = s.base_files {
                        for path in bf.keys() {
                            needed_trunk_paths.insert(path.clone());
                        }
                    }
                }
            }
            for path in needed_trunk_paths {
                let _ = self.load_trunk_bytes(&path);
            }

            let mut conflicts_map: BTreeMap<String, Vec<RadarConflictItem>> = BTreeMap::new();
            for id in &ids {
                conflicts_map.insert(id.clone(), Vec::new());
            }

            for i in 0..ids.len() {
                for j in (i + 1)..ids.len() {
                    let id_a = &ids[i];
                    let id_b = &ids[j];
                    let pair_key = if id_a <= id_b {
                        (id_a.clone(), id_b.clone())
                    } else {
                        (id_b.clone(), id_a.clone())
                    };
                    let (first_id, second_id) = (&pair_key.0, &pair_key.1);
                    let Some(vp_a) = self.viewports.get(first_id) else {
                        continue;
                    };
                    let Some(vp_b) = self.viewports.get(second_id) else {
                        continue;
                    };

                    let cache_hit = self
                        .pair_cache
                        .get(&pair_key)
                        .filter(|c| c.rev_a == vp_a.rev && c.rev_b == vp_b.rev);

                    let (fa, fb) = if let Some(cached) = cache_hit {
                        (
                            cached.conflicts_for_a.clone(),
                            cached.conflicts_for_b.clone(),
                        )
                    } else {
                        let eval = evaluate_viewport_pair(vp_a, vp_b, &self.trunk_files, &self.cas);
                        let fa = eval.conflicts_for_a.clone();
                        let fb = eval.conflicts_for_b.clone();
                        self.pair_cache.insert(pair_key.clone(), eval);
                        (fa, fb)
                    };

                    if let Some(list) = conflicts_map.get_mut(first_id) {
                        list.extend(fa);
                    }
                    if let Some(list) = conflicts_map.get_mut(second_id) {
                        list.extend(fb);
                    }
                }
            }

            // Evaluate each active viewport against advanced trunk (so stale sole/disjoint
            // viewports report trunk conflicts accurately instead of false GREEN).
            for id in &ids {
                let Some(vp) = self.viewports.get(id) else {
                    continue;
                };
                let trunk_conflicts = if let Some(cached) = self
                    .trunk_cache
                    .get(id)
                    .filter(|c| c.vp_rev == vp.rev && c.trunk_rev == self.trunk_rev)
                {
                    cached.conflicts.clone()
                } else {
                    let eval = evaluate_trunk_vs_viewport(
                        vp,
                        &self.trunk_files,
                        &self.cas,
                        self.trunk_rev,
                    );
                    let items = eval.conflicts.clone();
                    self.trunk_cache.insert(id.clone(), eval);
                    items
                };
                if let Some(list) = conflicts_map.get_mut(id) {
                    list.extend(trunk_conflicts);
                }
            }
            conflicts_map
        });

        let now_millis = now_ms();
        let mut escalated_notifications = Vec::new();
        let mut changed_vps: BTreeSet<String> = changed_hint.into_iter().collect();

        for id in &ids {
            let (
                touched_syms,
                dirty_files,
                vp_name,
                vp_path,
                pending_files,
                external_conflicts,
                has_written_status,
                prev_status,
                prev_conflicts,
                prev_warnings,
                prev_pending_files,
            ) = {
                let s = &self.viewports[id];
                (
                    s.touched_symbols(),
                    s.dirty_files(),
                    s.vp.name.clone(),
                    s.vp.path.clone(),
                    s.pending_syntax_files.iter().cloned().collect::<Vec<_>>(),
                    s.external_conflicts.clone(),
                    s.has_written_status,
                    s.last_status,
                    s.last_conflicts.clone(),
                    s.last_warnings.clone(),
                    s.last_pending_files.clone(),
                )
            };

            let negative_warnings = if touched_syms.is_empty() && dirty_files.is_empty() {
                Vec::new()
            } else {
                run_blocking_inline(|| {
                    self.db
                        .query_negative_edges_matching(&touched_syms, &dirty_files)
                        .unwrap_or_default()
                })
            };

            let mut conflicts = per_vp_conflicts.remove(id).unwrap_or_default();
            conflicts.extend(external_conflicts);

            let level = conflicts
                .iter()
                .map(|c| c.level)
                .max()
                .unwrap_or(RadarLevel::Green);

            let report = RadarStatusReport {
                viewport_id: id.clone(),
                viewport_name: vp_name,
                status: level,
                pending_syntax: !pending_files.is_empty(),
                conflicts: conflicts.clone(),
                pending_syntax_files: pending_files.clone(),
                negative_warnings: negative_warnings.clone(),
                updated_at_ms: now_millis,
            };

            let status_changed = level != prev_status
                || conflicts != prev_conflicts
                || negative_warnings != prev_warnings
                || pending_files != prev_pending_files;
            let status_path = vp_path.join(".loom").join("RADAR_STATUS.json");
            if !has_written_status
                || status_changed
                || changed_vps.contains(id)
                || !status_path.is_file()
            {
                write_radar_status_atomic(&vp_path, &report).await;
            }

            if status_changed {
                changed_vps.insert(id.clone());
                if level >= RadarLevel::Yellow {
                    escalated_notifications.push(report);
                }
            }

            if let Some(s) = self.viewports.get_mut(id) {
                s.has_written_status = true;
                s.last_status = level;
                s.last_conflicts = conflicts;
                s.last_warnings = negative_warnings;
                s.last_pending_files = pending_files;
            }
        }

        self.sequence += 1;
        let snapshot = self.build_snapshot();
        let _ = self.watch_tx.send(snapshot.clone());

        if !changed_vps.is_empty() || !escalated_notifications.is_empty() {
            let _ = self.broadcast_tx.send(RadarStreamEvent {
                snapshot,
                changed_viewports: changed_vps.into_iter().collect(),
                escalated_notifications,
            });
        }
    }

    fn build_snapshot(&self) -> RadarSnapshot {
        let cfg = self.config.radar.normalized();
        let effective_debounce_ms = if self.batch_mode {
            cfg.batch_window_ms
        } else {
            cfg.debounce_ms
        };
        let mut vp_summaries = Vec::with_capacity(self.viewports.len());
        let ids: Vec<String> = self.viewports.keys().cloned().collect();
        for id in &ids {
            let s = &self.viewports[id];
            vp_summaries.push(RadarViewportSummary {
                viewport_id: s.vp.id.clone(),
                viewport_name: s.vp.name.clone(),
                intent: s.vp.intent.clone(),
                state: s.vp.state,
                level: s.last_status,
                pending_syntax: !s.pending_syntax_files.is_empty(),
                pending_syntax_files: s.pending_syntax_files.iter().cloned().collect(),
                conflicts: s.last_conflicts.clone(),
                negative_warnings: s.last_warnings.clone(),
                dirty_files: s.dirty_files(),
                updated_at_ms: now_ms(),
            });
        }

        let mut matrix = Vec::new();
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                let id_a = &ids[i];
                let id_b = &ids[j];
                let pair_key = if id_a <= id_b {
                    (id_a.clone(), id_b.clone())
                } else {
                    (id_b.clone(), id_a.clone())
                };
                let va = &self.viewports[&pair_key.0];
                let vb = &self.viewports[&pair_key.1];
                let (level, conflicts) = match self.pair_cache.get(&pair_key) {
                    Some(eval) => (eval.level, eval.conflicts_for_a.clone()),
                    None => (RadarLevel::Green, Vec::new()),
                };
                matrix.push(RadarMatrixCell {
                    viewport_a_id: va.vp.id.clone(),
                    viewport_a_name: va.vp.name.clone(),
                    viewport_b_id: vb.vp.id.clone(),
                    viewport_b_name: vb.vp.name.clone(),
                    level,
                    conflicts,
                });
            }
        }

        RadarSnapshot {
            sequence: self.sequence,
            repo_root: self.repo_root.clone(),
            batch_mode: self.batch_mode,
            debounce_ms: effective_debounce_ms,
            metrics: RadarBatchMetrics {
                batch_mode: self.batch_mode,
                effective_debounce_ms,
                batch_switches: self.batch_switches,
                coalesced_events: self.coalesced_events,
            },
            viewports: vp_summaries,
            matrix,
            updated_at_ms: now_ms(),
        }
    }
}

fn merge_pending_syntax_symbols(
    last_valid: Option<&ExtractedSymbols>,
    current: &ExtractedSymbols,
) -> ExtractedSymbols {
    let Some(prev) = last_valid else {
        return current.clone();
    };

    let mut merged_provide_entries: Vec<SymbolProvideEntry> = Vec::new();
    let mut seen_fqns: HashSet<String> = HashSet::new();

    for entry in &current.provide_entries {
        if entry.status == SymbolStatus::Ready {
            seen_fqns.insert(entry.symbol_fqn.clone());
            merged_provide_entries.push(entry.clone());
        }
    }

    for old_entry in &prev.provide_entries {
        if !seen_fqns.contains(&old_entry.symbol_fqn) {
            let mut retained = old_entry.clone();
            retained.status = SymbolStatus::PendingSyntax;
            seen_fqns.insert(retained.symbol_fqn.clone());
            merged_provide_entries.push(retained);
        }
    }

    let mut merged_provides = Vec::new();
    let mut seen_prov_fqns: HashSet<String> = HashSet::new();
    for p in &current.provides {
        if p.status == SymbolStatus::Ready {
            seen_prov_fqns.insert(p.anchor.symbol_fqn.clone());
            merged_provides.push(p.clone());
        }
    }
    for old_p in &prev.provides {
        if !seen_prov_fqns.contains(&old_p.anchor.symbol_fqn) {
            let mut retained = old_p.clone();
            retained.status = SymbolStatus::PendingSyntax;
            seen_prov_fqns.insert(retained.anchor.symbol_fqn.clone());
            merged_provides.push(retained);
        }
    }

    let mut consume_map: BTreeMap<String, SymbolConsumeEntry> = BTreeMap::new();
    for c in &prev.consume_entries {
        consume_map.insert(c.symbol_fqn.clone(), c.clone());
    }
    for c in &current.consume_entries {
        consume_map
            .entry(c.symbol_fqn.clone())
            .and_modify(|existing| {
                for loc in &c.locations {
                    if !existing.locations.contains(loc) {
                        existing.locations.push(loc.clone());
                    }
                }
                existing.updated_at_ns = existing.updated_at_ns.max(c.updated_at_ns);
            })
            .or_insert_with(|| c.clone());
    }

    ExtractedSymbols {
        provides: merged_provides,
        consumes: current.consumes.clone(),
        provide_entries: merged_provide_entries,
        consume_entries: consume_map.into_values().collect(),
        pending_syntax: true,
    }
}

fn is_strict_descendant(candidate: &Path, dir: &Path) -> bool {
    if candidate == dir || dir.as_os_str().is_empty() {
        return false;
    }
    if candidate.starts_with(dir) {
        return true;
    }
    #[cfg(windows)]
    {
        let c_comps: Vec<String> = candidate
            .components()
            .filter_map(|c| match c {
                Component::Normal(s) => Some(s.to_string_lossy().to_lowercase()),
                _ => None,
            })
            .collect();
        let d_comps: Vec<String> = dir
            .components()
            .filter_map(|c| match c {
                Component::Normal(s) => Some(s.to_string_lossy().to_lowercase()),
                _ => None,
            })
            .collect();
        c_comps.len() > d_comps.len() && c_comps[..d_comps.len()] == d_comps[..]
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn collect_tracked_descendants(shadow: &ViewportShadow, dir_rel: &Path) -> BTreeSet<PathBuf> {
    let mut out = BTreeSet::new();
    let mut consider = |p: &Path| {
        if is_strict_descendant(p, dir_rel) {
            out.insert(p.to_path_buf());
        }
    };
    for p in shadow.files.keys() {
        consider(p);
    }
    for p in &shadow.pending_syntax_files {
        consider(p);
    }
    for p in shadow.file_patches.keys() {
        consider(p);
    }
    for p in shadow.provides_by_file.keys() {
        consider(p);
    }
    for p in shadow.consumes_by_file.keys() {
        consider(p);
    }
    for p in shadow.persisted_files.keys() {
        consider(p);
    }
    if let Some(bf) = &shadow.base_files {
        for p in bf.keys() {
            consider(p);
        }
    }
    out
}

fn collect_shadow_renames(
    vp: &ViewportShadow,
    trunk_files: &BTreeMap<PathBuf, Option<Vec<u8>>>,
    cas: &CasStore,
) -> Vec<(PathBuf, PathBuf)> {
    let mut deleted: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
    let mut created: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
    for (path, patch) in &vp.file_patches {
        let base_opt = vp.base_bytes(path, trunk_files, cas);
        let in_vp = vp.files.contains_key(path);
        if !in_vp {
            if let Some(base_bytes) = base_opt {
                deleted.insert(path.clone(), base_bytes);
            }
        } else if base_opt.is_none()
            && let Some(bytes) = patch
                .cas_after
                .and_then(|h| cas.get_bytes(&h).ok())
                .or_else(|| {
                    vp.files
                        .get(path)
                        .filter(|st| !st.current_source.is_empty())
                        .map(|st| st.current_source.as_bytes().to_vec())
                })
        {
            created.insert(path.clone(), bytes);
        }
    }
    if deleted.is_empty() || created.is_empty() {
        return Vec::new();
    }
    crate::viewport_mgr::detect_renames(&deleted, &created)
}

fn append_shadow_patches_with_renames(
    vp: &ViewportShadow,
    renames: &[(PathBuf, PathBuf)],
    trunk_files: &BTreeMap<PathBuf, Option<Vec<u8>>>,
    cas: &CasStore,
    subset_base: &mut BTreeMap<PathBuf, Vec<u8>>,
    all_patches: &mut Vec<AstPatch>,
) {
    let mut skip_paths: HashSet<PathBuf> = HashSet::new();
    for (old_path, new_path) in renames {
        skip_paths.insert(old_path.clone());
        skip_paths.insert(new_path.clone());
        let base_bytes = vp.base_bytes(old_path, trunk_files, cas).unwrap_or_default();
        let new_bytes = vp
            .file_patches
            .get(new_path)
            .and_then(|p| p.cas_after)
            .and_then(|h| cas.get_bytes(&h).ok())
            .or_else(|| {
                vp.files
                    .get(new_path)
                    .filter(|st| !st.current_source.is_empty())
                    .map(|st| st.current_source.as_bytes().to_vec())
            })
            .unwrap_or_default();
        subset_base.entry(old_path.clone()).or_insert_with(|| base_bytes.clone());
        let fid = compute_logical_file_id(old_path);
        let lang = LanguageKind::detect(new_path, &new_bytes);
        let ctx = DiffContext {
            logical_file_id: fid,
            file_path: new_path,
            language: lang,
            viewport_id: &vp.vp.id,
            actor_id: &vp.vp.agent_id,
            lamport_ts: vp.vp.lamport_ts.max(1),
            parents: Vec::new(),
        };
        if let Ok(patch) = diff_file_to_patch(&ctx, &base_bytes, &new_bytes, Some(old_path), Some(cas))
        {
            all_patches.push(patch);
        }
    }
    for (p, patch) in &vp.file_patches {
        if skip_paths.contains(p) {
            continue;
        }
        if let Some(bytes) = vp.base_bytes(p, trunk_files, cas) {
            subset_base.entry(p.clone()).or_insert(bytes);
        }
        all_patches.push(patch.clone());
    }
}

fn shadow_modified_existing_file(
    vp: &ViewportShadow,
    rel: &Path,
    trunk_files: &BTreeMap<PathBuf, Option<Vec<u8>>>,
    cas: &CasStore,
) -> bool {
    let Some(cached) = vp.files.get(rel) else {
        return false;
    };
    if vp.file_patches.contains_key(rel) {
        return true;
    }
    vp.base_bytes(rel, trunk_files, cas)
        .is_some_and(|bb| Blake3Hash::from_bytes(&bb) != cached.content_hash)
}

fn evaluate_viewport_pair(
    vp_a: &ViewportShadow,
    vp_b: &ViewportShadow,
    trunk_files: &BTreeMap<PathBuf, Option<Vec<u8>>>,
    cas: &CasStore,
) -> CachedPairEval {
    if vp_a.file_patches.is_empty() && vp_b.file_patches.is_empty() {
        return CachedPairEval {
            rev_a: vp_a.rev,
            rev_b: vp_b.rev,
            level: RadarLevel::Green,
            conflicts_for_a: Vec::new(),
            conflicts_for_b: Vec::new(),
        };
    }

    let mut conflicts_a: Vec<RadarConflictItem> = Vec::new();
    let mut conflicts_b: Vec<RadarConflictItem> = Vec::new();

    let renames_a = collect_shadow_renames(vp_a, trunk_files, cas);
    let renames_b = collect_shadow_renames(vp_b, trunk_files, cas);
    let renames_a_olds: HashSet<&PathBuf> = renames_a.iter().map(|(o, _)| o).collect();
    let renames_b_olds: HashSet<&PathBuf> = renames_b.iter().map(|(o, _)| o).collect();

    let mut delete_vs_modify_paths: BTreeSet<PathBuf> = BTreeSet::new();
    for rel in vp_a.file_patches.keys() {
        if !vp_a.files.contains_key(rel)
            && !renames_a_olds.contains(rel)
            && vp_b.files.contains_key(rel)
            && shadow_modified_existing_file(vp_b, rel, trunk_files, cas)
        {
            delete_vs_modify_paths.insert(rel.clone());
        }
    }
    for rel in vp_b.file_patches.keys() {
        if !vp_b.files.contains_key(rel)
            && !renames_b_olds.contains(rel)
            && vp_a.files.contains_key(rel)
            && shadow_modified_existing_file(vp_a, rel, trunk_files, cas)
        {
            delete_vs_modify_paths.insert(rel.clone());
        }
    }

    let has_rename_overlap = renames_a.iter().any(|(o, n)| {
        vp_b.file_patches.contains_key(o)
            || vp_b.file_patches.contains_key(n)
            || renames_b.iter().any(|(bo, bn)| bo == o || bn == n || bo == n || bn == o)
    }) || renames_b.iter().any(|(o, n)| {
        vp_a.file_patches.contains_key(o) || vp_a.file_patches.contains_key(n)
    });

    // Check fast disjointness: shared touched file or cross-viewport symbol contract/body touch
    let shared_files: BTreeSet<PathBuf> = vp_a
        .file_patches
        .keys()
        .filter(|p| vp_b.file_patches.contains_key(*p))
        .cloned()
        .collect();

    let a_mod_syms = collect_modified_symbols_with_files(&vp_a.file_patches);
    let b_mod_syms = collect_modified_symbols_with_files(&vp_b.file_patches);
    let a_consumes = vp_a.all_consumes();
    let b_consumes = vp_b.all_consumes();

    let a_touches_b_consume = a_mod_syms.iter().any(|(sym, _)| {
        b_consumes
            .iter()
            .any(|c| symbol_matches_query(sym, &c.symbol_fqn))
    });
    let b_touches_a_consume = b_mod_syms.iter().any(|(sym, _)| {
        a_consumes
            .iter()
            .any(|c| symbol_matches_query(sym, &c.symbol_fqn))
    });

    if shared_files.is_empty()
        && !has_rename_overlap
        && !a_touches_b_consume
        && !b_touches_a_consume
        && delete_vs_modify_paths.is_empty()
    {
        return CachedPairEval {
            rev_a: vp_a.rev,
            rev_b: vp_b.rev,
            level: RadarLevel::Green,
            conflicts_for_a: Vec::new(),
            conflicts_for_b: Vec::new(),
        };
    }

    let mut red_files: HashSet<PathBuf> = HashSet::new();
    let mut red_symbols: HashSet<String> = HashSet::new();

    // Detect rename-vs-delete conflicts explicitly (merge_patches applies edits to renamed dest)
    for (old_path, new_path) in &renames_a {
        let b_deleted_old = vp_b.file_patches.contains_key(old_path)
            && !vp_b.files.contains_key(old_path)
            && !renames_b_olds.contains(old_path);
        if b_deleted_old {
            red_files.insert(old_path.clone());
            let msg = format!(
                "rename-vs-delete conflict on '{}' (renamed to '{}')",
                old_path.display(),
                new_path.display()
            );
            conflicts_a.push(RadarConflictItem {
                peer_viewport_id: vp_b.vp.id.clone(),
                peer_viewport_name: vp_b.vp.name.clone(),
                level: RadarLevel::Red,
                kind: "structural_conflict".to_string(),
                file_path: Some(old_path.clone()),
                symbol_fqn: None,
                message: msg.clone(),
            });
            conflicts_b.push(RadarConflictItem {
                peer_viewport_id: vp_a.vp.id.clone(),
                peer_viewport_name: vp_a.vp.name.clone(),
                level: RadarLevel::Red,
                kind: "structural_conflict".to_string(),
                file_path: Some(old_path.clone()),
                symbol_fqn: None,
                message: msg,
            });
        }
    }
    for (old_path, new_path) in &renames_b {
        let a_deleted_old = vp_a.file_patches.contains_key(old_path)
            && !vp_a.files.contains_key(old_path)
            && !renames_a_olds.contains(old_path);
        if a_deleted_old && !red_files.contains(old_path) {
            red_files.insert(old_path.clone());
            let msg = format!(
                "rename-vs-delete conflict on '{}' (renamed to '{}')",
                old_path.display(),
                new_path.display()
            );
            conflicts_a.push(RadarConflictItem {
                peer_viewport_id: vp_b.vp.id.clone(),
                peer_viewport_name: vp_b.vp.name.clone(),
                level: RadarLevel::Red,
                kind: "structural_conflict".to_string(),
                file_path: Some(old_path.clone()),
                symbol_fqn: None,
                message: msg.clone(),
            });
            conflicts_b.push(RadarConflictItem {
                peer_viewport_id: vp_a.vp.id.clone(),
                peer_viewport_name: vp_a.vp.name.clone(),
                level: RadarLevel::Red,
                kind: "structural_conflict".to_string(),
                file_path: Some(old_path.clone()),
                symbol_fqn: None,
                message: msg,
            });
        }
    }

    // Detect delete-vs-modify conflicts explicitly (including top-level InsertMember or comment-only files)
    for rel in delete_vs_modify_paths {
        if red_files.insert(rel.clone()) {
            let msg = format!("delete-vs-modify conflict on '{}'", rel.display());
            conflicts_a.push(RadarConflictItem {
                peer_viewport_id: vp_b.vp.id.clone(),
                peer_viewport_name: vp_b.vp.name.clone(),
                level: RadarLevel::Red,
                kind: "structural_conflict".to_string(),
                file_path: Some(rel.clone()),
                symbol_fqn: None,
                message: msg.clone(),
            });
            conflicts_b.push(RadarConflictItem {
                peer_viewport_id: vp_a.vp.id.clone(),
                peer_viewport_name: vp_a.vp.name.clone(),
                level: RadarLevel::Red,
                kind: "structural_conflict".to_string(),
                file_path: Some(rel),
                symbol_fqn: None,
                message: msg,
            });
        }
    }

    // Build subset base for merge_patches (preferring newer branch base when bases differ,
    // retrying alternate base or per-viewport divergent-base conflict check on AnchorNotFound)
    let build_pair = |first: &ViewportShadow,
                      renames_first: &[(PathBuf, PathBuf)],
                      second: &ViewportShadow,
                      renames_second: &[(PathBuf, PathBuf)]| {
        let mut subset_base: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
        let mut all_patches: Vec<AstPatch> = Vec::new();
        append_shadow_patches_with_renames(
            first,
            renames_first,
            trunk_files,
            cas,
            &mut subset_base,
            &mut all_patches,
        );
        append_shadow_patches_with_renames(
            second,
            renames_second,
            trunk_files,
            cas,
            &mut subset_base,
            &mut all_patches,
        );
        (subset_base, all_patches)
    };

    let (primary, renames_p, secondary, renames_s) = if vp_b.vp.base_op_seq > vp_a.vp.base_op_seq
    {
        (vp_b, &renames_b, vp_a, &renames_a)
    } else {
        (vp_a, &renames_a, vp_b, &renames_b)
    };
    let (subset_base, all_patches) = build_pair(primary, renames_p, secondary, renames_s);

    let merge_outcomes = match merge_patches(&subset_base, &all_patches, Some(cas)) {
        Ok(report) => Ok(report.conflicts),
        Err(
            CstError::AnchorNotFound(_)
            | CstError::Crdt(loom_core::crdt::CrdtError::MissingDependencies(_)),
        ) => {
            let (alt_base, alt_patches) = build_pair(secondary, renames_s, primary, renames_p);
            match merge_patches(&alt_base, &alt_patches, Some(cas)) {
                Ok(report) => Ok(report.conflicts),
                Err(
                    CstError::AnchorNotFound(_)
                    | CstError::Crdt(loom_core::crdt::CrdtError::MissingDependencies(_)),
                ) => {
                    let (renames_older, renames_newer) =
                        if vp_b.vp.base_op_seq > vp_a.vp.base_op_seq {
                            (&renames_a, &renames_b)
                        } else {
                            (&renames_b, &renames_a)
                        };
                    let mut base_a = BTreeMap::new();
                    let mut patches_a = Vec::new();
                    append_shadow_patches_with_renames(
                        secondary,
                        renames_older,
                        trunk_files,
                        cas,
                        &mut base_a,
                        &mut patches_a,
                    );
                    let mut base_b = BTreeMap::new();
                    let mut patches_b = Vec::new();
                    append_shadow_patches_with_renames(
                        primary,
                        renames_newer,
                        trunk_files,
                        cas,
                        &mut base_b,
                        &mut patches_b,
                    );
                    detect_divergent_base_conflicts(&base_a, &patches_a, &base_b, &patches_b)
                }
                Err(err) => Err(err),
            }
        }
        Err(err) => Err(err),
    };

    match merge_outcomes {
        Ok(outcome_conflicts) => {
            for c in outcome_conflicts {
                match c {
                    MergeOutcome::StructuralConflict {
                        file,
                        anchor,
                        reason,
                        ..
                    } => {
                        if !red_files.insert(file.clone()) {
                            continue;
                        }
                        let sym = anchor.map(|a| a.symbol_fqn);
                        if let Some(ref s) = sym {
                            red_symbols.insert(s.clone());
                        }
                        conflicts_a.push(RadarConflictItem {
                            peer_viewport_id: vp_b.vp.id.clone(),
                            peer_viewport_name: vp_b.vp.name.clone(),
                            level: RadarLevel::Red,
                            kind: "structural_conflict".to_string(),
                            file_path: Some(file.clone()),
                            symbol_fqn: sym.clone(),
                            message: reason.clone(),
                        });
                        conflicts_b.push(RadarConflictItem {
                            peer_viewport_id: vp_a.vp.id.clone(),
                            peer_viewport_name: vp_a.vp.name.clone(),
                            level: RadarLevel::Red,
                            kind: "structural_conflict".to_string(),
                            file_path: Some(file),
                            symbol_fqn: sym,
                            message: reason,
                        });
                    }
                    MergeOutcome::SemanticConflict {
                        symbol_fqn, detail, ..
                    } => {
                        red_symbols.insert(symbol_fqn.clone());
                        conflicts_a.push(RadarConflictItem {
                            peer_viewport_id: vp_b.vp.id.clone(),
                            peer_viewport_name: vp_b.vp.name.clone(),
                            level: RadarLevel::Red,
                            kind: "semantic_conflict".to_string(),
                            file_path: None,
                            symbol_fqn: Some(symbol_fqn.clone()),
                            message: detail.clone(),
                        });
                        conflicts_b.push(RadarConflictItem {
                            peer_viewport_id: vp_a.vp.id.clone(),
                            peer_viewport_name: vp_a.vp.name.clone(),
                            level: RadarLevel::Red,
                            kind: "semantic_conflict".to_string(),
                            file_path: None,
                            symbol_fqn: Some(symbol_fqn),
                            message: detail,
                        });
                    }
                    MergeOutcome::Clean { .. } => {}
                }
            }
        }
        Err(err) => {
            let msg = format!("shadow merge error: {err}");
            conflicts_a.push(RadarConflictItem {
                peer_viewport_id: vp_b.vp.id.clone(),
                peer_viewport_name: vp_b.vp.name.clone(),
                level: RadarLevel::Red,
                kind: "structural_conflict".to_string(),
                file_path: shared_files.iter().next().cloned(),
                symbol_fqn: None,
                message: msg.clone(),
            });
            conflicts_b.push(RadarConflictItem {
                peer_viewport_id: vp_a.vp.id.clone(),
                peer_viewport_name: vp_a.vp.name.clone(),
                level: RadarLevel::Red,
                kind: "structural_conflict".to_string(),
                file_path: shared_files.iter().next().cloned(),
                symbol_fqn: None,
                message: msg,
            });
        }
    }

    // Advisory YELLOW: CRDT concurrent edits on shared non-code files (if not already RED)
    for file in &shared_files {
        if red_files.contains(file) {
            continue;
        }
        let lang = LanguageKind::from_path(file);
        if !lang.is_code() && lang != LanguageKind::Binary {
            let msg = format!(
                "concurrent CRDT edit on non-code file '{}'",
                file.display()
            );
            conflicts_a.push(RadarConflictItem {
                peer_viewport_id: vp_b.vp.id.clone(),
                peer_viewport_name: vp_b.vp.name.clone(),
                level: RadarLevel::Yellow,
                kind: "crdt_concurrent_edit".to_string(),
                file_path: Some(file.clone()),
                symbol_fqn: None,
                message: msg.clone(),
            });
            conflicts_b.push(RadarConflictItem {
                peer_viewport_id: vp_a.vp.id.clone(),
                peer_viewport_name: vp_a.vp.name.clone(),
                level: RadarLevel::Yellow,
                kind: "crdt_concurrent_edit".to_string(),
                file_path: Some(file.clone()),
                symbol_fqn: None,
                message: msg,
            });
        }
    }

    // Advisory YELLOW: consumed symbol modified by peer (if not already RED on that symbol)
    for (sym, file) in &a_mod_syms {
        if red_symbols.contains(sym) {
            continue;
        }
        if let Some(consume_entry) = b_consumes
            .iter()
            .find(|c| symbol_matches_query(sym, &c.symbol_fqn))
        {
            let consumer_file = consume_entry
                .locations
                .first()
                .map(|l| l.file_path.clone());
            let msg_for_a = format!(
                "symbol '{}' modified here is consumed by peer '{}'",
                sym, vp_b.vp.name
            );
            let msg_for_b = format!(
                "consumed symbol '{}' modified by peer '{}'",
                sym, vp_a.vp.name
            );
            conflicts_a.push(RadarConflictItem {
                peer_viewport_id: vp_b.vp.id.clone(),
                peer_viewport_name: vp_b.vp.name.clone(),
                level: RadarLevel::Yellow,
                kind: "consumed_symbol_modified".to_string(),
                file_path: Some(file.clone()),
                symbol_fqn: Some(sym.clone()),
                message: msg_for_a,
            });
            conflicts_b.push(RadarConflictItem {
                peer_viewport_id: vp_a.vp.id.clone(),
                peer_viewport_name: vp_a.vp.name.clone(),
                level: RadarLevel::Yellow,
                kind: "consumed_symbol_modified".to_string(),
                file_path: consumer_file.or_else(|| Some(file.clone())),
                symbol_fqn: Some(sym.clone()),
                message: msg_for_b,
            });
        }
    }

    for (sym, file) in &b_mod_syms {
        if red_symbols.contains(sym) {
            continue;
        }
        if let Some(consume_entry) = a_consumes
            .iter()
            .find(|c| symbol_matches_query(sym, &c.symbol_fqn))
        {
            let consumer_file = consume_entry
                .locations
                .first()
                .map(|l| l.file_path.clone());
            let msg_for_b = format!(
                "symbol '{}' modified here is consumed by peer '{}'",
                sym, vp_a.vp.name
            );
            let msg_for_a = format!(
                "consumed symbol '{}' modified by peer '{}'",
                sym, vp_b.vp.name
            );
            conflicts_b.push(RadarConflictItem {
                peer_viewport_id: vp_a.vp.id.clone(),
                peer_viewport_name: vp_a.vp.name.clone(),
                level: RadarLevel::Yellow,
                kind: "consumed_symbol_modified".to_string(),
                file_path: Some(file.clone()),
                symbol_fqn: Some(sym.clone()),
                message: msg_for_b,
            });
            conflicts_a.push(RadarConflictItem {
                peer_viewport_id: vp_b.vp.id.clone(),
                peer_viewport_name: vp_b.vp.name.clone(),
                level: RadarLevel::Yellow,
                kind: "consumed_symbol_modified".to_string(),
                file_path: consumer_file.or_else(|| Some(file.clone())),
                symbol_fqn: Some(sym.clone()),
                message: msg_for_a,
            });
        }
    }

    let level = conflicts_a
        .iter()
        .chain(conflicts_b.iter())
        .map(|c| c.level)
        .max()
        .unwrap_or(RadarLevel::Green);

    CachedPairEval {
        rev_a: vp_a.rev,
        rev_b: vp_b.rev,
        level,
        conflicts_for_a: conflicts_a,
        conflicts_for_b: conflicts_b,
    }
}

fn evaluate_trunk_vs_viewport(
    vp: &ViewportShadow,
    trunk_files: &BTreeMap<PathBuf, Option<Vec<u8>>>,
    cas: &CasStore,
    trunk_rev: u64,
) -> CachedTrunkEval {
    let Some(ref base_map) = vp.base_files else {
        return CachedTrunkEval {
            vp_rev: vp.rev,
            trunk_rev,
            conflicts: Vec::new(),
        };
    };
    let vp_has_changes = !vp.file_patches.is_empty()
        || vp.files.iter().any(|(rel, st)| {
            base_map.get(rel).copied() != Some(st.content_hash)
        })
        || base_map.keys().any(|rel| !vp.files.contains_key(rel));
    if !vp_has_changes {
        return CachedTrunkEval {
            vp_rev: vp.rev,
            trunk_rev,
            conflicts: Vec::new(),
        };
    }

    let mut all_rels: BTreeSet<PathBuf> = base_map.keys().cloned().collect();
    for (p, v) in trunk_files {
        if v.is_some() {
            all_rels.insert(p.clone());
        }
    }

    let mut trunk_patches: BTreeMap<PathBuf, AstPatch> = BTreeMap::new();
    let mut trunk_deleted: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
    let mut trunk_created: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();

    for rel in all_rels {
        let base_hash = base_map.get(&rel).copied();
        let trunk_opt = trunk_files.get(&rel).and_then(|o| o.as_deref());
        let trunk_hash = trunk_opt.map(Blake3Hash::from_bytes);
        if base_hash == trunk_hash {
            continue;
        }
        let base_bytes = base_hash
            .and_then(|h| cas.get_bytes(&h).ok())
            .unwrap_or_default();
        let trunk_slice = trunk_opt.unwrap_or(&[]);
        if base_hash.is_some() && trunk_opt.is_none() {
            trunk_deleted.insert(rel.clone(), base_bytes.clone());
        } else if base_hash.is_none() && let Some(tb) = trunk_opt {
            trunk_created.insert(rel.clone(), tb.to_vec());
        }
        let fid = compute_logical_file_id(&rel);
        let lang = LanguageKind::detect(
            &rel,
            if trunk_slice.is_empty() {
                &base_bytes
            } else {
                trunk_slice
            },
        );
        let ctx = DiffContext {
            logical_file_id: fid,
            file_path: &rel,
            language: lang,
            viewport_id: "__trunk__",
            actor_id: "__trunk__",
            lamport_ts: 1,
            parents: Vec::new(),
        };
        if let Ok(mut patch) = diff_file_to_patch(&ctx, &base_bytes, trunk_slice, None, Some(cas))
        {
            if trunk_opt.is_none() {
                patch.cas_after = None;
                patch.finalize_hash();
            }
            if !patch.ops.is_empty()
                || !patch.contract_deltas.is_empty()
                || patch.binary_manifest.is_some()
                || base_hash != trunk_hash
            {
                trunk_patches.insert(rel, patch);
            }
        }
    }

    if trunk_patches.is_empty() {
        return CachedTrunkEval {
            vp_rev: vp.rev,
            trunk_rev,
            conflicts: Vec::new(),
        };
    }

    let renames_vp = collect_shadow_renames(vp, trunk_files, cas);
    let renames_trunk = if !trunk_deleted.is_empty() && !trunk_created.is_empty() {
        crate::viewport_mgr::detect_renames(&trunk_deleted, &trunk_created)
    } else {
        Vec::new()
    };

    let has_rename_overlap = renames_vp.iter().any(|(o, n)| {
        trunk_patches.contains_key(o)
            || trunk_patches.contains_key(n)
            || renames_trunk
                .iter()
                .any(|(to, tn)| to == o || tn == n || to == n || tn == o)
    }) || renames_trunk
        .iter()
        .any(|(o, n)| vp.file_patches.contains_key(o) || vp.file_patches.contains_key(n));

    let shared_files: BTreeSet<PathBuf> = vp
        .file_patches
        .keys()
        .filter(|p| trunk_patches.contains_key(*p))
        .cloned()
        .collect();

    let trunk_mod_syms = collect_modified_symbols_with_files(&trunk_patches);
    let vp_consumes = vp.all_consumes();
    let trunk_touches_vp_consume = trunk_mod_syms.iter().any(|(sym, _)| {
        vp_consumes
            .iter()
            .any(|c| symbol_matches_query(sym, &c.symbol_fqn))
    });

    let vp_has_breaking_delta = vp.file_patches.values().any(|p| {
        p.contract_deltas.iter().any(|d| {
            matches!(
                d,
                SymbolContractDelta::RemovedSymbol { .. }
                    | SymbolContractDelta::SignatureChanged { .. }
                    | SymbolContractDelta::VisibilityReduced { .. }
            )
        })
    });

    let renames_vp_olds: HashSet<&PathBuf> = renames_vp.iter().map(|(o, _)| o).collect();
    let renames_trunk_olds: HashSet<&PathBuf> = renames_trunk.iter().map(|(o, _)| o).collect();

    let mut delete_vs_modify_paths: BTreeSet<PathBuf> = BTreeSet::new();
    for rel in trunk_deleted.keys() {
        if !renames_trunk_olds.contains(rel)
            && vp.files.contains_key(rel)
            && shadow_modified_existing_file(vp, rel, trunk_files, cas)
        {
            delete_vs_modify_paths.insert(rel.clone());
        }
    }
    for rel in vp.file_patches.keys().chain(base_map.keys()) {
        if !vp.files.contains_key(rel)
            && !renames_vp_olds.contains(rel)
            && trunk_files.get(rel).is_some_and(Option::is_some)
            && trunk_patches.contains_key(rel)
        {
            delete_vs_modify_paths.insert(rel.clone());
        }
    }

    if shared_files.is_empty()
        && !has_rename_overlap
        && !trunk_touches_vp_consume
        && !vp_has_breaking_delta
        && delete_vs_modify_paths.is_empty()
    {
        return CachedTrunkEval {
            vp_rev: vp.rev,
            trunk_rev,
            conflicts: Vec::new(),
        };
    }

    let mut conflicts: Vec<RadarConflictItem> = Vec::new();
    let mut red_files: HashSet<PathBuf> = HashSet::new();
    let mut red_symbols: HashSet<String> = HashSet::new();

    for (old_path, new_path) in &renames_vp {
        let trunk_deleted_old =
            trunk_deleted.contains_key(old_path) && !renames_trunk_olds.contains(old_path);
        if trunk_deleted_old {
            red_files.insert(old_path.clone());
            conflicts.push(RadarConflictItem {
                peer_viewport_id: "__trunk__".to_string(),
                peer_viewport_name: "trunk".to_string(),
                level: RadarLevel::Red,
                kind: "structural_conflict".to_string(),
                file_path: Some(old_path.clone()),
                symbol_fqn: None,
                message: format!(
                    "rename-vs-delete conflict on '{}' (renamed to '{}')",
                    old_path.display(),
                    new_path.display()
                ),
            });
        }
    }
    for (old_path, new_path) in &renames_trunk {
        let vp_deleted_old = vp.file_patches.contains_key(old_path)
            && !vp.files.contains_key(old_path)
            && !renames_vp_olds.contains(old_path);
        if vp_deleted_old && !red_files.contains(old_path) {
            red_files.insert(old_path.clone());
            conflicts.push(RadarConflictItem {
                peer_viewport_id: "__trunk__".to_string(),
                peer_viewport_name: "trunk".to_string(),
                level: RadarLevel::Red,
                kind: "structural_conflict".to_string(),
                file_path: Some(old_path.clone()),
                symbol_fqn: None,
                message: format!(
                    "rename-vs-delete conflict on '{}' (trunk renamed to '{}')",
                    old_path.display(),
                    new_path.display()
                ),
            });
        }
    }

    for rel in delete_vs_modify_paths {
        if red_files.insert(rel.clone()) {
            conflicts.push(RadarConflictItem {
                peer_viewport_id: "__trunk__".to_string(),
                peer_viewport_name: "trunk".to_string(),
                level: RadarLevel::Red,
                kind: "structural_conflict".to_string(),
                file_path: Some(rel.clone()),
                symbol_fqn: None,
                message: format!("delete-vs-modify conflict on '{}'", rel.display()),
            });
        }
    }

    let mut subset_base: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
    let mut all_patches: Vec<AstPatch> = Vec::new();
    append_shadow_patches_with_renames(
        vp,
        &renames_vp,
        trunk_files,
        cas,
        &mut subset_base,
        &mut all_patches,
    );

    let mut trunk_skip: HashSet<PathBuf> = HashSet::new();
    for (old_path, new_path) in &renames_trunk {
        trunk_skip.insert(old_path.clone());
        trunk_skip.insert(new_path.clone());
        let base_bytes = base_map
            .get(old_path)
            .and_then(|h| cas.get_bytes(h).ok())
            .unwrap_or_default();
        let new_bytes = trunk_files
            .get(new_path)
            .and_then(|o| o.as_deref())
            .unwrap_or(&[]);
        subset_base
            .entry(old_path.clone())
            .or_insert_with(|| base_bytes.clone());
        let fid = compute_logical_file_id(old_path);
        let lang = LanguageKind::detect(new_path, new_bytes);
        let ctx = DiffContext {
            logical_file_id: fid,
            file_path: new_path,
            language: lang,
            viewport_id: "__trunk__",
            actor_id: "__trunk__",
            lamport_ts: 1,
            parents: Vec::new(),
        };
        if let Ok(patch) = diff_file_to_patch(&ctx, &base_bytes, new_bytes, Some(old_path), Some(cas))
        {
            all_patches.push(patch);
        }
    }
    for (p, patch) in &trunk_patches {
        if trunk_skip.contains(p) {
            continue;
        }
        if let Some(bytes) = vp.base_bytes(p, trunk_files, cas) {
            subset_base.entry(p.clone()).or_insert(bytes);
        }
        all_patches.push(patch.clone());
    }

    match merge_patches(&subset_base, &all_patches, Some(cas)) {
        Ok(report) => {
            for c in report.conflicts {
                match c {
                    MergeOutcome::StructuralConflict {
                        file,
                        anchor,
                        reason,
                        ..
                    } => {
                        if !red_files.insert(file.clone()) {
                            continue;
                        }
                        let sym = anchor.map(|a| a.symbol_fqn);
                        if let Some(ref s) = sym {
                            red_symbols.insert(s.clone());
                        }
                        conflicts.push(RadarConflictItem {
                            peer_viewport_id: "__trunk__".to_string(),
                            peer_viewport_name: "trunk".to_string(),
                            level: RadarLevel::Red,
                            kind: "structural_conflict".to_string(),
                            file_path: Some(file),
                            symbol_fqn: sym,
                            message: reason,
                        });
                    }
                    MergeOutcome::SemanticConflict {
                        symbol_fqn, detail, ..
                    } => {
                        red_symbols.insert(symbol_fqn.clone());
                        conflicts.push(RadarConflictItem {
                            peer_viewport_id: "__trunk__".to_string(),
                            peer_viewport_name: "trunk".to_string(),
                            level: RadarLevel::Red,
                            kind: "semantic_conflict".to_string(),
                            file_path: None,
                            symbol_fqn: Some(symbol_fqn),
                            message: detail,
                        });
                    }
                    MergeOutcome::Clean { .. } => {}
                }
            }
        }
        Err(err) => {
            conflicts.push(RadarConflictItem {
                peer_viewport_id: "__trunk__".to_string(),
                peer_viewport_name: "trunk".to_string(),
                level: RadarLevel::Red,
                kind: "structural_conflict".to_string(),
                file_path: shared_files.iter().next().cloned(),
                symbol_fqn: None,
                message: format!("shadow merge error: {err}"),
            });
        }
    }

    for file in &shared_files {
        if red_files.contains(file) {
            continue;
        }
        let lang = LanguageKind::from_path(file);
        if !lang.is_code() && lang != LanguageKind::Binary {
            conflicts.push(RadarConflictItem {
                peer_viewport_id: "__trunk__".to_string(),
                peer_viewport_name: "trunk".to_string(),
                level: RadarLevel::Yellow,
                kind: "crdt_concurrent_edit".to_string(),
                file_path: Some(file.clone()),
                symbol_fqn: None,
                message: format!(
                    "concurrent CRDT edit on non-code file '{}' vs trunk",
                    file.display()
                ),
            });
        }
    }

    for (sym, file) in &trunk_mod_syms {
        if red_symbols.contains(sym) {
            continue;
        }
        if let Some(consume_entry) = vp_consumes
            .iter()
            .find(|c| symbol_matches_query(sym, &c.symbol_fqn))
        {
            let consumer_file = consume_entry
                .locations
                .first()
                .map(|l| l.file_path.clone());
            conflicts.push(RadarConflictItem {
                peer_viewport_id: "__trunk__".to_string(),
                peer_viewport_name: "trunk".to_string(),
                level: RadarLevel::Yellow,
                kind: "consumed_symbol_modified".to_string(),
                file_path: consumer_file.or_else(|| Some(file.clone())),
                symbol_fqn: Some(sym.clone()),
                message: format!("consumed symbol '{}' modified by trunk", sym),
            });
        }
    }

    CachedTrunkEval {
        vp_rev: vp.rev,
        trunk_rev,
        conflicts,
    }
}

fn collect_modified_symbols_with_files(
    patches: &BTreeMap<PathBuf, AstPatch>,
) -> Vec<(String, PathBuf)> {
    let mut seen: BTreeSet<(String, PathBuf)> = BTreeSet::new();
    for (file, patch) in patches {
        for delta in &patch.contract_deltas {
            match delta {
                SymbolContractDelta::RemovedSymbol { symbol_fqn, .. }
                | SymbolContractDelta::SignatureChanged { symbol_fqn, .. }
                | SymbolContractDelta::VisibilityReduced { symbol_fqn, .. } => {
                    seen.insert((symbol_fqn.clone(), file.clone()));
                }
                SymbolContractDelta::AddedSymbol { .. } => {}
            }
        }
        for op in &patch.ops {
            match op {
                SemanticOp::ReplaceBody { target, .. }
                | SemanticOp::MutateSignature { target, .. }
                | SemanticOp::RemoveNode { target, .. } => {
                    seen.insert((target.symbol_fqn.clone(), file.clone()));
                }
                SemanticOp::InsertMember { .. }
                | SemanticOp::RenameOrMoveFile { .. }
                | SemanticOp::TextCrdtFallback { .. } => {}
            }
        }
    }
    seen.into_iter().collect()
}

fn is_file_exact_case(root: &Path, rel_path: &Path) -> bool {
    if rel_path.as_os_str().is_empty() {
        return false;
    }
    let full_path = root.join(rel_path);
    let Ok(meta) = std::fs::metadata(&full_path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(windows)]
    {
        let mut cur = root.to_path_buf();
        for comp in rel_path.components() {
            let Component::Normal(expected) = comp else {
                return false;
            };
            let Ok(entries) = std::fs::read_dir(&cur) else {
                return false;
            };
            let mut matched = false;
            for entry in entries.flatten() {
                if entry.file_name() == expected {
                    matched = true;
                    break;
                }
            }
            if !matched {
                return false;
            }
            cur.push(expected);
        }
        true
    }
    #[cfg(not(windows))]
    {
        true
    }
}

pub fn collect_non_ignored_files(root: &Path, ignore_dirs: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel_dir) = stack.pop() {
        let abs_dir = if rel_dir.as_os_str().is_empty() {
            root.to_path_buf()
        } else {
            root.join(&rel_dir)
        };
        let Ok(rd) = std::fs::read_dir(&abs_dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let name = entry.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            if component_ignored(name_str, ignore_dirs) {
                continue;
            }
            let rel_child = if rel_dir.as_os_str().is_empty() {
                PathBuf::from(&name)
            } else {
                rel_dir.join(&name)
            };
            if let Ok(ft) = entry.file_type() {
                if ft.is_dir() {
                    if !ft.is_symlink() {
                        stack.push(rel_child);
                    }
                } else if ft.is_file() {
                    out.push(rel_child);
                }
            }
        }
    }
    out.sort();
    out
}

async fn write_radar_status_atomic(vp_root: &Path, report: &RadarStatusReport) {
    let vp_loom_dir = vp_root.join(".loom");
    let Ok(json) = serde_json::to_vec_pretty(report) else {
        return;
    };
    let status_path = vp_loom_dir.join("RADAR_STATUS.json");

    for attempt in 0..4 {
        let retry = run_blocking_inline(|| {
            if std::fs::create_dir_all(&vp_loom_dir).is_err() {
                return Some(false);
            }
            let Ok(mut tmp) = NamedTempFile::new_in(&vp_loom_dir) else {
                return Some(false);
            };
            if tmp.write_all(&json).is_err() || tmp.as_file().sync_data().is_err() {
                return Some(false);
            }
            match tmp.persist(&status_path) {
                Ok(_) => None,
                Err(err) => {
                    let is_sharing_violation = err.error.kind()
                        == std::io::ErrorKind::PermissionDenied
                        || matches!(err.error.raw_os_error(), Some(5) | Some(32));
                    if is_sharing_violation && attempt < 3 {
                        Some(true)
                    } else {
                        tracing::debug!(error = %err.error, path = %status_path.display(), "failed to persist RADAR_STATUS.json");
                        Some(false)
                    }
                }
            }
        });
        match retry {
            None => return,
            Some(true) => tokio::time::sleep(Duration::from_millis(3)).await,
            Some(false) => return,
        }
    }
}

pub fn filter_snapshot(mut snap: RadarSnapshot, viewport_filter: Option<&str>) -> RadarSnapshot {
    let Some(filter) = viewport_filter.map(str::trim).filter(|s| !s.is_empty()) else {
        return snap;
    };
    snap.viewports
        .retain(|v| v.viewport_id == filter || v.viewport_name == filter);
    let kept_ids: HashSet<String> = snap
        .viewports
        .iter()
        .map(|v| v.viewport_id.clone())
        .collect();
    snap.matrix.retain(|cell| {
        kept_ids.contains(&cell.viewport_a_id) || kept_ids.contains(&cell.viewport_b_id)
    });
    snap
}

fn aggregate_provide_entries<'a, I>(lists: I) -> HashMap<String, SymbolProvideEntry>
where
    I: IntoIterator<Item = &'a Vec<SymbolProvideEntry>>,
{
    let mut next_provides: HashMap<String, SymbolProvideEntry> = HashMap::new();
    for list in lists {
        for entry in list {
            next_provides
                .entry(entry.symbol_fqn.clone())
                .and_modify(|cur| {
                    let any_pending = cur.status == SymbolStatus::PendingSyntax
                        || entry.status == SymbolStatus::PendingSyntax;
                    if entry.updated_at_ns >= cur.updated_at_ns {
                        *cur = entry.clone();
                    }
                    if any_pending {
                        cur.status = SymbolStatus::PendingSyntax;
                    }
                })
                .or_insert_with(|| entry.clone());
        }
    }
    next_provides
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_case_mismatch_relative_path_preserves_filename_casing() {
        let filter = WatcherFilterState::new(vec![".loom".into()], Duration::from_millis(200));
        #[cfg(windows)]
        let root = PathBuf::from(r"C:\Users\Developer\Repo\.loom\viewports\v1");
        #[cfg(not(windows))]
        let root = PathBuf::from("/Users/Developer/Repo/.loom/viewports/v1");
        filter.add_watched_root(&root);

        #[cfg(windows)]
        let event_path = PathBuf::from(r"c:\users\developer\repo\.loom\viewports\v1\src\MixedCase.rs");
        #[cfg(not(windows))]
        let event_path = PathBuf::from("/Users/Developer/Repo/.loom/viewports/v1/src/MixedCase.rs");

        let rel = filter
            .relative_to_watched_root(&event_path)
            .expect("relative path should resolve");
        assert_eq!(rel, PathBuf::from("src").join("MixedCase.rs"));

        let skipped = relative_by_component_skip(
            &strip_windows_verbatim_prefix(&event_path),
            &strip_windows_verbatim_prefix(&root),
        );
        assert_eq!(skipped, PathBuf::from("src").join("MixedCase.rs"));
    }

    #[test]
    fn aggregate_provide_entries_preserves_pending_syntax_across_duplicate_fqns() {
        let pending_older = SymbolProvideEntry {
            symbol_fqn: "dup::sym".into(),
            viewport_id: "vp-1".into(),
            file_path: PathBuf::from("src/a.rs"),
            status: SymbolStatus::PendingSyntax,
            updated_at_ns: 10,
            ..SymbolProvideEntry::default()
        };
        let ready_newer = SymbolProvideEntry {
            symbol_fqn: "dup::sym".into(),
            viewport_id: "vp-1".into(),
            file_path: PathBuf::from("src/b.rs"),
            status: SymbolStatus::Ready,
            updated_at_ns: 20,
            ..SymbolProvideEntry::default()
        };

        let list_a = vec![pending_older];
        let list_b = vec![ready_newer];
        let agg = aggregate_provide_entries([&list_a, &list_b]);
        assert_eq!(agg["dup::sym"].status, SymbolStatus::PendingSyntax);

        let agg_rev = aggregate_provide_entries([&list_b, &list_a]);
        assert_eq!(agg_rev["dup::sym"].status, SymbolStatus::PendingSyntax);
    }

    #[test]
    fn evaluate_viewport_pair_detects_concurrent_divergent_renames() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cas = CasStore::new(tmp.path().join("cas"), 3).unwrap();
        let old_path = PathBuf::from("src/old.rs");
        let alpha_path = PathBuf::from("src/alpha.rs");
        let beta_path = PathBuf::from("src/beta.rs");
        let old_bytes = b"pub fn shared() -> i32 { 7 }\n";

        let mut trunk_files = BTreeMap::new();
        trunk_files.insert(old_path.clone(), Some(old_bytes.to_vec()));

        let mut vp_a = ViewportShadow::new(ViewportRecord {
            id: "vp-a".into(),
            name: "vp1".into(),
            agent_id: "a1".into(),
            lamport_ts: 1,
            ..ViewportRecord::default()
        });
        let del_a = diff_file_to_patch(
            &DiffContext {
                logical_file_id: compute_logical_file_id(&old_path),
                file_path: &old_path,
                language: LanguageKind::Rust,
                viewport_id: "vp-a",
                actor_id: "a1",
                lamport_ts: 1,
                parents: Vec::new(),
            },
            old_bytes,
            &[],
            None,
            Some(&cas),
        )
        .unwrap();
        let add_a = diff_file_to_patch(
            &DiffContext {
                logical_file_id: compute_logical_file_id(&alpha_path),
                file_path: &alpha_path,
                language: LanguageKind::Rust,
                viewport_id: "vp-a",
                actor_id: "a1",
                lamport_ts: 1,
                parents: Vec::new(),
            },
            &[],
            old_bytes,
            None,
            Some(&cas),
        )
        .unwrap();
        vp_a.file_patches.insert(old_path.clone(), del_a);
        vp_a.file_patches.insert(alpha_path.clone(), add_a);
        vp_a.files.insert(
            alpha_path.clone(),
            CachedFileState {
                content_hash: Blake3Hash::from_bytes(old_bytes),
                current_source: String::from_utf8_lossy(old_bytes).into_owned(),
                tree: None,
                last_valid_source: None,
                last_valid_extracted: None,
            },
        );

        let mut vp_b = ViewportShadow::new(ViewportRecord {
            id: "vp-b".into(),
            name: "vp2".into(),
            agent_id: "a2".into(),
            lamport_ts: 2,
            ..ViewportRecord::default()
        });
        let del_b = diff_file_to_patch(
            &DiffContext {
                logical_file_id: compute_logical_file_id(&old_path),
                file_path: &old_path,
                language: LanguageKind::Rust,
                viewport_id: "vp-b",
                actor_id: "a2",
                lamport_ts: 2,
                parents: Vec::new(),
            },
            old_bytes,
            &[],
            None,
            Some(&cas),
        )
        .unwrap();
        let add_b = diff_file_to_patch(
            &DiffContext {
                logical_file_id: compute_logical_file_id(&beta_path),
                file_path: &beta_path,
                language: LanguageKind::Rust,
                viewport_id: "vp-b",
                actor_id: "a2",
                lamport_ts: 2,
                parents: Vec::new(),
            },
            &[],
            old_bytes,
            None,
            Some(&cas),
        )
        .unwrap();
        vp_b.file_patches.insert(old_path, del_b);
        vp_b.file_patches.insert(beta_path.clone(), add_b);
        vp_b.files.insert(
            beta_path,
            CachedFileState {
                content_hash: Blake3Hash::from_bytes(old_bytes),
                current_source: String::from_utf8_lossy(old_bytes).into_owned(),
                tree: None,
                last_valid_source: None,
                last_valid_extracted: None,
            },
        );

        let eval = evaluate_viewport_pair(&vp_a, &vp_b, &trunk_files, &cas);
        assert_eq!(eval.level, RadarLevel::Red);
        assert!(!eval.conflicts_for_a.is_empty());
        assert!(!eval.conflicts_for_b.is_empty());
    }
}
