//! Small helpers for driving a private tmux server from the PoC scenarios.
//!
//! Everything here uses explicit argv (never a shell) so a session or window name
//! taken from the environment cannot execute anything.

use std::process::Command;

/// A tmux server addressed through its own socket, so the developer's server (and
/// the developer's terminal) is never touched.
#[derive(Debug, Clone)]
pub struct Tmux {
    socket: String,
}

/// Failure of a tmux invocation.
#[derive(Debug)]
pub struct TmuxError {
    /// Rendered command, for the failure message.
    pub command: String,
    /// Exit status, when the process ran.
    pub status: Option<i32>,
    /// Captured stderr.
    pub stderr: String,
}

impl core::fmt::Display for TmuxError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "tmux {} failed (status {:?}): {}",
            self.command,
            self.status,
            self.stderr.trim()
        )
    }
}

impl std::error::Error for TmuxError {}

impl Tmux {
    /// Addresses a server by socket name.
    #[must_use]
    pub fn new(socket: impl Into<String>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    /// The socket name (used to make scenarios reproducible and isolated).
    #[must_use]
    pub fn socket(&self) -> &str {
        &self.socket
    }

    /// Runs a tmux command, failing on a non-zero exit status.
    ///
    /// # Errors
    ///
    /// Returns [`TmuxError`] when tmux reports a failure.
    pub fn run(&self, args: &[&str]) -> Result<String, TmuxError> {
        let output = Command::new("tmux")
            .arg("-L")
            .arg(&self.socket)
            .args(args)
            .output()
            .map_err(|err| TmuxError {
                command: args.join(" "),
                status: None,
                stderr: err.to_string(),
            })?;
        if !output.status.success() {
            return Err(TmuxError {
                command: args.join(" "),
                status: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Runs a tmux command and ignores failures (used for teardown).
    pub fn run_ignoring_errors(&self, args: &[&str]) {
        let _ = Command::new("tmux")
            .arg("-L")
            .arg(&self.socket)
            .args(args)
            .output();
    }

    /// Kills the whole server; safe to call when nothing is running.
    pub fn kill_server(&self) {
        self.run_ignoring_errors(&["kill-server"]);
    }

    /// True when the server answers.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.run(&["list-sessions"]).is_ok()
    }
}
