//! Session picker and session-management dialog.
use super::*;

/// A recorded session choice for `/resume` selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionChoice {
    pub id: SessionId,
    pub title: Option<String>,
    pub events: u64,
    pub last_seen: String,
}

/// Build rows for `/resume` interactive picker.
pub fn session_rows(
    sessions: &[SessionChoice],
    current: Option<SessionId>,
) -> (Option<Vec<(String, String)>>, usize) {
    if sessions.is_empty() {
        return (
            Some(vec![(
                "no recorded sessions".to_owned(),
                "type a task to create a new session".to_owned(),
            )]),
            0,
        );
    }
    let mut selected = 0;
    let rows: Vec<(String, String)> = sessions
        .iter()
        .enumerate()
        .map(|(idx, s)| {
            if Some(s.id) == current {
                selected = idx;
            }
            let label = match &s.title {
                Some(title) => format!("{} · {}", s.id, title),
                None => s.id.to_string(),
            };
            let desc = format!("{} events · {}", s.events, s.last_seen);
            (label, desc)
        })
        .collect();
    (Some(rows), selected)
}

pub fn session_prompt(sessions: &[SessionChoice], colour: bool) -> String {
    let choices = if sessions.is_empty() {
        "no sessions".to_owned()
    } else {
        format!("Up/Down then Enter, an ID, or 1-{}", sessions.len())
    };
    paint(colour, sgr_dim(), &format!("  resume · {choices}"))
}

/// Actions resulting from the interactive session dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionAction {
    Resume(SessionId),
    Rename(SessionId, String),
    Delete(SessionId),
    Cancel,
}

/// Operational mode for the interactive session dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionDialogMode {
    Select,
    Rename,
    ConfirmDelete,
}

/// State for the interactive `/session` dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionDialogState {
    pub sessions: Vec<SessionChoice>,
    pub selected: usize,
    pub mode: SessionDialogMode,
    pub rename_buffer: String,
    pub active_session: SessionId,
}

impl SessionDialogState {
    pub fn new(sessions: Vec<SessionChoice>, active_session: SessionId) -> Self {
        let selected = sessions
            .iter()
            .position(|s| s.id == active_session)
            .unwrap_or(0);
        Self {
            sessions,
            selected,
            mode: SessionDialogMode::Select,
            rename_buffer: String::new(),
            active_session,
        }
    }

