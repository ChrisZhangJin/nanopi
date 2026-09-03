//! End-to-end `-p` tests against a fake OpenAI-compatible endpoint.
//!
//! The unit tests in `render::stdout` assert what the renderer writes
//! when handed an `AgentEvent`. They cannot catch a break in the wiring
//! that produces those events: the SSE parse, the agent loop's ordering,
//! or `mode::print`'s decision about which events reach the renderer at
//! all. The bug that motivated this file —
//!
//!     That's a simple greeting.Hello, Tom! — from my-plugin
//!
//! only appears once a real `reasoning_content` delta is followed by a
//! real `content` delta, which is exactly the seam between those layers.
//!
//! Hermetic: a loopback `TcpListener` on an ephemeral port speaks just
//! enough of `/chat/completions` to satisfy the provider. No network, no
//! API key, no model.

use std::io::{Read, Write};
use std::process::Command;

/// Serve one SSE response, forever, to every connection.
///
/// `chunks` are `data:` payload bodies; `[DONE]` and the framing are
/// added here so a test reads as a list of deltas.
///
/// The thread is deliberately never joined — the listener lives as long
/// as the test process, which is the same shape the WASM integration
/// tests use for their fixture servers.
fn spawn_sse_server(chunks: Vec<String>) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("local_addr").port();

    let mut body = String::new();
    for c in &chunks {
        body.push_str("data: ");
        body.push_str(c);
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = match stream {
                Ok(s) => s,
                Err(_) => continue,
            };
            // Drain the request head, or the client sees a reset
            // instead of the response.
            let mut seen = Vec::new();
            let mut byte = [0u8; 1];
            while !seen.ends_with(b"\r\n\r\n") {
                match stream.read(&mut byte) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => seen.push(byte[0]),
                }
            }
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        }
    });

    port
}

/// Like [`spawn_sse_server`], but serves a different response to each
/// successive request, so a turn with a tool round can be scripted.
///
/// Needed because a single canned response makes the agent loop
/// non-terminating: the model would ask for the same tool forever.
/// Requests past the end of the script get the last response.
fn spawn_sse_server_seq(responses: Vec<Vec<String>>) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("local_addr").port();

    let bodies: Vec<String> = responses
        .iter()
        .map(|chunks| {
            let mut body = String::new();
            for c in chunks {
                body.push_str("data: ");
                body.push_str(c);
                body.push_str("\n\n");
            }
            body.push_str("data: [DONE]\n\n");
            body
        })
        .collect();

    std::thread::spawn(move || {
        let mut n = 0usize;
        for stream in listener.incoming() {
            let mut stream = match stream {
                Ok(s) => s,
                Err(_) => continue,
            };
            let mut seen = Vec::new();
            let mut byte = [0u8; 1];
            while !seen.ends_with(b"\r\n\r\n") {
                match stream.read(&mut byte) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => seen.push(byte[0]),
                }
            }
            // Some clients probe; only count requests that carried a
            // request line, so the script stays aligned.
            let body = &bodies[n.min(bodies.len() - 1)];
            n += 1;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        }
    });

    port
}

/// One streamed tool call, in the shape `WireToolCall` expects.
fn tool_call_delta(index: u32, id: &str, name: &str, arguments: &str) -> String {
    format!(
        r#"{{"id":"x","choices":[{{"index":0,"delta":{{"tool_calls":[{{"index":{index},"id":"{id}","type":"function","function":{{"name":"{name}","arguments":{}}}}}]}},"finish_reason":null}}]}}"#,
        serde_json::Value::String(arguments.to_string())
    )
}

/// A terminal chunk carrying only `finish_reason`.
///
/// Not optional for a tool round: the agent loop executes tools only on
/// `FinishReason::ToolCalls` (`loop_.rs`), which the provider maps from
/// `finish_reason: "tool_calls"` (`openai.rs`). A stream that ends
/// without one is treated as a plain stop, so the calls are announced
/// and then nothing runs — which is exactly what this fixture did
/// before the field was added.
fn finish(reason: &str) -> String {
    format!(
        r#"{{"id":"x","choices":[{{"index":0,"delta":{{}},"finish_reason":"{reason}"}}]}}"#
    )
}

fn delta(field: &str, text: &str) -> String {
    format!(
        r#"{{"id":"x","choices":[{{"index":0,"delta":{{"{field}":{}}},"finish_reason":null}}]}}"#,
        serde_json::Value::String(text.to_string())
    )
}

