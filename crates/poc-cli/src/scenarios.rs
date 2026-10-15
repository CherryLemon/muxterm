//! The two scenarios the PoC must prove before any UI work starts.
//!
//! 1. [`reconnect`] — disconnect while a full-screen program keeps running, then
//!    reattach and show that the restored screen is *current* (not stale) and that
//!    live output continues in order afterwards.
//! 2. [`leak`] — run `tmux -CC` inside a real shell on a real PTY, leave control
//!    mode, and show that no control command ever reached the shell.  The check is
//!    falsifiable: `--inject-bug` deliberately writes a late command and the same
//!    detector must catch it.
//!
//! Together with the protocol tests they cover the three things the plan requires
//! of phase one: a reliable channel, correct restoration, and no leakage.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use tmux_client::session::{ControlSession, SessionEvent};
use tmux_client::sync::SyncStats;
use tmux_client::transport::Target;
use tmux_client::RemoteModel;
use tmux_protocol::ids::PaneId;
use tmux_protocol::limits::Limits;
use tmux_protocol::parser::Parser;
use tmux_protocol::switch::{SwitchAction, TakeoverSwitch};

use crate::tmux::Tmux;

/// One assertion with its evidence.
#[derive(Debug, Clone)]
pub struct Check {
    /// What was asserted.
    pub name: String,
    /// Whether it held.
    pub passed: bool,
    /// Evidence, always filled in (a passing check still reports its numbers).
    pub detail: String,
}

/// Collected assertions.
#[derive(Debug, Default, Clone)]
pub struct Report {
    /// All checks, in order.
    pub checks: Vec<Check>,
}

impl Report {
    /// Records an assertion.
    pub fn check(&mut self, name: impl Into<String>, passed: bool, detail: impl Into<String>) {
        self.checks.push(Check {
            name: name.into(),
            passed,
            detail: detail.into(),
        });
    }

    /// True when every check passed.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.checks.iter().all(|check| check.passed)
    }

    /// Prints a human-readable summary and returns the exit code.
    #[must_use]
    pub fn print(&self) -> i32 {
        for check in &self.checks {
            println!(
                "  [{}] {} — {}",
                if check.passed { "PASS" } else { "FAIL" },
                check.name,
                check.detail
            );
        }
        if self.passed() {
            println!("RESULT: PASS ({} checks)", self.checks.len());
            0
        } else {
            let failed = self.checks.iter().filter(|c| !c.passed).count();
            println!(
                "RESULT: FAIL ({failed}/{} checks failed)",
                self.checks.len()
            );
            1
        }
    }
}

/// Name of the session the scenarios create.
pub const POC_SESSION: &str = "poc";

/// A full-screen program: alternate screen, a ticking counter and a static marker.
///
/// The tick makes a stale restore detectable (a frozen screen shows an old number)
/// and a double-applied restore detectable as well (numbers would jump ahead).
#[must_use]
pub const fn fullscreen_script() -> &'static str {
    "printf '\\033[?1049h\\033[2J\\033[H'; i=0; while :; do i=$((i+1)); \
     printf '\\033[H\\033[2KFULLSCREEN tick %s\\033[3;1HSTATIC-MARKER-LINE\\033[4;1H' \"$i\"; \
     sleep 1; done"
}

/// A pane that produces a steady stream of output.
#[must_use]
pub const fn generator_script() -> &'static str {
    "i=0; while :; do i=$((i+1)); printf 'gen tick %s\\n' \"$i\"; sleep 0.2; done"
}

