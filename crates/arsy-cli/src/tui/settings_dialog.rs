//! The interactive `/settings` dialog: move through the settings the registry
//! names, edit one, or put one back to its default.
//!
//! The registry lives in the kernel (`config::SETTINGS`), so what this dialog
//! offers, what it validates a typed value against, and what the loader will
//! accept when it next reads the file are the same table. A value this dialog
//! refuses would otherwise be written and only rejected on the next start.
//!
//! Writes go to the operator's own `arsy.json` through `write_config`, the one
//! funnel every configuration write uses, so a workspace or enterprise layer
//! still outranks what is set here.
use super::*;

/// One row of the dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingRow {
    /// The dotted key, as it is written in `arsy.json`.
    pub key: String,
    /// The value in effect: what a layer set, or the built-in default.
    pub value: String,
    pub default: String,
    pub description: String,
    /// The values a `Choice` accepts; empty for every other kind.
    pub choices: Vec<String>,
    /// Whether a layer set this key, as opposed to it standing at default.
    pub set: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsAction {
    /// Edit the row at this index.
    Edit(usize),
    /// Remove the row's key, so it stands at its built-in default.
    Reset(usize),
    Close,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsDialogState {
    pub rows: Vec<SettingRow>,
    pub selected: usize,
    /// What the last action did, shown inside the frame rather than printed
    /// under it.
    pub notice: Option<String>,
}

impl SettingsDialogState {
    pub fn new(rows: Vec<SettingRow>) -> Self {
        Self {
            rows,
            selected: 0,
            notice: None,
        }
    }

    /// Replace the rows after a change, keeping the marker on the same key.
    pub fn reload(&mut self, rows: Vec<SettingRow>) {
        let key = self.rows.get(self.selected).map(|row| row.key.clone());
        self.selected = key
            .and_then(|key| rows.iter().position(|row| row.key == key))
            .unwrap_or(0);
        self.rows = rows;
    }

    pub fn render(&self, width: usize, colour: bool) -> String {
        let width = width.max(MIN_WIDTH);
        let inner = width.saturating_sub(4);
        let mut lines = vec![dialog_top(" SETTINGS ", width, colour)];
        if self.rows.is_empty() {
            lines.push(dialog_line(
                "  no setting can be written by this build",
                inner,
                colour,
                sgr_dim(),
            ));
        }
        let key_width = self
            .rows
            .iter()
            .map(|row| visible_len(&row.key))
            .max()
            .unwrap_or(0);
        lines.extend(self.rows.iter().enumerate().map(|(index, row)| {
            let marked = index == self.selected;
            let style = match (marked, row.set) {
                (true, _) => sgr_accent(),
                (false, true) => "",
                _ => sgr_dim(),
            };
            dialog_line(&setting_row(row, marked, key_width), inner, colour, style)
        }));
        lines.push(dialog_line("", inner, colour, ""));
        if let Some(notice) = &self.notice {
            lines.push(dialog_line(notice, inner, colour, sgr_dim()));
        }
        lines.push(dialog_line(
            "[↑/↓] Navigate  [Enter/e] Edit  [r] Reset  [Esc] Close",
            inner,
            colour,
            sgr_dim(),
        ));
        lines.push(dialog_line(
            "a setting takes effect when ARSY starts again",
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

    pub fn handle_key(&mut self, key: Key) -> Option<SettingsAction> {
        match key {
            Key::Up => {
                self.step(false);
                None
            }
            Key::Down => {
                self.step(true);
                None
            }
            Key::Char('e' | 'E') | Key::Enter | Key::Newline => self
                .rows
                .get(self.selected)
                .is_some()
                .then_some(SettingsAction::Edit(self.selected)),
            Key::Char('r' | 'R') => self
                .rows
                .get(self.selected)
                .is_some()
                .then_some(SettingsAction::Reset(self.selected)),
            Key::Interrupt | Key::Eof => Some(SettingsAction::Close),
            _ => None,
        }
    }

    /// Move the marker one row, wrapping at either end.
    fn step(&mut self, forward: bool) {
        let count = self.rows.len();
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

/// What the composer asks while a value is being typed.
///
/// A `Choice` spells its options, because an accepted typo would otherwise be
/// the only thing between the operator and a configuration that fails to load.
pub fn setting_prompt(row: &SettingRow) -> String {
    let options = if row.choices.is_empty() {
        String::new()
    } else {
        format!(" ({})", row.choices.join(" | "))
    };
    format!(
        "  {} {}{} — Enter to set, Esc to cancel\n",
        paint(false, sgr_accent(), &row.key),
        paint(false, sgr_dim(), &row.value),
        paint(false, sgr_dim(), &options),
    )
}

/// One setting as a row: marker, key, value, and where it came from.
fn setting_row(row: &SettingRow, marked: bool, key_width: usize) -> String {
    let key = format!("{:<key_width$}", row.key);
    let origin = if row.set { "set" } else { "default" };
    format!(
        "{} {key}  {}  {origin:<7}  {}",
        if marked { "›" } else { " " },
        row.value,
        row.description,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(key: &str, value: &str, set: bool) -> SettingRow {
        SettingRow {
            key: key.to_owned(),
            value: value.to_owned(),
            default: "modern".to_owned(),
            description: "how an interactive transcript is drawn".to_owned(),
            choices: vec!["modern".to_owned(), "classic".to_owned()],
            set,
        }
    }

    #[test]
    fn enter_edits_r_resets_and_esc_closes() {
        let mut dialog = SettingsDialogState::new(vec![
            row("ui.style", "modern", true),
            row("credentials.store", "file", false),
        ]);
        assert_eq!(dialog.handle_key(Key::Enter), Some(SettingsAction::Edit(0)));
        assert_eq!(
            dialog.handle_key(Key::Char('r')),
            Some(SettingsAction::Reset(0))
        );
        assert_eq!(dialog.handle_key(Key::Down), None);
        assert_eq!(dialog.selected, 1);
        assert_eq!(dialog.handle_key(Key::Up), None);
        assert_eq!(dialog.selected, 0);
        assert_eq!(
            dialog.handle_key(Key::Interrupt),
            Some(SettingsAction::Close)
        );
    }

    #[test]
    fn the_rows_name_the_key_the_value_and_whether_a_layer_set_it() {
        let dialog = SettingsDialogState::new(vec![row("ui.style", "classic", true)]);
        let frame = dialog.render(80, false);
        assert!(frame.contains(" SETTINGS "), "{frame}");
        assert!(frame.contains("ui.style"), "{frame}");
        assert!(frame.contains("classic"), "{frame}");
        assert!(frame.contains("set"), "{frame}");
        let empty = SettingsDialogState::new(Vec::new()).render(80, false);
        assert!(empty.contains("no setting can be written"), "{empty}");
    }

    #[test]
    fn the_edit_prompt_names_the_choices_a_choice_takes() {
        let prompt = setting_prompt(&row("ui.style", "modern", true));
        assert!(prompt.contains("modern | classic"), "{prompt}");
        assert!(prompt.contains("Esc to cancel"), "{prompt}");
    }

    #[test]
    fn a_reload_keeps_the_marker_on_the_same_key() {
        let mut dialog = SettingsDialogState::new(vec![
            row("ui.style", "modern", true),
            row("credentials.store", "file", false),
        ]);
        dialog.selected = 1;
        dialog.reload(vec![row("credentials.store", "file", true)]);
        assert_eq!(dialog.selected, 0);
        assert!(dialog.render(80, false).contains("set"));
    }
}
