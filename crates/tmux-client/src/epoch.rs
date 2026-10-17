//! Connection epochs.
//!
//! A control-mode connection is not a stable identity: TCP dies, the user
//! reattaches, the tmux server is restarted, and pane ids such as `%1` come back
//! meaning something else.  Each [`crate::session::ControlSession`] owns the
//! [`ConnectionEpoch`] it was created with, and the model it exposes is tied to
//! that epoch.
//!
//! A consumer that reconnects must create a *new* session and drop the old one;
//! it must never merge events from two sessions.  There is deliberately no
//! per-event epoch filter: events come out of one session in order, and keeping a
//! second connection alive would defeat the snapshot watermark rule.

use std::sync::atomic::{AtomicU64, Ordering};

/// Identifier of one control-mode connection attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnectionEpoch(u64);

static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);

impl ConnectionEpoch {
    /// Allocates a fresh epoch.  Epochs are strictly increasing for the lifetime
    /// of the process, so "older" is always well defined.
    #[must_use]
    pub fn next() -> Self {
        Self(NEXT_EPOCH.fetch_add(1, Ordering::Relaxed))
    }

    /// The raw counter value.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl core::fmt::Display for ConnectionEpoch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "epoch-{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epochs_are_strictly_increasing() {
        let a = ConnectionEpoch::next();
        let b = ConnectionEpoch::next();
        assert!(b > a);
    }

    #[test]
    fn epochs_are_unique_across_threads() {
        let handles: Vec<_> = (0..4)
            .map(|_| std::thread::spawn(ConnectionEpoch::next))
            .collect();
        let mut epochs: Vec<u64> = handles
            .into_iter()
            .map(|handle| handle.join().expect("thread").as_u64())
            .collect();
        epochs.sort_unstable();
        epochs.dedup();
        assert_eq!(epochs.len(), 4);
    }
}
