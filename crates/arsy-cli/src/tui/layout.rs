//! Terminal layout, palette, and sizing primitives.
use super::*;
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

pub(crate) fn stty(args: &[&str]) -> std::io::Result<String> {
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
