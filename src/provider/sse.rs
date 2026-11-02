//! Generic Server-Sent Events (SSE) parser.
//!
//! Takes a `Stream<Item = Result<Bytes, E>>` (the typical reqwest response
//! stream shape) and yields `SseEvent` values: events whose `data` has been
//! extracted as a string.
//!
//! SSE wire format (WHATWG HTML, "Server-sent events"):
//!   - lines end in `\n`, `\r\n` or a bare `\r`
//!   - a blank line ends an event
//!   - a line is `field: value` (or `field:value`; one leading space of the
//!     value is dropped) or, with no colon, `field` alone
//!   - a line starting with `:` is a comment
//!   - the field we care about is `data`; several `data` lines in one event
//!     are joined with `\n`. `event`, `id`, `retry` are accepted and
//!     dropped. An event with no `data` line yields nothing.
//!
//! The special payload `[DONE]` is **not** yielded as an event; callers
//! detect stream end themselves by noticing the stream finish. It is the
//! whole joined `data` of an event, not one line of a longer one.
//!
//! This parser is byte-stream-aware: HTTP chunks may split mid-line, mid-
//! `\r\n` or even mid-event. The parser buffers until it has a complete line.

use bytes::Bytes;
use futures_util::Stream;
use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};

/// One SSE event after parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// The `data:` payload, with the trailing newline stripped.
    pub data: String,
}

#[derive(Debug)]
pub enum SseError {
    /// Underlying stream error (reqwest connection drop, etc.).
    Stream(String),
    /// UTF-8 decoding failed for a line. The parser skips the bad line
    /// and continues (SSE allows comments / heartbeats to be non-UTF8).
    Utf8,
}

impl fmt::Display for SseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SseError::Stream(e) => write!(f, "SSE stream error: {e}"),
            SseError::Utf8 => write!(f, "SSE line is not valid UTF-8"),
        }
    }
}

impl std::error::Error for SseError {}

/// Buffered SSE parser wrapping a byte stream.
pub struct SseStream<S, E>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: fmt::Display,
{
    inner: S,
    /// Pending bytes not yet consumed as a complete line.
    buf: Vec<u8>,
    /// Prefix of `buf` already searched for a line terminator and found to
    /// have none. Without it every arriving chunk rescans the whole partial
    /// line, so one n-byte line delivered in small chunks costs O(n^2) —
    /// seconds of CPU for a multi-megabyte `data:` line.
    scanned: usize,
    /// The last line ended in a `\r` that was the final byte of `buf`: a
    /// `\n` arriving first in the next chunk is the other half of that
    /// line ending, not an empty line.
    skip_lf: bool,
    /// The event being assembled: its `data:` lines joined with `\n`.
    data: String,
    /// At least one `data` field line has been seen for the current event
    /// (`data` alone cannot tell "no data lines" from "one empty line").
    has_data: bool,
    /// Set to true when we hit `[DONE]`; we'll yield no more events.
    done: bool,
    /// The inner stream has ended and everything buffered has been flushed.
    ended: bool,
    _phantom: std::marker::PhantomData<E>,
}

// `S: Unpin` + `Vec<u8>: Unpin` => SseStream is Unpin. Lets us use
// `Pin::get_mut(self)` to mutate `buf` and `inner` without unsafe.
impl<S, E> Unpin for SseStream<S, E>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: fmt::Display,
{
}

impl<S, E> SseStream<S, E>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: fmt::Display,
{
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            buf: Vec::new(),
            scanned: 0,
            skip_lf: false,
            data: String::new(),
            has_data: false,
            done: false,
            ended: false,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Fold one line (terminator already removed) into the event being
    /// assembled. Returns true for the blank line that ends an event.
    fn feed(&mut self, line: &[u8]) -> bool {
        if line.is_empty() {
            return true;
        }
        // `:comment`. Also covers a comment that looks like a field.
        if line[0] == b':' {
            return false;
        }
        let (field, value) = match line.iter().position(|&b| b == b':') {
            Some(i) => {
                let value = &line[i + 1..];
                (&line[..i], value.strip_prefix(b" ").unwrap_or(value))
            }
            // No colon: the whole line is the field name, value empty.
            None => (line, &line[..0]),
        };
        // `event`, `id`, `retry` and unknown fields are not exposed. Only
        // a `data` value has to be text; a stray non-UTF-8 line is
        // skipped rather than failing the stream.
        if field == b"data" {
            if let Ok(value) = std::str::from_utf8(value) {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
        }
        false
    }

    /// End of event: hand it out, unless it had no `data` lines.
    fn dispatch(&mut self) -> Dispatch {
        if !self.has_data {
            return Dispatch::Nothing;
        }
        self.has_data = false;
        let data = std::mem::take(&mut self.data);
        if data == "[DONE]" {
            self.done = true;
            return Dispatch::Done;
        }
        Dispatch::Event(SseEvent { data })
    }
}

enum Dispatch {
    Nothing,
    Event(SseEvent),
    Done,
}

impl<S, E> Stream for SseStream<S, E>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: fmt::Display,
{
    type Item = Result<SseEvent, SseError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // We marked SseStream: Unpin (all fields are Unpin), so this is safe.
        let this = self.get_mut();

