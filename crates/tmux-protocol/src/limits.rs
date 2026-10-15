//! Resource limits enforced by the protocol layer.
//!
//! Remote output, window titles and captured content are untrusted input.  Every
//! buffer that a remote peer can grow must have a bound, and hitting a bound must
//! be reported (never silently absorbed).

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
}

impl Default for Limits {
    fn default() -> Self {
        Self::interactive()
    }
}
