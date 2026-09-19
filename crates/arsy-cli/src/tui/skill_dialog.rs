//! The interactive `/skill` dialog: move through the skills this workspace
//! declares, read one, and switch one off or back on.
//!
//! A skill is instructions, so listing one grants nothing; what the dialog
//! manages is whether the model is told the skill exists at all. A skill that
//! is off is still discovered and still listed by `arsy skill list` — it is
//! only left out of the prompt, so it cannot be picked up for a task.
use super::*;

/// One row of the dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillChoice {
    /// The key `skill.disabled` holds: `<ecosystem>/<name>`.
    pub key: String,
    pub name: String,
    /// `claude`, `codex`, or `omp`.
    pub ecosystem: String,
    /// The `SKILL.md` this skill is, as the operator would type it.
    pub source: String,
    /// One line on what the skill is for, from its front matter.
    pub description: Option<String>,
    /// What `skill.disabled` says about it: `Some(true)` is off.
    pub disabled: Option<bool>,
}

impl SkillChoice {
    /// A skill is offered unless a layer switched it off.
    pub fn enabled(&self) -> bool {
        self.disabled != Some(true)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkillAction {
    /// Flip `skill.disabled` on the skill at this row.
    Toggle(usize),
    /// Show the skill's body in the transcript.
    Read(usize),
    Close,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillDialogState {
    pub choices: Vec<SkillChoice>,
    pub selected: usize,
    /// What the last action did, shown inside the frame rather than printed
    /// under it.
    pub notice: Option<String>,
}

impl SkillDialogState {
    pub fn new(choices: Vec<SkillChoice>) -> Self {
        Self {
            choices,
            selected: 0,
            notice: None,
        }
    }

    /// Replace the rows after a change, keeping the marker on the same skill.
    pub fn reload(&mut self, choices: Vec<SkillChoice>) {
        let key = self
            .choices
            .get(self.selected)
            .map(|choice| choice.key.clone());
        self.selected = key
            .and_then(|key| choices.iter().position(|choice| choice.key == key))
            .unwrap_or(0);
        self.choices = choices;
    }

    pub fn render(&self, width: usize, colour: bool) -> String {
        let width = width.max(MIN_WIDTH);
        let inner = width.saturating_sub(4);
        let mut lines = vec![dialog_top(" SKILLS ", width, colour)];
        if self.choices.is_empty() {
            lines.push(dialog_line(
                "  no skill is declared in this workspace",
                inner,
                colour,
                sgr_dim(),
            ));
        }
        let name_width = self
            .choices
            .iter()
            .map(|choice| visible_len(&choice.name))
            .max()
            .unwrap_or(0);
        lines.extend(self.choices.iter().enumerate().map(|(index, choice)| {
            let marked = index == self.selected;
            let style = match (marked, choice.enabled()) {
                (true, _) => sgr_accent(),
                (false, true) => "",
                _ => sgr_dim(),
            };
            dialog_line(&skill_row(choice, marked, name_width), inner, colour, style)
        }));
        lines.push(dialog_line("", inner, colour, ""));
        if let Some(notice) = &self.notice {
            lines.push(dialog_line(notice, inner, colour, sgr_dim()));
        }
        lines.push(dialog_line(
            "[↑/↓] Navigate  [Space/Enter] Toggle  [r] Read  [Esc] Close",
            inner,
            colour,
            sgr_dim(),
        ));
        lines.push(dialog_line(
            "a skill that is off is not offered to the model",
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

    pub fn handle_key(&mut self, key: Key) -> Option<SkillAction> {
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
                .then_some(SkillAction::Toggle(self.selected)),
            Key::Char('r' | 'R') => self
                .choices
                .get(self.selected)
                .is_some()
                .then_some(SkillAction::Read(self.selected)),
            Key::Interrupt | Key::Eof => Some(SkillAction::Close),
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

/// One skill as a row: marker, state, name, ecosystem, what it is for.
fn skill_row(choice: &SkillChoice, marked: bool, name_width: usize) -> String {
    let name = format!("{:<name_width$}", choice.name);
    format!(
        "{} {}  {name}  {:<6}  {}",
        if marked { "›" } else { " " },
        if choice.enabled() {
            "● on "
        } else {
            "○ off"
        },
        choice.ecosystem,
        choice.description.as_deref().unwrap_or(""),
    )
}

/// A skill's body as the transcript shows it: a marker line, then the file.
///
/// The whole body is printed rather than excerpted, because this is the one
/// place an operator can read what the model would be told to follow.
pub fn skill_body(body: &str, name: &str) -> String {
    let mut rows = vec![paint(true, sgr_accent(), &format!("  ✻ Skill {name}"))];
    rows.extend(
        body.trim_end()
            .lines()
            .map(|line| paint(true, sgr_dim(), &format!("  {line}"))),
    );
    rows.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(name: &str, disabled: Option<bool>) -> SkillChoice {
        SkillChoice {
            key: format!("claude/{name}"),
            name: name.to_owned(),
            ecosystem: "claude".to_owned(),
            source: format!(".claude/skills/{name}/SKILL.md"),
            description: Some(format!("what {name} is for")),
            disabled,
        }
    }

    #[test]
    fn space_toggles_and_r_reads_the_marked_skill() {
        let mut dialog =
            SkillDialogState::new(vec![choice("review", None), choice("deploy", None)]);
        assert_eq!(
            dialog.handle_key(Key::Char(' ')),
            Some(SkillAction::Toggle(0))
        );
        assert_eq!(
            dialog.handle_key(Key::Char('r')),
            Some(SkillAction::Read(0))
        );
        assert_eq!(dialog.handle_key(Key::Down), None);
        assert_eq!(dialog.selected, 1);
        assert_eq!(dialog.handle_key(Key::Up), None);
        assert_eq!(dialog.selected, 0);
        assert_eq!(dialog.handle_key(Key::Interrupt), Some(SkillAction::Close));
    }

    #[test]
    fn the_rows_say_what_each_skill_is_for_and_whether_it_is_offered() {
        let dialog = SkillDialogState::new(vec![choice("review", Some(true))]);
        let frame = dialog.render(80, false);
        assert!(frame.contains(" SKILLS "), "{frame}");
        assert!(frame.contains("review"), "{frame}");
        assert!(frame.contains("what review is for"), "{frame}");
        assert!(frame.contains("○ off"), "{frame}");
        assert!(frame.contains("not offered to the model"), "{frame}");
        let empty = SkillDialogState::new(Vec::new()).render(80, false);
        assert!(empty.contains("no skill is declared"), "{empty}");
    }

    #[test]
    fn a_reload_keeps_the_marker_on_the_same_skill() {
        let mut dialog = SkillDialogState::new(vec![choice("a", None), choice("b", None)]);
        dialog.selected = 1;
        dialog.reload(vec![
            choice("a", Some(true)),
            choice("b", None),
            choice("c", None),
        ]);
        assert_eq!(dialog.selected, 1);
        assert!(dialog.render(80, false).contains("○ off"));
    }
}
