//! The terminal-core capability gate, against a **real** tmux server.
//!
//! `crates/term-core/tests/gate.rs` proves the five properties with synthetic data.
//! This scenario proves the one thing synthetic data cannot: that a screen rebuilt
//! from a tmux snapshot agrees with what tmux itself says the pane contains.
//!
//! For every pane of the session:
//!
//! 1. restore the snapshot into a fresh core;
//! 2. compare the resulting text, line by line, with `tmux capture-pane -p`
//!    (a second, independent query of the same pane);
//! 3. compare the cursor, the alternate-screen flag and the history size with the
//!    values tmux reports;
//! 4. feed live output for a moment and show that the restored screen keeps up.
//!
//! Any disagreement is reported with the line numbers, because "the screen looks
//! right" is not evidence.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use term_core::{AlacrittyCore, TerminalCore};
use tmux_client::session::{ControlSession, SessionEvent};
use tmux_client::transport::Target;
use tmux_protocol::ids::PaneId;
use tmux_protocol::limits::Limits;

use crate::pane_modes;
use crate::scenarios::Report;
use crate::tmux::Tmux;

/// One pane's restored screen and the evidence collected for it.
#[derive(Debug, Clone)]
pub struct PaneEvidence {
    pane: PaneId,
    columns: u16,
    rows: u16,
    /// Lines our emulator shows.
    ours: Vec<String>,
    /// Lines tmux reports for the same pane.
    theirs: Vec<String>,
    /// First differing line, if any.
    first_difference: Option<(usize, String, String)>,
    cursor_ours: (usize, usize),
    cursor_theirs: (u16, u16),
    alternate_ours: bool,
    alternate_theirs: bool,
    history_ours: usize,
    history_theirs: u32,
    responses_suppressed: usize,
}

impl PaneEvidence {
    fn text_matches(&self) -> bool {
        self.first_difference.is_none()
    }
}

fn attach(tmux: &Tmux, session: &str) -> Result<ControlSession, String> {
    let target = Target::Local {
        socket: Some(tmux.socket().to_string()),
        session: session.to_string(),
    };
    ControlSession::attach(&target, Limits::interactive()).map_err(|err| err.to_string())
}

