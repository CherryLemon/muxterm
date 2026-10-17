# Independent specification: the tmux control-mode protocol, as we implement it

**Spec version:** `control-mode/0.2` (kept in step with
`tmux_protocol::SPEC_VERSION`).

## Provenance

This document is written from:

1. the public `tmux(1)` manual page, section *CONTROL MODE*;
2. the public tmux wiki page *Control Mode*;
3. black-box captures recorded by `tools/probe/probe_control_mode.py` and stored
   in `crates/tmux-protocol/tests/fixtures/` (with their `manifest.json`).

Every rule marked **[observed]** is backed by a capture; every rule marked
**[documented]** comes from 1 or 2; rules marked **[ours]** are implementation
decisions (limits, ordering, recovery) that the protocol leaves open.

No iTerm2 source code or derived design document was used. See
[`../clean-room.md`](../clean-room.md).

## 1. Stream shape

The server speaks a **line-oriented, byte-oriented** protocol on a single stream:

```
%begin <time> <number> <flags>\n
<output line>\n            (zero or more, arbitrary bytes)
%end <time> <number> <flags>\n      or
%error <time> <number> <flags>\n
```

and, outside those blocks, single-line notifications that always start with `%`.

* **[documented]** Notifications are never emitted inside a command response
  block. Therefore a `%`-prefixed line *inside* a block is data, not an event.
  Our parser implements exactly this and has a regression test for it.
* **[documented]** The response numbers are the server's own; a client must not
  assume they match its own request counter. Our queue matches positionally and
  keeps the server number only for diagnostics.
* **[observed]** `%begin`/`%end` carry three fields; a block's `%end` repeats the
  `%begin` number.
* **[observed — important]** The transport changes the line ending. Over a pipe
  (`-C`, recorded in `attach_and_commands.*`) lines end with `\n`; on a PTY
  (`-CC`, recorded in `cc_entry.*`) they end with `\r\n`, because the PTY has
  `ONLCR` set. Since `%output` escapes every control byte, a CR immediately before
  the LF can only be that translation, so the parser removes exactly one trailing
  CR and keeps any other CR as data.

## 2. Notifications

| Notification | Meaning | Parsed into |
|---|---|---|
| `%output %<pane> <data>` | pane output, octal-escaped | `Event::Output` |
| `%extended-output %<pane> <age> : <data>` | output with an age (flow control) | `Event::ExtendedOutput` |
| `%pause %<pane>` / `%continue %<pane>` | flow control state | `Event::Pause` / `Event::Continue` |
| `%window-add @<id>` | a window was linked | `Event::WindowAdd` |
| `%window-close @<id>` | a window was unlinked/closed | `Event::WindowClose` |
| `%window-renamed @<id> <name>` | window renamed | `Event::WindowRenamed` |
| `%window-pane-changed @<id> %<pane>` | active pane changed | `Event::WindowPaneChanged` |
| `%unlinked-window-add/close/renamed/pane-changed` | same, for windows not linked to the attached session | `Event::UnlinkedWindow*` |
| `%layout-change @<id> <layout> <visible-layout> [flags]` | layout changed | `Event::LayoutChange` |
| `%session-changed $<id> <name>` | this client switched session | `Event::SessionChanged` |
| `%session-renamed $<id> <name>` | session renamed | `Event::SessionRenamed` |
| `%session-window-changed $<id> @<id>` | current window changed | `Event::SessionWindowChanged` |
| `%sessions-changed` | the session set changed (no details) | `Event::SessionsChanged` |
| `%client-session-changed <client> $<id> <name>` | another client attached | `Event::ClientSessionChanged` |
| `%client-detached <client>` | another client detached | `Event::ClientDetached` |
| `%pane-mode-changed %<pane> [mode]` | entered/left a tmux mode | `Event::PaneModeChanged` |
| `%paste-buffer-changed <name>` / `%paste-buffer-deleted <name>` | paste buffers | `Event::PasteBufferChanged` / `Deleted` |
| `%subscription-changed <name> …` | a `refresh-client -B` subscription fired | `Event::SubscriptionChanged` |
| `%message <text>` | message forwarded by tmux | `Event::Message` |
| `%config-error <text>` | configuration error | `Event::ConfigError` |
| `%exit [reason]` | control mode ended | `Event::Exit` |

