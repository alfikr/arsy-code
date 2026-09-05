//! Dependency-free terminal projection over canonical events.
//!
//! The visual language follows the `brainless` codex-session registry
//! (<https://brainless.swerdlow.dev>): a bordered launch card, `•` action rows
//! with a status dot and dim result, plain assistant text, and a `›` composer
//! over a warm-model / green-cwd status row.

use arsy_kernel::{
    domain::SessionId,
    event::{EventEnvelope, EventPayload},
    policy::{ApprovalRequest, SandboxAssurance},
    provider::Effort,
};
use serde_json::Value;
use std::{
    fmt,
    io::Write,
    process::{Command, Stdio},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const MAX_TIMELINE_EVENTS: usize = 1_000;
const DEFAULT_WIDTH: usize = 80;
const DEFAULT_HEIGHT: usize = 24;
const MIN_WIDTH: usize = 20;

/// brainless palette, as truecolor SGR prefixes.
const ASSISTANT: &str = "\x1b[38;2;201;201;201m";
const DIM: &str = "\x1b[38;2;122;122;122m";
const ACCENT: &str = "\x1b[38;2;92;194;224m";
const OK: &str = "\x1b[38;2;78;169;111m";
const ERR: &str = "\x1b[38;2;247;118;142m";
const RUN: &str = "\x1b[38;2;224;175;104m";
const MODEL: &str = "\x1b[38;2;246;226;183m";
const CWD: &str = "\x1b[38;2;171;223;167m";
const BORDER: &str = "\x1b[38;2;58;58;58m";
const BULLET: &str = "\x1b[38;2;167;167;167m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";
/// Codex `user_message_bg`: white at 12% over the `#1a1a1a` terminal surface.
const INPUT_BG: &str = "\x1b[48;2;53;53;53m";
const CLEAR_EOL: &str = "\x1b[K";
const CARET_UP_1: &str = "\x1b[1A";
const CLEAR_BELOW: &str = "\x1b[J";

/// Terminal modes, owned for as long as ARSY draws the composer.
///
/// The shell must stop echoing (ARSY paints the input line itself, so an echo
/// would double it and shift the block) and stop buffering lines (a key has to
/// arrive as it is pressed). `-isig` keeps Ctrl-C out of the signal path so it
/// arrives as a key and can interrupt the running turn instead of killing
/// ARSY. `stty` is used rather than `termios` because the workspace forbids
/// `unsafe_code`.
pub struct RawTerminal {
    saved: Option<String>,
}

impl RawTerminal {
    pub fn acquire() -> std::io::Result<Self> {
        let saved = stty(&["-g"])?;
        let terminal = Self {
            saved: Some(saved.trim().to_owned()),
        };
        stty(&["-echo", "-icanon", "-isig", "min", "1", "time", "0"])?;
        let mut stdout = std::io::stdout();
        write!(stdout, "\x1b[?2004h")?;
        stdout.flush()?;
        Ok(terminal)
    }
}

impl Drop for RawTerminal {
    /// Runs on every exit path, including unwind, so the shell is never left
    /// in raw mode.
    fn drop(&mut self) {
        let mut stdout = std::io::stdout();
        let _ = write!(stdout, "\x1b[?2004l{RESET}");
        let _ = stdout.flush();
        let _ = match &self.saved {
            Some(mode) => stty(&[mode]),
            None => stty(&["sane"]),
        };
    }
}

fn stty(args: &[&str]) -> std::io::Result<String> {
    let output = Command::new("stty")
        .args(args)
        .stdin(Stdio::inherit())
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(
            "stty could not configure the terminal",
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Read stdin bytes on a thread, so the main loop can watch keys and provider
/// output at the same time — that is what makes a turn interruptible.
pub fn spawn_key_reader() -> std::sync::mpsc::Receiver<u8> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(4096);
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut byte = [0_u8; 1];
        while std::io::Read::read(&mut stdin, &mut byte).is_ok_and(|read| read == 1) {
            if sender.send(byte[0]).is_err() {
                break;
            }
        }
    });
    receiver
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Key {
    Char(char),
    Backspace,
    Left,
    Right,
    Home,
    End,
    Delete,
    Up,
    Down,
    Enter,
    /// Ctrl-C, or Escape once it is known to stand alone.
    Interrupt,
    /// Ctrl-D on an empty line.
    Eof,
}

/// Turns the raw byte stream into keys, holding back partial UTF-8 characters
/// and partial escape sequences.
#[derive(Default)]
pub struct Keys {
    pending: Vec<u8>,
    pasting: bool,
}

impl Keys {
    pub fn feed(&mut self, byte: u8) -> Option<Key> {
        if self.pending.first() == Some(&0x1b) {
            return self.feed_escape(byte);
        }
        if self.pasting && matches!(byte, b'\r' | b'\n' | b'\t') {
            return Some(Key::Char(' '));
        }
        if self.pasting && byte != 0x1b && (byte < 0x20 || byte == 0x7f) {
            return None;
        }
        if byte < 0x80 && !self.pending.is_empty() {
            self.pending.clear();
        }
        match byte {
            0x03 => return Some(Key::Interrupt),
            0x04 => return Some(Key::Eof),
            b'\r' | b'\n' => return Some(Key::Enter),
            0x7f | 0x08 => return Some(Key::Backspace),
            0x1b => {
                self.pending.push(byte);
                return None;
            }
            // Any other control byte is not bound to an action.
            0x00..=0x1f => return None,
            _ => {}
        }
        self.pending.push(byte);
        match std::str::from_utf8(&self.pending) {
            Ok(text) => {
                let character = text.chars().next();
                self.pending.clear();
                character.map(Key::Char)
            }
            // Incomplete is normal mid-character; invalid means the stream is
            // not UTF-8, and holding the bytes would stall every later key.
            Err(error) if error.error_len().is_none() => None,
            Err(_) => {
                self.pending.clear();
                None
            }
        }
    }

    fn feed_escape(&mut self, byte: u8) -> Option<Key> {
        if self.pending.len() == 1 && !matches!(byte, b'[' | b'O') {
            // Escape did not introduce a sequence, so it was its own key.
            self.pending.clear();
            return self.feed(byte).or(Some(Key::Interrupt));
        }
        self.pending.push(byte);
        if self.pending.len() > 32 {
            self.pending.clear();
            return None;
        }
        if !(0x40..=0x7e).contains(&byte) || self.pending.len() == 2 {
            return None;
        }
        let sequence = std::mem::take(&mut self.pending);
        if sequence == b"\x1b[200~" {
            self.pasting = true;
            return None;
        }
        if sequence == b"\x1b[201~" {
            self.pasting = false;
            return None;
        }
        if self.pasting {
            return None;
        }
        match (sequence.last(), sequence.get(2)) {
            (Some(b'A'), _) => Some(Key::Up),
            (Some(b'B'), _) => Some(Key::Down),
            (Some(b'~'), Some(b'3')) => Some(Key::Delete),
            (Some(b'D'), _) => Some(Key::Left),
            (Some(b'C'), _) => Some(Key::Right),
            (Some(b'H'), _) | (Some(b'~'), Some(b'1')) => Some(Key::Home),
            (Some(b'F'), _) | (Some(b'~'), Some(b'4')) => Some(Key::End),
            _ => None,
        }
    }

    /// A lone Escape is only distinguishable from a sequence by the absence of
    /// what would follow it, so the caller reports the pause.
    pub fn flush_escape(&mut self) -> Option<Key> {
        let interrupt = self.pending.as_slice() == [0x1b] && !self.pasting;
        if self.pending.first() == Some(&0x1b) && !self.pasting {
            self.pending.clear();
        }
        interrupt.then_some(Key::Interrupt)
    }
}

/// What a key means to the session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    Redraw,
    Submit(String),
    Quit,
    None,
}

/// The slash commands the composer offers and `/help` prints. One table, so a
/// command cannot appear in the menu and not in the help, or the reverse.
pub const COMMANDS: &[(&str, &str)] = &[
    ("/model", "choose the provider model"),
    ("/effort", "set reasoning effort; low | medium | high | off"),
    (
        "/mcp",
        "inspect MCP declarations; list | show NAME, --source claude|codex|omp",
    ),
    ("/hooks", "inspect Claude hooks; list, --event NAME"),
    (
        "/settings",
        "show effective configuration and where each value came from; [KEY]",
    ),
    ("/doctor", "check workspace, storage, and sandbox assurance"),
    ("/auth", "list configured provider credentials"),
    (
        "/compat",
        "explain ecosystem mapping; claude | codex | omp | agents",
    ),
    ("/help", "show these actions"),
    ("/quit", "exit"),
];

