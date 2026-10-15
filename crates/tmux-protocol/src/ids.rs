//! Typed identifiers for tmux objects.
//!
//! The wire form uses a sigil: `%12` pane, `@3` window, `$0` session.  Parsing
//! is byte-oriented because these strings arrive from an untrusted remote.

use std::fmt;

macro_rules! id_type {
    ($name:ident, $sigil:literal, $sigil_byte:literal, $doc:expr) => {
        #[doc = $doc]
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u32);

        impl $name {
            /// Wraps the numeric part of the identifier.
            #[must_use]
            pub const fn new(raw: u32) -> Self {
                Self(raw)
            }

            /// The numeric part of the identifier.
            #[must_use]
            pub const fn as_u32(self) -> u32 {
                self.0
            }

            /// The sigil used in the wire form.
            #[must_use]
            pub const fn sigil() -> char {
                $sigil
            }

            /// Parses the wire form, for example `%12`.
            ///
            /// Returns `None` when the sigil is missing or the number is absent,
            /// not a valid decimal, or does not fit in 32 bits.
            #[must_use]
            pub fn from_wire(bytes: &[u8]) -> Option<Self> {
                let (first, rest) = bytes.split_first()?;
                if *first != $sigil_byte || rest.is_empty() {
                    return None;
                }
                let mut value: u32 = 0;
                for b in rest {
                    if !b.is_ascii_digit() {
                        return None;
                    }
                    value = value.checked_mul(10)?.checked_add(u32::from(*b - b'0'))?;
                }
                Some(Self(value))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}{}", $sigil, self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}{}", $sigil, self.0)
            }
        }
    };
}

id_type!(
    PaneId,
    '%',
    b'%',
    "Identifier of a tmux pane, wire form `%12`."
);
id_type!(
    WindowId,
    '@',
    b'@',
    "Identifier of a tmux window, wire form `@3`."
);
id_type!(
    SessionId,
    '$',
    b'$',
    "Identifier of a tmux session, wire form `$0`."
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wire_forms() {
        assert_eq!(PaneId::from_wire(b"%12"), Some(PaneId::new(12)));
        assert_eq!(WindowId::from_wire(b"@0"), Some(WindowId::new(0)));
        assert_eq!(SessionId::from_wire(b"$7"), Some(SessionId::new(7)));
    }

    #[test]
    fn rejects_bad_wire_forms() {
        assert_eq!(PaneId::from_wire(b"@12"), None, "wrong sigil");
        assert_eq!(PaneId::from_wire(b"%"), None, "no digits");
        assert_eq!(PaneId::from_wire(b"%1a"), None, "trailing garbage");
        assert_eq!(PaneId::from_wire(b"%-1"), None, "sign");
        assert_eq!(PaneId::from_wire(b""), None, "empty");
        // overflow must not wrap
        assert_eq!(PaneId::from_wire(b"%99999999999999"), None);
    }

    #[test]
    fn display_round_trips() {
        for raw in [0u32, 1, 42, u32::MAX] {
            let id = PaneId::new(raw);
            assert_eq!(PaneId::from_wire(id.to_string().as_bytes()), Some(id));
        }
    }

    #[test]
    fn overlong_but_valid_numbers_are_kept() {
        assert_eq!(PaneId::from_wire(b"%007"), Some(PaneId::new(7)));
    }
}
