//! Connection epochs.
//!
//! A control-mode connection is not a stable identity: TCP dies, the user
//! reattaches, the tmux server is restarted, and pane ids such as `%1` come back
//! meaning something else.  Every event, response and resize request therefore
//! carries the [`ConnectionEpoch`] of the connection that produced it, and a
//! consumer drops anything from an epoch it has already left behind.
//!
//! This is the mechanism that stops a late reply from a dead connection from
//! being applied to the freshly restored state.

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

/// Tracks which epoch a consumer currently accepts.
#[derive(Debug, Clone)]
pub struct EpochFilter {
    accepted: ConnectionEpoch,
    rejected: u64,
}

impl EpochFilter {
    /// Accepts events from `accepted` and everything newer, starting now.
    #[must_use]
    pub const fn new(accepted: ConnectionEpoch) -> Self {
        Self {
            accepted,
            rejected: 0,
        }
    }

    /// The epoch currently accepted.
    #[must_use]
    pub const fn accepted(&self) -> ConnectionEpoch {
        self.accepted
    }

    /// Moves to a newer epoch (a successful reconnect).
    ///
    /// Returns `false` when `epoch` is not newer, in which case nothing changes:
    /// state must never travel backwards.
    pub fn advance_to(&mut self, epoch: ConnectionEpoch) -> bool {
        if epoch > self.accepted {
            self.accepted = epoch;
            true
        } else {
            false
        }
    }

    /// True when an event from `epoch` must be applied.
    pub fn accepts(&mut self, epoch: ConnectionEpoch) -> bool {
        if epoch == self.accepted {
            true
        } else {
            self.rejected += 1;
            false
        }
    }

    /// How many stale events were dropped.
    #[must_use]
    pub const fn rejected(&self) -> u64 {
        self.rejected
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
    fn a_reconnected_consumer_drops_the_old_connection() {
        let first = ConnectionEpoch::next();
        let second = ConnectionEpoch::next();
        let mut filter = EpochFilter::new(first);
        assert!(filter.accepts(first));
        assert!(filter.advance_to(second));
        assert!(!filter.accepts(first), "stale output must be refused");
        assert!(filter.accepts(second));
        assert_eq!(filter.rejected(), 1);
    }

    #[test]
    fn epochs_never_move_backwards() {
        let a = ConnectionEpoch::next();
        let b = ConnectionEpoch::next();
        let mut filter = EpochFilter::new(b);
        assert!(!filter.advance_to(a));
        assert_eq!(filter.accepted(), b);
    }
}
