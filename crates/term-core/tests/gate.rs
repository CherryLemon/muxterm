//! The capability gate from `docs/adr/0002`, as executable checks.
//!
//! These tests use synthetic capture-style streams so they run without a tmux
//! server.  The cross-check against a *real* pane (our grid versus what tmux says
//! the pane contains) lives in `poc core-gate`, which runs inside the container.

use term_core::{
    AlacrittyCore, CellFlags, Color, CursorShape, Damage, ScreenSnapshot, TerminalCore,
};

fn core(columns: u16, rows: u16) -> AlacrittyCore {
    AlacrittyCore::new(columns, rows, 1000)
}

fn snapshot(columns: u16, rows: u16, lines: &[&[u8]]) -> ScreenSnapshot {
    ScreenSnapshot::new(
        columns,
        rows,
        lines.iter().map(|line| line.to_vec()).collect(),
    )
}

/// Gate 4 helper: rendering only needs a shared reference, and the boundary is
/// object safe, so a renderer never has to borrow the terminal mutably.
fn render_text(core: &dyn TerminalCore) -> String {
    core.screen().dump_text()
}

// ---------------------------------------------------------------- gate 1 -----

#[test]
fn gate1_restores_text_and_styles_from_a_capture() {
    let mut core = core(40, 5);
    let snapshot = snapshot(
        40,
        5,
        &[
            b"\x1b[1;31mRED BOLD\x1b[0m plain \x1b[4;44munderline-on-blue\x1b[0m",
            b"second line",
        ],
    );
    let report = core.restore(&snapshot).expect("restore succeeds");
    assert_eq!(report.lines_painted, 2);
    assert!(report.cursor_set);
    assert!(!report.alternate_switched);
    assert!(!report.alternate_screen);

    assert!(render_text(&core).starts_with("RED BOLD plain underline-on-blue"));

    let first = core.screen().cell(0, 0).expect("cell exists");
    assert_eq!(first.ch, 'R');
    assert!(first.flags.bold, "SGR 1 must survive the capture");
    assert_eq!(first.fg, Color::Indexed(1), "SGR 31 is palette red");
    assert_eq!(first.bg, Color::Default);

    // Find the runs by their text rather than by hard-coded columns: what matters
    // is that the attributes follow the characters through the capture.
    let row = core.screen().line_text(0);
    let plain_at = row.find("plain").expect("the plain run is on row 0");
    let plain = core.screen().cell(0, plain_at).expect("cell exists");
    assert!(!plain.flags.bold, "SGR 0 must reset the attributes");
    assert_eq!(plain.fg, Color::Default);
    assert_eq!(plain.bg, Color::Default);

    let underlined_at = row.find("underline").expect("the styled run is on row 0");
    let underlined = core.screen().cell(0, underlined_at).expect("cell exists");
    assert!(underlined.flags.underline);
    assert_eq!(underlined.bg, Color::Indexed(4));

    // Rows below the capture are blank, not leftovers.
    assert_eq!(core.screen().line_text(2).trim_end(), "");
}

#[test]
fn gate1_wide_characters_occupy_two_cells() {
    let mut core = core(20, 3);
    core.restore(&snapshot(20, 3, &["中文字".as_bytes()]))
        .expect("restore succeeds");

    let first = core.screen().cell(0, 0).expect("cell exists");
    assert_eq!(first.ch, '中');
    assert!(first.flags.wide, "the leading cell must be marked wide");
    let spacer = core.screen().cell(0, 1).expect("cell exists");
    assert!(
        spacer.flags.wide_spacer,
        "the second half of a wide glyph is a spacer cell"
    );

    // Text extraction must not double-count the spacer.
    assert_eq!(core.screen().line_text(0).trim_end(), "中文字");
}

#[test]
fn gate1_combining_characters_stay_attached() {
    let mut core = core(10, 2);
    core.restore(&snapshot(10, 2, &["e\u{0301}x".as_bytes()]))
        .expect("restore succeeds");
    let cell = core.screen().cell(0, 0).expect("cell exists");
    assert_eq!(cell.ch, 'e');
    assert_eq!(cell.zerowidth, vec!['\u{0301}']);
    assert_eq!(core.screen().line_text(0).trim_end(), "e\u{0301}x");
}

// ---------------------------------------------------------------- gate 2 -----