* **[ours]** Unknown verbs are surfaced as `Event::Unknown` and never treated as
  errors: a newer tmux must not be able to break the client.
* **[ours]** Malformed notifications produce `Event::Desync`, because the only
  honest response to "I do not understand the stream" is to resynchronise.
* **[observed]** `%sessions-changed` carries no payload, so it forces a resync
  instead of a local guess.
* **[observed, tmux 3.4]** `%pane-mode-changed` is sent **without** a mode name on
  both entering and leaving copy mode (fixtures
  `notifications.13_enter_copy_mode.bin` / `14_leave_copy_mode.bin`). The absence
  of a name therefore means "the mode changed", not "left the mode"; the model
  toggles. Versions that add the name on enter still work with the same rule.
* **[ours]** `%pause` / `%continue` are surfaced as
  `SessionEvent::OutputPaused` / `OutputResumed`; the GUI answers a pause with
  `set_pane_output(pane, Continue)` (encoded as `refresh-client -A '%<pane>:continue'`).
  `%extended-output` is delivered as ordinary output; its age is informational and
  only appears if a client enables `pause-after`, which we do not.
* **[ours]** `%message`, `%config-error` and lines that are not `%`-prefixed at all
  (`Event::Raw`) are surfaced instead of dropped. A raw line means the stream is not
  control mode any more; it is exactly the signal a shell leak would produce.

## 3. `%output` escaping

* **[documented]** Bytes that are not safe to print are written as a backslash
  followed by three octal digits, for example `\033` for ESC and `\012` for LF.
* **[observed]** Bytes `>= 0x80` are passed through raw, so the decoded payload is
  **not** necessarily valid UTF-8. The parser therefore never converts pane output
  to `String`; it hands `Vec<u8>` to the terminal emulator.
* **[ours]** Decoding accepts `\ooo` and `\\`; anything else is a desync rather
  than a guess.

## 4. Connection entries

### 4.1 Direct attach (`-C`)

`tmux -C attach -t <session>` over a pipe, or
`ssh -T <host> tmux -C attach -t <session>`.

* **[documented]** `-C` and `-CC` share the same core protocol; `-CC` adds
  terminal-mode handling and the DCS takeover sequence.
* **[ours]** The local case is spawned with an argv vector, never through a shell.
  The SSH case needs a *shell-quoted* remote command, which is a different encoding
  layer from tmux command quoting (see [`encoding.md`](encoding.md)).

### 4.2 Takeover inside a shell (`-CC`)