/// ponytail: the menu is capped rather than scrolled. It holds every command
/// there is; give it a window over `menu()` if the table outgrows the cap.
const MENU_ROWS: usize = 10;

/// What `/help` prints, built from the same table the menu offers.
pub fn help(colour: bool) -> String {
    let label = COMMANDS
        .iter()
        .map(|(name, _)| name.chars().count())
        .max()
        .unwrap_or(0);
    let mut text = String::new();
    for (name, description) in COMMANDS {
        text.push_str(&format!(
            "  {}{}{}\n",
            paint(colour, ACCENT, name),
            " ".repeat(label - name.chars().count() + 2),
            paint(colour, DIM, description),
        ));
    }
    for line in [
        "Type / to open this menu; Up/Down: input history, or the menu while one is open",
        "Enter: take the highlighted command, or send a line that is already one",
        "Esc/Ctrl-C: cancel turn · Ctrl-D: exit on empty input",
        "MCP connections and executable hooks are not loaded by ARSY; imported declarations grant no authority.",
    ] {
        text.push_str(&paint(colour, DIM, line));
        text.push('\n');
    }
    text
}

/// The input line ARSY owns: its text, its caret, and how many rows it last
/// painted. Nothing else writes to those rows, so redrawing is exact.
#[derive(Default)]
pub struct Composer {
    buffer: String,
    caret: usize,
    drawn: bool,
    history: std::collections::VecDeque<String>,
    history_index: Option<usize>,
    draft: String,
    /// Which menu row Up/Down has landed on, clamped to the matches on use.
    selected: usize,
    /// Set while the line is a picker answer rather than a task, so the menu
    /// does not offer commands that the picker would not accept.
    picking: bool,
    /// Terminal rows, refreshed with the width; `0` means not measured yet.
    height: usize,
}

impl Composer {
    pub fn restore(&mut self, text: String) {
        self.caret = text.chars().count();
        self.buffer = text;
        self.selected = 0;
    }

    /// The commands the line offers right now.
    ///
    /// The menu is only open while the line is still one word: once an argument
    /// is being typed the command is already chosen, and a list of commands
    /// would cover the terminal for the rest of the line.
    pub fn set_picking(&mut self, picking: bool) {
        self.picking = picking;
        self.selected = 0;
    }

    /// Measured with the width, and on the same schedule.
    pub fn set_height(&mut self, rows: usize) {
        self.height = rows;
    }

    pub fn menu(&self) -> Vec<(&'static str, &'static str)> {
        if self.picking || !self.buffer.starts_with('/') || self.buffer.contains(' ') {
            return Vec::new();
        }
        COMMANDS
            .iter()
            .filter(|(name, _)| name.starts_with(&self.buffer))
            .take(self.menu_capacity())
            .copied()
            .collect()
    }

    /// How many menu rows the terminal can hold.
    ///
    /// The block is four fixed rows — pad, input, pad, status — and the caret is
    /// returned to the input row by counting rows upward. A block taller than
    /// the screen scrolls, that count then lands on the wrong row, and the next
    /// repaint erases scrollback instead of the block. So the menu takes only
    /// the rows that are left.
    fn menu_capacity(&self) -> usize {
        match self.height {
            0 => MENU_ROWS,
            rows => MENU_ROWS.min(rows.saturating_sub(4)),
        }
    }
    pub fn press(&mut self, key: Key) -> Action {
        match key {
            Key::Char(character) if !character.is_control() => {
                self.buffer.insert(self.byte_at(self.caret), character);
                self.caret += 1;
                self.selected = 0;
                Action::Redraw
            }
            Key::Backspace if self.caret > 0 => {
                self.buffer.remove(self.byte_at(self.caret - 1));
                self.caret -= 1;
                self.selected = 0;
                Action::Redraw
            }
            Key::Delete if self.caret < self.buffer.chars().count() => {
                self.buffer.remove(self.byte_at(self.caret));
                self.selected = 0;
                Action::Redraw
            }
            // An open menu owns Up/Down: it is the list in front of the reader,
            // and history is still one Escape or Backspace away. The ends wrap,
            // so a short list is never a dead end in one direction.
            Key::Up if !self.menu().is_empty() => {
                let last = self.menu().len() - 1;
                self.selected = self.selected.min(last).checked_sub(1).unwrap_or(last);
                Action::Redraw
            }
            Key::Down if !self.menu().is_empty() => {
                let last = self.menu().len() - 1;
                self.selected = if self.selected >= last {
                    0
                } else {
                    self.selected + 1
                };
                Action::Redraw
            }
            Key::Up if !self.history.is_empty() => {
                let index = match self.history_index {
                    Some(index) => index.saturating_sub(1),
                    None => {
                        self.draft = self.buffer.clone();
                        self.history.len() - 1
                    }
                };
                self.history_index = Some(index);
                self.buffer = self.history[index].clone();
                self.caret = self.buffer.chars().count();
                Action::Redraw
            }
            Key::Down if self.history_index.is_some() => {
                let index = self.history_index.unwrap() + 1;
                self.history_index = (index < self.history.len()).then_some(index);
                self.buffer = self
                    .history_index
                    .map_or_else(|| self.draft.clone(), |index| self.history[index].clone());
                self.caret = self.buffer.chars().count();
                Action::Redraw
            }
            Key::Left if self.caret > 0 => {
                self.caret -= 1;
                Action::Redraw
            }
            Key::Right if self.caret < self.buffer.chars().count() => {
                self.caret += 1;
                Action::Redraw
            }
            Key::Home => {
                self.caret = 0;
                Action::Redraw
            }
            Key::End => {
                self.caret = self.buffer.chars().count();
                Action::Redraw
            }
            // Enter takes the highlighted command, unless the line already is
            // one: otherwise a typed-out `/quit` would refuse to send itself.
            Key::Enter if self.completion().is_some() => {
                self.restore(self.completion().unwrap_or_default());
                Action::Redraw
            }
            Key::Enter => {
                let line = self.take();
                if !line.trim().is_empty() && self.history.back() != Some(&line) {
                    self.history.push_back(line.clone());
                    if self.history.len() > 100 {
                        self.history.pop_front();
                    }
                }
                Action::Submit(line)
            }
            // Ctrl-C clears a drafted line first, and only quits once there is
            // nothing left to lose.
            Key::Interrupt if !self.buffer.is_empty() => {
                self.take();
                Action::Redraw
            }
            Key::Interrupt | Key::Eof if self.buffer.is_empty() => Action::Quit,
            _ => Action::None,
        }
    }

    /// The command Enter would fill in, or `None` when the line is already one
    /// and Enter should send it.
    fn completion(&self) -> Option<String> {
        let menu = self.menu();
        let selected = menu.get(self.selected.min(menu.len().checked_sub(1)?))?;
        (!menu.iter().any(|(name, _)| *name == self.buffer)).then(|| selected.0.to_owned())
    }

    fn take(&mut self) -> String {
        self.caret = 0;
        self.history_index = None;
        self.selected = 0;
        self.draft.clear();
        std::mem::take(&mut self.buffer)
    }

    fn byte_at(&self, caret: usize) -> usize {
        self.buffer
            .char_indices()
            .nth(caret)
            .map_or(self.buffer.len(), |(at, _)| at)
    }

    /// Paint the block — pad, input, pad, menu, status — and leave the caret in
    /// the input line where the next character belongs.
    ///
    /// A previous block is erased first: the caret always rests on the input
    /// row, one row into the block, so clearing from the row above removes the
    /// whole block however tall the menu made it.
    pub fn render(&mut self, width: usize, colour: bool, status: &str) -> String {
        let width = width.max(MIN_WIDTH);
        let status = fit(status, width.saturating_sub(1));
        let room = width.saturating_sub(3);
        let (text, caret) = self.window(room);
        let menu = self.menu_rows(width, colour);
        let mut frame = String::new();
        if self.drawn {
            frame.push_str(RESET);
            frame.push_str(CARET_UP_1);
            frame.push('\r');
            frame.push_str(CLEAR_BELOW);
        }
        self.drawn = true;
        let surface = if colour {
            format!("{INPUT_BG}{CLEAR_EOL}")
        } else {
            String::new()
        };
        frame.push_str(&format!(
            "{surface}\n{surface}{} {text}{}\n{surface}{}\n",
            if colour {
                format!("{INPUT_BG}›")
            } else {
                "›".to_owned()
            },
            if colour { CLEAR_EOL } else { "" },
            if colour { RESET } else { "" },
        ));
        for row in &menu {
            frame.push_str(row);
            frame.push('\n');
        }
        frame.push_str(&status);
        // Back onto the input row, over the pad, the menu, and the status row,
        // then across `› ` and the text before the caret. Nothing here can wrap:
        // `window` bounded the text and `fit` bounded every other row.
        frame.push_str(&format!("\x1b[{}A\r\x1b[{}C", menu.len() + 2, caret + 2));
        frame
    }

