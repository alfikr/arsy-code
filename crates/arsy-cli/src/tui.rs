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

const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

/// One SGR prefix per visual role the renderer paints. Owned strings, because
/// `[theme]` in the configuration can replace any of them with a colour the
/// operator picked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Palette {
    pub assistant: String,
    pub dim: String,
    pub accent: String,
    pub ok: String,
    pub err: String,
    pub run: String,
    pub model: String,
    pub cwd: String,
    pub border: String,
    pub bullet: String,
    pub input_bg: String,
}

/// The role names `[theme]` keys and the picker's error messages use, in the
/// order [`Palette::from_codes`] takes them.
pub const THEME_ROLES: &[&str] = &[
    "assistant",
    "dim",
    "accent",
    "ok",
    "err",
    "run",
    "model",
    "cwd",
    "border",
    "bullet",
    "input_bg",
];

impl Palette {
    fn from_codes(codes: [&str; 11]) -> Self {
        Self {
            assistant: codes[0].to_owned(),
            dim: codes[1].to_owned(),
            accent: codes[2].to_owned(),
            ok: codes[3].to_owned(),
            err: codes[4].to_owned(),
            run: codes[5].to_owned(),
            model: codes[6].to_owned(),
            cwd: codes[7].to_owned(),
            border: codes[8].to_owned(),
            bullet: codes[9].to_owned(),
            input_bg: codes[10].to_owned(),
        }
    }

    fn slot(&mut self, role: &str) -> Option<&mut String> {
        Some(match role {
            "assistant" => &mut self.assistant,
            "dim" => &mut self.dim,
            "accent" => &mut self.accent,
            "ok" => &mut self.ok,
            "err" => &mut self.err,
            "run" => &mut self.run,
            "model" => &mut self.model,
            "cwd" => &mut self.cwd,
            "border" => &mut self.border,
            "bullet" => &mut self.bullet,
            "input_bg" => &mut self.input_bg,
            _ => return None,
        })
    }

    /// Replace roles from a `name -> "#rrggbb"` map. A `#rrggbb` becomes a
    /// foreground prefix; `input_bg` a background one. An unknown role or a
    /// malformed colour is rejected rather than ignored, so a typo in the
    /// configuration is seen.
    pub fn with_overrides(
        mut self,
        overrides: &std::collections::BTreeMap<String, String>,
    ) -> Result<Self, String> {
        for (role, hex) in overrides {
            let code = hex_to_sgr(hex, role == "input_bg")
                .map_err(|why| format!("[theme].{role}: {why}"))?;
            *self.slot(role).ok_or_else(|| {
                format!(
                    "[theme] has no role `{role}`; expected one of {}",
                    THEME_ROLES.join(", ")
                )
            })? = code;
        }
        Ok(self)
    }
}

/// The themes `/theme` offers: name, then the line the picker shows. All are
/// built for a dark terminal — the surface the composer already draws over —
/// so the picker can preview one by just repainting.
pub const THEMES: &[(&str, &str)] = &[
    ("dark", "the original — grey text, cyan accents"),
    ("ocean", "cool — teal and blue"),
    ("sunset", "warm — amber and rose"),
    (
        "vivid",
        "vivid — vibrant, high-contrast, multi-colored accents",
    ),
    ("dracula", "dracula — iconic purple, cyan, green, and pink"),
    ("nord", "nord — arctic frost and cool pastel accents"),
    ("mono", "greys only, no hue"),
];

/// The theme in force when nothing has been chosen: the original palette.
pub const DEFAULT_THEME: &str = "dark";

/// A built-in theme's palette, or `None` when the name is not one.
pub fn builtin_palette(name: &str) -> Option<Palette> {
    // `dark` is the historical `brainless` palette, unchanged.
    Some(match name {
        "dark" => Palette::from_codes([
            "\x1b[38;2;201;201;201m",
            "\x1b[38;2;122;122;122m",
            "\x1b[38;2;92;194;224m",
            "\x1b[38;2;78;169;111m",
            "\x1b[38;2;247;118;142m",
            "\x1b[38;2;224;175;104m",
            "\x1b[38;2;246;226;183m",
            "\x1b[38;2;171;223;167m",
            "\x1b[38;2;58;58;58m",
            "\x1b[38;2;167;167;167m",
            "\x1b[48;2;53;53;53m",
        ]),
        // assistant, dim, accent, ok, err, run, model, cwd, border, bullet, input_bg
        "vivid" | "omp" => Palette::from_codes([
            "\x1b[38;2;230;237;243m",
            "\x1b[38;2;139;148;158m",
            "\x1b[38;2;88;166;255m",
            "\x1b[38;2;63;185;80m",
            "\x1b[38;2;248;81;73m",
            "\x1b[38;2;227;179;65m",
            "\x1b[38;2;210;168;255m",
            "\x1b[38;2;86;211;100m",
            "\x1b[38;2;88;166;255m",
            "\x1b[38;2;255;166;87m",
            "\x1b[48;2;22;27;34m",
        ]),
        "dracula" => Palette::from_codes([
            "\x1b[38;2;248;248;242m",
            "\x1b[38;2;98;114;164m",
            "\x1b[38;2;189;147;249m",
            "\x1b[38;2;80;250;123m",
            "\x1b[38;2;255;85;85m",
            "\x1b[38;2;241;250;140m",
            "\x1b[38;2;255;121;198m",
            "\x1b[38;2;139;233;253m",
            "\x1b[38;2;189;147;249m",
            "\x1b[38;2;255;184;108m",
            "\x1b[48;2;40;42;54m",
        ]),
        "nord" => Palette::from_codes([
            "\x1b[38;2;236;239;244m",
            "\x1b[38;2;129;161;193m",
            "\x1b[38;2;136;192;208m",
            "\x1b[38;2;163;190;140m",
            "\x1b[38;2;191;97;106m",
            "\x1b[38;2;235;203;139m",
            "\x1b[38;2;180;142;173m",
            "\x1b[38;2;143;188;187m",
            "\x1b[38;2;136;192;208m",
            "\x1b[38;2;208;135;112m",
            "\x1b[48;2;46;52;64m",
        ]),
        // assistant, dim, accent, ok, err, run, model, cwd, border, bullet, input_bg
        "ocean" => Palette::from_codes([
            "\x1b[38;2;205;214;224m",
            "\x1b[38;2;107;122;137m",
            "\x1b[38;2;79;201;201m",
            "\x1b[38;2;95;208;160m",
            "\x1b[38;2;244;132;156m",
            "\x1b[38;2;217;176;106m",
            "\x1b[38;2;215;230;230m",
            "\x1b[38;2;143;214;192m",
            "\x1b[38;2;55;67;76m",
            "\x1b[38;2;127;149;160m",
            "\x1b[48;2;36;48;56m",
        ]),
        "sunset" => Palette::from_codes([
            "\x1b[38;2;224;212;200m",
            "\x1b[38;2;138;122;108m",
            "\x1b[38;2;230;168;79m",
            "\x1b[38;2;168;201;106m",
            "\x1b[38;2;244;125;146m",
            "\x1b[38;2;224;138;74m",
            "\x1b[38;2;242;226;183m",
            "\x1b[38;2;188;212;154m",
            "\x1b[38;2;74;63;56m",
            "\x1b[38;2;160;140;124m",
            "\x1b[48;2;51;42;36m",
        ]),
        "mono" => Palette::from_codes([
            "\x1b[38;2;220;220;220m",
            "\x1b[38;2;122;122;122m",
            "\x1b[38;2;245;245;245m",
            "\x1b[38;2;200;200;200m",
            "\x1b[38;2;235;235;235m",
            "\x1b[38;2;180;180;180m",
            "\x1b[38;2;235;235;235m",
            "\x1b[38;2;205;205;205m",
            "\x1b[38;2;74;74;74m",
            "\x1b[38;2;160;160;160m",
            "\x1b[48;2;42;42;42m",
        ]),
        _ => return None,
    })
}

// The renderer reads the palette through the `sgr_*` helpers below, so a theme
// swap needs no change past `activate_palette`.
//
// ponytail: a process-global, not a value threaded through every render
// function — the TUI shows one session in one theme. `activate_palette` leaks
// one `Palette` the first time each distinct palette is set, so the helpers can
// hand out `&'static str`; an unchanged palette is a no-op, so repainting the
// theme picker on every keystroke does not accumulate anything. Thread a
// `&Palette` only if a split view ever needs two themes at once.
static ACTIVE_PALETTE: std::sync::RwLock<Option<&'static Palette>> = std::sync::RwLock::new(None);

/// Make `palette` the one the renderer paints with from now on. Safe to call
/// again — on every frame, even — when `/theme` previews or changes it.
pub fn activate_palette(palette: Palette) {
    if let Ok(mut active) = ACTIVE_PALETTE.write() {
        if active.map(|current| *current == palette).unwrap_or(false) {
            return;
        }
        *active = Some(Box::leak(Box::new(palette)));
    }
}

/// Activate a built-in theme palette with optional role overrides.
pub fn set_palette(name: &str, roles: &std::collections::BTreeMap<String, String>) {
    if let Some(mut palette) = builtin_palette(name) {
        if !roles.is_empty() {
            if let Ok(overridden) = palette.clone().with_overrides(roles) {
                palette = overridden;
            }
        }
        activate_palette(palette);
    }
}

fn palette() -> &'static Palette {
    if let Some(active) = ACTIVE_PALETTE.read().ok().and_then(|active| *active) {
        return active;
    }
    static DEFAULT: std::sync::OnceLock<Palette> = std::sync::OnceLock::new();
    DEFAULT.get_or_init(|| builtin_palette(DEFAULT_THEME).expect("`dark` is built in"))
}

fn sgr_assistant() -> &'static str {
    &palette().assistant
}
fn sgr_dim() -> &'static str {
    &palette().dim
}
fn sgr_accent() -> &'static str {
    &palette().accent
}
fn sgr_ok() -> &'static str {
    &palette().ok
}
fn sgr_err() -> &'static str {
    &palette().err
}
fn sgr_run() -> &'static str {
    &palette().run
}
fn sgr_model() -> &'static str {
    &palette().model
}
fn sgr_cwd() -> &'static str {
    &palette().cwd
}
fn sgr_border() -> &'static str {
    &palette().border
}
fn sgr_bullet() -> &'static str {
    &palette().bullet
}
/// Codex `user_message_bg`: white at 12% over the `#1a1a1a` terminal surface.
fn sgr_input_bg() -> &'static str {
    &palette().input_bg
}

/// `#rrggbb` to an SGR prefix — foreground, or background when `background`.
fn hex_to_sgr(hex: &str, background: bool) -> Result<String, String> {
    let body = hex.strip_prefix('#').unwrap_or(hex);
    if body.len() != 6 || !body.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("`{hex}` is not a #rrggbb colour"));
    }
    let channel = |at: usize| u8::from_str_radix(&body[at..at + 2], 16).unwrap_or(0);
    let (red, green, blue) = (channel(0), channel(2), channel(4));
    let lead = if background { 48 } else { 38 };
    Ok(format!("\x1b[{lead};2;{red};{green};{blue}m"))
}

