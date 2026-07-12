use std::thread;

use crate::AppSession;
use crate::components::format_relative_time;
use crate::components::keybindings::key;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::text_buffer::TextBuffer;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use maki_storage::StateDir;
use maki_storage::sessions::SessionStatus;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

const TITLE: &str = " Agents ";
const NO_SESSIONS_MSG: &str = "No sessions yet in this directory";
const FOOTER_HINTS: &[(&str, &str)] = &[
    ("↑/↓", "navigate"),
    ("→", "open"),
    ("Enter", "open/spawn"),
    (key::DELETE.label, "delete"),
];

const SECTION_NEEDS_INPUT: &str = "Needs input";
const SECTION_WORKING: &str = "Working";
const SECTION_COMPLETED: &str = "Completed";

/// The event loop ticks roughly per frame; refreshing the board about once a
/// second keeps background status changes visible without hammering storage.
const REFRESH_INTERVAL_TICKS: u16 = 60;

const TASK_BOX_TITLE: &str = " New session ";
const TASK_BOX_PLACEHOLDER: &str = "Describe a task for a new session";

#[derive(Debug)]
pub enum DashboardAction {
    Consumed,
    Open(String),
    NewSession,
    /// Spawn a new session seeded with the typed task prompt.
    SpawnTask(String),
    ConfirmDelete,
    Delete(String),
    None,
}

struct DashboardEntry {
    id: String,
    title: String,
    detail: String,
    section: &'static str,
    spinning: bool,
}

impl PickerItem for DashboardEntry {
    fn label(&self) -> &str {
        &self.title
    }
    fn detail(&self) -> Option<&str> {
        Some(&self.detail)
    }
    fn section(&self) -> Option<&str> {
        Some(self.section)
    }
    fn is_spinning(&self) -> bool {
        self.spinning
    }
}

/// Full-screen multi-session overview shown by `maki agents`. Lists the current
/// directory's sessions grouped into Needs input / Working / Completed sections.
pub struct SessionDashboard {
    picker: ListPicker<DashboardEntry>,
    task_input: TextBuffer,
    confirming: Option<(String, u64)>,
    pending_rx: Option<flume::Receiver<Result<Vec<DashboardEntry>, String>>>,
    flash: Option<String>,
    source: Option<(String, StateDir)>,
    refreshing: bool,
    ticks_since_refresh: u16,
}

impl SessionDashboard {
    pub fn new() -> Self {
        Self {
            picker: ListPicker::new().with_footer(FOOTER_HINTS),
            task_input: TextBuffer::new(String::new()),
            confirming: None,
            pending_rx: None,
            flash: None,
            source: None,
            refreshing: false,
            ticks_since_refresh: 0,
        }
    }

    pub fn open(&mut self, cwd: &str, dir: &StateDir) {
        self.picker.open_loading(TITLE);
        self.source = Some((cwd.to_owned(), dir.clone()));
        self.refreshing = false;
        self.ticks_since_refresh = 0;
        self.pending_rx = Some(spawn_scan(cwd.to_owned(), dir.clone()));
    }

    fn try_resolve(&mut self) {
        let Some(ref rx) = self.pending_rx else {
            return;
        };
        let Ok(result) = rx.try_recv() else {
            return;
        };
        self.pending_rx = None;
        let was_refresh = self.refreshing;
        self.refreshing = false;
        match result {
            Ok(entries) if entries.is_empty() => {
                self.picker.resolve(entries);
                self.picker.set_error_text(Some(NO_SESSIONS_MSG.into()));
            }
            // A live refresh replaces items in place so the user's current
            // selection and scroll position survive the status update.
            Ok(entries) if was_refresh => {
                self.picker.set_error_text(None);
                self.picker.replace_items(entries);
            }
            Ok(entries) => self.picker.resolve(entries),
            Err(e) => {
                if !was_refresh {
                    self.picker.resolve(Vec::new());
                    self.picker.set_error_text(Some(e));
                }
            }
        }
    }

    /// Kick off a background re-scan to reflect status changes from other
    /// sessions without disturbing the current selection. No-op while an
    /// initial load or another refresh is in flight.
    fn refresh(&mut self) {
        if self.pending_rx.is_some() || self.picker.is_loading() {
            return;
        }
        let Some((cwd, dir)) = self.source.clone() else {
            return;
        };
        self.refreshing = true;
        self.pending_rx = Some(spawn_scan(cwd, dir));
    }

