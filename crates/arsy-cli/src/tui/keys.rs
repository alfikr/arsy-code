//! Terminal byte decoding and semantic input actions.
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
    /// Page up/down scroll a long plan preview.
    PageUp,
    PageDown,
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
        if sequence == b"\x1b[5~" {
            return Some(Key::PageUp);
        }
        if sequence == b"\x1b[6~" {
            return Some(Key::PageDown);
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

/// The typed equivalent of Shift+Tab, kept for command documentation.
pub const CYCLE_APPROVAL_MODE: &str = "/approval cycle";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    Submit(String),
    /// Change approval mode without submitting or queueing the draft.
    CycleMode,
    Quit,
    Redraw,
    None,
}