    /// One row per offered command, marked at the selection.
    fn menu_rows(&self, width: usize, colour: bool) -> Vec<String> {
        let menu = self.menu();
        let Some(last) = menu.len().checked_sub(1) else {
            return Vec::new();
        };
        let selected = self.selected.min(last);
        let label = menu
            .iter()
            .map(|(name, _)| name.chars().count())
            .max()
            .unwrap_or(0);
        menu.iter()
            .enumerate()
            .map(|(index, (name, description))| {
                let chosen = index == selected;
                let row = format!(
                    "  {} {}{}{}",
                    paint(colour, ACCENT, if chosen { "›" } else { " " }),
                    paint(colour, if chosen { ACCENT } else { BULLET }, name),
                    " ".repeat(label - name.chars().count() + 2),
                    paint(colour, DIM, description),
                );
                fit(&row, width)
            })
            .collect()
    }

    /// Erase the block so turn output starts on a clean row, and keep the
    /// submitted line in the scrollback the way a shell would.
    pub fn commit(&mut self, submitted: &str, colour: bool) -> String {
        format!(
            "{}{} {}\n",
            self.clear(),
            paint(colour, BOLD, "›"),
            paint(colour, ASSISTANT, submitted),
        )
    }

    pub fn clear(&mut self) -> String {
        if !std::mem::take(&mut self.drawn) {
            return String::new();
        }
        format!("{RESET}{CARET_UP_1}\r{CLEAR_BELOW}")
    }

    /// Slide the visible text so the caret stays on the row instead of
    /// wrapping, which would break the block's row count.
    fn window(&self, room: usize) -> (String, usize) {
        let characters: Vec<char> = self.buffer.chars().collect();
        let budget = room.saturating_sub(1);
        let width = |character: &char| character.width().unwrap_or(0);
        let mut start = self.caret;
        let mut caret = 0;
        while start > 0 && caret + width(&characters[start - 1]) <= budget {
            start -= 1;
            caret += width(&characters[start]);
        }
        let mut used = 0;
        let text = characters[start..]
            .iter()
            .take_while(|character| {
                used += width(character);
                used <= budget
            })
            .collect();
        (text, caret)
    }
}

const LABEL_WIDTH: usize = 10;

/// The mark from `assets/arsy-code-logo.svg`, reduced offline to half-block
/// rows. The artwork is a single colour, so only the silhouette is stored —
/// 22 columns by 11 rows of glyphs, not an embedded image.
const LOGO_COLOUR: &str = "\x1b[38;2;64;220;121m";
const LOGO_WIDTH: usize = 22;
const LOGO_GAP: usize = 3;
const LOGO: [&str; 11] = [
    " █▀▀▀▀▀▀▀     ▀▀▀▀▀▀▀▀",
    "██     ▄▄▄███▄▄▄▄     ",
    "██  ▄▄▀  ▄▀█▀▄  ▀▀▄▄  ",
    "██ █▀▀▄▄█▀ █  ███▀▀█▄ ",
    "▀▄█    █▀▀▄█   ▀▄   ▀▄",
    "██▄   █    ██▀▀▄██   █",
    "▄ ▀█▄█ ▄▄▄▀   ▄▀ ▀▄ █▀",
    "██   ▀▀▀▀▀█▄▄█▄▄▄▄██  ",
    "██         ▀▀█  ▄█    ",
    "██            ▀██     ",
    " █▄▄▄▄▄▄▄▄▄▄▄  ▀  ▄▄▄▄",
];

fn paint(colour: bool, code: &str, text: &str) -> String {
    let text = safe_text(text);
    if colour {
        format!("{code}{text}{RESET}")
    } else {
        text.to_owned()
    }
}

/// External text cannot move the cursor, set a title, or access the clipboard.
pub fn safe_text(text: &str) -> String {
    crate::terminal_text(text)
}

/// A provider must be reaped even when rendering or reading its stream fails.
pub struct ProviderChild(pub std::process::Child);

impl ProviderChild {
    pub fn stop(&mut self, force: bool) {
        #[cfg(unix)]
        let _ = Command::new("kill")
            .args([
                if force { "-KILL" } else { "-TERM" },
                "--",
                &format!("-{}", self.0.id()),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        #[cfg(windows)]
        {
            let mut command = Command::new("taskkill");
            command.args(["/PID", &self.0.id().to_string(), "/T"]);
            if force {
                command.arg("/F");
            }
            let _ = command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        if force {
            let _ = self.0.kill();
        }
    }
}

impl Drop for ProviderChild {
    fn drop(&mut self) {
        self.stop(true);
        let _ = self.0.wait();
    }
}

pub fn provider_lines(
    reader: impl std::io::Read + Send + 'static,
) -> std::sync::mpsc::Receiver<std::io::Result<String>> {
    use std::io::{BufRead, Read};
    let (sender, receiver) = std::sync::mpsc::sync_channel(16);
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(reader);
        loop {
            let mut bytes = Vec::new();
            let line = match Read::take(&mut reader, 1_048_577).read_until(b'\n', &mut bytes) {
                Ok(0) => break,
                Ok(_) if bytes.len() > 1_048_576 => {
                    Err(std::io::Error::other("provider event exceeds 1 MiB"))
                }
                Ok(_) => String::from_utf8(bytes)
                    .map_err(|_| std::io::Error::other("provider event is not UTF-8")),
                Err(error) => Err(error),
            };
            let failed = line.is_err();
            if sender.send(line).is_err() || failed {
                break;
            }
        }
    });
    receiver
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimelineEntry {
    pub sequence: u64,
    pub name: String,
}

/// Read-only projection: only canonical envelopes can advance its cursor.
pub struct TuiState {
    workspace: String,
    session: SessionId,
    cursor: u64,
    timeline: Vec<TimelineEntry>,
    streaming: Option<String>,
    sandbox_assurance: SandboxAssurance,
    model_route: Option<ModelRoute>,
    effort: Option<Effort>,
}

impl TuiState {
    pub fn new(workspace: String, session: SessionId) -> Self {
        Self {
            workspace,
            session,
            cursor: 0,
            timeline: Vec::new(),
            streaming: None,
            sandbox_assurance: SandboxAssurance::None,
            model_route: None,
            effort: None,
        }
    }

    pub fn set_sandbox_assurance(&mut self, assurance: SandboxAssurance) {
        self.sandbox_assurance = assurance;
    }

    pub fn set_model_route(&mut self, route: ModelRoute) {
        self.model_route = Some(route);
    }

    pub fn set_effort(&mut self, effort: Option<Effort>) {
        self.effort = effort;
    }

    pub fn apply(&mut self, event: &EventEnvelope) -> Result<(), TuiError> {
        if event.session != self.session {
            return Err(TuiError::WrongSession);
        }
        let expected = self.cursor.checked_add(1).ok_or(TuiError::CursorOverflow)?;
        if event.sequence != expected {
            return Err(TuiError::Gap {
                expected,
                actual: event.sequence,
            });
        }
        self.cursor = event.sequence;
        self.timeline.push(TimelineEntry {
            sequence: event.sequence,
            name: event.kind.clone(),
        });
        if event.kind == "model.delta" {
            self.streaming = match &event.payload {
                EventPayload::Inline { data } => data
                    .get("text")
                    .and_then(|value| value.as_str())
                    .map(str::to_owned),
                EventPayload::Artifact { .. } => None,
            };
        }
        if self.timeline.len() > MAX_TIMELINE_EVENTS {
            self.timeline.remove(0);
        }
        Ok(())
    }

    /// The launch card: a bordered box with `>_ ARSY CODE` and its label rows.
    pub fn render(&self, width: usize, colour: bool) -> String {
        let width = width.max(MIN_WIDTH);
        let inner = width.saturating_sub(4);
        let mut rows = vec![
            format!(
                "{} {}{}",
                paint(colour, DIM, ">_"),
                paint(colour, BOLD, "ARSY CODE"),
                paint(colour, DIM, &format!(" (v{})", env!("CARGO_PKG_VERSION"))),
            ),
            String::new(),
        ];
        if let Some(route) = &self.model_route {
            rows.push(format!(
                "{}   {}",
                label_row(colour, "model:", &route.to_string(), MODEL),
                paint(colour, DIM, "/model to change"),
            ));
        }
        rows.push(label_row(colour, "directory:", &self.workspace, CWD));
        rows.push(label_row(
            colour,
            "sandbox:",
            &format!("{} · read-only", self.sandbox_assurance),
            DIM,
        ));
        rows.push(label_row(
            colour,
            "session:",
            &self.session.to_string(),
            DIM,
        ));
        if let Some(entry) = self.timeline.last() {
            rows.push(label_row(
                colour,
                "event:",
                &format!("{} {}", entry.sequence, entry.name),
                DIM,
            ));
        }
        if let Some(text) = &self.streaming {
            rows.push(paint(colour, ASSISTANT, text));
        }

        let rows = beside_logo(rows, inner, colour);
        let rule = "─".repeat(width.saturating_sub(2));
        let mut lines = vec![paint(colour, BORDER, &format!("╭{rule}╮"))];
        for row in &rows {
            let row = fit(row, inner);
            let pad = " ".repeat(inner.saturating_sub(visible_len(&row)));
            lines.push(format!(
                "{} {row}{pad} {}",
                paint(colour, BORDER, "│"),
                paint(colour, BORDER, "│"),
            ));
        }
        lines.push(paint(colour, BORDER, &format!("╰{rule}╯")));
        lines.join("\n")
    }

    /// The status row shown under the composer: warm model, green directory.
    /// `branch` is passed rather than kept, because it belongs to the checkout
    /// and can change while the session is open.
    pub fn status_row(&self, width: usize, colour: bool, branch: Option<&str>) -> String {
        let route = self
            .model_route
            .as_ref()
            .map_or_else(|| "no model".to_owned(), ModelRoute::to_string);
        let mut row = format!("  {}", paint(colour, MODEL, &route));
        // An unset effort says so, because "no reasoning knob is sent" and
        // "some level is in force" have to be told apart at a glance.
        row.push_str(&format!(
            "  {}",
            paint(
                colour,
                DIM,
                &self.effort.map_or_else(
                    || "effort:—".to_owned(),
                    |effort| format!("effort:{effort}")
                ),
            ),
        ));
        if let Some(branch) = branch {
            row.push_str(&format!("  {}", paint(colour, ACCENT, branch)));
        }
        row.push_str(&format!("  {}", paint(colour, CWD, &self.workspace)));
        fit(&row, width.max(MIN_WIDTH))
    }

    pub fn render_approval(request: &ApprovalRequest, width: usize) -> String {
        [
            "APPROVAL REQUIRED".to_owned(),
            format!("Effect: {}", request.intended_effect()),
            format!("Scope: {}", request.scope()),
            format!("Reversibility: {}", request.reversibility()),
            format!("Reason: {}", request.reason),
            "Choices: [d] deny  [o] approve operation  [r] approve displayed rule".to_owned(),
        ]
        .into_iter()
        .map(|line| fit(&line, width.max(MIN_WIDTH)))
        .collect::<Vec<_>>()
        .join("\n")
            + "\n"
    }
}

/// Put the mark to the left of the card's text, vertically centred against it.
///
/// The mark is dropped when the card is too narrow to hold both, so a small
/// terminal keeps the text it needs instead of a cropped picture.
fn beside_logo(text: Vec<String>, inner: usize, colour: bool) -> Vec<String> {
    let gutter = LOGO_WIDTH + LOGO_GAP;
    if inner < gutter + LABEL_WIDTH + 12 {
        return text;
    }
    let offset = LOGO.len().saturating_sub(text.len()) / 2;
    (0..LOGO.len().max(text.len() + offset))
        .map(|row| {
            let mark = LOGO.get(row).map_or_else(
                || " ".repeat(LOGO_WIDTH),
                |art| paint(colour, LOGO_COLOUR, art),
            );
            let line = row
                .checked_sub(offset)
                .and_then(|index| text.get(index))
                .map_or("", String::as_str);
            format!("{mark}{}{line}", " ".repeat(LOGO_GAP))
        })
        .collect()
}

fn label_row(colour: bool, label: &str, value: &str, value_colour: &str) -> String {
    format!(
        "{}{}{}",
        paint(colour, DIM, label),
        " ".repeat(LABEL_WIDTH.saturating_sub(label.chars().count()) + 1),
        paint(colour, value_colour, value),
    )
}

/// Status dot colours from the brainless `CodexExec` component.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Status {
    Ok,
    Error,
    Run,
}

impl Status {
    const fn colour(self) -> &'static str {
        match self {
            Self::Ok => OK,
            Self::Error => ERR,
            Self::Run => RUN,
        }
    }
}

