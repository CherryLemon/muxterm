# Encoding: tmux commands, SSH commands and terminal input

Four encodings that are easy to confuse and dangerous to merge. They are
implemented by different functions on purpose, and each has its own tests.

## 1. tmux command encoding — `tmux_protocol::command`

A command is built from a static name and typed arguments, then encoded once:

```rust
Command::new("rename-window").arg("my window")   // rename-window 'my window'
Command::new("display-message").arg("-p").arg("#{session_name}")
Command::new("select-pane").arg("-t").arg("%12")
```

Rules:

* an argument is left unquoted only if every byte is in
  `[A-Za-z0-9_-./:@%+,=^]`;
* otherwise it is single-quoted (`'…'`), which is literal in tmux;
* an argument containing `'` uses double quotes, with `\`, `"`, `$` and a backtick
  backslash-escaped, and a **leading `~`** written as `\~`: tmux expands `~user`
  even inside double quotes (verified on tmux 3.4), while a single-quoted value is
  literal;
* a raw newline or carriage return is **refused** for general arguments
  (`CommandError::EmbeddedNewline`); `send_keys` handles text separately (below);
* the encoded line is bounded by `Limits::max_encoded_command_bytes`.

The encoder is the only place that assembles a command line; nothing concatenates
user text into a command string.

## 2. SSH command encoding — `tmux_client::transport::shell_quote`

`ssh host <command>` hands `<command>` to the **remote shell**, which parses it
again. That is a different grammar, so it gets a different function:

```rust
Target::Ssh { host, session, .. }   // ssh -T host "tmux -C attach -t 'dev'"
```

* the tmux command is built with tmux quoting first;
* the resulting string is passed to `ssh` as **one argv element**;
* `shell_quote` is used for values that come from the user and end up inside that
  remote command string.

A local target never goes through a shell at all: `Command::new("tmux").args([...])`.

`Target::validate` rejects a host that starts with `-` (ssh would read it as an
option), a host containing whitespace/control bytes, and a session/socket name
containing a newline, carriage return or NUL. The SSH invocation appends
`BatchMode=yes`, `ConnectTimeout=15`, `ServerAliveInterval=30` and
`ServerAliveCountMax=3` **after** the caller's options; ssh keeps the first value it
sees, so an explicit user option still wins.

## 3. Key input — `tmux_protocol::command::KeySpec`

Three kinds of input are kept apart, because a terminal backend and a tmux backend
must treat them differently:

| Kind | Sent as | Why |
|---|---|---|
| `KeySpec::Bytes` | `send-keys -H <hex…>` | every byte value survives (control bytes, invalid UTF-8, sequences) |
| `KeySpec::Literal` | `send-keys -l '<text>'` | text must not be interpreted as key names; must be valid UTF-8 |
| `KeySpec::Named` | `send-keys Enter` | a key name gives tmux the semantic key it knows |

Because `-l`, `-H` and key names are mutually exclusive per invocation, one call is
split into homogeneous runs, in order.

* **Newlines.** `\n`, `\r\n` and a lone `\r` become separate `Enter` key presses; a
  command line cannot carry a raw newline, and a paste should arrive as lines. A
  blank line produces an `Enter` of its own.
* **Size.** Long literal and `-H` runs are chunked so no encoded command exceeds
  `Limits::max_encoded_command_bytes`. Literal chunks are cut on UTF-8 character
  boundaries (tmux parses `-l` as text); arbitrary bytes belong in `Bytes`, not in
  `Literal`.
* **Invalid UTF-8.** `KeySpec::Literal` returns `CommandError::NotUtf8` instead of
  guessing how tmux would render a broken sequence.

## 4. Terminal input — `term_core::input`

Keyboard *event* handling is a GUI concern, but the byte sequences are
project-owned and tested without a window:

* `encode_mouse(&MouseEvent, &Modes)` — xterm normal (`CSI M`) and SGR
  (`CSI < … M/m`) reports, wheel, motion/drag, modifiers; returns `None` when the
  pane did not enable reporting and for coordinates the normal form cannot carry;
* `encode_paste(bytes, bracketed)` — `CSI 200 ~` / `CSI 201 ~` when the pane enabled
  bracketed paste;
* `encode_focus(focused, modes)` — `CSI I` / `CSI O` when DECSET 1004 is on.

These bytes are sent with `ControlSession::send_bytes`, i.e. `send-keys -H`. The
full rules and the mode flags are in [`input.md`](input.md).

## 5. What this layer deliberately does *not* decide

* whether the UI should use bracketed paste; the encoder is provided, the policy
  depends on the pane's mode;
* whether a terminal query response should be sent back to the pane at all — a
  control client must not blindly forward everything its own terminal emulator
  generates, and newer tmux releases have an interface for reporting specific
  responses (`refresh-client -r`);
* how a selection or a scrollback view is drawn. Neither changes what is sent to
  the pane.