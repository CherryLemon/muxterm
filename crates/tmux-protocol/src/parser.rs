//! Streaming, byte-oriented parser for the tmux control-mode protocol.
//!
//! # Design rules (see `docs/spec/control-mode.md`)
//!
//! 1. **Bytes, not strings.**  `%output` payloads are octal-escaped and the
//!    decoded bytes are not guaranteed to be valid UTF-8.  Names and titles are
//!    only decoded when they are actually displayed.
//! 2. **Responses and notifications are different things.**  The server
//!    guarantees that it never emits a notification inside a command response
//!    block, so while a block is open every line — including lines starting with
//!    `%` — belongs to the block.  Conversely, the server's command number is
//!    *its* numbering; the client keeps its own (see [`crate::queue`]).
//! 3. **Never crash on remote input.**  Unknown verbs are surfaced as
//!    [`Event::Unknown`] for forward compatibility, and every bound is enforced
//!    with an explicit [`Event::Desync`] instead of unbounded buffering.
//! 4. **Recoverable.**  After a limit hit or a framing error the parser resynchronises
//!    at the next safe line boundary and keeps going: a desynchronised session
//!    must be reported and repairable, never silently "still refreshing".

use crate::escape::decode_octal;
use crate::ids::{PaneId, SessionId, WindowId};
use crate::limits::Limits;

/// The three numbers that frame a command response.
///
/// Wire form: `%begin <time> <number> <flags>`.  `time` is a server timestamp
/// (seconds), `number` is the server's own command counter and `flags` is a
/// bitmask (bit 0 set means the command produced output).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResponseHeader {
    /// Server timestamp in seconds.
    pub time: u64,
    /// Server-side command number of this block.
    pub number: u64,
    /// Server-side flags bitmask.
    pub flags: u64,
}

/// Why the byte stream stopped making sense.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Desync {
    /// A line exceeded [`Limits::max_line_bytes`] and was discarded.
    LineTooLong,
    /// A command response exceeded [`Limits::max_response_bytes`] or
    /// [`Limits::max_response_lines`] and the rest of the block was discarded.
    ResponseTooLarge,
    /// `%end`/`%error` arrived while no response block was open.
    UnmatchedResponseEnd,
    /// The block was closed with a different command number than it was opened with.
    ResponseNumberMismatch,
    /// A `%output` payload contained a malformed escape sequence.
    MalformedOutput,
    /// A notification was missing a mandatory argument or had a malformed one.
    MalformedNotification,
}

impl Desync {
    /// Short, stable, log-friendly description.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LineTooLong => "line-too-long",
            Self::ResponseTooLarge => "response-too-large",
            Self::UnmatchedResponseEnd => "unmatched-response-end",
            Self::ResponseNumberMismatch => "response-number-mismatch",
            Self::MalformedOutput => "malformed-output",
            Self::MalformedNotification => "malformed-notification",
        }
    }
}

/// Fatal parser configuration errors.
///
/// The parser itself is total: it reports [`Desync`] events instead of failing.
/// This type only exists for constructors that validate their arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// A limit was configured as zero, which would make the parser useless.
    ZeroLimit(&'static str),
}

impl core::fmt::Display for ParseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ZeroLimit(name) => write!(f, "limit {name} must not be zero"),
        }
    }
}

impl std::error::Error for ParseError {}

