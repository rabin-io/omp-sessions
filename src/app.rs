use crate::model::{Paths, Session, SortMode, cmp_f64_desc, short_id, tokens_match, truncate_chars};
use crate::store::{LoadProgress, delete_sessions, load_sessions};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::TableState;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

const TOAST_TTL: Duration = Duration::from_secs(4);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Table,
    Filter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone)]
pub struct Notification {
    pub message: String,
    pub severity: Severity,
    pub expires_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmDelete {
    None,
    Single(String),
    Batch,
}

#[derive(Debug, Clone)]
pub struct ResumeAction {
    pub session: Session,
    pub restore: bool,
}

impl ResumeAction {
    pub fn command_args(&self) -> [&str; 3] {
        let flag = if self.restore { "--restore" } else { "--resume" };
        ["omp", flag, self.session.path.as_str()]
    }
}

enum LoadMsg {
    Progress(LoadProgress),
    Finished {
        sessions: Vec<Session>,
        is_refresh: bool,
    },
}

pub struct App {
    pub paths: Paths,
    pub sessions: Vec<Session>,
    pub filtered: Vec<Session>,
    pub table_state: TableState,
    pub selected: HashSet<String>,
    pub sort_mode: SortMode,
    pub focus: Focus,
    pub filter_query: String,
    pub filter_cursor: usize,
    pub confirm_delete: ConfirmDelete,
    pub loading: bool,
    pub loading_is_refresh: bool,
    pub loading_progress: LoadProgress,
    pub loading_tick: usize,
    pub notifications: Vec<Notification>,
    pub should_quit: bool,
    pub resume_action: Option<ResumeAction>,
    pub last_table_height: usize,
    load_rx: Option<Receiver<LoadMsg>>,
    cancel_load: Option<Arc<AtomicBool>>,
}

impl App {
    pub fn new(paths: Paths) -> Self {
        Self {
            paths,
            sessions: Vec::new(),
            filtered: Vec::new(),
            table_state: TableState::default(),
            selected: HashSet::new(),
            sort_mode: SortMode::Date,
            focus: Focus::Table,
            filter_query: String::new(),
            filter_cursor: 0,
            confirm_delete: ConfirmDelete::None,
            loading: false,
            loading_is_refresh: false,
            loading_progress: LoadProgress {
                stage: "Loading sessions",
                done: 0,
                total: 0,
            },
            loading_tick: 0,
            notifications: Vec::new(),
            should_quit: false,
            resume_action: None,
            last_table_height: 20,
            load_rx: None,
            cancel_load: None,
        }
    }

    pub fn start_load(&mut self, is_refresh: bool) {
        if self.loading {
            return;
        }
        self.loading = true;
        self.loading_is_refresh = is_refresh;
        self.loading_tick = 0;
        self.loading_progress = LoadProgress {
            stage: if is_refresh {
                "Refreshing sessions"
            } else {
                "Loading sessions"
            },
            done: 0,
            total: 0,
        };
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel_load = Some(Arc::clone(&cancel));
        let (tx, rx) = mpsc::channel();
        self.load_rx = Some(rx);
        let paths = self.paths.clone();

        thread::spawn(move || {
            let mut last_emit = Instant::now() - Duration::from_secs(1);
            let mut last_stage: &'static str = "";
            let sessions = load_sessions(
                &paths,
                |progress| {
                    let now = Instant::now();
                    if progress.stage == last_stage
                        && progress.done != 0
                        && progress.done != progress.total
                        && now.duration_since(last_emit) < Duration::from_millis(50)
                    {
                        return;
                    }
                    last_emit = now;
                    last_stage = progress.stage;
                    let _ = tx.send(LoadMsg::Progress(progress));
                },
                Some(cancel.as_ref()),
            );
            if !cancel.load(Ordering::Relaxed) {
                let _ = tx.send(LoadMsg::Finished {
                    sessions,
                    is_refresh,
                });
            }
        });
    }

    pub fn cancel_background_load(&mut self) {
        if let Some(cancel) = &self.cancel_load {
            cancel.store(true, Ordering::Relaxed);
        }
        self.load_rx = None;
        self.loading = false;
    }

