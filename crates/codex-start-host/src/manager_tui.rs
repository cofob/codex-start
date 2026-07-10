//! Full-screen managers for persistent sessions and managed worktrees.

use std::{
    io::{self, IsTerminal},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use ratatui::{
    Frame,
    crossterm::event::{self, Event, KeyCode, KeyEventKind},
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState,
        Wrap,
    },
};
use uuid::Uuid;

use crate::{
    cli::OutputFormat,
    configuration::ConfigContext,
    error::{HostError, Result},
    git::{GitRepo, ManagedWorktree},
    session::{SessionKind, SessionRecord, SessionStatus, SessionStore},
};

const SESSION_REFRESH: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub enum SessionManagerAction {
    Quit,
    Attach(Uuid),
    Logs(Uuid, bool),
    Refresh(Uuid),
    Stop(Uuid),
    Restart(Uuid),
    Remove(Uuid),
}

#[derive(Clone, Debug)]
pub enum WorktreeManagerAction {
    Quit,
    Commit(String),
    Squash(String),
    Move(String),
    Edit(String),
    Cleanup,
}

#[derive(Clone, Debug)]
struct MenuItem<A> {
    label: String,
    action: Option<A>,
    confirm: Option<String>,
}

#[derive(Clone, Debug)]
enum InputMode<A> {
    Browse,
    Filter,
    Actions {
        selected: usize,
        items: Vec<MenuItem<A>>,
    },
    Confirm {
        prompt: String,
        action: A,
    },
    Help,
}

pub fn run_sessions(
    context: &ConfigContext,
    output: OutputFormat,
    notice: Option<String>,
) -> Result<SessionManagerAction> {
    validate_terminal(output, "session list")?;
    let store = SessionStore::for_context(context)?;
    let project_id = context.repo.as_ref().map_or_else(
        || codex_start_core::canonical_path_hash(context.project_root()),
        |repo| repo.project_id.clone(),
    );
    let mut state = SessionState::new(store.list()?, project_id, notice);
    let mut outcome = SessionManagerAction::Quit;
    ratatui::run(|terminal| session_loop(terminal, &store, &mut state, &mut outcome))
        .map_err(|source| HostError::io("session manager terminal", source))?;
    Ok(outcome)
}

pub fn run_worktrees(
    context: &ConfigContext,
    output: OutputFormat,
    notice: Option<String>,
) -> Result<WorktreeManagerAction> {
    validate_terminal(output, "worktree list")?;
    let repo = GitRepo::require(&context.cwd)?;
    let resolved = context.resolve(None)?;
    let base = resolved
        .config
        .git
        .worktree_base
        .clone()
        .unwrap_or_else(|| context.paths.worktrees_dir());
    let prefix = resolved.config.git.branch_prefix;
    let mut state = WorktreeState::new(repo.list_workspaces(&base, &prefix)?, notice);
    let mut outcome = WorktreeManagerAction::Quit;
    ratatui::run(|terminal| {
        worktree_loop(terminal, &repo, &base, &prefix, &mut state, &mut outcome)
    })
    .map_err(|source| HostError::io("worktree manager terminal", source))?;
    Ok(outcome)
}

fn validate_terminal(output: OutputFormat, list_command: &str) -> Result<()> {
    validate_terminal_with(
        output,
        io::stdin().is_terminal(),
        io::stdout().is_terminal(),
        list_command,
    )
}

fn validate_terminal_with(
    output: OutputFormat,
    input_terminal: bool,
    output_terminal: bool,
    list_command: &str,
) -> Result<()> {
    if output != OutputFormat::Human {
        return Err(HostError::Usage(format!(
            "interactive managers require human output; use `{list_command}` with --output json"
        )));
    }
    if !input_terminal || !output_terminal {
        return Err(HostError::Usage(format!(
            "interactive managers require a terminal; use `{list_command}` in non-interactive environments"
        )));
    }
    Ok(())
}

struct SessionState {
    records: Vec<SessionRecord>,
    project_id: String,
    show_all: bool,
    selected: usize,
    filter: String,
    mode: InputMode<SessionManagerAction>,
    notice: Option<String>,
    last_refresh: Instant,
}

