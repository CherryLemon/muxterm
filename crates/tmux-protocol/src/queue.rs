//! Command scheduling and response matching.
//!
//! The server numbers command responses with *its own* counter (`%begin <time>
//! <number> <flags>`), and it also answers commands the client never sent — the
//! ones tmux issues itself while attaching.  Two rules follow:
//!
//! * the client never trusts the server's numbering for its own bookkeeping; it
//!   matches responses to requests in order, which the protocol guarantees;
//! * responses that arrive while nothing is in flight are *unsolicited* and are
//!   surfaced separately, so an initialisation reply can never be mistaken for
//!   the answer to the first user action.
//!
//! Writing stops permanently once the queue is closed (`%exit`, transport error),
//! which is what keeps a late command from leaking into a shell that has taken
//! the terminal back.

use std::collections::VecDeque;

use crate::command::{Command, CommandError};
use crate::limits::Limits;
use crate::parser::{Event, ResponseHeader};

/// Client-side identifier of a scheduled command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestId(u64);

impl RequestId {
    /// The raw counter value.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl core::fmt::Display for RequestId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "req#{}", self.0)
    }
}

/// A completed command response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// The request this answers, or `None` for an unsolicited server response.
    pub request: Option<RequestId>,
    /// Framing numbers as sent by the server.
    pub header: ResponseHeader,
    /// Response lines, raw bytes without terminators.
    pub lines: Vec<Vec<u8>>,
    /// False when the block ended with `%error` or was cut short.
    pub ok: bool,
    /// True when a limit truncated the body: the content is unreliable and the
    /// session needs resynchronising.
    pub truncated: bool,
    /// True when the queue was closed (for example `%exit`) before the answer came.
    pub closed_by_exit: bool,
}

impl Response {
    /// The first response line, which is the value for most queries.
    #[must_use]
    pub fn first_line(&self) -> Option<&[u8]> {
        self.lines.first().map(Vec::as_slice)
    }

    /// The first line decoded lossily — for logs and diagnostics, never for
    /// pane content.
    #[must_use]
    pub fn first_line_lossy(&self) -> Option<String> {
        self.first_line()
            .map(|line| String::from_utf8_lossy(line).into_owned())
    }

    /// All lines decoded lossily.
    #[must_use]
    pub fn lines_lossy(&self) -> Vec<String> {
        self.lines
            .iter()
            .map(|line| String::from_utf8_lossy(line).into_owned())
            .collect()
    }

    /// True when the response can be trusted as the answer to `request`.
    #[must_use]
    pub fn is_complete_for(&self, request: RequestId) -> bool {
        self.request == Some(request) && self.ok && !self.truncated && !self.closed_by_exit
    }
}

/// Why a command could not be scheduled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueError {
    /// The queue is closed: `%exit` was seen or the transport failed.
    Closed,
    /// The pending queue is full; the caller must wait for responses.
    PendingFull {
        /// Configured bound.
        limit: usize,
    },
    /// The command could not be encoded.
    Encoding(CommandError),
}

impl core::fmt::Display for QueueError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Closed => write!(f, "command queue is closed"),
            Self::PendingFull { limit } => write!(f, "command queue is full ({limit} pending)"),
            Self::Encoding(err) => write!(f, "command encoding failed: {err}"),
        }
    }
}

impl std::error::Error for QueueError {}

impl From<CommandError> for QueueError {
    fn from(err: CommandError) -> Self {
        Self::Encoding(err)
    }
}

/// A synchronisation barrier: no other command is written until this request is
/// answered, and an answer that does not carry the expected marker is treated as
/// server-initiated output instead of being matched to the request.
#[derive(Debug, Clone)]
struct Barrier {
    request: RequestId,
    expected_first_line: Vec<u8>,
}

#[derive(Debug)]
struct OpenResponse {
    request: Option<RequestId>,
    header: ResponseHeader,
    lines: Vec<Vec<u8>>,
    truncated: bool,
    ok: bool,
}

/// Schedules commands, tracks what is in flight and assembles responses.
#[derive(Debug)]
pub struct CommandQueue {
    limits: Limits,
    next_request: u64,
    pending: VecDeque<(RequestId, Vec<u8>)>,
    in_flight: VecDeque<RequestId>,
    open: Option<OpenResponse>,
    unsolicited: Vec<Response>,
    barrier: Option<Barrier>,
    closed: bool,
}

