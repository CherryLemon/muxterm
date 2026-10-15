# Encoding: tmux commands, SSH commands, key input

Three encodings that are easy to confuse and dangerous to merge. They are
implemented by three different functions on purpose.

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
  backslash-escaped;
* a raw newline or carriage return is **refused** (`CommandError::EmbeddedNewline`)
  until a capture proves how tmux parses it. Refusing is a documented limitation;
  silently corrupting a paste buffer would not be;
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

## 3. Key input — `tmux_protocol::command::KeySpec`

Three kinds of input are kept apart, because a terminal backend and a tmux backend
must treat them differently:

| Kind | Sent as | Why |
|---|---|---|
| `KeySpec::Bytes` | `send-keys -H <hex…>` | every byte value survives (control bytes, invalid UTF-8, sequences) |
| `KeySpec::Literal` | `send-keys -l '<text>'` | text must not be interpreted as key names |
| `KeySpec::Named` | `send-keys Enter` | a key name gives tmux the semantic key it knows |

Because `-l`, `-H` and key names are mutually exclusive per invocation, one call is
split into homogeneous runs, in order — cheapest first, correctness always.

## 4. What this layer deliberately does *not* decide

* whether paste content is wrapped in bracketed-paste markers — that depends on the
  pane's mode and belongs to the terminal/UI layer;
* whether a terminal query response should be sent back to the pane at all — a
  control client must not blindly forward everything its own terminal emulator
  generates, and newer tmux releases have an interface for reporting specific
  responses;
* how mouse events are translated. All three need per-pane mode knowledge and get
  their own specification when the terminal layer lands.