#[test]
fn gate2_cursor_and_alternate_screen_come_from_outside_the_stream() {
    let mut core = core(20, 5);
    let mut snap = snapshot(20, 5, &[b"content"]);
    snap.cursor = (7, 3);
    let report = core.restore(&snap).expect("restore succeeds");
    assert!(report.cursor_set);

    let cursor = core.cursor();
    assert_eq!((cursor.row, cursor.column), (3, 7));
    assert_eq!(cursor.shape, CursorShape::Block);

    // Alternate screen: the flag alone (no `?1049h` in the capture text) must be
    // enough to put the core on the alternate screen.
    assert!(!core.modes().alternate_screen);
    snap.alternate_on = true;
    let report = core.restore(&snap).expect("restore succeeds");
    assert!(report.alternate_switched, "the switch is reported");
    assert!(core.modes().alternate_screen);

    // ...and back again.
    snap.alternate_on = false;
    core.restore(&snap).expect("restore succeeds");
    assert!(!core.modes().alternate_screen);
}

#[test]
fn gate2_history_is_rebuilt_from_the_capture() {
    let mut core = core(20, 2);
    let snapshot = snapshot(
        20,
        2,
        &[b"old 1", b"old 2", b"old 3", b"screen 1", b"screen 2"],
    );
    assert_eq!(snapshot.history_lines(), 3);
    let report = core.restore(&snapshot).expect("restore succeeds");
    assert_eq!(report.history_lines, 3);
    assert_eq!(core.screen().line_text(0).trim_end(), "screen 1");
    assert_eq!(core.screen().line_text(1).trim_end(), "screen 2");
    assert!(
        core.history_size() >= 3,
        "the captured scrollback must be available again, got {}",
        core.history_size()
    );
}

// ---------------------------------------------------------------- gate 3 -----

#[test]
fn gate3_responses_are_captured_not_written() {
    let mut core = core(40, 5);
    // Cursor position report, primary device attributes, and a bell.
    core.feed(b"\x1b[6n\x1b[c\x07");

    let responses = core.take_responses();
    assert!(
        responses.len() >= 2,
        "the emulator must produce responses for DSR and DA: {responses:?}"
    );
    assert!(responses.iter().all(|response| !response.is_empty()));
    assert!(
        core.take_responses().is_empty(),
        "taking responses drains them: nothing is sent anywhere"
    );

    let effects = core.side_effects();
    assert_eq!(effects.bell, 1, "the bell is counted, not rung");
}

#[test]
fn gate3_a_restore_is_silent() {
    let mut core = core(20, 3);
    // A capture whose text happens to contain a query: repainting it must not
    // answer the query, and the dropped response has to be visible in the report.
    let report = core
        .restore(&snapshot(20, 3, &[b"query: \x1b[6n"]))
        .expect("restore succeeds");
    assert!(
        report.responses_suppressed >= 1,
        "a repaint must suppress the responses it provokes, report: {report:?}"
    );
    assert!(
        core.take_responses().is_empty(),
        "nothing may be queued for sending during a restore"
    );
}

// ---------------------------------------------------------------- gate 4 -----

#[test]
fn gate4_the_view_is_read_only_and_object_safe() {
    let mut core = core(20, 3);
    let baseline_history = core.history_size();
    core.restore(&snapshot(20, 3, &[b"read me"]))
        .expect("restore succeeds");

    // Rendering takes a shared reference only; this function cannot mutate.
    let text = render_text(&core);
    assert!(text.starts_with("read me"));

    let view = core.screen();
    assert_eq!(view.columns(), 20);
    assert_eq!(view.rows(), 3);
    assert_eq!(
        view.history_size(),
        baseline_history,
        "restoring a one-line capture must not invent scrollback"
    );
    assert_eq!(view.display_offset(), 0);
    let cell = view.cell(0, 0).expect("cell exists");
    assert_eq!(cell.ch, 'r');
    assert!(!cell.is_blank());
    assert!(
        view.cell(0, 100).is_none(),
        "out of range is None, not a panic"
    );
    assert!(view.line(99).is_none());
}

#[test]
fn gate4_cell_flags_are_exposed_without_emulator_types() {
    let mut core = core(20, 3);
    core.feed(b"\x1b[1;3;4;7;9mX\x1b[0m");
    let cell = core.screen().cell(0, 0).expect("cell exists");
    let expected = CellFlags {
        bold: true,
        italic: true,
        underline: true,
        inverse: true,
        strikeout: true,
        ..CellFlags::default()
    };
    assert_eq!(cell.flags, expected);
}