    pub fn on_tick(&mut self) {
        if self.loading {
            self.loading_tick = self.loading_tick.wrapping_add(1);
        }
        let now = Instant::now();
        self.notifications.retain(|n| n.expires_at > now);
        self.poll_load();
    }

    pub fn poll_load(&mut self) {
        let mut finished: Option<(Vec<Session>, bool)> = None;
        let mut disconnected = false;

        if let Some(rx) = &self.load_rx {
            loop {
                match rx.try_recv() {
                    Ok(LoadMsg::Progress(p)) => {
                        self.loading_progress = p;
                    }
                    Ok(LoadMsg::Finished {
                        sessions,
                        is_refresh,
                    }) => {
                        finished = Some((sessions, is_refresh));
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }

        if let Some((sessions, is_refresh)) = finished {
            self.load_rx = None;
            self.cancel_load = None;
            self.loading = false;
            self.sessions = sessions;
            let live_ids: HashSet<&str> = self.sessions.iter().map(|s| s.id.as_str()).collect();
            self.selected.retain(|id| live_ids.contains(id.as_str()));
            self.confirm_delete = ConfirmDelete::None;
            self.apply_filter(None);
            self.focus = Focus::Table;
            if is_refresh {
                self.notify(
                    format!("Refreshed · {} sessions", self.sessions.len()),
                    Severity::Info,
                );
            }
        } else if disconnected {
            self.load_rx = None;
            self.cancel_load = None;
            self.loading = false;
            let prefix = if self.loading_is_refresh {
                "Refresh failed"
            } else {
                "Failed to load sessions"
            };
            self.notify(prefix, Severity::Error);
        }
    }

    #[cfg(test)]
    pub fn set_sessions_for_test(&mut self, sessions: Vec<Session>) {
        self.sessions = sessions;
        self.loading = false;
        self.apply_filter(None);
    }

    pub fn notify(&mut self, message: impl Into<String>, severity: Severity) {
        self.notifications.push(Notification {
            message: message.into(),
            severity,
            expires_at: Instant::now() + TOAST_TTL,
        });
        if self.notifications.len() > 2 {
            let excess = self.notifications.len() - 2;
            self.notifications.drain(..excess);
        }
    }

    pub fn subtitle(&self) -> String {
        if self.loading {
            let p = &self.loading_progress;
            let verb = if self.loading_is_refresh {
                "Refreshing"
            } else {
                "Loading"
            };
            if p.total > 0 && p.stage == "Reading transcripts" {
                return format!("{verb}… {}/{}", p.done, p.total);
            }
            return format!("{}…", p.stage);
        }
        let mut parts = Vec::new();
        if !self.selected.is_empty() {
            parts.push(format!("{} selected", self.selected.len()));
        }
        if self.filtered.len() != self.sessions.len() {
            parts.push(format!(
                "{}/{} sessions",
                self.filtered.len(),
                self.sessions.len()
            ));
        } else {
            parts.push(format!("{} sessions", self.sessions.len()));
        }
        parts.push(format!("sort: {}", self.sort_mode.as_str()));
        parts.join(" · ")
    }

    pub fn current_session(&self) -> Option<&Session> {
        let row = self.table_state.selected()?;
        self.filtered.get(row)
    }

    fn sort_filtered(&mut self) {
        match self.sort_mode {
            SortMode::Project => {
                self.filtered.sort_by(|a, b| {
                    a.project_name()
                        .cmp(b.project_name())
                        .then_with(|| cmp_f64_desc(a.last_ts, b.last_ts))
                });
            }
            SortMode::Status => {
                self.filtered.sort_by(|a, b| {
                    b.live
                        .cmp(&a.live)
                        .then_with(|| cmp_f64_desc(a.last_ts, b.last_ts))
                });
            }
            SortMode::Date => {
                self.filtered
                    .sort_by(|a, b| cmp_f64_desc(a.last_ts, b.last_ts));
            }
        }
    }

    pub fn apply_filter(&mut self, preserve_id: Option<&str>) {
        let tokens: Vec<String> = self
            .filter_query
            .trim()
            .to_lowercase()
            .split_whitespace()
            .map(str::to_string)
            .collect();
        if tokens.is_empty() {
            self.filtered = self.sessions.clone();
        } else {
            self.filtered = self
                .sessions
                .iter()
                .filter(|s| tokens_match(&tokens, &s.searchable_text()))
                .cloned()
                .collect();
        }
        self.sort_filtered();
        if self.filtered.is_empty() {
            self.table_state.select(None);
        } else {
            let target = preserve_id
                .and_then(|id| self.filtered.iter().position(|s| s.id == id))
                .unwrap_or(0);
            self.table_state.select(Some(target));
        }
    }

    fn select_row(&mut self, row: usize) {
        if self.filtered.is_empty() {
            self.table_state.select(None);
            return;
        }
        let clamped = row.min(self.filtered.len() - 1);
        if self.table_state.selected() != Some(clamped) {
            self.confirm_delete = ConfirmDelete::None;
        }
        self.table_state.select(Some(clamped));
    }

    fn move_cursor(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            self.table_state.select(None);
            return;
        }
        let cur = self.table_state.selected().unwrap_or(0) as isize;
        let max_idx = (self.filtered.len() - 1) as isize;
        let next = (cur + delta).clamp(0, max_idx) as usize;
        self.select_row(next);
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            self.cancel_background_load();
            self.should_quit = true;
            return;
        }

        if self.loading {
            if matches!(key.code, KeyCode::Char('q')) && self.focus == Focus::Table {
                self.cancel_background_load();
                self.should_quit = true;
            }
            return;
        }

        match self.focus {
            Focus::Filter => self.handle_filter_key(key),
            Focus::Table => self.handle_table_key(key),
        }
    }

    fn handle_filter_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter | KeyCode::Down => {
                self.focus = Focus::Table;
            }
            KeyCode::Esc => {
                if !self.filter_query.is_empty() {
                    self.filter_query.clear();
                    self.filter_cursor = 0;
                    self.apply_filter(None);
                }
                self.focus = Focus::Table;
            }
            KeyCode::Left => {
                self.filter_cursor = self.filter_cursor.saturating_sub(1);
            }
            KeyCode::Right => {
                let len = self.filter_query.chars().count();
                if self.filter_cursor < len {
                    self.filter_cursor += 1;
                }
            }
            KeyCode::Home => {
                self.filter_cursor = 0;
            }
            KeyCode::End => {
                self.filter_cursor = self.filter_query.chars().count();
            }
            KeyCode::Char('a') if ctrl => {
                self.filter_cursor = 0;
            }
            KeyCode::Char('e') if ctrl => {
                self.filter_cursor = self.filter_query.chars().count();
            }
            KeyCode::Char('u') if ctrl => {
                if self.filter_cursor > 0 {
                    let byte_idx = char_to_byte_idx(&self.filter_query, self.filter_cursor);
                    self.filter_query.drain(..byte_idx);
                    self.filter_cursor = 0;
                    self.apply_filter(None);
                }
            }
            KeyCode::Char('w') if ctrl => {
                if self.filter_cursor > 0 {
                    let chars: Vec<char> = self.filter_query.chars().collect();
                    let mut new_cur = self.filter_cursor;
                    while new_cur > 0 && chars[new_cur - 1].is_whitespace() {
                        new_cur -= 1;
                    }
                    while new_cur > 0 && !chars[new_cur - 1].is_whitespace() {
                        new_cur -= 1;
                    }
                    let start_b = char_to_byte_idx(&self.filter_query, new_cur);
                    let end_b = char_to_byte_idx(&self.filter_query, self.filter_cursor);
                    self.filter_query.drain(start_b..end_b);
                    self.filter_cursor = new_cur;
                    self.apply_filter(None);
                }
            }
            KeyCode::Backspace => {
                if self.filter_cursor > 0 {
                    let start_b = char_to_byte_idx(&self.filter_query, self.filter_cursor - 1);
                    let end_b = char_to_byte_idx(&self.filter_query, self.filter_cursor);
                    self.filter_query.drain(start_b..end_b);
                    self.filter_cursor -= 1;
                    self.apply_filter(None);
                }
            }
            KeyCode::Delete => {
                let len = self.filter_query.chars().count();
                if self.filter_cursor < len {
                    let start_b = char_to_byte_idx(&self.filter_query, self.filter_cursor);
                    let end_b = char_to_byte_idx(&self.filter_query, self.filter_cursor + 1);
                    self.filter_query.drain(start_b..end_b);
                    self.apply_filter(None);
                }
            }
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                let byte_idx = char_to_byte_idx(&self.filter_query, self.filter_cursor);
                self.filter_query.insert(byte_idx, c);
                self.filter_cursor += 1;
                self.apply_filter(None);
            }
            _ => {}
        }
    }

    fn handle_table_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page_step = self.last_table_height.saturating_sub(2).max(1) as isize;
        match key.code {
            KeyCode::Char('q') => {
                self.should_quit = true;
            }
            KeyCode::Char('/') => {
                self.focus = Focus::Filter;
                self.filter_cursor = self.filter_query.chars().count();
            }
            KeyCode::Enter => {
                if let Some(session) = self.current_session().cloned() {
                    self.resume_action = Some(ResumeAction {
                        session,
                        restore: false,
                    });
                    self.should_quit = true;
                }
            }
            KeyCode::Char('f') => {
                if let Some(session) = self.current_session().cloned() {
                    self.resume_action = Some(ResumeAction {
                        session,
                        restore: true,
                    });
                    self.should_quit = true;
                }
            }
            KeyCode::Char(' ') => {
                self.action_toggle_select();
            }
            KeyCode::Char('d') if !ctrl => {
                self.action_delete();
            }
            KeyCode::Char('s') => {
                self.sort_mode = self.sort_mode.next();
                self.sort_filtered();
                if !self.filtered.is_empty() {
                    self.table_state.select(Some(0));
                }
                self.confirm_delete = ConfirmDelete::None;
                self.notify(format!("Sort: {}", self.sort_mode.as_str()), Severity::Info);
            }
            KeyCode::Char('r') => {
                self.start_load(true);
            }
            KeyCode::Esc => {
                if !self.filter_query.is_empty() {
                    self.filter_query.clear();
                    self.filter_cursor = 0;
                    self.apply_filter(None);
                } else if !self.selected.is_empty() {
                    self.selected.clear();
                    self.confirm_delete = ConfirmDelete::None;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::PageUp => self.move_cursor(-page_step),
            KeyCode::PageDown => self.move_cursor(page_step),
            KeyCode::Char('u') if ctrl => self.move_cursor(-page_step / 2),
            KeyCode::Char('d') if ctrl => self.move_cursor(page_step / 2),
            KeyCode::Home | KeyCode::Char('g') => self.select_row(0),
            KeyCode::End | KeyCode::Char('G') => {
                if !self.filtered.is_empty() {
                    self.select_row(self.filtered.len() - 1);
                }
            }
            _ => {}
        }
    }

    fn action_toggle_select(&mut self) {
        let Some(row) = self.table_state.selected() else {
            return;
        };
        let Some(id) = self.filtered.get(row).map(|s| s.id.clone()) else {
            return;
        };
        if !self.selected.remove(&id) {
            self.selected.insert(id);
        }
        self.confirm_delete = ConfirmDelete::None;
        let next = (row + 1).min(self.filtered.len().saturating_sub(1));
        self.table_state.select(Some(next));
    }

    fn format_skip_details(live_count: usize, hidden_count: usize) -> String {
        let mut parts = Vec::new();
        if live_count > 0 {
            parts.push(format!("{live_count} live"));
        }
        if hidden_count > 0 {
            parts.push(format!("{hidden_count} hidden by filter"));
        }
        parts.join(", ")
    }

    fn mark_live(&mut self, ids: &HashSet<String>) {
        if ids.is_empty() {
            return;
        }
        for s in &mut self.sessions {
            if ids.contains(&s.id) {
                s.live = true;
            }
        }
        for s in &mut self.filtered {
            if ids.contains(&s.id) {
                s.live = true;
            }
        }
    }

    fn remove_deleted_rows(&mut self, gone: &HashSet<String>, restore_row: Option<usize>) {
        self.sessions.retain(|s| !gone.contains(&s.id));
        self.filtered.retain(|s| !gone.contains(&s.id));
        self.selected.retain(|id| !gone.contains(id));
        self.confirm_delete = ConfirmDelete::None;
        self.sort_filtered();
        if self.filtered.is_empty() {
            self.table_state.select(None);
        } else {
            let target = restore_row
                .map(|r| r.min(self.filtered.len() - 1))
                .unwrap_or(0);
            self.table_state.select(Some(target));
        }
    }

    fn delete_selected(&mut self) {
        let visible_selected: Vec<Session> = self
            .filtered
            .iter()
            .filter(|s| self.selected.contains(&s.id))
            .cloned()
            .collect();
        let hidden_count = self.selected.len().saturating_sub(visible_selected.len());
        let targets: Vec<Session> = visible_selected
            .iter()
            .filter(|s| !s.live)
            .cloned()
            .collect();
        let live_count = visible_selected.len().saturating_sub(targets.len());

        if targets.is_empty() {
            let detail = Self::format_skip_details(live_count, hidden_count);
            let msg = if detail.is_empty() {
                "No deletable sessions selected".to_string()
            } else {
                format!("No deletable sessions selected ({detail})")
            };
            self.notify(msg, Severity::Warning);
            return;
        }

        if self.confirm_delete != ConfirmDelete::Batch {
            self.confirm_delete = ConfirmDelete::Batch;
            let detail = Self::format_skip_details(live_count, hidden_count);
            let mut msg = format!("Press d again to delete {} sessions", targets.len());
            if !detail.is_empty() {
                msg.push_str(&format!(" ({detail} will be skipped)"));
            }
            self.notify(msg, Severity::Warning);
            return;
        }

        let outcome = delete_sessions(&self.paths, &targets);
        let total_live = live_count + outcome.skipped_live.len();
        let newly_live: HashSet<String> = outcome
            .skipped_live
            .iter()
            .map(|s| s.id.clone())
            .collect();
        self.mark_live(&newly_live);

        let gone: HashSet<String> = outcome.deleted.iter().map(|s| s.id.clone()).collect();
        if !gone.is_empty() || !newly_live.is_empty() {
            self.remove_deleted_rows(&gone, None);
        } else {
            self.confirm_delete = ConfirmDelete::None;
        }

        let detail = Self::format_skip_details(total_live, hidden_count);
        let mut msg = format!("Deleted {} sessions", outcome.deleted.len());
        if !detail.is_empty() {
            msg.push_str(&format!(" ({detail} skipped)"));
        }
        self.notify(msg, Severity::Info);
        if !outcome.errors.is_empty() {
            self.notify(
                format!("Delete errors: {}", outcome.errors.join("; ")),
                Severity::Error,
            );
        }
    }

    fn action_delete(&mut self) {
        if !self.selected.is_empty() {
            self.delete_selected();
            return;
        }
        let Some(row) = self.table_state.selected() else {
            return;
        };
        let Some(s) = self.filtered.get(row).cloned() else {
            return;
        };
        if s.live {
            self.notify("Cannot delete a live session", Severity::Warning);
            return;
        }
        if self.confirm_delete != ConfirmDelete::Single(s.id.clone()) {
            self.confirm_delete = ConfirmDelete::Single(s.id.clone());
            let title = if !s.title.is_empty() {
                s.title.as_str()
            } else {
                truncate_chars(&s.first_prompt, 40)
            };
            self.notify(format!("Press d again to delete: {title}"), Severity::Warning);
            return;
        }

        let outcome = delete_sessions(&self.paths, &[s.clone()]);
        if !outcome.skipped_live.is_empty() {
            self.confirm_delete = ConfirmDelete::None;
            let newly_live: HashSet<String> = outcome
                .skipped_live
                .iter()
                .map(|x| x.id.clone())
                .collect();
            self.mark_live(&newly_live);
            self.notify("Cannot delete a live session", Severity::Warning);
            return;
        }
        if !outcome.deleted.is_empty() {
            let mut gone = HashSet::new();
            gone.insert(s.id.clone());
            self.remove_deleted_rows(&gone, Some(row));
            self.notify(format!("Deleted session {}…", short_id(&s.id)), Severity::Info);
        } else {
            self.confirm_delete = ConfirmDelete::None;
        }
        if !outcome.errors.is_empty() {
            self.notify(
                format!("Delete failed: {}", outcome.errors.join("; ")),
                Severity::Error,
            );
        }
    }
}

pub fn char_to_byte_idx(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(idx, _)| idx)
        .unwrap_or(s.len())
}
