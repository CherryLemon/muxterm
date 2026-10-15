//! The `-CC` takeover switch.
//!
//! When the user runs `tmux -CC` *inside* a shell, tmux first writes a DCS
//! introducer to the terminal and then switches to the control-mode protocol;
//! when the session ends it writes `%exit`, a string terminator, and hands the
//! terminal back to the shell.  The switch below sits in front of the terminal
//! emulator and routes bytes accordingly.
//!
//! Two failure modes are explicitly handled because they are the ones that leak
//! control commands into a user's shell:
//!
//! * the introducer is split across reads (for example a read ends with
//!   `ESC P 10`), so the scanner buffers only the longest possible prefix;
//! * the switch back to the shell must happen *before* the client writes another
//!   command.  Callers therefore stop the command queue on
//!   [`SwitchAction::ExitControlMode`]; see `tmux_client::session`.
//!
//! A generic DCS handler must **not** be used here: control mode is a stream, not
//! a string that is collected until its terminator arrives.

use crate::limits::Limits;

/// The DCS introducer tmux uses to take over the terminal (`ESC P 1000 p`).
pub const CONTROL_MODE_DCS: &[u8] = b"\x1bP1000p";

/// The string terminator that ends the DCS.
pub const STRING_TERMINATOR: &[u8] = b"\x1b\\";

/// The line the server writes when control mode ends.
pub const EXIT_LINE: &[u8] = b"%exit\n";

/// Bytes retained to recognise `%exit` split across reads.
const EXIT_PROBE_BYTES: usize = 8;

/// Maximum number of bytes held after `%exit` while waiting for the terminator
/// before the switch gives up and treats the session as abnormally ended.
const MAX_TERMINATOR_WAIT: usize = 64;

/// Which side of the takeover the byte stream is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchMode {
    /// Ordinary terminal bytes: they belong to the local terminal emulator.
    Terminal,
    /// Control-mode protocol bytes.
    Control,
}

/// What the caller must do with a chunk of bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitchAction {
    /// Feed these bytes to the local terminal emulator.
    Terminal(Vec<u8>),
    /// Feed these bytes to the control-mode parser.
    Control(Vec<u8>),
    /// The takeover sequence completed: start the control-mode session now.
    EnterControlMode,
    /// Control mode ended and the shell owns the terminal again.
    ExitControlMode,
}

/// State machine that splits a byte stream between the terminal and the protocol.
#[derive(Debug, Clone)]
pub struct TakeoverSwitch {
    limits: Limits,
    mode: SwitchMode,
    /// Partial DCS introducer held back in terminal mode.
    terminal_pending: Vec<u8>,
    /// Control-mode bytes held back while an exit is being resolved.
    control_pending: Vec<u8>,
    /// Tail of the control bytes already emitted, used to spot `%exit` in pieces.
    control_history: Vec<u8>,
    exit_pending: bool,
    /// Set when the session ended abnormally and the switch had to give up.
    abnormal_exit: bool,
}