impl CommandQueue {
    /// Creates a queue with the given limits.
    #[must_use]
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            next_request: 1,
            pending: VecDeque::new(),
            in_flight: VecDeque::new(),
            open: None,
            unsolicited: Vec::new(),
            barrier: None,
            closed: false,
        }
    }

    /// Encodes and schedules a command.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::Closed`] after the session ended and
    /// [`QueueError::PendingFull`] when too many commands are waiting.
    pub fn enqueue(&mut self, command: &Command) -> Result<RequestId, QueueError> {
        if self.closed {
            return Err(QueueError::Closed);
        }
        let limit = self
            .limits
            .max_pending_commands
            .max(self.limits.max_in_flight_commands);
        if self.pending.len() >= limit {
            return Err(QueueError::PendingFull { limit });
        }
        let bytes = command.encode(self.limits)?;
        self.enqueue_encoded(bytes)
    }

    /// Schedules an already encoded command line.
    ///
    /// Used where the encoder produced several lines at once (`send-keys` splits
    /// heterogeneous input into homogeneous runs).
    ///
    /// # Errors
    ///
    /// As [`CommandQueue::enqueue`].
    pub fn enqueue_encoded(&mut self, bytes: Vec<u8>) -> Result<RequestId, QueueError> {
        if self.closed {
            return Err(QueueError::Closed);
        }
        let limit = self
            .limits
            .max_pending_commands
            .max(self.limits.max_in_flight_commands);
        if self.pending.len() >= limit {
            return Err(QueueError::PendingFull { limit });
        }
        let id = RequestId(self.next_request);
        self.next_request += 1;
        self.pending.push_back((id, bytes));
        Ok(id)
    }

    /// Marks `id` as a synchronisation barrier.
    ///
    /// Commands queued after the barrier are not released until the barrier's
    /// response has been seen *and recognised*: a response block whose first line
    /// is not `expected_first_line` is handed back as an unsolicited block, because
    /// it must be one of the answers the server produced for the commands it issued
    /// itself while attaching.  That is what keeps an initialisation reply from
    /// being mistaken for the answer to the first real command.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::Closed`] if the queue is closed.
    pub fn set_barrier(
        &mut self,
        id: RequestId,
        expected_first_line: impl Into<Vec<u8>>,
    ) -> Result<(), QueueError> {
        if self.closed {
            return Err(QueueError::Closed);
        }
        self.barrier = Some(Barrier {
            request: id,
            expected_first_line: expected_first_line.into(),
        });
        Ok(())
    }

    /// The barrier request, if one is still outstanding.
    #[must_use]
    pub fn barrier(&self) -> Option<RequestId> {
        self.barrier.as_ref().map(|barrier| barrier.request)
    }

    /// Number of commands awaiting a response or a write.
    #[must_use]
    pub fn outstanding(&self) -> usize {
        self.pending.len() + self.in_flight.len()
    }

    /// Number of commands already written and awaiting a response.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    /// True while a response block is being assembled.
    #[must_use]
    pub const fn has_open_response(&self) -> bool {
        self.open.is_some()
    }

    /// True once the queue is closed and will never write again.
    #[must_use]
    pub const fn is_closed(&self) -> bool {
        self.closed
    }

    /// The next command to write, if the in-flight window allows it.
    ///
    /// The returned bytes include the trailing newline.
    pub fn next_to_write(&mut self) -> Option<(RequestId, Vec<u8>)> {
        if self.closed || self.in_flight.len() >= self.limits.max_in_flight_commands {
            return None;
        }
        let (front_id, _) = self.pending.front()?;
        if let Some(barrier) = self.barrier.as_ref() {
            // The barrier is enqueued before anything it guards, so it is either
            // still at the front (let it through) or already written and awaiting
            // its response (hold everything back).
            if *front_id != barrier.request {
                return None;
            }
        }
        let (id, bytes) = self.pending.pop_front()?;
        self.in_flight.push_back(id);
        Some((id, bytes))
    }

    /// Feeds a parser event and returns a response when one completes.
    pub fn on_event(&mut self, event: &Event) -> Option<Response> {
        match event {
            Event::ResponseBegin(header) => {
                let request = self.in_flight.pop_front();
                self.open = Some(OpenResponse {
                    request,
                    header: *header,
                    lines: Vec::new(),
                    truncated: false,
                    ok: true,
                });
                None
            }
            Event::ResponseLine(line) => {
                if let Some(open) = self.open.as_mut() {
                    open.lines.push(line.clone());
                }
                None
            }
            Event::ResponseEnd(header) => self.finish(true, *header),
            Event::ResponseError(header) => self.finish(false, *header),
            Event::Desync {
                reason: crate::parser::Desync::ResponseTooLarge,
                ..
            } => {
                if let Some(open) = self.open.as_mut() {
                    open.truncated = true;
                }
                None
            }
            _ => None,
        }
    }

    fn finish(&mut self, ok: bool, header: ResponseHeader) -> Option<Response> {
        let open = self.open.take()?;
        let mut response = Response {
            request: open.request,
            header,
            lines: open.lines,
            ok: ok && open.ok,
            truncated: open.truncated,
            closed_by_exit: false,
        };

        // Content-verified barrier: while it is outstanding, only the block that
        // actually carries the marker may claim the barrier request.
        let mut barrier_matched = false;
        let mut unmatched_barrier = false;
        if let (Some(barrier), Some(request)) = (self.barrier.as_ref(), response.request) {
            if barrier.request == request {
                if response
                    .first_line()
                    .is_some_and(|line| line == barrier.expected_first_line.as_slice())
                {
                    barrier_matched = true;
                } else {
                    unmatched_barrier = true;
                }
            }
        }
        if unmatched_barrier {
            let request = response
                .request
                .take()
                .expect("a barrier response always has a request");
            // The command is still unanswered: give it back its place in flight.
            self.in_flight.push_front(request);
            self.unsolicited.push(response.clone());
            return Some(response);
        }
        if barrier_matched {
            self.barrier = None;
        }

        if response.request.is_none() {
            self.unsolicited.push(response.clone());
        }
        Some(response)
    }

    /// Closes the queue and fails every outstanding request.
    ///
    /// Called on `%exit` and on transport failure.  After this the only legal
    /// operation is reading the returned responses.
    pub fn close(&mut self) -> Vec<Response> {
        self.closed = true;
        self.barrier = None;
        let mut failed = Vec::new();
        for request in self
            .in_flight
            .drain(..)
            .chain(self.pending.drain(..).map(|(id, _)| id))
        {
            failed.push(Response {
                request: Some(request),
                header: ResponseHeader::default(),
                lines: Vec::new(),
                ok: false,
                truncated: false,
                closed_by_exit: true,
            });
        }
        if let Some(open) = self.open.take() {
            failed.push(Response {
                request: open.request,
                header: open.header,
                lines: open.lines,
                ok: false,
                truncated: true,
                closed_by_exit: true,
            });
        }
        failed
    }

    /// Takes the unsolicited (initialisation) responses seen so far.
    pub fn take_unsolicited(&mut self) -> Vec<Response> {
        std::mem::take(&mut self.unsolicited)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::Command;

    fn queue() -> CommandQueue {
        CommandQueue::new(Limits {
            max_in_flight_commands: 1,
            ..Limits::default()
        })
    }

    fn begin(number: u64) -> Event {
        Event::ResponseBegin(ResponseHeader {
            time: 0,
            number,
            flags: 0,
        })
    }

    fn end(number: u64) -> Event {
        Event::ResponseEnd(ResponseHeader {
            time: 0,
            number,
            flags: 0,
        })
    }

    #[test]
    fn matches_a_response_to_its_request_in_order() {
        let mut q = queue();
        let id = q.enqueue(&Command::new("list-windows")).unwrap();
        let (written, bytes) = q.next_to_write().unwrap();
        assert_eq!(written, id);
        assert_eq!(bytes, b"list-windows\n");
        assert_eq!(q.on_event(&begin(11)), None);
        assert_eq!(q.on_event(&Event::ResponseLine(b"@0 dev".to_vec())), None);
        let response = q.on_event(&end(11)).unwrap();
        assert_eq!(response.request, Some(id));
        assert!(response.ok);
        assert_eq!(response.first_line(), Some(&b"@0 dev"[..]));
        assert_eq!(q.in_flight(), 0);
    }

    #[test]
    fn server_numbering_is_not_used_as_the_client_identifier() {
        let mut q = queue();
        let id = q.enqueue(&Command::new("list-sessions")).unwrap();
        let _ = q.next_to_write();
        q.on_event(&begin(999_999));
        let response = q.on_event(&end(999_999)).unwrap();
        assert_eq!(response.request, Some(id), "matching is positional");
        assert_eq!(
            response.header.number, 999_999,
            "but the server number is kept"
        );
    }

    #[test]
    fn only_one_command_is_in_flight_at_a_time() {
        let mut q = queue();
        q.enqueue(&Command::new("a")).unwrap();
        q.enqueue(&Command::new("b")).unwrap();
        assert!(q.next_to_write().is_some());
        assert!(q.next_to_write().is_none(), "window is full");
        q.on_event(&begin(1));
        q.on_event(&end(1));
        assert!(
            q.next_to_write().is_some(),
            "window reopens after the response"
        );
    }

    #[test]
    fn initialisation_responses_are_unsolicited_not_mismatched() {
        let mut q = queue();
        // The server answers its own attach commands before any client command.
        q.on_event(&begin(1));
        q.on_event(&Event::ResponseLine(b"attach output".to_vec()));
        let init = q.on_event(&end(1)).unwrap();
        assert_eq!(init.request, None, "no client request may be claimed");

        let id = q.enqueue(&Command::new("display-message")).unwrap();
        let _ = q.next_to_write();
        q.on_event(&begin(2));
        let response = q.on_event(&end(2)).unwrap();
        assert_eq!(
            response.request,
            Some(id),
            "the first user command gets its own answer"
        );
        assert_eq!(q.take_unsolicited().len(), 1);
    }

    #[test]
    fn barrier_holds_back_later_commands_until_the_marker_is_recognised() {
        let mut q = queue();
        let marker = q.enqueue(&Command::new("display-message")).unwrap();
        q.set_barrier(marker, b"__marker__".to_vec()).unwrap();
        let after = q.enqueue(&Command::new("list-panes")).unwrap();
        assert_eq!(q.next_to_write().unwrap().0, marker);
        assert!(
            q.next_to_write().is_none(),
            "nothing may be written before the barrier resolves"
        );

        // The server answers its own attach command first.  Its block does not
        // carry the marker, so it must NOT be matched to our request.
        q.on_event(&begin(1));
        q.on_event(&Event::ResponseLine(b"attach bookkeeping".to_vec()));
        let init = q.on_event(&end(1)).unwrap();
        assert_eq!(init.request, None, "an init block is unsolicited");
        assert_eq!(
            q.barrier(),
            Some(marker),
            "the barrier is still outstanding"
        );
        assert_eq!(q.in_flight(), 1, "our command is still awaiting its answer");
        assert!(q.next_to_write().is_none(), "the queue stays held back");

        // Then the real answer, carrying the marker.
        q.on_event(&begin(2));
        q.on_event(&Event::ResponseLine(b"__marker__".to_vec()));
        let answered = q.on_event(&end(2)).unwrap();
        assert_eq!(answered.request, Some(marker));
        assert_eq!(q.barrier(), None);
        assert_eq!(q.take_unsolicited().len(), 1);
        assert_eq!(q.next_to_write().unwrap().0, after);
    }

    #[test]
    fn errors_close_the_response_as_failed() {
        let mut q = queue();
        let id = q.enqueue(&Command::new("nope")).unwrap();
        let _ = q.next_to_write();
        q.on_event(&begin(1));
        let response = q
            .on_event(&Event::ResponseError(ResponseHeader {
                time: 0,
                number: 1,
                flags: 0,
            }))
            .unwrap();
        assert_eq!(response.request, Some(id));
        assert!(!response.ok);
        assert!(!response.is_complete_for(id));
    }

    #[test]
    fn closing_fails_everything_outstanding_and_stops_writes() {
        let mut q = queue();
        let a = q.enqueue(&Command::new("a")).unwrap();
        let b = q.enqueue(&Command::new("b")).unwrap();
        let _ = q.next_to_write();
        let failed = q.close();
        let ids: Vec<_> = failed.iter().filter_map(|r| r.request).collect();
        assert_eq!(ids, vec![a, b]);
        assert!(failed.iter().all(|r| r.closed_by_exit));
        assert!(q.next_to_write().is_none());
        assert_eq!(
            q.enqueue(&Command::new("c")),
            Err(QueueError::Closed),
            "a closed queue must never emit another byte"
        );
    }

    #[test]
    fn truncation_is_visible_on_the_response() {
        let mut q = queue();
        q.enqueue(&Command::new("capture-pane")).unwrap();
        let _ = q.next_to_write();
        q.on_event(&begin(1));
        q.on_event(&Event::Desync {
            reason: crate::parser::Desync::ResponseTooLarge,
            dropped_bytes: 10,
        });
        let response = q.on_event(&end(1)).unwrap();
        assert!(response.truncated);
    }

    #[test]
    fn pending_queue_is_bounded() {
        let mut q = CommandQueue::new(Limits {
            max_in_flight_commands: 1,
            max_pending_commands: 3,
            ..Limits::default()
        });
        for _ in 0..3 {
            q.enqueue(&Command::new("x")).unwrap();
        }
        assert_eq!(
            q.enqueue(&Command::new("x")),
            Err(QueueError::PendingFull { limit: 3 })
        );
    }
}
