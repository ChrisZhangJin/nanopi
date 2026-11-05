//! Provider-level SSE wire-format fixtures (issue #3).
//!
//! A gateway may legally write `data:{...}` (no space after the colon),
//! `data: {...}`, CRLF or bare-CR line endings, and terminate with
//! `data: [DONE]` or `data:[DONE]`; TCP may cut the body at any byte.
//! `provider::sse` unit-tests the line parser in isolation. These tests
//! drive the REAL `OpenAiProvider` and `AnthropicProvider` streaming
//! code against a loopback server and assert that every wire variant
//! decodes to byte-for-byte the same `AgentEvent`s (and final `Usage`)
//! as the canonical spaced, LF, whole-body stream.
//!
//! The reported failure (a3c7936) was that `data:{...}` lines were
//! skipped, so the turn ended with no events at all. The canonical
//! fixture is checked against hard-coded expectations first, so a
//! variant can never "match" an empty reference.
//!
//! Hermetic: loopback only, no network, no API key. Chunking is varied
//! by how the server writes (pauses between pieces), but the assertions
//! are on decoded events, so they do not depend on where TCP happens to
//! cut the stream.

use std::io::{Read, Write};
use std::time::Duration;

use nanopi::agent::context::Context;
use nanopi::agent::loop_::Provider;
use nanopi::event::AgentEvent;
use nanopi::provider::anthropic::AnthropicProvider;
use nanopi::provider::openai::OpenAiProvider;
use tokio::sync::mpsc;

// ── fixtures ────────────────────────────────────────────────────────