fn exec_row(colour: bool, status: Status, command: &str, result: Option<&str>) -> String {
    let head = format!(
        "  {} {}",
        paint(colour, status.colour(), "•"),
        paint(colour, ACCENT, command),
    );
    match result {
        Some(result) => format!("{head}  {}", paint(colour, DIM, result)),
        None => head,
    }
}

/// Render one `codex exec --json` JSONL event as brainless rows.
///
/// Returns `None` for events with no visual form, and for anything that does
/// not parse — a projection must never abort the turn it is displaying.
pub fn render_codex_event(line: &str, colour: bool) -> Option<String> {
    let event: Value = serde_json::from_str(line).ok()?;
    match event.get("type")?.as_str()? {
        // Working is transient composer status, not permanent scrollback.
        "turn.started" => None,
        "item.started" | "item.updated" | "item.completed" => {
            render_codex_item(event.get("item")?, colour)
        }
        "error" => Some(error_row(
            colour,
            event.get("message").and_then(Value::as_str)?,
        )),
        "turn.failed" => Some(error_row(
            colour,
            event
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("Turn failed"),
        )),
        _ => None,
    }
}

pub fn working_row(colour: bool) -> String {
    format!(
        "  {} {}",
        paint(colour, BULLET, "•"),
        paint(colour, BOLD, "Working…"),
    )
}

/// One line of streamed model text, styled as the Codex projection styles an
/// assistant message, so both routes read the same in scrollback.
pub fn assistant_row(colour: bool, text: &str) -> String {
    paint(colour, ASSISTANT, text.trim_end())
}

/// Shown when a turn is stopped from the keyboard.
pub fn interrupted_row(colour: bool) -> String {
    exec_row(colour, Status::Run, "Interrupted", None)
}

fn error_row(colour: bool, message: &str) -> String {
    format!(
        "  {} {}",
        paint(colour, ERR, "•"),
        paint(colour, ERR, &unwrap_api_error(message.trim())),
    )
}

/// Provider transport errors arrive as an embedded JSON body; the readable
/// sentence is one level in.
fn unwrap_api_error(message: &str) -> String {
    serde_json::from_str::<Value>(message)
        .ok()
        .and_then(|body| {
            body.pointer("/error/message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| message.to_owned())
}

fn render_codex_item(item: &Value, colour: bool) -> Option<String> {
    let text = |key: &str| item.get(key).and_then(Value::as_str).unwrap_or_default();
    match item.get("type")?.as_str()? {
        "agent_message" => Some(paint(colour, ASSISTANT, text("text").trim())),
        // Codex reports some failures as an item rather than a top-level event.
        "error" => Some(error_row(colour, text("message"))),
        "reasoning" => Some(paint(
            colour,
            DIM,
            &format!("  ✻ {}", first_line(text("text"))),
        )),
        "command_execution" => {
            let exit = item.get("exit_code").and_then(Value::as_i64);
            let (status, result) = match exit {
                Some(0) => (Status::Ok, "→ done".to_owned()),
                Some(code) => (Status::Error, format!("→ exit {code}")),
                None => (Status::Run, "→ running".to_owned()),
            };
            Some(exec_row(
                colour,
                status,
                &format!("Ran {}", first_line(unwrap_shell(text("command")))),
                Some(&result),
            ))
        }
        "file_change" => {
            let rows = item
                .get("changes")?
                .as_array()?
                .iter()
                .map(|change| {
                    let path = change.get("path").and_then(Value::as_str).unwrap_or("?");
                    let verb = match change.get("kind").and_then(Value::as_str) {
                        Some("add") => "Added",
                        Some("delete") => "Deleted",
                        _ => "Edited",
                    };
                    exec_row(colour, Status::Ok, &format!("{verb} {path}"), None)
                })
                .collect::<Vec<_>>();
            (!rows.is_empty()).then(|| rows.join("\n"))
        }
        "mcp_tool_call" => Some(exec_row(
            colour,
            item_status(item),
            &format!("{}.{}", text("server"), text("tool")),
            Some(
                item.pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| match item.get("status").and_then(Value::as_str) {
                        Some("in_progress") => "→ running",
                        Some("failed") => "→ failed",
                        _ => "→ done",
                    }),
            ),
        )),
        "web_search" => Some(exec_row(
            colour,
            Status::Ok,
            &format!("Searched {}", first_line(text("query"))),
            None,
        )),
        // todo_list has no brainless row; unknown kinds still get a dim marker
        // so a codex upgrade never renders as silence.
        "todo_list" => None,
        other => Some(exec_row(colour, item_status(item), other, None)),
    }
}

fn item_status(item: &Value) -> Status {
    match item.get("status").and_then(Value::as_str) {
        Some("failed") => Status::Error,
        Some("in_progress") => Status::Run,
        _ => Status::Ok,
    }
}

/// Codex wraps most commands as `<shell> -lc "<command>"`; the wrapper is the
/// same on every row, so showing it buries the command that actually ran.
fn unwrap_shell(command: &str) -> &str {
    let Some((_, inner)) = command.split_once(" -lc ") else {
        return command;
    };
    ['"', '\'']
        .into_iter()
        .find_map(|quote| {
            inner
                .strip_prefix(quote)
                .and_then(|rest| rest.strip_suffix(quote))
        })
        .unwrap_or(inner)
}

fn first_line(text: &str) -> &str {
    text.trim().lines().next().unwrap_or_default()
}

/// Which provider serves a turn, and with which model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelRoute {
    /// A configured provider endpoint, or [`CODEX_PROVIDER`] for the
    /// subprocess fallback.
    pub provider: String,
    pub model: String,
}

/// The provider id that means "hand the turn to the logged-in Codex CLI".
pub const CODEX_PROVIDER: &str = "codex";

impl ModelRoute {
    /// Whether this turn goes to the Codex CLI rather than to a provider ARSY
    /// talks to itself.
    pub fn is_codex(&self) -> bool {
        self.provider == CODEX_PROVIDER
    }

    /// `provider/model`, the form remembered between sessions. A bare model
    /// name is a file written before routes named a provider, and meant Codex.
    pub fn parse(raw: &str) -> Self {
        match raw.split_once('/') {
            Some((provider, model)) => Self {
                provider: provider.to_owned(),
                model: model.to_owned(),
            },
            None => Self {
                provider: CODEX_PROVIDER.to_owned(),
                model: raw.to_owned(),
            },
        }
    }
}

impl fmt::Display for ModelRoute {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.provider, self.model)
    }
}

