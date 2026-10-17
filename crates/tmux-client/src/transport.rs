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
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
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

/// Why a [`Target`] cannot be turned into a process invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetError {
    /// A session name is empty.
    EmptySession,
    /// A session name contains a newline, carriage return or NUL.
    InvalidSession,
    /// A socket name is empty or contains an unusable byte.
    InvalidSocket,
    /// An SSH host is empty, starts with `-` (which would be read as an option),
    /// or contains whitespace/control bytes.
    InvalidHost,
    /// An SSH option contains a NUL byte, which an argv element cannot carry.
    InvalidSshOption,
}

impl core::fmt::Display for TargetError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::EmptySession => write!(f, "the session name is empty"),
            Self::InvalidSession => {
                write!(
                    f,
                    "the session name contains a newline, carriage return or NUL"
                )
            }
            Self::InvalidSocket => {
                write!(f, "the socket name is empty or contains a newline or NUL")
            }
            Self::InvalidHost => write!(
                f,
                "the SSH host is empty, starts with '-', or contains whitespace/control bytes"
            ),
            Self::InvalidSshOption => write!(f, "an SSH option contains a NUL byte"),
        }
    }
}

impl std::error::Error for TargetError {}

/// A tmux object name (session/socket) that can travel through argv or a remote
/// command line without changing the meaning of the command.
fn valid_tmux_name(name: &str) -> bool {
    !name.is_empty() && !name.bytes().any(|byte| matches!(byte, 0 | b'\n' | b'\r'))
}

/// An SSH destination that cannot be mistaken for an option or split by the
/// remote shell.
fn valid_ssh_host(host: &str) -> bool {
    !host.is_empty()
        && !host.starts_with('-')
        && !host
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_whitespace() || byte.is_ascii_control())
}

impl Target {
    /// Checks that every field can be passed to the process without a quoting
    /// accident.
    ///
    /// # Errors
    ///
    /// Returns the offending field as a [`TargetError`].  This is deliberately
    /// strict: a host beginning with `-` would be parsed by `ssh` as an option,
    /// and a session name containing a newline cannot be represented in a command
    /// line.
    pub fn validate(&self) -> Result<(), TargetError> {
        match self {
            Self::Local { socket, session } => {
                if session.is_empty() {
                    return Err(TargetError::EmptySession);
                }
                if !valid_tmux_name(session) {
                    return Err(TargetError::InvalidSession);
                }
                if let Some(socket) = socket {
                    if !valid_tmux_name(socket) {
                        return Err(TargetError::InvalidSocket);
                    }
                }
            }
            Self::Ssh {
                host,
                socket,
                session,
                ssh_options,
            } => {
                if !valid_ssh_host(host) {
                    return Err(TargetError::InvalidHost);
                }
                if session.is_empty() {
                    return Err(TargetError::EmptySession);
                }
                if !valid_tmux_name(session) {
                    return Err(TargetError::InvalidSession);
                }
                if let Some(socket) = socket {
                    if !valid_tmux_name(socket) {
                        return Err(TargetError::InvalidSocket);
                    }
                }
                if ssh_options
                    .iter()
                    .any(|option| option.as_bytes().contains(&0))
                {
                    return Err(TargetError::InvalidSshOption);
                }
            }
        }
        Ok(())
    }