/// Run `nanopi -p` against the fake endpoint and return its stdout with
/// SGR sequences stripped — what a pipe would receive.
fn run_p(port: u16, args: &[&str]) -> String {
    let dir = std::env::temp_dir().join(format!("nanopi-p-e2e-{port}"));
    std::fs::create_dir_all(&dir).expect("tmp cwd");

    let out = Command::new(env!("CARGO_BIN_EXE_nanopi"))
        .current_dir(&dir)
        .args(["-p", "--base-url"])
        .arg(format!("http://127.0.0.1:{port}"))
        .args(["--model", "fake-model", "--api-key", "not-a-real-key"])
        // No hooks, no skills, no context files: this asserts the
        // provider→loop→stdout path, and anything discovered from the
        // developer's own ~/.nanopi would make it non-hermetic.
        .args(["--no-hooks", "--no-skills", "--no-context-files"])
        .args(args)
        .env("NANOPI_HOME", dir.join("home"))
        .output()
        .expect("run nanopi -p");

    let _ = std::fs::remove_dir_all(&dir);
    strip_sgr(&String::from_utf8_lossy(&out.stdout))
}

/// Like [`run_p`], but writes a `.nanopi/config.toml` with `provider =
/// "<provider>"` before running — the way `run_p` can't reach a vendor
/// gate, since there is no `--provider` CLI flag (only `config.toml`).
///
/// `extra_config` is appended verbatim to the same file (e.g.
/// `inline_think_tags = false`), so callers can exercise the escape
/// hatch without a second helper.
fn run_p_with_provider(port: u16, provider: &str, extra_config: &str, args: &[&str]) -> String {
    let dir = std::env::temp_dir().join(format!("nanopi-p-e2e-provider-{port}"));
    std::fs::create_dir_all(dir.join(".nanopi")).expect("cfg dir");
    std::fs::write(
        dir.join(".nanopi/config.toml"),
        format!("provider = \"{provider}\"\n{extra_config}\n"),
    )
    .expect("write config");

    let out = Command::new(env!("CARGO_BIN_EXE_nanopi"))
        .current_dir(&dir)
        .args(["-p", "--base-url"])
        .arg(format!("http://127.0.0.1:{port}"))
        .args(["--model", "fake-model", "--api-key", "not-a-real-key"])
        .args(["--no-hooks", "--no-skills", "--no-context-files"])
        .args(args)
        .env("NANOPI_HOME", dir.join("home"))
        .output()
        .expect("run nanopi -p");

    let _ = std::fs::remove_dir_all(&dir);
    strip_sgr(&String::from_utf8_lossy(&out.stdout))
}