/// One SSE event: optional `event:` name plus the JSON payload.
type Ev = (Option<&'static str>, String);

fn openai_events() -> Vec<Ev> {
    let j = |s: &str| (None, s.to_string());
    vec![
        j(r#"{"id":"chatcmpl-1","choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"hmm"},"finish_reason":null}]}"#),
        j(r#"{"id":"chatcmpl-1","choices":[{"index":0,"delta":{"content":"Hel"},"finish_reason":null}]}"#),
        j(r#"{"id":"chatcmpl-1","choices":[{"index":0,"delta":{"content":"lo"},"finish_reason":null}]}"#),
        j(r#"{"id":"chatcmpl-1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"bash","arguments":"{\"comm"}}]},"finish_reason":null}]}"#),
        j(r#"{"id":"chatcmpl-1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"and\":\"ls\"}"}}]},"finish_reason":null}]}"#),
        j(r#"{"id":"chatcmpl-1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#),
        j(r#"{"id":"chatcmpl-1","choices":[],"usage":{"prompt_tokens":11,"completion_tokens":5}}"#),
    ]
}

fn anthropic_events() -> Vec<Ev> {
    let e = |n: &'static str, s: &str| (Some(n), s.to_string());
    vec![
        e("message_start", r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":11,"output_tokens":1}}}"#),
        e("content_block_start", r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#),
        e("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}"#),
        e("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}"#),
        e("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
        e("content_block_start", r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"bash"}}"#),
        e("content_block_delta", r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"comm"}}"#),
        e("content_block_delta", r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"and\":\"ls\"}"}}"#),
        e("content_block_stop", r#"{"type":"content_block_stop","index":1}"#),
        e("message_delta", r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"input_tokens":11,"output_tokens":5}}"#),
        e("message_stop", r#"{"type":"message_stop"}"#),
    ]
}

/// How one stream is written on the wire.
#[derive(Clone, Copy, Debug)]
struct Wire {
    /// `data: x` (true) vs `data:x` (false). Also applied to `event:`.
    space: bool,
    /// Line terminator.
    eol: &'static str,
    /// `[DONE]` terminator: `None` omits it, `Some(true)` is
    /// `data: [DONE]`, `Some(false)` is `data:[DONE]`.
    done: Option<bool>,
    /// Write the body in pieces of this many bytes (a split can land
    /// mid-line, mid-field-name, or between a CR and its LF). `None`
    /// writes it in one piece.
    piece: Option<usize>,
}

const CANONICAL: Wire = Wire {
    space: true,
    eol: "\n",
    done: Some(true),
    piece: None,
};

fn render(events: &[Ev], w: Wire) -> Vec<u8> {
    let sp = if w.space { " " } else { "" };
    let mut s = String::new();
    for (name, data) in events {
        if let Some(n) = name {
            s.push_str(&format!("event:{sp}{n}{}", w.eol));
        }
        s.push_str(&format!("data:{sp}{data}{}{}", w.eol, w.eol));
    }
    if let Some(done_space) = w.done {
        let dsp = if done_space { " " } else { "" };
        s.push_str(&format!("data:{dsp}[DONE]{}{}", w.eol, w.eol));
    }
    s.into_bytes()
}

fn variants() -> Vec<(&'static str, Wire)> {
    let w = |space, eol, done, piece| Wire { space, eol, done, piece };
    vec![
        ("no space after data:", w(false, "\n", Some(true), None)),
        ("no space, data:[DONE]", w(false, "\n", Some(false), None)),
        ("space, data:[DONE]", w(true, "\n", Some(false), None)),
        ("space, no [DONE]", w(true, "\n", None, None)),
        ("no space, no [DONE]", w(false, "\n", None, None)),
        ("space, CRLF", w(true, "\r\n", Some(true), None)),
        ("no space, CRLF, data:[DONE]", w(false, "\r\n", Some(false), None)),
        ("space, bare CR", w(true, "\r", Some(true), None)),
        ("no space, bare CR", w(false, "\r", Some(false), None)),
        ("space, split every 7 bytes", w(true, "\n", Some(true), Some(7))),
        ("no space, split every 7 bytes", w(false, "\n", Some(false), Some(7))),
        ("no space, CRLF, split every 3 bytes", w(false, "\r\n", Some(false), Some(3))),
        ("space, CRLF, split every 1 byte", w(true, "\r\n", Some(true), Some(1))),
        ("no space, CRLF, split every 1 byte", w(false, "\r\n", Some(false), Some(1))),
    ]
}

// ── loopback server ─────────────────────────────────────────────────

/// Serve `body` to every connection as a close-delimited
/// `text/event-stream` response, in `piece`-byte writes with a short
/// pause between them. Returns the base URL.
fn serve(body: Vec<u8>, piece: Option<usize>) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let body = body.clone();
            std::thread::spawn(move || {
                let _ = stream.set_nodelay(true);
                let mut seen = Vec::new();
                let mut byte = [0u8; 1];
                while !seen.ends_with(b"\r\n\r\n") {
                    match stream.read(&mut byte) {
                        Ok(0) | Err(_) => return,
                        Ok(_) => seen.push(byte[0]),
                    }
                }
                let head = String::from_utf8_lossy(&seen).to_ascii_lowercase();
                let len: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                let mut req = vec![0u8; len];
                let _ = stream.read_exact(&mut req);

                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                );
                match piece {
                    None => {
                        let _ = stream.write_all(&body);
                    }
                    Some(n) => {
                        for c in body.chunks(n) {
                            if stream.write_all(c).is_err() || stream.flush().is_err() {
                                return;
                            }
                            // A 1-byte write still has to reach the
                            // client as its own read often enough to
                            // exercise mid-line buffering; a pause
                            // makes that likely, not guaranteed. The
                            // assertions never depend on it.
                            std::thread::sleep(Duration::from_micros(300));
                        }
                    }
                }
                let _ = stream.flush();
            });
        }
    });
    format!("http://127.0.0.1:{port}")
}

fn client() -> reqwest::Client {
    // Never route a loopback fixture through an ambient proxy.
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("client")
}

/// Run one turn; return the emitted events and the returned usage as
/// JSON, which is what must be identical across wire variants.
async fn run_openai(body: Vec<u8>, piece: Option<usize>) -> (Vec<serde_json::Value>, serde_json::Value) {
    let base = serve(body, piece);
    let mut p = OpenAiProvider::new(base, "k", "m");
    p.client = client();
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
    let usage = tokio::time::timeout(Duration::from_secs(30), p.stream_turn(&Context::default(), tx))
        .await
        .expect("stream_turn must finish")
        .expect("stream_turn must succeed");
    (drain(&mut rx), serde_json::to_value(usage).unwrap())
}

async fn run_anthropic(body: Vec<u8>, piece: Option<usize>) -> (Vec<serde_json::Value>, serde_json::Value) {
    let base = serve(body, piece);
    let mut p = AnthropicProvider::new(base, "k", "m");
    p.client = client();
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
    let usage = tokio::time::timeout(Duration::from_secs(30), p.stream_turn(&Context::default(), tx))
        .await
        .expect("stream_turn must finish")
        .expect("stream_turn must succeed");
    (drain(&mut rx), serde_json::to_value(usage).unwrap())
}

fn drain(rx: &mut mpsc::Receiver<AgentEvent>) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(serde_json::to_value(ev).unwrap());
    }
    out
}

/// The decoded text, concatenated, from `text_delta` events.
fn text_of(evs: &[serde_json::Value]) -> String {
    evs.iter()
        .filter(|e| e["type"] == "text_delta")
        .map(|e| e["text"].as_str().unwrap())
        .collect()
}

fn tool_calls_of(evs: &[serde_json::Value]) -> Vec<(String, String, serde_json::Value)> {
    evs.iter()
        .filter(|e| e["type"] == "tool_call")
        .map(|e| {
            let c = &e["call"];
            (
                c["id"].as_str().unwrap().to_string(),
                c["name"].as_str().unwrap().to_string(),
                c["arguments"].clone(),
            )
        })
        .collect()
}

// ── tests ───────────────────────────────────────────────────────────

/// The canonical stream decodes to the expected events. Without this a
/// variant "matching" an empty reference would pass vacuously.
#[tokio::test]
async fn openai_canonical_fixture_decodes_as_expected() {
    let (evs, usage) = run_openai(render(&openai_events(), CANONICAL), None).await;
    assert_eq!(text_of(&evs), "Hello", "{evs:?}");
    assert_eq!(
        tool_calls_of(&evs),
        vec![("call_1".into(), "bash".into(), serde_json::json!({"command": "ls"}))]
    );
    assert!(evs.iter().any(|e| e["type"] == "thinking_delta" && e["text"] == "hmm"), "{evs:?}");
    assert!(evs.iter().any(|e| e["type"] == "done" && e["finish_reason"] == "tool_calls"), "{evs:?}");
    assert_eq!(usage["input_tokens"], 11);
    assert_eq!(usage["output_tokens"], 5);
}

#[tokio::test]
async fn anthropic_canonical_fixture_decodes_as_expected() {
    let (evs, usage) = run_anthropic(render(&anthropic_events(), CANONICAL), None).await;
    assert_eq!(text_of(&evs), "Hello", "{evs:?}");
    assert_eq!(
        tool_calls_of(&evs),
        vec![("toolu_1".into(), "bash".into(), serde_json::json!({"command": "ls"}))]
    );
    assert!(evs.iter().any(|e| e["type"] == "done" && e["finish_reason"] == "tool_calls"), "{evs:?}");
    assert_eq!(usage["input_tokens"], 11);
    assert_eq!(usage["output_tokens"], 5);
}

/// Every wire variant yields exactly the events the canonical stream
/// does, through the real OpenAI-compatible streaming parser.
#[tokio::test]
async fn openai_wire_variants_decode_like_the_canonical_stream() {
    let events = openai_events();
    let reference = run_openai(render(&events, CANONICAL), None).await;
    for (name, w) in variants() {
        let got = run_openai(render(&events, w), w.piece).await;
        assert_eq!(got, reference, "variant `{name}` ({w:?}) diverged");
    }
}

/// Same, through the real Anthropic streaming parser (which also sees
/// `event:` lines, with and without the space).
#[tokio::test]
async fn anthropic_wire_variants_decode_like_the_canonical_stream() {
    let events = anthropic_events();
    let reference = run_anthropic(render(&events, CANONICAL), None).await;
    for (name, w) in variants() {
        let got = run_anthropic(render(&events, w), w.piece).await;
        assert_eq!(got, reference, "variant `{name}` ({w:?}) diverged");
    }
}

/// The exact shape in the report: a gateway that writes `data:{...}`
/// and ends with `data:[DONE]`, nothing else unusual. The turn must
/// produce its text rather than ending silently.
#[tokio::test]
async fn the_reported_gateway_shape_produces_text_not_silence() {
    let body = b"data:{\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n\
                 data:{\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                 data:[DONE]\n\n"
        .to_vec();
    let (evs, _) = run_openai(body, None).await;
    assert_eq!(text_of(&evs), "hi", "{evs:?}");
    assert!(evs.iter().any(|e| e["type"] == "done" && e["finish_reason"] == "stop"), "{evs:?}");
}