    /// Builds the program and its argv.
    ///
    /// For [`Target::Ssh`] the remote command is a single argv element, quoted for
    /// the remote shell by [`shell_quote`].
    ///
    /// The SSH invocation appends a small set of robustness defaults *after* the
    /// caller's options: `ssh` keeps the first value it sees for an option, so a
    /// user-supplied `-o` still wins, while a plain target never hangs on an
    /// interactive password prompt (`BatchMode=yes`) or a silent half-open
    /// connection (`ServerAliveInterval`).
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
                for option in SSH_DEFAULT_OPTIONS {
                    args.push((*option).to_string());
                }
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

/// Options appended to every SSH invocation.
///
/// They come after the caller's `ssh_options`, and `ssh` keeps the first value it
/// sees for an option, so an explicit user value always wins.
const SSH_DEFAULT_OPTIONS: &[&str] = &[
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=15",
    "-o",
    "ServerAliveInterval=30",
    "-o",
    "ServerAliveCountMax=3",
];

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

/// Chunks the reader thread may run ahead of the consumer.
///
/// The parser has bounds of its own, but the pipe between the child process and
/// the parser is upstream of them: an unbounded channel would let a slow GUI grow
/// memory without limit.  64 chunks of 64 KiB is a 4 MiB ceiling; when it is full
/// the reader blocks and the child's pty/pipe applies backpressure.
const TRANSPORT_QUEUE_CHUNKS: usize = 64;

/// A control-mode client running as a child process (local tmux or ssh).
pub struct ProcessTransport {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<Message>,
    stderr: Arc<Mutex<VecDeque<u8>>>,
    closed: bool,
    eof_seen: bool,
    /// Set once the child has been waited for; `closed` is not enough because a
    /// failed write also closes the channel but must still be reaped.
    reaped: bool,
    description: String,
}

impl ProcessTransport {
    /// Spawns the client described by `target`.
    ///
    /// # Errors
    ///
    /// Returns the spawn error when the program cannot be started.
    pub fn spawn(target: &Target) -> io::Result<Self> {
        target
            .validate()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
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
        let (tx, rx) = mpsc::sync_channel(TRANSPORT_QUEUE_CHUNKS);

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
            reaped: false,
            description,
        })
    }

    /// The child process identifier.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}

fn spawn_reader(mut stdout: impl Read + Send + 'static, tx: SyncSender<Message>) {
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
        // `closed` is also set by a failed write; only `reaped` proves the child
        // was waited for.  Returning early on `closed` used to leak a zombie (and
        // possibly a live tmux/ssh client) after the first write error.
        if self.reaped {
            return;
        }
        self.reaped = true;
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
        assert_eq!(
            args,
            vec![
                "-T",
                "-p",
                "2224",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=15",
                "-o",
                "ServerAliveInterval=30",
                "-o",
                "ServerAliveCountMax=3",
                "example-host",
                "tmux -C attach -t 'dev; rm -rf /'",
            ]
        );
        assert_eq!(
            args.last().map(String::as_str),
            Some("tmux -C attach -t 'dev; rm -rf /'"),
            "the remote command stays one argv element"
        );
    }

    #[test]
    fn target_validation_rejects_option_and_line_injection() {
        let bad_host = Target::Ssh {
            host: "-oProxyCommand=sh".into(),
            socket: None,
            session: "dev".into(),
            ssh_options: Vec::new(),
        };
        assert_eq!(bad_host.validate(), Err(TargetError::InvalidHost));

        let newline = Target::Local {
            socket: None,
            session: "dev\nkill-server".into(),
        };
        assert_eq!(newline.validate(), Err(TargetError::InvalidSession));

        let empty_socket = Target::Local {
            socket: Some(String::new()),
            session: "dev".into(),
        };
        assert_eq!(empty_socket.validate(), Err(TargetError::InvalidSocket));

        let ssh_with_nul = Target::Ssh {
            host: "host".into(),
            socket: None,
            session: "dev".into(),
            ssh_options: vec!["-o\u{0}x".into()],
        };
        assert_eq!(ssh_with_nul.validate(), Err(TargetError::InvalidSshOption));

        assert!(Target::Ssh {
            host: "user@example-host".into(),
            socket: Some("my socket".into()),
            session: "my session".into(),
            ssh_options: vec!["-p".into(), "2224".into()],
        }
        .validate()
        .is_ok());
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
        // A second shutdown must not wait/kill the same child again.
        transport.shutdown();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_failed_write_is_still_reaped_on_shutdown() {
        // The child closes its stdin and then sleeps.  Writing to the closed pipe
        // fails and marks the transport closed; shutdown must still kill and wait,
        // otherwise the process is left behind.
        let mut command = Command::new("sh");
        command.arg("-c").arg("exec 0<&-; sleep 30");
        let mut transport =
            ProcessTransport::spawn_command(command, "test-reap".to_string()).unwrap();
        let pid = transport.pid();
        std::thread::sleep(Duration::from_millis(300));
        let write = transport.write_all(b"x");
        assert!(write.is_err(), "the closed pipe must reject the write");
        assert!(transport.is_closed());
        transport.shutdown();
        let proc_entry = format!("/proc/{pid}");
        assert!(
            !std::path::Path::new(&proc_entry).exists(),
            "the child must have been reaped after a failed write"
        );
    }
}