    pub fn render(&self, width: usize, colour: bool) -> String {
        let width = width.max(MIN_WIDTH);
        let inner = width.saturating_sub(4);
        let rule = "─".repeat(width.saturating_sub(2));

        match self.mode {
            SessionDialogMode::Select => {
                let title = " SESSIONS ";
                let title_len = visible_len(title);
                let top_left = "─".repeat(2);
                let top_right = "─".repeat(width.saturating_sub(2 + 2 + title_len));
                let mut lines = vec![format!(
                    "{}{}{}{}",
                    paint(colour, sgr_border(), "╭"),
                    paint(colour, sgr_border(), &top_left),
                    paint(colour, BOLD, title),
                    paint(colour, sgr_border(), &format!("{top_right}╮")),
                )];

                if self.sessions.is_empty() {
                    lines.push(Self::box_line(
                        "  no recorded sessions found",
                        inner,
                        colour,
                        sgr_dim(),
                    ));
                } else {
                    for (idx, s) in self.sessions.iter().enumerate() {
                        let is_sel = idx == self.selected;
                        let is_active = s.id == self.active_session;
                        let radio = if is_sel { "(•)" } else { "( )" };
                        let active_tag = if is_active { " [active]" } else { "" };
                        let title_part = match &s.title {
                            Some(t) => format!(" · \"{t}\""),
                            None => String::new(),
                        };
                        let row_label =
                            format!("{radio} {}. {}{title_part}{active_tag}", idx + 1, s.id);
                        let sgr = if is_sel { sgr_accent() } else { sgr_dim() };
                        lines.push(Self::box_line(&row_label, inner, colour, sgr));
                        let detail = format!("     {} events · {}", s.events, s.last_seen);
                        lines.push(Self::box_line(&detail, inner, colour, sgr_dim()));
                    }
                }

                lines.push(Self::box_line("", inner, colour, ""));
                lines.push(Self::box_line(
                    "[↑/↓] Navigate  [Enter] Resume  [r] Rename  [d] Delete  [Esc] Cancel",
                    inner,
                    colour,
                    sgr_dim(),
                ));
                lines.push(paint(colour, sgr_border(), &format!("╰{rule}╯")));
                lines.join("\n")
            }
            SessionDialogMode::Rename => {
                let title = " RENAME SESSION ";
                let title_len = visible_len(title);
                let top_left = "─".repeat(2);
                let top_right = "─".repeat(width.saturating_sub(2 + 2 + title_len));
                let mut lines = vec![format!(
                    "{}{}{}{}",
                    paint(colour, sgr_border(), "╭"),
                    paint(colour, sgr_border(), &top_left),
                    paint(colour, BOLD, title),
                    paint(colour, sgr_border(), &format!("{top_right}╮")),
                )];

                if let Some(target) = self.sessions.get(self.selected) {
                    let sess_row = format!("Session: {}", target.id);
                    lines.push(Self::box_line(&sess_row, inner, colour, sgr_dim()));
                    if let Some(cur) = &target.title {
                        let cur_row = format!("Current: {cur}");
                        lines.push(Self::box_line(&cur_row, inner, colour, sgr_dim()));
                    }
                }
                lines.push(Self::box_line("", inner, colour, ""));
                let input_row = format!("New title: {}█", self.rename_buffer);
                lines.push(Self::box_line(&input_row, inner, colour, sgr_accent()));
                lines.push(Self::box_line("", inner, colour, ""));
                lines.push(Self::box_line(
                    "[Enter] Save Title  [Esc] Back to Session List",
                    inner,
                    colour,
                    sgr_dim(),
                ));
                lines.push(paint(colour, sgr_border(), &format!("╰{rule}╯")));
                lines.join("\n")
            }
            SessionDialogMode::ConfirmDelete => {
                let title = " DELETE SESSION ";
                let title_len = visible_len(title);
                let top_left = "─".repeat(2);
                let top_right = "─".repeat(width.saturating_sub(2 + 2 + title_len));
                let mut lines = vec![format!(
                    "{}{}{}{}",
                    paint(colour, sgr_border(), "╭"),
                    paint(colour, sgr_border(), &top_left),
                    paint(colour, BOLD, title),
                    paint(colour, sgr_border(), &format!("{top_right}╮")),
                )];

                if let Some(target) = self.sessions.get(self.selected) {
                    let msg = format!("Are you sure you want to delete session {}?", target.id);
                    lines.push(Self::box_line(&msg, inner, colour, sgr_err()));
                    if let Some(t) = &target.title {
                        let t_row = format!("Title: \"{t}\"");
                        lines.push(Self::box_line(&t_row, inner, colour, sgr_dim()));
                    }
                    lines.push(Self::box_line(
                        "This will permanently remove its recorded history and events.",
                        inner,
                        colour,
                        sgr_dim(),
                    ));
                }
                lines.push(Self::box_line("", inner, colour, ""));
                lines.push(Self::box_line(
                    "[y/Enter] Confirm Delete  [n/Esc] Cancel",
                    inner,
                    colour,
                    sgr_dim(),
                ));
                lines.push(paint(colour, sgr_border(), &format!("╰{rule}╯")));
                lines.join("\n")
            }
        }
    }

    fn box_line(content: &str, inner: usize, colour: bool, sgr: &str) -> String {
        let fitted = fit(content, inner);
        let pad = " ".repeat(inner.saturating_sub(visible_len(&fitted)));
        format!(
            "{} {}{pad} {}",
            paint(colour, sgr_border(), "│"),
            paint(colour, sgr, &fitted),
            paint(colour, sgr_border(), "│"),
        )
    }