impl TakeoverSwitch {
    /// Creates a switch in terminal mode.
    #[must_use]
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            mode: SwitchMode::Terminal,
            terminal_pending: Vec::new(),
            control_pending: Vec::new(),
            control_history: Vec::new(),
            exit_pending: false,
            abnormal_exit: false,
        }
    }

    /// Current mode.
    #[must_use]
    pub const fn mode(&self) -> SwitchMode {
        self.mode
    }

    /// True when control mode ended without a proper terminator.
    #[must_use]
    pub const fn ended_abnormally(&self) -> bool {
        self.abnormal_exit
    }

    /// The client's parser saw `%exit`; finish the takeover as soon as the
    /// terminator arrives.
    pub fn note_protocol_exit(&mut self) {
        if self.mode == SwitchMode::Control {
            self.exit_pending = true;
        }
    }

    /// Forces the switch back to terminal mode (transport died, user killed the
    /// session).  Any bytes held back are handed to the terminal.
    pub fn force_terminal(&mut self, out: &mut Vec<SwitchAction>) {
        if self.mode == SwitchMode::Terminal {
            return;
        }
        self.abnormal_exit = true;
        if !self.control_pending.is_empty() {
            out.push(SwitchAction::Control(std::mem::take(
                &mut self.control_pending,
            )));
        }
        self.control_history.clear();
        self.exit_pending = false;
        self.mode = SwitchMode::Terminal;
        out.push(SwitchAction::ExitControlMode);
    }

    /// Feeds raw bytes and appends the actions the caller must perform.
    pub fn feed(&mut self, input: &[u8], out: &mut Vec<SwitchAction>) {
        match self.mode {
            SwitchMode::Terminal => self.feed_terminal(input, out),
            SwitchMode::Control => self.feed_control(input, out),
        }
    }

    fn feed_terminal(&mut self, input: &[u8], out: &mut Vec<SwitchAction>) {
        if self.terminal_pending.is_empty() && input.is_empty() {
            return;
        }
        let mut work = std::mem::take(&mut self.terminal_pending);
        work.extend_from_slice(input);

        let Some(index) = find(&work, CONTROL_MODE_DCS) else {
            // Keep only a suffix that is genuinely a prefix of the introducer: a
            // fixed look-behind would delay every byte by its length.
            let keep = partial_suffix_len(&work, CONTROL_MODE_DCS);
            let split = work.len() - keep;
            if split > 0 {
                out.push(SwitchAction::Terminal(work[..split].to_vec()));
            }
            self.terminal_pending = work[split..].to_vec();
            self.enforce_terminal_bound(out);
            return;
        };

        if index > 0 {
            out.push(SwitchAction::Terminal(work[..index].to_vec()));
        }
        self.mode = SwitchMode::Control;
        out.push(SwitchAction::EnterControlMode);
        let rest = work[index + CONTROL_MODE_DCS.len()..].to_vec();
        if !rest.is_empty() {
            self.feed_control(&rest, out);
        }
    }

    fn enforce_terminal_bound(&mut self, out: &mut Vec<SwitchAction>) {
        if self.terminal_pending.len() <= self.limits.max_switch_buffer_bytes {
            return;
        }
        // A prefix that never completes is not a takeover; hand it to the terminal.
        out.push(SwitchAction::Terminal(std::mem::take(
            &mut self.terminal_pending,
        )));
    }

    fn feed_control(&mut self, input: &[u8], out: &mut Vec<SwitchAction>) {
        self.control_pending.extend_from_slice(input);
        self.update_exit_pending();

        while let Some(index) = find(&self.control_pending, STRING_TERMINATOR) {
            if self.exit_pending {
                if index > 0 {
                    out.push(SwitchAction::Control(
                        self.control_pending[..index].to_vec(),
                    ));
                }
                let rest = self.control_pending[index + STRING_TERMINATOR.len()..].to_vec();
                self.control_pending.clear();
                self.control_history.clear();
                self.exit_pending = false;
                self.mode = SwitchMode::Terminal;
                out.push(SwitchAction::ExitControlMode);
                if !rest.is_empty() {
                    self.feed_terminal(&rest, out);
                }
                return;
            }
            // A terminator that does not follow `%exit` is just data (it cannot
            // appear inside an `%output` payload, which is octal-escaped, but a
            // defensive pass-through is safer than dropping bytes).
            let end = index + STRING_TERMINATOR.len();
            out.push(SwitchAction::Control(self.control_pending[..end].to_vec()));
            self.control_pending.drain(..end);
        }

        // Nothing to terminate with: release everything except a suffix that could
        // still turn into the terminator (the only such suffix is a trailing ESC).
        let keep = partial_suffix_len(&self.control_pending, STRING_TERMINATOR);
        let split = self.control_pending.len().saturating_sub(keep);
        if split > 0 {
            let released = self.control_pending[..split].to_vec();
            self.control_pending.drain(..split);
            self.remember_history(&released);
            out.push(SwitchAction::Control(released));
        }
        if self.exit_pending && self.control_pending.len() > MAX_TERMINATOR_WAIT {
            // The session ended without a terminator; do not hold bytes forever.
            let released = std::mem::take(&mut self.control_pending);
            out.push(SwitchAction::Control(released));
        }
        self.update_exit_pending();
    }

    fn remember_history(&mut self, released: &[u8]) {
        if released.is_empty() {
            return;
        }
        self.control_history.extend_from_slice(released);
        let excess = self.control_history.len().saturating_sub(EXIT_PROBE_BYTES);
        if excess > 0 {
            self.control_history.drain(..excess);
        }
    }

    /// Recognises `%exit` even when it arrived split across reads.
    fn update_exit_pending(&mut self) {
        if self.exit_pending {
            return;
        }
        let mut probe = Vec::with_capacity(self.control_history.len() + self.control_pending.len());
        probe.extend_from_slice(&self.control_history);
        probe.extend_from_slice(&self.control_pending);
        if find(&probe, EXIT_LINE).is_some() || find(&probe, b"%exit\r\n").is_some() {
            self.exit_pending = true;
        }
    }
}