/// Take an answer to the `/theme` picker: a list number, a theme name, or an
/// empty line to keep what is set. A rejected answer reports why, like the
/// effort picker, because an accepted one is written to the user configuration.
pub fn resolve_theme_answer(line: &str, current: &str) -> Result<String, String> {
    let answer = line.trim();
    if answer.is_empty() {
        return Ok(current.to_owned());
    }
    if let Ok(number) = answer.parse::<usize>() {
        return THEMES
            .get(
                number
                    .checked_sub(1)
                    .ok_or_else(|| format!("`{answer}` is out of range; the list starts at 1"))?,
            )
            .map(|(name, _)| (*name).to_owned())
            .ok_or_else(|| format!("`{answer}` is not on the list"));
    }
    THEMES
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(answer))
        .map(|(name, _)| (*name).to_owned())
        .ok_or_else(|| {
            format!(
                "`{}` is not a theme; use {}",
                safe_text(answer),
                THEMES
                    .iter()
                    .map(|(name, _)| *name)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

/// The row the `/theme` picker opens on: the theme in force.
pub fn theme_row(current: &str) -> usize {
    THEMES
        .iter()
        .position(|(name, _)| name.eq_ignore_ascii_case(current))
        .unwrap_or(0)
}

pub fn theme_prompt(current: &str, colour: bool) -> String {
    paint(
        colour,
        sgr_dim(),
        &format!(
            "  theme [{current}] · Up/Down then Enter, a name, or 1-{}",
            THEMES.len()
        ),
    )
}
const CLEAR_EOL: &str = "\x1b[K";
#[cfg(test)]
const CARET_UP_1: &str = "\x1b[1A";
#[cfg(test)]
const CARET_UP_2: &str = "\x1b[2A";
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
    #[cfg(unix)]
    if let Ok(tty) = std::fs::File::open("/dev/tty") {
        if let Ok(output) = Command::new("stty").args(args).stdin(tty).output() {
            if output.status.success() {
                return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
            }
        }
    }
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
    WordBackspace,
    Left,
    Right,
    WordLeft,
    WordRight,
    Home,
    End,
    Delete,
    Up,
    Down,
    Enter,
    /// Shift+Enter or Alt+Enter to insert a newline without submitting.
    Newline,
    /// Shift+Tab: step to the next approval mode without leaving the line.
    CycleMode,
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
            0x01 => return Some(Key::Home),
            0x03 => return Some(Key::Interrupt),
            0x04 => return Some(Key::Eof),
            0x05 => return Some(Key::End),
            0x17 => return Some(Key::WordBackspace),
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
        if self.pending.len() == 1 {
            match byte {
                b'\r' | b'\n' => {
                    self.pending.clear();
                    return Some(Key::Newline);
                }
                b'b' | b'B' => {
                    self.pending.clear();
                    return Some(Key::WordLeft);
                }
                b'f' | b'F' => {
                    self.pending.clear();
                    return Some(Key::WordRight);
                }
                0x7f | 0x08 => {
                    self.pending.clear();
                    return Some(Key::WordBackspace);
                }
                b if !matches!(b, b'[' | b'O') => {
                    self.pending.clear();
                    return self.feed(byte).or(Some(Key::Interrupt));
                }
                _ => {}
            }
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
        if sequence == b"\x1b[13;2u"
            || sequence == b"\x1b[13;3u"
            || sequence == b"\x1b[13;5u"
            || sequence == b"\x1b[27;2;13~"
            || sequence == b"\x1b[27;3;13~"
            || sequence == b"\x1b[27;5;13~"
            || sequence == b"\x1bOM"
            || sequence == b"\x1b[13~"
        {
            return Some(Key::Newline);
        }
        if sequence == b"\x1b[1;3D"
            || sequence == b"\x1b[1;5D"
            || sequence == b"\x1b[5D"
            || sequence == b"\x1b[1;4D"
        {
            return Some(Key::WordLeft);
        }
        if sequence == b"\x1b[1;3C"
            || sequence == b"\x1b[1;5C"
            || sequence == b"\x1b[5C"
            || sequence == b"\x1b[1;4C"
        {
            return Some(Key::WordRight);
        }
        if sequence == b"\x1b[1;9D" || sequence == b"\x1b[1;2D" {
            return Some(Key::Home);
        }
        if sequence == b"\x1b[1;9C" || sequence == b"\x1b[1;2C" {
            return Some(Key::End);
        }
        if sequence == b"\x1b[3;3~" || sequence == b"\x1b[3;5~" {
            return Some(Key::WordBackspace);
        }
        // Shift+Tab. `CSI Z` is what every terminal here sends; the modified
        // form is what a terminal in kitty-style key reporting sends instead.
        if sequence == b"\x1b[Z" || sequence == b"\x1b[1;2Z" {
            return Some(Key::CycleMode);
        }
        match (sequence.last(), sequence.get(2)) {
            (Some(b'A'), _) => Some(Key::Up),
            (Some(b'B'), _) => Some(Key::Down),
            (Some(b'~'), Some(b'3')) => Some(Key::Delete),
            (Some(b'D'), _) => Some(Key::Left),
            (Some(b'C'), _) => Some(Key::Right),
            (Some(b'H'), _) | (Some(b'~'), Some(b'1')) | (Some(b'~'), Some(b'7')) => {
                Some(Key::Home)
            }
            (Some(b'F'), _) | (Some(b'~'), Some(b'4')) | (Some(b'~'), Some(b'8')) => Some(Key::End),
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

/// What Shift+Tab submits. A command rather than a new action, so the mode is
/// changed by the one dispatch arm that already knows how to announce it.
pub const CYCLE_APPROVAL_MODE: &str = "/approval cycle";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    Submit(String),
    Quit,
    Redraw,
    None,
}

/// The slash commands the composer offers and `/help` prints. One table, so a
/// command cannot appear in the menu and not in the help, or the reverse.
pub const COMMANDS: &[(&str, &str)] = &[
    ("/new", "start a fresh session"),
    ("/clear", "clear conversation context in place"),
    ("/resume", "resume a recorded session; [SESSION_ID]"),
    ("/rename", "rename current session; <TITLE>"),
    (
        "/session",
        "manage sessions; list | rename <TITLE> | delete [ID]",
    ),
    (
        "/approval",
        "set approval mode; default | acceptEdits | plan | auto | dontAsk | bypassPermissions",
    ),
    ("/plan", "plan a task; approve | revise [NOTE] | cancel"),
    ("/provider", "choose, add, or remove a provider endpoint"),
    ("/model", "choose the provider model"),
    ("/effort", "set reasoning effort; low | medium | high | off"),
    (
        "/theme",
        "choose the colour theme; dark | ocean | sunset | mono",
    ),
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
    (
        "/auth",
        "manage credentials; list | login PROVIDER | set PROVIDER | remove HANDLE",
    ),
    (
        "/compat",
        "explain ecosystem mapping; claude | codex | omp | agents",
    ),
    ("/update", "check for and install arsy-code updates"),
    ("/help", "show these actions"),
    ("/quit", "exit"),
];

/// The rows `/provider` offers under the list of configured providers.
pub const PROVIDER_ACTIONS: &[(&str, &str)] = &[
    (
        "+new",
        "add a provider: name, dialect, URL, model, credential",
    ),
    ("-remove", "remove a provider from the configuration"),
];

pub const AUTH_ACTIONS: &[(&str, &str)] = &[
    (
        "login",
        "sign in to a provider with OAuth (browser / device flow)",
    ),
    ("list", "show saved credentials in catalog"),
    ("set", "store an API key for a provider"),
    ("remove", "delete a credential from catalog"),
];

/// What `/auth` is collecting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthStep {
    Pick,
    LoginProvider,
    SetProvider,
    SetKey,
    RemoveHandle,
}

impl AuthStep {
    pub fn prompt(self, draft: &str, colour: bool) -> String {
        let text = match self {
            Self::Pick => "auth · Up/Down then Enter, or an action".to_owned(),
            Self::LoginProvider => "sign in to which provider · Up/Down then Enter".to_owned(),
            Self::SetProvider => "store key for which provider · Up/Down then Enter".to_owned(),
            Self::SetKey => format!("credential for {draft} · not shown as you type"),
            Self::RemoveHandle => "remove which credential · Up/Down then Enter".to_owned(),
        };
        paint(colour, sgr_dim(), &format!("  {text}"))
    }

    pub fn rows(self, providers: &[String], handles: &[String]) -> Option<Vec<(String, String)>> {
        let named = |rows: &[(&str, &str)]| {
            Some(
                rows.iter()
                    .map(|(name, description)| ((*name).to_owned(), (*description).to_owned()))
                    .collect(),
            )
        };
        match self {
            Self::Pick => named(AUTH_ACTIONS),
            Self::SetProvider => Some(
                providers
                    .iter()
                    .map(|p| (p.clone(), format!("configured endpoint `{p}`")))
                    .collect(),
            ),
            Self::LoginProvider => {
                let mut rows: Vec<(String, String)> = providers
                    .iter()
                    .map(|p| (p.clone(), format!("configured endpoint `{p}`")))
                    .collect();
                // Built-in presets that are not already configured: signing in
                // to one writes its endpoint.
                for preset in arsy_kernel::oauth::presets::all() {
                    if !providers.iter().any(|p| p == preset.id) {
                        rows.push((preset.id.to_owned(), preset.label.to_owned()));
                    }
                }
                Some(rows)
            }
            Self::RemoveHandle => Some(
                handles
                    .iter()
                    .map(|h| (h.clone(), "saved credential".to_owned()))
                    .collect(),
            ),
            Self::SetKey => None,
        }
    }

    pub fn masked(self) -> bool {
        matches!(self, Self::SetKey)
    }
}
/// The dialects an endpoint can speak. Same two the configuration accepts.
pub const PROVIDER_KINDS: &[(&str, &str)] = &[
    ("openai", "Chat Completions, and anything that speaks it"),
    ("anthropic", "Anthropic Messages"),
];

/// Where a credential typed into the TUI is put.
pub const PROVIDER_STORES: &[(&str, &str)] = &[
    (
        "file",
        "a 0600 file beside the configuration; no unlock prompt",
    ),
    ("keychain", "the OS credential store"),
];

pub const CONFIRM_ROWS: &[(&str, &str)] = &[("no", "keep it"), ("yes", "remove it")];

/// What `/provider` is collecting. One variant per question, so the loop always
/// knows which answer it is holding and what to ask next.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderStep {
    /// Pick a configured provider, or one of the actions under them.
    Pick,
    Name,
    Kind,
    BaseUrl,
    Model,
    Store,
    /// The credential itself, typed masked.
    Key,
    /// Which provider to remove.
    Remove,
    /// Confirm that removal, because it rewrites the operator's file.
    ConfirmRemove,
}

impl ProviderStep {
    /// The prompt line shown under the composer while this step collects.
    pub fn prompt(self, draft: &ProviderDraft, colour: bool) -> String {
        let text = match self {
            Self::Pick => "provider · Up/Down then Enter, or a name".to_owned(),
            Self::Name => "new provider · a short id, letters and dashes".to_owned(),
            Self::Kind => "dialect · Up/Down then Enter, or a name".to_owned(),
            Self::BaseUrl => format!("base URL for {} · the API root", draft.name),
            Self::Model => format!(
                "models for {} · one slug, or several separated by commas",
                draft.name
            ),
            Self::Store => "where to keep the credential · Up/Down then Enter".to_owned(),
            Self::Key => format!("credential for {} · not shown as you type", draft.name),
            Self::Remove => "remove which provider · Up/Down then Enter".to_owned(),
            Self::ConfirmRemove => {
                format!("remove `{}` from the configuration?", draft.name)
            }
        };
        paint(colour, sgr_dim(), &format!("  {text}"))
    }

    /// The rows this step offers, or none when it collects free text.
    /// `running` is the provider this session resolved at startup; `default` is
    /// what the configuration names now. They differ between a switch and the
    /// restart that picks it up, and saying so is the whole point of the
    /// marker.
    pub fn rows(
        self,
        providers: &[String],
        running: &str,
        default: Option<&str>,
    ) -> Option<Vec<(String, String)>> {
        let named = |rows: &[(&str, &str)]| {
            Some(
                rows.iter()
                    .map(|(name, description)| ((*name).to_owned(), (*description).to_owned()))
                    .collect(),
            )
        };
        match self {
            Self::Pick => {
                // Which one is in force has to be on the row: adding a provider
                // makes it the default, and without a marker the one it
                // replaced reads as gone rather than as merely not current.
                let mut rows: Vec<(String, String)> = providers
                    .iter()
                    .map(|name| {
                        let note = if name == running {
                            "in use"
                        } else if default == Some(name.as_str()) {
                            "chosen · in use after a restart"
                        } else {
                            "switch to this provider"
                        };
                        (name.clone(), note.to_owned())
                    })
                    .collect();
                for (name, description) in PROVIDER_ACTIONS {
                    // Nothing to remove until something is configured.
                    if *name == "-remove" && providers.is_empty() {
                        continue;
                    }
                    rows.push(((*name).to_owned(), (*description).to_owned()));
                }
                Some(rows)
            }
            Self::Kind => named(PROVIDER_KINDS),
            Self::Store => named(PROVIDER_STORES),
            Self::ConfirmRemove => named(CONFIRM_ROWS),
            Self::Remove => Some(
                providers
                    .iter()
                    .map(|name| (name.clone(), "remove this one".to_owned()))
                    .collect(),
            ),
            Self::Name | Self::BaseUrl | Self::Model | Self::Key => None,
        }
    }

    /// Whether the answer to this step is a secret.
    pub const fn masked(self) -> bool {
        matches!(self, Self::Key)
    }
}

/// What `/provider` has collected so far.
#[derive(Clone, Debug, Default)]
pub struct ProviderDraft {
    pub name: String,
    pub kind: String,
    pub base_url: String,
    /// Every model the endpoint offers. The first is its default.
    pub models: Vec<String>,
    pub store: String,
}

/// The levels the effort picker offers. Rows in the same shape the command menu
/// takes, so the picker is arrowed and taken with the keys the composer already
/// answers rather than a second selection mechanism.
pub const EFFORT_ROWS: &[(&str, &str)] = &[
    ("low", "least reasoning, fastest and cheapest"),
    ("medium", "balanced"),
    ("high", "most reasoning, slowest and dearest"),
    ("off", "send no reasoning setting at all"),
];

/// ponytail: the menu is capped rather than scrolled. It holds every command
/// there is; give it a window over `menu()` if the table outgrows the cap.
const MENU_ROWS: usize = 20;

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
            paint(colour, sgr_accent(), name),
            " ".repeat(label - name.chars().count() + 2),
            paint(colour, sgr_dim(), description),
        ));
    }
    for line in [
        "Type / to open this menu; Up/Down: input history, or the menu while one is open",
        "Enter: take the highlighted command, or send a line that is already one",
        "Esc/Ctrl-C: cancel turn · Ctrl-D: exit on empty input",
        "Shift+Tab: step the approval mode (default, acceptEdits, plan, auto)",
        // Hooks the engine loaded do run, and `/hooks` marks which; an MCP
        // declaration is still only a reading of a file.
        "MCP connections are not loaded by ARSY; imported declarations grant no authority. `/hooks` marks the hooks that run.",
    ] {
        text.push_str(&paint(colour, sgr_dim(), line));
        text.push('\n');
    }
    text
}

