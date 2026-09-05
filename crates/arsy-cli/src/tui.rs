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
};
use serde_json::Value;
use std::{
    fmt,
    io::Write,
    process::{Command, Stdio},
};

const MAX_TIMELINE_EVENTS: usize = 1_000;
const DEFAULT_WIDTH: usize = 80;
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
    pub fn acquire() -> Self {
        let saved = stty(&["-g"]).map(|mode| mode.trim().to_owned()).ok();
        let _ = stty(&["-echo", "-icanon", "-isig", "min", "1", "time", "0"]);
        Self { saved }
    }
}

impl Drop for RawTerminal {
    /// Runs on every exit path, including unwind, so the shell is never left
    /// in raw mode.
    fn drop(&mut self) {
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
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Read stdin bytes on a thread, so the main loop can watch keys and provider
/// output at the same time — that is what makes a turn interruptible.
pub fn spawn_key_reader() -> std::sync::mpsc::Receiver<u8> {
    let (sender, receiver) = std::sync::mpsc::channel();
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
}

impl Keys {
    pub fn feed(&mut self, byte: u8) -> Option<Key> {
        if self.pending.first() == Some(&0x1b) {
            return self.feed_escape(byte);
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
        if !(0x40..=0x7e).contains(&byte) || self.pending.len() == 2 {
            return None;
        }
        let sequence = std::mem::take(&mut self.pending);
        match (sequence.last(), sequence.get(2)) {
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
        (self.pending.as_slice() == [0x1b]).then(|| {
            self.pending.clear();
            Key::Interrupt
        })
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

/// The input line ARSY owns: its text, its caret, and how many rows it last
/// painted. Nothing else writes to those rows, so redrawing is exact.
#[derive(Default)]
pub struct Composer {
    buffer: String,
    caret: usize,
    drawn: bool,
}

impl Composer {
    pub fn press(&mut self, key: Key) -> Action {
        match key {
            Key::Char(character) => {
                self.buffer.insert(self.byte_at(self.caret), character);
                self.caret += 1;
                Action::Redraw
            }
            Key::Backspace if self.caret > 0 => {
                self.buffer.remove(self.byte_at(self.caret - 1));
                self.caret -= 1;
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
            Key::Enter => Action::Submit(self.take()),
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

    fn take(&mut self) -> String {
        self.caret = 0;
        std::mem::take(&mut self.buffer)
    }

    fn byte_at(&self, caret: usize) -> usize {
        self.buffer
            .char_indices()
            .nth(caret)
            .map_or(self.buffer.len(), |(at, _)| at)
    }

    /// Paint the block — pad, input, pad, status — and leave the caret in the
    /// input line where the next character belongs.
    ///
    /// A previous block is erased first: the caret always rests on the input
    /// row, one row into the block.
    pub fn render(&mut self, width: usize, colour: bool, status: &str) -> String {
        let width = width.max(MIN_WIDTH);
        let room = width.saturating_sub(3);
        let (text, caret) = self.window(room);
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
            "{surface}\n{surface}{} {text}{}\n{surface}{}\n{status}",
            if colour {
                format!("{INPUT_BG}›")
            } else {
                "›".to_owned()
            },
            if colour { CLEAR_EOL } else { "" },
            if colour { RESET } else { "" },
        ));
        // Back onto the input row, then across `› ` and the text before the
        // caret. Nothing here can wrap: `window` bounded the text to the row.
        frame.push_str(&format!("\x1b[2A\r\x1b[{}C", caret + 2));
        frame
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
        if characters.len() < room {
            return (self.buffer.clone(), self.caret);
        }
        let last = room.saturating_sub(1);
        let start = self.caret.saturating_sub(last);
        (
            characters[start..(start + last).min(characters.len())]
                .iter()
                .collect(),
            self.caret - start,
        )
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
    if colour {
        format!("{code}{text}{RESET}")
    } else {
        text.to_owned()
    }
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
        }
    }

    pub fn set_sandbox_assurance(&mut self, assurance: SandboxAssurance) {
        self.sandbox_assurance = assurance;
    }

    pub fn set_model_route(&mut self, route: ModelRoute) {
        self.model_route = Some(route);
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
    pub fn status_row(&self, width: usize, colour: bool) -> String {
        let route = self
            .model_route
            .as_ref()
            .map_or_else(|| "no model".to_owned(), ModelRoute::to_string);
        fit(
            &format!(
                "  {}  {}",
                paint(colour, MODEL, &route),
                paint(colour, CWD, &self.workspace),
            ),
            width.max(MIN_WIDTH),
        )
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
        "item.completed" => render_codex_item(event.get("item")?, colour),
        "error" => Some(error_row(
            colour,
            event.get("message").and_then(Value::as_str)?,
        )),
        // `turn.failed` repeats the `error` event verbatim, and a failure with
        // no `error` still reaches the caller as a non-zero exit status.
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
            None,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelRoute {
    pub model: String,
}

impl fmt::Display for ModelRoute {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "codex/{}", self.model)
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
pub fn model_prompt(models: &[ModelChoice], current: &ModelRoute, colour: bool) -> String {
    paint(
        colour,
        DIM,
        &format!("  model [{}] · 1-{} or a slug", current.model, models.len()),
    )
}

/// Resolve a picker answer: a list index, a slug typed in full, or an empty
/// line to keep the current model.
pub fn resolve_model(answer: &str, models: &[ModelChoice], current: &ModelRoute) -> ModelRoute {
    let answer = answer.trim();
    let model = match answer.parse::<usize>() {
        Ok(number) => models
            .get(number.checked_sub(1).unwrap_or(usize::MAX))
            .map_or_else(|| current.model.clone(), |choice| choice.slug.clone()),
        Err(_) if answer.is_empty() => current.model.clone(),
        Err(_) => answer.to_owned(),
    };
    ModelRoute { model }
}

/// `stty size` is asked first: `COLUMNS` is inherited from the shell and goes
/// stale as soon as the window is resized.
pub fn terminal_width() -> usize {
    stty(&["size"])
        .ok()
        .and_then(|size| size.split_whitespace().nth(1)?.parse().ok())
        .or_else(|| std::env::var("COLUMNS").ok().and_then(|v| v.parse().ok()))
        .filter(|width| *width > 0)
        .unwrap_or(DEFAULT_WIDTH)
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
    strip_sgr(text).chars().count()
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
        if printed == budget {
            break;
        }
        out.push(character);
        printed += 1;
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
        assert_eq!(state.status_row(80, false), "  no model  /repo");

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
            None
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
            model: "gpt-5.6-luna".into(),
        };
        let pick = |answer: &str| resolve_model(answer, &models, &current).model;

        assert_eq!(pick("1"), "gpt-5.6-sol");
        assert_eq!(pick(""), "gpt-5.6-luna", "empty keeps the current model");
        assert_eq!(pick("9"), "gpt-5.6-luna", "out of range keeps it");
        assert_eq!(pick("0"), "gpt-5.6-luna", "zero keeps it");
        assert_eq!(pick("o3-custom"), "o3-custom", "free text is a slug");
        assert_eq!(
            pick("  2  "),
            "gpt-5.6-luna",
            "surrounding space is ignored"
        );

        // Every model is offered, and the current one is marked.
        let mut listing = Vec::new();
        render_model_list(&mut listing, &models, &current, false).unwrap();
        let listing = String::from_utf8(listing).unwrap();
        assert!(listing.contains("1. gpt-5.6-sol  GPT-5.6-Sol"));
        assert!(listing.contains("› 2. gpt-5.6-luna"));
        assert!(model_prompt(&models, &current, false).contains("model [gpt-5.6-luna] · 1-2"));
    }

    #[test]
    fn the_card_sets_the_mark_beside_its_text_and_keeps_colour_when_cut() {
        let mut state = TuiState::new(
            "/a/very/long/workspace/path/that/overflows".into(),
            SessionId::new(),
        );
        state.set_model_route(ModelRoute {
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
