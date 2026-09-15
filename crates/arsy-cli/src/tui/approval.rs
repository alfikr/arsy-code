//! Approval and plan decision cards.
use super::*;
/// An option in the interactive Ask/Approval dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskOption {
    pub label: String,
    pub description: Option<String>,
}

/// Result of an interactive Ask/Approval dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AskDialogResult {
    Approve {
        note: Option<String>,
    },
    AlwaysApprove {
        note: Option<String>,
    },
    Deny {
        note: Option<String>,
    },
    /// Shift+Tab changes mode without submitting the draft.
    CycleMode,
    Cancel,
}

/// State for interactive Ask/Approval modal dialogs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskDialogState {
    pub title: String,
    pub summary: String,
    pub reason: String,
    pub diff_preview: Option<String>,
    pub options: Vec<AskOption>,
    pub(super) preview_offset: usize,
    pub(super) preview_height: usize,
    pub selected: usize,
    pub custom_note: String,
    pub editing_note: bool,
    plan_decision: bool,
}

impl AskDialogState {
    pub fn for_approval(
        name: &str,
        summary: &str,
        reason: &str,
        diff_preview: Option<String>,
    ) -> Self {
        Self {
            title: format!("APPROVAL REQUIRED: {name}"),
            summary: summary.to_owned(),
            reason: reason.to_owned(),
            diff_preview,
            options: vec![
                AskOption {
                    label: "Approve this call once (yes)".to_owned(),
                    description: Some("Execute this tool call and continue".to_owned()),
                },
                AskOption {
                    label: "Always approve for this session (auto)".to_owned(),
                    description: Some(
                        "Auto-approve this and all subsequent calls in this session".to_owned(),
                    ),
                },
                AskOption {
                    label: "Deny this call (no)".to_owned(),
                    description: Some("Decline this tool call and inform the agent".to_owned()),
                },
            ],
            selected: 0,
            custom_note: String::new(),
            preview_offset: 0,
            preview_height: 15,
            editing_note: false,
            plan_decision: false,
        }
    }
    pub fn for_plan(preview: impl Into<String>) -> Self {
        Self {
            title: "PLAN READY".to_owned(),
            summary: "Review the repository-aware plan before any implementation begins."
                .to_owned(),
            reason: "Plan Mode blocks workspace mutations until approval.".to_owned(),
            diff_preview: Some(preview.into()),
            options: vec![
                AskOption {
                    label: "Approve and implement".to_owned(),
                    description: Some("Enter acceptEdits mode and execute this plan".to_owned()),
                },
                AskOption {
                    label: "Continue planning / revise".to_owned(),
                    description: Some("Stay in Plan Mode and send the optional note".to_owned()),
                },
                AskOption {
                    label: "Cancel planning".to_owned(),
                    description: Some("Leave Plan Mode without implementing".to_owned()),
                },
            ],
            selected: 0,
            custom_note: String::new(),
            preview_offset: 0,
            preview_height: 15,
            editing_note: false,
            plan_decision: true,
        }
    }
    /// Limit the plan body to the rows the terminal can show, retaining every
    /// line for PageUp/PageDown navigation.
    pub fn set_preview_height(&mut self, rows: usize) {
        self.preview_height = rows.max(3);
        self.clamp_preview();
    }

    fn clamp_preview(&mut self) {
        let total = self
            .diff_preview
            .as_deref()
            .map_or(0, |preview| preview.lines().count());
        self.preview_offset = self
            .preview_offset
            .min(total.saturating_sub(self.preview_height));
    }

    fn scroll_preview(&mut self, down: bool) {
        let amount = self.preview_height.max(1);
        self.preview_offset = if down {
            self.preview_offset.saturating_add(amount)
        } else {
            self.preview_offset.saturating_sub(amount)
        };
        self.clamp_preview();
    }