impl SessionState {
    fn new(records: Vec<SessionRecord>, project_id: String, notice: Option<String>) -> Self {
        Self {
            records,
            project_id,
            show_all: false,
            selected: 0,
            filter: String::new(),
            mode: InputMode::Browse,
            notice,
            last_refresh: Instant::now(),
        }
    }

    fn visible(&self) -> Vec<&SessionRecord> {
        let needle = self.filter.to_ascii_lowercase();
        self.records
            .iter()
            .filter(|record| self.show_all || record.project_id == self.project_id)
            .filter(|record| {
                needle.is_empty()
                    || format!(
                        "{} {} {} {:?} {:?} {}",
                        record.id,
                        record.alias,
                        record.project_id,
                        record.kind,
                        record.status,
                        record.environment
                    )
                    .to_ascii_lowercase()
                    .contains(&needle)
            })
            .collect()
    }

    fn selected_record(&self) -> Option<&SessionRecord> {
        self.visible().get(self.selected).copied()
    }

    fn clamp(&mut self) {
        self.selected = self.selected.min(self.visible().len().saturating_sub(1));
    }

    fn reload(&mut self, store: &SessionStore) {
        let selected_id = self.selected_record().map(|record| record.id);
        match store.list() {
            Ok(records) => {
                self.records = records;
                self.selected = selected_id
                    .and_then(|id| self.visible().iter().position(|record| record.id == id))
                    .unwrap_or_else(|| self.selected.min(self.visible().len().saturating_sub(1)));
                self.last_refresh = Instant::now();
            }
            Err(error) => {
                self.notice = Some(format!("Refresh failed: {error}"));
                self.last_refresh = Instant::now();
            }
        }
    }
}

struct WorktreeState {
    worktrees: Vec<ManagedWorktree>,
    selected: usize,
    filter: String,
    mode: InputMode<WorktreeManagerAction>,
    notice: Option<String>,
}

impl WorktreeState {
    fn new(worktrees: Vec<ManagedWorktree>, notice: Option<String>) -> Self {
        Self {
            worktrees,
            selected: 0,
            filter: String::new(),
            mode: InputMode::Browse,
            notice,
        }
    }

    fn visible(&self) -> Vec<&ManagedWorktree> {
        let needle = self.filter.to_ascii_lowercase();
        self.worktrees
            .iter()
            .filter(|worktree| {
                needle.is_empty()
                    || format!(
                        "{} {} {}",
                        worktree.name,
                        worktree.branch,
                        worktree.path.display()
                    )
                    .to_ascii_lowercase()
                    .contains(&needle)
            })
            .collect()
    }

    fn selected_worktree(&self) -> Option<&ManagedWorktree> {
        self.visible().get(self.selected).copied()
    }

    fn clamp(&mut self) {
        self.selected = self.selected.min(self.visible().len().saturating_sub(1));
    }

    fn reload(&mut self, repo: &GitRepo, base: &std::path::Path, prefix: &str) {
        let selected_name = self
            .selected_worktree()
            .map(|worktree| worktree.name.clone());
        match repo.list_workspaces(base, prefix) {
            Ok(worktrees) => {
                self.worktrees = worktrees;
                self.selected = selected_name
                    .and_then(|name| {
                        self.visible()
                            .iter()
                            .position(|worktree| worktree.name == name)
                    })
                    .unwrap_or_else(|| self.selected.min(self.visible().len().saturating_sub(1)));
            }
            Err(error) => self.notice = Some(format!("Refresh failed: {error}")),
        }
    }
}

fn session_loop(
    terminal: &mut ratatui::DefaultTerminal,
    store: &SessionStore,
    state: &mut SessionState,
    outcome: &mut SessionManagerAction,
) -> io::Result<()> {
    loop {
        if state.last_refresh.elapsed() >= SESSION_REFRESH {
            state.reload(store);
        }
        terminal.draw(|frame| render_sessions(frame, state))?;
        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if handle_session_key(state, key.code, outcome) {
            return Ok(());
        }
    }
}