    pub fn take_flash(&mut self) -> Option<String> {
        self.flash.take()
    }

    pub fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub fn close(&mut self) {
        self.picker.close();
        self.pending_rx = None;
        self.source = None;
        self.refreshing = false;
        self.task_input.clear();
    }

    pub fn remove_entry(&mut self, id: &str) {
        self.picker.retain(|e| e.id != id);
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.picker.contains(pos)
    }

    pub fn scroll(&mut self, delta: i32) {
        self.picker.scroll(delta);
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        self.picker.handle_paste(text)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DashboardAction {
        if is_delete_key(&key) {
            return self.handle_delete_key();
        }

        // Ctrl-N spawns a brand-new empty session.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
            && key.code == KeyCode::Char('n')
        {
            return DashboardAction::NewSession;
        }

        // List navigation keys always drive the picker, so the task box never
        // swallows them. Everything else is treated as editing the task prompt.
        match key.code {
            KeyCode::Up | KeyCode::Down => {
                self.picker.handle_key(key);
                return DashboardAction::Consumed;
            }
            KeyCode::Right => {
                return match self.picker.selected_item() {
                    Some(item) => DashboardAction::Open(item.id.clone()),
                    None => DashboardAction::Consumed,
                };
            }
            KeyCode::Enter => {
                let task = self.task_input.value();
                if !task.trim().is_empty() {
                    self.task_input.clear();
                    return DashboardAction::SpawnTask(task);
                }
                return match self.picker.selected_item() {
                    Some(item) => DashboardAction::Open(item.id.clone()),
                    None => DashboardAction::Consumed,
                };
            }
            KeyCode::Esc => {
                return match self.picker.handle_key(key) {
                    PickerAction::Close => DashboardAction::None,
                    _ => DashboardAction::Consumed,
                };
            }
            _ => {}
        }

        self.task_input.handle_key(key);
        DashboardAction::Consumed
    }

    fn handle_delete_key(&mut self) -> DashboardAction {
        let Some(selected) = self.picker.selected_item() else {
            return DashboardAction::Consumed;
        };

        let generation = self.picker.generation();
        if self
            .confirming
            .as_ref()
            .is_some_and(|(id, g)| id == &selected.id && *g == generation)
        {
            return DashboardAction::Delete(selected.id.clone());
        }

        self.confirming = Some((selected.id.clone(), generation));
        DashboardAction::ConfirmDelete
    }

    pub fn tick(&mut self) {
        self.try_resolve();

        self.ticks_since_refresh = self.ticks_since_refresh.saturating_add(1);
        if self.ticks_since_refresh >= REFRESH_INTERVAL_TICKS {
            self.ticks_since_refresh = 0;
            self.refresh();
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        const TASK_BOX_HEIGHT: u16 = 2;

        // Reserve a strip at the bottom for the "new task" prompt, letting the
        // picker modal center itself in the remaining space above it.
        let [list_area, task_area] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(TASK_BOX_HEIGHT),
        ])
        .areas(area);

        let popup = self.picker.view(frame, list_area);

        // Align the task box under the picker popup for a coherent column.
        let box_area = Rect {
            x: popup.x,
            y: task_area.y,
            width: popup.width.max(1),
            height: TASK_BOX_HEIGHT.min(task_area.height),
        };
        self.render_task_box(frame, box_area);

        popup
    }

    fn render_task_box(&self, frame: &mut Frame, area: Rect) {
        use ratatui::widgets::{Block, BorderType, Borders};

        let theme = crate::theme::current();
        // A single titled top rule separates the input from the list above;
        // no side or bottom borders, so it reads as a clean input line rather
        // than a boxed panel (avoids the stacked double-line a full box gave).
        let block = Block::default()
            .borders(Borders::TOP)
            .border_type(BorderType::Plain)
            .border_style(theme.input_border)
            .title(Span::styled(TASK_BOX_TITLE, theme.panel_title));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let text = self.task_input.value();
        let line = if text.is_empty() {
            Line::from(vec![
                super::chevron_span(),
                Span::styled(TASK_BOX_PLACEHOLDER, theme.item_desc),
            ])
        } else {
            let cursor_x = self.task_input.x();
            let chars: Vec<char> = text.chars().collect();
            let before: String = chars[..cursor_x.min(chars.len())].iter().collect();
            let cursor_char = chars.get(cursor_x).copied().unwrap_or(' ');
            let after_start = cursor_x.saturating_add(1).min(chars.len());
            let after: String = chars[after_start..].iter().collect();
            Line::from(vec![
                super::chevron_span(),
                Span::raw(before),
                Span::styled(cursor_char.to_string(), theme.cursor),
                Span::raw(after),
            ])
        };
        frame.render_widget(Paragraph::new(vec![line]), inner);
    }
}