    pub fn render(&self, width: usize, colour: bool) -> String {
        let width = width.max(MIN_WIDTH);
        let inner = width.saturating_sub(4);
        let rule = "─".repeat(width.saturating_sub(2));
        let title_disp = format!(" {} ", self.title);
        let title_len = visible_len(&title_disp);
        let top_left = "─".repeat(2);
        let top_right = "─".repeat(width.saturating_sub(2 + 2 + title_len));
        let mut lines = vec![format!(
            "{}{}{}{}",
            paint(colour, sgr_border(), "╭"),
            paint(colour, sgr_border(), &top_left),
            paint(colour, BOLD, &title_disp),
            paint(colour, sgr_border(), &format!("{top_right}╮")),
        )];

        if !self.summary.is_empty() {
            let row = format!("Summary: {}", self.summary);
            lines.push(Self::box_line(&row, inner, colour, sgr_dim()));
        }
        if !self.reason.is_empty() {
            let row = format!("Reason:  {}", self.reason);
            lines.push(Self::box_line(&row, inner, colour, sgr_dim()));
        }

        if let Some(diff) = &self.diff_preview {
            lines.push(Self::box_line("", inner, colour, ""));
            lines.push(Self::box_line(
                if self.plan_decision {
                    "Plan preview:"
                } else {
                    "Proposed Changes:"
                },
                inner,
                colour,
                sgr_accent(),
            ));
            let preview_lines: Vec<&str> = diff.lines().collect();
            let max_preview = self.preview_height.max(1);
            let start = self
                .preview_offset
                .min(preview_lines.len().saturating_sub(max_preview));
            for line in preview_lines.iter().skip(start).take(max_preview) {
                lines.push(Self::render_diff_line(line, inner, colour));
            }
            if preview_lines.len() > max_preview {
                let end = (start + max_preview).min(preview_lines.len());
                let more = if self.plan_decision {
                    format!(
                        "… lines {}-{} of {} · PgUp/PgDn scroll",
                        start + 1,
                        end,
                        preview_lines.len()
                    )
                } else {
                    format!("… ({} more lines omitted)", preview_lines.len() - end)
                };
                lines.push(Self::box_line(&more, inner, colour, sgr_dim()));
            }
        }

        lines.push(Self::box_line("", inner, colour, ""));

        for (idx, opt) in self.options.iter().enumerate() {
            let is_sel = idx == self.selected;
            let radio = if is_sel { "(•)" } else { "( )" };
            let opt_num = idx + 1;
            let label_part = format!("{radio} {opt_num}. {}", opt.label);
            let sgr = if is_sel { sgr_accent() } else { sgr_dim() };
            lines.push(Self::box_line(&label_part, inner, colour, sgr));
            if let Some(desc) = &opt.description {
                let desc_part = format!("     {desc}");
                lines.push(Self::box_line(&desc_part, inner, colour, sgr_dim()));
            }
        }

        if self.editing_note || !self.custom_note.is_empty() {
            lines.push(Self::box_line("", inner, colour, ""));
            let note_display = if self.editing_note {
                format!("Note: {}█", self.custom_note)
            } else {
                format!("Note: {}", self.custom_note)
            };
            lines.push(Self::box_line(
                &note_display,
                inner,
                colour,
                sgr_assistant(),
            ));
        }

        lines.push(Self::box_line("", inner, colour, ""));
        let hint = if self.editing_note {
            "[Enter] Done Note  [Esc] Clear Note"
        } else if self.plan_decision && self.custom_note.is_empty() {
            "[↑/↓] Navigate  [PgUp/PgDn] Scroll plan  [1-3] Choose  [e] Add Note  [i] Implement  [r] Revise  [c] Cancel"
        } else if self.plan_decision {
            "[↑/↓] Navigate  [PgUp/PgDn] Scroll plan  [1-3] Choose  [e] Edit Note  [i] Implement  [r] Revise  [c] Cancel"
        } else if self.custom_note.is_empty() {
            "[↑/↓] Navigate  [1-3] Choose  [n] Add Note  [y] Yes  [a] Auto  [d] Deny  [Enter] Confirm"
        } else {
            "[↑/↓] Navigate  [1-3] Choose  [n] Edit Note  [y] Yes  [a] Auto  [d] Deny  [Enter] Confirm"
        };
        lines.push(Self::box_line(hint, inner, colour, sgr_dim()));
        lines.push(paint(colour, sgr_border(), &format!("╰{rule}╯")));
        lines.join("\n")
    }

