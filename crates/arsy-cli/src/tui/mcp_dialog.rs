//! The interactive `/mcp` dialog: move through the connections and turn one on
//! or off, or adopt one another tool declared.
use super::*;

/// One row of the dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpChoice {
    pub name: String,
    /// `arsy` for a connection ARSY defines, otherwise the tool that declared it.
    pub source: String,
    /// The layer an ARSY definition came from; empty for a declaration.
    pub trust: String,
    /// What a connection runs or reaches, shortened for the row.
    pub target: String,
    /// The whole command line or URL, shown before anything is adopted.
    pub detail: String,
    /// `None` while only another tool declares it: ARSY does not start a
    /// connection it has not been told to adopt.
    pub enabled: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpAction {
    /// Flip `enabled` on the ARSY definition at this row.
    Toggle(usize),
    /// Write the declaration at this row into ARSY's configuration.
    Adopt(usize),
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpDialogMode {
    Select,
    /// Adopting decides that a program may be started, so it is asked first.
    ConfirmAdopt,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpDialogState {
    pub choices: Vec<McpChoice>,
    pub selected: usize,
    pub mode: McpDialogMode,
    /// What the last action did, shown inside the frame rather than printed
    /// under it.
    pub notice: Option<String>,
}

impl McpDialogState {
    pub fn new(choices: Vec<McpChoice>) -> Self {
        Self {
            choices,
            selected: 0,
            mode: McpDialogMode::Select,
            notice: None,
        }
    }

    /// Replace the rows after a change, keeping the marker on the same name.
    pub fn reload(&mut self, choices: Vec<McpChoice>) {
        let name = self.choices.get(self.selected).map(|c| c.name.clone());
        self.selected = name
            .and_then(|name| choices.iter().position(|c| c.name == name))
            .unwrap_or(0);
        self.choices = choices;
        self.mode = McpDialogMode::Select;
    }

    pub fn render(&self, width: usize, colour: bool) -> String {
        let width = width.max(MIN_WIDTH);
        let inner = width.saturating_sub(4);
        let mut lines = Vec::new();
        match self.mode {
            McpDialogMode::Select => {
                lines.push(dialog_top(" MCP ", width, colour));
                if self.choices.is_empty() {
                    lines.push(dialog_line(
                        "  no MCP connection is defined or declared",
                        inner,
                        colour,
                        sgr_dim(),
                    ));
                }
                let name_width = self
                    .choices
                    .iter()
                    .map(|c| visible_len(&c.name))
                    .max()
                    .unwrap_or(0);
                for (index, choice) in self.choices.iter().enumerate() {
                    let marked = index == self.selected;
                    let state = match choice.enabled {
                        Some(true) => "● on ",
                        Some(false) => "○ off",
                        None => "◌ —  ",
                    };
                    let source = if choice.trust.is_empty() {
                        choice.source.clone()
                    } else {
                        format!("{} · {}", choice.source, choice.trust)
                    };
                    let row = format!(
                        "{} {state}  {:name_width$}  {source:<16}  {}",
                        if marked { "›" } else { " " },
                        choice.name,
                        choice.target,
                    );
                    let style = match (marked, choice.enabled) {
                        (true, _) => sgr_accent(),
                        (false, Some(true)) => "",
                        _ => sgr_dim(),
                    };
                    lines.push(dialog_line(&row, inner, colour, style));
                }
                lines.push(dialog_line("", inner, colour, ""));
                if let Some(notice) = &self.notice {
                    lines.push(dialog_line(notice, inner, colour, sgr_dim()));
                }
                lines.push(dialog_line(
                    "[↑/↓] Navigate  [Space/Enter] Toggle or adopt  [Esc] Close",
                    inner,
                    colour,
                    sgr_dim(),
                ));
            }
            McpDialogMode::ConfirmAdopt => {
                lines.push(dialog_top(" ADOPT MCP CONNECTION ", width, colour));
                if let Some(choice) = self.choices.get(self.selected) {
                    lines.push(dialog_line(
                        &format!(
                            "Adopt `{}` from {} into your user arsy.json, enabled?",
                            choice.name, choice.source
                        ),
                        inner,
                        colour,
                        sgr_accent(),
                    ));
                    lines.push(dialog_line(
                        &format!("It runs: {}", choice.detail),
                        inner,
                        colour,
                        "",
                    ));
                    lines.push(dialog_line(
                        "Environment variables and headers are not copied; a server that needs them will not start.",
                        inner,
                        colour,
                        sgr_dim(),
                    ));
                }
                lines.push(dialog_line("", inner, colour, ""));
                lines.push(dialog_line(
                    "[y/Enter] Adopt  [n/Esc] Back",
                    inner,
                    colour,
                    sgr_dim(),
                ));
            }
        }
        lines.push(paint(
            colour,
            sgr_border(),
            &format!("╰{}╯", "─".repeat(width.saturating_sub(2))),
        ));
        lines.join("\n")
    }

    pub fn handle_key(&mut self, key: Key) -> Option<McpAction> {
        match self.mode {
            McpDialogMode::Select => match key {
                Key::Up => {
                    self.step(false);
                    None
                }
                Key::Down => {
                    self.step(true);
                    None
                }
                Key::Char(' ') | Key::Enter | Key::Newline => {
                    match self.choices.get(self.selected)?.enabled {
                        Some(_) => Some(McpAction::Toggle(self.selected)),
                        None => {
                            self.mode = McpDialogMode::ConfirmAdopt;
                            None
                        }
                    }
                }
                Key::Interrupt | Key::Eof => Some(McpAction::Close),
                _ => None,
            },
            McpDialogMode::ConfirmAdopt => match key {
                Key::Char('y' | 'Y') | Key::Enter | Key::Newline => {
                    self.mode = McpDialogMode::Select;
                    Some(McpAction::Adopt(self.selected))
                }
                Key::Char('n' | 'N') | Key::Interrupt | Key::Eof => {
                    self.mode = McpDialogMode::Select;
                    None
                }
                _ => None,
            },
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

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(name: &str, enabled: Option<bool>) -> McpChoice {
        McpChoice {
            name: name.to_owned(),
            source: if enabled.is_some() { "arsy" } else { "claude" }.to_owned(),
            trust: String::new(),
            target: "server".to_owned(),
            detail: "server --stdio".to_owned(),
            enabled,
        }
    }

    #[test]
    fn space_toggles_a_definition_and_asks_before_adopting_a_declaration() {
        let mut dialog =
            McpDialogState::new(vec![choice("docs", Some(true)), choice("jira", None)]);
        assert_eq!(
            dialog.handle_key(Key::Char(' ')),
            Some(McpAction::Toggle(0))
        );

        // Up from the first row wraps to the last.
        assert_eq!(dialog.handle_key(Key::Up), None);
        assert_eq!(dialog.selected, 1);
        assert_eq!(
            dialog.handle_key(Key::Enter),
            None,
            "nothing is written yet"
        );
        assert_eq!(dialog.mode, McpDialogMode::ConfirmAdopt);
        assert!(dialog.render(80, false).contains("server --stdio"));

        // Backing out adopts nothing; confirming adopts the marked row.
        assert_eq!(dialog.handle_key(Key::Char('n')), None);
        assert_eq!(dialog.mode, McpDialogMode::Select);
        dialog.handle_key(Key::Enter);
        assert_eq!(dialog.handle_key(Key::Char('y')), Some(McpAction::Adopt(1)));

        assert_eq!(dialog.handle_key(Key::Interrupt), Some(McpAction::Close));
    }

    #[test]
    fn a_reload_keeps_the_marker_on_the_same_connection() {
        let mut dialog =
            McpDialogState::new(vec![choice("docs", Some(true)), choice("jira", None)]);
        dialog.selected = 1;
        dialog.reload(vec![
            choice("alpha", Some(true)),
            choice("docs", Some(true)),
            choice("jira", Some(true)),
        ]);
        assert_eq!(dialog.selected, 2);
        let frame = dialog.render(80, false);
        assert_eq!(frame.lines().count(), 1 + 3 + 1 + 1 + 1, "{frame}");
    }
}
