//! From what tmux reports to what the terminal core restores.
//!
//! The two sides use different vocabularies on purpose.  `tmux-client` names the
//! `#{..._flag}` formats it queries (an application asked for DECSET 1002, tmux
//! reports `mouse_button_flag`), and `term-core` names the switches it emits
//! (`mouse_drag`).  This module is the only place that knows both, so neither crate
//! has to depend on the other.

use term_core::{InputModes, ScreenSnapshot};
use tmux_client::model::{PaneModes, PaneState};

/// Translates the modes tmux reported into the modes a restore can apply.
///
/// Unknown modes stay unknown.  `bracketed_paste` stays `None` when the server could
/// not report it (before tmux 3.7), so the restore leaves the emulator default
/// alone; focus reporting and UTF-8 mouse coordinates have no tmux flag on any
/// release up to 3.8 and are therefore neither restored nor guessed.
#[must_use]
pub(crate) fn input_modes(modes: &PaneModes) -> InputModes {
    InputModes {
        mouse_click: modes.mouse_click,
        mouse_drag: modes.mouse_drag,
        mouse_motion: modes.mouse_motion,
        mouse_sgr: modes.mouse_sgr,
        application_cursor: modes.application_cursor,
        application_keypad: modes.application_keypad,
        show_cursor: modes.show_cursor,
        insert: modes.insert,
        origin: modes.origin,
        line_wrap: modes.line_wrap,
        bracketed_paste: modes.bracketed_paste,
    }
}

/// Builds the snapshot the terminal core restores for one pane.
///
/// Geometry comes from the model, contents from the `capture-pane`, and the parts a
/// capture cannot carry — cursor, alternate screen, history bounds and the
/// application input modes — from the per-pane metadata.  `state.modes` is `None`
/// only before a metadata response has been applied, in which case the snapshot
/// says so instead of claiming every mode is off.
#[must_use]
pub(crate) fn snapshot(state: &PaneState, lines: Vec<Vec<u8>>) -> ScreenSnapshot {
    let mut snapshot = ScreenSnapshot::new(
        u16::try_from(state.width).unwrap_or(u16::MAX),
        u16::try_from(state.height).unwrap_or(u16::MAX),
        lines,
    );
    snapshot.cursor = (
        u16::try_from(state.cursor.0).unwrap_or(u16::MAX),
        u16::try_from(state.cursor.1).unwrap_or(u16::MAX),
    );
    snapshot.alternate_on = state.alternate_on;
    snapshot.history_size = state.history_size;
    snapshot.history_limit = state.history_limit;
    snapshot.in_mode = state.in_mode;
    snapshot.mode_name = state.mode.clone();
    snapshot.dead = state.dead;
    snapshot.input_modes = state.modes.as_ref().map(input_modes);
    snapshot
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pane_without_metadata_does_not_claim_everything_is_off() {
        // `PaneState` has a private field on purpose (the snapshot generation), so
        // it is built through `Default` and then filled in.
        let mut state = PaneState::default();
        state.width = 80;
        state.height = 24;
        state.cursor = (3, 4);
        let snap = snapshot(&state, vec![b"line".to_vec()]);
        assert_eq!(snap.input_modes, None);

        state.modes = Some(PaneModes {
            mouse_drag: true,
            bracketed_paste: None,
            ..PaneModes::default()
        });
        let snap = snapshot(&state, Vec::new());
        let modes = snap.input_modes.expect("the modes are carried over");
        assert!(modes.mouse_drag);
        assert!(!modes.mouse_click);
        assert_eq!(modes.bracketed_paste, None);
        assert_eq!(snap.cursor, (3, 4));
    }
}