fn worktree_loop(
    terminal: &mut ratatui::DefaultTerminal,
    repo: &GitRepo,
    base: &std::path::Path,
    prefix: &str,
    state: &mut WorktreeState,
    outcome: &mut WorktreeManagerAction,
) -> io::Result<()> {
    loop {
        terminal.draw(|frame| render_worktrees(frame, state))?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if matches!(state.mode, InputMode::Browse) && key.code == KeyCode::Char('r') {
            state.reload(repo, base, prefix);
            continue;
        }
        if handle_worktree_key(state, key.code, outcome) {
            return Ok(());
        }
    }
}

fn handle_session_key(
    state: &mut SessionState,
    code: KeyCode,
    outcome: &mut SessionManagerAction,
) -> bool {
    let visible_len = state.visible().len();
    match &mut state.mode {
        InputMode::Browse => match code {
            KeyCode::Char('q') | KeyCode::Esc => return true,
            KeyCode::Down | KeyCode::Char('j') => {
                move_selection(&mut state.selected, visible_len, 1);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                move_selection(&mut state.selected, visible_len, -1);
            }
            KeyCode::PageDown => move_selection(&mut state.selected, visible_len, 10),
            KeyCode::PageUp => move_selection(&mut state.selected, visible_len, -10),
            KeyCode::Home => state.selected = 0,
            KeyCode::End => state.selected = visible_len.saturating_sub(1),
            KeyCode::Char('/') => state.mode = InputMode::Filter,
            KeyCode::Char('?') => state.mode = InputMode::Help,
            KeyCode::Char('a') => {
                state.show_all = !state.show_all;
                state.selected = 0;
            }
            KeyCode::Char('r') => {
                state.last_refresh = Instant::now()
                    .checked_sub(SESSION_REFRESH)
                    .unwrap_or_else(Instant::now);
            }
            KeyCode::Enter => {
                if let Some(record) = state.selected_record() {
                    state.mode = InputMode::Actions {
                        selected: 0,
                        items: session_actions(record),
                    };
                }
            }
            _ => {}
        },
        InputMode::Filter => match code {
            KeyCode::Esc | KeyCode::Enter => state.mode = InputMode::Browse,
            KeyCode::Backspace => {
                state.filter.pop();
                state.selected = 0;
            }
            KeyCode::Char(character) => {
                state.filter.push(character);
                state.selected = 0;
            }
            _ => {}
        },
        InputMode::Actions { selected, items } => match code {
            KeyCode::Esc | KeyCode::Char('q') => state.mode = InputMode::Browse,
            KeyCode::Down | KeyCode::Char('j') => move_selection(selected, items.len(), 1),
            KeyCode::Up | KeyCode::Char('k') => move_selection(selected, items.len(), -1),
            KeyCode::Enter => {
                if let Some(item) = items.get(*selected).cloned() {
                    if let Some(action) = item.action {
                        if let Some(prompt) = item.confirm {
                            state.mode = InputMode::Confirm { prompt, action };
                        } else {
                            *outcome = action;
                            return true;
                        }
                    }
                }
            }
            _ => {}
        },
        InputMode::Confirm { action, .. } => match code {
            KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
                *outcome = action.clone();
                return true;
            }
            KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                state.mode = InputMode::Browse;
            }
            _ => {}
        },
        InputMode::Help => {
            if matches!(
                code,
                KeyCode::Esc | KeyCode::Char('q' | '?') | KeyCode::Enter
            ) {
                state.mode = InputMode::Browse;
            }
        }
    }
    state.clamp();
    false
}