/// The input line ARSY owns: its text, its caret, and how many rows it last
#[derive(Default)]
pub struct Composer {
    buffer: String,
    caret: usize,
    drawn: bool,
    top_status: bool,
    history: std::collections::VecDeque<String>,
    history_index: Option<usize>,
    draft: String,
    /// Which menu row Up/Down has landed on, clamped to the matches on use.
    selected: usize,
    /// Rows a picker put in front of the reader, offered instead of the command
    /// table for as long as it is collecting an answer.
    offered: Option<Vec<(String, String)>>,
    /// Set while the line is a secret being typed: it is painted as bullets,
    /// never kept in history, and never offered a menu.
    masked: bool,
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

    /// Collect the line as a secret. Nothing about it reaches the screen, the
    /// scrollback, or the history a later Up would walk back into.
    pub fn set_masked(&mut self, masked: bool) {
        self.masked = masked;
    }

    /// Put a picker's rows in the menu, marked at `selected`.
    ///
    /// Called once per line from the prompt state, so a selection never
    /// outlives the answer it was made for.
    pub fn offer(&mut self, rows: Option<Vec<(String, String)>>, selected: usize) {
        self.offered = rows;
        self.selected = selected;
    }

    /// The same, for a picker whose rows are a fixed table.
    pub fn offer_table(&mut self, rows: Option<&[(&str, &str)]>, selected: usize) {
        self.offer(
            rows.map(|rows| {
                rows.iter()
                    .map(|(name, description)| ((*name).to_owned(), (*description).to_owned()))
                    .collect()
            }),
            selected,
        );
    }

    /// Measured with the width, and on the same schedule.
    pub fn set_height(&mut self, rows: usize) {
        self.height = rows;
    }

    fn all_matches(&self) -> Vec<(String, String)> {
        if self.masked {
            return Vec::new();
        }
        if let Some(rows) = &self.offered {
            return rows
                .iter()
                .filter(|(name, _)| name.starts_with(&self.buffer))
                .cloned()
                .collect();
        }
        if self.picking || !self.buffer.starts_with('/') || self.buffer.contains(' ') {
            return Vec::new();
        }
        COMMANDS
            .iter()
            .filter(|(name, _)| name.starts_with(&self.buffer))
            .map(|(name, description)| ((*name).to_owned(), (*description).to_owned()))
            .collect()
    }

    fn menu_window(&self) -> (Vec<(String, String)>, usize) {
        let capacity = self.menu_capacity();
        if capacity == 0 {
            return (Vec::new(), 0);
        }
        let matches = self.all_matches();
        let total = matches.len();
        if total == 0 {
            return (Vec::new(), 0);
        }
        let selected = self.selected.min(total - 1);
        if total <= capacity {
            return (matches, selected);
        }
        let start = if selected >= capacity {
            selected + 1 - capacity
        } else {
            0
        };
        let window = matches[start..(start + capacity).min(total)].to_vec();
        (window, selected - start)
    }

    pub fn menu(&self) -> Vec<(String, String)> {
        self.all_matches()
    }

    /// The row the picker is on right now, for a live preview of a choice
    /// before Enter takes it. `None` when no menu is open.
    pub fn highlighted(&self) -> Option<String> {
        let matches = self.all_matches();
        let idx = self.selected.min(matches.len().checked_sub(1)?);
        matches.get(idx).map(|(name, _)| name.clone())
    }

    /// How many menu rows the terminal can hold.
    fn menu_capacity(&self) -> usize {
        match self.height {
            0 => MENU_ROWS,
            rows => MENU_ROWS.min(rows.saturating_sub(4)),
        }
    }

    /// Move the mark over the open menu. Both ends wrap, so a short list is
    /// never a dead end in one direction, and the index is clamped to the
    /// current matches first: a selection left over from a wider list must not
    /// step outside a narrowed one.
    fn mark(&mut self, down: bool) -> Action {
        let matches = self.all_matches();
        if matches.is_empty() {
            return Action::None;
        }
        let last = matches.len().saturating_sub(1);
        let selected = self.selected.min(last);
        self.selected = if down {
            if selected >= last {
                0
            } else {
                selected + 1
            }
        } else {
            selected.checked_sub(1).unwrap_or(last)
        };
        Action::Redraw
    }

    /// Walk the submitted lines. Going back past the newest returns the draft
    /// that was stashed on the way in, so browsing history cannot lose a line
    /// that was being typed.
    fn recall(&mut self, back: bool) -> Action {
        self.history_index = if back {
            Some(match self.history_index {
                Some(index) => index.saturating_sub(1),
                None => {
                    self.draft = self.buffer.clone();
                    self.history.len() - 1
                }
            })
        } else {
            self.history_index
                .map(|index| index + 1)
                .filter(|index| *index < self.history.len())
        };
        self.buffer = self
            .history_index
            .map_or_else(|| self.draft.clone(), |index| self.history[index].clone());
        self.caret = self.buffer.chars().count();
        Action::Redraw
    }