pub fn detect_model_route() -> Option<ModelRoute> {
    Command::new("codex")
        .args(["login", "status"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
        .then(|| ModelRoute {
            provider: CODEX_PROVIDER.to_owned(),
            model: "default".to_owned(),
        })
}

/// A model the logged-in Codex CLI offers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelChoice {
    pub slug: String,
    pub name: String,
}

/// Read the models the Codex CLI has cached for the signed-in account.
///
/// This is Codex's own cache, so an unreadable or reshaped file is not an
/// error: the picker falls back to free text, which always worked.
pub fn available_models() -> Vec<ModelChoice> {
    let home = std::env::var_os("CODEX_HOME").map_or_else(
        || {
            std::env::var_os("HOME")
                .map(|home| std::path::Path::new(&home).join(".codex"))
                .unwrap_or_default()
        },
        std::path::PathBuf::from,
    );
    let Ok(text) = std::fs::read_to_string(home.join("models_cache.json")) else {
        return Vec::new();
    };
    let Ok(cache) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    let Some(models) = cache.get("models").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut listed: Vec<(i64, ModelChoice)> = models
        .iter()
        .filter(|model| model.get("visibility").and_then(Value::as_str) == Some("list"))
        .filter_map(|model| {
            let slug = model.get("slug").and_then(Value::as_str)?;
            Some((
                model.get("priority").and_then(Value::as_i64).unwrap_or(0),
                ModelChoice {
                    slug: slug.to_owned(),
                    name: model
                        .get("display_name")
                        .and_then(Value::as_str)
                        .unwrap_or(slug)
                        .to_owned(),
                },
            ))
        })
        .collect();
    listed.sort_by_key(|(priority, _)| *priority);
    listed.into_iter().map(|(_, choice)| choice).collect()
}

/// Offer the cached models by number, defaulting to `current`.
///
/// Accepts a list index, a slug typed in full, or an empty line to keep
/// `current`. Returns `None` on end of input or `:quit`.
pub fn render_model_list(
    writer: &mut impl Write,
    models: &[ModelChoice],
    current: &ModelRoute,
    colour: bool,
) -> std::io::Result<()> {
    let selected = models
        .iter()
        .position(|choice| choice.slug == current.model);
    for (index, choice) in models.iter().enumerate() {
        let marker = if Some(index) == selected { "›" } else { " " };
        writeln!(
            writer,
            "  {} {} {}  {}",
            paint(colour, ACCENT, marker),
            paint(colour, DIM, &format!("{}.", index + 1)),
            paint(colour, MODEL, &choice.slug),
            paint(colour, DIM, &choice.name),
        )?;
    }
    Ok(())
}

/// The status row shown under the composer while a model is being picked.
///
/// A configured endpoint has no list to offer — nothing tells ARSY what a
/// gateway serves — so there the prompt asks for a slug instead of a number
/// in a range of none.
pub fn model_prompt(models: &[ModelChoice], current: &ModelRoute, colour: bool) -> String {
    let choices = if models.is_empty() {
        "a slug".to_owned()
    } else {
        format!("1-{} or a slug", models.len())
    };
    paint(
        colour,
        DIM,
        &format!("  model [{}] · {choices}", current.model),
    )
}

/// Resolve a picker answer: a list index, a slug typed in full, or an empty
/// line to keep the current model.
///
/// A rejected answer is returned as the sentence to show, because the picker is
/// the only guard before the slug is passed to the provider CLI *and* written to
/// the user configuration: an accepted typo would otherwise fail every later
/// turn, in every later session, with a provider error that names the wrong
/// cause.
pub fn resolve_model(
    answer: &str,
    models: &[ModelChoice],
    current: &ModelRoute,
) -> Result<ModelRoute, String> {
    let answer = answer.trim();
    if answer.is_empty() {
        return Ok(current.clone());
    }
    if let Ok(number) = answer.parse::<usize>() {
        return match number.checked_sub(1).and_then(|index| models.get(index)) {
            Some(choice) => Ok(ModelRoute {
                provider: current.provider.clone(),
                model: choice.slug.clone(),
            }),
            None if models.is_empty() => Err("no models are listed; type a model slug".to_owned()),
            None => Err(format!("no model {number}; choose 1-{}", models.len())),
        };
    }
    validate_slug(answer)?;
    Ok(ModelRoute {
        provider: current.provider.clone(),
        model: answer.to_owned(),
    })
}

/// Accept what a provider slug can contain and nothing else. The picker shares
/// its line with the composer, so a mistyped slash command arrives here as text.
pub fn validate_slug(slug: &str) -> Result<(), String> {
    // The length is checked first because the messages below quote the answer,
    // and a paste arrives here as one line: bracketed paste turns a whole file
    // into a single composer line, which must not be echoed back in full.
    if slug.chars().count() > 64 {
        return Err("a model slug is at most 64 characters".to_owned());
    }
    if slug.starts_with('/') {
        return Err(format!(
            "`{slug}` is a command, not a model; press Enter to keep the current one"
        ));
    }
    if !slug.chars().any(char::is_alphanumeric)
        || !slug
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | ':' | '/'))
    {
        return Err(format!(
            "`{slug}` is not a model slug; use letters, digits, or - . _ : /"
        ));
    }
    Ok(())
}

