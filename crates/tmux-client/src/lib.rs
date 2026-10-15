//! Transport, remote model and synchronisation for tmux control mode.
//!
//! Layer map:
//!
//! | module | responsibility | may talk to the network? |
//! |---|---|---|
//! | [`crate::transport`] | spawn and talk to `tmux -C` / `ssh` | yes (that is its job) |
//! | [`crate::epoch`] | reject events from an abandoned connection | no |
//! | [`crate::model`] | the remote object model (source of truth) | no |
//! | [`crate::sync`] | snapshot / resynchronisation sequence | no |
//! | [`crate::session`] | drives all of the above in stream order | through the transport |
//!
//! The protocol itself lives in the `tmux-protocol` crate, which has no I/O at all.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(clippy::all)]
#![allow(clippy::module_name_repetitions)]

pub mod epoch;
pub mod model;
pub mod session;
pub mod sync;
pub mod transport;

pub use epoch::{ConnectionEpoch, EpochFilter};
pub use model::{Consistency, ModelChange, PaneState, RemoteModel, SessionState, WindowState};
pub use session::{ControlSession, SessionError, SessionEvent};
pub use sync::{SyncEngine, SyncError, SyncOutcome, SyncStats, Tag, TaggedCommand};
pub use transport::{shell_quote, ProcessTransport, Target, Transport, TransportError};