    pub fn handle_key(&mut self, key: Key) -> Option<SessionAction> {
        match self.mode {
            SessionDialogMode::Select => match key {
                Key::Up => {
                    if !self.sessions.is_empty() {
                        if self.selected == 0 {
                            self.selected = self.sessions.len().saturating_sub(1);
                        } else {
                            self.selected -= 1;
                        }
                    }
                    None
                }
                Key::Down => {
                    if !self.sessions.is_empty() {
                        if self.selected + 1 >= self.sessions.len() {
                            self.selected = 0;
                        } else {
                            self.selected += 1;
                        }
                    }
                    None
                }
                Key::Enter | Key::Newline => {
                    if let Some(target) = self.sessions.get(self.selected) {
                        Some(SessionAction::Resume(target.id))
                    } else {
                        Some(SessionAction::Cancel)
                    }
                }
                Key::Char('r' | 'R') => {
                    if let Some(target) = self.sessions.get(self.selected) {
                        self.rename_buffer = target.title.clone().unwrap_or_default();
                        self.mode = SessionDialogMode::Rename;
                    }
                    None
                }
                Key::Char('d' | 'D') => {
                    if !self.sessions.is_empty() {
                        self.mode = SessionDialogMode::ConfirmDelete;
                    }
                    None
                }
                Key::Char(c) if c.is_ascii_digit() && c != '0' => {
                    let idx = (c as usize) - ('1' as usize);
                    if idx < self.sessions.len() {
                        self.selected = idx;
                        return Some(SessionAction::Resume(self.sessions[idx].id));
                    }
                    None
                }
                Key::Interrupt => Some(SessionAction::Cancel),
                _ => None,
            },
            SessionDialogMode::Rename => match key {
                Key::Enter | Key::Newline => {
                    let title = self.rename_buffer.trim().to_owned();
                    if let Some(target) = self.sessions.get(self.selected) {
                        Some(SessionAction::Rename(target.id, title))
                    } else {
                        self.mode = SessionDialogMode::Select;
                        None
                    }
                }
                Key::Char(c) => {
                    self.rename_buffer.push(c);
                    None
                }
                Key::Backspace => {
                    self.rename_buffer.pop();
                    None
                }
                Key::Interrupt => {
                    self.mode = SessionDialogMode::Select;
                    None
                }
                _ => None,
            },
            SessionDialogMode::ConfirmDelete => match key {
                Key::Enter | Key::Newline | Key::Char('y' | 'Y') => {
                    if let Some(target) = self.sessions.get(self.selected) {
                        Some(SessionAction::Delete(target.id))
                    } else {
                        self.mode = SessionDialogMode::Select;
                        None
                    }
                }
                Key::Char('n' | 'N') | Key::Interrupt => {
                    self.mode = SessionDialogMode::Select;
                    None
                }
                _ => None,
            },
        }
    }
}

pub fn resolve_session_answer(
    answer: &str,
    sessions: &[SessionChoice],
    current: SessionId,
) -> Result<SessionId, String> {
    let answer = answer.trim();
    if answer.is_empty() {
        return Ok(current);
    }
    if let Ok(number) = answer.parse::<usize>() {
        return match number.checked_sub(1).and_then(|idx| sessions.get(idx)) {
            Some(choice) => Ok(choice.id),
            None if sessions.is_empty() => Err("no sessions found".to_owned()),
            None => Err(format!("no session {number}; choose 1-{}", sessions.len())),
        };
    }
    if let Ok(id) = answer.parse::<SessionId>() {
        return Ok(id);
    }
    // Prefix search
    if let Some(choice) = sessions
        .iter()
        .find(|s| s.id.to_string().starts_with(answer))
    {
        return Ok(choice.id);
    }
    Err(format!("`{answer}` is not a valid session ID"))
}
