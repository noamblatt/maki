use std::thread;

use crate::AppSession;
use crate::components::format_relative_time;
use crate::components::keybindings::key;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use maki_storage::StateDir;
use maki_storage::sessions::SessionStatus;
use ratatui::Frame;
use ratatui::layout::{Position, Rect};

const TITLE: &str = " Agents ";
const NO_SESSIONS_MSG: &str = "No sessions yet in this directory";
const FOOTER_HINTS: &[(&str, &str)] = &[
    ("↑/↓", "navigate"),
    ("→/Enter", "open"),
    (key::TASKS.label, "delete"),
];

const SECTION_NEEDS_INPUT: &str = "Needs input";
const SECTION_WORKING: &str = "Working";
const SECTION_COMPLETED: &str = "Completed";

pub enum DashboardAction {
    Consumed,
    Open(String),
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
    confirming: Option<(String, u64)>,
    pending_rx: Option<flume::Receiver<Result<Vec<DashboardEntry>, String>>>,
    flash: Option<String>,
}

impl SessionDashboard {
    pub fn new() -> Self {
        Self {
            picker: ListPicker::new().with_footer(FOOTER_HINTS),
            confirming: None,
            pending_rx: None,
            flash: None,
        }
    }

    pub fn open(&mut self, cwd: &str, dir: &StateDir) {
        self.picker.open_loading(TITLE);
        let cwd = cwd.to_owned();
        let dir = dir.clone();
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
        self.pending_rx = Some(rx);
    }

    fn try_resolve(&mut self) {
        let Some(ref rx) = self.pending_rx else {
            return;
        };
        let Ok(result) = rx.try_recv() else {
            return;
        };
        self.pending_rx = None;
        match result {
            Ok(entries) if entries.is_empty() => {
                self.picker.resolve(entries);
                self.picker.set_error_text(Some(NO_SESSIONS_MSG.into()));
            }
            Ok(entries) => self.picker.resolve(entries),
            Err(e) => {
                self.picker.resolve(Vec::new());
                self.picker.set_error_text(Some(e));
            }
        }
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

    pub fn handle_key(&mut self, key: KeyEvent) -> DashboardAction {
        if is_delete_key(&key) {
            return self.handle_delete_key();
        }

        // Right arrow opens the highlighted session (Claude Code parity).
        if key.code == KeyCode::Right && key.modifiers.is_empty() {
            return match self.picker.selected_item() {
                Some(item) => DashboardAction::Open(item.id.clone()),
                None => DashboardAction::Consumed,
            };
        }

        match self.picker.handle_key(key) {
            PickerAction::Consumed => DashboardAction::Consumed,
            PickerAction::Select(_, entry) => DashboardAction::Open(entry.id),
            PickerAction::Close => DashboardAction::None,
            PickerAction::Toggle(..) => DashboardAction::Consumed,
        }
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
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        self.picker.view(frame, area)
    }
}

fn is_delete_key(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::ALT)
        && key.code == KeyCode::Char('x')
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
}