/// Everything the server can say, decoded.
///
/// Byte-valued fields stay as `Vec<u8>`: they are untrusted remote data and are
/// decoded to text only where the UI needs text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// `%begin` — a command response block starts.
    ResponseBegin(ResponseHeader),
    /// One output line inside the current response block (raw bytes, no newline).
    ResponseLine(Vec<u8>),
    /// `%end` — the current response block finished successfully.
    ResponseEnd(ResponseHeader),
    /// `%error` — the current response block finished with an error.
    ResponseError(ResponseHeader),
    /// `%output` — pane output.
    Output {
        /// Pane the bytes came from.
        pane: PaneId,
        /// Decoded pane bytes (arbitrary bytes, not necessarily UTF-8).
        data: Vec<u8>,
    },
    /// `%extended-output` — pane output with an age, used with pause/continue.
    ExtendedOutput {
        /// Pane the bytes came from.
        pane: PaneId,
        /// Age in milliseconds of the oldest byte in this chunk.
        age: u64,
        /// Decoded pane bytes.
        data: Vec<u8>,
    },
    /// `%pause` — the server stopped sending output for this pane.
    Pause {
        /// Pane whose output was paused.
        pane: PaneId,
    },
    /// `%continue` — the server resumed sending output for this pane.
    Continue {
        /// Pane whose output was resumed.
        pane: PaneId,
    },
    /// `%window-add`.
    WindowAdd {
        /// Newly linked window.
        window: WindowId,
    },
    /// `%window-close`.
    WindowClose {
        /// Window that disappeared.
        window: WindowId,
    },
    /// `%window-renamed`.
    WindowRenamed {
        /// Renamed window.
        window: WindowId,
        /// New name, untrusted bytes.
        name: Vec<u8>,
    },
    /// `%window-pane-changed` — the active pane of a window changed.
    WindowPaneChanged {
        /// Window whose active pane changed.
        window: WindowId,
        /// Newly active pane.
        pane: PaneId,
    },
    /// `%unlinked-window-add`.
    UnlinkedWindowAdd {
        /// Window that exists but is not linked to the current session.
        window: WindowId,
    },
    /// `%unlinked-window-close`.
    UnlinkedWindowClose {
        /// Window that disappeared.
        window: WindowId,
    },
    /// `%unlinked-window-renamed`.
    UnlinkedWindowRenamed {
        /// Renamed window.
        window: WindowId,
        /// New name, untrusted bytes.
        name: Vec<u8>,
    },
    /// `%unlinked-window-pane-changed`.
    UnlinkedWindowPaneChanged {
        /// Window whose active pane changed.
        window: WindowId,
        /// Newly active pane.
        pane: PaneId,
    },
    /// `%layout-change` — the layout of a window changed.
    LayoutChange {
        /// Affected window.
        window: WindowId,
        /// Full layout string.
        layout: Vec<u8>,
        /// Visible layout string.
        visible_layout: Vec<u8>,
        /// Trailing flags (`zoomed`, `window-size`, ...) exactly as sent.
        flags: Vec<Vec<u8>>,
    },
    /// `%session-changed` — this control client switched to another session.
    SessionChanged {
        /// Session now attached.
        session: SessionId,
        /// Session name, untrusted bytes.
        name: Vec<u8>,
    },
    /// `%session-renamed`.
    SessionRenamed {
        /// Renamed session.
        session: SessionId,
        /// New name, untrusted bytes.
        name: Vec<u8>,
    },
    /// `%session-window-changed` — the current window of a session changed.
    SessionWindowChanged {
        /// Session whose current window changed.
        session: SessionId,
        /// New current window.
        window: WindowId,
    },
    /// `%sessions-changed` — the set of sessions changed; a full refresh is needed.
    SessionsChanged,
    /// `%client-session-changed` — another client attached elsewhere.
    ClientSessionChanged {
        /// Client name, untrusted bytes.
        client: Vec<u8>,
        /// Session the client is attached to.
        session: SessionId,
        /// Session name, untrusted bytes.
        name: Vec<u8>,
    },
    /// `%client-detached`.
    ClientDetached {
        /// Client name, untrusted bytes.
        client: Vec<u8>,
    },
    /// `%pane-mode-changed` — a pane entered or left a tmux mode (copy mode, ...).
    PaneModeChanged {
        /// Affected pane.
        pane: PaneId,
        /// Mode name when the server reports one (newer tmux releases).
        mode: Option<Vec<u8>>,
    },
    /// `%paste-buffer-changed`.
    PasteBufferChanged {
        /// Buffer name, untrusted bytes.
        name: Vec<u8>,
    },
    /// `%paste-buffer-deleted`.
    PasteBufferDeleted {
        /// Buffer name, untrusted bytes.
        name: Vec<u8>,
    },
    /// `%subscription-changed` — a `refresh-client -B` subscription fired.
    SubscriptionChanged {
        /// Subscription name, untrusted bytes.
        name: Vec<u8>,
        /// Raw remainder after the name (identifiers, index and value).
        rest: Vec<u8>,
    },
    /// `%message` — a message forwarded from tmux.
    Message {
        /// Message text, untrusted bytes.
        text: Vec<u8>,
    },
    /// `%config-error` — tmux refused part of its configuration.
    ConfigError {
        /// Error text, untrusted bytes.
        text: Vec<u8>,
    },
    /// `%exit` — the control mode session ended (normally or abnormally).
    Exit {
        /// Optional reason; present when the server explained itself.
        reason: Option<Vec<u8>>,
    },
    /// A verb this implementation does not know (forward compatibility).
    Unknown {
        /// The `%verb` token.
        verb: Vec<u8>,
        /// Everything after the first space.
        rest: Vec<u8>,
    },
    /// A line outside a response block that does not start with `%`.
    ///
    /// In control mode this should not happen; seeing it means the byte stream
    /// is not (or no longer) control-mode protocol.
    Raw {
        /// The offending line.
        line: Vec<u8>,
    },
    /// The stream desynchronised; the client must resynchronise before trusting
    /// anything it renders.
    Desync {
        /// What went wrong.
        reason: Desync,
        /// Bytes discarded while recovering.
        dropped_bytes: u64,
    },
}