/// The checked-out branch, read straight from `.git/HEAD`.
///
/// ponytail: a file read rather than `git rev-parse`, so the status row costs
/// no subprocess per prompt. It follows the `gitdir:` pointer a worktree or
/// submodule leaves behind, and reports a detached head as a short id. It does
/// not walk up to a parent repository: a workspace that is not itself a
/// checkout simply has no branch to show.
pub fn branch(workspace: &std::path::Path) -> Option<String> {
    let dot_git = workspace.join(".git");
    let git_dir = match std::fs::read_to_string(&dot_git) {
        Ok(pointer) => workspace.join(pointer.trim().strip_prefix("gitdir:")?.trim()),
        Err(_) => dot_git,
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    let name = match head.strip_prefix("ref: refs/heads/") {
        Some(name) => name,
        // Detached: the file holds the commit id itself.
        None => head.get(..8)?,
    };
    (!name.is_empty()).then(|| safe_text(name))
}

/// `stty size` is asked first: `COLUMNS` is inherited from the shell and goes
/// stale as soon as the window is resized.
pub fn terminal_width() -> usize {
    terminal_size(1, "COLUMNS", DEFAULT_WIDTH)
}

/// Rows, read the same way and on the same schedule as the width.
pub fn terminal_rows() -> usize {
    terminal_size(0, "LINES", DEFAULT_HEIGHT)
}

fn terminal_size(field: usize, variable: &str, default: usize) -> usize {
    stty(&["size"])
        .ok()
        .and_then(|size| size.split_whitespace().nth(field)?.parse().ok())
        .or_else(|| std::env::var(variable).ok().and_then(|v| v.parse().ok()))
        .filter(|size| *size > 0)
        .unwrap_or(default)
}

/// Strip SGR escapes (`ESC [ ... m`) so padding counts printed columns only.
fn strip_sgr(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(character) = chars.next() {
        if character == '\x1b' {
            // Consume `[ ... m`; an unterminated sequence drops to end of text.
            for escaped in chars.by_ref() {
                if escaped == 'm' {
                    break;
                }
            }
        } else {
            out.push(character);
        }
    }
    out
}

fn visible_len(text: &str) -> usize {
    UnicodeWidthStr::width(strip_sgr(text).as_str())
}

/// Truncate to `width` printed columns, keeping the SGR escapes that styled
/// the part that survives. A row cut by a narrow card keeps its colours.
fn fit(text: &str, width: usize) -> String {
    if visible_len(text) <= width {
        return text.to_owned();
    }
    let budget = width.saturating_sub(1);
    let mut out = String::with_capacity(text.len());
    let mut printed = 0;
    let mut chars = text.chars();
    while let Some(character) = chars.next() {
        if character == '\x1b' {
            out.push(character);
            for escaped in chars.by_ref() {
                out.push(escaped);
                if escaped == 'm' {
                    break;
                }
            }
            continue;
        }
        let columns = character.width().unwrap_or(0);
        if printed + columns > budget {
            break;
        }
        out.push(character);
        printed += columns;
    }
    out.push('…');
    if out.contains('\x1b') {
        out.push_str(RESET);
    }
    out
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TuiError {
    WrongSession,
    Gap { expected: u64, actual: u64 },
    CursorOverflow,
}

impl fmt::Display for TuiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongSession => formatter.write_str("event belongs to another session"),
            Self::Gap { expected, actual } => {
                write!(formatter, "event gap: expected {expected}, got {actual}")
            }
            Self::CursorOverflow => formatter.write_str("event cursor overflow"),
        }
    }
}

impl std::error::Error for TuiError {}

#[cfg(test)]
mod tests {
    use super::*;
    use arsy_kernel::{
        capability::{CapabilityAction, CapabilityRequirement},
        domain::{ApprovalId, CorrelationId, Principal, ResourceRef, StateVersion},
        event::{EventPayload, SchemaVersion},
        operation::OperationKind,
        policy::ApprovalRequest,
    };
    use std::time::{Duration, Instant};

    #[test]
    fn first_frame_stream_resize_no_colour_and_approval_are_complete() {
        let session = SessionId::new();
        let mut state = TuiState::new("/repo".into(), session);
        let started = Instant::now();
        let first = state.render(80, false);
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(first.contains(">_ ARSY CODE"));
        assert!(first.contains("sandbox:"));
        assert!(first.contains("none · read-only"));
        assert!(!first.contains("\x1b["));
        assert!(first.lines().all(|line| line.chars().count() == 80));
        // No model, no effort, and no checkout: the row still says what is
        // missing rather than dropping the field.
        assert_eq!(
            state.status_row(80, false, None),
            "  no model  effort:—  /repo"
        );
        state.set_effort(Some(Effort::High));
        assert_eq!(
            state.status_row(80, false, Some("feat/x")),
            "  no model  effort:high  feat/x  /repo"
        );
        state.set_effort(None);

        let event = EventEnvelope::new(
            session,
            1,
            Principal::System,
            None,
            CorrelationId::new(),
            SchemaVersion(1),
            "model.delta",
            EventPayload::Inline {
                data: serde_json::json!({"text": "streamed answer"}),
            },
        );
        state.apply(&event).unwrap();
        let narrow = state.render(20, false);
        assert!(narrow.contains("streamed answer"));
        assert!(narrow.lines().all(|line| line.chars().count() <= 20));

        let approval = ApprovalRequest {
            id: ApprovalId::new(),
            actor: Principal::System,
            operation: OperationKind::new("fs.write").unwrap(),
            requirement: CapabilityRequirement {
                action: CapabilityAction::FsWrite,
                resource: ResourceRef::new("file", "/repo/a").unwrap(),
            },
            operation_digest: StateVersion::from_digest([1; 32]),
            reversible: true,
            expires_at_ms: None,
            delegation_depth: 0,
            reason: "workspace rule requires consent".into(),
        };
        let prompt = TuiState::render_approval(&approval, 120);
        for label in ["Effect:", "Scope:", "Reversibility:", "Reason:"] {
            assert!(prompt.contains(label));
        }

        let wrong = EventEnvelope {
            session: SessionId::new(),
            ..event
        };
        assert_eq!(state.apply(&wrong), Err(TuiError::WrongSession));
    }

