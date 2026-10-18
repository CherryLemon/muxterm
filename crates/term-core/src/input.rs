//! Encoding of mouse, paste and focus input.
//!
//! The GUI owns the widgets; this module owns the byte sequences an application
//! expects when it asked the terminal for mouse reporting (DECSET 1000/1002/1003),
//! bracketed paste or focus events.  Every function returns owned bytes so the
//! caller can hand them to `send-keys -H` unchanged.
//!
//! The encoding rules follow xterm.  They are deliberately kept here rather than in
//! the GUI so they can be tested without a window and stay reusable if the
//! emulator behind [`crate::TerminalCore`] is replaced.

use crate::view::Modes;

/// Keyboard modifiers held during an input event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers {
    /// Shift is held.
    pub shift: bool,
    /// Alt (Meta) is held.
    pub alt: bool,
    /// Control is held.
    pub ctrl: bool,
}

impl Modifiers {
    /// The xterm modifier bits: shift 4, alt 8, ctrl 16.
    fn bits(self) -> u8 {
        let mut bits = 0;
        if self.shift {
            bits |= 4;
        }
        if self.alt {
            bits |= 8;
        }
        if self.ctrl {
            bits |= 16;
        }
        bits
    }
}

/// The button involved in a mouse event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    /// Left button.
    Left,
    /// Middle button.
    Middle,
    /// Right button.
    Right,
    /// Wheel scrolled up.
    WheelUp,
    /// Wheel scrolled down.
    WheelDown,
}

impl MouseButton {
    /// The xterm button code.
    const fn code(self) -> u8 {
        match self {
            Self::Left => 0,
            Self::Middle => 1,
            Self::Right => 2,
            Self::WheelUp => 64,
            Self::WheelDown => 65,
        }
    }

    /// Wheels only report presses: they cannot be dragged or released.
    const fn is_wheel(self) -> bool {
        matches!(self, Self::WheelUp | Self::WheelDown)
    }
}

/// What happened to the button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseAction {
    /// A button (or wheel) was pressed.
    Press,
    /// A button was released.  Wheel releases are never reported.
    Release,
    /// The pointer moved while a button was held (or, with motion reporting, at
    /// all).
    Motion,
}

/// One mouse event in zero-based cell coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEvent {
    /// Cell column, zero-based.
    pub column: u16,
    /// Cell row, zero-based.
    pub row: u16,
    /// The button involved.
    pub button: MouseButton,
    /// What happened.
    pub action: MouseAction,
    /// Modifiers held during the event.
    pub modifiers: Modifiers,
}

/// True when the terminal asked for mouse reports at all.
fn reporting_enabled(modes: &Modes) -> bool {
    // `mouse_reporting` is the aggregate the view exports.  The individual flags
    // are checked too so a hand-built `Modes` value behaves sensibly.
    modes.mouse_reporting || modes.mouse_click || modes.mouse_drag || modes.mouse_motion
}

/// Encodes a mouse event for the reporting modes the pane enabled.
///
/// Returns `None` when the terminal did not ask for mouse input, when a motion
/// event is not reportable (motion reporting is off, and either drag reporting is
/// off or the "button" is a wheel), and for wheel releases.  SGR encoding
/// (`DECSET 1006`) carries arbitrary coordinates; the normal `CSI M` encoding can
/// only carry values up to 255 per field, so coordinates above 222 return `None`
/// unless UTF-8 coordinate reporting (`DECSET 1005`) is on.  In that mode the
/// field value `32 + coordinate + 1` is written as the UTF-8 encoding of that
/// Unicode scalar; values in the surrogate range are rejected because they have
/// no UTF-8 representation.
#[must_use]
pub fn encode_mouse(event: &MouseEvent, modes: &Modes) -> Option<Vec<u8>> {
    if !reporting_enabled(modes) {
        return None;
    }

    // A wheel has presses only: releasing it and dragging it are meaningless.
    if event.button.is_wheel() && event.action != MouseAction::Press {
        return None;
    }

    if event.action == MouseAction::Motion {
        // 1003 reports every motion; 1002 reports motion only while a button is
        // held, which is what a `Motion` event with a real button describes.
        if event.button.is_wheel() || !(modes.mouse_motion || modes.mouse_drag) {
            return None;
        }
    }

    if modes.mouse_sgr {
        Some(encode_mouse_sgr(event))
    } else {
        encode_mouse_normal(event, modes)
    }
}

/// SGR encoding: `CSI < code ; x+1 ; y+1 M` (press/motion) or `m` (release).
fn encode_mouse_sgr(event: &MouseEvent) -> Vec<u8> {
    let button = match event.action {
        MouseAction::Motion => event.button.code() + 32,
        MouseAction::Press | MouseAction::Release => event.button.code(),
    };
    let code = button + event.modifiers.bits();
    let terminator = if event.action == MouseAction::Release {
        'm'
    } else {
        'M'
    };
    format!(
        "\x1b[<{code};{};{}{terminator}",
        u32::from(event.column) + 1,
        u32::from(event.row) + 1
    )
    .into_bytes()
}

