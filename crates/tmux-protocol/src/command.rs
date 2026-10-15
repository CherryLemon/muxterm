//! Typed encoders for tmux commands.
//!
//! Commands are assembled from typed arguments and encoded once, at the edge.
//! Nothing in this module concatenates user text into a command string, and SSH
//! quoting is deliberately *not* implemented here: the two quoting layers are
//! different grammars and must never share a helper (see `docs/spec/encoding.md`).

use crate::ids::PaneId;
use crate::limits::Limits;

/// Failure modes of command encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandError {
    /// An argument contained a raw newline or carriage return.
    ///
    /// The parser accepts `\n` inside double quotes, but this implementation has
    /// not verified that behaviour against the compatibility matrix yet, so it
    /// refuses to guess and reports the limitation instead of corrupting data.
    EmbeddedNewline,
    /// The encoded command exceeded [`Limits::max_encoded_command_bytes`].
    TooLong {
        /// Size of the encoded command.
        size: usize,
        /// Configured maximum.
        limit: usize,
    },
    /// A command was built with no command name.
    EmptyCommandName,
}

impl core::fmt::Display for CommandError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::EmbeddedNewline => write!(f, "argument contains a raw newline"),
            Self::TooLong { size, limit } => {
                write!(f, "encoded command is {size} bytes (limit {limit})")
            }
            Self::EmptyCommandName => write!(f, "command name is empty"),
        }
    }
}

impl std::error::Error for CommandError {}

/// Bytes that may appear unquoted in a tmux command line.
///
/// Everything else is quoted; see [`quote_arg`].
fn is_safe_unquoted(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'-' | b'_' | b'.' | b'/' | b':' | b'@' | b'%' | b'+' | b',' | b'=' | b'^'
        )
}

/// Writes `arg` into `out` using tmux's quoting rules.
///
/// * an argument that is empty or contains any non-safe byte is quoted;
/// * single quotes are preferred because they are literal;
/// * arguments containing `'` fall back to double quotes, where `\`, `"`, `$`
///   and a backtick are backslash-escaped;
/// * raw newlines are rejected (see [`CommandError::EmbeddedNewline`]).
pub fn quote_arg(arg: &[u8], out: &mut Vec<u8>) -> Result<(), CommandError> {
    if arg.iter().any(|b| *b == b'\n' || *b == b'\r') {
        return Err(CommandError::EmbeddedNewline);
    }
    if !arg.is_empty() && arg.iter().all(|b| is_safe_unquoted(*b)) {
        out.extend_from_slice(arg);
        return Ok(());
    }
    if !arg.contains(&b'\'') {
        out.push(b'\'');
        out.extend_from_slice(arg);
        out.push(b'\'');
        return Ok(());
    }
    out.push(b'"');
    for &b in arg {
        if matches!(b, b'\\' | b'"' | b'$' | b'`') {
            out.push(b'\\');
        }
        out.push(b);
    }
    out.push(b'"');
    Ok(())
}

/// A tmux command under construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    name: &'static str,
    args: Vec<Vec<u8>>,
}

impl Command {
    /// Starts a command with a static name (never user-controlled).
    #[must_use]
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            args: Vec::new(),
        }
    }

    /// Appends one argument.
    #[must_use]
    pub fn arg(mut self, arg: impl Into<Vec<u8>>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Appends one argument only when `Some`.
    #[must_use]
    pub fn arg_opt(self, arg: Option<impl Into<Vec<u8>>>) -> Self {
        match arg {
            Some(value) => self.arg(value),
            None => self,
        }
    }

    /// Appends `-flag value`.
    #[must_use]
    pub fn flag(self, flag: &'static str, value: impl Into<Vec<u8>>) -> Self {
        self.arg(flag).arg(value)
    }

    /// The command name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// The arguments, for assertions and logging.
    #[must_use]
    pub fn args(&self) -> &[Vec<u8>] {
        &self.args
    }

    /// Encodes the command including the trailing newline.
    pub fn encode(&self, limits: Limits) -> Result<Vec<u8>, CommandError> {
        if self.name.is_empty() {
            return Err(CommandError::EmptyCommandName);
        }
        let mut out = Vec::with_capacity(32);
        out.extend_from_slice(self.name.as_bytes());
        for arg in &self.args {
            out.push(b' ');
            quote_arg(arg, &mut out)?;
        }
        out.push(b'\n');
        if out.len() > limits.max_encoded_command_bytes {
            return Err(CommandError::TooLong {
                size: out.len(),
                limit: limits.max_encoded_command_bytes,
            });
        }
        Ok(out)
    }
}