    fn render_diff_line(line: &str, inner: usize, colour: bool) -> String {
        let fitted = fit(line, inner);
        let pad = " ".repeat(inner.saturating_sub(visible_len(&fitted)));
        if !colour {
            return format!("│ {fitted}{pad} │");
        }
        if line.starts_with('+') {
            let text = format!("\x1b[38;2;120;225;145m\x1b[48;2;25;50;35m{fitted}\x1b[0m");
            format!(
                "{} {text}{pad} {}",
                paint(true, sgr_border(), "│"),
                paint(true, sgr_border(), "│")
            )
        } else if line.starts_with('-') {
            let text = format!("\x1b[38;2;255;120;135m\x1b[48;2;55;25;30m{fitted}\x1b[0m");
            format!(
                "{} {text}{pad} {}",
                paint(true, sgr_border(), "│"),
                paint(true, sgr_border(), "│")
            )
        } else if line.starts_with('@') || line.starts_with('[') || line.starts_with('$') {
            format!(
                "{} {}{pad} {}",
                paint(true, sgr_border(), "│"),
                paint(true, sgr_accent(), &fitted),
                paint(true, sgr_border(), "│")
            )
        } else {
            format!(
                "{} {}{pad} {}",
                paint(true, sgr_border(), "│"),
                paint(true, sgr_dim(), &fitted),
                paint(true, sgr_border(), "│")
            )
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

    fn current_note(&self) -> Option<String> {
        let trimmed = self.custom_note.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    }

    pub fn handle_key(&mut self, key: Key) -> Option<AskDialogResult> {
        if self.editing_note {
            match key {
                Key::Enter | Key::Newline => {
                    self.editing_note = false;
                    return None;
                }
                Key::Char(c) => {
                    self.custom_note.push(c);
                    return None;
                }
                Key::Backspace => {
                    self.custom_note.pop();
                    return None;
                }
                Key::Interrupt => {
                    self.editing_note = false;
                    self.custom_note.clear();
                    return None;
                }
                _ => return None,
            }
        }
        if self.plan_decision {
            match key {
                Key::PageUp => {
                    self.scroll_preview(false);
                    return None;
                }
                Key::PageDown => {
                    self.scroll_preview(true);
                    return None;
                }
                Key::Home => {
                    self.preview_offset = 0;
                    return None;
                }
                Key::End => {
                    self.preview_offset = usize::MAX;
                    self.clamp_preview();
                    return None;
                }
                Key::CycleMode => return Some(AskDialogResult::CycleMode),
                _ => {}
            }
        }

        match key {
            Key::Up => {
                if self.selected == 0 {
                    self.selected = self.options.len().saturating_sub(1);
                } else {
                    self.selected -= 1;
                }
                None
            }
            Key::Down => {
                if self.selected + 1 >= self.options.len() {
                    self.selected = 0;
                } else {
                    self.selected += 1;
                }
                None
            }
            Key::Char('e' | 'E') if self.plan_decision => {
                self.editing_note = true;
                None
            }
            Key::Char('n' | 'N') if !self.plan_decision => {
                self.editing_note = true;
                None
            }
            Key::Char('i' | 'I') if self.plan_decision => Some(AskDialogResult::Approve {
                note: self.current_note(),
            }),
            Key::Char('r' | 'R') if self.plan_decision => Some(AskDialogResult::AlwaysApprove {
                note: self.current_note(),
            }),
            Key::Char('c' | 'C') if self.plan_decision => Some(AskDialogResult::Deny {
                note: self.current_note(),
            }),
            Key::Char('1' | 'y' | 'Y') => Some(AskDialogResult::Approve {
                note: self.current_note(),
            }),
            Key::Char('2' | 'a' | 'A') => Some(AskDialogResult::AlwaysApprove {
                note: self.current_note(),
            }),
            Key::Char('3' | 'd' | 'D') => Some(AskDialogResult::Deny {
                note: self.current_note(),
            }),
            Key::Enter | Key::Newline | Key::Char(' ') => match self.selected {
                0 => Some(AskDialogResult::Approve {
                    note: self.current_note(),
                }),
                1 => Some(AskDialogResult::AlwaysApprove {
                    note: self.current_note(),
                }),
                2 => Some(AskDialogResult::Deny {
                    note: self.current_note(),
                }),
                _ => Some(AskDialogResult::Approve {
                    note: self.current_note(),
                }),
            },
            Key::Interrupt => Some(AskDialogResult::Cancel),
            _ => None,
        }
    }
}