/// Normal encoding: `CSI M` followed by three offset bytes.
fn encode_mouse_normal(event: &MouseEvent, modes: &Modes) -> Option<Vec<u8>> {
    let button = match event.action {
        // Release always reports button 3, whatever was released.
        MouseAction::Release => 3,
        MouseAction::Motion => event.button.code() + 32,
        MouseAction::Press => event.button.code(),
    };
    let code = button + event.modifiers.bits();
    let x = u32::from(event.column) + 1;
    let y = u32::from(event.row) + 1;

    let mut out = Vec::with_capacity(6);
    out.extend_from_slice(b"\x1b[M");
    push_normal_field(&mut out, u32::from(code) + 32, modes.mouse_utf8)?;
    push_normal_field(&mut out, 32 + x, modes.mouse_utf8)?;
    push_normal_field(&mut out, 32 + y, modes.mouse_utf8)?;
    Some(out)
}

/// Writes one field of a normal-encoding report.
///
/// Without UTF-8 mouse mode the field must fit in one byte.  With it, the field
/// value is written as a UTF-8 scalar, which is how xterm extends the range.
fn push_normal_field(out: &mut Vec<u8>, value: u32, utf8: bool) -> Option<()> {
    if utf8 {
        let ch = char::from_u32(value)?;
        let mut buffer = [0_u8; 4];
        out.extend_from_slice(ch.encode_utf8(&mut buffer).as_bytes());
        return Some(());
    }
    out.push(u8::try_from(value).ok()?);
    Some(())
}

/// Wraps pasted bytes for the pane.
///
/// Bracketed paste is a byte-for-byte wrapper: `ESC [ 200 ~` before the text and
/// `ESC [ 201 ~` after it.  Nothing inside the text is escaped, matching what
/// xterm sends; a bracketed-paste marker embedded in the text is the
/// application's problem, not the terminal's.
#[must_use]
pub fn encode_paste(bytes: &[u8], bracketed: bool) -> Vec<u8> {
    if !bracketed {
        return bytes.to_vec();
    }
    let mut out = Vec::with_capacity(bytes.len() + 12);
    out.extend_from_slice(b"\x1b[200~");
    out.extend_from_slice(bytes);
    out.extend_from_slice(b"\x1b[201~");
    out
}

