//! Generic Server-Sent Events (SSE) parser.
//!
//! Takes a `Stream<Item = Result<Bytes, E>>` (the typical reqwest response
//! stream shape) and yields `SseEvent` values: events whose `data:` field
//! has been extracted as a string.
//!
//! SSE wire format (RFC):
//!   - events are separated by `\n\n` (or `\r\n\r\n`)
//!   - each event has one or more `field: value` lines
//!   - the field we care about is `data: <payload>`
//!   - lines that don't start with `data:` are ignored (event types, comments)
//!
//! The special payload `[DONE]` is **not** yielded as an event; callers
//! detect stream end themselves by noticing the rx channel closes.
//!
//! This parser is byte-stream-aware: HTTP chunks may split mid-line or
//! even mid-event. The parser buffers until it has a complete line.

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
    /// Pending bytes not yet emitted as a complete event.
    /// Always ends on a `\n` boundary (or is empty) once we yield an event.
    buf: Vec<u8>,
    /// Set to true when we hit `[DONE]`; we'll yield no more events.
    done: bool,
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
            done: false,
            _phantom: std::marker::PhantomData,
        }
    }
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
        if this.done {
            return Poll::Ready(None);
        }

        loop {
            // Try to extract a complete line (ending in \n) from buf.
            if let Some(idx) = this.buf.iter().position(|&b| b == b'\n') {
                let line_bytes: Vec<u8> = this.buf.drain(..=idx).collect();
                // Strip trailing \n (and any \r before it).
                let line_bytes = if line_bytes.len() >= 2
                    && line_bytes[line_bytes.len() - 2] == b'\r'
                {
                    &line_bytes[..line_bytes.len() - 2]
                } else {
                    &line_bytes[..line_bytes.len() - 1]
                };

                let line = match std::str::from_utf8(line_bytes) {
                    Ok(s) => s,
                    Err(_) => continue, // skip non-UTF8 lines (e.g. comments)
                };

                // Only process data: lines.
                if let Some(payload) = line.strip_prefix("data: ") {
                    if payload == "[DONE]" {
                        this.done = true;
                        return Poll::Ready(None);
                    }
                    return Poll::Ready(Some(Ok(SseEvent {
                        data: payload.to_string(),
                    })));
                }
                // else: ignore event:, id:, retry:, comments, blank lines
                continue;
            }

            // No complete line in buf. Pull more bytes.
            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    this.buf.extend_from_slice(&chunk);
                    // Loop back to check for new complete lines.
                }
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Some(Err(SseError::Stream(e.to_string()))));
                }
                Poll::Ready(None) => {
                    // Stream ended without [DONE]. If we have a partial line,
                    // try to flush it as a final event.
                    if !this.buf.is_empty() {
                        let line_bytes: Vec<u8> = this.buf.drain(..).collect();
                        let line = match std::str::from_utf8(&line_bytes) {
                            Ok(s) => s.trim(),
                            Err(_) => return Poll::Ready(None),
                        };
                        if let Some(payload) = line.strip_prefix("data: ") {
                            if payload == "[DONE]" {
                                return Poll::Ready(None);
                            }
                            return Poll::Ready(Some(Ok(SseEvent {
                                data: payload.to_string(),
                            })));
                        }
                    }
                    return Poll::Ready(None);
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
    use futures_util::{StreamExt, stream};

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
    async fn handles_chunk_split_mid_event() {
        // Two events split such that second line is in the next chunk.
        let s = chunks(vec!["data: first\nevent: foo\n", "data: second\n\n"]);
        let events = unwrap_ok(collect(SseStream::new(s)).await);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "first");
        assert_eq!(events[1].data, "second");
    }

    #[tokio::test]
    async fn done_terminates_stream() {
        let s = chunks(vec!["data: a\n\n", "data: b\n\n", "data: [DONE]\n\n", "data: c\n\n"]);
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