/// Length of the longest **proper** suffix of `haystack` that is a prefix of
/// `needle`.
///
/// This is what makes split-sequence handling cheap: bytes are held back only when
/// they could actually begin the sequence we are looking for, so ordinary output is
/// never delayed.
fn partial_suffix_len(haystack: &[u8], needle: &[u8]) -> usize {
    let max = needle.len().saturating_sub(1).min(haystack.len());
    for len in (1..=max).rev() {
        if haystack.ends_with(&needle[..len]) {
            return len;
        }
    }
    0
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&[u8]]) -> (Vec<SwitchAction>, TakeoverSwitch) {
        let mut switch = TakeoverSwitch::new(Limits::default());
        let mut out = Vec::new();
        for chunk in chunks {
            switch.feed(chunk, &mut out);
        }
        (out, switch)
    }

    fn flatten(actions: &[SwitchAction]) -> String {
        // Adjacent same-kind chunks are merged: how many actions a read is split
        // into is an implementation detail, which side the bytes go to is not.
        let mut text = String::new();
        let mut index = 0;
        while index < actions.len() {
            match &actions[index] {
                SwitchAction::Terminal(_) | SwitchAction::Control(_) => {
                    let is_terminal = matches!(actions[index], SwitchAction::Terminal(_));
                    let mut merged = Vec::new();
                    while index < actions.len() {
                        match &actions[index] {
                            SwitchAction::Terminal(bytes) if is_terminal => {
                                merged.extend_from_slice(bytes);
                                index += 1;
                            }
                            SwitchAction::Control(bytes) if !is_terminal => {
                                merged.extend_from_slice(bytes);
                                index += 1;
                            }
                            _ => break,
                        }
                    }
                    text.push(if is_terminal { 'T' } else { 'C' });
                    text.push('<');
                    text.push_str(&String::from_utf8_lossy(&merged));
                    text.push('>');
                }
                SwitchAction::EnterControlMode => {
                    text.push_str("ENTER");
                    index += 1;
                }
                SwitchAction::ExitControlMode => {
                    text.push_str("EXIT");
                    index += 1;
                }
            }
        }
        text
    }

    #[test]
    fn plain_terminal_output_passes_through() {
        let (actions, switch) = run(&[b"$ echo hi\r\nhi\r\n"]);
        assert_eq!(switch.mode(), SwitchMode::Terminal);
        assert_eq!(flatten(&actions), "T<$ echo hi\r\nhi\r\n>");
    }

    #[test]
    fn detects_the_takeover_sequence() {
        let (actions, switch) = run(&[b"$ tmux -CC\r\n\x1bP1000p%begin 1 1 0\n"]);
        assert_eq!(switch.mode(), SwitchMode::Control);
        assert_eq!(flatten(&actions), "T<$ tmux -CC\r\n>ENTERC<%begin 1 1 0\n>");
    }

    #[test]
    fn detects_a_takeover_split_across_reads() {
        let (actions, switch) = run(&[b"$ tmux -CC\r\n\x1bP10", b"00p%window-add @1\n"]);
        assert_eq!(switch.mode(), SwitchMode::Control);
        assert_eq!(
            flatten(&actions),
            "T<$ tmux -CC\r\n>ENTERC<%window-add @1\n>"
        );
    }

    #[test]
    fn byte_at_a_time_takeover() {
        let mut switch = TakeoverSwitch::new(Limits::default());
        let mut out = Vec::new();
        for byte in b"x\x1bP1000p%exit\n\x1b\\y" {
            switch.feed(&[*byte], &mut out);
        }
        assert_eq!(switch.mode(), SwitchMode::Terminal);
        assert_eq!(flatten(&out), "T<x>ENTERC<%exit\n>EXITT<y>");
    }

    #[test]
    fn exit_returns_the_terminal_when_the_terminator_arrives() {
        let (actions, switch) = run(&[b"\x1bP1000p%output %0 hi\n%exit\n\x1b\\$ "]);
        assert_eq!(switch.mode(), SwitchMode::Terminal);
        assert_eq!(flatten(&actions), "ENTERC<%output %0 hi\n%exit\n>EXITT<$ >");
    }

    #[test]
    fn exit_split_between_line_and_terminator() {
        let (actions, switch) = run(&[b"\x1bP1000p%exit\n", b"\x1b", b"\\$ "]);
        assert_eq!(switch.mode(), SwitchMode::Terminal);
        assert_eq!(flatten(&actions), "ENTERC<%exit\n>EXITT<$ >");
    }

    #[test]
    fn a_stray_terminator_inside_control_mode_is_data() {
        let (actions, switch) = run(&[b"\x1bP1000p%output %0 a\x1b\\b\n"]);
        assert_eq!(switch.mode(), SwitchMode::Control);
        assert_eq!(flatten(&actions), "ENTERC<%output %0 a\x1b\\b\n>");
    }

    #[test]
    fn protocol_exit_notification_also_closes_the_takeover() {
        let mut switch = TakeoverSwitch::new(Limits::default());
        let mut out = Vec::new();
        switch.feed(b"\x1bP1000p%exit\n", &mut out);
        switch.note_protocol_exit();
        switch.feed(b"\x1b\\$ ", &mut out);
        assert_eq!(switch.mode(), SwitchMode::Terminal);
        assert_eq!(flatten(&out), "ENTERC<%exit\n>EXITT<$ >");
    }

    #[test]
    fn forced_exit_handles_a_dead_control_client() {
        let mut switch = TakeoverSwitch::new(Limits::default());
        let mut out = Vec::new();
        switch.feed(b"\x1bP1000p%output %0 partial", &mut out);
        // Everything before the disconnect was already delivered...
        assert_eq!(flatten(&out), "ENTERC<%output %0 partial>");
        // ...and a forced exit must not swallow or duplicate anything.
        out.clear();
        switch.force_terminal(&mut out);
        assert_eq!(switch.mode(), SwitchMode::Terminal);
        assert!(switch.ended_abnormally());
        assert_eq!(flatten(&out), "EXIT");
    }

    #[test]
    fn incomplete_prefixes_never_escape_to_the_terminal() {
        // ESC P 1 0 0 0 without the final 'p' must not be treated as takeover.
        let (actions, switch) = run(&[b"\x1bP1000"]);
        assert_eq!(switch.mode(), SwitchMode::Terminal);
        assert_eq!(flatten(&actions), "");
        let (actions, switch) = run(&[b"\x1bP1000", b"x"]);
        assert_eq!(switch.mode(), SwitchMode::Terminal);
        assert_eq!(flatten(&actions), "T<\x1bP1000x>");
    }

    #[test]
    fn switching_back_reparses_the_remainder_as_terminal_bytes() {
        let (actions, switch) =
            run(&[b"\x1bP1000p%exit\n\x1b\\\x1bP1000p%window-add @2\n%exit\n\x1b\\"]);
        assert_eq!(switch.mode(), SwitchMode::Terminal);
        assert_eq!(
            flatten(&actions),
            "ENTERC<%exit\n>EXITENTERC<%window-add @2\n%exit\n>EXIT"
        );
    }

    #[test]
    fn only_real_prefixes_are_withheld() {
        assert_eq!(partial_suffix_len(b"abc", b"\x1bP1000p"), 0);
        assert_eq!(partial_suffix_len(b"abc\x1b", b"\x1bP1000p"), 1);
        assert_eq!(partial_suffix_len(b"abc\x1bP", b"\x1bP1000p"), 2);
        assert_eq!(partial_suffix_len(b"\x1bP1000", b"\x1bP1000p"), 6);
        // a full match is not a *proper* suffix: it is handled by find()
        assert_eq!(partial_suffix_len(b"\x1bP1000p", b"\x1bP1000p"), 0);
    }

    #[test]
    fn terminal_buffer_is_bounded() {
        let limits = Limits {
            max_switch_buffer_bytes: 16,
            ..Limits::default()
        };
        let mut switch = TakeoverSwitch::new(limits);
        let mut out = Vec::new();
        switch.feed(b"\x1bP", &mut out);
        switch.feed(&[b'x'; 64], &mut out);
        assert!(
            out.iter()
                .any(|a| matches!(a, SwitchAction::Terminal(b) if b.len() > 16)),
            "an unbounded prefix must be released, not buffered forever"
        );
    }
}