/// One piece of typed input destined for a pane.
///
/// The three kinds are kept apart because a terminal backend and a tmux backend
/// must treat them differently (see `docs/spec/input.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySpec {
    /// Raw bytes, sent with `send-keys -H` so that every byte value survives.
    Bytes(Vec<u8>),
    /// Literal text, sent with `send-keys -l` so key names are not interpreted.
    Literal(Vec<u8>),
    /// A tmux key name such as `Enter`, `C-c`, `M-x`, `Up`, `F5`.
    Named(&'static str),
}

/// How a spec has to be sent.  `send-keys` applies `-l` and `-H` to *every*
/// argument of one invocation, so a run may only contain one kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunKind {
    Hex,
    Literal,
    Named,
}

impl KeySpec {
    /// The invocation kind this spec requires.
    #[must_use]
    const fn run_kind(&self) -> RunKind {
        match self {
            Self::Bytes(_) => RunKind::Hex,
            Self::Literal(_) => RunKind::Literal,
            Self::Named(_) => RunKind::Named,
        }
    }
}

/// Encodes typed input for a pane.
///
/// Specs are grouped into homogeneous runs, because `send-keys -l`, `-H` and key
/// names are mutually exclusive per invocation.  A single call therefore yields
/// one command per run, in order.
///
/// # Errors
///
/// Propagates encoding errors from [`Command::encode`] and [`quote_arg`].
pub fn send_keys(
    pane: PaneId,
    specs: &[KeySpec],
    limits: Limits,
) -> Result<Vec<Vec<u8>>, CommandError> {
    let mut encoded = Vec::new();
    let mut run: Vec<&KeySpec> = Vec::new();
    let mut kind = RunKind::Named;

    for spec in specs {
        let next = spec.run_kind();
        if !run.is_empty() && next != kind {
            encoded.push(encode_run(pane, &run, kind, limits)?);
            run.clear();
        }
        kind = next;
        run.push(spec);
    }
    if !run.is_empty() {
        encoded.push(encode_run(pane, &run, kind, limits)?);
    }
    Ok(encoded)
}