    pub fn press(&mut self, key: Key) -> Action {
        match key {
            Key::Char(character) if !character.is_control() => {
                self.buffer.insert(self.byte_at(self.caret), character);
                self.caret += 1;
                self.selected = 0;
                Action::Redraw
            }
            Key::Newline => {
                self.buffer.insert(self.byte_at(self.caret), '\n');
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
            Key::Up if !self.menu().is_empty() => self.mark(false),
            Key::Down if !self.menu().is_empty() => self.mark(true),
            Key::Up if !self.history.is_empty() => self.recall(true),
            Key::Down if self.history_index.is_some() => self.recall(false),
            Key::Left if self.caret > 0 => {
                self.caret -= 1;
                Action::Redraw
            }
            Key::Right if self.caret < self.buffer.chars().count() => {
                self.caret += 1;
                Action::Redraw
            }
            Key::WordLeft => {
                self.word_left();
                Action::Redraw
            }
            Key::WordRight => {
                self.word_right();
                Action::Redraw
            }
            Key::WordBackspace => {
                self.word_backspace();
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
                if !self.masked && !line.trim().is_empty() && self.history.back() != Some(&line) {
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
            // Shift+Tab steps the approval mode. It is submitted as the command
            // an operator could have typed instead, so the mode is changed in
            // exactly one place and the shortcut cannot drift away from what
            // `/approval` does. The drafted line is deliberately left alone:
            // changing mode mid-sentence must not cost the sentence.
            Key::CycleMode if !self.picking && !self.masked => {
                Action::Submit(CYCLE_APPROVAL_MODE.to_owned())
            }
            Key::Interrupt | Key::Eof if self.buffer.is_empty() => Action::Quit,
            _ => Action::None,
        }
    }

    /// The row the mark is on, for a caller that needs to see the selection
    /// without pressing Enter to find out.
    pub fn marked(&self) -> Option<String> {
        let menu = self.menu();
        menu.get(self.selected.min(menu.len().checked_sub(1)?))
            .map(|(name, _)| name.clone())
    }

    /// The command Enter would fill in, or `None` when the line is already one
    /// and Enter should send it.
    fn completion(&self) -> Option<String> {
        let menu = self.menu();
        let selected = menu.get(self.selected.min(menu.len().checked_sub(1)?))?;
        (!menu.iter().any(|(name, _)| *name == self.buffer)).then(|| selected.0.clone())
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

    fn word_left(&mut self) {
        let chars: Vec<char> = self.buffer.chars().collect();
        let mut idx = self.caret.min(chars.len());
        while idx > 0 && !chars[idx - 1].is_alphanumeric() {
            idx -= 1;
        }
        while idx > 0 && chars[idx - 1].is_alphanumeric() {
            idx -= 1;
        }
        self.caret = idx;
    }

    fn word_right(&mut self) {
        let chars: Vec<char> = self.buffer.chars().collect();
        let len = chars.len();
        let mut idx = self.caret.min(len);
        while idx < len && chars[idx].is_alphanumeric() {
            idx += 1;
        }
        while idx < len && !chars[idx].is_alphanumeric() {
            idx += 1;
        }
        self.caret = idx;
    }

    fn word_backspace(&mut self) {
        let chars: Vec<char> = self.buffer.chars().collect();
        let old_caret = self.caret.min(chars.len());
        let mut idx = old_caret;
        while idx > 0 && !chars[idx - 1].is_alphanumeric() {
            idx -= 1;
        }
        while idx > 0 && chars[idx - 1].is_alphanumeric() {
            idx -= 1;
        }
        let start_byte = self.byte_at(idx);
        let end_byte = self.byte_at(old_caret);
        self.buffer.drain(start_byte..end_byte);
        self.caret = idx;
        self.selected = 0;
    }

    fn caret_line_col(&self) -> (usize, usize, usize) {
        let chars: Vec<char> = self.buffer.chars().collect();
        let total_chars = chars.len();
        let caret = self.caret.min(total_chars);
        let mut line_idx = 0;
        let mut col_offset = 0;
        for &ch in &chars[..caret] {
            if ch == '\n' {
                line_idx += 1;
                col_offset = 0;
            } else {
                col_offset += 1;
            }
        }
        let total_lines = self.buffer.split('\n').count().max(1);
        (line_idx, col_offset, total_lines)
    }

    /// Paint the block — pad, input, pad, menu, status — with the status row
    /// at the bottom, so model, effort, directory and branch anchor the prompt.
    pub fn render(&mut self, width: usize, colour: bool, status: &str) -> String {
        let width = width.max(MIN_WIDTH);
        let status = fit(status, width);
        let room = width.saturating_sub(3);
        let menu = self.menu_rows(width, colour);
        let (line_idx, col_offset, total_lines) = self.caret_line_col();
        let mut frame = String::new();
        if self.drawn {
            frame.push_str(RESET);
            let lines_above = line_idx + if self.top_status { 2 } else { 1 };
            frame.push_str(&format!("\x1b[{}A", lines_above));
            frame.push('\r');
            frame.push_str(CLEAR_BELOW);
        }
        self.drawn = true;
        self.top_status = false;
        let surface = if colour {
            format!("{}{CLEAR_EOL}", sgr_input_bg())
        } else {
            String::new()
        };
        // Top surface pad
        frame.push_str(&surface);
        frame.push('\n');
        // Input lines
        let mut active_caret_col = col_offset;
        if self.masked {
            let (text, caret) = self.window(room);
            active_caret_col = caret;
            frame.push_str(&format!(
                "{surface}{} {text}{}\n",
                if colour {
                    format!("{}›", sgr_input_bg())
                } else {
                    "›".to_owned()
                },
                if colour { CLEAR_EOL } else { "" },
            ));
        } else {
            for (idx, line) in self.buffer.split('\n').enumerate() {
                let prompt_char = if idx == 0 { "›" } else { "·" };
                let prompt_str = if colour {
                    format!("{}{prompt_char}", sgr_input_bg())
                } else {
                    prompt_char.to_owned()
                };
                let chars: Vec<char> = line.chars().collect();
                let is_active = idx == line_idx;
                let (fitted_line, _) = if is_active {
                    let (w_text, w_caret) = Self::window_line(&chars, col_offset, room);
                    active_caret_col = w_caret;
                    (w_text, w_caret)
                } else {
                    Self::window_line(&chars, 0, room)
                };
                frame.push_str(&format!(
                    "{surface}{prompt_str} {fitted_line}{}\n",
                    if colour { CLEAR_EOL } else { "" },
                ));
            }
        }
        // Bottom surface pad
        frame.push_str(&format!("{surface}{}\n", if colour { RESET } else { "" }));
        for row in &menu {
            frame.push_str(row);
            frame.push('\n');
        }
        frame.push_str(&status);
        // Back onto the active input row, over the bottom pad, the menu, and the status row
        let lines_below = (total_lines.saturating_sub(1 + line_idx)) + 1 + menu.len() + 1;
        frame.push_str(&format!(
            "\x1b[{}A\r\x1b[{}C",
            lines_below,
            active_caret_col + 2
        ));
        frame
    }

    /// Paint the block with the live status (e.g. spinner and elapsed seconds)
    /// at the top, directly under the streaming output and above the input box,
    /// and the footer (model, effort, directory, branch) at the bottom.
    pub fn render_turn(
        &mut self,
        width: usize,
        colour: bool,
        status: &str,
        footer: &str,
    ) -> String {
        let width = width.max(MIN_WIDTH);
        let status = fit(status, width);
        let footer = fit(footer, width);
        let room = width.saturating_sub(3);
        let menu = self.menu_rows(width, colour);
        let (line_idx, col_offset, total_lines) = self.caret_line_col();
        let mut frame = String::new();
        if self.drawn {
            frame.push_str(RESET);
            let lines_above = line_idx + if self.top_status { 2 } else { 1 };
            frame.push_str(&format!("\x1b[{}A", lines_above));
            frame.push('\r');
            frame.push_str(CLEAR_BELOW);
        }
        self.drawn = true;
        self.top_status = true;
        let surface = if colour {
            format!("{}{CLEAR_EOL}", sgr_input_bg())
        } else {
            String::new()
        };
        frame.push_str(&status);
        frame.push('\n');
        // Top surface pad
        frame.push_str(&surface);
        frame.push('\n');
        // Input lines
        let mut active_caret_col = col_offset;
        if self.masked {
            let (text, caret) = self.window(room);
            active_caret_col = caret;
            frame.push_str(&format!(
                "{surface}{} {text}{}\n",
                if colour {
                    format!("{}›", sgr_input_bg())
                } else {
                    "›".to_owned()
                },
                if colour { CLEAR_EOL } else { "" },
            ));
        } else {
            for (idx, line) in self.buffer.split('\n').enumerate() {
                let prompt_char = if idx == 0 { "›" } else { "·" };
                let prompt_str = if colour {
                    format!("{}{prompt_char}", sgr_input_bg())
                } else {
                    prompt_char.to_owned()
                };
                let chars: Vec<char> = line.chars().collect();
                let is_active = idx == line_idx;
                let (fitted_line, _) = if is_active {
                    let (w_text, w_caret) = Self::window_line(&chars, col_offset, room);
                    active_caret_col = w_caret;
                    (w_text, w_caret)
                } else {
                    Self::window_line(&chars, 0, room)
                };
                frame.push_str(&format!(
                    "{surface}{prompt_str} {fitted_line}{}\n",
                    if colour { CLEAR_EOL } else { "" },
                ));
            }
        }
        // Bottom surface pad
        frame.push_str(&format!("{surface}{}\n", if colour { RESET } else { "" }));
        for row in &menu {
            frame.push_str(row);
            frame.push('\n');
        }
        frame.push_str(&footer);
        // Back onto the active input row, over the bottom pad, the menu, and the footer
        let lines_below = (total_lines.saturating_sub(1 + line_idx)) + 1 + menu.len() + 1;
        frame.push_str(&format!(
            "\x1b[{}A\r\x1b[{}C",
            lines_below,
            active_caret_col + 2
        ));
        frame
    }

    fn window_line(chars: &[char], caret_in_line: usize, room: usize) -> (String, usize) {
        let budget = room.saturating_sub(1);
        let width = |character: &char| character.width().unwrap_or(0);
        let mut start = caret_in_line.min(chars.len());
        let mut caret = 0;
        while start > 0 && caret + width(&chars[start - 1]) <= budget {
            start -= 1;
            caret += width(&chars[start]);
        }
        let mut used = 0;
        let text = chars[start..]
            .iter()
            .take_while(|character| {
                used += width(character);
                used <= budget
            })
            .collect();
        (text, caret)
    }

    /// One row per offered command, marked at the selection.
    fn menu_rows(&self, width: usize, colour: bool) -> Vec<String> {
        let (window, visible_selected) = self.menu_window();
        let Some(last) = window.len().checked_sub(1) else {
            return Vec::new();
        };
        let selected = visible_selected.min(last);
        let label = window
            .iter()
            .map(|(name, _)| name.chars().count())
            .max()
            .unwrap_or(0);
        window
            .iter()
            .enumerate()
            .map(|(index, (name, description))| {
                let chosen = index == selected;
                let row = format!(
                    "  {} {}{}{}",
                    paint(colour, sgr_accent(), if chosen { "›" } else { " " }),
                    paint(
                        colour,
                        if chosen { sgr_accent() } else { sgr_bullet() },
                        name
                    ),
                    " ".repeat(label.saturating_sub(name.chars().count()) + 2),
                    paint(colour, sgr_dim(), description),
                );
                fit(&row, width)
            })
            .collect()
    }

    /// Erase the block so turn output starts on a clean row, and keep the
    /// submitted line in the scrollback the way a shell would.
    pub fn commit(&mut self, submitted: &str, colour: bool) -> String {
        let mut out = self.clear();
        for (i, line) in submitted.lines().enumerate() {
            let prompt = if i == 0 { "›" } else { "·" };
            out.push_str(&format!(
                "{} {}\n",
                paint(colour, BOLD, prompt),
                paint(colour, sgr_assistant(), line),
            ));
        }
        if submitted.trim().is_empty() {
            out.push('\n');
        }
        out
    }

    pub fn clear(&mut self) -> String {
        if !std::mem::take(&mut self.drawn) {
            return String::new();
        }
        let (line_idx, _, _) = self.caret_line_col();
        let lines_above = line_idx + if self.top_status { 2 } else { 1 };
        format!("{RESET}\x1b[{}A\r{CLEAR_BELOW}", lines_above)
    }

    /// Slide the visible text so the caret stays on the row instead of
    /// wrapping, which would break the block's row count.
    fn window(&self, room: usize) -> (String, usize) {
        // A masked line is one bullet per character, so what is painted is the
        // same width as what was typed and the caret still lands where the
        // reader expects it.
        let characters: Vec<char> = if self.masked {
            std::iter::repeat_n('•', self.buffer.chars().count()).collect()
        } else {
            self.buffer.chars().collect()
        };
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
    approval_mode: String,
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
            approval_mode: "default".to_owned(),
        }
    }

    pub fn session_id(&self) -> SessionId {
        self.session
    }

    pub fn set_session_id(&mut self, session: SessionId) {
        self.session = session;
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

    pub fn set_approval_mode(&mut self, mode: impl Into<String>) {
        self.approval_mode = mode.into();
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
                paint(colour, sgr_dim(), ">_"),
                paint(colour, BOLD, "ARSY CODE"),
                paint(
                    colour,
                    sgr_dim(),
                    &format!(" (v{})", env!("CARGO_PKG_VERSION"))
                ),
            ),
            String::new(),
        ];
        if let Some(route) = &self.model_route {
            rows.push(format!(
                "{}   {}",
                label_row(colour, "model:", &route.to_string(), sgr_model()),
                paint(colour, sgr_dim(), "/model to change"),
            ));
        }
        rows.push(label_row(colour, "directory:", &self.workspace, sgr_cwd()));
        rows.push(label_row(
            colour,
            "sandbox:",
            &format!("{} · read-only", self.sandbox_assurance),
            sgr_dim(),
        ));
        rows.push(label_row(
            colour,
            "session:",
            &self.session.to_string(),
            sgr_dim(),
        ));
        if self.approval_mode == "plan" {
            rows.push(label_row(colour, "mode:", "PLAN", sgr_accent()));
        }
        if let Some(entry) = self.timeline.last() {
            rows.push(label_row(
                colour,
                "event:",
                &format!("{} {}", entry.sequence, entry.name),
                sgr_dim(),
            ));
        }
        if let Some(text) = &self.streaming {
            rows.push(paint(colour, sgr_assistant(), text));
        }

        let rows = beside_logo(rows, inner, colour);
        let rule = "─".repeat(width.saturating_sub(2));
        let mut lines = vec![paint(colour, sgr_border(), &format!("╭{rule}╮"))];
        for row in &rows {
            let row = fit(row, inner);
            let pad = " ".repeat(inner.saturating_sub(visible_len(&row)));
            lines.push(format!(
                "{} {row}{pad} {}",
                paint(colour, sgr_border(), "│"),
                paint(colour, sgr_border(), "│"),
            ));
        }
        lines.push(paint(colour, sgr_border(), &format!("╰{rule}╯")));
        lines.join("\n")
    }

    /// The status row shown under the composer: warm model, green directory,
    /// branch at the right edge.
    ///
    /// `branch` is passed rather than kept, because it belongs to the checkout
    /// and can change while the session is open.
    ///
    /// A narrow terminal gives up the fields in the order they can be spared:
    /// the workspace path shrinks to its last segments, then disappears, and
    /// only then is the branch dropped. The branch is never shortened, because
    /// half a branch name reads as a different branch — and it is the field a
    /// reader is least able to reconstruct from anything else on screen.
    pub fn status_row(&self, width: usize, colour: bool, branch: Option<&str>) -> String {
        const INDENT: usize = 2;
        const GAP: usize = 2;
        /// Below this a path has lost the segments that identify it.
        const PATH_FLOOR: usize = 6;

        let width = width.max(MIN_WIDTH);
        let route = self
            .model_route
            .as_ref()
            .map_or_else(|| "no model".to_owned(), ModelRoute::to_string);
        let effort_label = match self.effort {
            None => "○ off".to_owned(),
            Some(Effort::Low) => "◔ low".to_owned(),
            Some(Effort::Medium) => "◑ medium".to_owned(),
            Some(Effort::High) => "● high".to_owned(),
        };
        // `default` is the mode the row means when it says nothing, so naming it
        // would cost a field to tell the reader what they already assume. Every
        // other mode is a standing decision about what runs without asking, and
        // Shift+Tab can change it between two glances at the screen.
        let mode_label = match self.approval_mode.as_str() {
            "default" => None,
            "plan" => Some("⏸ PLAN".to_owned()),
            mode => Some(format!("⚙ {mode}")),
        };
        let mode_label = mode_label.as_deref();
        let branch = branch.unwrap_or_default();

        let model_label = format!("✦ {route}");
        let head = INDENT
            + visible_len(&model_label)
            + GAP
            + visible_len(&effort_label)
            + mode_label.map_or(0, |label| GAP + visible_len(label));
        let branch_label = if branch.is_empty() {
            String::new()
        } else {
            format!("⎇ {branch}")
        };
        let right = if branch_label.is_empty() {
            0
        } else {
            GAP + visible_len(&branch_label)
        };

        // Whatever is left over once the fields that cannot shrink are placed.
        let budget = width.saturating_sub(head + GAP + right);
        let ws_icon_len = visible_len("📁 ");
        let path_budget = budget.saturating_sub(ws_icon_len);
        let workspace = (path_budget >= PATH_FLOOR)
            .then(|| format!("📁 {}", shrink_path(&self.workspace, path_budget)));

        let mut row = format!(
            "{}{}{}{}",
            " ".repeat(INDENT),
            paint(colour, sgr_model(), &model_label),
            " ".repeat(GAP),
            paint(colour, sgr_dim(), &effort_label),
        );
        let mut used = head;
        if let Some(label) = mode_label {
            row.push_str(&" ".repeat(GAP));
            row.push_str(&paint(colour, sgr_accent(), label));
        }
        if let Some(workspace) = &workspace {
            row.push_str(&" ".repeat(GAP));
            row.push_str(&paint(colour, sgr_cwd(), workspace));
            used += GAP + visible_len(workspace);
        }
        // Only now is there a final answer on whether the branch fits.
        if !branch_label.is_empty() {
            if let Some(gap) = width.checked_sub(used + visible_len(&branch_label)) {
                if gap >= GAP {
                    row.push_str(&" ".repeat(gap));
                    row.push_str(&paint(colour, sgr_accent(), &branch_label));
                    return row;
                }
            }
        }
        fit(&row, width)
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
        paint(colour, sgr_dim(), label),
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
    fn colour(self) -> &'static str {
        match self {
            Self::Ok => sgr_ok(),
            Self::Error => sgr_err(),
            Self::Run => sgr_run(),
        }
    }
}

fn exec_row(colour: bool, status: Status, command: &str, result: Option<&str>) -> String {
    let head = format!(
        "  {} {}",
        paint(colour, status.colour(), "•"),
        paint(colour, sgr_accent(), command),
    );
    match result {
        Some(result) => format!("{head}  {}", paint(colour, sgr_dim(), result)),
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
        paint(colour, sgr_bullet(), "•"),
        paint(colour, BOLD, "Working…"),
    )
}

/// The top border of a thinking section box.
pub fn thinking_box_top(width: usize, colour: bool) -> String {
    let width = width.max(MIN_WIDTH);
    let title = " ✻ Thinking ";
    let title_len = visible_len(title);
    let prefix = "╭──";
    let prefix_len = 3;
    let rule_len = width.saturating_sub(prefix_len + title_len + 1);
    format!(
        "{}{}{}",
        paint(colour, sgr_border(), prefix),
        paint(colour, sgr_accent(), title),
        paint(colour, sgr_border(), &format!("{}╮", "─".repeat(rule_len))),
    )
}

/// One line of model reasoning inside a bordered thinking box.
pub fn thinking_box_row(width: usize, colour: bool, text: &str) -> String {
    let width = width.max(MIN_WIDTH);
    let inner = width.saturating_sub(4);
    let fitted = fit(text.trim_end(), inner);
    let pad = " ".repeat(inner.saturating_sub(visible_len(&fitted)));
    format!(
        "{} {}{} {}",
        paint(colour, sgr_border(), "│"),
        paint(colour, sgr_dim(), &fitted),
        pad,
        paint(colour, sgr_border(), "│"),
    )
}

/// The bottom border of a thinking section box.
pub fn thinking_box_bottom(width: usize, colour: bool) -> String {
    let width = width.max(MIN_WIDTH);
    let rule = "─".repeat(width.saturating_sub(2));
    paint(colour, sgr_border(), &format!("╰{rule}╯"))
}

/// A complete boxed thinking section.
pub fn thinking_box(width: usize, colour: bool, body: &str) -> String {
    let mut rows = vec![thinking_box_top(width, colour)];
    for line in body.lines() {
        rows.push(thinking_box_row(width, colour, line));
    }
    rows.push(thinking_box_bottom(width, colour));
    rows.join("\n")
}

/// The composer status line while a turn runs: a spinner, the phase the turn
/// is in, the seconds elapsed, and the cancel hint.
///
/// `phase` is `Connecting…` until the provider produced its first event, then
/// `Working…`; a connect that takes a minute is otherwise indistinguishable
/// from a hang. The spinner frames make the wait visibly alive, which is the
/// whole point: a static line reads as a dead terminal, not as a working one.
pub fn turn_status(
    colour: bool,
    phase: TurnPhase,
    elapsed: std::time::Duration,
    tick: usize,
    queued: usize,
) -> String {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let label = match phase {
        TurnPhase::Cancelling => "Cancelling…",
        TurnPhase::Connecting => "Connecting…",
        TurnPhase::Working => "Working…",
        TurnPhase::Answering => "Answering…",
    };
    let mut status = format!(
        "  {} {} · {}s",
        paint(colour, sgr_run(), FRAMES[tick % FRAMES.len()]),
        paint(colour, BOLD, label),
        elapsed.as_secs(),
    );
    if queued > 0 {
        status.push_str(&paint(colour, sgr_dim(), &format!(" · {queued} queued")));
    }
    status.push_str(&paint(colour, sgr_dim(), " · Esc cancel"));
    status
}

/// Which phase a running turn is in, for the composer status line.
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum TurnPhase {
    Connecting,
    Working,
    Answering,
    Cancelling,
}

/// One line of streamed model text, styled as the Codex projection styles an
/// assistant message, so both routes read the same in scrollback.
pub fn assistant_row(colour: bool, text: &str) -> String {
    paint(colour, sgr_assistant(), text.trim_end())
}

/// Header for the final assistant response, separating it from tool trace.
pub fn assistant_header(colour: bool) -> String {
    paint(colour, sgr_assistant(), "  ✦ Response")
}

/// Shown when a turn is stopped from the keyboard.
pub fn interrupted_row(colour: bool) -> String {
    exec_row(colour, Status::Run, "Interrupted", None)
}

/// A tool the model wants to run, waiting on the operator's answer. The
/// command or the file list is shown, because that is what is being agreed to.
pub fn tool_prompt_row(colour: bool, name: &str, summary: &str) -> String {
    exec_row(
        colour,
        Status::Run,
        &format!("{name} {summary}"),
        Some("run it? y / n"),
    )
}

/// Shown while an approved tool is executing.
pub fn tool_running_row(colour: bool, name: &str, summary: &str) -> String {
    exec_row(
        colour,
        Status::Run,
        &format!("{name} {summary}"),
        Some("running…"),
    )
}

/// Render one animated tool execution frame for a long-running call.
pub fn tool_running_frame(
    colour: bool,
    frame: &str,
    name: &str,
    summary: &str,
    elapsed_ms: u128,
) -> String {
    let kind = tool_card_kind(name);
    let (accent_sgr, _) = tool_card_colors(kind, colour);
    format!(
        "  {} {} {} · {}ms",
        paint(colour, sgr_run(), frame),
        paint(colour, accent_sgr, name),
        paint(colour, sgr_dim(), summary),
        elapsed_ms
    )
}

/// Render a running tool card with a bounded tail of live stdout/stderr.
pub fn tool_running_frame_with_output(
    colour: bool,
    frame: &str,
    name: &str,
    summary: &str,
    elapsed_ms: u128,
    output: &str,
    expanded: bool,
) -> String {
    let kind = tool_card_kind(name);
    let (accent_sgr, _) = tool_card_colors(kind, colour);
    let detail = if expanded {
        let lines: Vec<&str> = output.lines().rev().take(8).collect();
        let tail = lines.into_iter().rev().collect::<Vec<_>>().join(" │ ");
        if tail.is_empty() {
            format!("{summary} │ expanded")
        } else {
            format!("{summary} │ {tail}")
        }
    } else {
        let tail = output.lines().last().unwrap_or_default();
        if tail.is_empty() {
            summary.to_owned()
        } else {
            format!("{summary} │ {tail}")
        }
    };
    format!(
        "  {} {} {} · {}ms · {}",
        paint(colour, sgr_run(), frame),
        paint(colour, accent_sgr, name),
        paint(
            colour,
            sgr_dim(),
            &fit(&detail, terminal_width().saturating_sub(24))
        ),
        elapsed_ms,
        if expanded { "e collapse" } else { "e expand" }
    )
}

/// Execution state passed to format the live running tool card.
pub struct RunningToolState<'a> {
    pub name: &'a str,
    pub summary: &'a str,
    pub frame: &'a str,
    pub elapsed_ms: u128,
    pub live_output: &'a str,
    pub expanded: bool,
}

/// Render an in-progress animated box for an actively executing tool call.
pub fn tool_running_box(width: usize, colour: bool, state: &RunningToolState<'_>) -> Vec<String> {
    let width = width.max(MIN_WIDTH);
    let inner = width.saturating_sub(4);
    let kind = tool_card_kind(state.name);
    let icon = tool_card_icon(kind);
    let (accent_sgr, border_sgr) = tool_card_colors(kind, colour);

    let clean_name = state
        .name
        .trim_start_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '.')
        .trim();
    let display_name = if clean_name.is_empty() {
        state.name
    } else {
        clean_name
    };