// ---------------------------------------------------------------- gate 5 -----

#[test]
fn gate5_scrollback_bound_is_enforced() {
    let mut core = AlacrittyCore::new(20, 2, 5);
    for index in 0..50 {
        core.feed(format!("line {index}\r\n").as_bytes());
    }
    assert!(
        core.history_size() <= 5,
        "history must respect the configured bound, got {}",
        core.history_size()
    );
}

#[test]
fn gate5_the_bound_can_be_changed_at_runtime() {
    let mut core = AlacrittyCore::new(20, 2, 100);
    for index in 0..60 {
        core.feed(format!("line {index}\r\n").as_bytes());
    }
    assert!(core.history_size() > 10);

    core.set_history_limit(4);
    assert!(
        core.history_size() <= 4,
        "lowering the bound must trim what is already there, got {}",
        core.history_size()
    );

    core.set_history_limit(1000);
    for index in 0..40 {
        core.feed(format!("more {index}\r\n").as_bytes());
    }
    assert!(
        core.history_size() > 4,
        "raising the bound takes effect again"
    );
}

// ------------------------------------------------------------- integration ---

#[test]
fn live_output_continues_after_a_restore() {
    // The watermark rule from docs/spec/state-sync.md, at this layer: once the
    // snapshot is painted, later bytes append to it in order.
    let mut core = core(20, 3);
    let mut snap = snapshot(20, 3, &[b"$ prompt"]);
    // The snapshot's cursor sits on the empty row below the content, which is where
    // the shell would leave it.
    snap.cursor = (0, 1);
    core.restore(&snap).expect("restore succeeds");
    assert_eq!(core.screen().line_text(0).trim_end(), "$ prompt");

    core.feed(b"echo hi");
    let text = render_text(&core);
    assert!(
        text.starts_with("$ prompt\necho hi"),
        "live bytes must be appended to the restored screen in order: {text:?}"
    );
    let cursor = core.cursor();
    assert_eq!((cursor.row, cursor.column), (1, 7));
}

// ------------------------------------------------- incremental refresh ------

#[test]
fn damage_after_feed_covers_the_changed_row() {
    let mut core = core(20, 3);
    assert_eq!(
        core.take_damage(),
        Damage::Full,
        "a freshly built core must be painted once"
    );

    core.feed(b"visible");
    let damage = core.take_damage();
    match &damage {
        Damage::Partial(lines) => {
            assert!(!lines.is_empty(), "feeding a visible line damages it");
            let row = lines
                .iter()
                .find(|line| line.row == 0)
                .unwrap_or_else(|| panic!("row 0 must be in the report: {lines:?}"));
            assert!(
                row.left == 0 && row.right >= 6,
                "the text span is covered: {row:?}"
            );
        }
        Damage::Full => panic!("a feed on a settled core must be partial: {damage:?}"),
    }

    assert_eq!(
        core.take_damage(),
        Damage::Partial(Vec::new()),
        "without new input there is nothing to repaint"
    );
}

#[test]
fn a_feed_before_the_first_report_is_reported_as_lines() {
    let mut core = core(20, 3);
    core.feed(b"visible");

    match core.take_damage() {
        Damage::Partial(lines) => {
            assert!(!lines.is_empty());
            assert!(
                lines
                    .iter()
                    .any(|line| line.row == 0 && line.left == 0 && line.right >= 6),
                "the fed row must be covered: {lines:?}"
            );
            assert_eq!(
                lines.len(),
                3,
                "the initial full damage is expressed as one span per row"
            );
        }
        Damage::Full => panic!("a feed before the first report must be line-level"),
    }

    assert_eq!(core.take_damage(), Damage::Partial(Vec::new()));
}

#[test]
fn resize_and_restore_mark_the_screen_fully_damaged() {
    let mut core = core(20, 3);
    assert_eq!(core.take_damage(), Damage::Full);

    core.resize(30, 4);
    assert_eq!(
        core.take_damage(),
        Damage::Full,
        "a resize invalidates the old geometry"
    );

    core.restore(&snapshot(20, 3, &[b"restored"]))
        .expect("restore succeeds");
    assert_eq!(
        core.take_damage(),
        Damage::Full,
        "a restore repaints every cell"
    );
}