fn handle_worktree_key(
    state: &mut WorktreeState,
    code: KeyCode,
    outcome: &mut WorktreeManagerAction,
) -> bool {
    let visible_len = state.visible().len();
    match &mut state.mode {
        InputMode::Browse => match code {
            KeyCode::Char('q') | KeyCode::Esc => return true,
            KeyCode::Down | KeyCode::Char('j') => {
                move_selection(&mut state.selected, visible_len, 1);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                move_selection(&mut state.selected, visible_len, -1);
            }
            KeyCode::PageDown => move_selection(&mut state.selected, visible_len, 10),
            KeyCode::PageUp => move_selection(&mut state.selected, visible_len, -10),
            KeyCode::Home => state.selected = 0,
            KeyCode::End => state.selected = visible_len.saturating_sub(1),
            KeyCode::Char('/') => state.mode = InputMode::Filter,
            KeyCode::Char('?') => state.mode = InputMode::Help,
            KeyCode::Char('c') => {
                state.mode = InputMode::Confirm {
                    prompt: "Remove all clean managed worktrees and merged owned branches?"
                        .to_owned(),
                    action: WorktreeManagerAction::Cleanup,
                };
            }
            KeyCode::Enter => {
                if let Some(worktree) = state.selected_worktree() {
                    state.mode = InputMode::Actions {
                        selected: 0,
                        items: worktree_actions(worktree),
                    };
                }
            }
            _ => {}
        },
        InputMode::Filter => match code {
            KeyCode::Esc | KeyCode::Enter => state.mode = InputMode::Browse,
            KeyCode::Backspace => {
                state.filter.pop();
                state.selected = 0;
            }
            KeyCode::Char(character) => {
                state.filter.push(character);
                state.selected = 0;
            }
            _ => {}
        },
        InputMode::Actions { selected, items } => match code {
            KeyCode::Esc | KeyCode::Char('q') => state.mode = InputMode::Browse,
            KeyCode::Down | KeyCode::Char('j') => move_selection(selected, items.len(), 1),
            KeyCode::Up | KeyCode::Char('k') => move_selection(selected, items.len(), -1),
            KeyCode::Enter => {
                if let Some(item) = items.get(*selected).cloned() {
                    if let Some(action) = item.action {
                        if let Some(prompt) = item.confirm {
                            state.mode = InputMode::Confirm { prompt, action };
                        } else {
                            *outcome = action;
                            return true;
                        }
                    }
                }
            }
            _ => {}
        },
        InputMode::Confirm { action, .. } => match code {
            KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
                *outcome = action.clone();
                return true;
            }
            KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                state.mode = InputMode::Browse;
            }
            _ => {}
        },
        InputMode::Help => {
            if matches!(
                code,
                KeyCode::Esc | KeyCode::Char('q' | '?') | KeyCode::Enter
            ) {
                state.mode = InputMode::Browse;
            }
        }
    }
    state.clamp();
    false
}

fn move_selection(selected: &mut usize, len: usize, amount: isize) {
    if len == 0 {
        *selected = 0;
        return;
    }
    *selected = selected.saturating_add_signed(amount).min(len - 1);
}

fn session_actions(record: &SessionRecord) -> Vec<MenuItem<SessionManagerAction>> {
    let live = record.status.is_live();
    let mut actions = vec![
        if live {
            menu("Attach", SessionManagerAction::Attach(record.id))
        } else {
            disabled("Attach", "session is not live")
        },
        menu("Show logs", SessionManagerAction::Logs(record.id, false)),
        menu("Follow logs", SessionManagerAction::Logs(record.id, true)),
    ];
    if live {
        actions.push(menu(
            "Refresh host integrations",
            SessionManagerAction::Refresh(record.id),
        ));
        actions.push(confirm_menu(
            "Stop",
            SessionManagerAction::Stop(record.id),
            format!("Stop session {:?}?", record.alias),
        ));
    } else {
        actions.push(disabled("Refresh host integrations", "session is not live"));
        actions.push(disabled("Stop", "session is not live"));
    }
    if !live && record.kind == SessionKind::Interactive {
        actions.push(menu("Restart", SessionManagerAction::Restart(record.id)));
    } else {
        actions.push(disabled(
            "Restart",
            if live {
                "session is already live"
            } else {
                "jobs cannot be replayed"
            },
        ));
    }
    if live {
        actions.push(disabled("Remove", "stop the session first"));
    } else {
        actions.push(confirm_menu(
            "Remove",
            SessionManagerAction::Remove(record.id),
            format!(
                "Remove stopped session {:?} and its runtime state?",
                record.alias
            ),
        ));
    }
    actions
}