    let header = if matches!(kind, ToolCardKind::Bash) {
        let max_cmd_len = inner.saturating_sub(4);
        let fitted_cmd = fit(state.summary, max_cmd_len);
        format!(" $ {fitted_cmd} ")
    } else if state.summary.is_empty() {
        format!(" {icon} {display_name} ")
    } else {
        let max_sum_len = inner.saturating_sub(visible_len(display_name) + 5);
        let fitted_sum = fit(state.summary, max_sum_len);
        format!(" {icon} {display_name} {fitted_sum} ")
    };

    let header_len = visible_len(&header);
    let top_left = "─".repeat(2);
    let top_right = "─".repeat(width.saturating_sub(2 + 2 + header_len));

    let mut lines = vec![format!(
        "{}{}{}{}",
        paint(colour, border_sgr, "╭"),
        paint(colour, border_sgr, &top_left),
        paint(colour, accent_sgr, &header),
        paint(colour, border_sgr, &format!("{top_right}╮")),
    )];

    let status_lead = format!(" {} running ({}ms)", state.frame, state.elapsed_ms);
    if !state.expanded {
        let tail = state.live_output.lines().last().unwrap_or_default().trim();
        let status_row = if tail.is_empty() {
            status_lead
        } else {
            let max_tail = inner.saturating_sub(visible_len(&status_lead) + 3);
            let fitted_tail = fit(tail, max_tail);
            format!("{status_lead} · {fitted_tail}")
        };
        let fitted = fit(&status_row, inner);
        let pad = " ".repeat(inner.saturating_sub(visible_len(&fitted)));
        lines.push(format!(
            "{} {}{pad} {}",
            paint(colour, border_sgr, "│"),
            paint(colour, sgr_dim(), &fitted),
            paint(colour, border_sgr, "│"),
        ));
    } else {
        let status_fitted = fit(&status_lead, inner);
        let pad = " ".repeat(inner.saturating_sub(visible_len(&status_fitted)));
        lines.push(format!(
            "{} {}{pad} {}",
            paint(colour, border_sgr, "│"),
            paint(colour, sgr_run(), &status_fitted),
            paint(colour, border_sgr, "│"),
        ));

        let out_lines: Vec<&str> = state.live_output.lines().collect();
        let tail_count = 6;
        let start = out_lines.len().saturating_sub(tail_count);
        for line in out_lines.iter().skip(start) {
            let line_fmt = format!("   {line}");
            let fitted = fit(&line_fmt, inner);
            let pad = " ".repeat(inner.saturating_sub(visible_len(&fitted)));
            lines.push(format!(
                "{} {}{pad} {}",
                paint(colour, border_sgr, "│"),
                paint(colour, sgr_dim(), &fitted),
                paint(colour, border_sgr, "│"),
            ));
        }
    }

    let toggle_hint = if state.expanded {
        " [e: collapse] "
    } else {
        " [e: expand] "
    };
    let toggle_len = visible_len(toggle_hint);
    let bot_fill = width.saturating_sub(2 + toggle_len);
    let bot_bar = "─".repeat(bot_fill);
    lines.push(format!(
        "{}{}{}{}",
        paint(colour, border_sgr, "╰"),
        paint(colour, border_sgr, &bot_bar),
        paint(colour, sgr_dim(), toggle_hint),
        paint(colour, border_sgr, "╯"),
    ));

    lines
}

/// What a tool call did, once it ran or was declined.
pub fn tool_result_row(colour: bool, name: &str, ok: bool, detail: &str) -> String {
    exec_row(
        colour,
        if ok { Status::Ok } else { Status::Error },
        name,
        Some(detail),
    )
}

fn error_row(colour: bool, message: &str) -> String {
    format!(
        "  {} {}",
        paint(colour, sgr_err(), "•"),
        paint(colour, sgr_err(), &unwrap_api_error(message.trim())),
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
        "agent_message" => Some(paint(colour, sgr_assistant(), text("text").trim())),
        // Codex reports some failures as an item rather than a top-level event.
        "error" => Some(error_row(colour, text("message"))),
        "reasoning" => {
            let body = text("text").trim();
            if body.is_empty() {
                return None;
            }
            Some(thinking_box(terminal_width(), colour, body))
        }
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

/// A styled bash execution frame with command, output, and duration.
pub fn bash_box(
    width: usize,
    colour: bool,
    command: &str,
    output: &str,
    exit_code: Option<i32>,
    duration: std::time::Duration,
) -> String {
    let width = width.max(MIN_WIDTH);
    let inner = width.saturating_sub(4);
    let max_cmd_len = inner.saturating_sub(4);
    let fitted_command = fit(command, max_cmd_len);
    let header = format!(" $ {fitted_command} ");
    let header_len = visible_len(&header);
    let (accent_sgr, border_sgr) = tool_card_colors(ToolCardKind::Bash, colour);
    let top_left = "─".repeat(2);
    let top_right = "─".repeat(width.saturating_sub(2 + 2 + header_len));
    let mut lines = vec![format!(
        "{}{}{}{}",
        paint(colour, border_sgr, "╭"),
        paint(colour, border_sgr, &top_left),
        paint(colour, accent_sgr, &header),
        paint(colour, border_sgr, &format!("{top_right}╮")),
    )];

    let out_lines: Vec<&str> = output.lines().collect();
    let max_preview = 10;
    if out_lines.len() <= max_preview {
        for line in &out_lines {
            let fitted = fit(line, inner);
            let pad = " ".repeat(inner.saturating_sub(visible_len(&fitted)));
            lines.push(format!(
                "{} {}{pad} {}",
                paint(colour, border_sgr, "│"),
                paint(colour, sgr_dim(), &fitted),
                paint(colour, border_sgr, "│"),
            ));
        }
    } else {
        let omitted = out_lines.len() - max_preview;
        let more = format!("… ({} earlier lines omitted)", omitted);
        let pad = " ".repeat(inner.saturating_sub(visible_len(&more)));
        lines.push(format!(
            "{} {}{pad} {}",
            paint(colour, border_sgr, "│"),
            paint(colour, sgr_dim(), &more),
            paint(colour, border_sgr, "│"),
        ));
        for line in out_lines.iter().skip(omitted) {
            let fitted = fit(line, inner);
            let pad = " ".repeat(inner.saturating_sub(visible_len(&fitted)));
            lines.push(format!(
                "{} {}{pad} {}",
                paint(colour, border_sgr, "│"),
                paint(colour, sgr_dim(), &fitted),
                paint(colour, border_sgr, "│"),
            ));
        }
    }

    let total_lines = out_lines.len();
    let status_lead = match exit_code {
        Some(0) => format!(" ✓ done ({}ms)", duration.as_millis()),
        Some(code) => format!(" ✗ exit {code} ({}ms)", duration.as_millis()),
        None => format!(" ⚙ running ({}ms)", duration.as_millis()),
    };
    let status_text = if total_lines > max_preview {
        format!(" {status_lead} · {total_lines} lines ")
    } else {
        format!(" {status_lead} ")
    };
    let status_sgr = match exit_code {
        Some(0) => sgr_ok(),
        Some(_) => sgr_err(),
        None => sgr_run(),
    };
    let max_status_len = inner.saturating_sub(2);
    let fitted_status = fit(&status_text, max_status_len);
    let bot_len = visible_len(&fitted_status);
    let bot_left = "─".repeat(2);
    let bot_right = "─".repeat(width.saturating_sub(2 + 2 + bot_len));
    lines.push(format!(
        "{}{}{}{}",
        paint(colour, border_sgr, "╰"),
        paint(colour, border_sgr, &bot_left),
        paint(colour, status_sgr, &fitted_status),
        paint(colour, border_sgr, &format!("{bot_right}╯")),
    ));
    lines.join("\n")
}

/// A styled tool execution box for filesystem, search, or MCP operations.
pub fn tool_box(
    width: usize,
    colour: bool,
    name: &str,
    summary: &str,
    output: &str,
    success: bool,
    duration: std::time::Duration,
) -> String {
    let width = width.max(MIN_WIDTH);
    let inner = width.saturating_sub(4);
    let kind = tool_card_kind(name);
    let icon = tool_card_icon(kind);
    let clean_name = name
        .trim_start_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '.')
        .trim();
    let display_name = if clean_name.is_empty() {
        name
    } else {
        clean_name
    };
    let prefix = format!(" {icon} {display_name} ");
    let prefix_len = visible_len(&prefix);
    let max_summary_len = inner.saturating_sub(prefix_len + 1);
    let fitted_summary = fit(summary, max_summary_len);
    let header = if summary.is_empty() {
        prefix
    } else {
        format!("{prefix}{fitted_summary} ")
    };
    let header_len = visible_len(&header);
    let (accent_sgr, border_sgr) = tool_card_colors(kind, colour);
    let top_left = "─".repeat(2);
    let top_right = "─".repeat(width.saturating_sub(2 + 2 + header_len));
    let mut lines = vec![format!(
        "{}{}{}{}",
        paint(colour, border_sgr, "╭"),
        paint(colour, border_sgr, &top_left),
        paint(colour, accent_sgr, &header),
        paint(colour, border_sgr, &format!("{top_right}╮")),
    )];

    let out_lines: Vec<&str> = output.lines().collect();
    let max_preview = 10;
    if out_lines.len() <= max_preview {
        for line in &out_lines {
            let fitted = fit(line, inner);
            let pad = " ".repeat(inner.saturating_sub(visible_len(&fitted)));
            lines.push(format!(
                "{} {}{pad} {}",
                paint(colour, border_sgr, "│"),
                paint(colour, sgr_dim(), &fitted),
                paint(colour, border_sgr, "│"),
            ));
        }
    } else {
        let omitted = out_lines.len() - max_preview;
        let more = format!("… ({} earlier lines omitted)", omitted);
        let pad = " ".repeat(inner.saturating_sub(visible_len(&more)));
        lines.push(format!(
            "{} {}{pad} {}",
            paint(colour, border_sgr, "│"),
            paint(colour, sgr_dim(), &more),
            paint(colour, border_sgr, "│"),
        ));
        for line in out_lines.iter().skip(omitted) {
            let fitted = fit(line, inner);
            let pad = " ".repeat(inner.saturating_sub(visible_len(&fitted)));
            lines.push(format!(
                "{} {}{pad} {}",
                paint(colour, border_sgr, "│"),
                paint(colour, sgr_dim(), &fitted),
                paint(colour, border_sgr, "│"),
            ));
        }
    }

    let total_lines = out_lines.len();
    let status_lead = if success {
        format!(" ✓ completed ({}ms)", duration.as_millis())
    } else {
        format!(" ✗ failed ({}ms)", duration.as_millis())
    };
    let status_text = if total_lines > max_preview {
        format!(" {status_lead} · {total_lines} lines ")
    } else {
        format!(" {status_lead} ")
    };
    let status_sgr = if success { sgr_ok() } else { sgr_err() };
    let max_status_len = inner.saturating_sub(2);
    let fitted_status = fit(&status_text, max_status_len);
    let bot_len = visible_len(&fitted_status);
    let bot_left = "─".repeat(2);
    let bot_right = "─".repeat(width.saturating_sub(2 + 2 + bot_len));
    lines.push(format!(
        "{}{}{}{}",
        paint(colour, border_sgr, "╰"),
        paint(colour, border_sgr, &bot_left),
        paint(colour, status_sgr, &fitted_status),
        paint(colour, border_sgr, &format!("{bot_right}╯")),
    ));
    lines.join("\n")
}

/// Tool categories used to keep verbose cards visually consistent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolCardKind {
    Bash,
    File,
    Network,
    Mcp,
    Search,
    Generic,
}

