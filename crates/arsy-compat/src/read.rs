//! Bounded, lenient reads of another tool's files.
//!
//! Lenient because a live read happens on every launch: a missing file is
//! normal, and a malformed one is reported against itself rather than stopping
//! ARSY from starting.

use serde_json::Value;
use std::path::Path;

/// `~/.claude.json` also keeps per-project history, so it grows; anything past
/// this is not a configuration file.
const MAX_SOURCE_BYTES: u64 = 16 * 1024 * 1024;

/// `Ok(None)` when the file does not exist.
pub fn json(path: &Path) -> Result<Option<Value>, String> {
    text(path)?
        .map(|raw| {
            serde_json::from_str(&raw).map_err(|error| format!("is not valid JSON: {error}"))
        })
        .transpose()
}

/// `Ok(None)` when the file does not exist.
pub fn toml(path: &Path) -> Result<Option<toml::Table>, String> {
    text(path)?
        .map(|raw| {
            raw.parse::<toml::Table>()
                .map_err(|error| format!("is not valid TOML: {error}"))
        })
        .transpose()
}

fn text(path: &Path) -> Result<Option<String>, String> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    if metadata.len() > MAX_SOURCE_BYTES {
        return Err(format!("is larger than {MAX_SOURCE_BYTES} bytes"));
    }
    std::fs::read_to_string(path)
        .map(Some)
        .map_err(|error| error.to_string())
}

/// At most `max` bytes of a UTF-8 file, cut back to a character boundary, and
/// whether it was cut. `None` when it is missing, unreadable, or not text.
pub fn text_prefix(path: &Path, max: u64) -> Option<(String, bool)> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(max + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    let truncated = bytes.len() as u64 > max;
    bytes.truncate(usize::try_from(max).unwrap_or(usize::MAX));
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) if truncated => {
            let valid = error.utf8_error().valid_up_to();
            let mut bytes = error.into_bytes();
            bytes.truncate(valid);
            String::from_utf8(bytes).ok()?
        }
        Err(_) => return None,
    };
    Some((text, truncated))
}
