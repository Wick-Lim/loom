use crate::client::subscribe_radar;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use loom_core::model::RadarLevel;
use loom_core::rpc::{RadarMatrixCell, RadarSnapshot, RadarViewportSummary};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, List, ListItem, ListState, Paragraph, Row, Table, Tabs, Wrap};
use ratatui::{Frame, Terminal};
use std::io::{IsTerminal, Stdout};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RadarTab {
    Matrix,
    Conflicts,
    PendingSyntax,
}

impl RadarTab {
    fn index(self) -> usize {
        match self {
            Self::Matrix => 0,
            Self::Conflicts => 1,
            Self::PendingSyntax => 2,
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Matrix => Self::Conflicts,
            Self::Conflicts => Self::PendingSyntax,
            Self::PendingSyntax => Self::Matrix,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::Matrix => Self::PendingSyntax,
            Self::Conflicts => Self::Matrix,
            Self::PendingSyntax => Self::Conflicts,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiAction {
    None,
    Refresh,
    Quit,
}

#[derive(Debug, Clone)]
pub struct RadarTuiState {
    pub snapshot: RadarSnapshot,
    pub selected_viewport: usize,
    pub active_tab: RadarTab,
    pub status_banner: String,
    pub should_quit: bool,
}

impl RadarTuiState {
    pub fn new(snapshot: RadarSnapshot) -> Self {
        Self {
            snapshot,
            selected_viewport: 0,
            active_tab: RadarTab::Matrix,
            status_banner: "Connected".to_string(),
            should_quit: false,
        }
    }

    pub fn update_snapshot(&mut self, snapshot: RadarSnapshot) {
        self.snapshot = snapshot;
        if self.snapshot.viewports.is_empty() {
            self.selected_viewport = 0;
        } else if self.selected_viewport >= self.snapshot.viewports.len() {
            self.selected_viewport = self.snapshot.viewports.len() - 1;
        }
    }

    pub fn selected_summary(&self) -> Option<&RadarViewportSummary> {
        self.snapshot.viewports.get(self.selected_viewport)
    }

    pub fn handle_key_event(&mut self, key: KeyEvent) -> TuiAction {
        if key.kind != KeyEventKind::Press {
            return TuiAction::None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
        {
            self.should_quit = true;
            return TuiAction::Quit;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => {
                self.should_quit = true;
                TuiAction::Quit
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                self.status_banner = "Refreshing...".to_string();
                TuiAction::Refresh
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if !self.snapshot.viewports.is_empty() {
                    if self.selected_viewport == 0 {
                        self.selected_viewport = self.snapshot.viewports.len() - 1;
                    } else {
                        self.selected_viewport -= 1;
                    }
                }
                TuiAction::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.snapshot.viewports.is_empty() {
                    self.selected_viewport =
                        (self.selected_viewport + 1) % self.snapshot.viewports.len();
                }
                TuiAction::None
            }
            KeyCode::Tab | KeyCode::Right => {
                self.active_tab = self.active_tab.next();
                TuiAction::None
            }
            KeyCode::BackTab | KeyCode::Left => {
                self.active_tab = self.active_tab.prev();
                TuiAction::None
            }
            _ => TuiAction::None,
        }
    }
}

struct TerminalRestoreGuard {
    active: bool,
}

impl TerminalRestoreGuard {
    fn enter() -> anyhow::Result<Self> {
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
            prev_hook(info);
        }));

        enable_raw_mode()?;
        if let Err(err) = execute!(std::io::stdout(), EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(err.into());
        }
        Ok(Self { active: true })
    }
}

impl Drop for TerminalRestoreGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = disable_raw_mode();
            let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
            self.active = false;
        }
    }
}

fn level_badge(level: RadarLevel) -> (&'static str, Style) {
    match level {
        RadarLevel::Green => (
            "GREEN",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        RadarLevel::Yellow => (
            "YELLOW",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        RadarLevel::Red => (
            "RED",
            Style::default()
                .fg(Color::Red)
                .add_modifier(Modifier::BOLD),
        ),
    }
}

fn lookup_pair_cell<'a>(
    matrix: &'a [RadarMatrixCell],
    a_id: &str,
    b_id: &str,
) -> Option<&'a RadarMatrixCell> {
    matrix.iter().find(|c| {
        (c.viewport_a_id == a_id && c.viewport_b_id == b_id)
            || (c.viewport_a_id == b_id && c.viewport_b_id == a_id)
    })
}