/// Consumer of parser events.
pub trait EventSink {
    /// Handles one event.
    fn event(&mut self, event: Event);
}

impl EventSink for Vec<Event> {
    fn event(&mut self, event: Event) {
        self.push(event);
    }
}

impl<F: FnMut(Event)> EventSink for F {
    fn event(&mut self, event: Event) {
        self(event);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Between response blocks.
    Outside,
    /// Inside a response block belonging to `header`.
    InResponse(ResponseHeader),
    /// Discarding the remainder of a response block after a limit was hit.
    Skipping(ResponseHeader),
    /// Discarding bytes until the next newline after an over-long line.
    SkipLine,
}

/// The protocol parser.  Feed it bytes; it emits [`Event`]s.
#[derive(Debug, Clone)]
pub struct Parser {
    limits: Limits,
    buf: Vec<u8>,
    /// Index into `buf` of the first unconsumed byte.
    scan: usize,
    state: State,
    response_bytes: u64,
    response_lines: u32,
    dropped_bytes: u64,
    /// Set once `%exit` has been seen; the stream must not be trusted afterwards.
    exited: bool,
}

impl Parser {
    /// Creates a parser with the given limits.
    #[must_use]
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            buf: Vec::new(),
            scan: 0,
            state: State::Outside,
            response_bytes: 0,
            response_lines: 0,
            dropped_bytes: 0,
            exited: false,
        }
    }

    /// Creates a parser with [`Limits::interactive`].
    #[must_use]
    pub fn with_default_limits() -> Self {
        Self::new(Limits::default())
    }

    /// True once `%exit` has been observed.
    #[must_use]
    pub const fn has_exited(&self) -> bool {
        self.exited
    }

    /// True while a command response block is open.
    #[must_use]
    pub const fn in_response(&self) -> bool {
        matches!(self.state, State::InResponse(_))
    }

    /// Bytes currently buffered because no line terminator was seen yet.
    #[must_use]
    pub fn buffered_bytes(&self) -> usize {
        self.buf.len() - self.scan
    }

    /// Feeds bytes and emits events.
    ///
    /// Calling this with a zero-length slice is allowed and emits nothing.
    pub fn feed(&mut self, input: &[u8], sink: &mut impl EventSink) {
        self.buf.extend_from_slice(input);
        loop {
            let Some(rel) = memchr(b'\n', &self.buf[self.scan..]) else {
                self.check_partial_line(sink);
                break;
            };
            let end = self.scan + rel;
            // A PTY with ONLCR translates every LF into CRLF, which is how `-CC`
            // reaches us (recorded in tests/fixtures/cc_entry.*).  The protocol's
            // terminator is LF, and a CR can never be part of a payload: `%output`
            // escapes every control byte.  So one CR before the LF is transport
            // translation, not data.
            let line_end = if end > self.scan && self.buf[end - 1] == b'\r' {
                end - 1
            } else {
                end
            };
            let line = self.buf[self.scan..line_end].to_vec();
            self.scan = end + 1;
            self.handle_line(&line, sink);
            if self.scan == self.buf.len() {
                break;
            }
        }
        self.compact();
    }

    /// Signals end of input (for example the transport closed).
    ///
    /// A partial trailing line is reported as [`Event::Raw`] with
    /// `complete = false` semantics: the caller learns that bytes were lost.
    pub fn finish(&mut self, sink: &mut impl EventSink) {
        if self.buffered_bytes() > 0 {
            let line = self.buf[self.scan..].to_vec();
            self.scan = self.buf.len();
            sink.event(Event::Raw { line });
        }
        self.compact();
    }

    fn compact(&mut self) {
        if self.scan == self.buf.len() {
            self.buf.clear();
            self.scan = 0;
        } else if self.scan > 64 * 1024 {
            self.buf.drain(..self.scan);
            self.scan = 0;
        }
    }

    fn check_partial_line(&mut self, sink: &mut impl EventSink) {
        if self.buffered_bytes() <= self.limits.max_line_bytes {
            return;
        }
        let dropped = self.buffered_bytes() as u64;
        self.dropped_bytes += dropped;
        self.buf.clear();
        self.scan = 0;
        // Inside a response block we must drop the whole block, not just the line:
        // its framing is no longer trustworthy.
        self.state = match self.state {
            State::InResponse(header) | State::Skipping(header) => State::Skipping(header),
            _ => State::SkipLine,
        };
        sink.event(Event::Desync {
            reason: Desync::LineTooLong,
            dropped_bytes: dropped,
        });
    }

    fn handle_line(&mut self, line: &[u8], sink: &mut impl EventSink) {
        match self.state {
            State::SkipLine => {
                self.state = State::Outside;
                self.dropped_bytes += line.len() as u64;
            }
            State::Skipping(header) => {
                self.dropped_bytes += line.len() as u64;
                if let Some((is_error, parsed)) = parse_frame_end(line) {
                    if parsed.number == header.number {
                        self.state = State::Outside;
                        self.reset_response_counters();
                        sink.event(if is_error {
                            Event::ResponseError(header)
                        } else {
                            Event::ResponseEnd(header)
                        });
                    }
                }
            }
            State::InResponse(header) => {
                if let Some((is_error, parsed)) = parse_frame_end(line) {
                    if parsed.number == header.number {
                        self.state = State::Outside;
                        self.reset_response_counters();
                        sink.event(if is_error {
                            Event::ResponseError(header)
                        } else {
                            Event::ResponseEnd(header)
                        });
                    } else {
                        self.end_block_with_desync(header, Desync::ResponseNumberMismatch, sink);
                    }
                    return;
                }
                self.response_bytes += line.len() as u64 + 1;
                self.response_lines += 1;
                if self.response_bytes > self.limits.max_response_bytes
                    || self.response_lines > self.limits.max_response_lines
                {
                    let dropped = self.response_bytes;
                    self.dropped_bytes += dropped;
                    self.state = State::Skipping(header);
                    sink.event(Event::Desync {
                        reason: Desync::ResponseTooLarge,
                        dropped_bytes: dropped,
                    });
                    return;
                }
                sink.event(Event::ResponseLine(line.to_vec()));
            }
            State::Outside => self.handle_notification(line, sink),
        }
    }

    fn end_block_with_desync(
        &mut self,
        header: ResponseHeader,
        reason: Desync,
        sink: &mut impl EventSink,
    ) {
        // The block we thought was open has ended in an unexpected way.  Close it
        // so the queue can settle the request, and let the client resynchronise.
        self.state = State::Outside;
        self.reset_response_counters();
        sink.event(Event::Desync {
            reason,
            dropped_bytes: 0,
        });
        sink.event(Event::ResponseError(header));
    }

    fn reset_response_counters(&mut self) {
        self.response_bytes = 0;
        self.response_lines = 0;
    }

    fn handle_notification(&mut self, line: &[u8], sink: &mut impl EventSink) {
        if !line.starts_with(b"%") {
            sink.event(Event::Raw {
                line: line.to_vec(),
            });
            return;
        }
        let (verb, rest) = split_verb(line);
        match verb {
            b"%begin" => match parse_header(rest) {
                Some(header) => {
                    self.response_bytes = 0;
                    self.response_lines = 0;
                    self.state = State::InResponse(header);
                    sink.event(Event::ResponseBegin(header));
                }
                None => self.malformed(sink),
            },
            b"%end" | b"%error" => {
                // A frame end without a matching begin: report and ignore.
                sink.event(Event::Desync {
                    reason: Desync::UnmatchedResponseEnd,
                    dropped_bytes: line.len() as u64,
                });
            }
            b"%output" => self.notification_output(rest, sink),
            b"%extended-output" => self.notification_extended_output(rest, sink),
            b"%pause" | b"%continue" => {
                let is_pause = verb == b"%pause";
                match PaneId::from_wire(rest) {
                    Some(pane) if is_pause => sink.event(Event::Pause { pane }),
                    Some(pane) => sink.event(Event::Continue { pane }),
                    None => self.malformed(sink),
                }
            }
            b"%exit" => {
                self.exited = true;
                let reason = (!rest.is_empty()).then(|| rest.to_vec());
                sink.event(Event::Exit { reason });
            }
            b"%window-add" | b"%unlinked-window-add" => {
                let unlinked = verb == b"%unlinked-window-add";
                match WindowId::from_wire(rest) {
                    Some(window) if unlinked => sink.event(Event::UnlinkedWindowAdd { window }),
                    Some(window) => sink.event(Event::WindowAdd { window }),
                    None => self.malformed(sink),
                }
            }
            b"%window-close" | b"%unlinked-window-close" => {
                let unlinked = verb == b"%unlinked-window-close";
                match WindowId::from_wire(rest) {
                    Some(window) if unlinked => sink.event(Event::UnlinkedWindowClose { window }),
                    Some(window) => sink.event(Event::WindowClose { window }),
                    None => self.malformed(sink),
                }
            }
            b"%window-renamed" | b"%unlinked-window-renamed" => {
                let unlinked = verb == b"%unlinked-window-renamed";
                let (id, name) = split_first_token(rest);
                match WindowId::from_wire(id) {
                    Some(window) if unlinked => sink.event(Event::UnlinkedWindowRenamed {
                        window,
                        name: name.to_vec(),
                    }),
                    Some(window) => sink.event(Event::WindowRenamed {
                        window,
                        name: name.to_vec(),
                    }),
                    None => self.malformed(sink),
                }
            }
            b"%window-pane-changed" | b"%unlinked-window-pane-changed" => {
                let unlinked = verb == b"%unlinked-window-pane-changed";
                let (id, pane) = split_first_token(rest);
                match (WindowId::from_wire(id), PaneId::from_wire(pane)) {
                    (Some(window), Some(pane)) if unlinked => {
                        sink.event(Event::UnlinkedWindowPaneChanged { window, pane });
                    }
                    (Some(window), Some(pane)) => {
                        sink.event(Event::WindowPaneChanged { window, pane });
                    }
                    _ => self.malformed(sink),
                }
            }
            b"%layout-change" => {
                let (id, rest) = split_first_token(rest);
                let (layout, rest) = split_first_token(rest);
                let (visible, rest) = split_first_token(rest);
                match WindowId::from_wire(id) {
                    Some(window) => sink.event(Event::LayoutChange {
                        window,
                        layout: layout.to_vec(),
                        visible_layout: visible.to_vec(),
                        flags: rest
                            .split(|b| *b == b' ')
                            .filter(|t| !t.is_empty())
                            .map(<[u8]>::to_vec)
                            .collect(),
                    }),
                    None => self.malformed(sink),
                }
            }
            b"%session-changed" | b"%session-renamed" => {
                let renamed = verb == b"%session-renamed";
                let (id, name) = split_first_token(rest);
                match SessionId::from_wire(id) {
                    Some(session) if renamed => sink.event(Event::SessionRenamed {
                        session,
                        name: name.to_vec(),
                    }),
                    Some(session) => sink.event(Event::SessionChanged {
                        session,
                        name: name.to_vec(),
                    }),
                    None => self.malformed(sink),
                }
            }
            b"%session-window-changed" => {
                let (session, window) = split_first_token(rest);
                match (SessionId::from_wire(session), WindowId::from_wire(window)) {
                    (Some(session), Some(window)) => {
                        sink.event(Event::SessionWindowChanged { session, window });
                    }
                    _ => self.malformed(sink),
                }
            }
            b"%sessions-changed" => sink.event(Event::SessionsChanged),
            b"%client-session-changed" => {
                let (client, rest) = split_first_token(rest);
                let (session, name) = split_first_token(rest);
                match SessionId::from_wire(session) {
                    Some(session) => sink.event(Event::ClientSessionChanged {
                        client: client.to_vec(),
                        session,
                        name: name.to_vec(),
                    }),
                    None => self.malformed(sink),
                }
            }
            b"%client-detached" => sink.event(Event::ClientDetached {
                client: rest.to_vec(),
            }),
            b"%pane-mode-changed" => {
                let (id, mode) = split_first_token(rest);
                match PaneId::from_wire(id) {
                    Some(pane) => sink.event(Event::PaneModeChanged {
                        pane,
                        mode: (!mode.is_empty()).then(|| mode.to_vec()),
                    }),
                    None => self.malformed(sink),
                }
            }
            b"%paste-buffer-changed" => sink.event(Event::PasteBufferChanged {
                name: rest.to_vec(),
            }),
            b"%paste-buffer-deleted" => sink.event(Event::PasteBufferDeleted {
                name: rest.to_vec(),
            }),
            b"%subscription-changed" => {
                let (name, rest) = split_first_token(rest);
                sink.event(Event::SubscriptionChanged {
                    name: name.to_vec(),
                    rest: rest.to_vec(),
                });
            }
            b"%message" => sink.event(Event::Message {
                text: rest.to_vec(),
            }),
            b"%config-error" => sink.event(Event::ConfigError {
                text: rest.to_vec(),
            }),
            _ => sink.event(Event::Unknown {
                verb: verb.to_vec(),
                rest: rest.to_vec(),
            }),
        }
    }

    fn notification_output(&mut self, rest: &[u8], sink: &mut impl EventSink) {
        let (id, payload) = split_first_token(rest);
        let Some(pane) = PaneId::from_wire(id) else {
            sink.event(Event::Desync {
                reason: Desync::MalformedNotification,
                dropped_bytes: 0,
            });
            return;
        };
        let mut data = Vec::with_capacity(payload.len());
        if decode_octal(payload, &mut data).is_err() {
            sink.event(Event::Desync {
                reason: Desync::MalformedOutput,
                dropped_bytes: payload.len() as u64,
            });
            return;
        }
        sink.event(Event::Output { pane, data });
    }

    fn notification_extended_output(&mut self, rest: &[u8], sink: &mut impl EventSink) {
        // `%extended-output %<pane> <age> : <escaped data>`
        let (id, rest) = split_first_token(rest);
        let (age, rest) = split_first_token(rest);
        let Some(pane) = PaneId::from_wire(id) else {
            self.malformed(sink);
            return;
        };
        let Some(age) = parse_u64(age) else {
            self.malformed(sink);
            return;
        };
        let payload = rest
            .strip_prefix(b":")
            .map_or(rest, |r| r.strip_prefix(b" ").unwrap_or(r));
        let mut data = Vec::with_capacity(payload.len());
        if decode_octal(payload, &mut data).is_err() {
            sink.event(Event::Desync {
                reason: Desync::MalformedOutput,
                dropped_bytes: payload.len() as u64,
            });
            return;
        }
        sink.event(Event::ExtendedOutput { pane, age, data });
    }

    fn malformed(&mut self, sink: &mut impl EventSink) {
        sink.event(Event::Desync {
            reason: Desync::MalformedNotification,
            dropped_bytes: 0,
        });
    }
}

