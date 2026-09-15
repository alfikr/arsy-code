//! Provider stream transport and cleanup primitives.
use super::*;
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
