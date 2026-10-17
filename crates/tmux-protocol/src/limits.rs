//! Resource limits enforced by the protocol layer.
//!
//! Remote output, window titles and captured content are untrusted input.  Every
//! buffer that a remote peer can grow must have a bound, and hitting a bound must
//! be reported (never silently absorbed).

use crate::parser::ParseError;

/// Bounds applied by [`crate::parser::Parser`] and friends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Longest accepted protocol line, in bytes.
    ///
    /// Applies to notifications, `%output` payloads and the `%begin`/`%end`
    /// framing lines.  A pane that prints a single line longer than this is
    /// reported as a desynchronisation instead of being buffered without bound.
    pub max_line_bytes: usize,
    /// Largest number of bytes accumulated inside one command response block.
    pub max_response_bytes: u64,
    /// Largest number of lines accumulated inside one command response block.
    pub max_response_lines: u32,
    /// Largest buffer the `-CC` takeover scanner may hold while looking for a
    /// DCS introducer that is split across reads.
    pub max_switch_buffer_bytes: usize,
    /// Largest command line this crate will encode.
    pub max_encoded_command_bytes: usize,
    /// Maximum number of commands that may be awaiting a response.
    pub max_in_flight_commands: usize,
    /// Maximum number of encoded commands waiting to be written.
    pub max_pending_commands: usize,
}

impl Limits {
    /// Conservative limits for an interactive session: 1 MiB lines, 32 MiB
    /// response blocks, 4096 queued lines, one command in flight.
    #[must_use]
    pub const fn interactive() -> Self {
        Self {
            max_line_bytes: 1 << 20,
            max_response_bytes: 32 << 20,
            max_response_lines: 4096,
            max_switch_buffer_bytes: 1 << 16,
            max_encoded_command_bytes: 1 << 20,
            max_in_flight_commands: 1,
            max_pending_commands: 4096,
        }
    }

    /// Limits for replaying recorded fixtures and fuzzing: small enough that a
    /// bound is hit quickly, large enough for the recorded captures.
    #[must_use]
    pub const fn strict() -> Self {
        Self {
            max_line_bytes: 4096,
            max_response_bytes: 1 << 20,
            max_response_lines: 256,
            max_switch_buffer_bytes: 4096,
            max_encoded_command_bytes: 4096,
            max_in_flight_commands: 1,
            max_pending_commands: 64,
        }
    }

    /// Checks that every field is usable.
    ///
    /// A zero limit makes the corresponding buffer or queue useless (a zero
    /// `max_line_bytes`, for example, would reject every protocol line), so it
    /// is treated as a configuration error rather than silently clamped.
    ///
    /// # Errors
    ///
    /// Returns [`ParseError::ZeroLimit`] naming the first field that is zero.
    pub fn validate(&self) -> Result<(), ParseError> {
        if self.max_line_bytes == 0 {
            return Err(ParseError::ZeroLimit("max_line_bytes"));
        }
        if self.max_response_bytes == 0 {
            return Err(ParseError::ZeroLimit("max_response_bytes"));
        }
        if self.max_response_lines == 0 {
            return Err(ParseError::ZeroLimit("max_response_lines"));
        }
        if self.max_switch_buffer_bytes == 0 {
            return Err(ParseError::ZeroLimit("max_switch_buffer_bytes"));
        }
        if self.max_encoded_command_bytes == 0 {
            return Err(ParseError::ZeroLimit("max_encoded_command_bytes"));
        }
        if self.max_in_flight_commands == 0 {
            return Err(ParseError::ZeroLimit("max_in_flight_commands"));
        }
        if self.max_pending_commands == 0 {
            return Err(ParseError::ZeroLimit("max_pending_commands"));
        }
        Ok(())
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self::interactive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_limit_sets_are_valid() {
        assert_eq!(Limits::interactive().validate(), Ok(()));
        assert_eq!(Limits::strict().validate(), Ok(()));
        assert_eq!(Limits::default().validate(), Ok(()));
    }

    #[test]
    fn every_zero_field_is_rejected_by_name() {
        let cases: [(Limits, &'static str); 7] = [
            (
                Limits {
                    max_line_bytes: 0,
                    ..Limits::interactive()
                },
                "max_line_bytes",
            ),
            (
                Limits {
                    max_response_bytes: 0,
                    ..Limits::interactive()
                },
                "max_response_bytes",
            ),
            (
                Limits {
                    max_response_lines: 0,
                    ..Limits::interactive()
                },
                "max_response_lines",
            ),
            (
                Limits {
                    max_switch_buffer_bytes: 0,
                    ..Limits::interactive()
                },
                "max_switch_buffer_bytes",
            ),
            (
                Limits {
                    max_encoded_command_bytes: 0,
                    ..Limits::interactive()
                },
                "max_encoded_command_bytes",
            ),
            (
                Limits {
                    max_in_flight_commands: 0,
                    ..Limits::interactive()
                },
                "max_in_flight_commands",
            ),
            (
                Limits {
                    max_pending_commands: 0,
                    ..Limits::interactive()
                },
                "max_pending_commands",
            ),
        ];
        for (limits, field) in cases {
            assert_eq!(limits.validate(), Err(ParseError::ZeroLimit(field)));
        }
    }
}
