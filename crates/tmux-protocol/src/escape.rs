//! Octal escaping used by `%output` and `%extended-output`.
//!
//! Pane output is transported as a byte string in which a few bytes are replaced
//! by a backslash followed by exactly three octal digits.  The decoded result is
//! **not** guaranteed to be valid UTF-8 and must never be converted to `String`
//! before it reaches the terminal emulator.

/// Why an escaped byte sequence could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscapeError {
    /// A backslash was followed by something that is neither an octal escape nor
    /// an escaped backslash.
    InvalidEscape {
        /// Offset inside the input slice.
        offset: usize,
    },
    /// An octal escape was truncated at the end of the input.
    TruncatedEscape {
        /// Offset inside the input slice.
        offset: usize,
    },
}

impl core::fmt::Display for EscapeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidEscape { offset } => {
                write!(f, "invalid escape sequence at offset {offset}")
            }
            Self::TruncatedEscape { offset } => {
                write!(f, "truncated octal escape at offset {offset}")
            }
        }
    }
}

impl std::error::Error for EscapeError {}

/// Decodes a `%output` payload, appending the raw bytes to `out`.
///
/// Accepted sequences:
/// * `\ooo` — exactly three octal digits, the documented form;
/// * `\\` — a literal backslash, tolerated because it is unambiguous;
/// * any other `\x` is rejected rather than silently mis-decoded.
///
/// Bytes that are not part of an escape are copied verbatim, including bytes
/// `>= 0x80`, so invalid UTF-8 survives the round trip.
pub fn decode_octal(src: &[u8], out: &mut Vec<u8>) -> Result<(), EscapeError> {
    let mut i = 0;
    while i < src.len() {
        let b = src[i];
        if b != b'\\' {
            out.push(b);
            i += 1;
            continue;
        }
        match src.get(i + 1) {
            None => return Err(EscapeError::TruncatedEscape { offset: i }),
            Some(b'\\') => {
                out.push(b'\\');
                i += 2;
            }
            Some(d0) if (b'0'..=b'7').contains(d0) => {
                let digits = src.get(i + 1..i + 4);
                let Some([d0, d1, d2]) = digits else {
                    return Err(EscapeError::TruncatedEscape { offset: i });
                };
                if !(b'0'..=b'7').contains(d1) || !(b'0'..=b'7').contains(d2) {
                    return Err(EscapeError::InvalidEscape { offset: i });
                }
                let value = (u16::from(*d0 - b'0') << 6)
                    | (u16::from(*d1 - b'0') << 3)
                    | u16::from(*d2 - b'0');
                // Three octal digits cover 0..=0777; only 0..=0377 is one byte.
                out.push(
                    u8::try_from(value).map_err(|_| EscapeError::InvalidEscape { offset: i })?,
                );
                i += 4;
            }
            Some(_) => return Err(EscapeError::InvalidEscape { offset: i }),
        }
    }
    Ok(())
}

/// Encodes bytes into the `%output` escaped form.
///
/// Escaped: backslash, space, `0x00..=0x1f` and `0x7f`.  Bytes `>= 0x80` are
/// passed through unchanged, matching the observed server behaviour recorded in
/// `tests/fixtures/manifest.json` (`attach_and_commands.07_cmd_binary_output`).
/// This function exists for round-trip tests and for synthetic fixtures; the
/// production path only ever decodes.
pub fn encode_octal(src: &[u8], out: &mut Vec<u8>) {
    for &b in src {
        if b == b'\\' || b == b' ' || b < 0x20 || b == 0x7f {
            out.push(b'\\');
            out.push(b'0' + ((b >> 6) & 0o7));
            out.push(b'0' + ((b >> 3) & 0o7));
            out.push(b'0' + (b & 0o7));
        } else {
            out.push(b);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(src: &[u8]) -> Result<Vec<u8>, EscapeError> {
        let mut out = Vec::new();
        decode_octal(src, &mut out)?;
        Ok(out)
    }

    #[test]
    fn plain_bytes_pass_through() {
        assert_eq!(dec(b"hello world").unwrap(), b"hello world");
    }

    #[test]
    fn decodes_octal_escapes() {
        assert_eq!(dec(b"A\\377\\376B").unwrap(), b"A\xff\xfeB");
        assert_eq!(dec(b"\\033[31mRED\\033[0m").unwrap(), b"\x1b[31mRED\x1b[0m");
        assert_eq!(dec(b"\\012").unwrap(), b"\n");
        assert_eq!(dec(b"\\000").unwrap(), b"\x00");
        assert_eq!(dec(b"\\177").unwrap(), b"\x7f");
    }

    #[test]
    fn high_bytes_are_not_escaped_by_the_encoder() {
        let mut out = Vec::new();
        encode_octal("中".as_bytes(), &mut out);
        assert_eq!(out, "中".as_bytes(), "UTF-8 bytes survive verbatim");
    }

    #[test]
    fn round_trips_every_byte_value() {
        let all: Vec<u8> = (0u8..=255).collect();
        let mut encoded = Vec::new();
        encode_octal(&all, &mut encoded);
        // escapes must not introduce raw control bytes or spaces
        assert!(!encoded.contains(&b' '));
        assert!(encoded
            .iter()
            .all(|b| *b >= 0x20 || *b == b'\\' || *b >= 0x80));
        assert_eq!(dec(&encoded).unwrap(), all);
    }

    #[test]
    fn rejects_malformed_escapes() {
        assert_eq!(dec(b"\\x"), Err(EscapeError::InvalidEscape { offset: 0 }));
        assert_eq!(dec(b"ab\\8"), Err(EscapeError::InvalidEscape { offset: 2 }));
        assert_eq!(
            dec(b"ab\\12"),
            Err(EscapeError::TruncatedEscape { offset: 2 })
        );
        assert_eq!(dec(b"\\"), Err(EscapeError::TruncatedEscape { offset: 0 }));
    }

    #[test]
    fn accepts_escaped_backslash() {
        assert_eq!(dec(b"a\\\\b").unwrap(), b"a\\b");
    }
}
