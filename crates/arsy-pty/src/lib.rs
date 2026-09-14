//! A pseudoterminal for a child process, and nothing else.
//!
//! Some programs behave differently when they are talking to a terminal: a
//! REPL prints a prompt, a test runner colours its output, an installer asks
//! rather than assuming. A pipe cannot make any of that happen, because the
//! question those programs ask — `isatty(0)` — is about the file descriptor,
//! not about who is reading it.
//!
//! The whole crate is one `openpty` call plus the child-side setup that gives
//! the pseudoterminal to the process being started. It exists separately so
//! `unsafe` is relaxed for sixty lines instead of for the harness.

#[cfg(unix)]
mod unix {
    use std::{
        fs::File,
        io,
        os::fd::{FromRawFd, OwnedFd, RawFd},
        process::{Command, Stdio},
    };

    /// The two ends of one pseudoterminal.
    ///
    /// `controller` is the harness's end: what it writes appears on the
    /// child's stdin, and what the child prints can be read from it.
    /// `device` is the child's end, handed to the process at spawn.
    pub struct Pty {
        pub controller: File,
        device: OwnedFd,
    }

    impl Pty {
        /// Allocate a pseudoterminal pair.
        pub fn open() -> io::Result<Self> {
            let mut controller: RawFd = -1;
            let mut device: RawFd = -1;
            // SAFETY: both out-parameters are valid, initialised `c_int`s, and
            // the three optional pointers are null, which `openpty` documents
            // as "use the defaults". Nothing else is read or written.
            let status = unsafe {
                libc::openpty(
                    &raw mut controller,
                    &raw mut device,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if status != 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `openpty` returned success, so both descriptors are open
            // and owned by this process, and neither is owned anywhere else.
            let (controller, device) =
                unsafe { (File::from_raw_fd(controller), OwnedFd::from_raw_fd(device)) };
            Ok(Self { controller, device })
        }

        /// Point a command's stdin, stdout, and stderr at the device end, and
        /// make it the child's controlling terminal.
        ///
        /// Three separate descriptors rather than three clones of one: the
        /// standard streams are closed independently, and a shared descriptor
        /// would let the first close take the other two with it.
        pub fn attach(&self, command: &mut Command) -> io::Result<()> {
            command
                .stdin(Stdio::from(self.device.try_clone()?))
                .stdout(Stdio::from(self.device.try_clone()?))
                .stderr(Stdio::from(self.device.try_clone()?));
            // Moved into the closure as an `OwnedFd`, not handed over as a raw
            // one. The closure lives as long as the `Command`, so the parent's
            // copy is closed when the `Command` is dropped; a raw descriptor
            // here would be closed by nobody, and every pty-backed start would
            // leak one in a process that is meant to run for hours.
            let device = self.device.try_clone()?;
            // SAFETY: `pre_exec` runs between fork and exec, where only
            // async-signal-safe calls are allowed. `setsid` and `ioctl` are
            // both on that list, and neither allocates nor takes a lock.
            // `as_raw_fd` reads a field.
            unsafe {
                use std::os::unix::{io::AsRawFd, process::CommandExt};
                command.pre_exec(move || {
                    // A new session, so the pseudoterminal can become this
                    // process's controlling terminal — without it, a job
                    // control signal from the terminal reaches nobody.
                    if libc::setsid() == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    // TIOCSCTTY is already c_ulong on Linux but narrower on the BSDs,
                    // so this conversion is redundant on one target and required on
                    // another.
                    #[allow(clippy::useless_conversion)]
                    let request = libc::TIOCSCTTY.into();
                    if libc::ioctl(device.as_raw_fd(), request, 0) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            Ok(())
        }

        /// Release the harness's copy of the child's end.
        ///
        /// Until this is dropped the pseudoterminal has a writer that never
        /// writes, so a read of `controller` blocks forever instead of ending
        /// when the child exits. Call it once the child has been spawned.
        pub fn close_device(self) -> File {
            self.controller
        }
    }
}

#[cfg(unix)]
pub use unix::Pty;

/// Whether this build can allocate a pseudoterminal at all.
///
/// Windows would need ConPTY, which is a different API rather than a missing
/// call; a caller asks this and reports a capability error rather than
/// pretending a pipe is a terminal.
pub const fn supported() -> bool {
    cfg!(unix)
}
