//! The interactive `/settings` dialog: move through the settings the registry
//! names, edit one with the arrow keys, or put one back to its default.
//!
//! The registry lives in the kernel (`config::SETTINGS`), so what this dialog
//! offers and what the loader will accept when it next reads the file are the
//! same table. Editing never asks the operator to type: the arrows move the
//! pending value through the values the kind allows, and Enter writes the one
//! the row shows. A value this dialog offers is therefore always one the
//! loader takes.
//!
//! Writes go to the operator's own `arsy.json` through `write_config`, the one
//! funnel every configuration write uses, so a workspace or enterprise layer
//! still outranks what is set here.
use super::*;

/// The value shape of one setting, as this dialog edits it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingKind {
    /// Any non-empty text. Nothing to cycle, so this dialog does not edit it;
    /// the row says where it can be set instead.
    Text,
    /// `true` or `false`.
    Bool,
    /// One of the row's `choices`.
    Choice,
    /// A whole number between the two bounds, inclusive.
    Integer { min: usize, max: usize },
}

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
    /// How this dialog edits the row.
    pub kind: SettingKind,
    /// Whether a layer set this key, as opposed to it standing at default.
    pub set: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettingsAction {
    /// Set the row at this index to the pending value the arrows chose.
    Apply(usize, String),
    /// Remove the row's key, so it stands at its built-in default.
    Reset(usize),
    Close,
}

/// A value being chosen for one row, inside the dialog rather than typed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Editing {
    /// The row the pending value belongs to.
    pub index: usize,
    /// The value Enter would set.
    pub pending: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsDialogState {
    pub rows: Vec<SettingRow>,
    pub selected: usize,
    /// The row being edited, and the value Enter would set for it.
    pub editing: Option<Editing>,
    /// What the last action did, shown inside the frame rather than printed
    /// under it.
    pub notice: Option<String>,
}

impl SettingsDialogState {
    pub fn new(rows: Vec<SettingRow>) -> Self {
        Self {
            rows,
            selected: 0,
            editing: None,
            notice: None,
        }
    }