pub fn render_radar_ui(frame: &mut Frame<'_>, state: &mut RadarTuiState) {
    let root_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(1),
        ])
        .split(frame.area());

    // Header bar
    let header_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
        .split(root_chunks[0]);

    let aggregate_level = state
        .snapshot
        .viewports
        .iter()
        .map(|v| v.level)
        .max()
        .unwrap_or(RadarLevel::Green);
    let (agg_label, agg_style) = level_badge(aggregate_level);

    let mode_span = if state.snapshot.batch_mode {
        Span::styled(
            format!("BATCH MODE ({}ms)", state.snapshot.debounce_ms),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            format!("INTERACTIVE ({}ms)", state.snapshot.debounce_ms),
            Style::default().fg(Color::Cyan),
        )
    };

    let repo_label = state
        .snapshot
        .repo_root
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| state.snapshot.repo_root.display().to_string());

    let header_line = Line::from(vec![
        Span::styled(
            format!(" LOOM RADAR [{repo_label}] "),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("| Status: "),
        Span::styled(agg_label, agg_style),
        Span::raw(" | Mode: "),
        mode_span,
        Span::raw(format!(" | Viewports: {}", state.snapshot.viewports.len())),
    ]);
    let header_block = Paragraph::new(header_line)
        .block(Block::default().borders(Borders::ALL).title("Speculative Merge Radar"));
    frame.render_widget(header_block, header_chunks[0]);

    let tab_titles = ["Matrix (NxN)", "Conflicts", "Pending Syntax"];
    let tabs = Tabs::new(tab_titles.iter().copied())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("q:Quit r:Refresh Tab:View"),
        )
        .select(state.active_tab.index())
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_widget(tabs, header_chunks[1]);

    // Body: Left viewports list + Right details
    let body_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(root_chunks[1]);

    let vp_items: Vec<ListItem<'_>> = if state.snapshot.viewports.is_empty() {
        vec![ListItem::new("No active viewports")]
    } else {
        state
            .snapshot
            .viewports
            .iter()
            .map(|vp| {
                let (badge, style) = level_badge(vp.level);
                let line = Line::from(vec![
                    Span::styled(format!("[{badge}] "), style),
                    Span::styled(
                        vp.viewport_name.clone(),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!(
                        " ({}) d:{} p:{}",
                        vp.state,
                        vp.dirty_files.len(),
                        vp.pending_syntax_files.len()
                    )),
                ]);
                ListItem::new(line)
            })
            .collect()
    };

    let mut list_state = ListState::default();
    if !state.snapshot.viewports.is_empty() {
        list_state.select(Some(state.selected_viewport));
    }
    let vp_list = List::new(vp_items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Active Viewports"),
        )
        .highlight_style(Style::default().bg(Color::DarkGray))
        .highlight_symbol("> ");
    frame.render_stateful_widget(vp_list, body_chunks[0], &mut list_state);

    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(body_chunks[1]);

    // Top-right pane by active tab
    match state.active_tab {
        RadarTab::Matrix => {
            if state.snapshot.viewports.is_empty() {
                let empty = Paragraph::new("No active viewports to build N×N matrix.")
                    .block(Block::default().borders(Borders::ALL).title("N×N Matrix"));
                frame.render_widget(empty, right_chunks[0]);
            } else {
                let mut header_cells = vec![Cell::from("VP")];
                for vp in &state.snapshot.viewports {
                    header_cells.push(Cell::from(vp.viewport_name.clone()));
                }
                let header = Row::new(header_cells).style(Style::default().add_modifier(Modifier::BOLD));

                let rows: Vec<Row<'_>> = state
                    .snapshot
                    .viewports
                    .iter()
                    .enumerate()
                    .map(|(r_idx, va)| {
                        let mut cells = vec![Cell::from(va.viewport_name.clone())];
                        for (c_idx, vb) in state.snapshot.viewports.iter().enumerate() {
                            if r_idx == c_idx {
                                cells.push(Cell::from("SELF"));
                            } else if let Some(cell) =
                                lookup_pair_cell(&state.snapshot.matrix, &va.viewport_id, &vb.viewport_id)
                            {
                                let (lbl, st) = level_badge(cell.level);
                                cells.push(Cell::from(lbl).style(st));
                            } else {
                                let (lbl, st) = level_badge(RadarLevel::Green);
                                cells.push(Cell::from(lbl).style(st));
                            }
                        }
                        Row::new(cells)
                    })
                    .collect();

                let col_count = state.snapshot.viewports.len() + 1;
                let widths: Vec<Constraint> = (0..col_count)
                    .map(|i| {
                        if i == 0 {
                            Constraint::Length(14)
                        } else {
                            Constraint::Length(10)
                        }
                    })
                    .collect();

                let table = Table::new(rows, widths)
                    .header(header)
                    .block(Block::default().borders(Borders::ALL).title("N×N Conflict Matrix"));
                frame.render_widget(table, right_chunks[0]);
            }
        }
        RadarTab::Conflicts => {
            let mut lines = Vec::new();
            for vp in &state.snapshot.viewports {
                for c in &vp.conflicts {
                    let (lbl, st) = level_badge(c.level);
                    lines.push(Line::from(vec![
                        Span::styled(format!("[{lbl}] "), st),
                        Span::raw(format!(
                            "{} <-> {} ({}): {}",
                            vp.viewport_name, c.peer_viewport_name, c.kind, c.message
                        )),
                    ]));
                }
            }
            if lines.is_empty() {
                lines.push(Line::from("All active viewports are GREEN (no conflicts)."));
            }
            let p = Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(Block::default().borders(Borders::ALL).title("All Active Conflicts"));
            frame.render_widget(p, right_chunks[0]);
        }
        RadarTab::PendingSyntax => {
            let mut lines = Vec::new();
            for vp in &state.snapshot.viewports {
                for f in &vp.pending_syntax_files {
                    lines.push(Line::from(vec![
                        Span::styled(
                            "PENDING_SYNTAX ",
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(format!("{}: {}", vp.viewport_name, f.display())),
                    ]));
                }
            }
            if lines.is_empty() {
                lines.push(Line::from("No PENDING_SYNTAX files."));
            }
            let p = Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(Block::default().borders(Borders::ALL).title("PENDING_SYNTAX Files"));
            frame.render_widget(p, right_chunks[0]);
        }
    }

    // Bottom-right pane: selected viewport details
    let detail_lines: Vec<Line<'_>> = if let Some(sel) = state.selected_summary() {
        let (lbl, st) = level_badge(sel.level);
        let mut lines = vec![
            Line::from(vec![
                Span::raw("Viewport: "),
                Span::styled(
                    sel.viewport_name.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(" | Level: "),
                Span::styled(lbl, st),
                Span::raw(format!(" | Intent: {}", sel.intent)),
            ]),
            Line::from(format!(
                "Dirty files: {} | PENDING_SYNTAX: {} | Negative warnings: {}",
                sel.dirty_files.len(),
                sel.pending_syntax_files.len(),
                sel.negative_warnings.len()
            )),
        ];
        if !sel.pending_syntax_files.is_empty() {
            let list: Vec<String> = sel
                .pending_syntax_files
                .iter()
                .map(|p| p.display().to_string())
                .collect();
            lines.push(Line::from(Span::styled(
                format!("PENDING_SYNTAX: {}", list.join(", ")),
                Style::default().fg(Color::Yellow),
            )));
        }
        for c in &sel.conflicts {
            let (clbl, cst) = level_badge(c.level);
            let sym = c.symbol_fqn.as_deref().unwrap_or("-");
            let file = c
                .file_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "-".to_string());
            lines.push(Line::from(vec![
                Span::styled(format!("  [{clbl}] "), cst),
                Span::raw(format!(
                    "peer={} kind={} file={} sym={} msg={}",
                    c.peer_viewport_name, c.kind, file, sym, c.message
                )),
            ]));
        }
        for nw in &sel.negative_warnings {
            lines.push(Line::from(Span::styled(
                format!("  [DEADEND {:?}] {}", nw.failure_kind, nw.lesson_summary),
                Style::default().fg(Color::Yellow),
            )));
        }
        lines
    } else {
        vec![Line::from("Select a viewport to inspect conflict and contract details.")]
    };

    let detail_widget = Paragraph::new(detail_lines)
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Conflict & Contract Details"),
        );
    frame.render_widget(detail_widget, right_chunks[1]);

    // Footer bar
    let footer = Paragraph::new(format!(
        "seq={} | updated_ms={} | switches={} | coalesced={} | {}",
        state.snapshot.sequence,
        state.snapshot.updated_at_ms,
        state.snapshot.metrics.batch_switches,
        state.snapshot.metrics.coalesced_events,
        state.status_banner
    ));
    frame.render_widget(footer, root_chunks[2]);
}