fn worktree_actions(worktree: &ManagedWorktree) -> Vec<MenuItem<WorktreeManagerAction>> {
    vec![
        menu(
            "Commit",
            WorktreeManagerAction::Commit(worktree.name.clone()),
        ),
        if worktree.current {
            disabled(
                "Squash into current worktree",
                "this is the current worktree",
            )
        } else {
            confirm_menu(
                "Squash into current worktree",
                WorktreeManagerAction::Squash(worktree.name.clone()),
                format!(
                    "Autosave and squash {:?} into the current worktree?",
                    worktree.name
                ),
            )
        },
        if worktree.current {
            disabled(
                "Move changes into current worktree",
                "this is the current worktree",
            )
        } else {
            confirm_menu(
                "Move changes into current worktree",
                WorktreeManagerAction::Move(worktree.name.clone()),
                format!(
                    "Apply changes from {:?} into the current worktree?",
                    worktree.name
                ),
            )
        },
        menu(
            "Open in editor",
            WorktreeManagerAction::Edit(worktree.name.clone()),
        ),
        confirm_menu(
            "Clean up merged worktrees",
            WorktreeManagerAction::Cleanup,
            "Remove all clean managed worktrees and merged owned branches?".to_owned(),
        ),
    ]
}

fn menu<A>(label: &str, action: A) -> MenuItem<A> {
    MenuItem {
        label: label.to_owned(),
        action: Some(action),
        confirm: None,
    }
}

fn confirm_menu<A>(label: &str, action: A, prompt: String) -> MenuItem<A> {
    MenuItem {
        label: label.to_owned(),
        action: Some(action),
        confirm: Some(prompt),
    }
}

fn disabled<A>(label: &str, reason: &str) -> MenuItem<A> {
    MenuItem {
        label: format!("{label} — unavailable: {reason}"),
        action: None,
        confirm: None,
    }
}