/// Runs the gate and returns the report plus the evidence for the summary.
///
/// # Errors
///
/// Returns a description when the scenario cannot run at all (no session, tmux
/// command failures).  Individual disagreements are checks in the [`Report`].
pub fn run(tmux: &Tmux, session: &str) -> Result<(Report, Vec<PaneEvidence>), String> {
    let mut report = Report::default();

    let mut session_ctl = attach(tmux, session)?;
    let events = session_ctl
        .pump_until_synced(Duration::from_secs(20))
        .map_err(|err| err.to_string())?;

    let mut captured: BTreeMap<PaneId, Vec<Vec<u8>>> = BTreeMap::new();
    for event in events {
        match event {
            SessionEvent::PaneSnapshot { pane, lines } => {
                captured.insert(pane, lines);
            }
            SessionEvent::Desync { reason } => {
                return Err(format!(
                    "protocol desynchronised during the snapshot: {reason}"
                ));
            }
            _ => {}
        }
    }

    report.check(
        "a snapshot was taken for every pane",
        captured.len() == session_ctl.model().panes().len() && !captured.is_empty(),
        format!(
            "captured {} of {} panes",
            captured.len(),
            session_ctl.model().panes().len()
        ),
    );

    let panes: Vec<PaneId> = captured.keys().copied().collect();
    let mut evidence = Vec::new();

    for pane in panes {
        let Some(state) = session_ctl.model().panes().get(&pane).cloned() else {
            continue;
        };
        let Some(lines) = captured.get(&pane).cloned() else {
            continue;
        };

        let snapshot = pane_modes::snapshot(&state, lines);
        let columns = snapshot.columns;
        let rows = snapshot.rows;

        let mut core = AlacrittyCore::new(
            columns,
            rows,
            usize::try_from(state.history_limit.max(1)).unwrap_or(1000),
        );
        let restore = core
            .restore(&snapshot)
            .map_err(|err| format!("restoring {pane} failed: {err}"))?;

        // Independent second query of the same pane: plain text, no escapes.
        let expected_raw = tmux
            .run(&[
                "capture-pane",
                "-p",
                "-q",
                "-N",
                "-S",
                "-",
                "-E",
                "-",
                "-t",
                &pane.to_string(),
            ])
            .map_err(|err| err.to_string())?;
        let expected: Vec<String> = expected_raw
            .lines()
            .map(|line| line.trim_end().to_string())
            .collect();
        let visible_start = expected.len().saturating_sub(usize::from(rows));
        let expected_visible: Vec<String> = expected[visible_start..].to_vec();

        let ours: Vec<String> = (0..usize::from(rows))
            .map(|row| core.screen().line_text(row).trim_end().to_string())
            .collect();

        let mut first_difference = None;
        for index in 0..usize::from(rows) {
            let a = ours.get(index).cloned().unwrap_or_default();
            let b = expected_visible.get(index).cloned().unwrap_or_default();
            if a != b {
                first_difference = Some((index, a, b));
                break;
            }
        }

        let cursor = core.cursor();
        evidence.push(PaneEvidence {
            pane,
            columns,
            rows,
            ours: ours.clone(),
            theirs: expected_visible.clone(),
            first_difference: first_difference.clone(),
            cursor_ours: (cursor.row, cursor.column),
            cursor_theirs: (state.cursor.1 as u16, state.cursor.0 as u16),
            alternate_ours: core.modes().alternate_screen,
            alternate_theirs: state.alternate_on,
            history_ours: core.history_size(),
            history_theirs: state.history_size,
            responses_suppressed: restore.responses_suppressed,
        });

        let label = format!("{pane} ({columns}x{rows})");
        match &first_difference {
            None => report.check(
                format!("{label}: restored text matches tmux"),
                true,
                format!("{rows} rows compared cell by cell against capture-pane"),
            ),
            Some((index, ours_line, theirs_line)) => report.check(
                format!("{label}: restored text matches tmux"),
                false,
                format!("row {index}: ours={ours_line:?} tmux={theirs_line:?}"),
            ),
        }
        report.check(
            format!("{label}: cursor matches tmux"),
            cursor.row == usize::from(snapshot.cursor.1)
                && cursor.column == usize::from(snapshot.cursor.0),
            format!(
                "ours=({},{}) tmux=({},{})",
                cursor.row, cursor.column, snapshot.cursor.1, snapshot.cursor.0
            ),
        );
        report.check(
            format!("{label}: alternate screen matches tmux"),
            core.modes().alternate_screen == state.alternate_on,
            format!(
                "ours={} tmux={}",
                core.modes().alternate_screen,
                state.alternate_on
            ),
        );
        // The emulator's scrollback accounting and tmux's line counter are not the
        // same number: tmux counts history lines, the core counts storage entries
        // and may hold a spare.  A small delta is acceptable and is *reported*, a
        // large one is not.
        let delta = (core.history_size() as i64 - i64::from(state.history_size)).abs();
        // The emulator counts storage entries, tmux counts history lines; a spare
        // entry is normal, a real divergence is not.
        let tolerance = 2;
        report.check(
            format!("{label}: scrollback size is close to tmux"),
            delta <= tolerance,
            format!(
                "ours={} tmux={} delta={delta}",
                core.history_size(),
                state.history_size
            ),
        );
    }

    // Live continuation: after the restore, the screens must keep up with new pane
    // output.  Every pane gets its own core, and the check requires the busiest pane
    // to be among the ones that changed — an idle shell pane changing nothing is not
    // evidence of anything.
    if !captured.is_empty() {
        let mut cores: BTreeMap<PaneId, AlacrittyCore> = BTreeMap::new();
        for (pane, lines) in &captured {
            let Some(state) = session_ctl.model().panes().get(pane).cloned() else {
                continue;
            };
            let snapshot = pane_modes::snapshot(&state, lines.clone());
            // The core and the snapshot must agree on the geometry, so both come
            // from the snapshot.
            let mut core = AlacrittyCore::new(snapshot.columns, snapshot.rows, 1000);
            if core.restore(&snapshot).is_ok() {
                cores.insert(*pane, core);
            }
        }
        let before: BTreeMap<PaneId, String> = cores
            .iter()
            .map(|(pane, core)| (*pane, core.screen().dump_text()))
            .collect();
        let mut bytes: BTreeMap<PaneId, usize> = BTreeMap::new();

        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline {
            for event in session_ctl
                .pump(Duration::from_millis(200))
                .map_err(|err| err.to_string())?
            {
                if let SessionEvent::PaneOutput { pane, data } = event {
                    *bytes.entry(pane).or_default() += data.len();
                    if let Some(core) = cores.get_mut(&pane) {
                        core.feed(&data);
                    }
                }
            }
        }

        let changed: Vec<PaneId> = cores
            .iter()
            .filter(|(pane, core)| {
                before
                    .get(pane)
                    .is_some_and(|text| *text != core.screen().dump_text())
            })
            .map(|(pane, _)| *pane)
            .collect();
        let busiest = bytes
            .iter()
            .max_by_key(|(_, count)| **count)
            .map(|(pane, count)| (*pane, *count));

        report.check(
            "live output keeps updating the restored screens",
            !changed.is_empty(),
            format!(
                "{} of {} restored panes changed; bytes received: {:?}",
                changed.len(),
                cores.len(),
                bytes
            ),
        );
        if let Some((pane, count)) = busiest {
            report.check(
                "the busiest pane is among the updated ones",
                changed.contains(&pane),
                format!(
                    "pane {pane} received {count} bytes and changed={}",
                    changed.contains(&pane)
                ),
            );
        }
    }

    session_ctl.close();
    Ok((report, evidence))
}

/// Prints a readable diff for the first disagreement of every pane.
pub fn print_evidence(evidence: &[PaneEvidence]) {
    for entry in evidence {
        println!(
            "--- pane {} ({}x{}): cursor ours=({},{}) tmux=({},{}) | alternate ours={} tmux={} | history ours={} tmux={}",
            entry.pane,
            entry.columns,
            entry.rows,
            entry.cursor_ours.0,
            entry.cursor_ours.1,
            entry.cursor_theirs.0,
            entry.cursor_theirs.1,
            entry.alternate_ours,
            entry.alternate_theirs,
            entry.history_ours,
            entry.history_theirs
        );
        println!(
            "    responses suppressed during the restore: {}",
            entry.responses_suppressed
        );
        if entry.text_matches() {
            let non_blank = entry
                .ours
                .iter()
                .filter(|line| !line.trim().is_empty())
                .count();
            let last = entry
                .theirs
                .iter()
                .rev()
                .find(|line| !line.trim().is_empty())
                .cloned()
                .unwrap_or_default();
            println!(
                "    text matches ({non_blank} non-blank rows compared); last non-blank row: {last:?}"
            );
        } else if let Some((row, ours, theirs)) = &entry.first_difference {
            println!("    first difference on row {row}:");
            println!("      ours: {ours:?}");
            println!("      tmux: {theirs:?}");
        }
    }
}
