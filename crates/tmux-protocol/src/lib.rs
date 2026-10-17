//! Clean-room implementation of the tmux **control mode** protocol.
//!
//! This crate is the core asset of the project.  It is deliberately free of any
//! I/O, GUI or runtime dependency so that it can be exercised by replay tests,
//! property tests and fuzzers, and so that its behaviour is provable without a
//! terminal, an SSH connection or a display server.
//!
//! # Provenance
//!
//! Every behaviour implemented here must be traceable to either
//! * public documentation (`tmux(1)`, the tmux wiki page *Control Mode*), or
//! * a recorded black-box observation produced by `tools/probe/probe_control_mode.py`
//!   and stored under `tests/fixtures/`.
//!
//! No iTerm2 source code, translation of it, or design document derived from its
//! internals may be used here.  See `docs/clean-room.md`.
//!
//! # Layering
//!
//! ```text
//!   bytes ──▶ TakeoverSwitch ──▶ Parser ──▶ Event
//!              (only for -CC)      │
//!                                  └──▶ CommandQueue ──▶ CommandEncoder ──▶ bytes
//! ```
//!
//! * [`switch`] recognises the `-CC` DCS takeover sequence and routes bytes
//!   between a normal terminal emulator and the control-mode parser.
//! * [`parser`] is a streaming, byte-oriented parser.  It never assumes UTF-8.
//! * [`command`] encodes commands (including typed key input) without string
//!   concatenation.
//! * [`queue`] schedules commands and matches server responses back to requests
//!   without trusting the server's own numbering.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(clippy::all)]
#![allow(clippy::module_name_repetitions)]

pub mod command;
pub mod escape;
pub mod ids;
pub mod layout;
pub mod limits;
pub mod parser;
pub mod queue;
pub mod switch;

pub use command::{Command, KeySpec};
pub use escape::{decode_octal, encode_octal};
pub use ids::{PaneId, SessionId, WindowId};
pub use layout::{Layout, LayoutNode, Orientation};
pub use limits::Limits;
pub use parser::{Event, ParseError, Parser, ResponseHeader};
pub use queue::{CommandQueue, QueueError, RequestId, Response};
pub use switch::{SwitchAction, TakeoverSwitch};

/// The version of the protocol specification this implementation targets.
///
/// Kept in step with `docs/spec/control-mode.md`; the compatibility matrix is in
/// `docs/spec/compatibility.md`.
pub const SPEC_VERSION: &str = "control-mode/0.2";

/// Default nonce marker used to find the end of the unsolicited initialisation
/// responses.  The client sends `display-message -p <SYNC_MARKER>` and treats
/// every response up to and including that one as initialisation output.
pub const SYNC_MARKER: &str = "__muxterm_sync__";
