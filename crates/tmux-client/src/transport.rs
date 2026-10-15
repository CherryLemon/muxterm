//! Transports: how bytes get to and from a tmux control-mode client.
//!
//! Two rules shape this module:
//!
//! * **Two quoting layers, never one helper.**  A tmux command is a tmux command;
//!   a remote command handed to `ssh` is parsed by the *remote shell*.  They are
//!   quoted by different functions ([`crate::transport::shell_quote`] for the
//!   shell, `tmux_protocol::command::quote_arg` inside the tmux command line).
//! * **Local spawns never go through a shell.**  A local `tmux` is spawned with an
//!   argv vector, so a session name containing `;` cannot execute anything.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Where to attach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A tmux server reachable as a local process.
    Local {
        /// `-L <socket>`: use a private socket instead of the user's default one.
        socket: Option<String>,
        /// Session name to attach to.
        session: String,
    },
    /// A tmux server reachable over SSH using the system OpenSSH client.
    Ssh {
        /// Host alias or `user@host` as understood by `ssh`.
        host: String,
        /// `-L <socket>` on the remote side.
        socket: Option<String>,
        /// Session name to attach to.
        session: String,
        /// Extra options passed to `ssh` verbatim (for example `-p 2222`).
        ssh_options: Vec<String>,
    },
}

impl Target {
    /// Builds the program and its argv.
    ///
    /// For [`Target::Ssh`] the remote command is a single argv element, quoted for
    /// the remote shell by [`shell_quote`].
    #[must_use]
    pub fn command(&self) -> (String, Vec<String>) {
        match self {
            Self::Local { socket, session } => {
                let mut args = Vec::new();
                if let Some(socket) = socket {
                    args.push("-L".to_string());
                    args.push(socket.clone());
                }
                args.push("-C".to_string());
                args.push("attach".to_string());
                args.push("-t".to_string());
                args.push(session.clone());
                ("tmux".to_string(), args)
            }
            Self::Ssh {
                host,
                socket,
                session,
                ssh_options,
            } => {
                let mut tmux_argv: Vec<String> = vec!["tmux".to_string()];
                if let Some(socket) = socket {
                    tmux_argv.push("-L".to_string());
                    tmux_argv.push(shell_quote(socket));
                }
                tmux_argv.push("-C".to_string());
                tmux_argv.push("attach".to_string());
                tmux_argv.push("-t".to_string());
                tmux_argv.push(shell_quote(session));

                let mut args: Vec<String> = vec!["-T".to_string()];
                args.extend(ssh_options.iter().cloned());
                args.push(host.clone());
                args.push(tmux_argv.join(" "));
                ("ssh".to_string(), args)
            }
        }
    }

    /// Human-readable description for logs (never contains secrets).
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Local { socket, session } => match socket {
                Some(socket) => format!("local tmux -L {socket} attach -t {session}"),
                None => format!("local tmux attach -t {session}"),
            },
            Self::Ssh {
                host,
                socket,
                session,
                ..
            } => match socket {
                Some(socket) => format!("ssh {host} tmux -L {socket} attach -t {session}"),
                None => format!("ssh {host} tmux attach -t {session}"),
            },
        }
    }
}

/// Quotes one argument for a POSIX shell.
///
/// Used **only** for the command string that `ssh` hands to the remote shell.
/// tmux command arguments use `tmux_protocol::command::quote_arg` instead.
#[must_use]
pub fn shell_quote(arg: &str) -> String {
    if arg.is_empty() {
        return "''".to_string();
    }
    if arg.bytes().all(|b| {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'-' | b'_' | b'.' | b'/' | b':' | b'@' | b'%' | b'+' | b',' | b'='
            )
    }) {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('\'');
    for ch in arg.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Transport failures.
#[derive(Debug)]
pub enum TransportError {
    /// The underlying process or pipe failed.
    Io(io::Error),
    /// The peer closed the connection.
    Closed,
    /// The connection did not produce anything within the timeout.
    Timeout,
}

impl core::fmt::Display for TransportError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "transport i/o error: {err}"),
            Self::Closed => write!(f, "transport closed"),
            Self::Timeout => write!(f, "transport timed out"),
        }
    }
}

impl std::error::Error for TransportError {}

impl From<io::Error> for TransportError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// A duplex byte channel to a control-mode client.
pub trait Transport: Send {
    /// Writes bytes, or fails if the channel is closed.
    ///
    /// # Errors
    ///
    /// Returns [`TransportError::Closed`] after [`Transport::shutdown`] or EOF.
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), TransportError>;

    /// Waits up to `timeout` for the next chunk of bytes.
    ///
    /// * `Ok(Some(bytes))` — data;
    /// * `Ok(None)` — timeout, the channel is still open;
    /// * `Err(TransportError::Closed)` — the peer is gone for good.
    ///
    /// # Errors
    ///
    /// As above.
    fn recv(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>, TransportError>;

    /// Closes the write side and reaps the process.  Idempotent.
    fn shutdown(&mut self);

    /// True once the channel can no longer be used.
    fn is_closed(&self) -> bool;

    /// Human-readable description for logs.
    fn describe(&self) -> String;

    /// Recent stderr output, for diagnostics.  Never contains terminal content
    /// because it only captures what the tmux client itself printed.
    fn stderr_tail(&self) -> String;
}

enum Message {
    Data(Vec<u8>),
    Eof,
    Error(String),
}

const STDERR_TAIL_BYTES: usize = 4096;

/// A control-mode client running as a child process (local tmux or ssh).
pub struct ProcessTransport {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<Message>,
    stderr: Arc<Mutex<VecDeque<u8>>>,
    closed: bool,
    eof_seen: bool,
    description: String,
}