fn memchr(needle: u8, haystack: &[u8]) -> Option<usize> {
    haystack.iter().position(|b| *b == needle)
}

fn split_verb(line: &[u8]) -> (&[u8], &[u8]) {
    match memchr(b' ', line) {
        Some(i) => (&line[..i], &line[i + 1..]),
        None => (line, &[]),
    }
}

/// Splits off the first space-delimited token; the remainder keeps its spacing.
fn split_first_token(input: &[u8]) -> (&[u8], &[u8]) {
    match memchr(b' ', input) {
        Some(i) => (&input[..i], &input[i + 1..]),
        None => (input, &[]),
    }
}

fn parse_frame_end(line: &[u8]) -> Option<(bool, ResponseHeader)> {
    let (verb, rest) = split_verb(line);
    let is_error = match verb {
        b"%end" => false,
        b"%error" => true,
        _ => return None,
    };
    // The number must match even if the timestamp or flags are unexpected, so a
    // partially malformed frame end still closes the block it belongs to.
    let mut fields = rest.split(|b| *b == b' ').filter(|f| !f.is_empty());
    let time = fields.next().and_then(parse_u64).unwrap_or(0);
    let number = fields.next().and_then(parse_u64)?;
    let flags = fields.next().and_then(parse_u64).unwrap_or(0);
    Some((
        is_error,
        ResponseHeader {
            time,
            number,
            flags,
        },
    ))
}