        // If we've already emitted [DONE], end the stream.
        if this.done || this.ended {
            return Poll::Ready(None);
        }

        loop {
            // A `\n` completing a `\r\n` that straddled two chunks.
            if this.skip_lf && !this.buf.is_empty() {
                this.skip_lf = false;
                if this.buf[0] == b'\n' {
                    this.buf.remove(0);
                    this.scanned = this.scanned.saturating_sub(1);
                }
            }

            // Try to extract a complete line (any of \n, \r\n, \r) from buf.
            let end = this.buf[this.scanned..]
                .iter()
                .position(|&b| b == b'\n' || b == b'\r')
                .map(|i| this.scanned + i);
            if let Some(idx) = end {
                let term = match (this.buf[idx], this.buf.get(idx + 1)) {
                    (b'\r', Some(b'\n')) => 2,
                    (b'\r', None) => {
                        this.skip_lf = true;
                        1
                    }
                    _ => 1,
                };
                // `feed` borrows `self` mutably, so lift the line out of
                // `buf` first. Taking the bytes (not copying) leaves the
                // tail to be shifted down by the drain below.
                let line = std::mem::take(&mut this.buf);
                let blank = this.feed(&line[..idx]);
                this.buf = line;
                this.buf.drain(..idx + term);
                this.scanned = 0;
                if blank {
                    match this.dispatch() {
                        Dispatch::Event(ev) => return Poll::Ready(Some(Ok(ev))),
                        Dispatch::Done => return Poll::Ready(None),
                        Dispatch::Nothing => {}
                    }
                }
                continue;
            }

            // No complete line in buf. Pull more bytes.
            this.scanned = this.buf.len();
            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    this.buf.extend_from_slice(&chunk);
                    // Loop back to check for new complete lines.
                }
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Some(Err(SseError::Stream(e.to_string()))));
                }
                Poll::Ready(None) => {
                    // Stream ended mid-event. The spec drops such an event;
                    // we deliver it, as this parser always has, so a
                    // server that closes without the last blank line
                    // loses nothing.
                    this.ended = true;
                    let line = std::mem::take(&mut this.buf);
                    this.scanned = 0;
                    if !line.is_empty() {
                        this.feed(&line);
                    }
                    return match this.dispatch() {
                        Dispatch::Event(ev) => Poll::Ready(Some(Ok(ev))),
                        Dispatch::Nothing | Dispatch::Done => Poll::Ready(None),
                    };
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use futures_util::{stream, StreamExt};

    /// Helper: collect all events from an SseStream into a Vec<Result>.
    async fn collect<S, E>(sse: SseStream<S, E>) -> Vec<Result<SseEvent, SseError>>
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin,
        E: fmt::Display,
    {
        sse.collect().await
    }

    /// Helper: convert Vec<&str> chunks into a byte stream.
    fn chunks(items: Vec<&'static str>) -> impl Stream<Item = Result<Bytes, String>> + Unpin {
        stream::iter(items.into_iter().map(|s| Ok(Bytes::from(s))))
    }

    /// Unwrap all Ok results from a Vec<Result<SseEvent, _>>.
    fn unwrap_ok(v: Vec<Result<SseEvent, SseError>>) -> Vec<SseEvent> {
        v.into_iter().map(|r| r.unwrap()).collect()
    }

    #[tokio::test]
    async fn parses_single_event() {
        let s = chunks(vec!["data: hello\n\n"]);
        let events = unwrap_ok(collect(SseStream::new(s)).await);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello");
    }

    #[tokio::test]
    async fn parses_multiple_events() {
        let s = chunks(vec![
            "data: {\"a\":1}\n\n",
            "data: {\"a\":2}\n\n",
            "data: [DONE]\n\n",
        ]);
        let events = unwrap_ok(collect(SseStream::new(s)).await);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "{\"a\":1}");
        assert_eq!(events[1].data, "{\"a\":2}");
    }

    #[tokio::test]
    async fn handles_chunk_split_mid_line() {
        // Line `data: hello` split across two chunks.
        let s = chunks(vec!["da", "ta: hel", "lo\n\n"]);
        let events = unwrap_ok(collect(SseStream::new(s)).await);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello");
    }

    #[tokio::test]
    async fn newline_arriving_alone_in_a_later_chunk_is_found() {
        // The scan resumes where it stopped, so the newline that is the
        // whole of a chunk (and one right after a drained line) must
        // still be seen.
        let s = chunks(vec!["data: a", "\n", "\n", "data: b", "\n", "\n", "da", "ta: c\n"]);
        let events = unwrap_ok(collect(SseStream::new(s)).await);
        let data: Vec<_> = events.iter().map(|e| e.data.as_str()).collect();
        assert_eq!(data, ["a", "b", "c"]);
    }

    #[tokio::test]
    async fn handles_chunk_split_mid_event() {
        // Two events split such that second line is in the next chunk.
        let s = chunks(vec!["data: first\nevent: foo\n\n", "data: second\n\n"]);
        let events = unwrap_ok(collect(SseStream::new(s)).await);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "first");
        assert_eq!(events[1].data, "second");
    }

    #[tokio::test]
    async fn done_terminates_stream() {
        let s = chunks(vec![
            "data: a\n\n",
            "data: b\n\n",
            "data: [DONE]\n\n",
            "data: c\n\n",
        ]);
        let events = unwrap_ok(collect(SseStream::new(s)).await);
        // c should not appear.
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "a");
        assert_eq!(events[1].data, "b");
    }

    #[tokio::test]
    async fn skips_non_data_lines() {
        // OpenAI's actual stream format has fields like:
        //   data: {...}\n\n
        // but the parser must also tolerate `event:` and `id:` lines.
        let s = chunks(vec![
            "event: message\n",
            "data: {\"x\":1}\n",
            "id: 42\n",
            "\n", // event boundary
        ]);
        let events = unwrap_ok(collect(SseStream::new(s)).await);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "{\"x\":1}");
    }

    #[tokio::test]
    async fn crlf_line_endings() {
        let s = chunks(vec!["data: hello\r\n\r\n"]);
        let events = unwrap_ok(collect(SseStream::new(s)).await);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello");
    }

    #[tokio::test]
    async fn empty_stream_yields_no_events() {
        let s = chunks(vec![]);
        let events = unwrap_ok(collect(SseStream::new(s)).await);
        assert!(events.is_empty());
    }

    /// Helper: the `data` of every event, in order.
    async fn data_of(items: Vec<&'static str>) -> Vec<String> {
        unwrap_ok(collect(SseStream::new(chunks(items))).await)
            .into_iter()
            .map(|e| e.data)
            .collect()
    }

    /// Helper: the `data` of every event from raw byte chunks.
    async fn data_of_bytes(items: Vec<Vec<u8>>) -> Vec<String> {
        let s = stream::iter(items.into_iter().map(|b| Ok::<_, String>(Bytes::from(b))));
        unwrap_ok(collect(SseStream::new(s)).await)
            .into_iter()
            .map(|e| e.data)
            .collect()
    }

    #[tokio::test]
    async fn data_with_and_without_the_space_after_the_colon() {
        assert_eq!(data_of(vec!["data:x\n\n"]).await, ["x"]);
        assert_eq!(data_of(vec!["data: x\n\n"]).await, ["x"]);
    }

    #[tokio::test]
    async fn only_one_leading_space_is_stripped() {
        assert_eq!(data_of(vec!["data:  x\n\n"]).await, [" x"]);
        assert_eq!(data_of(vec!["data:   \n\n"]).await, ["  "]);
        // Trailing space is data too.
        assert_eq!(data_of(vec!["data: x \n\n"]).await, ["x "]);
    }

    #[tokio::test]
    async fn several_data_lines_are_one_event_joined_with_newline() {
        assert_eq!(
            data_of(vec!["data: {\ndata:   \"a\": 1\ndata: }\n\n"]).await,
            ["{\n  \"a\": 1\n}"]
        );
        // The event ends at the blank line, not before it.
        assert_eq!(
            data_of(vec!["data: a\n", "data: b\n", "\n", "data: c\n\n"]).await,
            ["a\nb", "c"]
        );
    }

    #[tokio::test]
    async fn an_empty_data_line_still_contributes_a_line() {
        assert_eq!(data_of(vec!["data: a\ndata:\ndata: b\n\n"]).await, ["a\n\nb"]);
        assert_eq!(data_of(vec!["data:\n\n"]).await, [""]);
    }

    #[tokio::test]
    async fn a_field_with_no_colon_is_the_field_name_with_an_empty_value() {
        // `data` alone is an empty data line; `event` alone is ignored.
        assert_eq!(data_of(vec!["data\n\n"]).await, [""]);
        assert_eq!(data_of(vec!["data: a\ndata\n\n"]).await, ["a\n"]);
        assert_eq!(data_of(vec!["event\ndata: a\n\n"]).await, ["a"]);
    }

    #[tokio::test]
    async fn other_fields_and_comments_between_data_lines_do_not_leak() {
        assert_eq!(
            data_of(vec![
                ": hello\nevent: message\nid: 7\ndata: a\nretry: 1000\n: mid\ndata: b\nevent: x\n\n"
            ])
            .await,
            ["a\nb"]
        );
        // Names that merely start with `data` are other fields.
        assert_eq!(data_of(vec!["database: no\ndata_x: no\ndata: yes\n\n"]).await, ["yes"]);
        // A comment that looks like a data line is still a comment.
        assert_eq!(data_of(vec![":data: no\ndata: yes\n\n"]).await, ["yes"]);
    }

    #[tokio::test]
    async fn an_event_with_no_data_lines_yields_nothing() {
        assert!(data_of(vec!["event: ping\nid: 1\nretry: 5\n\n"]).await.is_empty());
        assert!(data_of(vec![": keep-alive\n\n", "\n\n\n"]).await.is_empty());
        // ... and does not eat the event after it, nor carry its fields over.
        assert_eq!(data_of(vec!["event: ping\n\n", "data: a\n\n"]).await, ["a"]);
    }

    #[tokio::test]
    async fn every_line_ending_works_alone_and_mixed() {
        for wire in [
            "data: a\ndata: b\n\ndata: c\n\n",
            "data: a\r\ndata: b\r\n\r\ndata: c\r\n\r\n",
            "data: a\rdata: b\r\rdata: c\r\r",
            "data: a\r\ndata: b\n\rdata: c\r\n\n",
        ] {
            assert_eq!(data_of(vec![wire]).await, ["a\nb", "c"], "{wire:?}");
        }
    }

    #[tokio::test]
    async fn crlf_split_between_chunks_is_one_line_ending() {
        // `\r` ends a line; the `\n` that starts the next chunk belongs to
        // it and must not read as a blank line (which would cut the event).
        assert_eq!(
            data_of(vec!["data: a\r", "\ndata: b\r", "\n\r", "\n", "data: c\r\n\r\n"]).await,
            ["a\nb", "c"]
        );
    }

    #[tokio::test]
    async fn every_split_point_gives_the_same_events() {
        let wire = "event: m\r\nid: 1\r\ndata: {\r\ndata:  \"a\": 1\r\ndata: }\r\n\r\n: c\rdata:b\r\rdata: [DONE]\n\n";
        let want = ["{\n \"a\": 1\n}", "b"];
        for cut in 0..=wire.len() {
            let (a, b) = wire.as_bytes().split_at(cut);
            assert_eq!(data_of_bytes(vec![a.to_vec(), b.to_vec()]).await, want, "cut at {cut}");
        }
        // And one byte at a time.
        let bytes = wire.bytes().map(|b| vec![b]).collect();
        assert_eq!(data_of_bytes(bytes).await, want);
    }

    #[tokio::test]
    async fn done_is_matched_on_the_whole_event() {
        assert_eq!(data_of(vec!["data:[DONE]\n\ndata: x\n\n"]).await, Vec::<String>::new());
        // `[DONE]` as one line of several is data, not the sentinel.
        assert_eq!(data_of(vec!["data: a\ndata: [DONE]\n\n"]).await, ["a\n[DONE]"]);
        assert_eq!(
            data_of(vec!["data: a\n\ndata: [DONE]\r\n\r\ndata: x\n\n"]).await,
            ["a"]
        );
    }

    #[tokio::test]
    async fn an_event_cut_off_by_the_end_of_the_stream_is_still_delivered() {
        // Lenient on purpose (the parser always did this): a server that
        // closes without the final blank line loses nothing.
        assert_eq!(data_of(vec!["data: a\n\ndata: b"]).await, ["a", "b"]);
        assert_eq!(data_of(vec!["data: a\ndata: b\n"]).await, ["a\nb"]);
        assert_eq!(data_of(vec!["data: [DONE]"]).await, Vec::<String>::new());
        assert!(data_of(vec!["event: x\n"]).await.is_empty());
    }

    #[tokio::test]
    async fn a_line_that_is_not_utf8_is_skipped_not_fatal() {
        let got = data_of_bytes(vec![b"data: a\n\xff\xfe: x\ndata: b\n\n".to_vec()]).await;
        assert_eq!(got, ["a\nb"]);
    }

    #[tokio::test]
    async fn stream_error_propagates() {
        let s = stream::iter(vec![
            Ok::<_, String>(Bytes::from("data: ok\n\n")),
            Err("connection lost".to_string()),
            Ok(Bytes::from("data: never\n\n")), // never reached if we terminate
        ]);
        let mut sse = SseStream::new(s);
        let mut events = Vec::new();
        while let Some(item) = sse.next().await {
            match item {
                Ok(ev) => events.push(ev),
                Err(_) => break,
            }
        }
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "ok");
    }
}