impl crate::components::Overlay for SessionDashboard {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close()
    }
}

fn is_delete_key(key: &KeyEvent) -> bool {
    key::DELETE.matches(*key)
}

fn section_rank(status: SessionStatus) -> u8 {
    match status {
        SessionStatus::NeedsInput => 0,
        SessionStatus::Working => 1,
        SessionStatus::Completed | SessionStatus::Idle | SessionStatus::Error => 2,
    }
}

fn section_label(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::NeedsInput => SECTION_NEEDS_INPUT,
        SessionStatus::Working => SECTION_WORKING,
        SessionStatus::Completed | SessionStatus::Idle | SessionStatus::Error => SECTION_COMPLETED,
    }
}

fn spawn_scan(cwd: String, dir: StateDir) -> flume::Receiver<Result<Vec<DashboardEntry>, String>> {
    let (tx, rx) = flume::bounded(1);
    thread::spawn(move || {
        let result = AppSession::list(&cwd, &dir)
            .map(|mut summaries| {
                summaries.sort_by(|a, b| {
                    section_rank(a.status)
                        .cmp(&section_rank(b.status))
                        .then(b.updated_at.cmp(&a.updated_at))
                });
                summaries.into_iter().map(entry_from_summary).collect()
            })
            .map_err(|e| format!("Failed to list sessions: {e}"));
        let _ = tx.send(result);
    });
    rx
}

fn entry_from_summary(s: maki_storage::sessions::SessionSummary) -> DashboardEntry {
    let time = format_relative_time(s.updated_at);
    let detail = match &s.summary {
        Some(summary) if !summary.is_empty() => format!("{summary}  ·  {time}"),
        _ => time,
    };
    DashboardEntry {
        id: s.id,
        title: s.title,
        detail,
        section: section_label(s.status),
        spinning: matches!(s.status, SessionStatus::Working),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case(SessionStatus::NeedsInput, 0 ; "needs_input_first")]
    #[test_case(SessionStatus::Working, 1 ; "working_second")]
    #[test_case(SessionStatus::Completed, 2 ; "completed_last")]
    #[test_case(SessionStatus::Idle, 2 ; "idle_with_completed")]
    #[test_case(SessionStatus::Error, 2 ; "error_with_completed")]
    fn status_orders_into_sections(status: SessionStatus, expected_rank: u8) {
        assert_eq!(section_rank(status), expected_rank);
    }

    #[test_case(SessionStatus::NeedsInput, SECTION_NEEDS_INPUT ; "needs_input_label")]
    #[test_case(SessionStatus::Working, SECTION_WORKING ; "working_label")]
    #[test_case(SessionStatus::Completed, SECTION_COMPLETED ; "completed_label")]
    fn status_maps_to_section_label(status: SessionStatus, expected: &str) {
        assert_eq!(section_label(status), expected);
    }

    fn press(dash: &mut SessionDashboard, code: KeyCode) -> DashboardAction {
        dash.handle_key(KeyEvent::new(code, KeyModifiers::empty()))
    }

    #[test]
    fn typing_fills_task_box_and_enter_spawns() {
        let mut dash = SessionDashboard::new();
        for c in "fix bug".chars() {
            assert!(matches!(press(&mut dash, KeyCode::Char(c)), DashboardAction::Consumed));
        }
        assert_eq!(dash.task_input.value(), "fix bug");

        match press(&mut dash, KeyCode::Enter) {
            DashboardAction::SpawnTask(task) => assert_eq!(task, "fix bug"),
            other => panic!("expected SpawnTask, got {other:?}"),
        }
        // Task box is cleared after spawning.
        assert_eq!(dash.task_input.value(), "");
    }

    #[test]
    fn enter_with_empty_task_does_not_spawn() {
        let mut dash = SessionDashboard::new();
        // No sessions loaded and no task typed: Enter is a no-op open attempt.
        assert!(matches!(
            press(&mut dash, KeyCode::Enter),
            DashboardAction::Consumed
        ));
    }
}