    #[test]
    fn codex_events_project_to_rows_and_never_abort_on_bad_input() {
        let row = |line: &str| render_codex_event(line, false);

        assert_eq!(
            row(r#"{"type":"turn.started"}"#),
            None,
            "transient status must not remain in scrollback"
        );
        assert_eq!(working_row(false), "  • Working…");
        assert_eq!(row(r#"{"type":"thread.started","thread_id":"t"}"#), None);
        assert_eq!(
            row(r#"{"type":"item.completed","item":{"type":"agent_message","text":"PONG\n"}}"#)
                .as_deref(),
            Some("PONG")
        );
        assert_eq!(
            row(r#"{"type":"item.completed","item":{"type":"command_execution","command":"/bin/zsh -lc \"cargo test\"","exit_code":1}}"#)
                .as_deref(),
            Some("  • Ran cargo test  → exit 1")
        );
        assert_eq!(
            row(r#"{"type":"item.completed","item":{"type":"command_execution","command":"bash -lc 'ls crates'","exit_code":0}}"#)
                .as_deref(),
            Some("  • Ran ls crates  → done")
        );
        assert_eq!(
            row(r#"{"type":"item.completed","item":{"type":"file_change","changes":[{"path":"a.rs","kind":"add"},{"path":"b.rs","kind":"update"}]}}"#)
                .as_deref(),
            Some("  • Added a.rs\n  • Edited b.rs")
        );
        assert_eq!(
            row(r#"{"type":"turn.failed","error":{"message":"x"}}"#),
            Some("  • x".into())
        );
        assert_eq!(
            row(r#"{"type":"error","message":"You've hit your usage limit."}"#).as_deref(),
            Some("  • You've hit your usage limit.")
        );
        // Codex reports some failures as an item, and provider transport
        // errors arrive as an embedded JSON body.
        assert_eq!(
            row(r#"{"type":"item.completed","item":{"type":"error","message":"Model metadata not found."}}"#)
                .as_deref(),
            Some("  • Model metadata not found.")
        );
        assert_eq!(
            row(r#"{"type":"error","message":"{\"type\":\"error\",\"status\":400,\"error\":{\"type\":\"invalid_request_error\",\"message\":\"The 'no-such-model' model is not supported.\"}}"}"#)
                .as_deref(),
            Some("  • The 'no-such-model' model is not supported.")
        );
        // An unknown item kind still renders; malformed input renders nothing.
        assert_eq!(
            row(r#"{"type":"item.completed","item":{"type":"future_kind"}}"#).as_deref(),
            Some("  • future_kind")
        );
        for bad in ["", "not json", "{}", r#"{"type":"item.completed"}"#] {
            assert_eq!(row(bad), None, "{bad}");
        }

        // Colour output stays a single printed column count.
        let painted = working_row(true);
        assert!(painted.contains("\x1b["));
        assert_eq!(visible_len(&painted), "  • Working…".chars().count());
    }

    #[test]
    fn the_model_picker_takes_a_number_a_slug_or_the_current_default() {
        let models = [
            ModelChoice {
                slug: "gpt-5.6-sol".into(),
                name: "GPT-5.6-Sol".into(),
            },
            ModelChoice {
                slug: "gpt-5.6-luna".into(),
                name: "GPT-5.6-Luna".into(),
            },
        ];
        let current = ModelRoute {
            provider: CODEX_PROVIDER.into(),
            model: "gpt-5.6-luna".into(),
        };
        let pick = |answer: &str| resolve_model(answer, &models, &current);
        assert_eq!(
            resolve_model("o3-custom", &models, &current)
                .unwrap()
                .provider,
            CODEX_PROVIDER,
            "picking a model never moves the turn to another provider"
        );

        assert_eq!(pick("1").unwrap().model, "gpt-5.6-sol");
        assert_eq!(
            pick("").unwrap().model,
            "gpt-5.6-luna",
            "empty keeps the current model"
        );
        assert_eq!(
            pick("  2  ").unwrap().model,
            "gpt-5.6-luna",
            "surrounding space is ignored"
        );
        assert_eq!(pick("o3-custom").unwrap().model, "o3-custom");
        assert_eq!(
            pick("openai/gpt-5.6:high").unwrap().model,
            "openai/gpt-5.6:high"
        );

        // A rejected answer keeps the current model and says why, because an
        // accepted one is written to the user configuration and would then
        // fail every later turn in every later session.
        for (answer, expected) in [
            ("9", "choose 1-2"),
            ("0", "choose 1-2"),
            ("/model gpt-5.6-luna", "is a command"),
            ("gpt 5.6", "not a model slug"),
            ("!!", "not a model slug"),
        ] {
            let reason = pick(answer).unwrap_err();
            assert!(reason.contains(expected), "{answer}: {reason}");
        }
        assert!(validate_slug("").is_err(), "an empty slug is not a model");
        assert!(validate_slug(&"a".repeat(65)).is_err());
        // A rejection quotes the answer, so an oversized one is refused on its
        // length before any message can echo it back.
        let pasted = format!("/{}", "x ".repeat(4096));
        let reason = validate_slug(&pasted).unwrap_err();
        assert!(reason.len() < 128, "{} bytes echoed", reason.len());

        // Every model is offered, and the current one is marked.
        let mut listing = Vec::new();
        render_model_list(&mut listing, &models, &current, false).unwrap();
        let listing = String::from_utf8(listing).unwrap();
        assert!(listing.contains("1. gpt-5.6-sol  GPT-5.6-Sol"));
        assert!(listing.contains("› 2. gpt-5.6-luna"));
        assert!(model_prompt(&models, &current, false).contains("model [gpt-5.6-luna] · 1-2"));
        assert!(
            model_prompt(&[], &current, false).contains("model [gpt-5.6-luna] · a slug"),
            "a configured endpoint offers no list, so it asks for a slug"
        );
    }

    #[test]
    fn a_remembered_route_names_its_provider_and_older_files_still_read() {
        let native = ModelRoute::parse("gateway/qwen3-coder");
        assert_eq!(native.provider, "gateway");
        assert_eq!(native.model, "qwen3-coder");
        assert!(!native.is_codex());
        assert_eq!(native.to_string(), "gateway/qwen3-coder");

        let legacy = ModelRoute::parse("gpt-5.6-luna");
        assert!(
            legacy.is_codex(),
            "a file written before routes named a provider meant Codex"
        );
        assert_eq!(legacy.model, "gpt-5.6-luna");
    }

    #[test]
    fn the_card_sets_the_mark_beside_its_text_and_keeps_colour_when_cut() {
        let mut state = TuiState::new(
            "/a/very/long/workspace/path/that/overflows".into(),
            SessionId::new(),
        );
        state.set_model_route(ModelRoute {
            provider: CODEX_PROVIDER.into(),
            model: "gpt-5.6-luna".into(),
        });

        let wide = state.render(92, true);
        let rows: Vec<&str> = wide.lines().collect();
        assert!(
            rows[1].contains(LOGO[0]),
            "mark starts on the first card row"
        );
        assert!(
            strip_sgr(rows[3]).contains(">_ ARSY CODE"),
            "text is centred against it"
        );
        assert_eq!(rows.len(), LOGO.len() + 2, "the mark sets the card height");
        for row in &rows {
            assert_eq!(visible_len(row), 92, "every row still reaches the border");
        }

        // A cut row keeps the styling of the part that survived.
        let cut = fit(&paint(true, CWD, "/a/very/long/path"), 8);
        assert_eq!(visible_len(&cut), 8);
        assert!(cut.starts_with(CWD));
        assert!(cut.ends_with(RESET));
        assert!(cut.contains('…'));

        // Too narrow for both: the text wins, the mark is dropped.
        let narrow = state.render(40, true);
        assert!(!narrow.contains(LOGO[0]));
        assert!(strip_sgr(&narrow).contains(">_ ARSY CODE"));
    }

    #[test]
    fn keys_decode_utf8_escape_sequences_and_control_characters() {
        let mut keys = Keys::default();
        let feed = |keys: &mut Keys, bytes: &[u8]| -> Vec<Key> {
            bytes.iter().filter_map(|byte| keys.feed(*byte)).collect()
        };

        assert_eq!(feed(&mut keys, b"hi"), [Key::Char('h'), Key::Char('i')]);
        // A multi-byte character is held back until it is complete.
        assert_eq!(keys.feed(0xc3), None);
        assert_eq!(keys.feed(0xa9), Some(Key::Char('é')));

        assert_eq!(feed(&mut keys, b"\r"), [Key::Enter]);
        assert_eq!(feed(&mut keys, b"\x7f"), [Key::Backspace]);
        assert_eq!(feed(&mut keys, &[0x03]), [Key::Interrupt]);
        assert_eq!(feed(&mut keys, &[0x04]), [Key::Eof]);
        assert_eq!(feed(&mut keys, b"\x1b[D"), [Key::Left]);
        assert_eq!(feed(&mut keys, b"\x1b[C"), [Key::Right]);
        assert_eq!(feed(&mut keys, b"\x1b[H"), [Key::Home]);
        assert_eq!(feed(&mut keys, b"\x1b[F"), [Key::End]);
        // An unbound sequence is swallowed whole, not leaked as characters.
        assert_eq!(feed(&mut keys, b"\x1b[5~"), []);
        assert_eq!(feed(&mut keys, b"x"), [Key::Char('x')], "decoder recovers");

        // Escape only becomes Interrupt once nothing follows it.
        assert_eq!(keys.feed(0x1b), None);
        assert_eq!(keys.flush_escape(), Some(Key::Interrupt));
        assert_eq!(keys.flush_escape(), None, "only fires once");
    }

    #[test]
    fn paste_history_delete_and_wide_input_remain_editable() {
        let mut keys = Keys::default();
        let mut composer = Composer::default();
        for byte in b"\x1b[200~first\nsecond\x03\x1b[201~" {
            if let Some(key) = keys.feed(*byte) {
                assert_ne!(key, Key::Enter);
                assert_ne!(key, Key::Interrupt);
                composer.press(key);
            }
        }
        assert_eq!(
            composer.press(Key::Enter),
            Action::Submit("first second".into())
        );
        composer.press(Key::Char('x'));
        composer.press(Key::Up);
        assert_eq!(composer.buffer, "first second");
        composer.press(Key::Down);
        assert_eq!(composer.buffer, "x");
        composer.press(Key::Home);
        composer.press(Key::Delete);
        assert_eq!(composer.buffer, "");
        composer.restore("界界界界界界界界界界".into());
        let (text, caret) = composer.window(10);
        assert!(UnicodeWidthStr::width(text.as_str()) < 10);
        assert_eq!(caret, UnicodeWidthStr::width(text.as_str()));
        let frame = composer.render(
            20,
            false,
            "status that is much longer than the terminal window",
        );
        assert!(frame.split('\n').nth(1).unwrap().width() < 20);
        assert_eq!(
            safe_text("hello\x1b]52;c;clipboard\x07\rworld"),
            "hello]52;c;clipboardworld"
        );
        keys.feed(0xc3);
        assert_eq!(keys.feed(b'a'), Some(Key::Char('a')));
        for byte in b"\x1b[123" {
            keys.feed(*byte);
        }
        keys.flush_escape();
        assert_eq!(keys.feed(b'b'), Some(Key::Char('b')));
    }

    #[test]
    fn mcp_progress_and_failures_are_visible_and_provider_frames_are_bounded() {
        let progress = render_codex_event(r#"{"type":"item.started","item":{"type":"mcp_tool_call","server":"docs","tool":"search","status":"in_progress"}}"#, false).unwrap();
        assert!(progress.contains("docs.search"));
        assert!(progress.contains("running"));
        let failure = render_codex_event(r#"{"type":"item.completed","item":{"type":"mcp_tool_call","server":"docs","tool":"search","status":"failed","error":{"message":"connection lost"}}}"#, false).unwrap();
        assert!(failure.contains("connection lost"));
        let events = provider_lines(std::io::Cursor::new(vec![b'x'; 1_048_577]));
        assert!(events
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("1 MiB"));
        let events = provider_lines(std::io::Cursor::new(b"{}\n\xff\n"));
        assert_eq!(
            events
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap(),
            "{}\n"
        );
        assert!(events
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn provider_ignoring_termination_can_be_forced_and_reaped() {
        use std::io::BufRead;
        use std::os::unix::process::CommandExt;
        let child = Command::new("sh")
            .args([
                "-c",
                "trap '' TERM; printf 'ready\\n'; while :; do sleep 1; done",
            ])
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut child = ProviderChild(child);
        let mut ready = String::new();
        std::io::BufReader::new(child.0.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready, "ready\n");
        child.stop(false);
        assert!(child.0.try_wait().unwrap().is_none());
        child.stop(true);
        assert!(!child.0.wait().unwrap().success());
    }

    #[test]
    fn a_slash_opens_a_command_menu_that_arrows_select_and_enter_takes() {
        let mut composer = Composer::default();
        composer.history.push_back("an earlier task".into());
        assert!(composer.menu().is_empty(), "a task line offers no menu");

        composer.press(Key::Char('/'));
        assert_eq!(composer.menu().len(), COMMANDS.len(), "`/` offers them all");

        // Up/Down move the selection, and history stays out of the way while
        // the menu is the list in front of the reader.
        assert_eq!(composer.press(Key::Down), Action::Redraw);
        assert_eq!(composer.press(Key::Down), Action::Redraw);
        assert_eq!(composer.press(Key::Up), Action::Redraw);
        assert_eq!(composer.selected, 1);
        assert_eq!(composer.buffer, "/", "history did not replace the line");
        for _ in 0..COMMANDS.len() - 2 {
            composer.press(Key::Down);
        }
        assert_eq!(
            composer.selected,
            COMMANDS.len() - 1,
            "the selection reaches the last row"
        );

        // Both ends wrap, so neither direction is a dead end.
        composer.press(Key::Down);
        assert_eq!(composer.selected, 0, "the last row wraps to the first");
        composer.press(Key::Up);
        assert_eq!(
            composer.selected,
            COMMANDS.len() - 1,
            "the first row wraps to the last"
        );
        composer.press(Key::Up);

        // Enter takes the highlighted command; a second Enter sends it, so a
        // line that is already a command is never held back.
        let highlighted = COMMANDS[COMMANDS.len() - 2].0;
        assert_eq!(composer.press(Key::Enter), Action::Redraw);
        assert_eq!(composer.buffer, highlighted);
        assert_eq!(
            composer.press(Key::Enter),
            Action::Submit(highlighted.to_owned())
        );

        // Typing narrows the list; an argument closes it, so Enter sends the
        // whole line instead of completing the command again.
        for character in "/mo".chars() {
            composer.press(Key::Char(character));
        }
        assert_eq!(
            composer.menu(),
            vec![("/model", "choose the provider model")]
        );
        for character in " x".chars() {
            composer.press(Key::Char(character));
        }
        assert!(composer.menu().is_empty(), "an argument closes the menu");
        assert_eq!(
            composer.press(Key::Enter),
            Action::Submit("/mo x".to_owned())
        );

        // The picker collects an answer, not a command, so it offers no menu.
        composer.set_picking(true);
        composer.press(Key::Char('/'));
        assert!(composer.menu().is_empty(), "the picker offers no commands");
        assert_eq!(composer.press(Key::Enter), Action::Submit("/".to_owned()));

        // `/help` and the menu are the same table, so neither can list a
        // command the other does not.
        let help = help(false);
        for (name, description) in COMMANDS {
            assert!(help.contains(name), "{name} is missing from /help");
            assert!(help.contains(description), "{name} has no description");
        }
        assert!(help.contains("Up/Down: input history"));
    }

    #[test]
    fn the_menu_extends_the_block_and_the_caret_still_lands_on_the_input() {
        let mut composer = Composer::default();
        composer.press(Key::Char('/'));
        let frame = composer.render(80, false, "  status");
        let rows: Vec<&str> = frame.split('\n').collect();
        assert_eq!(
            rows.len(),
            4 + COMMANDS.len(),
            "pad, input, pad, one row per command, status"
        );
        assert!(rows[3].contains("› /model"), "{:?}", rows[3]);
        assert!(rows[4].starts_with("    "), "only one row is marked");
        for row in &rows {
            assert!(visible_len(row) <= 80, "{row:?}");
        }
        // Up over the pad, the menu, and the status row, then across `› /`.
        assert!(
            frame.ends_with(&format!("\x1b[{}A\r\x1b[3C", COMMANDS.len() + 2)),
            "{frame:?}"
        );

        // A block taller than the screen would scroll, and the caret count back
        // to the input row would then land on the wrong one, so the menu takes
        // only the rows the terminal has left after pad, input, pad and status.
        composer.set_height(7);
        assert_eq!(composer.menu().len(), 3);
        assert_eq!(
            composer.render(80, false, "  status").split('\n').count(),
            7
        );
        composer.set_height(4);
        assert!(composer.menu().is_empty(), "no room leaves no menu");
        let frame = composer.render(80, false, "  status");
        assert_eq!(frame.split('\n').count(), 4);
        assert!(frame.ends_with("\x1b[2A\r\x1b[3C"), "{frame:?}");
        composer.set_height(0);

        // A narrowed list shrinks the block, and the previous one is erased
        // from the row above the input whatever height it had.
        composer.press(Key::Char('q'));
        let frame = composer.render(80, false, "  status");
        assert!(frame.starts_with(&format!("{RESET}{CARET_UP_1}\r{CLEAR_BELOW}")));
        assert_eq!(
            frame.split('\n').count(),
            5,
            "pad, input, pad, /quit, status"
        );
        assert!(frame.ends_with("\x1b[3A\r\x1b[4C"), "{frame:?}");
    }

    #[test]
    fn the_composer_edits_a_line_and_repaints_a_block_of_known_height() {
        let mut composer = Composer::default();
        for key in [Key::Char('a'), Key::Char('c')] {
            assert_eq!(composer.press(key), Action::Redraw);
        }
        composer.press(Key::Left);
        composer.press(Key::Char('b'));
        assert_eq!(composer.press(Key::Enter), Action::Submit("abc".into()));

        // Ctrl-C drops a drafted line; only an empty line ends the session.
        composer.press(Key::Char('x'));
        assert_eq!(
            composer.press(Key::Eof),
            Action::None,
            "Ctrl-D must not discard a drafted line"
        );
        assert_eq!(composer.press(Key::Interrupt), Action::Redraw);
        assert_eq!(composer.press(Key::Interrupt), Action::Quit);
        assert_eq!(composer.press(Key::Eof), Action::Quit);
        // Editing past either end is a no-op, never a panic.
        assert_eq!(composer.press(Key::Backspace), Action::None);
        assert_eq!(composer.press(Key::Left), Action::None);
        assert_eq!(composer.press(Key::Right), Action::None);

        let plain = composer.render(80, false, "  status");
        assert!(!plain.starts_with('\x1b'), "the first frame erases nothing");
        assert_eq!(plain.split('\n').count(), 4, "pad, input, pad, status");
        assert!(
            plain.ends_with("\x1b[2A\r\x1b[2C"),
            "caret returns to input"
        );

        // Every later frame erases the previous block from its first row.
        let painted = composer.render(80, true, "  status");
        assert!(painted.starts_with(&format!("{RESET}{CARET_UP_1}\r{CLEAR_BELOW}")));
        assert_eq!(painted.matches(INPUT_BG).count(), 4);
        assert_eq!(
            composer.clear(),
            format!("{RESET}{CARET_UP_1}\r{CLEAR_BELOW}")
        );
        assert_eq!(composer.clear(), "", "nothing is drawn twice");

        // A line longer than the row scrolls instead of wrapping, because a
        // wrap would add a row the block does not account for.
        let mut long = Composer::default();
        for character in "0123456789abcdefghijklmnopqrst".chars() {
            long.press(Key::Char(character));
        }
        let frame = long.render(MIN_WIDTH, false, "");
        let input = frame.split('\n').nth(1).unwrap();
        assert!(visible_len(input) <= MIN_WIDTH, "{input:?}");
        assert!(input.ends_with('t'), "the caret end stays visible");
        assert!(!input.contains('0'), "the start scrolled away");

        // Home scrolls the other way, back to the start of the line.
        long.press(Key::Home);
        let frame = long.render(MIN_WIDTH, false, "");
        let input = frame.split('\n').nth(1).unwrap();
        assert!(visible_len(input) <= MIN_WIDTH, "{input:?}");
        assert!(input.starts_with("› 0"), "{input:?}");
        assert!(!input.contains('t'), "the far end scrolled away");
        assert!(
            frame.ends_with("\x1b[2A\r\x1b[2C"),
            "caret sits at column 0"
        );
    }
}