impl ProcessTransport {
    /// Spawns the client described by `target`.
    ///
    /// # Errors
    ///
    /// Returns the spawn error when the program cannot be started.
    pub fn spawn(target: &Target) -> io::Result<Self> {
        let (program, args) = target.command();
        let mut command = Command::new(&program);
        command.args(&args);
        Self::spawn_command(command, target.describe())
    }

    /// Spawns an already prepared command (used by tests and by `-CC` sessions).
    ///
    /// stdin/stdout/stderr are always piped: the transport needs a duplex byte
    /// channel, so the caller does not have to configure stdio.
    ///
    /// # Errors
    ///
    /// Returns the spawn error when the program cannot be started.
    pub fn spawn_command(mut command: Command, description: String) -> io::Result<Self> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (tx, rx) = mpsc::channel();

        if let Some(stdout) = stdout {
            spawn_reader(stdout, tx.clone());
        } else {
            let _ = tx.send(Message::Eof);
        }

        let tail = Arc::new(Mutex::new(VecDeque::new()));
        if let Some(stderr) = stderr {
            spawn_stderr_reader(stderr, Arc::clone(&tail));
        }

        Ok(Self {
            child,
            stdin,
            rx,
            stderr: tail,
            closed: false,
            eof_seen: false,
            description,
        })
    }

    /// The child process identifier.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}

fn spawn_reader(mut stdout: impl Read + Send + 'static, tx: Sender<Message>) {
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match stdout.read(&mut buf) {
                Ok(0) => {
                    let _ = tx.send(Message::Eof);
                    return;
                }
                Ok(n) => {
                    if tx.send(Message::Data(buf[..n].to_vec())).is_err() {
                        return;
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => {
                    let _ = tx.send(Message::Error(err.to_string()));
                    return;
                }
            }
        }
    });
}

fn spawn_stderr_reader(mut stderr: impl Read + Send + 'static, tail: Arc<Mutex<VecDeque<u8>>>) {
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 4096];
        loop {
            match stderr.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if let Ok(mut tail) = tail.lock() {
                        for byte in &buf[..n] {
                            if tail.len() == STDERR_TAIL_BYTES {
                                tail.pop_front();
                            }
                            tail.push_back(*byte);
                        }
                    }
                }
            }
        }
    });
}

impl Transport for ProcessTransport {
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
        if self.closed {
            return Err(TransportError::Closed);
        }
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(TransportError::Closed);
        };
        match stdin.write_all(bytes).and_then(|()| stdin.flush()) {
            Ok(()) => Ok(()),
            Err(err) => {
                self.closed = true;
                Err(TransportError::Io(err))
            }
        }
    }

    fn recv(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>, TransportError> {
        if self.eof_seen {
            return Err(TransportError::Closed);
        }
        match self.rx.recv_timeout(timeout) {
            Ok(Message::Data(bytes)) => Ok(Some(bytes)),
            Ok(Message::Eof) => {
                self.eof_seen = true;
                Err(TransportError::Closed)
            }
            Ok(Message::Error(message)) => {
                self.eof_seen = true;
                Err(TransportError::Io(io::Error::other(message)))
            }
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => {
                self.eof_seen = true;
                Err(TransportError::Closed)
            }
        }
    }

    fn shutdown(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        // Closing stdin is a graceful detach: tmux exits the control client but
        // leaves the session and its processes running.
        self.stdin.take();
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn is_closed(&self) -> bool {
        self.closed || self.eof_seen
    }

    fn describe(&self) -> String {
        self.description.clone()
    }

    fn stderr_tail(&self) -> String {
        match self.stderr.lock() {
            Ok(tail) => {
                String::from_utf8_lossy(&tail.iter().copied().collect::<Vec<u8>>()).into_owned()
            }
            Err(_) => String::new(),
        }
    }
}

impl Drop for ProcessTransport {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_target_never_uses_a_shell() {
        let (program, args) = Target::Local {
            socket: Some("s".into()),
            session: "dev; rm -rf /".into(),
        }
        .command();
        assert_eq!(program, "tmux");
        assert_eq!(args, vec!["-L", "s", "-C", "attach", "-t", "dev; rm -rf /"]);
    }

    #[test]
    fn ssh_target_quotes_the_remote_command_for_the_shell() {
        let (program, args) = Target::Ssh {
            host: "example-host".into(),
            socket: None,
            session: "dev; rm -rf /".into(),
            ssh_options: vec!["-p".into(), "2224".into()],
        }
        .command();
        assert_eq!(program, "ssh");
        assert_eq!(args[0], "-T");
        assert_eq!(args[1], "-p");
        assert_eq!(args[2], "2224");
        assert_eq!(args[3], "example-host");
        assert_eq!(args[4], "tmux -C attach -t 'dev; rm -rf /'");
    }

    #[test]
    fn shell_quoting_rules() {
        assert_eq!(shell_quote("dev"), "dev");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("$HOME"), "'$HOME'");
    }

    #[test]
    fn transport_reports_eof_as_closed() {
        let command = {
            let mut c = Command::new("sh");
            c.arg("-c").arg("exit 0");
            c
        };
        let mut transport =
            ProcessTransport::spawn_command(command, "test-eof".to_string()).unwrap();
        let result = transport.recv(Duration::from_secs(5));
        assert!(matches!(result, Err(TransportError::Closed)));
        assert!(transport.is_closed());
        transport.shutdown();
    }

    #[test]
    fn writes_after_shutdown_are_refused() {
        let command = Command::new("cat");
        let mut transport =
            ProcessTransport::spawn_command(command, "test-write".to_string()).unwrap();
        transport.write_all(b"hello\n").unwrap();
        transport.shutdown();
        assert!(matches!(
            transport.write_all(b"late command\n"),
            Err(TransportError::Closed)
        ));
    }
}