pub fn tool_card_kind(name: &str) -> ToolCardKind {
    let clean = name
        .trim_start_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '.')
        .trim();
    if matches!(clean, "bash" | "shell.execute") {
        ToolCardKind::Bash
    } else if clean.starts_with("mcp.") || clean == "mcp" || clean.starts_with("mcp_") {
        ToolCardKind::Mcp
    } else if matches!(
        clean,
        "fs.read"
            | "fs.list"
            | "fs.write"
            | "fs.edit"
            | "fs.delete"
            | "fs.move"
            | "edit"
            | "apply_patch"
            | "view_file"
            | "write_to_file"
            | "replace_file_content"
            | "list_dir"
    ) || clean.starts_with("fs.")
        || clean.ends_with("_file")
        || clean.ends_with("_dir")
    {
        ToolCardKind::File
    } else if clean.starts_with("search.")
        || clean.contains("search")
        || clean.contains("grep")
        || clean.contains("find")
    {
        ToolCardKind::Search
    } else if clean == "network.connect"
        || clean == "curl"
        || clean == "http"
        || clean.contains("url")
        || clean.contains("fetch")
    {
        ToolCardKind::Network
    } else {
        ToolCardKind::Generic
    }
}

pub fn tool_card_icon(kind: ToolCardKind) -> &'static str {
    match kind {
        ToolCardKind::Bash => "$",
        ToolCardKind::File => "✎",
        ToolCardKind::Network => "⇄",
        ToolCardKind::Mcp => "⌘",
        ToolCardKind::Search => "⌕",
        ToolCardKind::Generic => "⚙",
    }
}

/// Category-specific (accent, border) color pair for tool cards.
pub fn tool_card_colors(kind: ToolCardKind, colour: bool) -> (&'static str, &'static str) {
    if !colour {
        return ("", "");
    }
    if palette().border == "\x1b[38;2;74;74;74m" && palette().accent == "\x1b[38;2;205;205;205m" {
        return (sgr_accent(), sgr_border());
    }
    match kind {
        ToolCardKind::Bash => ("\x1b[38;2;97;175;239m", "\x1b[38;2;60;125;190m"),
        ToolCardKind::File => ("\x1b[38;2;229;192;123m", "\x1b[38;2;176;136;59m"),
        ToolCardKind::Search => ("\x1b[38;2;198;120;221m", "\x1b[38;2;142;78;163m"),
        ToolCardKind::Mcp => ("\x1b[38;2;86;182;194m", "\x1b[38;2;53;127;137m"),
        ToolCardKind::Network => ("\x1b[38;2;152;195;121m", "\x1b[38;2;93;142;67m"),
        ToolCardKind::Generic => ("\x1b[38;2;224;108;117m", "\x1b[38;2;157;72;80m"),
    }
}

/// Render one completed verbose card with a typed header and bounded body.
pub fn tool_card(
    width: usize,
    colour: bool,
    name: &str,
    summary: &str,
    output: &str,
    success: bool,
    duration: std::time::Duration,
) -> String {
    let kind = tool_card_kind(name);
    match kind {
        ToolCardKind::Bash => bash_box(
            width,
            colour,
            summary,
            output,
            Some(i32::from(!success)),
            duration,
        ),
        _ => tool_box(width, colour, name, summary, output, success, duration),
    }
}

/// A diff row showing modified file paths and change stats.
pub fn diff_row(colour: bool, path: &str, added: usize, deleted: usize) -> String {
    format!(
        "  {} {} {} {}",
        paint(colour, sgr_bullet(), "•"),
        paint(colour, sgr_accent(), path),
        paint(colour, sgr_ok(), &format!("+{added}")),
        paint(colour, sgr_err(), &format!("-{deleted}")),
    )
}

/// An option in the interactive Ask/Approval dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskOption {
    pub label: String,
    pub description: Option<String>,
}