fn render_sessions(frame: &mut Frame<'_>, state: &SessionState) {
    let scope = if state.show_all {
        "all projects"
    } else {
        "current project"
    };
    let title = format!(" Sessions — {scope} ");
    let footer = if matches!(state.mode, InputMode::Filter) {
        format!(" Filter: {}_ ", state.filter)
    } else {
        " ↑/↓ navigate  Enter actions  / filter  a scope  r refresh  ? help  q quit ".to_owned()
    };
    let areas = page_layout(frame.area(), state.notice.is_some());
    let visible = state.visible();
    let rows = visible.iter().map(|record| {
        Row::new([
            Cell::from(record.alias.clone()),
            Cell::from(status_name(record.status)),
            Cell::from(kind_name(record.kind)),
            Cell::from(record.environment.clone()),
            Cell::from(relative_age(record.updated_unix_seconds)),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Percentage(26),
            Constraint::Length(18),
            Constraint::Length(11),
            Constraint::Percentage(24),
            Constraint::Length(10),
        ],
    )
    .header(
        Row::new(["Alias", "Status", "Kind", "Environment", "Updated"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(Block::default().borders(Borders::ALL).title(title))
    .row_highlight_style(
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("› ");
    let mut table_state =
        TableState::default().with_selected((!visible.is_empty()).then_some(state.selected));
    let (list_area, detail_area) = content_areas(areas.content);
    frame.render_stateful_widget(table, list_area, &mut table_state);
    render_session_detail(frame, detail_area, state.selected_record());
    render_footer_and_notice(frame, areas, &footer, state.notice.as_deref());
    render_overlay(frame, &state.mode, true);
}

fn render_worktrees(frame: &mut Frame<'_>, state: &WorktreeState) {
    let footer = if matches!(state.mode, InputMode::Filter) {
        format!(" Filter: {}_ ", state.filter)
    } else {
        " ↑/↓ navigate  Enter actions  / filter  r refresh  c cleanup  ? help  q quit ".to_owned()
    };
    let areas = page_layout(frame.area(), state.notice.is_some());
    let visible = state.visible();
    let rows = visible.iter().map(|worktree| {
        Row::new([
            Cell::from(worktree.name.clone()),
            Cell::from(if worktree.current {
                "current"
            } else if worktree.dirty {
                "dirty"
            } else {
                "clean"
            }),
            Cell::from(worktree.branch.clone()),
            Cell::from(relative_age(worktree.modified_unix_seconds)),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Percentage(28),
            Constraint::Length(10),
            Constraint::Percentage(44),
            Constraint::Length(10),
        ],
    )
    .header(
        Row::new(["Name", "State", "Branch", "Updated"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Managed worktrees "),
    )
    .row_highlight_style(
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("› ");
    let mut table_state =
        TableState::default().with_selected((!visible.is_empty()).then_some(state.selected));
    let (list_area, detail_area) = content_areas(areas.content);
    frame.render_stateful_widget(table, list_area, &mut table_state);
    render_worktree_detail(frame, detail_area, state.selected_worktree());
    render_footer_and_notice(frame, areas, &footer, state.notice.as_deref());
    render_overlay(frame, &state.mode, false);
}

#[derive(Clone, Copy)]
struct PageAreas {
    content: Rect,
    notice: Option<Rect>,
    footer: Rect,
}

fn page_layout(area: Rect, has_notice: bool) -> PageAreas {
    let notice_height = u16::from(has_notice);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),
            Constraint::Length(notice_height),
            Constraint::Length(1),
        ])
        .split(area);
    PageAreas {
        content: chunks[0],
        notice: has_notice.then_some(chunks[1]),
        footer: chunks[2],
    }
}

fn content_areas(area: Rect) -> (Rect, Rect) {
    if area.width >= 100 {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
            .split(area);
        (chunks[0], chunks[1])
    } else if area.height >= 16 {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
            .split(area);
        (chunks[0], chunks[1])
    } else {
        (area, Rect::default())
    }
}

fn render_session_detail(frame: &mut Frame<'_>, area: Rect, record: Option<&SessionRecord>) {
    if area.is_empty() {
        return;
    }
    let text = record.map_or_else(
        || "No sessions. Start one with `codex-start session start`.".to_owned(),
        |record| {
            format!(
                "UUID: {}\nProject: {}\nRuntime: {:?}\nContainer: {}\nHome: {}\nHost cwd: {}\nContainer cwd: {}\nThread: {}\nCreated: {} ago",
                record.id,
                record.project_id,
                record.runtime,
                record.container_name,
                record.home,
                record.cwd.as_os_str().to_string_lossy(),
                record.container_workdir.as_os_str().to_string_lossy(),
                record.codex_thread_id.map_or_else(|| "—".to_owned(), |id| id.to_string()),
                relative_age(record.created_unix_seconds),
            )
        },
    );
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(" Details ")),
        area,
    );
}

fn render_worktree_detail(frame: &mut Frame<'_>, area: Rect, worktree: Option<&ManagedWorktree>) {
    if area.is_empty() {
        return;
    }
    let text = worktree.map_or_else(
        || "No managed worktrees. A named run creates one automatically.".to_owned(),
        |worktree| {
            format!(
                "Name: {}\nBranch: {}\nHEAD: {}\nState: {}\nCurrent: {}\nPath: {}",
                worktree.name,
                worktree.branch,
                worktree.head,
                if worktree.dirty { "dirty" } else { "clean" },
                worktree.current,
                worktree.path.display(),
            )
        },
    );
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(" Details ")),
        area,
    );
}

fn render_footer_and_notice(
    frame: &mut Frame<'_>,
    areas: PageAreas,
    footer: &str,
    notice: Option<&str>,
) {
    if let (Some(area), Some(notice)) = (areas.notice, notice) {
        frame.render_widget(
            Paragraph::new(notice).style(Style::default().fg(Color::Yellow)),
            area,
        );
    }
    frame.render_widget(
        Paragraph::new(footer).style(Style::default().fg(Color::DarkGray)),
        areas.footer,
    );
}

fn render_overlay<A>(frame: &mut Frame<'_>, mode: &InputMode<A>, sessions: bool) {
    match mode {
        InputMode::Actions { selected, items } => {
            let area = centered_rect(
                60,
                u16::try_from(items.len()).unwrap_or(10).saturating_add(2),
                frame.area(),
            );
            frame.render_widget(Clear, area);
            let mut list_state = ListState::default().with_selected(Some(*selected));
            let list = List::new(items.iter().map(|item| {
                let row = ListItem::new(item.label.clone());
                if item.action.is_none() {
                    row.style(Style::default().fg(Color::DarkGray))
                } else {
                    row
                }
            }))
            .block(Block::default().borders(Borders::ALL).title(" Actions "))
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("› ");
            frame.render_stateful_widget(list, area, &mut list_state);
        }
        InputMode::Confirm { prompt, .. } => {
            let area = centered_rect(70, 5, frame.area());
            frame.render_widget(Clear, area);
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(prompt.clone()),
                    Line::from(vec![
                        Span::styled(" y/Enter ", Style::default().fg(Color::Green)),
                        Span::raw("confirm   "),
                        Span::styled("n/Esc", Style::default().fg(Color::Red)),
                        Span::raw(" cancel"),
                    ]),
                ])
                .wrap(Wrap { trim: true })
                .block(Block::default().borders(Borders::ALL).title(" Confirm ")),
                area,
            );
        }
        InputMode::Help => {
            let area = centered_rect(72, 11, frame.area());
            frame.render_widget(Clear, area);
            let resource_help = if sessions {
                "a  Toggle current/all project scope"
            } else {
                "c  Clean up merged worktrees"
            };
            frame.render_widget(
                Paragraph::new(format!("↑/↓ or j/k  Move selection\nPageUp/PageDown  Move faster\nHome/End  First/last item\nEnter  Open actions\n/  Filter visible items\nr  Refresh\n{resource_help}\nq or Esc  Close"))
                    .block(Block::default().borders(Borders::ALL).title(" Help ")),
                area,
            );
        }
        InputMode::Browse | InputMode::Filter => {}
    }
}

fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let width = area
        .width
        .saturating_mul(percent_x)
        .saturating_div(100)
        .max(20)
        .min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn status_name(status: SessionStatus) -> String {
    format!("{status:?}").to_ascii_lowercase()
}

const fn kind_name(kind: SessionKind) -> &'static str {
    match kind {
        SessionKind::Interactive => "interactive",
        SessionKind::Job => "job",
    }
}