    /// Replace the rows after a change, keeping the marker on the same key.
    ///
    /// The change is written, so the value being chosen is settled either way:
    /// the edit ends and the marker stands on the key in the fresh rows.
    pub fn reload(&mut self, rows: Vec<SettingRow>) {
        let key = self.rows.get(self.selected).map(|row| row.key.clone());
        self.selected = key
            .and_then(|key| rows.iter().position(|row| row.key == key))
            .unwrap_or(0);
        self.rows = rows;
        self.editing = None;
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
            let pending = self
                .editing
                .as_ref()
                .filter(|edit| edit.index == index)
                .map(|edit| edit.pending.as_str());
            let style = match (marked, row.set) {
                (true, _) => sgr_accent(),
                (false, true) => "",
                _ => sgr_dim(),
            };
            dialog_line(
                &setting_row(row, marked, key_width, pending),
                inner,
                colour,
                style,
            )
        }));
        if let Some(hint) = self.choices_hint() {
            lines.push(dialog_line(&hint, inner, colour, sgr_dim()));
        }
        lines.push(dialog_line("", inner, colour, ""));
        if let Some(notice) = &self.notice {
            lines.push(dialog_line(notice, inner, colour, sgr_dim()));
        }
        let footer = if self.editing.is_some() {
            "[←/→/↑/↓] Change  [Enter] Set  [r] Reset  [Esc] Close"
        } else {
            "[↑/↓] Navigate  [Enter/e] Edit  [r] Reset  [Esc] Close"
        };
        lines.push(dialog_line(footer, inner, colour, sgr_dim()));
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
        if self.editing.is_some() {
            return self.handle_edit_key(key);
        }
        match key {
            Key::Up => {
                self.step(false);
                None
            }
            Key::Down => {
                self.step(true);
                None
            }
            Key::Char('e' | 'E') | Key::Enter | Key::Newline => self.begin_edit(),
            Key::Char('r' | 'R') => self
                .rows
                .get(self.selected)
                .is_some()
                .then_some(SettingsAction::Reset(self.selected)),
            Key::Interrupt | Key::Eof => Some(SettingsAction::Close),
            _ => None,
        }
    }

    /// One keystroke while a value is being chosen. The arrows move the
    /// pending value through what the kind allows, Enter sets it, and escape
    /// backs out leaving the row as it was.
    fn handle_edit_key(&mut self, key: Key) -> Option<SettingsAction> {
        let index = self.editing.as_ref()?.index;
        let kind = self.rows.get(index)?.kind;
        match (key, kind) {
            (Key::Left | Key::Right, SettingKind::Choice) => {
                self.cycle_pending(index, key == Key::Right);
                None
            }
            (Key::Left | Key::Right, SettingKind::Bool) => {
                self.flip_pending(index);
                None
            }
            (Key::Up | Key::Down | Key::Left | Key::Right, SettingKind::Integer { min, max }) => {
                self.step_pending(index, min, max, matches!(key, Key::Up | Key::Right));
                None
            }
            (Key::Enter | Key::Newline, _) => {
                let pending = self.editing.as_ref()?.pending.clone();
                Some(SettingsAction::Apply(index, pending))
            }
            (Key::Char('r' | 'R'), _) => Some(SettingsAction::Reset(index)),
            // Escape backs out of the edit; a hung-up keyboard closes the
            // dialog rather than holding the session on it.
            (Key::Interrupt, _) => {
                self.editing = None;
                None
            }
            (Key::Eof, _) => Some(SettingsAction::Close),
            _ => None,
        }
    }

    /// Start choosing a value for the marked row. A `Text` row has nothing to
    /// cycle, so the dialog says where it can be set instead of pretending.
    fn begin_edit(&mut self) -> Option<SettingsAction> {
        let row = self.rows.get(self.selected)?;
        match row.kind {
            SettingKind::Text => {
                self.notice = Some(format!("`{}` is free text; set it in arsy.json", row.key));
                None
            }
            _ => {
                self.editing = Some(Editing {
                    index: self.selected,
                    pending: row.value.clone(),
                });
                None
            }
        }
    }

    /// Move a `Choice` row's pending value one option on, wrapping at either
    /// end.
    fn cycle_pending(&mut self, index: usize, forward: bool) {
        let Some(edit) = self.editing.as_mut() else {
            return;
        };
        let choices = &self.rows[index].choices;
        if choices.is_empty() {
            return;
        }
        let at = choices
            .iter()
            .position(|choice| *choice == edit.pending)
            .unwrap_or(0);
        edit.pending = if forward {
            choices[(at + 1) % choices.len()].clone()
        } else {
            choices[(at + choices.len() - 1) % choices.len()].clone()
        };
    }

    /// Move a `Bool` row's pending value to the other side; the wrap of a
    /// two-value cycle is the same step either way.
    fn flip_pending(&mut self, index: usize) {
        let Some(edit) = self.editing.as_mut() else {
            return;
        };
        let _ = index;
        edit.pending = if edit.pending == "true" {
            "false".to_owned()
        } else {
            "true".to_owned()
        };
    }

    /// Step an `Integer` row's pending value by one, held inside its bounds.
    fn step_pending(&mut self, index: usize, min: usize, max: usize, up: bool) {
        let Some(edit) = self.editing.as_mut() else {
            return;
        };
        let _ = index;
        let current = edit.pending.parse::<usize>().unwrap_or(min);
        edit.pending = if up {
            current.saturating_add(1).min(max)
        } else {
            current.saturating_sub(1).max(min)
        }
        .to_string();
    }

    /// The values the row being edited accepts, when there is a fixed set to
    /// name.
    fn choices_hint(&self) -> Option<String> {
        let edit = self.editing.as_ref()?;
        let row = self.rows.get(edit.index)?;
        let hint = match row.kind {
            SettingKind::Choice if !row.choices.is_empty() => {
                format!("one of {}", row.choices.join(" | "))
            }
            SettingKind::Bool => "true or false".to_owned(),
            SettingKind::Integer { min, max } => format!("from {min} to {max}"),
            SettingKind::Choice | SettingKind::Text => return None,
        };
        Some(format!("  {hint}"))
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

/// One setting as a row: marker, key, value, and where it came from. While the
/// row is being edited, the value shown is the one Enter would set.
fn setting_row(row: &SettingRow, marked: bool, key_width: usize, pending: Option<&str>) -> String {
    let key = format!("{:<key_width$}", row.key);
    let origin = if row.set { "set" } else { "default" };
    let value = match pending {
        Some(pending) if pending != row.value => format!("{} → {pending}", row.value),
        _ => row.value.clone(),
    };
    format!(
        "{} {key}  {value}  {origin:<7}  {}",
        if marked { "›" } else { " " },
        row.description,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice_row(key: &str, value: &str, set: bool) -> SettingRow {
        SettingRow {
            key: key.to_owned(),
            value: value.to_owned(),
            default: "modern".to_owned(),
            description: "how an interactive transcript is drawn".to_owned(),
            choices: vec!["modern".to_owned(), "classic".to_owned()],
            kind: SettingKind::Choice,
            set,
        }
    }

    fn bool_row(value: &str) -> SettingRow {
        SettingRow {
            key: "compat.omp.enabled".to_owned(),
            value: value.to_owned(),
            default: "true".to_owned(),
            description: "read OMP's files as a lower layer".to_owned(),
            choices: Vec::new(),
            kind: SettingKind::Bool,
            set: false,
        }
    }

    fn integer_row(value: &str) -> SettingRow {
        SettingRow {
            key: "execution.max_parallel".to_owned(),
            value: value.to_owned(),
            default: "8".to_owned(),
            description: "how many tools may run at once".to_owned(),
            choices: Vec::new(),
            kind: SettingKind::Integer { min: 1, max: 3 },
            set: false,
        }
    }

    #[test]
    fn enter_begins_editing_r_resets_and_esc_closes() {
        let mut dialog = SettingsDialogState::new(vec![
            choice_row("ui.style", "modern", true),
            choice_row("credentials.store", "file", false),
        ]);
        assert_eq!(dialog.handle_key(Key::Enter), None);
        assert_eq!(
            dialog.editing,
            Some(Editing {
                index: 0,
                pending: "modern".to_owned()
            })
        );
        dialog.editing = None;
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
        let dialog = SettingsDialogState::new(vec![choice_row("ui.style", "classic", true)]);
        let frame = dialog.render(80, false);
        assert!(frame.contains(" SETTINGS "), "{frame}");
        assert!(frame.contains("ui.style"), "{frame}");
        assert!(frame.contains("classic"), "{frame}");
        assert!(frame.contains("set"), "{frame}");
        let empty = SettingsDialogState::new(Vec::new()).render(80, false);
        assert!(empty.contains("no setting can be written"), "{empty}");
    }

    #[test]
    fn the_arrows_cycle_a_choice_with_wrap() {
        let mut dialog = SettingsDialogState::new(vec![choice_row("ui.style", "modern", true)]);
        dialog.handle_key(Key::Enter);
        dialog.handle_key(Key::Right);
        assert_eq!(dialog.editing.as_ref().unwrap().pending, "classic");
        dialog.handle_key(Key::Right);
        assert_eq!(dialog.editing.as_ref().unwrap().pending, "modern");
        dialog.handle_key(Key::Left);
        assert_eq!(dialog.editing.as_ref().unwrap().pending, "classic");
        // While a value is being chosen, the row shows the one Enter would
        // set, and the footer names the arrows.
        let frame = dialog.render(80, false);
        assert!(frame.contains("modern → classic"), "{frame}");
        assert!(frame.contains("[←/→/↑/↓] Change"), "{frame}");
        assert!(frame.contains("one of modern | classic"), "{frame}");
    }

    #[test]
    fn a_bool_flips_between_true_and_false() {
        let mut dialog = SettingsDialogState::new(vec![bool_row("true")]);
        dialog.handle_key(Key::Enter);
        dialog.handle_key(Key::Right);
        assert_eq!(dialog.editing.as_ref().unwrap().pending, "false");
        dialog.handle_key(Key::Left);
        assert_eq!(dialog.editing.as_ref().unwrap().pending, "true");
        let frame = dialog.render(80, false);
        assert!(frame.contains("true or false"), "{frame}");
    }

    #[test]
    fn an_integer_steps_within_its_bounds() {
        let mut dialog = SettingsDialogState::new(vec![integer_row("2")]);
        dialog.handle_key(Key::Enter);
        dialog.handle_key(Key::Right);
        assert_eq!(dialog.editing.as_ref().unwrap().pending, "3");
        // Held at the maximum.
        dialog.handle_key(Key::Up);
        assert_eq!(dialog.editing.as_ref().unwrap().pending, "3");
        dialog.handle_key(Key::Left);
        assert_eq!(dialog.editing.as_ref().unwrap().pending, "2");
        dialog.handle_key(Key::Down);
        assert_eq!(dialog.editing.as_ref().unwrap().pending, "1");
        // Held at the minimum.
        dialog.handle_key(Key::Left);
        assert_eq!(dialog.editing.as_ref().unwrap().pending, "1");
        let frame = dialog.render(80, false);
        assert!(frame.contains("from 1 to 3"), "{frame}");
    }

    #[test]
    fn enter_applies_the_pending_value() {
        let mut dialog = SettingsDialogState::new(vec![choice_row("ui.style", "modern", true)]);
        dialog.handle_key(Key::Enter);
        dialog.handle_key(Key::Right);
        assert_eq!(
            dialog.handle_key(Key::Enter),
            Some(SettingsAction::Apply(0, "classic".to_owned()))
        );
    }

    #[test]
    fn esc_backs_out_of_an_edit_leaving_the_row_as_it_was() {
        let mut dialog = SettingsDialogState::new(vec![choice_row("ui.style", "modern", true)]);
        dialog.handle_key(Key::Enter);
        dialog.handle_key(Key::Right);
        assert_eq!(dialog.handle_key(Key::Interrupt), None);
        assert_eq!(dialog.editing, None);
        // The marker navigates again, and the row kept its written value:
        // what the arrows moved was only the pending one.
        assert_eq!(dialog.handle_key(Key::Down), None);
        assert_eq!(dialog.selected, 0);
        let frame = dialog.render(80, false);
        assert!(frame.contains("modern"), "{frame}");
        assert!(!frame.contains("→"), "{frame}");
        assert!(frame.contains("[↑/↓] Navigate"), "{frame}");
    }

    #[test]
    fn a_text_row_is_not_edited_here() {
        let mut dialog = SettingsDialogState::new(vec![SettingRow {
            key: "model.default".to_owned(),
            value: "m".to_owned(),
            default: String::new(),
            description: "which model a turn uses".to_owned(),
            choices: Vec::new(),
            kind: SettingKind::Text,
            set: false,
        }]);
        assert_eq!(dialog.handle_key(Key::Enter), None);
        assert_eq!(dialog.editing, None);
        let notice = dialog.notice.expect("a notice");
        assert!(notice.contains("arsy.json"), "{notice}");
    }

    #[test]
    fn a_reload_ends_the_edit_and_keeps_the_marker_on_the_same_key() {
        let mut dialog = SettingsDialogState::new(vec![
            choice_row("ui.style", "modern", true),
            choice_row("credentials.store", "file", false),
        ]);
        dialog.selected = 1;
        dialog.handle_key(Key::Enter);
        dialog.reload(vec![choice_row("credentials.store", "classic", true)]);
        assert_eq!(dialog.editing, None);
        assert_eq!(dialog.selected, 0);
        assert!(dialog.render(80, false).contains("set"));
    }
}
