//! The interactive `/hooks` dialog: move through the lifecycle hooks that are
//! declared, and switch one off or back on.
//!
//! Switching a hook off is ARSY's own record in the operator's `arsy.json`
//! (`hook.disabled`), for the same reason `/mcp` writes `arsy.json` and never
//! another tool's settings file: a file the operator did not write is not a
//! file ARSY rewrites. The engine reads that record when it builds the rules,
//! so a declaration listed as off here really does not run.
use super::*;

/// One row of the dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookChoice {
    /// The key `hook.disabled` holds for this declaration. This is what a
    /// toggle writes, so it must be the key the engine builds for the same
    /// handler.
    pub declaration: String,
    /// The lifecycle the hook runs on, as it is spelled in the file.
    pub event: String,
    /// What the hook runs, shortened for the row.
    pub matcher: String,
    /// The file that declared it, with the tool that owns it.
    pub source: String,
    /// Whether the hook runs: off means a `hook.disabled` entry exists for it.
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookAction {
    /// Flip `hook.disabled` on the declaration at this row.
    Toggle(usize),
    Close,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookDialogState {
    pub choices: Vec<HookChoice>,
    pub selected: usize,
    /// What the last action did, shown inside the frame rather than printed
    /// under it.
    pub notice: Option<String>,
}

impl HookDialogState {
    pub fn new(choices: Vec<HookChoice>) -> Self {
        Self {
            choices,
            selected: 0,
            notice: None,
        }
    }

    /// Replace the rows after a change, keeping the marker on the same
    /// declaration.
    pub fn reload(&mut self, choices: Vec<HookChoice>) {
        let declaration = self
            .choices
            .get(self.selected)
            .map(|choice| choice.declaration.clone());
        self.selected = declaration
            .and_then(|declaration| {
                choices
                    .iter()
                    .position(|choice| choice.declaration == declaration)
            })
            .unwrap_or(0);
        self.choices = choices;
    }

    pub fn render(&self, width: usize, colour: bool) -> String {
        let width = width.max(MIN_WIDTH);
        let inner = width.saturating_sub(4);
        let mut lines = vec![dialog_top(" HOOKS ", width, colour)];
        if self.choices.is_empty() {
            lines.push(dialog_line(
                "  no lifecycle hook is declared",
                inner,
                colour,
                sgr_dim(),
            ));
        }
        let width_of = |column: usize| {
            self.choices
                .iter()
                .map(|choice| visible_len(&hook_cell(choice, column)))
                .max()
                .unwrap_or(0)
        };
        let (event_width, matcher_width) = (width_of(0), width_of(1));
        lines.extend(self.choices.iter().enumerate().map(|(index, choice)| {
            let marked = index == self.selected;
            let style = match (marked, choice.enabled) {
                (true, _) => sgr_accent(),
                (false, true) => "",
                _ => sgr_dim(),
            };
            dialog_line(
                &hook_row(choice, marked, event_width, matcher_width),
                inner,
                colour,
                style,
            )
        }));
        lines.push(dialog_line("", inner, colour, ""));
        if let Some(notice) = &self.notice {
            lines.push(dialog_line(notice, inner, colour, sgr_dim()));
        }
        lines.push(dialog_line(
            "[↑/↓] Navigate  [Space/Enter] Toggle  [Esc] Close",
            inner,
            colour,
            sgr_dim(),
        ));
        lines.push(paint(
            colour,
            sgr_border(),
            &format!("╰{}╯", "─".repeat(width.saturating_sub(2))),
        ));
        lines.join("\n")
    }

    pub fn handle_key(&mut self, key: Key) -> Option<HookAction> {
        match key {
            Key::Up => {
                self.step(false);
                None
            }
            Key::Down => {
                self.step(true);
                None
            }
            Key::Char(' ') | Key::Enter | Key::Newline => self
                .choices
                .get(self.selected)
                .is_some()
                .then_some(HookAction::Toggle(self.selected)),
            Key::Interrupt | Key::Eof => Some(HookAction::Close),
            _ => None,
        }
    }

    /// Move the marker one row, wrapping at either end.
    fn step(&mut self, forward: bool) {
        let count = self.choices.len();
        if count == 0 {
            return;
        }
        self.selected = if forward {
            (self.selected + 1) % count
        } else {
            (self.selected + count - 1) % count
        };
    }
}

/// One column of a row, named so the two width calls cannot drift apart.
fn hook_cell(choice: &HookChoice, column: usize) -> String {
    match column {
        0 => choice.event.clone(),
        1 => choice.matcher.clone(),
        _ => choice.source.clone(),
    }
}

/// One hook as a row: marker, state, event, matcher, where it came from.
fn hook_row(choice: &HookChoice, marked: bool, event_width: usize, matcher_width: usize) -> String {
    let event = format!("{:<event_width$}", hook_cell(choice, 0));
    let matcher = format!("{:<matcher_width$}", hook_cell(choice, 1));
    format!(
        "{} {}  {event}  {matcher}  {}",
        if marked { "›" } else { " " },
        if choice.enabled { "● on " } else { "○ off" },
        choice.source,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(declaration: &str, enabled: bool) -> HookChoice {
        HookChoice {
            declaration: declaration.to_owned(),
            event: "PreToolUse".to_owned(),
            matcher: "Bash".to_owned(),
            source: "claude · user".to_owned(),
            enabled,
        }
    }

    #[test]
    fn space_toggles_a_declaration_and_up_wraps_to_the_last() {
        let mut dialog = HookDialogState::new(vec![choice("a", true), choice("b", true)]);
        assert_eq!(
            dialog.handle_key(Key::Char(' ')),
            Some(HookAction::Toggle(0))
        );
        assert_eq!(dialog.handle_key(Key::Up), None);
        assert_eq!(dialog.selected, 1);
        assert_eq!(dialog.handle_key(Key::Enter), Some(HookAction::Toggle(1)));
        assert_eq!(dialog.handle_key(Key::Interrupt), Some(HookAction::Close));
    }

    #[test]
    fn the_rows_name_what_each_hook_runs_and_where_it_came_from() {
        let dialog = HookDialogState::new(vec![choice("a", false)]);
        let frame = dialog.render(80, false);
        assert!(frame.contains(" HOOKS "), "{frame}");
        assert!(frame.contains("PreToolUse"), "{frame}");
        assert!(frame.contains("Bash"), "{frame}");
        assert!(frame.contains("claude · user"), "{frame}");
        assert!(frame.contains("○ off"), "{frame}");
        // The empty case says so rather than drawing an empty frame.
        let empty = HookDialogState::new(Vec::new()).render(80, false);
        assert!(empty.contains("no lifecycle hook is declared"), "{empty}");
    }

    #[test]
    fn a_reload_keeps_the_marker_on_the_same_declaration() {
        let mut dialog = HookDialogState::new(vec![choice("a", true), choice("b", true)]);
        dialog.selected = 1;
        dialog.reload(vec![
            choice("a", true),
            choice("b", false),
            choice("c", true),
        ]);
        assert_eq!(dialog.selected, 1);
        assert!(dialog.render(80, false).contains("○ off"));
    }
}