fn encode_run(
    pane: PaneId,
    run: &[&KeySpec],
    kind: RunKind,
    limits: Limits,
) -> Result<Vec<u8>, CommandError> {
    let mut cmd = Command::new("send-keys").flag("-t", pane.to_string());
    match kind {
        RunKind::Hex => {
            cmd = cmd.arg("-H");
            for spec in run {
                if let KeySpec::Bytes(bytes) = spec {
                    for b in bytes {
                        cmd = cmd.arg(format!("{b:02x}"));
                    }
                }
            }
        }
        RunKind::Literal => {
            cmd = cmd.arg("-l");
            for spec in run {
                if let KeySpec::Literal(text) = spec {
                    cmd = cmd.arg(text.clone());
                }
            }
        }
        RunKind::Named => {
            for spec in run {
                if let KeySpec::Named(name) = spec {
                    cmd = cmd.arg(name.as_bytes().to_vec());
                }
            }
        }
    }
    cmd.encode(limits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(cmd: &Command) -> String {
        String::from_utf8(cmd.encode(Limits::default()).unwrap()).unwrap()
    }

    #[test]
    fn simple_arguments_stay_unquoted() {
        assert_eq!(
            enc(&Command::new("list-windows").arg("-t").arg("dev")),
            "list-windows -t dev\n"
        );
        assert_eq!(
            enc(&Command::new("select-pane").arg("-t").arg("%12")),
            "select-pane -t %12\n"
        );
    }

    #[test]
    fn format_strings_are_quoted_so_they_are_not_expanded_by_the_shell_or_split() {
        assert_eq!(
            enc(&Command::new("display-message")
                .arg("-p")
                .arg("#{session_name}")),
            "display-message -p '#{session_name}'\n"
        );
    }

    #[test]
    fn spaces_and_specials_are_quoted() {
        assert_eq!(
            enc(&Command::new("rename-window").arg("my window")),
            "rename-window 'my window'\n"
        );
        assert_eq!(
            enc(&Command::new("rename-window").arg("a;b")),
            "rename-window 'a;b'\n"
        );
        assert_eq!(
            enc(&Command::new("rename-window").arg("")),
            "rename-window ''\n"
        );
    }

    #[test]
    fn single_quotes_fall_back_to_double_quotes() {
        assert_eq!(
            enc(&Command::new("rename-window").arg("it's")),
            "rename-window \"it's\"\n"
        );
        // No single quote in the argument: single quoting is literal, so it wins
        // even when the text contains `"`, `\\`, `$` or a backtick.
        assert_eq!(
            enc(&Command::new("rename-window").arg("a\"b\\c$d`e")),
            "rename-window 'a\"b\\c$d`e'\n"
        );
        // A single quote forces double quoting, where those bytes are escaped.
        assert_eq!(
            enc(&Command::new("rename-window").arg("it's \"quoted\" $HOME `x`")),
            "rename-window \"it's \\\"quoted\\\" \\$HOME \\`x\\`\"\n"
        );
    }

    #[test]
    fn newlines_are_rejected_not_guessed() {
        let err = Command::new("set-buffer")
            .arg("a\nb")
            .encode(Limits::default());
        assert_eq!(err, Err(CommandError::EmbeddedNewline));
    }

    #[test]
    fn overlong_commands_are_rejected() {
        let limits = Limits {
            max_encoded_command_bytes: 8,
            ..Limits::default()
        };
        assert!(matches!(
            Command::new("list-windows").arg("dev").encode(limits),
            Err(CommandError::TooLong { .. })
        ));
    }

    #[test]
    fn bytes_are_sent_as_hex() {
        let cmds = send_keys(
            PaneId::new(1),
            &[KeySpec::Bytes(vec![0x1b, 0x5b, 0x41, 0xff])],
            Limits::default(),
        )
        .unwrap();
        assert_eq!(cmds.len(), 1);
        assert_eq!(
            String::from_utf8(cmds[0].clone()).unwrap(),
            "send-keys -t %1 -H 1b 5b 41 ff\n"
        );
    }

    #[test]
    fn mixed_input_is_split_into_homogeneous_runs() {
        let cmds = send_keys(
            PaneId::new(2),
            &[
                KeySpec::Named("Enter"),
                KeySpec::Literal(b"hi there".to_vec()),
                KeySpec::Bytes(vec![0x03]),
            ],
            Limits::default(),
        )
        .unwrap();
        let text: Vec<String> = cmds
            .into_iter()
            .map(|c| String::from_utf8(c).unwrap())
            .collect();
        assert_eq!(
            text,
            vec![
                "send-keys -t %2 Enter\n".to_string(),
                "send-keys -t %2 -l 'hi there'\n".to_string(),
                "send-keys -t %2 -H 03\n".to_string(),
            ]
        );
    }

    #[test]
    fn consecutive_literals_share_one_command() {
        let cmds = send_keys(
            PaneId::new(0),
            &[
                KeySpec::Literal(b"a".to_vec()),
                KeySpec::Literal(b"b c".to_vec()),
            ],
            Limits::default(),
        )
        .unwrap();
        assert_eq!(cmds.len(), 1);
        assert_eq!(
            String::from_utf8(cmds[0].clone()).unwrap(),
            "send-keys -t %0 -l a 'b c'\n"
        );
    }

    #[test]
    fn adversarial_pane_input_cannot_break_out_of_quoting() {
        // A pane title or paste content that tries to inject a second command.
        let cmds = send_keys(
            PaneId::new(1),
            &[KeySpec::Literal(b"x\nkill-server\n".to_vec())],
            Limits::default(),
        );
        assert_eq!(cmds, Err(CommandError::EmbeddedNewline));

        let cmd = Command::new("rename-window").arg("'; kill-server; '");
        assert_eq!(enc(&cmd), "rename-window \"'; kill-server; '\"\n");
    }
}