pub async fn run_radar_tui(repo_root: &Path, viewport: Option<String>) -> anyhow::Result<()> {
    if !std::io::stdout().is_terminal() {
        anyhow::bail!(
            "interactive `loom radar --watch` TUI requires a TTY; use `loom radar --watch --json` or one-shot `loom radar`"
        );
    }

    let (initial, mut sub) = subscribe_radar(repo_root, viewport.clone()).await?;
    let _guard = TerminalRestoreGuard::enter()?;
    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal: Terminal<CrosstermBackend<Stdout>> = Terminal::new(backend)?;
    terminal.clear()?;

    let mut state = RadarTuiState::new(initial);

    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_clone = Arc::clone(&stop_flag);
    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Event>();
    let input_thread = std::thread::spawn(move || {
        while !stop_clone.load(Ordering::Relaxed) {
            if let Ok(true) = event::poll(Duration::from_millis(50))
                && let Ok(ev) = event::read()
                && input_tx.send(ev).is_err()
            {
                break;
            }
        }
    });

    let mut stream_closed = false;

    loop {
        terminal.draw(|frame| render_radar_ui(frame, &mut state))?;
        if state.should_quit {
            break;
        }

        tokio::select! {
            ev_opt = input_rx.recv() => {
                match ev_opt {
                    Some(Event::Key(k)) => {
                        match state.handle_key_event(k) {
                            TuiAction::Quit => break,
                            TuiAction::Refresh => {
                                if let Err(err) = sub.request_refresh(viewport.clone()).await {
                                    state.status_banner = format!("Refresh failed: {err}");
                                }
                            }
                            TuiAction::None => {}
                        }
                    }
                    Some(Event::Resize(_, _)) => {}
                    Some(_) => {}
                    None => break,
                }
            }
            sub_res = sub.next_event(), if !stream_closed => {
                match sub_res {
                    Ok(Some(snap)) => {
                        state.update_snapshot(snap);
                        state.status_banner = "Live".to_string();
                    }
                    Ok(None) => {
                        stream_closed = true;
                        state.status_banner = "Daemon disconnected (press q to quit)".to_string();
                    }
                    Err(err) => {
                        stream_closed = true;
                        state.status_banner = format!("Stream error: {err}");
                    }
                }
            }
        }
    }

    stop_flag.store(true, Ordering::Relaxed);
    let _ = input_thread.join();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_core::model::{RadarConflictItem, ViewportState};
    use loom_core::rpc::RadarBatchMetrics;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    fn sample_snapshot() -> RadarSnapshot {
        RadarSnapshot {
            sequence: 7,
            repo_root: PathBuf::from("demo-repo"),
            batch_mode: true,
            debounce_ms: 200,
            metrics: RadarBatchMetrics {
                batch_mode: true,
                effective_debounce_ms: 200,
                batch_switches: 2,
                coalesced_events: 25,
            },
            viewports: vec![
                RadarViewportSummary {
                    viewport_id: "vp-1".into(),
                    viewport_name: "alpha".into(),
                    intent: "edit lib".into(),
                    state: ViewportState::Active,
                    level: RadarLevel::Red,
                    pending_syntax: true,
                    pending_syntax_files: vec![PathBuf::from("src/wip.rs")],
                    conflicts: vec![RadarConflictItem {
                        peer_viewport_id: "vp-2".into(),
                        peer_viewport_name: "beta".into(),
                        level: RadarLevel::Red,
                        kind: "structural_conflict".into(),
                        file_path: Some(PathBuf::from("src/lib.rs")),
                        symbol_fqn: Some("crate::foo".into()),
                        message: "concurrent body edit".into(),
                    }],
                    negative_warnings: vec![],
                    dirty_files: vec![PathBuf::from("src/lib.rs")],
                    updated_at_ms: 1000,
                },
                RadarViewportSummary {
                    viewport_id: "vp-2".into(),
                    viewport_name: "beta".into(),
                    intent: "call foo".into(),
                    state: ViewportState::Active,
                    level: RadarLevel::Green,
                    pending_syntax: false,
                    pending_syntax_files: vec![],
                    conflicts: vec![],
                    negative_warnings: vec![],
                    dirty_files: vec![],
                    updated_at_ms: 1000,
                },
            ],
            matrix: vec![RadarMatrixCell {
                viewport_a_id: "vp-1".into(),
                viewport_a_name: "alpha".into(),
                viewport_b_id: "vp-2".into(),
                viewport_b_name: "beta".into(),
                level: RadarLevel::Red,
                conflicts: vec![],
            }],
            updated_at_ms: 1000,
        }
    }

    fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
        let mut s = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                s.push_str(buf[(x, y)].symbol());
            }
            s.push('\n');
        }
        s
    }

    #[test]
    fn tui_renders_matrix_badges_batch_mode_and_pending_syntax_on_test_backend() {
        let backend = TestBackend::new(120, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = RadarTuiState::new(sample_snapshot());

        terminal.draw(|f| render_radar_ui(f, &mut state)).unwrap();
        let rendered = buffer_text(terminal.backend().buffer());

        assert!(rendered.contains("LOOM RADAR"), "missing header title");
        assert!(rendered.contains("BATCH MODE"), "missing BATCH MODE badge");
        assert!(rendered.contains("alpha"), "missing viewport alpha");
        assert!(rendered.contains("beta"), "missing viewport beta");
        assert!(rendered.contains("RED"), "missing RED badge");
        assert!(rendered.contains("SELF"), "missing matrix SELF cell");
        assert!(rendered.contains("src/wip.rs"), "missing PENDING_SYNTAX file");
        assert!(
            rendered.contains("concurrent body edit"),
            "missing conflict detail message"
        );
    }

    #[test]
    fn tui_key_handling_navigation_tab_refresh_and_quit() {
        let mut state = RadarTuiState::new(sample_snapshot());

        // Release events must be ignored
        let rel = KeyEvent::new_with_kind(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        assert_eq!(state.handle_key_event(rel), TuiAction::None);
        assert!(!state.should_quit);

        // Down / Up wrap around safely
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(state.handle_key_event(down), TuiAction::None);
        assert_eq!(state.selected_viewport, 1);
        assert_eq!(state.handle_key_event(down), TuiAction::None);
        assert_eq!(state.selected_viewport, 0);

        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(state.handle_key_event(up), TuiAction::None);
        assert_eq!(state.selected_viewport, 1);

        // Tab cycles through tabs
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        state.handle_key_event(tab);
        assert_eq!(state.active_tab, RadarTab::Conflicts);
        state.handle_key_event(tab);
        assert_eq!(state.active_tab, RadarTab::PendingSyntax);
        state.handle_key_event(tab);
        assert_eq!(state.active_tab, RadarTab::Matrix);

        // Refresh
        let refresh = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE);
        assert_eq!(state.handle_key_event(refresh), TuiAction::Refresh);

        // Quit
        let quit = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert_eq!(state.handle_key_event(quit), TuiAction::Quit);
        assert!(state.should_quit);
    }
}