#[test]
fn mouse_modes_are_exported_individually() {
    let mut core = core(20, 3);
    core.feed(b"\x1b[?1000h\x1b[?1006h");

    let modes = core.modes();
    assert!(
        modes.mouse_reporting,
        "the aggregate flag follows click mode"
    );
    assert!(modes.mouse_click);
    assert!(!modes.mouse_drag);
    assert!(!modes.mouse_motion);
    assert!(modes.mouse_sgr);
    assert!(!modes.mouse_utf8);

    core.feed(b"\x1b[?1003h\x1b[?1005h");
    let modes = core.modes();
    assert!(modes.mouse_reporting);
    assert!(!modes.mouse_click, "mouse reporting modes are exclusive");
    assert!(modes.mouse_motion);
    assert!(!modes.mouse_sgr, "mouse encodings are exclusive");
    assert!(modes.mouse_utf8);
}

// ------------------------------------------------------- restore idempotency --

#[test]
fn restore_is_idempotent_across_snapshots() {
    let mut snap_a = snapshot(
        24,
        4,
        &[
            b"$ history one",
            b"$ history two",
            b"$ history three",
            b"alpha \x1b[1mbold\x1b[0m",
            b"bravo",
            b"charlie",
            b"$ cursor \x1b[6n",
        ],
    );
    snap_a.cursor = (2, 3);
    assert_eq!(snap_a.history_lines(), 3);

    let mut fresh = core(24, 4);
    let report = fresh.restore(&snap_a).expect("restore succeeds");
    assert!(
        report.responses_suppressed >= 1,
        "the repaint's query must be suppressed: {report:?}"
    );
    let expected_text = fresh.screen().dump_text();
    let expected_cursor = fresh.cursor();
    let expected_modes = fresh.modes();
    let expected_history = fresh.history_size();
    assert_eq!(expected_history, 3);

    // Leave the reused core in a completely different state: other geometry,
    // other content, history, cursor and modes.
    let mut reused = core(24, 4);
    reused.feed(b"\x1b[?1h\x1b[?6h\x1b[?1000h\x1b[?1006h");
    assert!(reused.modes().application_cursor);
    let mut snap_b = snapshot(
        30,
        6,
        &[
            b"stale history",
            b"stale one",
            b"stale two",
            b"stale three",
            b"stale four",
            b"stale five",
            b"stale six",
        ],
    );
    snap_b.cursor = (9, 4);
    reused.restore(&snap_b).expect("restore succeeds");
    assert_eq!(reused.screen().line_text(0).trim_end(), "stale one");

    let report = reused.restore(&snap_a).expect("restore succeeds");
    assert!(
        report.responses_suppressed >= 1,
        "a restore still suppresses responses: {report:?}"
    );

    assert_eq!(
        reused.screen().dump_text(),
        expected_text,
        "a second restore must not paint over the previous screen"
    );
    assert_eq!(reused.cursor(), expected_cursor);
    assert_eq!(
        reused.modes(),
        expected_modes,
        "modes must come from the snapshot, not from the old screen"
    );
    assert_eq!(reused.history_size(), expected_history);
}

#[test]
fn restore_is_idempotent_on_the_alternate_screen() {
    let mut snap_a = snapshot(20, 3, &[b"alt one", b"alt two", b"alt three"]);
    snap_a.alternate_on = true;
    snap_a.cursor = (4, 1);

    let mut fresh = core(20, 3);
    fresh.restore(&snap_a).expect("restore succeeds");
    let expected_text = fresh.screen().dump_text();
    let expected_cursor = fresh.cursor();
    let expected_modes = fresh.modes();
    let expected_history = fresh.history_size();

    let mut reused = core(20, 3);
    reused.feed(b"\x1b[?1h\x1b[?6h");
    let mut snap_b = snapshot(
        20,
        3,
        &[b"history", b"primary one", b"primary two", b"primary three"],
    );
    snap_b.cursor = (5, 2);
    reused.restore(&snap_b).expect("restore succeeds");
    assert!(!reused.modes().alternate_screen);

    let report = reused.restore(&snap_a).expect("restore succeeds");
    assert!(report.alternate_switched);
    assert!(report.cursor_set);
    assert_eq!(reused.screen().dump_text(), expected_text);
    assert_eq!(reused.cursor(), expected_cursor);
    assert_eq!(reused.modes(), expected_modes);
    assert_eq!(reused.history_size(), expected_history);
}