fn relative_age(timestamp: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let seconds = now.saturating_sub(timestamp);
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3_599 => format!("{}m", seconds / 60),
        3_600..=86_399 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use codex_start_core::UnixArgument;
    use ratatui::{Terminal, backend::TestBackend};

    use super::{
        SessionManagerAction, SessionState, WorktreeManagerAction, WorktreeState, render_sessions,
        render_worktrees, session_actions, validate_terminal_with, worktree_actions,
    };
    use crate::{
        cli::OutputFormat,
        git::ManagedWorktree,
        runtime::RuntimeKind,
        session::{SessionKind, SessionRecord, SessionStatus},
    };

    fn session(alias: &str, project_id: &str, status: SessionStatus) -> SessionRecord {
        let mut record = SessionRecord::new(
            alias.to_owned(),
            project_id.to_owned(),
            "rust".to_owned(),
            "default".to_owned(),
            SessionKind::Interactive,
            RuntimeKind::Docker,
            UnixArgument::from("docker"),
            format!("container-{alias}"),
            UnixArgument::from("/workspace"),
            UnixArgument::from("/workspace"),
        );
        record.status = status;
        record
    }

    fn worktree(name: &str, current: bool) -> ManagedWorktree {
        ManagedWorktree {
            name: name.to_owned(),
            branch: format!("codex/{name}"),
            head: "0123456789012345678901234567890123456789".to_owned(),
            path: PathBuf::from(format!("/worktrees/{name}")),
            dirty: false,
            modified_unix_seconds: 0,
            current,
        }
    }

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    #[test]
    fn validates_manager_terminal_and_output_mode() {
        assert!(validate_terminal_with(OutputFormat::Human, true, true, "session list").is_ok());
        assert!(validate_terminal_with(OutputFormat::Json, true, true, "session list").is_err());
        assert!(validate_terminal_with(OutputFormat::Human, false, true, "session list").is_err());
        assert!(validate_terminal_with(OutputFormat::Human, true, false, "session list").is_err());
    }

    #[test]
    fn session_scope_filter_and_actions_follow_lifecycle_state() {
        let running = session("feature", "current", SessionStatus::Running);
        let stopped = session("done", "other", SessionStatus::Stopped);
        let mut state = SessionState::new(
            vec![running.clone(), stopped.clone()],
            "current".to_owned(),
            None,
        );
        assert_eq!(state.visible().len(), 1);
        state.show_all = true;
        state.filter = "done".to_owned();
        assert_eq!(state.visible().len(), 1);
        assert_eq!(state.visible()[0].alias, "done");

        let running_actions = session_actions(&running);
        assert!(
            running_actions
                .iter()
                .any(|item| matches!(item.action, Some(SessionManagerAction::Attach(_))))
        );
        assert!(running_actions.iter().any(|item| matches!(
            item.action,
            Some(SessionManagerAction::Stop(_))
        ) && item.confirm.is_some()));
        assert!(
            !running_actions
                .iter()
                .any(|item| matches!(item.action, Some(SessionManagerAction::Remove(_))))
        );

        let stopped_actions = session_actions(&stopped);
        assert!(
            stopped_actions
                .iter()
                .any(|item| matches!(item.action, Some(SessionManagerAction::Restart(_))))
        );
        assert!(stopped_actions.iter().any(|item| matches!(
            item.action,
            Some(SessionManagerAction::Remove(_))
        ) && item.confirm.is_some()));
    }

    #[test]
    fn current_worktree_cannot_be_applied_to_itself() {
        let current_actions = worktree_actions(&worktree("current", true));
        assert!(!current_actions.iter().any(|item| matches!(
            item.action,
            Some(WorktreeManagerAction::Squash(_) | WorktreeManagerAction::Move(_))
        )));
        let other_actions = worktree_actions(&worktree("other", false));
        assert!(other_actions.iter().any(|item| matches!(
            item.action,
            Some(WorktreeManagerAction::Squash(_))
        ) && item.confirm.is_some()));
        assert!(other_actions.iter().any(|item| matches!(
            item.action,
            Some(WorktreeManagerAction::Move(_))
        ) && item.confirm.is_some()));
        assert!(!other_actions.iter().any(|item| matches!(
            item.action,
            Some(WorktreeManagerAction::Cleanup)
        ) && item.confirm.is_none()));
    }

    #[test]
    fn renders_wide_narrow_and_empty_manager_states() {
        let sessions = SessionState::new(
            vec![session("feature", "current", SessionStatus::Detached)],
            "current".to_owned(),
            None,
        );
        let mut wide = Terminal::new(TestBackend::new(120, 30)).expect("wide terminal");
        wide.draw(|frame| render_sessions(frame, &sessions))
            .expect("render sessions");
        let wide_text = buffer_text(&wide);
        assert!(wide_text.contains("Sessions"));
        assert!(wide_text.contains("feature"));
        assert!(wide_text.contains("Details"));

        let worktrees = WorktreeState::new(vec![worktree("agent", false)], None);
        let mut narrow = Terminal::new(TestBackend::new(70, 20)).expect("narrow terminal");
        narrow
            .draw(|frame| render_worktrees(frame, &worktrees))
            .expect("render worktrees");
        let narrow_text = buffer_text(&narrow);
        assert!(narrow_text.contains("Managed worktrees"));
        assert!(narrow_text.contains("agent"));

        let empty = WorktreeState::new(Vec::new(), None);
        let mut compact = Terminal::new(TestBackend::new(60, 12)).expect("compact terminal");
        compact
            .draw(|frame| render_worktrees(frame, &empty))
            .expect("render empty");
        assert!(buffer_text(&compact).contains("Managed worktrees"));
    }
}