/// Result of an interactive Ask/Approval dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AskDialogResult {
    Approve { note: Option<String> },
    AlwaysApprove { note: Option<String> },
    Deny { note: Option<String> },
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
            editing_note: false,
            plan_decision: false,
        }
    }

    pub fn for_plan() -> Self {
        Self {
            title: "PLAN READY".to_owned(),
            summary: "Review the repository-aware plan before any implementation begins."
                .to_owned(),
            reason: "Plan Mode blocks workspace mutations until approval.".to_owned(),
            diff_preview: None,
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
            editing_note: false,
            plan_decision: true,
        }
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
                "Proposed Changes:",
                inner,
                colour,
                sgr_accent(),
            ));
            for line in diff.lines().take(15) {
                lines.push(Self::render_diff_line(line, inner, colour));
            }
            if diff.lines().count() > 15 {
                let more = format!("… ({} more lines omitted)", diff.lines().count() - 15);
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
            "[↑/↓] Navigate  [1-3] Choose  [e] Add Note  [i] Implement  [r] Revise  [c] Cancel"
        } else if self.plan_decision {
            "[↑/↓] Navigate  [1-3] Choose  [e] Edit Note  [i] Implement  [r] Revise  [c] Cancel"
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

/// A model one provider offers. The provider rides on the row, so the picker
/// lists every configured provider's models and a single answer can move the
/// route to another provider as well as to another model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelChoice {
    pub provider: String,
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
                    provider: CODEX_PROVIDER.to_owned(),
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

/// Offer every configured provider's models, grouped under their provider, by
/// number; `current` is marked where it appears.
///
/// Accepts a list index, a slug typed in full, or an empty line to keep
/// `current`.
pub fn render_model_list(
    writer: &mut impl Write,
    models: &[ModelChoice],
    current: &ModelRoute,
    colour: bool,
) -> std::io::Result<()> {
    // The current row wins the mark even if two providers list one slug.
    let selected = models
        .iter()
        .position(|choice| choice.provider == current.provider && choice.slug == current.model);
    let mut last_provider: Option<&str> = None;
    for (index, choice) in models.iter().enumerate() {
        if last_provider != Some(choice.provider.as_str()) {
            writeln!(
                writer,
                "{}",
                paint(colour, sgr_accent(), &format!("  [{}]", choice.provider))
            )?;
            last_provider = Some(&choice.provider);
        }
        let marker = if Some(index) == selected { "›" } else { " " };
        writeln!(
            writer,
            "    {} {} {}  {}",
            paint(colour, sgr_accent(), marker),
            paint(colour, sgr_dim(), &format!("{}.", index + 1)),
            paint(colour, sgr_model(), &choice.slug),
            paint(colour, sgr_dim(), &choice.name),
        )?;
    }
    Ok(())
}

/// The rows the effort picker offers, in the order it numbers them.
pub fn effort_choices() -> Vec<Option<Effort>> {
    let mut choices: Vec<Option<Effort>> = Effort::ALL.into_iter().map(Some).collect();
    choices.push(None);
    choices
}

/// Which offered row the mark starts on, so the picker opens on what is set.
pub fn effort_row(current: Option<Effort>) -> usize {
    effort_choices()
        .iter()
        .position(|choice| *choice == current)
        .unwrap_or(0)
}

pub fn effort_prompt(current: Option<Effort>, colour: bool) -> String {
    let current = current.map_or_else(|| "off".to_owned(), |effort| effort.to_string());
    paint(
        colour,
        sgr_dim(),
        &format!(
            "  effort [{current}] · Up/Down then Enter, a name, or 1-{}",
            effort_choices().len()
        ),
    )
}

/// Take an answer to the effort picker: a list number, a level name, `off`, or
/// an empty line to keep what is set.
///
/// Rejected answers report why, for the same reason the model picker does: an
/// accepted answer is written to the user configuration.
pub fn resolve_effort_answer(
    line: &str,
    current: Option<Effort>,
) -> Result<Option<Effort>, String> {
    let answer = line.trim();
    if answer.is_empty() {
        return Ok(current);
    }
    if let Ok(number) = answer.parse::<usize>() {
        return effort_choices()
            .get(
                number
                    .checked_sub(1)
                    .ok_or_else(|| format!("`{answer}` is out of range; the list starts at 1"))?,
            )
            .copied()
            .ok_or_else(|| format!("`{answer}` is not on the list"));
    }
    match answer {
        "off" | "none" | "unset" => Ok(None),
        _ => Effort::parse(answer).map(Some).ok_or_else(|| {
            format!(
                "`{}` is not an effort level; use {}, or off",
                safe_text(answer),
                Effort::ALL.map(Effort::as_str).join(", "),
            )
        }),
    }
}

/// The rows the model picker offers, in the order it numbers them.
pub fn model_rows(
    models: &[ModelChoice],
    current: &ModelRoute,
) -> (Option<Vec<(String, String)>>, usize) {
    if models.is_empty() {
        return (None, 0);
    }
    let selected = models
        .iter()
        .position(|choice| choice.provider == current.provider && choice.slug == current.model)
        .unwrap_or(0);
    let rows = models
        .iter()
        .map(|choice| {
            let label = format!("[{}] {}", choice.provider, choice.slug);
            let desc = if choice.name.is_empty() || choice.name == choice.slug {
                format!("on {}", choice.provider)
            } else if choice.provider == CODEX_PROVIDER {
                format!("{} · codex", choice.name)
            } else {
                format!("{} · on {}", choice.name, choice.provider)
            };
            (label, desc)
        })
        .collect();
    (Some(rows), selected)
}

pub fn model_prompt(models: &[ModelChoice], current: &ModelRoute, colour: bool) -> String {
    let choices = if models.is_empty() {
        "a slug".to_owned()
    } else {
        format!("Up/Down then Enter, a name, or 1-{}", models.len())
    };
    paint(
        colour,
        sgr_dim(),
        &format!("  model [{}] · {choices}", current),
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
            // The row names the provider, so one answer can move the turn to
            // another provider and its model at once.
            Some(choice) => Ok(ModelRoute {
                provider: choice.provider.clone(),
                model: choice.slug.clone(),
            }),
            None if models.is_empty() => Err("no models are listed; type a model slug".to_owned()),
            None => Err(format!("no model {number}; choose 1-{}", models.len())),
        };
    }
    // `[provider] model` bracketed notation from the interactive picker.
    if let Some(rest) = answer.strip_prefix('[') {
        if let Some((provider, model_part)) = rest.split_once(']') {
            let model_slug = model_part.trim();
            if let Some(choice) = models
                .iter()
                .find(|c| c.provider == provider && c.slug == model_slug)
            {
                return Ok(ModelRoute {
                    provider: choice.provider.clone(),
                    model: choice.slug.clone(),
                });
            }
            validate_slug(model_slug)?;
            return Ok(ModelRoute {
                provider: provider.to_owned(),
                model: model_slug.to_owned(),
            });
        }
    }
    validate_slug(answer)?;
    // `provider/model` when answering with a qualified name.
    if let Some((provider, slug)) = answer.split_once('/') {
        if let Some(choice) = models
            .iter()
            .find(|c| c.provider == provider && c.slug == slug)
        {
            return Ok(ModelRoute {
                provider: choice.provider.clone(),
                model: choice.slug.clone(),
            });
        }
        validate_slug(slug)?;
        return Ok(ModelRoute {
            provider: provider.to_owned(),
            model: slug.to_owned(),
        });
    }
    // An exact match on a listed slug carries its provider.
    if let Some(choice) = models.iter().find(|c| c.slug == answer) {
        return Ok(ModelRoute {
            provider: choice.provider.clone(),
            model: choice.slug.clone(),
        });
    }
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
/// Fit a path into `budget` columns by dropping leading segments: the tail is
/// what tells one checkout from another.
fn shrink_path(path: &str, budget: usize) -> String {
    if visible_len(path) <= budget {
        return path.to_owned();
    }
    let mut kept = String::new();
    for segment in path.rsplit('/').filter(|segment| !segment.is_empty()) {
        let candidate = if kept.is_empty() {
            segment.to_owned()
        } else {
            format!("{segment}/{kept}")
        };
        // Two columns are owed to the `…/` that says something was dropped.
        if visible_len(&candidate) + 2 > budget {
            break;
        }
        kept = candidate;
    }
    if kept.is_empty() {
        // Not even the last segment fits, so keep its end.
        let tail: String = path.chars().rev().take(budget.saturating_sub(1)).collect();
        return format!("…{}", tail.chars().rev().collect::<String>());
    }
    format!("…/{kept}")
}

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
    fn theme_answers_and_overrides_resolve() {
        // Every built-in name in the picker table has a palette, and its
        // description is non-empty.
        for (name, description) in THEMES {
            assert!(builtin_palette(name).is_some(), "{name} has no palette");
            assert!(!description.is_empty());
        }
        assert!(builtin_palette("chartreuse").is_none());

        // A number, a name (any case), or an empty line to keep what is set.
        assert_eq!(resolve_theme_answer("2", "dark").unwrap(), "ocean");
        assert_eq!(resolve_theme_answer("OCEAN", "dark").unwrap(), "ocean");
        assert_eq!(resolve_theme_answer("   ", "mono").unwrap(), "mono");
        assert!(resolve_theme_answer("0", "dark").is_err());
        assert!(resolve_theme_answer("99", "dark").is_err());
        assert!(resolve_theme_answer("solarized", "dark").is_err());

        // #rrggbb (with or without the hash) becomes a truecolor prefix;
        // input_bg is a background one.
        assert_eq!(hex_to_sgr("#ff0000", false).unwrap(), "\x1b[38;2;255;0;0m");
        assert_eq!(hex_to_sgr("00ff80", true).unwrap(), "\x1b[48;2;0;255;128m");
        assert!(hex_to_sgr("#fff", false).is_err());
        assert!(hex_to_sgr("#gggggg", false).is_err());

        // Overrides replace only the named roles; an unknown role or a bad
        // colour is rejected, not ignored.
        let mut roles = std::collections::BTreeMap::new();
        roles.insert("accent".to_owned(), "#123456".to_owned());
        roles.insert("input_bg".to_owned(), "#abcdef".to_owned());
        let painted = builtin_palette("dark")
            .unwrap()
            .with_overrides(&roles)
            .unwrap();
        assert_eq!(painted.accent, "\x1b[38;2;18;52;86m");
        assert_eq!(painted.input_bg, "\x1b[48;2;171;205;239m");
        assert_eq!(
            painted.assistant,
            builtin_palette("dark").unwrap().assistant
        );

        let mut bad_role = std::collections::BTreeMap::new();
        bad_role.insert("accnt".to_owned(), "#123456".to_owned());
        assert!(builtin_palette("dark")
            .unwrap()
            .with_overrides(&bad_role)
            .is_err());

        let mut bad_hex = std::collections::BTreeMap::new();
        bad_hex.insert("accent".to_owned(), "red".to_owned());
        assert!(builtin_palette("dark")
            .unwrap()
            .with_overrides(&bad_hex)
            .is_err());
    }

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
            "  ✦ no model  ○ off  📁 /repo"
        );
        state.set_effort(Some(Effort::High));
        // The branch sits at the right edge, so it holds its column while the
        // fields on the left change length.
        let row = state.status_row(80, false, Some("feat/x"));
        assert!(row.starts_with("  ✦ no model  ● high  📁 /repo"), "{row:?}");
        assert!(row.ends_with("⎇ feat/x"), "{row:?}");
        assert_eq!(visible_len(&row), 80, "{row:?}");
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
                provider: CODEX_PROVIDER.into(),
                slug: "gpt-5.6-sol".into(),
                name: "GPT-5.6-Sol".into(),
            },
            ModelChoice {
                provider: CODEX_PROVIDER.into(),
                slug: "gpt-5.6-luna".into(),
                name: "GPT-5.6-Luna".into(),
            },
            ModelChoice {
                provider: "hari".into(),
                slug: "mimo".into(),
                name: "on hari".into(),
            },
        ];
        let current = ModelRoute {
            provider: CODEX_PROVIDER.into(),
            model: "gpt-5.6-luna".into(),
        };
        let pick = |answer: &str| resolve_model(answer, &models, &current);

        assert_eq!(
            pick("1").unwrap(),
            ModelRoute {
                provider: CODEX_PROVIDER.into(),
                model: "gpt-5.6-sol".into(),
            }
        );
        assert_eq!(pick("").unwrap(), current, "empty keeps the current model");
        assert_eq!(
            pick("  2  ").unwrap().model,
            "gpt-5.6-luna",
            "surrounding space is ignored"
        );
        // A number answers with the row's provider, so the picker moves the
        // turn between providers as well as between models.
        assert_eq!(
            pick("3").unwrap(),
            ModelRoute {
                provider: "hari".into(),
                model: "mimo".into(),
            }
        );
        // `[provider] model` bracketed notation from the interactive picker.
        assert_eq!(
            pick("[hari] mimo").unwrap(),
            ModelRoute {
                provider: "hari".into(),
                model: "mimo".into(),
            }
        );
        // A free-text slug stays on the current provider.
        assert_eq!(pick("o3-custom").unwrap().provider, CODEX_PROVIDER);
        assert_eq!(pick("o3-custom").unwrap().model, "o3-custom");
        // Qualified provider/model names switch provider.
        assert_eq!(
            pick("openai/gpt-5.6:high").unwrap(),
            ModelRoute {
                provider: "openai".into(),
                model: "gpt-5.6:high".into(),
            }
        );

        // A rejected answer keeps the current model and says why, because an
        // accepted one is written to the user configuration and would then
        // fail every later turn in every later session.
        for (answer, expected) in [
            ("9", "choose 1-3"),
            ("0", "choose 1-3"),
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

        // Every provider's models are offered under their own heading, and the
        // current row is marked in place.
        let mut listing = Vec::new();
        render_model_list(&mut listing, &models, &current, false).unwrap();
        let listing = String::from_utf8(listing).unwrap();
        assert!(listing.contains("[codex]"));
        assert!(listing.contains("[hari]"));
        assert!(listing.contains("1. gpt-5.6-sol  GPT-5.6-Sol"));
        assert!(listing.contains("› 2. gpt-5.6-luna"));
        assert!(listing.contains("3. mimo  on hari"));
        assert!(model_prompt(&models, &current, false)
            .contains("model [codex/gpt-5.6-luna] · Up/Down then Enter, a name, or 1-3"));
        assert!(
            model_prompt(&[], &current, false).contains("model [codex/gpt-5.6-luna] · a slug"),
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
        let cut = fit(&paint(true, sgr_cwd(), "/a/very/long/path"), 8);
        assert_eq!(visible_len(&cut), 8);
        assert!(cut.starts_with(sgr_cwd()));
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
            vec![("/model".to_owned(), "choose the provider model".to_owned())]
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
        // The keybinding list is where a shortcut is discovered, so a binding
        // the composer answers has to be named there.
        assert!(help.contains("Shift+Tab: step the approval mode"), "{help}");
    }

    /// A typed credential must not survive anywhere a later keystroke or a
    /// scrollback search could reach it.
    #[test]
    fn a_masked_line_is_not_painted_not_remembered_and_offers_no_menu() {
        let mut composer = Composer::default();
        composer.history.push_back("an earlier task".into());
        composer.set_masked(true);

        for character in "sk-secret".chars() {
            composer.press(Key::Char(character));
        }
        let frame = composer.render(80, false, "  status");
        assert!(!frame.contains("sk-secret"), "the secret was painted");
        assert!(!frame.contains("sk-"), "part of the secret was painted");
        assert!(frame.contains("•••••••••"), "one bullet per character");

        // A `/` in a secret is a character, not the start of a command.
        composer.press(Key::Char('/'));
        assert!(
            composer.menu().is_empty(),
            "a secret opened the command menu"
        );

        // The line still submits its real value, and leaves no copy behind.
        assert_eq!(
            composer.press(Key::Enter),
            Action::Submit("sk-secret/".to_owned())
        );
        assert_eq!(
            composer.history.len(),
            1,
            "the secret entered history: {:?}",
            composer.history
        );
        assert_eq!(
            composer.history.back().map(String::as_str),
            Some("an earlier task")
        );

        // Unmasking is what returns the line to ordinary behaviour.
        composer.set_masked(false);
        for character in "hello".chars() {
            composer.press(Key::Char(character));
        }
        assert!(composer.render(80, false, "  status").contains("hello"));
        assert_eq!(
            composer.press(Key::Enter),
            Action::Submit("hello".to_owned())
        );
        assert_eq!(composer.history.back().map(String::as_str), Some("hello"));
    }

    #[test]
    fn a_narrow_status_row_gives_up_the_path_before_the_branch() {
        let session = SessionId::new();
        let mut state = TuiState::new(
            "/Users/someone/Development/github/acme/arsy-code".into(),
            session,
        );
        state.set_model_route(ModelRoute::parse("myai/suiflex"));
        state.set_effort(Some(Effort::High));

        // Wide: everything, with the branch at the right edge.
        let wide = state.status_row(120, false, Some("feat/slash-menu"));
        assert!(wide.contains("/Users/someone/Development"), "{wide:?}");
        assert!(wide.ends_with("feat/slash-menu"), "{wide:?}");
        assert_eq!(visible_len(&wide), 120, "{wide:?}");

        // Narrower: the path loses its leading segments, the branch stays whole.
        let middle = state.status_row(72, false, Some("feat/slash-menu"));
        assert!(middle.contains("…/"), "{middle:?}");
        assert!(!middle.contains("/Users/someone"), "{middle:?}");
        assert!(middle.ends_with("feat/slash-menu"), "{middle:?}");
        assert!(visible_len(&middle) <= 72, "{middle:?}");

        // Narrower still: the path goes entirely before the branch is touched.
        let narrow = state.status_row(48, false, Some("feat/slash-menu"));
        assert!(!narrow.contains("arsy-code"), "{narrow:?}");
        assert!(narrow.ends_with("feat/slash-menu"), "{narrow:?}");
        assert!(visible_len(&narrow) <= 48, "{narrow:?}");

        // Only when even that cannot fit is the branch dropped, never cut.
        let tiny = state.status_row(30, false, Some("feat/slash-menu"));
        assert!(!tiny.contains("feat/"), "{tiny:?}");
        assert!(visible_len(&tiny) <= 30, "{tiny:?}");

        // Every width in between stays inside the terminal.
        for width in 20..=120 {
            let row = state.status_row(width, false, Some("feat/slash-menu"));
            assert!(
                visible_len(&row) <= width.max(MIN_WIDTH),
                "width {width}: {row:?}"
            );
        }
    }

    #[test]
    fn shift_tab_is_decoded_and_asks_the_composer_for_the_next_approval_mode() {
        for sequence in [b"\x1b[Z".as_slice(), b"\x1b[1;2Z".as_slice()] {
            let mut keys = Keys::default();
            let decoded: Vec<Key> = sequence
                .iter()
                .filter_map(|byte| keys.feed(*byte))
                .collect();
            assert_eq!(decoded, vec![Key::CycleMode], "{sequence:?}");
        }

        // The drafted line survives the mode change.
        let mut composer = Composer::default();
        for key in "write the parser".chars().map(Key::Char) {
            composer.press(key);
        }
        assert_eq!(
            composer.press(Key::CycleMode),
            Action::Submit(CYCLE_APPROVAL_MODE.to_owned())
        );
        assert_eq!(
            composer.press(Key::Enter),
            Action::Submit("write the parser".to_owned())
        );

        // A picker is collecting an answer, not a task: Shift+Tab there would
        // submit a command the picker cannot take.
        composer.set_picking(true);
        assert_eq!(composer.press(Key::CycleMode), Action::None);
    }

    #[test]
    fn plan_mode_is_visible_in_the_launch_card_and_status_row() {
        let mut state = TuiState::new("/workspace".into(), SessionId::new());
        state.set_approval_mode("plan");

        let launch = state.render(80, false);
        assert!(launch.contains("mode:"), "{launch}");
        assert!(launch.contains("PLAN"), "{launch}");
        assert!(state.status_row(80, false, None).contains("⏸ PLAN"));
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
        assert!(
            rows[3].contains(&format!("› {}", COMMANDS[0].0)),
            "{:?}",
            rows[3]
        );
        assert!(rows[4].starts_with("    "), "only one row is marked");
        assert!(
            rows.last().unwrap().contains("status"),
            "status is at the bottom"
        );
        for row in &rows {
            assert!(visible_len(row) <= 80, "{row:?}");
        }
        // Up over the bottom pad, the menu, and the status row, then across `› /`.
        assert!(
            frame.ends_with(&format!("\x1b[{}A\r\x1b[3C", COMMANDS.len() + 2)),
            "{frame:?}"
        );

        // A block taller than the screen would scroll, and the caret count back
        // to the input row would then land on the wrong one, so the menu takes
        // only the rows the terminal has left after pad, input, pad and status.
        composer.set_height(7);
        assert_eq!(composer.menu_window().0.len(), 3);
        assert_eq!(
            composer.render(80, false, "  status").split('\n').count(),
            7
        );
        composer.set_height(4);
        assert!(
            composer.menu_window().0.is_empty(),
            "no room leaves no menu"
        );
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
        assert_eq!(painted.matches(sgr_input_bg()).count(), 4);
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

    #[test]
    fn render_turn_puts_loading_at_top_and_footer_at_bottom() {
        let mut composer = Composer::default();
        let frame = composer.render_turn(
            80,
            false,
            "  ⠋ Working… · 3s · Esc cancel",
            "  hari/mimo  effort:low  /workspace  main",
        );
        let rows: Vec<&str> = frame.split('\n').collect();
        assert_eq!(rows.len(), 5, "loading, pad, input, pad, footer");
        assert!(rows[0].contains("Working…"), "loading is at the top");
        assert!(rows[2].contains("›"), "input row is on line 3");
        assert!(rows[4].contains("hari/mimo"), "footer is at the bottom");
        assert!(
            frame.ends_with("\x1b[2A\r\x1b[2C"),
            "caret returns to line 3"
        );
        assert_eq!(
            composer.clear(),
            format!("{RESET}{CARET_UP_2}\r{CLEAR_BELOW}"),
            "clear moves up 2 lines when loading is at the top"
        );
    }

    #[test]
    fn thinking_box_renders_bordered_and_fitted_lines() {
        let box_out = thinking_box(80, false, "first thought\nsecond thought that is longer");
        let lines: Vec<&str> = box_out.lines().collect();
        assert_eq!(lines.len(), 4, "top, row 1, row 2, bottom");
        assert!(lines[0].contains("✻ Thinking"));
        assert!(lines[0].starts_with("╭──"));
        assert!(lines[0].ends_with('╮'));
        assert!(lines[1].starts_with("│ "));
        assert!(lines[1].contains("first thought"));
        assert!(lines[1].ends_with(" │"));
        assert!(lines[2].contains("second thought"));
        assert!(lines[3].starts_with('╰'));
        assert!(lines[3].ends_with('╯'));
        for line in &lines {
            assert_eq!(visible_len(line), 80, "{line:?}");
        }
    }

    #[test]
    fn shift_enter_and_multiline_composer_input() {
        let mut keys = Keys::default();
        // Alt+Enter / Option+Enter (\x1b\r)
        assert_eq!(keys.feed(0x1b), None);
        assert_eq!(keys.feed(b'\r'), Some(Key::Newline));

        // CSI u Shift+Enter (\x1b[13;2u)
        for b in b"\x1b[13;2" {
            assert_eq!(keys.feed(*b), None);
        }
        assert_eq!(keys.feed(b'u'), Some(Key::Newline));

        // xterm Shift+Enter (\x1b[27;2;13~)
        for b in b"\x1b[27;2;13" {
            assert_eq!(keys.feed(*b), None);
        }
        assert_eq!(keys.feed(b'~'), Some(Key::Newline));

        let mut composer = Composer::default();
        for ch in "first".chars() {
            composer.press(Key::Char(ch));
        }
        composer.press(Key::Newline);
        for ch in "second".chars() {
            composer.press(Key::Char(ch));
        }
        assert_eq!(composer.buffer, "first\nsecond");

        let frame = composer.render(80, false, "  status");
        let rows: Vec<&str> = frame.split('\n').collect();
        assert_eq!(rows.len(), 5, "top pad, line 1, line 2, bottom pad, status");
        assert!(rows[1].contains("› first"));
        assert!(rows[2].contains("· second"));

        let committed = composer.commit("first\nsecond", false);
        assert!(committed.contains("› first\n· second\n"));
    }

    #[test]
    fn option_and_command_arrow_word_navigation() {
        let mut keys = Keys::default();
        // Option+Left (ESC b)
        assert_eq!(keys.feed(0x1b), None);
        assert_eq!(keys.feed(b'b'), Some(Key::WordLeft));

        // Option+Right (ESC f)
        assert_eq!(keys.feed(0x1b), None);
        assert_eq!(keys.feed(b'f'), Some(Key::WordRight));

        // Option+Backspace (ESC DEL)
        assert_eq!(keys.feed(0x1b), None);
        assert_eq!(keys.feed(0x7f), Some(Key::WordBackspace));

        // Ctrl+W
        assert_eq!(keys.feed(0x17), Some(Key::WordBackspace));

        // xterm Alt+Left (\x1b[1;3D)
        for b in b"\x1b[1;3" {
            assert_eq!(keys.feed(*b), None);
        }
        assert_eq!(keys.feed(b'D'), Some(Key::WordLeft));

        // Command+Left / Home (\x1b[1;9D)
        for b in b"\x1b[1;9" {
            assert_eq!(keys.feed(*b), None);
        }
        assert_eq!(keys.feed(b'D'), Some(Key::Home));

        let mut composer = Composer::default();
        for ch in "hello world arsy".chars() {
            composer.press(Key::Char(ch));
        }
        assert_eq!(composer.caret, 16);

        // WordLeft moves back by word
        composer.press(Key::WordLeft);
        assert_eq!(composer.caret, 12); // start of "arsy"

        composer.press(Key::WordLeft);
        assert_eq!(composer.caret, 6); // start of "world"

        // WordRight moves forward by word
        composer.press(Key::WordRight);
        assert_eq!(composer.caret, 12); // start of "arsy"

        // WordBackspace deletes word backward
        composer.press(Key::WordBackspace);
        assert_eq!(composer.buffer, "hello arsy");
        assert_eq!(composer.caret, 6);
    }

    #[test]
    fn ask_dialog_interactive_navigation_and_selection() {
        let mut dialog = AskDialogState::for_approval(
            "bash",
            "rm -rf target",
            "file deletion",
            Some("$ rm -rf target".to_owned()),
        );
        assert_eq!(dialog.selected, 0);
        assert_eq!(dialog.options.len(), 3);

        // Render output has border, title, and diff preview
        let rendered = dialog.render(80, false);
        assert!(rendered.contains("APPROVAL REQUIRED: bash"));
        assert!(rendered.contains("Summary: rm -rf target"));
        assert!(rendered.contains("Proposed Changes:"));
        assert!(rendered.contains("$ rm -rf target"));
        assert!(rendered.contains("1. Approve this call once"));

        // Down key navigates to next option
        assert_eq!(dialog.handle_key(Key::Down), None);
        assert_eq!(dialog.selected, 1);

        // Number 1 key immediately approves once
        assert_eq!(
            dialog.handle_key(Key::Char('1')),
            Some(AskDialogResult::Approve { note: None })
        );

        // Number 2 key always approves for session
        assert_eq!(
            dialog.handle_key(Key::Char('2')),
            Some(AskDialogResult::AlwaysApprove { note: None })
        );

        // Number 3 key denies
        assert_eq!(
            dialog.handle_key(Key::Char('3')),
            Some(AskDialogResult::Deny { note: None })
        );

        // 'n' opens custom note editing
        assert_eq!(dialog.handle_key(Key::Char('n')), None);
        assert!(dialog.editing_note);
        dialog.handle_key(Key::Char('a'));
        dialog.handle_key(Key::Char('b'));
        assert_eq!(dialog.custom_note, "ab");
        assert_eq!(dialog.handle_key(Key::Enter), None);
        assert!(!dialog.editing_note);
        assert_eq!(
            dialog.handle_key(Key::Char('1')),
            Some(AskDialogResult::Approve {
                note: Some("ab".to_owned())
            })
        );
    }

    #[test]
    fn plan_dialog_offers_implement_revise_and_cancel() {
        let mut dialog = AskDialogState::for_plan();
        let rendered = dialog.render(80, false);
        assert!(rendered.contains("PLAN READY"));
        assert!(rendered.contains("Approve and implement"));
        assert!(rendered.contains("Continue planning / revise"));
        assert!(rendered.contains("Cancel planning"));
        assert_eq!(
            dialog.handle_key(Key::Char('r')),
            Some(AskDialogResult::AlwaysApprove { note: None })
        );
        assert_eq!(
            dialog.handle_key(Key::Char('c')),
            Some(AskDialogResult::Deny { note: None })
        );
    }

    #[test]
    fn execution_boxes_render_cleanly() {
        let bash = bash_box(
            80,
            false,
            "cargo build",
            "Finished dev profile",
            Some(0),
            Duration::from_millis(150),
        );
        assert!(bash.contains("$ cargo build"));
        assert!(bash.contains("Finished dev profile"));
        assert!(bash.contains("✓ done (150ms)"));

        let tool = tool_box(
            80,
            false,
            "fs.write",
            "src/main.rs",
            "wrote 10 lines",
            true,
            Duration::from_millis(20),
        );
        assert!(tool.contains("fs.write src/main.rs"));
        assert!(tool.contains("✓ completed (20ms)"));

        let diff = diff_row(false, "src/lib.rs", 12, 3);
        assert!(diff.contains("src/lib.rs"));
        assert!(diff.contains("+12"));
        assert!(diff.contains("-3"));
        assert_eq!(tool_card_kind("bash"), ToolCardKind::Bash);
        assert_eq!(tool_card_kind("fs.edit"), ToolCardKind::File);
        assert_eq!(tool_card_kind("curl"), ToolCardKind::Network);
        assert_eq!(tool_card_kind("mcp.search"), ToolCardKind::Mcp);
        assert_eq!(tool_card_kind("search.text"), ToolCardKind::Search);
        let (bash_acc, bash_brd) = tool_card_colors(ToolCardKind::Bash, true);
        assert!(!bash_acc.is_empty());
        assert!(!bash_brd.is_empty());
        assert_eq!(tool_card_colors(ToolCardKind::Bash, false), ("", ""));

        let vivid = builtin_palette("vivid").expect("vivid theme is built in");
        assert_eq!(vivid.accent, "\x1b[38;2;88;166;255m");
        let dracula = builtin_palette("dracula").expect("dracula theme is built in");
        assert_eq!(dracula.accent, "\x1b[38;2;189;147;249m");
        let nord = builtin_palette("nord").expect("nord theme is built in");
        assert_eq!(nord.accent, "\x1b[38;2;136;192;208m");
        let running = tool_running_frame_with_output(
            false,
            "⠋",
            "bash",
            "cargo test",
            420,
            "line one\nline two",
            false,
        );
        assert!(running.contains("line two"));
        assert!(running.contains("e expand"));
        let expanded = tool_running_frame_with_output(
            false,
            "⠙",
            "bash",
            "cargo test",
            840,
            "line one\nline two",
            true,
        );
        assert!(expanded.contains("line one"));

        let state = RunningToolState {
            name: "bash",
            summary: "cargo test",
            frame: "⠋",
            elapsed_ms: 120,
            live_output: "running test",
            expanded: false,
        };
        let running_box = tool_running_box(80, false, &state);
        assert_eq!(running_box.len(), 3);
        assert!(running_box[0].contains("$ cargo test"));
        assert!(running_box[1].contains("running (120ms)"));
        assert!(running_box[2].contains("[e: expand]"));

        let long_output = (1..=20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let bounded_box = bash_box(
            80,
            false,
            "test_cmd",
            &long_output,
            Some(0),
            Duration::from_millis(50),
        );
        assert!(bounded_box.contains("earlier lines omitted"));
        assert!(bounded_box.contains("20 lines"));
    }

    #[test]
    fn session_choice_resolution_and_rows() {
        let s1 = SessionId::new();
        let s2 = SessionId::new();
        let choices = vec![
            SessionChoice {
                id: s1,
                title: Some("feature work".to_owned()),
                events: 10,
                last_seen: "2m ago".to_owned(),
            },
            SessionChoice {
                id: s2,
                title: None,
                events: 5,
                last_seen: "1h ago".to_owned(),
            },
        ];

        let (rows, selected) = session_rows(&choices, Some(s2));
        assert_eq!(selected, 1);
        assert_eq!(rows.unwrap().len(), 2);

        // Direct number resolution
        assert_eq!(resolve_session_answer("1", &choices, s1).unwrap(), s1);
        assert_eq!(resolve_session_answer("2", &choices, s1).unwrap(), s2);

        // Direct UUID resolution
        assert_eq!(
            resolve_session_answer(&s2.to_string(), &choices, s1).unwrap(),
            s2
        );
    }
}
