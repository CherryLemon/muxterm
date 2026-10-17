# Input encoding: keys, paste, mouse and focus

**Spec version:** `input/0.1`.

The terminal/GUI layer turns an input event into bytes.  Those bytes are **not**
written to a socket directly: tmux control mode exposes no "write bytes to this
pane" command, so every path ends in `send-keys`:

| Input | `tmux_protocol::command::KeySpec` | tmux invocation |
|---|---|---|
| arbitrary bytes (mouse report, escape sequence, invalid UTF-8) | `Bytes(Vec<u8>)` | `send-keys -t %p -H 1b 5b ...` |
| text typed or pasted | `Literal(Vec<u8>)` | `send-keys -t %p -l '<text>'` |
| a semantic key | `Named("Enter")` | `send-keys -t %p Enter` |

`-l`, `-H` and key names are mutually exclusive per invocation, so `send_keys`
groups the specs into homogeneous runs, in order.

## 1. Literal text

* The bytes must be valid UTF-8. tmux parses a literal argument as text; arbitrary
  bytes must use `Bytes`.
* A `\n`, `\r\n` or lone `\r` is not sent as a raw byte (a command line cannot
  contain a newline). It becomes a separate `Enter` key press, so a multi-line
  paste arrives as the lines a user would type. A blank line produces an `Enter`.
* Long text is split into several `-l` invocations so no encoded command exceeds
  `Limits::max_encoded_command_bytes`. Splits happen on UTF-8 character
  boundaries; the byte stream order is preserved.

## 2. Mouse

`term_core::input::encode_mouse(&MouseEvent, &Modes) -> Option<Vec<u8>>` follows
xterm:

* enabled by any of DECSET 1000 (click), 1002 (button/drag), 1003 (motion);
* SGR form (`CSI < b ; x+1 ; y+1 M` press / `m` release) when DECSET 1006 is set;
* otherwise the normal `CSI M` form with `32 + code`, `32 + x + 1`,
  `32 + y + 1`; coordinates above 222 cannot be represented, so `encode_mouse`
  returns `None` (unless DECSET 1005 UTF-8 coordinates are enabled);
* modifiers add shift 4, alt 8, ctrl 16; motion adds 32; wheel is 64/65 and only
  reports presses; normal-mode release uses code 3, SGR release reuses the button
  code;
* a motion event is only reported with 1003, or with 1002 while a button is held;
* no mouse mode → `None`.

The caller sends the bytes with `ControlSession::send_bytes`, which uses
`send-keys -H`; a real pane application receives exactly those bytes
(`poc mouse` checks this against a live tmux).

## 3. Paste and focus

* `encode_paste(bytes, bracketed)`: wraps the payload in `CSI 200 ~` / `CSI 201 ~`
  when the pane enabled bracketed paste (DECSET 2004), otherwise copies it.
  Whether to use bracketed paste is a UI decision, but the encoder is project-owned
  so it can be tested without a window.
* `encode_focus(focused, modes)`: `CSI I` / `CSI O` only when DECSET 1004 is on.

## 4. What is *not* restored

The pane's *application* modes (bracketed paste, mouse reporting, application
cursor keys, focus reporting) live inside the remote program. A reconnect can
restore the screen but cannot set those modes; see
[`state-sync.md`](state-sync.md) §5. The GUI must therefore treat the modes from
its own terminal core as current only after the application has drawn again.