* **[documented]** tmux writes the DCS introducer `ESC P 1000 p`, then speaks
  control mode on the same terminal, and terminates with `%exit` followed by the
  string terminator `ESC \`.
* **[observed]** The introducer and the terminator can be split across reads.
* **[ours]** A dedicated switch layer sits in front of the terminal emulator. It
  buffers only the longest possible DCS prefix, routes bytes in control mode to the
  protocol parser, and leaves control mode as soon as `%exit` plus the terminator
  has been seen.
* **[ours — the important one]** When control mode ends, the command queue is
  stopped *before* the switch hands the terminal back. A late command written after
  that point would be read by the user's shell; `poc leak` and
  `poc leak --inject-bug` prove both halves of that statement.

## 4.3 Barrier rules **[ours]**

The server answers commands the client never sent: while attaching it issues its
own and reports them on the same stream. Two rules keep that from being confused
with a client request:

* responses are matched positionally, and anything that completes while nothing was
  in flight is surfaced as *unsolicited*;
* the first command a client sends is a marker (`display-message -p <nonce>`), and
  while that marker is outstanding a response block is only matched to it **if its
  first line is the marker**. Anything else is unsolicited and the marker request
  keeps its place in flight.

The second rule is what makes the ordering irrelevant: it does not matter whether
the server's own answers arrive before or after the client's first write. When a
snapshot fails, the barrier is cleared explicitly: otherwise every later command
would be held behind a response that will never arrive.

## 5. Resource limits **[ours]**

Untrusted input must not be able to grow a buffer without bound:

| Limit | Default | Behaviour on breach |
|---|---|---|
| `max_line_bytes` | 1 MiB | drop the line (partial *or* complete), `Desync::LineTooLong`, resync at the next newline/block end |
| `max_response_bytes` | 32 MiB | drop the rest of the block, `Desync::ResponseTooLarge`, close the block with `%end`; also bounds how long a `Skipping` state may last |
| `max_response_lines` | 4096 | as above; also the budget for `capture-pane` (`max_response_lines - pane_height` history lines) |
| `max_switch_buffer_bytes` | 64 KiB | release the buffered bytes to the terminal |
| `max_encoded_command_bytes` | 1 MiB | chunk `send-keys` runs; refuse a single item that cannot fit |
| `max_in_flight_commands` | 1 | hold the command in the pending queue |
| `max_pending_commands` | 4096 | refuse to schedule |

A desync inside a response block marks that response `truncated`; the synchroniser
turns a truncated *snapshot* response into a hard error rather than applying it.
Desynchronisation is always reported, never silently repaired: the model is marked
untrustworthy and the caller must resynchronise before presenting state as accurate.

### 5.1 Flow control, client size and resynchronisation **[ours]**

* `refresh-client -A '%<pane>:<on|off|pause|continue>'` is how a control client
  answers `%pause`/`%continue` and can throttle a pane.
* `refresh-client -C <width>x<height>` (or `@<window>:<width>x<height>`) announces
  the control client's size. Without it tmux keeps sizing the window from the
  default it gave the pipe, and the GUI cannot drive pane geometry.
* `resync()` re-runs the snapshot sequence; `%sessions-changed` and a desync
  request it automatically (bounded), and a pane discovered after the snapshot gets
  its own capture. See [`state-sync.md`](state-sync.md) §4.1.
* A session created with `new_with_takeover` routes a `-CC` byte stream through
  the DCS switch, so the shell bytes before/after control mode become
  `SessionEvent::TerminalOutput` and the queue is closed on `%exit` / exit of the
  switch.

## 6. Layout strings

```
layout := checksum ',' node
node   := W 'x' H ',' X ',' Y ( ',' pane | '[' node (',' node)* ']'
                                        | '{' node (',' node)* '}' )
```

* **[ours]** A split node may have **more than two** children; the GUI must not
  assume binary splits.
* **[observed, and the opposite of the obvious guess]** `{` denotes a left/right
  arrangement (children side by side) and `[` a top/bottom one (stacked). Recorded
  from tmux 3.4:

  ```text
  split-window -h -> ...{40x24,0,0,1,39x24,41,0,2}      x differs
  split-window -v -> ...[39x12,41,0,2,39x11,41,13,3]    y differs
  ```

  `crates/tmux-protocol/tests/fixtures.rs` re-asserts this against the captures and
  also checks that the geometry agrees with the orientation, so a release that
  changes it fails the build before the GUI can mis-render.
* **[ours]** The checksum is parsed and preserved but not verified: the client
  never edits layouts locally, so an integrity check would only ever reject data
  the server just sent.

## 7. Open questions tracked here

* Whether a raw newline can be embedded in a general tmux command argument (needed
  for a single `set-buffer`). `send_keys` avoids the question for text input by
  splitting literal text into `-l` runs and `Enter`; other command arguments still
  refuse newlines instead of guessing — see [`encoding.md`](encoding.md).
* Which tmux version first sends a mode name on `%pane-mode-changed`; 3.4 does not
  (fixtures 13/14), and the parser accepts both forms.
* Whether `%extended-output`'s age is in milliseconds on every supported version.