/// Encodes a focus change, or `None` when the application did not request focus
/// events.
#[must_use]
pub fn encode_focus(focused: bool, modes: &Modes) -> Option<Vec<u8>> {
    if !modes.focus_events {
        return None;
    }
    Some(if focused {
        b"\x1b[I".to_vec()
    } else {
        b"\x1b[O".to_vec()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mouse_modes() -> Modes {
        Modes {
            mouse_reporting: true,
            mouse_click: true,
            ..Modes::default()
        }
    }

    fn event(column: u16, row: u16, button: MouseButton, action: MouseAction) -> MouseEvent {
        MouseEvent {
            column,
            row,
            button,
            action,
            modifiers: Modifiers::default(),
        }
    }

    #[test]
    fn normal_encoding_covers_press_release_and_drag() {
        let modes = mouse_modes();
        let press = event(0, 0, MouseButton::Left, MouseAction::Press);
        assert_eq!(encode_mouse(&press, &modes), Some(b"\x1b[M !!".to_vec()));

        let release = event(0, 0, MouseButton::Left, MouseAction::Release);
        assert_eq!(encode_mouse(&release, &modes), Some(b"\x1b[M#!!".to_vec()));

        // Release ignores which button it was: normal encoding says "3".
        let right_release = event(5, 1, MouseButton::Right, MouseAction::Release);
        assert_eq!(
            encode_mouse(&right_release, &modes),
            Some(b"\x1b[M#&\"".to_vec())
        );

        // 1000 alone does not report motion.
        let motion = event(3, 2, MouseButton::Left, MouseAction::Motion);
        assert_eq!(encode_mouse(&motion, &modes), None);

        // 1002 reports motion while a button is held.
        let drag_modes = Modes {
            mouse_click: false,
            mouse_drag: true,
            ..mouse_modes()
        };
        assert_eq!(
            encode_mouse(&motion, &drag_modes),
            Some(b"\x1b[M@$#".to_vec())
        );
    }

    #[test]
    fn sgr_encoding_covers_press_release_motion_and_wheel() {
        let modes = Modes {
            mouse_sgr: true,
            mouse_click: true,
            mouse_drag: true,
            mouse_motion: true,
            ..mouse_modes()
        };

        let press = event(0, 0, MouseButton::Left, MouseAction::Press);
        assert_eq!(encode_mouse(&press, &modes), Some(b"\x1b[<0;1;1M".to_vec()));

        let release = event(2, 4, MouseButton::Left, MouseAction::Release);
        assert_eq!(
            encode_mouse(&release, &modes),
            Some(b"\x1b[<0;3;5m".to_vec())
        );

        let motion = event(3, 2, MouseButton::Left, MouseAction::Motion);
        assert_eq!(
            encode_mouse(&motion, &modes),
            Some(b"\x1b[<32;4;3M".to_vec())
        );

        let wheel_up = event(1, 1, MouseButton::WheelUp, MouseAction::Press);
        assert_eq!(
            encode_mouse(&wheel_up, &modes),
            Some(b"\x1b[<64;2;2M".to_vec())
        );
        let wheel_down = event(1, 1, MouseButton::WheelDown, MouseAction::Press);
        assert_eq!(
            encode_mouse(&wheel_down, &modes),
            Some(b"\x1b[<65;2;2M".to_vec())
        );

        // A wheel is never released or dragged.
        let wheel_release = event(1, 1, MouseButton::WheelUp, MouseAction::Release);
        assert_eq!(encode_mouse(&wheel_release, &modes), None);
        let wheel_motion = event(1, 1, MouseButton::WheelUp, MouseAction::Motion);
        assert_eq!(encode_mouse(&wheel_motion, &modes), None);
    }

    #[test]
    fn modifier_bits_are_added_to_the_button_code() {
        let modes = Modes {
            mouse_sgr: true,
            ..mouse_modes()
        };
        let all = MouseEvent {
            column: 0,
            row: 0,
            button: MouseButton::Left,
            action: MouseAction::Press,
            modifiers: Modifiers {
                shift: true,
                alt: true,
                ctrl: true,
            },
        };
        assert_eq!(encode_mouse(&all, &modes), Some(b"\x1b[<28;1;1M".to_vec()));

        let shift_only = MouseEvent {
            modifiers: Modifiers {
                shift: true,
                ..Modifiers::default()
            },
            ..all
        };
        assert_eq!(
            encode_mouse(&shift_only, &modes),
            Some(b"\x1b[<4;1;1M".to_vec())
        );

        // The bits also apply to motion: 4 + 32 (drag) + 16 (ctrl).
        let drag = MouseEvent {
            column: 3,
            row: 2,
            action: MouseAction::Motion,
            modifiers: Modifiers {
                shift: true,
                ctrl: true,
                alt: false,
            },
            ..all
        };
        let drag_modes = Modes {
            mouse_drag: true,
            ..modes
        };
        assert_eq!(
            encode_mouse(&drag, &drag_modes),
            Some(b"\x1b[<52;4;3M".to_vec())
        );
    }

    #[test]
    fn no_reporting_mode_means_no_bytes() {
        let modes = Modes::default();
        let press = event(1, 1, MouseButton::Left, MouseAction::Press);
        assert_eq!(encode_mouse(&press, &modes), None);
        assert_eq!(
            encode_mouse(
                &MouseEvent {
                    action: MouseAction::Motion,
                    ..press
                },
                &modes
            ),
            None
        );

        // SGR encoding without a reporting mode is still off.
        let sgr_only = Modes {
            mouse_sgr: true,
            ..Modes::default()
        };
        assert_eq!(encode_mouse(&press, &sgr_only), None);
    }

    #[test]
    fn normal_encoding_rejects_coordinates_that_do_not_fit() {
        let modes = mouse_modes();

        // 224 columns: column 223 makes 32 + 223 + 1 exceed one byte.
        let last = event(223, 0, MouseButton::Left, MouseAction::Press);
        assert_eq!(encode_mouse(&last, &modes), None);

        let fits = event(222, 0, MouseButton::Left, MouseAction::Press);
        assert_eq!(
            encode_mouse(&fits, &modes),
            Some(vec![0x1b, b'[', b'M', 32, 255, 33])
        );

        // UTF-8 mouse mode extends the field: 256 is U+0100.
        let utf8_modes = Modes {
            mouse_utf8: true,
            ..modes
        };
        assert_eq!(
            encode_mouse(&last, &utf8_modes),
            Some(vec![0x1b, b'[', b'M', 32, 0xc4, 0x80, 33])
        );
    }

    #[test]
    fn paste_is_wrapped_only_for_bracketed_mode() {
        assert_eq!(encode_paste(b"hello", false), b"hello".to_vec());
        assert_eq!(
            encode_paste(b"hello", true),
            b"\x1b[200~hello\x1b[201~".to_vec()
        );
        assert_eq!(encode_paste(b"", true), b"\x1b[200~\x1b[201~".to_vec());
    }

    #[test]
    fn focus_is_encoded_only_when_requested() {
        let off = Modes::default();
        assert_eq!(encode_focus(true, &off), None);
        assert_eq!(encode_focus(false, &off), None);

        let on = Modes {
            focus_events: true,
            ..Modes::default()
        };
        assert_eq!(encode_focus(true, &on), Some(b"\x1b[I".to_vec()));
        assert_eq!(encode_focus(false, &on), Some(b"\x1b[O".to_vec()));
    }
}