fn parse_header(rest: &[u8]) -> Option<ResponseHeader> {
    let mut fields = rest.split(|b| *b == b' ').filter(|f| !f.is_empty());
    let time = parse_u64(fields.next()?)?;
    let number = parse_u64(fields.next()?)?;
    let flags = fields.next().and_then(parse_u64).unwrap_or(0);
    Some(ResponseHeader {
        time,
        number,
        flags,
    })
}

fn parse_u64(bytes: &[u8]) -> Option<u64> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut value: u64 = 0;
    for b in bytes {
        value = value.checked_mul(10)?.checked_add(u64::from(*b - b'0'))?;
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_all(input: &[u8]) -> Vec<Event> {
        let mut parser = Parser::with_default_limits();
        let mut events = Vec::new();
        parser.feed(input, &mut events);
        events
    }

    #[test]
    fn crlf_terminators_are_normalised() {
        // -CC on a PTY delivers CRLF; the value must come back without the CR.
        let events = parse_all(b"%begin 1 7 0\r\nvalue\r\n%end 1 7 0\r\n%exit\r\n");
        assert_eq!(
            events,
            vec![
                Event::ResponseBegin(ResponseHeader {
                    time: 1,
                    number: 7,
                    flags: 0
                }),
                Event::ResponseLine(b"value".to_vec()),
                Event::ResponseEnd(ResponseHeader {
                    time: 1,
                    number: 7,
                    flags: 0
                }),
                Event::Exit { reason: None },
            ]
        );
    }

    #[test]
    fn a_genuine_carriage_return_in_content_is_kept() {
        // A pane line that really ends with CR arrives as CR CR LF: only the last
        // one is the transport's translation.
        let events = parse_all(b"%begin 1 1 0\r\nA\r\r\n%end 1 1 0\r\n");
        match &events[1] {
            Event::ResponseLine(line) => assert_eq!(line, b"A\r"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parses_a_command_response() {
        let events = parse_all(b"%begin 1 7 0\nline one\nline two\n%end 1 7 0\n");
        assert_eq!(
            events,
            vec![
                Event::ResponseBegin(ResponseHeader {
                    time: 1,
                    number: 7,
                    flags: 0
                }),
                Event::ResponseLine(b"line one".to_vec()),
                Event::ResponseLine(b"line two".to_vec()),
                Event::ResponseEnd(ResponseHeader {
                    time: 1,
                    number: 7,
                    flags: 0
                }),
            ]
        );
    }

    #[test]
    fn percent_lines_inside_a_block_are_output_not_events() {
        // Documented guarantee: the server never injects notifications inside a
        // response block.  A line that merely looks like one is payload.
        let events = parse_all(b"%begin 5 1 1\n%output %0 hi\n%window-add @9\n%end 5 1 1\n");
        assert_eq!(
            events,
            vec![
                Event::ResponseBegin(ResponseHeader {
                    time: 5,
                    number: 1,
                    flags: 1
                }),
                Event::ResponseLine(b"%output %0 hi".to_vec()),
                Event::ResponseLine(b"%window-add @9".to_vec()),
                Event::ResponseEnd(ResponseHeader {
                    time: 5,
                    number: 1,
                    flags: 1
                }),
            ]
        );
    }

    #[test]
    fn parses_error_blocks() {
        let events = parse_all(b"%begin 1 2 0\nunknown command\n%error 1 2 0\n");
        assert!(matches!(events.last(), Some(Event::ResponseError(_))));
    }

    #[test]
    fn parses_output_with_octal_escapes() {
        let events = parse_all(b"%output %3 A\\377B\\033[0m\n");
        assert_eq!(
            events,
            vec![Event::Output {
                pane: PaneId::new(3),
                data: b"A\xffB\x1b[0m".to_vec()
            }]
        );
    }

    #[test]
    fn output_can_carry_invalid_utf8() {
        let events = parse_all(b"%output %0 \\376\\377\n");
        match &events[0] {
            Event::Output { data, .. } => {
                assert_eq!(data, &[0xfe, 0xff]);
                assert!(String::from_utf8(data.clone()).is_err());
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn malformed_output_is_reported_not_guessed() {
        let events = parse_all(b"%output %0 bad\\qescape\n");
        assert!(matches!(
            events[0],
            Event::Desync {
                reason: Desync::MalformedOutput,
                ..
            }
        ));
    }

    #[test]
    fn parses_structural_notifications() {
        let events = parse_all(
            b"%window-add @1\n\
              %window-renamed @1 my window\n\
              %layout-change @1 bb62,80x24,0,0[80x12,0,0,0,80x11,0,13,1] bb62,80x24,0,0[80x24,0,0,0] *\n\
              %window-close @1\n",
        );
        assert_eq!(
            events,
            vec![
                Event::WindowAdd {
                    window: WindowId::new(1)
                },
                Event::WindowRenamed {
                    window: WindowId::new(1),
                    name: b"my window".to_vec()
                },
                Event::LayoutChange {
                    window: WindowId::new(1),
                    layout: b"bb62,80x24,0,0[80x12,0,0,0,80x11,0,13,1]".to_vec(),
                    visible_layout: b"bb62,80x24,0,0[80x24,0,0,0]".to_vec(),
                    flags: vec![b"*".to_vec()],
                },
                Event::WindowClose {
                    window: WindowId::new(1)
                },
            ]
        );
    }

    #[test]
    fn parses_unknown_verbs_for_forward_compatibility() {
        let events = parse_all(b"%future-thing @1 whatever\n");
        assert_eq!(
            events,
            vec![Event::Unknown {
                verb: b"%future-thing".to_vec(),
                rest: b"@1 whatever".to_vec()
            }]
        );
    }

    #[test]
    fn raw_lines_are_surfaced() {
        let events = parse_all(b"this is not protocol\n");
        assert_eq!(
            events,
            vec![Event::Raw {
                line: b"this is not protocol".to_vec()
            }]
        );
    }

    #[test]
    fn unmatched_frame_end_desynchronises() {
        let events = parse_all(b"%end 1 4 0\n");
        assert!(matches!(
            events[0],
            Event::Desync {
                reason: Desync::UnmatchedResponseEnd,
                ..
            }
        ));
    }

    #[test]
    fn mismatched_block_end_closes_the_block() {
        let events = parse_all(b"%begin 1 4 0\nbody\n%end 1 5 0\n");
        assert!(events.iter().any(|e| matches!(
            e,
            Event::Desync {
                reason: Desync::ResponseNumberMismatch,
                ..
            }
        )));
        assert!(matches!(events.last(), Some(Event::ResponseError(_))));
        let mut parser = Parser::with_default_limits();
        let mut sink = Vec::new();
        parser.feed(
            b"%begin 1 4 0\nbody\n%end 1 5 0\n%window-add @2\n",
            &mut sink,
        );
        assert!(sink.contains(&Event::WindowAdd {
            window: WindowId::new(2)
        }));
    }

    #[test]
    fn exit_is_terminal_and_carries_a_reason() {
        let events = parse_all(b"%exit\nexiting\n");
        assert_eq!(events[0], Event::Exit { reason: None });
        let events = parse_all(b"%exit server exited\n");
        assert_eq!(
            events[0],
            Event::Exit {
                reason: Some(b"server exited".to_vec())
            }
        );
        let mut parser = Parser::with_default_limits();
        let mut sink = Vec::new();
        parser.feed(b"%exit\n", &mut sink);
        assert!(parser.has_exited());
    }

    #[test]
    fn overlong_line_is_bounded_and_reported() {
        let limits = Limits {
            max_line_bytes: 32,
            ..Limits::strict()
        };
        let mut parser = Parser::new(limits);
        let mut events = Vec::new();
        parser.feed(&vec![b'x'; 1024], &mut events);
        assert!(matches!(
            events[0],
            Event::Desync {
                reason: Desync::LineTooLong,
                ..
            }
        ));
        // after the newline the parser is usable again
        events.clear();
        parser.feed(b"\n%window-add @4\n", &mut events);
        assert_eq!(
            events,
            vec![Event::WindowAdd {
                window: WindowId::new(4)
            }]
        );
    }

    #[test]
    fn oversized_response_is_truncated_and_closed() {
        let limits = Limits {
            max_response_lines: 2,
            ..Limits::strict()
        };
        let mut parser = Parser::new(limits);
        let mut events = Vec::new();
        parser.feed(
            b"%begin 1 1 0\na\nb\nc\nd\n%end 1 1 0\n%window-add @8\n",
            &mut events,
        );
        assert!(events.iter().any(|e| matches!(
            e,
            Event::Desync {
                reason: Desync::ResponseTooLarge,
                ..
            }
        )));
        assert!(
            events.iter().any(|e| matches!(e, Event::ResponseEnd(_))),
            "the block must still be closed so the queue can settle"
        );
        assert!(
            events.contains(&Event::WindowAdd {
                window: WindowId::new(8)
            }),
            "parsing resumes after resynchronisation"
        );
    }

    #[test]
    fn in_response_flag_tracks_state() {
        let mut parser = Parser::with_default_limits();
        let mut events = Vec::new();
        parser.feed(b"%begin 1 1 0\n", &mut events);
        assert!(parser.in_response());
        parser.feed(b"%end 1 1 0\n", &mut events);
        assert!(!parser.in_response());
    }

    #[test]
    fn partial_line_is_buffered_then_completed() {
        let mut parser = Parser::with_default_limits();
        let mut events = Vec::new();
        parser.feed(b"%window-ad", &mut events);
        assert!(events.is_empty());
        assert_eq!(parser.buffered_bytes(), 10);
        parser.feed(b"d @5\n", &mut events);
        assert_eq!(
            events,
            vec![Event::WindowAdd {
                window: WindowId::new(5)
            }]
        );
        assert_eq!(parser.buffered_bytes(), 0);
    }

    #[test]
    fn finish_reports_a_dangling_line() {
        let mut parser = Parser::with_default_limits();
        let mut events = Vec::new();
        parser.feed(b"%window-ad", &mut events);
        parser.finish(&mut events);
        assert_eq!(
            events,
            vec![Event::Raw {
                line: b"%window-ad".to_vec()
            }]
        );
    }
}