/// Creates the standard PoC session: three panes — an interactive shell, a
/// full-screen program and a noisy generator — in a tiled layout.
///
/// # Errors
///
/// Returns the failing tmux invocation.
pub fn create_poc_session(tmux: &Tmux, session: &str) -> Result<(), String> {
    tmux.run(&["new-session", "-d", "-s", session, "-x", "100", "-y", "30"])
        .map_err(|err| err.to_string())?;
    tmux.run(&["rename-window", "-t", &format!("{session}:0"), "shell"])
        .map_err(|err| err.to_string())?;
    // pane 0 exists; -h creates pane 1 beside it, -v splits pane 1 into 1 and 2.
    tmux.run(&["split-window", "-h", "-t", &format!("{session}:0")])
        .map_err(|err| err.to_string())?;
    tmux.run(&["split-window", "-v", "-t", &format!("{session}:0.1")])
        .map_err(|err| err.to_string())?;
    tmux.run(&["select-layout", "-t", &format!("{session}:0"), "tiled"])
        .map_err(|err| err.to_string())?;
    let panes = tmux
        .run(&[
            "list-panes",
            "-t",
            &format!("{session}:0"),
            "-F",
            "#{pane_index}",
        ])
        .map_err(|err| err.to_string())?;
    if panes.lines().count() != 3 {
        return Err(format!(
            "expected 3 panes in {session}:0, got {:?}",
            panes.trim()
        ));
    }
    // pane 0 keeps an interactive login-less shell for input tests
    tmux.run(&[
        "respawn-pane",
        "-k",
        "-t",
        &format!("{session}:0.1"),
        fullscreen_script(),
    ])
    .map_err(|err| err.to_string())?;
    tmux.run(&[
        "respawn-pane",
        "-k",
        "-t",
        &format!("{session}:0.2"),
        generator_script(),
    ])
    .map_err(|err| err.to_string())?;
    Ok(())
}

/// The pane command that produces the rich content used by the core gate.
///
/// The script lives in the repository (`tools/poc/rich-content.sh`), which is
/// mounted at `/work` inside the sandbox, so nothing has to be quoted through a
/// terminal.
#[must_use]
pub const fn rich_content_command() -> &'static str {
    "sh /work/tools/poc/rich-content.sh"
}