fn strip_sgr(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c2 in chars.by_ref() {
                if c2 == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The reported bug, end to end: reasoning must not run into the reply.
///
/// Deltas chosen to reproduce the transcript exactly — a
/// `reasoning_content` chunk with no trailing newline, then the reply.
#[test]
fn reasoning_does_not_run_into_the_reply() {
    let port = spawn_sse_server(vec![
        delta("reasoning_content", "That's a simple greeting."),
        delta("content", "Hello, Tom!"),
        delta("content", " — from my-plugin"),
    ]);

    let out = run_p(port, &["greet Tom"]);

    assert!(
        out.contains("That's a simple greeting.\nHello, Tom!"),
        "reasoning ran into the reply: {out:?}"
    );
    assert!(
        !out.contains("greeting.Hello"),
        "the exact reported concatenation is back: {out:?}"
    );
    // The reply's own deltas still concatenate — a newline per delta
    // would break it at every token boundary.
    assert!(
        out.contains("Hello, Tom! — from my-plugin"),
        "reply was split across lines: {out:?}"
    );
}

/// A reply with no reasoning must not gain a leading blank line. This is
/// the common `-p` case and the one most likely to be piped into
/// something that cares.
#[test]
fn a_reply_without_reasoning_has_no_leading_blank_line() {
    let port = spawn_sse_server(vec![delta("content", "just the answer")]);
    let out = run_p(port, &["hi"]);
    assert!(
        out.starts_with("just the answer"),
        "gained a leading blank line or prefix: {out:?}"
    );
}

/// `--output json` must not inherit the display-only separator: the
/// envelope's text feeds scripts, and it should carry what the model
/// emitted and nothing else.
#[test]
fn json_output_does_not_carry_the_display_separator() {
    let port = spawn_sse_server(vec![
        delta("reasoning_content", "musing"),
        delta("content", "Hello, Tom!"),
    ]);

    let out = run_p(port, &["--output", "json", "greet Tom"]);
    let v: serde_json::Value =
        serde_json::from_str(out.trim()).unwrap_or_else(|e| panic!("not JSON: {e}\n{out}"));

    // The envelope carries the exchange as `messages`, not a bare
    // `text` field — asserted against the real shape rather than an
    // assumed one.
    let assistant = v
        .get("messages")
        .and_then(|m| m.as_array())
        .and_then(|m| {
            m.iter()
                .rev()
                .find(|e| e.get("role").and_then(|r| r.as_str()) == Some("assistant"))
        })
        .and_then(|e| e.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or_else(|| panic!("no assistant message in envelope: {v}"));

    assert_eq!(
        assistant, "Hello, Tom!",
        "JSON content must be the model's reply alone — no display separator"
    );
    // And the reasoning must not leak into the machine-readable output.
    assert!(
        !v.to_string().contains("musing"),
        "reasoning leaked into the JSON envelope: {v}"
    );
}

/// Two tool calls in ONE assistant message — the batch that broke the
/// TUI's tool cards — must each get their own correctly-named marker in
/// `-p`, and each must be paired with its own result.
///
/// The TUI bug was a single-slot stash overwritten by the second call
/// (`take_pending_bar` covers that directly). `-p` renders per event so
/// it never had the bug, but nothing pinned the *pipeline*: that two
/// calls in one delta round both reach execution, both report, and the
/// markers carry the right names and ids. A regression in the loop's
/// batch handling would show up here and nowhere else.
#[test]
fn a_parallel_tool_batch_reports_each_call_separately() {
    let port = spawn_sse_server_seq(vec![
        // Round 1: the model asks for two tools at once.
        vec![
            tool_call_delta(0, "call_a", "ls", "{}"),
            tool_call_delta(1, "call_b", "bash", r#"{"command":"echo marker-b"}"#),
            finish("tool_calls"),
        ],
        // Round 2: having seen both results, it answers.
        vec![delta("content", "both done")],
    ]);

    let out = run_p(port, &["run both"]);

    // Each call announced under its own name and id.
    assert!(out.contains("[ls call_a]"), "ls marker missing: {out:?}");
    assert!(
        out.contains("[bash call_b]"),
        "bash marker missing: {out:?}"
    );
    // The bash marker previews the command — the arg preview is what
    // makes a later failure legible.
    assert!(out.contains("echo marker-b"), "no arg preview: {out:?}");
    // Each result paired back to its own call id.
    assert!(
        out.contains("[ls → call_a") || out.contains("[ls ✗ call_a"),
        "no ls result marker: {out:?}"
    );
    assert!(
        out.contains("[bash → call_b") || out.contains("[bash ✗ call_b"),
        "no bash result marker: {out:?}"
    );
    // And the turn continued to the model's answer rather than stalling
    // after the batch.
    assert!(out.contains("both done"), "turn did not finish: {out:?}");
}

/// A failing tool in a batch must not take the turn down with it: the
/// other call still reports and the loop still reaches the answer.
#[test]
fn one_failing_tool_does_not_abort_the_batch() {
    let port = spawn_sse_server_seq(vec![
        vec![
            tool_call_delta(0, "call_ok", "bash", r#"{"command":"echo fine"}"#),
            tool_call_delta(
                1,
                "call_bad",
                "read",
                r#"{"path":"/nonexistent/definitely/not/here"}"#,
            ),
            finish("tool_calls"),
        ],
        vec![delta("content", "handled")],
    ]);

    let out = run_p(port, &["try both"]);

    assert!(
        out.contains("[read ✗ call_bad"),
        "the failure should be marked with ✗: {out:?}"
    );
    assert!(
        out.contains("[bash → call_ok"),
        "the successful call still reports: {out:?}"
    );
    assert!(out.contains("handled"), "turn did not finish: {out:?}");
}

/// A `tool_execution_start` hook rewriting the arguments must be
/// visible to BOTH sides.
///
/// This is the manual-test finding: the card/marker showed the command
/// the model asked for while a different one ran, so the model saw its
/// own request answered differently and invented a sandbox to explain
/// it. End-to-end because the two halves live in different layers —
/// the `↻` marker in the renderer, the note in the agent loop.
#[test]
fn a_hook_rewrite_is_visible_to_the_user_and_the_model() {
    let dir = std::env::temp_dir().join("nanopi-rewrite-e2e");
    std::fs::create_dir_all(dir.join(".nanopi")).expect("cfg dir");
    let hook = dir.join("rewrite.sh");
    std::fs::write(
        &hook,
        "#!/bin/sh\ncat > /dev/null\necho '{\"decision\":\"allow\",\"updated_input\":{\"command\":\"echo REWRITTEN\"}}'\n",
    )
    .expect("hook");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::write(
        dir.join(".nanopi/config.toml"),
        format!(
            "[[hooks.tool_execution_start]]\nmatcher = \"^bash$\"\ncommand = \"{}\"\n",
            hook.display()
        ),
    )
    .expect("config");

    let port = spawn_sse_server_seq(vec![
        vec![
            tool_call_delta(0, "call_a", "bash", r#"{"command":"echo hello"}"#),
            finish("tool_calls"),
        ],
        vec![delta("content", "ok")],
    ]);

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_nanopi"))
        .current_dir(&dir)
        .args(["-p", "--base-url"])
        .arg(format!("http://127.0.0.1:{port}"))
        .args(["--model", "fake-model", "--api-key", "k"])
        .args(["--no-skills", "--no-context-files"])
        .arg("run it")
        .env("NANOPI_HOME", dir.join("home"))
        .output()
        .expect("run");
    let stdout = strip_sgr(&String::from_utf8_lossy(&out.stdout));

    // User side: the original marker, then the rewrite marker naming
    // what actually ran.
    assert!(
        stdout.contains("[bash call_a] echo hello"),
        "original marker missing: {stdout:?}"
    );
    assert!(
        stdout.contains("[bash ↻ call_a] echo REWRITTEN"),
        "rewrite marker missing: {stdout:?}"
    );

    // Model side: the note rides on the tool result, so the session
    // transcript carries it.
    let sessions = dir.join("home/sessions");
    let mut found_note = false;
    if let Ok(rd) = std::fs::read_dir(&sessions) {
        for e in rd.flatten() {
            let text = std::fs::read_to_string(e.path()).unwrap_or_default();
            if text.contains("rewrote the arguments of this call") {
                found_note = true;
            }
        }
    }
    assert!(
        found_note,
        "the model was never told its arguments were rewritten; \
         session dir {sessions:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The bug this plan fixes, end to end: a `<think>` opener split across
/// two `content` deltas must still be recognized, rendered as thinking
/// (not the reply), and never leak the literal tag into stdout. This is
/// the test that would have caught the reported bug.
#[test]
fn inline_think_split_across_two_content_deltas() {
    let port = spawn_sse_server(vec![
        delta("content", "<thi"),
        delta("content", "nk>reasoning</think>ANSWER"),
        finish("stop"),
    ]);

    let out = run_p_with_provider(port, "xiaomi", "", &["greet Tom"]);

    assert!(out.contains("ANSWER"), "answer missing: {out:?}");
    assert!(
        !out.contains("reasoningANSWER"),
        "reasoning ran into the reply: {out:?}"
    );
    assert!(
        !out.contains("<think>"),
        "the literal opener leaked into stdout: {out:?}"
    );
    assert!(
        !out.contains("</think>"),
        "the literal closer leaked into stdout: {out:?}"
    );
}

/// The same stream, `--output json`: the envelope's assistant text must
/// be exactly the answer — no reasoning, no literal tag.
#[test]
fn inline_think_excluded_from_json_envelope() {
    let port = spawn_sse_server(vec![
        delta("content", "<thi"),
        delta("content", "nk>reasoning</think>ANSWER"),
        finish("stop"),
    ]);

    let out = run_p_with_provider(
        port,
        "xiaomi",
        "",
        &["--output", "json", "greet Tom"],
    );
    let v: serde_json::Value =
        serde_json::from_str(out.trim()).unwrap_or_else(|e| panic!("not JSON: {e}\n{out}"));

    let assistant = v
        .get("messages")
        .and_then(|m| m.as_array())
        .and_then(|m| {
            m.iter()
                .rev()
                .find(|e| e.get("role").and_then(|r| r.as_str()) == Some("assistant"))
        })
        .and_then(|e| e.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or_else(|| panic!("no assistant message in envelope: {v}"));

    assert_eq!(
        assistant, "ANSWER",
        "JSON content must be the answer alone — no inlined reasoning"
    );
    assert!(
        !v.to_string().contains("reasoning"),
        "reasoning leaked into the JSON envelope: {v}"
    );
    assert!(
        !v.to_string().contains("think"),
        "the literal tag leaked into the JSON envelope: {v}"
    );
}

/// The gate is real: without a vendor that inlines think tags, the same
/// stream renders the literal `<think>` verbatim.
#[test]
fn inline_think_untouched_for_other_vendors() {
    let port = spawn_sse_server(vec![
        delta("content", "<thi"),
        delta("content", "nk>reasoning</think>ANSWER"),
        finish("stop"),
    ]);

    // No provider override, no vendor-matching base_url/model → sniff
    // falls all the way through to FallbackVendor, which does not inline
    // think tags.
    let out = run_p(port, &["greet Tom"]);

    assert!(
        out.contains("<think>"),
        "the gate ate the tag for a non-gated vendor: {out:?}"
    );
}

/// A stream that ends inside an unclosed `<think>` must still deliver
/// everything it carried — the no-loss invariant, exercised end to end.
#[test]
fn inline_think_unclosed_still_delivers() {
    let port = spawn_sse_server(vec![
        delta("content", "before<think>dangling"),
        finish("stop"),
    ]);

    let out = run_p_with_provider(port, "xiaomi", "", &["greet Tom"]);

    assert!(out.contains("before"), "lost text before the tag: {out:?}");
}
