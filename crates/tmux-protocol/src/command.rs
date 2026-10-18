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
    /// A [`KeySpec::Literal`] was not valid UTF-8.
    ///
    /// Literal text is parsed by tmux as text; arbitrary bytes (including an
    /// invalid UTF-8 sequence split across two `-l` invocations) must use
    /// [`KeySpec::Bytes`] instead, which is byte-exact.
    NotUtf8,
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
            Self::NotUtf8 => write!(f, "literal input is not valid UTF-8; use KeySpec::Bytes"),
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
///   and a backtick are backslash-escaped, and a *leading* `~` is escaped as
///   `\~` (tmux expands `~user` even inside double quotes; a single quote is
///   literal and needs no escape);
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
    for (index, &b) in arg.iter().enumerate() {
        if index == 0 && b == b'~' {
            // tmux expands a leading `~user` even within double quotes.
            out.push(b'\\');
        }
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
    ///
    /// Newlines are not sent as bytes: every `\n`, `\r\n` or lone `\r` becomes a
    /// separate `Enter` key press, so a multi-line paste arrives as the same
    /// sequence of lines a user would type.  A blank line therefore produces an
    /// `Enter` of its own.
    ///
    /// The bytes must be valid UTF-8: tmux parses a literal argument as text, and
    /// a multi-byte character split across two chunked `-l` invocations would be
    /// corrupted.  Arbitrary bytes belong in [`KeySpec::Bytes`].
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
/// names are mutually exclusive per invocation.  A run may yield several
/// commands: literal newlines become `Enter` keys, and long literal or `-H` runs
/// are chunked so that no encoded command exceeds
/// [`Limits::max_encoded_command_bytes`].
///
/// # Errors
///
/// Propagates encoding errors from [`Command::encode`] and [`quote_arg`], and
/// returns [`CommandError::TooLong`] when a single item cannot fit in one
/// command even after chunking.
pub fn send_keys(
    pane: PaneId,
    specs: &[KeySpec],
    limits: Limits,
) -> Result<Vec<Vec<u8>>, CommandError> {
    for spec in specs {
        if let KeySpec::Literal(text) = spec {
            if std::str::from_utf8(text).is_err() {
                return Err(CommandError::NotUtf8);
            }
        }
    }
    let mut encoded = Vec::new();
    let mut run: Vec<&KeySpec> = Vec::new();
    let mut kind = RunKind::Named;

    for spec in specs {
        let next = spec.run_kind();
        if !run.is_empty() && next != kind {
            encode_run(pane, &run, kind, limits, &mut encoded)?;
            run.clear();
        }
        kind = next;
        run.push(spec);
    }
    if !run.is_empty() {
        encode_run(pane, &run, kind, limits, &mut encoded)?;
    }
    Ok(encoded)
}

/// Length of `send-keys -t <pane> [flag]` plus the trailing newline.
fn send_keys_base_len(pane: PaneId, flag: Option<&str>) -> usize {
    let pane = pane.to_string();
    let mut len = "send-keys".len() + 1 + "-t".len() + 1 + pane.len();
    if let Some(flag) = flag {
        len += 1 + flag.len();
    }
    len + 1
}

/// Encoded length of one argument, exactly as [`quote_arg`] writes it.
fn encoded_arg_len(arg: &[u8]) -> usize {
    if !arg.is_empty() && arg.iter().all(|b| is_safe_unquoted(*b)) {
        arg.len()
    } else if !arg.contains(&b'\'') {
        arg.len() + 2
    } else {
        let escapes = arg
            .iter()
            .filter(|b| matches!(b, b'\\' | b'"' | b'$' | b'`'))
            .count();
        let tilde = usize::from(arg.first() == Some(&b'~'));
        arg.len() + 2 + escapes + tilde
    }
}

/// Longest prefix of `arg` whose encoding fits in `max_encoded` bytes.
///
/// [`encoded_arg_len`] is monotonic in the prefix length, so this is a binary
/// search rather than a byte-by-byte scan.
fn max_prefix_len(arg: &[u8], max_encoded: usize) -> usize {
    let mut low = 0usize;
    let mut high = arg.len();
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        if encoded_arg_len(&arg[..mid]) <= max_encoded {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    low
}

fn encode_run(
    pane: PaneId,
    run: &[&KeySpec],
    kind: RunKind,
    limits: Limits,
    out: &mut Vec<Vec<u8>>,
) -> Result<(), CommandError> {
    match kind {
        RunKind::Hex => encode_hex_run(pane, run, limits, out),
        RunKind::Literal => encode_literal_run(pane, run, limits, out),
        RunKind::Named => {
            let mut cmd = Command::new("send-keys").flag("-t", pane.to_string());
            for spec in run {
                if let KeySpec::Named(name) = spec {
                    cmd = cmd.arg(name.as_bytes().to_vec());
                }
            }
            out.push(cmd.encode(limits)?);
            Ok(())
        }
    }
}

fn encode_hex_run(
    pane: PaneId,
    run: &[&KeySpec],
    limits: Limits,
    out: &mut Vec<Vec<u8>>,
) -> Result<(), CommandError> {
    let base = send_keys_base_len(pane, Some("-H"));
    // Each byte costs one space plus two hex digits.
    const PER_BYTE: usize = 3;
    let max_bytes = limits.max_encoded_command_bytes.saturating_sub(base) / PER_BYTE;
    if max_bytes == 0 {
        return Err(CommandError::TooLong {
            size: base.saturating_add(PER_BYTE),
            limit: limits.max_encoded_command_bytes,
        });
    }

    let mut cmd = Command::new("send-keys")
        .flag("-t", pane.to_string())
        .arg("-H");
    let mut queued = 0usize;
    for spec in run {
        if let KeySpec::Bytes(bytes) = spec {
            for byte in bytes {
                if queued == max_bytes {
                    out.push(cmd.encode(limits)?);
                    cmd = Command::new("send-keys")
                        .flag("-t", pane.to_string())
                        .arg("-H");
                    queued = 0;
                }
                cmd = cmd.arg(format!("{byte:02x}"));
                queued += 1;
            }
        }
    }
    out.push(cmd.encode(limits)?);
    Ok(())
}

/// Accumulates one `send-keys -l` invocation, splitting it when necessary.
struct LiteralBatch<'a> {
    pane: PaneId,
    limits: Limits,
    /// Encoded length of the command without arguments, including the newline.
    base: usize,
    args: Vec<Vec<u8>>,
    /// Encoded length of the command with `args` so far.
    used: usize,
    out: &'a mut Vec<Vec<u8>>,
}

impl<'a> LiteralBatch<'a> {
    fn new(pane: PaneId, limits: Limits, out: &'a mut Vec<Vec<u8>>) -> Self {
        let base = send_keys_base_len(pane, Some("-l"));
        Self {
            pane,
            limits,
            base,
            args: Vec::new(),
            used: base,
            out,
        }
    }

    /// Encoded bytes still available for one argument (including its space).
    fn free(&self) -> usize {
        self.limits
            .max_encoded_command_bytes
            .saturating_sub(self.used.saturating_add(1))
    }

    fn flush(&mut self) -> Result<(), CommandError> {
        if self.args.is_empty() {
            return Ok(());
        }
        let mut cmd = Command::new("send-keys")
            .flag("-t", self.pane.to_string())
            .arg("-l");
        for arg in self.args.drain(..) {
            cmd = cmd.arg(arg);
        }
        self.out.push(cmd.encode(self.limits)?);
        self.used = self.base;
        Ok(())
    }

    /// Types one `Enter` key, flushing the preceding literal text first.
    fn newline(&mut self) -> Result<(), CommandError> {
        self.flush()?;
        let cmd = Command::new("send-keys")
            .flag("-t", self.pane.to_string())
            .arg("Enter");
        self.out.push(cmd.encode(self.limits)?);
        Ok(())
    }

    /// Appends one complete argument (used for the legacy empty literal).
    fn push_arg(&mut self, arg: &[u8]) -> Result<(), CommandError> {
        let encoded = encoded_arg_len(arg);
        if encoded <= self.free() {
            self.args.push(arg.to_vec());
            self.used += 1 + encoded;
            return Ok(());
        }
        if !self.args.is_empty() {
            self.flush()?;
            return self.push_arg(arg);
        }
        Err(CommandError::TooLong {
            size: self.base.saturating_add(1).saturating_add(encoded),
            limit: self.limits.max_encoded_command_bytes,
        })
    }

    /// Appends text, splitting it over several `-l` commands when needed.
    fn push_text(&mut self, text: &[u8]) -> Result<(), CommandError> {
        let mut rest = text;
        while !rest.is_empty() {
            let free = self.free();
            let encoded = encoded_arg_len(rest);
            if encoded <= free {
                self.args.push(rest.to_vec());
                self.used += 1 + encoded;
                return Ok(());
            }
            if !self.args.is_empty() {
                self.flush()?;
                continue;
            }
            let mut take = max_prefix_len(rest, free);
            // Never split a UTF-8 character: tmux parses `-l` arguments as text,
            // so two invalid fragments may be rejected or replaced instead of
            // reaching the pane as the original bytes.
            while take > 0 && take < rest.len() && (rest[take] & 0xC0) == 0x80 {
                take -= 1;
            }
            if take == 0 {
                return Err(CommandError::TooLong {
                    size: self
                        .base
                        .saturating_add(1)
                        .saturating_add(encoded_arg_len(&rest[..1])),
                    limit: self.limits.max_encoded_command_bytes,
                });
            }
            let piece = &rest[..take];
            let encoded = encoded_arg_len(piece);
            self.args.push(piece.to_vec());
            self.used += 1 + encoded;
            self.flush()?;
            rest = &rest[take..];
        }
        Ok(())
    }
}

fn encode_literal_run(
    pane: PaneId,
    run: &[&KeySpec],
    limits: Limits,
    out: &mut Vec<Vec<u8>>,
) -> Result<(), CommandError> {
    let mut batch = LiteralBatch::new(pane, limits, out);
    for spec in run {
        if let KeySpec::Literal(text) = spec {
            encode_literal_text(&mut batch, text)?;
        }
    }
    batch.flush()
}

/// Splits literal text at newline boundaries and turns each boundary into an
/// `Enter` key press.
fn encode_literal_text(batch: &mut LiteralBatch<'_>, text: &[u8]) -> Result<(), CommandError> {
    if text.is_empty() {
        // Preserve the historical encoding of an empty literal: one quoted empty
        // argument (which sends nothing) rather than no command at all.
        return batch.push_arg(&[]);
    }
    let mut start = 0usize;
    let mut index = 0usize;
    while index < text.len() {
        match text[index] {
            b'\r' => {
                if start < index {
                    batch.push_text(&text[start..index])?;
                }
                index += usize::from(text.get(index + 1) == Some(&b'\n')) + 1;
                batch.newline()?;
                start = index;
            }
            b'\n' => {
                if start < index {
                    batch.push_text(&text[start..index])?;
                }
                index += 1;
                batch.newline()?;
                start = index;
            }
            _ => index += 1,
        }
    }
    if start < text.len() {
        batch.push_text(&text[start..])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(cmd: &Command) -> String {
        String::from_utf8(cmd.encode(Limits::default()).unwrap()).unwrap()
    }

    fn texts(cmds: Vec<Vec<u8>>) -> Vec<String> {
        cmds.into_iter()
            .map(|cmd| String::from_utf8(cmd).unwrap())
            .collect()
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
    fn a_leading_tilde_is_escaped_only_in_double_quotes() {
        // tmux expands `~user` even inside double quotes, so the double-quote
        // fallback has to protect a leading tilde.
        assert_eq!(
            enc(&Command::new("set").arg("~root's")),
            "set \"\\~root's\"\n"
        );
        // Without a single quote the literal single-quote branch is used, where
        // a tilde is not expanded at all.
        assert_eq!(enc(&Command::new("set").arg("~root")), "set '~root'\n");
        // Only the leading tilde needs protection.
        assert_eq!(
            enc(&Command::new("set").arg("a~root's")),
            "set \"a~root's\"\n"
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
    fn literal_newlines_become_enter_keys() {
        let cmds = send_keys(
            PaneId::new(3),
            &[KeySpec::Literal(b"one\ntwo".to_vec())],
            Limits::default(),
        )
        .unwrap();
        assert_eq!(
            texts(cmds),
            vec![
                "send-keys -t %3 -l one\n".to_string(),
                "send-keys -t %3 Enter\n".to_string(),
                "send-keys -t %3 -l two\n".to_string(),
            ]
        );
    }

    #[test]
    fn blank_lines_and_carriage_returns_each_produce_enter() {
        let cmds = send_keys(
            PaneId::new(1),
            &[KeySpec::Literal(b"a\n\nb\r\nc\rd".to_vec())],
            Limits::default(),
        )
        .unwrap();
        assert_eq!(
            texts(cmds),
            vec![
                "send-keys -t %1 -l a\n".to_string(),
                "send-keys -t %1 Enter\n".to_string(),
                "send-keys -t %1 Enter\n".to_string(),
                "send-keys -t %1 -l b\n".to_string(),
                "send-keys -t %1 Enter\n".to_string(),
                "send-keys -t %1 -l c\n".to_string(),
                "send-keys -t %1 Enter\n".to_string(),
                "send-keys -t %1 -l d\n".to_string(),
            ],
            "a blank line yields its own Enter and CRLF is one boundary"
        );
    }

    #[test]
    fn multi_line_literals_stay_ordered_between_other_runs() {
        let cmds = send_keys(
            PaneId::new(4),
            &[
                KeySpec::Named("C-c"),
                KeySpec::Literal(b"echo hi\nit's\n".to_vec()),
                KeySpec::Bytes(vec![0x03]),
            ],
            Limits::default(),
        )
        .unwrap();
        assert_eq!(
            texts(cmds),
            vec![
                "send-keys -t %4 C-c\n".to_string(),
                "send-keys -t %4 -l 'echo hi'\n".to_string(),
                "send-keys -t %4 Enter\n".to_string(),
                "send-keys -t %4 -l \"it's\"\n".to_string(),
                "send-keys -t %4 Enter\n".to_string(),
                "send-keys -t %4 -H 03\n".to_string(),
            ]
        );
    }

    #[test]
    fn long_literal_text_is_chunked_under_the_limit() {
        let limits = Limits {
            max_encoded_command_bytes: 40,
            ..Limits::default()
        };
        let text = vec![b'a'; 100];
        let cmds = send_keys(PaneId::new(1), &[KeySpec::Literal(text.clone())], limits).unwrap();
        assert!(
            cmds.len() > 1,
            "100 bytes cannot fit in one 40-byte command"
        );
        let mut joined = Vec::new();
        for cmd in &cmds {
            assert!(
                cmd.len() <= limits.max_encoded_command_bytes,
                "command of {} bytes exceeds the limit: {}",
                cmd.len(),
                String::from_utf8_lossy(cmd)
            );
            let payload = cmd
                .strip_prefix(b"send-keys -t %1 -l ")
                .expect("literal chunk keeps the prefix");
            let payload = payload.strip_suffix(b"\n").expect("newline terminator");
            joined.extend_from_slice(payload);
        }
        assert_eq!(joined, text, "chunking must not lose or reorder text");
    }

    #[test]
    fn long_quoted_literal_text_is_chunked_under_the_limit() {
        let limits = Limits {
            max_encoded_command_bytes: 48,
            ..Limits::default()
        };
        // Spaces and single quotes force double quoting with escapes, which
        // changes the encoded size of each candidate chunk.
        let text = b"it's a long string with 'quotes' and $HOME and `ticks` ".repeat(4);
        let cmds = send_keys(PaneId::new(1), &[KeySpec::Literal(text.clone())], limits).unwrap();
        assert!(cmds.len() > 1);
        for cmd in &cmds {
            assert!(
                cmd.len() <= limits.max_encoded_command_bytes,
                "command of {} bytes exceeds the limit: {:?}",
                cmd.len(),
                String::from_utf8_lossy(cmd)
            );
        }
    }

    #[test]
    fn long_byte_runs_are_chunked_under_the_limit() {
        let limits = Limits {
            max_encoded_command_bytes: 32,
            ..Limits::default()
        };
        let bytes: Vec<u8> = (0u8..40).collect();
        let cmds = send_keys(PaneId::new(5), &[KeySpec::Bytes(bytes.clone())], limits).unwrap();
        // 19 bytes of prefix and terminator leave (32 - 19) / 3 = 4 bytes per command.
        assert_eq!(cmds.len(), 10);
        let mut decoded = Vec::new();
        for cmd in &cmds {
            assert!(cmd.len() <= 32, "command too long: {cmd:?}");
            let payload = cmd
                .strip_prefix(b"send-keys -t %5 -H ")
                .expect("hex chunk keeps the prefix");
            let payload = payload.strip_suffix(b"\n").expect("newline terminator");
            for token in payload.split(|b| *b == b' ') {
                let token = std::str::from_utf8(token).expect("hex digits are ASCII");
                decoded.push(u8::from_str_radix(token, 16).expect("valid hex byte"));
            }
        }
        assert_eq!(decoded, bytes, "hex payloads preserve every byte");
    }

    #[test]
    fn a_limit_that_cannot_hold_one_item_is_rejected() {
        let limits = Limits {
            max_encoded_command_bytes: 16,
            ..Limits::default()
        };
        assert!(matches!(
            send_keys(PaneId::new(1), &[KeySpec::Bytes(vec![0x03])], limits),
            Err(CommandError::TooLong { .. })
        ));
        assert!(matches!(
            send_keys(PaneId::new(1), &[KeySpec::Literal(b"a".to_vec())], limits),
            Err(CommandError::TooLong { .. })
        ));
    }

    #[test]
    fn no_command_exceeds_a_small_limit_for_mixed_input() {
        let limits = Limits {
            max_encoded_command_bytes: 64,
            ..Limits::default()
        };
        let cmds = send_keys(
            PaneId::new(2),
            &[
                KeySpec::Literal(b"first line\nsecond line with spaces\n\n".to_vec()),
                KeySpec::Bytes((0u8..32).collect()),
                KeySpec::Named("Enter"),
                KeySpec::Named("C-c"),
            ],
            limits,
        )
        .unwrap();
        assert!(cmds.len() > 4);
        for cmd in &cmds {
            assert!(
                cmd.len() <= limits.max_encoded_command_bytes,
                "command of {} bytes exceeds the limit: {}",
                cmd.len(),
                String::from_utf8_lossy(cmd)
            );
        }
    }

    #[test]
    fn adversarial_pane_input_cannot_break_out_of_quoting() {
        // A paste that tries to inject a second command: newlines become Enter
        // key presses rather than raw bytes inside a command line.
        let cmds = send_keys(
            PaneId::new(1),
            &[KeySpec::Literal(b"x\nkill-server\n".to_vec())],
            Limits::default(),
        )
        .unwrap();
        assert_eq!(
            texts(cmds),
            vec![
                "send-keys -t %1 -l x\n".to_string(),
                "send-keys -t %1 Enter\n".to_string(),
                "send-keys -t %1 -l kill-server\n".to_string(),
                "send-keys -t %1 Enter\n".to_string(),
            ]
        );

        // `Command::encode` itself still refuses raw newlines for other commands.
        assert_eq!(
            Command::new("set-buffer")
                .arg("x\nkill-server")
                .encode(Limits::default()),
            Err(CommandError::EmbeddedNewline)
        );

        let cmd = Command::new("rename-window").arg("'; kill-server; '");
        assert_eq!(enc(&cmd), "rename-window \"'; kill-server; '\"\n");
    }

    #[test]
    fn multibyte_literal_chunks_do_not_split_characters() {
        let limits = Limits {
            max_encoded_command_bytes: 40,
            ..Limits::default()
        };
        let text = "中文中文中文中文中文".as_bytes().to_vec();
        let cmds = send_keys(PaneId::new(1), &[KeySpec::Literal(text.clone())], limits).unwrap();
        assert!(cmds.len() > 1, "the text cannot fit in one 40-byte command");
        let mut joined = Vec::new();
        for cmd in &cmds {
            assert!(
                cmd.len() <= limits.max_encoded_command_bytes,
                "command of {} bytes exceeds the limit: {:?}",
                cmd.len(),
                String::from_utf8_lossy(cmd)
            );
            // Every chunk must be valid UTF-8 on its own: tmux parses `-l`
            // arguments as text, and half a character would be replaced.
            let encoded = String::from_utf8(cmd.clone()).expect("a chunk is valid UTF-8");
            let payload = encoded
                .strip_prefix("send-keys -t %1 -l '")
                .and_then(|rest| rest.strip_suffix("'\n"))
                .expect("literal chunk keeps the prefix and quotes");
            joined.extend_from_slice(payload.as_bytes());
        }
        assert_eq!(joined, text, "chunking must preserve the characters");
    }

    #[test]
    fn invalid_utf8_literals_are_rejected_but_bytes_are_not() {
        assert_eq!(
            send_keys(
                PaneId::new(1),
                &[KeySpec::Literal(vec![0xff])],
                Limits::default()
            ),
            Err(CommandError::NotUtf8)
        );
        assert!(
            send_keys(
                PaneId::new(1),
                &[KeySpec::Bytes(vec![0xff])],
                Limits::default()
            )
            .is_ok(),
            "arbitrary bytes belong in KeySpec::Bytes and must remain possible"
        );
    }
}