/// Runs the rich-content script in a pane and waits until it has finished.
///
/// Waiting for the marker matters: without it the snapshot would race the program
/// and a text mismatch would say nothing about the implementation.
///
/// # Errors
///
/// Returns a description when the pane never reaches the marker.
pub fn prepare_rich_content(tmux: &Tmux, session: &str, pane: &str) -> Result<(), String> {
    let target = format!("{session}:0.{pane}");
    tmux.run(&["send-keys", "-t", &target, rich_content_command(), "Enter"])
        .map_err(|err| err.to_string())?;

    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        let screen = tmux
            .run(&["capture-pane", "-p", "-t", &target])
            .map_err(|err| err.to_string())?;
        if screen.contains(RICH_CONTENT_MARKER) {
            // Let the shell finish writing its prompt so the snapshot is stable.
            std::thread::sleep(Duration::from_millis(300));
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Err(format!("pane {target} never printed {RICH_CONTENT_MARKER}"))
}

/// Marker the rich-content script ends with.
pub const RICH_CONTENT_MARKER: &str = "CORE-GATE-READY";

/// Everything one snapshot restored, per pane.
#[derive(Debug, Default, Clone)]
pub struct RestoredState {
    /// Captured lines per pane.
    pub screens: BTreeMap<PaneId, Vec<Vec<u8>>>,
    /// Snapshot statistics.
    pub stats: SyncStats,
    /// Bytes dropped because they preceded their pane's watermark.
    pub dropped_bytes: u64,
}

fn attach(tmux: &Tmux, session: &str) -> Result<ControlSession, String> {
    let target = Target::Local {
        socket: Some(tmux.socket().to_string()),
        session: session.to_string(),
    };
    ControlSession::attach(&target, Limits::interactive()).map_err(|err| err.to_string())
}

fn run_snapshot(session: &mut ControlSession, timeout: Duration) -> Result<RestoredState, String> {
    let events = session
        .pump_until_synced(timeout)
        .map_err(|err| err.to_string())?;
    let mut state = RestoredState::default();
    for event in events {
        match event {
            SessionEvent::PaneSnapshot { pane, lines } => {
                state.screens.insert(pane, lines);
            }
            SessionEvent::SnapshotComplete(stats) => state.stats = stats,
            SessionEvent::Desync { reason } => {
                return Err(format!("protocol desynchronised during snapshot: {reason}"))
            }
            SessionEvent::Exited { reason } => {
                return Err(format!("session exited during snapshot: {reason:?}"))
            }
            _ => {}
        }
    }
    state.dropped_bytes = session.model().dropped_output_bytes();
    Ok(state)
}

/// Extracts the number after `prefix` from a screen dump.
fn find_tick(lines: &[Vec<u8>], prefix: &str) -> Option<u64> {
    for line in lines {
        let text = String::from_utf8_lossy(line);
        let mut rest = text.as_ref();
        while let Some(index) = rest.find(prefix) {
            let after = &rest[index + prefix.len()..];
            let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(value) = digits.parse::<u64>() {
                return Some(value);
            }
            rest = after;
        }
    }
    None
}

/// Finds the pane running the full-screen program by looking for its marker.
fn fullscreen_pane(state: &RestoredState) -> Option<(PaneId, u64)> {
    state
        .screens
        .iter()
        .filter_map(|(pane, lines)| find_tick(lines, "FULLSCREEN tick ").map(|tick| (*pane, tick)))
        .next()
}

/// The reconnect scenario.
///
/// # Errors
///
/// Returns a description when a step cannot be performed; individual assertions are
/// recorded in the returned [`Report`] instead.
pub fn reconnect(tmux: &Tmux, session: &str, attach_seconds: u64) -> Result<Report, String> {
    let mut report = Report::default();

    // --- first attachment -------------------------------------------------
    let started = Instant::now();
    let mut first = attach(tmux, session)?;
    let first_state = run_snapshot(&mut first, Duration::from_secs(20))?;
    report.check(
        "first attach produced a snapshot",
        first_state.stats.panes > 0,
        format!(
            "sessions={} windows={} panes={} captures={} in {:?}",
            first_state.stats.sessions,
            first_state.stats.windows,
            first_state.stats.panes,
            first_state.stats.captures,
            started.elapsed()
        ),
    );
    report.check(
        "model is marked synced",
        first.model().consistency().is_synced(),
        format!("consistency={:?}", first.model().consistency()),
    );

    let Some((fs_pane, first_tick)) = fullscreen_pane(&first_state) else {
        return Err("the full-screen pane was not found in the first snapshot".to_string());
    };
    report.check(
        "full-screen pane restored with its content",
        first_tick > 0,
        format!("pane={fs_pane} tick={first_tick}"),
    );

    // layout must describe all three panes
    let window_panes = first
        .model()
        .windows()
        .values()
        .map(|window| window.panes.len())
        .max()
        .unwrap_or(0);
    report.check(
        "layout parsed with every pane",
        window_panes == 3,
        format!("panes in the widest window = {window_panes}"),
    );

    // --- external layout change must be visible ---------------------------
    tmux.run(&["split-window", "-v", "-t", &format!("{session}:0")])
        .map_err(|err| err.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut saw_four_panes = false;
    while Instant::now() < deadline && !saw_four_panes {
        let _ = first
            .pump(Duration::from_millis(200))
            .map_err(|err| err.to_string())?;
        saw_four_panes = first
            .model()
            .windows()
            .values()
            .any(|window| window.panes.len() >= 4);
    }
    report.check(
        "an external split is reflected in the model",
        saw_four_panes,
        "waited up to 5s for %layout-change".to_string(),
    );
    tmux.run(&["kill-pane", "-t", &format!("{session}:0.3")])
        .map_err(|err| err.to_string())?;
    let _ = first.pump(Duration::from_millis(300));

    // --- detach and let the remote state move on --------------------------
    let detach_tick = drain_ticks(&mut first, fs_pane, Duration::from_millis(600))?;
    first.close();
    report.check(
        "detaching closed the session without killing the server",
        first.is_closed() && tmux.is_running(),
        "session closed locally, server still answers".to_string(),
    );

    std::thread::sleep(Duration::from_secs(attach_seconds.max(2)));

    // --- reattach ---------------------------------------------------------
    let reattach_started = Instant::now();
    let mut second = attach(tmux, session)?;
    let second_state = run_snapshot(&mut second, Duration::from_secs(20))?;
    let Some((_, second_tick)) = fullscreen_pane(&second_state) else {
        return Err("the full-screen pane was not found after reattaching".to_string());
    };
    report.check(
        "reattach restored the full-screen pane",
        second_tick > 0,
        format!("tick={second_tick} in {:?}", reattach_started.elapsed()),
    );
    report.check(
        "restored screen is current, not stale",
        second_tick > detach_tick && second_tick >= first_tick,
        format!("first={first_tick} before-detach={detach_tick} restored={second_tick}"),
    );
    report.check(
        "restored screen was not over-applied",
        second_tick <= detach_tick + attach_seconds + 4,
        format!(
            "restored={second_tick}, detached for ~{}s",
            attach_seconds.max(2)
        ),
    );
    report.check(
        "new connection has a new epoch",
        second.epoch() > first.epoch(),
        format!("{} -> {}", first.epoch(), second.epoch()),
    );
    report.check(
        "reconnected model desynchronised nothing",
        second.model().consistency().is_synced(),
        format!("consistency={:?}", second.model().consistency()),
    );

    // --- live output continues after the restore --------------------------
    let live_deadline = Instant::now() + Duration::from_secs(4);
    let mut ticks = Vec::new();
    while Instant::now() < live_deadline {
        for event in second
            .pump(Duration::from_millis(200))
            .map_err(|err| err.to_string())?
        {
            if let SessionEvent::PaneOutput { pane, data } = event {
                if pane == fs_pane {
                    if let Some(tick) = find_tick(&[data], "FULLSCREEN tick ") {
                        ticks.push(tick);
                    }
                }
            }
        }
    }
    let advanced = ticks.iter().any(|tick| *tick > second_tick);
    report.check(
        "live output continues after the restore",
        advanced,
        format!(
            "ticks seen after restore: {:?} (restored at {second_tick})",
            ticks.iter().take(8).collect::<Vec<_>>()
        ),
    );
    report.check(
        "output is ordered and not replayed",
        ticks.windows(2).all(|pair| pair[1] >= pair[0]),
        format!("{} tick events", ticks.len()),
    );

    // --- input still works -------------------------------------------------
    let shell_pane = shell_pane(second.model());
    if let Some(shell_pane) = shell_pane {
        let ids = second
            .send_keys(
                shell_pane,
                &[
                    tmux_protocol::command::KeySpec::Literal(b"echo AFTER-RECONNECT-OK".to_vec()),
                    tmux_protocol::command::KeySpec::Named("Enter"),
                ],
            )
            .map_err(|err| err.to_string())?;
        report.check(
            "typing into a restored pane is accepted",
            !ids.is_empty(),
            format!("{} commands queued for {shell_pane}", ids.len()),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = false;
        while Instant::now() < deadline && !seen {
            for event in second
                .pump(Duration::from_millis(200))
                .map_err(|err| err.to_string())?
            {
                if let SessionEvent::PaneOutput { pane, data } = event {
                    if pane == shell_pane
                        && String::from_utf8_lossy(&data).contains("AFTER-RECONNECT-OK")
                    {
                        seen = true;
                    }
                }
            }
        }
        report.check(
            "the restored pane ran the typed command",
            seen,
            "looked for AFTER-RECONNECT-OK in pane output".to_string(),
        );
    } else {
        report.check("a shell pane is available", false, "not found".to_string());
    }

    second.close();
    Ok(report)
}

fn shell_pane(model: &RemoteModel) -> Option<PaneId> {
    model
        .panes()
        .iter()
        .find(|(_, state)| !state.dead && state.index == 0)
        .map(|(pane, _)| *pane)
}

fn drain_ticks(
    session: &mut ControlSession,
    pane: PaneId,
    window: Duration,
) -> Result<u64, String> {
    let deadline = Instant::now() + window;
    let mut last = 0;
    while Instant::now() < deadline {
        for event in session
            .pump(Duration::from_millis(100))
            .map_err(|err| err.to_string())?
        {
            if let SessionEvent::PaneOutput { pane: source, data } = event {
                if source == pane {
                    if let Some(tick) = find_tick(&[data], "FULLSCREEN tick ") {
                        last = tick;
                    }
                }
            }
        }
    }
    Ok(last)
}

/// Result of the takeover/leak scenario.
#[derive(Debug, Default, Clone)]
pub struct LeakOutcome {
    /// Everything the shell wrote to the terminal while control mode was active
    /// or afterwards.
    pub shell_transcript: String,
    /// True when control mode was actually entered.
    pub entered_control_mode: bool,
    /// True when control mode ended cleanly.
    pub exited_control_mode: bool,
    /// Command responses seen while in control mode.
    pub responses: Vec<String>,
}

impl LeakOutcome {
    /// True when the transcript contains a token that could only come from a
    /// control command reaching the shell.
    fn leaked(&self, token: &str) -> bool {
        self.shell_transcript.contains(token)
    }
}

/// Runs `tmux -CC` inside a real shell on a real PTY and checks for leakage.
///
/// When `inject_bug` is set, one queued command is written *after* control mode
/// ended, which must make the detector fire.  That is what makes this a test
/// rather than a demonstration.
///
/// # Errors
///
/// Returns a description when the PTY or the tmux client cannot be started.
pub fn leak(tmux: &Tmux, session: &str, inject_bug: bool) -> Result<(Report, LeakOutcome), String> {
    let mut report = Report::default();
    let mut outcome = LeakOutcome::default();

    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(portable_pty::PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|err| format!("openpty failed: {err}"))?;

    let mut command = portable_pty::CommandBuilder::new("sh");
    command.env("TERM", "xterm-256color");
    command.env("PS1", "$ ");
    command.env_remove("TMUX");
    let mut child = pair
        .slave
        .spawn_command(command)
        .map_err(|err| format!("could not start the shell: {err}"))?;

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|err| format!("pty reader: {err}"))?;
    let mut writer = pair
        .master
        .take_writer()
        .map_err(|err| format!("pty writer: {err}"))?;

    let (tx, rx): (Sender<Vec<u8>>, Receiver<Vec<u8>>) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        return;
                    }
                }
            }
        }
    });

    let mut switch = TakeoverSwitch::new(Limits::interactive());
    let mut parser = Parser::with_default_limits();
    let mut in_control = false;
    let mut queue: Vec<Vec<u8>> = Vec::new();
    let mut queue_closed = false;

    // Start tmux -CC inside the shell.
    let start_command = format!("tmux -L {} -CC attach -t {}\n", tmux.socket(), session);
    writer
        .write_all(start_command.as_bytes())
        .map_err(|err| format!("pty write: {err}"))?;
    writer.flush().ok();

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut exit_seen = false;
    while Instant::now() < deadline {
        let Ok(chunk) = rx.recv_timeout(Duration::from_millis(200)) else {
            if exit_seen {
                break;
            }
            continue;
        };
        let mut actions = Vec::new();
        switch.feed(&chunk, &mut actions);
        for action in actions {
            match action {
                SwitchAction::Terminal(bytes) => {
                    outcome
                        .shell_transcript
                        .push_str(&String::from_utf8_lossy(&bytes));
                }
                SwitchAction::EnterControlMode => {
                    in_control = true;
                    outcome.entered_control_mode = true;
                    // A distinctive probe command: if it ever reaches the shell,
                    // the shell will report that it cannot find it.
                    queue.push(b"display-message -p LEAK-MARKER-ALPHA\n".to_vec());
                }
                SwitchAction::Control(bytes) => {
                    let mut events = Vec::new();
                    parser.feed(&bytes, &mut events);
                    for event in events {
                        match event {
                            tmux_protocol::Event::ResponseLine(line) => {
                                outcome
                                    .responses
                                    .push(String::from_utf8_lossy(&line).into_owned());
                            }
                            tmux_protocol::Event::Exit { .. } => {
                                exit_seen = true;
                            }
                            _ => {}
                        }
                    }
                }
                SwitchAction::ExitControlMode => {
                    in_control = false;
                    outcome.exited_control_mode = true;
                    // THE RULE: the command queue must be stopped here, before the
                    // shell gets the terminal back.
                    if inject_bug {
                        // Falsification: write a control command after the takeover
                        // ended.  The delay makes the injection unambiguous - tmux
                        // has exited and the shell owns the terminal again - so a
                        // detector that misses it is genuinely broken.
                        let stale = b"display-message -p LEAK-MARKER-BETA\n".to_vec();
                        std::thread::sleep(Duration::from_millis(250));
                        let _ = writer.write_all(&stale);
                        let _ = writer.flush();
                        std::thread::sleep(Duration::from_millis(250));
                    }
                    queue_closed = true;
                    queue.clear();
                }
            }
        }
        // While control mode is active, send queued commands.
        if in_control && !queue_closed {
            while let Some(command) = queue.pop() {
                if writer.write_all(&command).is_err() {
                    break;
                }
                writer.flush().ok();
                std::thread::sleep(Duration::from_millis(150));
            }
            // Ask tmux to end control mode.
            if outcome
                .responses
                .iter()
                .any(|r| r.contains("LEAK-MARKER-ALPHA"))
            {
                let _ = writer.write_all(b"detach-client\n");
                let _ = writer.flush();
            }
        }
    }

    // The shell must be usable again and must never have seen a control command.
    let _ = writer.write_all(b"echo SHELL-AFTER-CONTROL-OK\n");
    let _ = writer.flush();
    let after_deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < after_deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(chunk) => {
                let mut actions = Vec::new();
                switch.feed(&chunk, &mut actions);
                for action in actions {
                    if let SwitchAction::Terminal(bytes) = action {
                        outcome
                            .shell_transcript
                            .push_str(&String::from_utf8_lossy(&bytes));
                    }
                }
                if outcome.shell_transcript.contains("SHELL-AFTER-CONTROL-OK") {
                    // give the shell a moment to flush anything else
                    std::thread::sleep(Duration::from_millis(300));
                    while let Ok(chunk) = rx.recv_timeout(Duration::from_millis(100)) {
                        outcome
                            .shell_transcript
                            .push_str(&String::from_utf8_lossy(&chunk));
                    }
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    report.check(
        "control mode was entered",
        outcome.entered_control_mode,
        "DCS ESC P 1000 p observed on the PTY".to_string(),
    );
    report.check(
        "control mode was left cleanly",
        outcome.exited_control_mode,
        "%exit followed by the string terminator".to_string(),
    );
    report.check(
        "control-mode command was answered",
        outcome
            .responses
            .iter()
            .any(|line| line.contains("LEAK-MARKER-ALPHA")),
        format!("responses: {:?}", outcome.responses),
    );
    let shell_alive = outcome.shell_transcript.contains("SHELL-AFTER-CONTROL-OK");
    report.check(
        "the shell is usable after control mode",
        shell_alive,
        "echo after the takeover was echoed back".to_string(),
    );
    let alpha_leaked = outcome.leaked("LEAK-MARKER-ALPHA");
    let beta_leaked = outcome.leaked("LEAK-MARKER-BETA");

    // The safety property under test, checked in both modes: whatever the client
    // had queued, none of it may reach the shell.
    report.check(
        "no control command reached the shell",
        !alpha_leaked && !beta_leaked,
        format!(
            "transcript scanned for probe markers ({} bytes){}",
            outcome.shell_transcript.len(),
            if inject_bug {
                " - a leak is EXPECTED here: this run injected one"
            } else {
                ""
            }
        ),
    );
    if inject_bug {
        // Falsification: the detector above must not be vacuous, so it has to fire
        // on the leak this run deliberately created.
        report.check(
            "the leak detector fires when a control command is written late",
            beta_leaked,
            "injected bug: a stale command was written 250ms after the exit".to_string(),
        );
    }

    let _ = writer.write_all(b"exit\n");
    let _ = writer.flush();
    let _ = child.kill();
    let _ = child.wait();
    Ok((report, outcome))
}
