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
fn inline_think_mid_answer_reaches_stdout_literally() {
    let port = spawn_sse_server(vec![
        delta("content", "ANSWER first, "),
        delta("content", "then <think>reasoning</think> more"),
        finish("stop"),
    ]);

    // No provider override, no vendor-matching base_url/model → sniff
    // falls through to FallbackVendor. The splitter is on by default
    // regardless of vendor now (position rule, not a vendor gate) — but
    // this `<think>` arrives AFTER other text, so it's not a leading
    // block and must render literally.
    let out = run_p(port, &["greet Tom"]);

    assert!(
        out.contains("<think>reasoning</think>"),
        "a non-leading <think> block must render literally: {out:?}"
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

/// `inline_think_tags = false` in config.toml is the escape hatch: it
/// disables the splitter entirely, so even a genuinely LEADING `<think>`
/// block (which would otherwise be reclassified as reasoning) renders as
/// plain text.
#[test]
fn inline_think_tags_false_disables_splitting_even_for_leading_block() {
    let port = spawn_sse_server(vec![
        delta("content", "<thi"),
        delta("content", "nk>reasoning</think>ANSWER"),
        finish("stop"),
    ]);

    let out = run_p_with_provider(
        port,
        "xiaomi",
        "inline_think_tags = false",
        &["greet Tom"],
    );

    assert!(
        out.contains("<think>reasoning</think>"),
        "escape hatch should leave a leading block untouched: {out:?}"
    );
}

/// A `SettingsError` must abort startup, not degrade to "no hooks".
///
/// The matcher validator itself was correct and well-tested
/// (`settings.rs` asserts `load_settings` returns the error), but both
/// call sites — here and `mode/tui.rs` — caught it, printed
/// `warning: failed to load settings`, and substituted
/// `HooksConfig::default()`. That drops *every* hook in the file, not
/// just the invalid entry, so a config with one bad `input` matcher and
/// a working `check-rm-rf.sh` veto hook ran with the veto silently
/// disarmed. Nothing pinned the call site, which is why a green unit
/// test coexisted with the defect for the whole of v0.12.
///
/// Asserting on the exit status is the load-bearing part: the old code
/// also left the sibling hook's marker file absent, so only "did the
/// process refuse to run" separates fixed from broken.
#[test]
fn invalid_hook_matcher_refuses_to_start_rather_than_disarming_every_hook() {
    let dir = std::env::temp_dir().join("nanopi-p-e2e-bad-matcher");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".nanopi")).expect("cfg dir");
    let marker = dir.join("sibling-hook-ran");
    std::fs::write(
        dir.join(".nanopi/config.toml"),
        format!(
            // `input` carries no tool name, so any matcher other than
            // `*` provably never fires.
            "[[hooks.input]]\nmatcher = \"hello\"\ncommand = \"true\"\n\n\
             [[hooks.session_start]]\nmatcher = \"*\"\ncommand = \"touch {}\"\n",
            marker.display()
        ),
    )
    .expect("write config");

    // No SSE server: a settings error is resolved before any request,
    // so reaching the network at all would itself be the failure.
    let out = Command::new(env!("CARGO_BIN_EXE_nanopi"))
        .current_dir(&dir)
        .args(["-p", "--base-url", "http://127.0.0.1:1"])
        .args(["--model", "fake-model", "--api-key", "not-a-real-key"])
        .args(["--no-skills", "--no-context-files"])
        .arg("hello world")
        .env("NANOPI_HOME", dir.join("home"))
        .output()
        .expect("run nanopi -p");

    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        !out.status.success(),
        "an unsatisfiable matcher must be fatal, not a warning; \
         exit={:?} stderr={stderr:?}",
        out.status.code()
    );
    assert!(
        stderr.contains("can never match"),
        "the error must name the offending matcher: {stderr:?}"
    );
    assert!(
        !stderr.contains("warning: failed to load settings"),
        "the old fail-open path is gone; this must be an error, \
         not a warning: {stderr:?}"
    );
}

/// Control for the test above: a *valid* hook config must still start
/// and complete a turn. Without this, "make every settings error fatal"
/// could be satisfied by refusing to start whenever hooks are present.
#[test]
fn valid_hook_config_still_starts_and_completes_a_turn() {
    let port = spawn_sse_server(vec![delta("content", "ANSWER"), finish("stop")]);

    let dir = std::env::temp_dir().join("nanopi-p-e2e-good-matcher");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".nanopi")).expect("cfg dir");
    std::fs::write(
        dir.join(".nanopi/config.toml"),
        "[[hooks.input]]\nmatcher = \"*\"\ncommand = \"true\"\n",
    )
    .expect("write config");

    let out = Command::new(env!("CARGO_BIN_EXE_nanopi"))
        .current_dir(&dir)
        .args(["-p", "--base-url"])
        .arg(format!("http://127.0.0.1:{port}"))
        .args(["--model", "fake-model", "--api-key", "not-a-real-key"])
        .args(["--no-skills", "--no-context-files"])
        .arg("hello world")
        .env("NANOPI_HOME", dir.join("home"))
        .output()
        .expect("run nanopi -p");

    let stdout = strip_sgr(&String::from_utf8_lossy(&out.stdout));
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        out.status.success(),
        "a valid `*` matcher must not be rejected; stderr={stderr:?}"
    );
    assert!(
        stdout.contains("ANSWER"),
        "the turn should still complete: {stdout:?}"
    );
}

/// `--no-session` must be ephemeral: after the run, `~/.nanopi/sessions/`
/// holds no session file and no `active` pointer, yet the JSON envelope
/// still carries the exchange (read back before the temp file is
/// deleted). Mirrors PI's `SessionManager.inMemory`.
#[test]
fn no_session_leaves_no_session_file_behind() {
    let port = spawn_sse_server(vec![delta("content", "ANSWER"), finish("stop")]);

    let dir = std::env::temp_dir().join(format!("nanopi-p-e2e-nosession-{port}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("tmp cwd");
    let home = dir.join("home");

    let out = Command::new(env!("CARGO_BIN_EXE_nanopi"))
        .current_dir(&dir)
        .args(["-p", "--output", "json", "--base-url"])
        .arg(format!("http://127.0.0.1:{port}"))
        .args(["--model", "fake-model", "--api-key", "not-a-real-key"])
        .args(["--no-hooks", "--no-skills", "--no-context-files"])
        .arg("--no-session")
        .arg("hello world")
        .env("NANOPI_HOME", &home)
        .output()
        .expect("run nanopi -p --no-session");

    let stdout = strip_sgr(&String::from_utf8_lossy(&out.stdout));

    // The envelope still carries the exchange.
    let v: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("not JSON: {e}\n{stdout}"));
    let has_answer = v
        .get("messages")
        .and_then(|m| m.as_array())
        .map(|m| m.iter().any(|e| e.to_string().contains("ANSWER")))
        .unwrap_or(false);
    assert!(has_answer, "envelope lost the exchange: {v}");

    // No session file, no active pointer — nothing persisted.
    let sessions = home.join("sessions");
    let leftover: Vec<_> = std::fs::read_dir(&sessions)
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    assert!(
        leftover.is_empty(),
        "--no-session persisted something: {leftover:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `--no-session` combined with a resume flag is a contradiction (one
/// says "don't persist", the other "reload from disk"). It must be
/// rejected up front with a clear error, not silently honored.
#[test]
fn no_session_rejects_incompatible_resume_flags() {
    for flag in ["--continue", "--fork=abc", "--session=abc"] {
        let dir = std::env::temp_dir()
            .join(format!("nanopi-p-e2e-nosession-bad{}", flag.replace(['-', '='], "")));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("tmp cwd");

        let out = Command::new(env!("CARGO_BIN_EXE_nanopi"))
            .current_dir(&dir)
            .args(["-p", "--base-url", "http://127.0.0.1:1"])
            .args(["--model", "fake-model", "--api-key", "not-a-real-key"])
            .args(["--no-hooks", "--no-skills", "--no-context-files"])
            .arg("--no-session")
            .arg(flag)
            .arg("hello")
            .env("NANOPI_HOME", dir.join("home"))
            .output()
            .expect("run nanopi -p");

        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            !out.status.success(),
            "--no-session {flag} must be rejected; exit={:?} stderr={stderr:?}",
            out.status.code()
        );
        assert!(
            stderr.contains("--no-session cannot be combined"),
            "error must explain the conflict for {flag}: {stderr:?}"
        );
    }
}

// ── Child-process contract (01-04): --session-file, limits, allowlist ──

/// Like [`spawn_sse_server_seq`], but also records each request body so
/// a test can assert what context the child sent to the model.
fn spawn_recording_server(
    responses: Vec<Vec<String>>,
) -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    spawn_recording_server_delayed(responses, Vec::new())
}

/// [`spawn_recording_server`] with a per-response delay (ms) applied
/// after the request is logged and before the response is written.
fn spawn_recording_server_delayed(
    responses: Vec<Vec<String>>,
    delays_ms: Vec<u64>,
) -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("local_addr").port();
    let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let log2 = log.clone();
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
            let Ok(mut stream) = stream else { continue };
            let mut seen = Vec::new();
            let mut byte = [0u8; 1];
            while !seen.ends_with(b"\r\n\r\n") {
                match stream.read(&mut byte) {
                    Ok(0) | Err(_) => break,
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
            log2.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&req).to_string());
            let body = &bodies[n.min(bodies.len() - 1)];
            if let Some(ms) = delays_ms.get(n) {
                std::thread::sleep(std::time::Duration::from_millis(*ms));
            }
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
    (port, log)
}

/// A finish chunk that also carries usage numbers.
fn finish_with_usage(reason: &str, prompt: u32, completion: u32) -> String {
    format!(
        r#"{{"id":"x","choices":[{{"index":0,"delta":{{}},"finish_reason":"{reason}"}}],"usage":{{"prompt_tokens":{prompt},"completion_tokens":{completion}}}}}"#
    )
}

/// Run a child-style `nanopi -p --output json` in `dir` with HOME and
/// NANOPI_HOME pointed inside it, stdin null, and a hard timeout so a
/// child that waits on a prompt fails the test instead of hanging it.
fn run_child(
    dir: &std::path::Path,
    port: u16,
    args: &[&str],
    envs: &[(&str, &str)],
) -> (std::process::ExitStatus, serde_json::Value, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nanopi"));
    cmd.current_dir(dir)
        .args(["-p", "--output", "json", "--base-url"])
        .arg(format!("http://127.0.0.1:{port}"))
        .args(["--model", "fake-model", "--api-key", "not-a-real-key"])
        .args(["--no-hooks", "--no-skills", "--no-context-files"])
        .args(args)
        .env("HOME", dir.join("home"))
        .env("NANOPI_HOME", dir.join("home/.nanopi"))
        .env_remove("NANOPI_AGENT_ID")
        .env_remove("NANOPI_PARENT_PID")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("spawn nanopi");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let status = loop {
        if let Some(s) = child.try_wait().expect("try_wait") {
            break s;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("nanopi child hung (stdin null, non-interactive)");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    let stdout = strip_sgr(&stdout);
    let v: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("not JSON: {e}\nstdout={stdout}\nstderr={stderr}"));
    (status, v, stderr)
}

fn fresh_dir(tag: &str, port: u16) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("nanopi-p-e2e-{tag}-{port}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("home")).expect("tmp dir");
    dir
}

#[test]
fn session_file_writes_transcript_and_leaves_active_pointer_alone() {
    let (port, _log) =
        spawn_recording_server(vec![vec![delta("content", "REPLY-ONE"), finish("stop")]]);
    let dir = fresh_dir("sessfile", port);
    let tpath = dir.join("agents/t.jsonl");
    let (status, v, stderr) = run_child(
        &dir,
        port,
        &["--session-file", tpath.to_str().unwrap(), "FIRST-MSG"],
        &[],
    );
    assert!(status.success(), "exit {status:?}: {stderr}");
    assert_eq!(v["status"], "completed", "{v}");
    let t = std::fs::read_to_string(&tpath).expect("transcript at --session-file");
    assert!(t.contains("FIRST-MSG") && t.contains("REPLY-ONE"), "{t}");
    let active = dir.join("home/.nanopi/sessions/active");
    assert!(
        !active.exists(),
        "active-session pointer must not be written"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn session_file_resume_appends_and_replays_history() {
    let (port, log) = spawn_recording_server(vec![
        vec![delta("content", "REPLY-ONE"), finish("stop")],
        vec![delta("content", "REPLY-TWO"), finish("stop")],
    ]);
    let dir = fresh_dir("sessresume", port);
    let tpath = dir.join("t.jsonl");
    let tp = tpath.to_str().unwrap();
    let (s1, _, e1) = run_child(&dir, port, &["--session-file", tp, "FIRST-MSG"], &[]);
    assert!(s1.success(), "{e1}");
    let (s2, v2, e2) = run_child(&dir, port, &["--session-file", tp, "SECOND-MSG"], &[]);
    assert!(s2.success(), "{e2}");
    let reqs = log.lock().unwrap().clone();
    assert_eq!(reqs.len(), 2, "one request per run");
    assert!(
        reqs[1].contains("FIRST-MSG") && reqs[1].contains("REPLY-ONE"),
        "second run must send first run's history: {}",
        reqs[1]
    );
    let t = std::fs::read_to_string(&tpath).unwrap();
    for needle in ["FIRST-MSG", "REPLY-ONE", "SECOND-MSG", "REPLY-TWO"] {
        assert!(t.contains(needle), "transcript missing {needle}: {t}");
    }
    assert_eq!(t.matches("\"type\":\"session\"").count(), 1, "one header");
    assert!(v2.to_string().contains("REPLY-TWO"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn limit_max_turns_reports_limit_reached() {
    let port = spawn_sse_server(vec![
        tool_call_delta(0, "c1", "ls", r#"{"path":"."}"#),
        finish("tool_calls"),
    ]);
    let dir = fresh_dir("maxturns", port);
    let (status, v, stderr) = run_child(&dir, port, &["--max-turns", "2", "go"], &[]);
    assert_eq!(status.code(), Some(0), "{stderr}");
    assert_eq!(v["status"], "limit_reached", "{v}");
    assert_eq!(v["limit"], "max_turns", "{v}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn limit_token_budget_reports_limit_reached() {
    let port = spawn_sse_server(vec![
        tool_call_delta(0, "c1", "ls", r#"{"path":"."}"#),
        finish_with_usage("tool_calls", 25, 25),
    ]);
    let dir = fresh_dir("budget", port);
    let (status, v, stderr) = run_child(&dir, port, &["--token-budget", "10", "go"], &[]);
    assert_eq!(status.code(), Some(0), "{stderr}");
    assert_eq!(v["status"], "limit_reached", "{v}");
    assert_eq!(v["limit"], "token_budget", "{v}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn tools_allowlist_agent_strips_subagent_and_denies_unlisted_in_band() {
    let (port, log) = spawn_recording_server(vec![
        vec![
            tool_call_delta(0, "c1", "bash", r#"{"command":"echo hi"}"#),
            finish("tool_calls"),
        ],
        vec![delta("content", "DONE"), finish("stop")],
    ]);
    let dir = fresh_dir("agentallow", port);
    let brief = dir.join("brief.md");
    let (status, v, stderr) = run_child(
        &dir,
        port,
        &[
            "--tools",
            "read,subagent",
            "--brief",
            brief.to_str().unwrap(),
            "go",
        ],
        &[("NANOPI_AGENT_ID", "a1")],
    );
    assert!(status.success(), "{stderr}");
    assert_eq!(v["agent_id"], "a1", "{v}");
    assert_eq!(v["report_path"], dir.join("report.md").to_str().unwrap(), "{v}");
    let msgs = v["messages"].as_array().unwrap();
    let tool = msgs
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("tool result in envelope");
    assert_eq!(tool["is_error"], true, "{tool}");
    assert!(
        tool["content"].as_str().unwrap().contains("unknown tool"),
        "{tool}"
    );
    let first = &log.lock().unwrap()[0];
    let req: serde_json::Value = serde_json::from_str(first).expect("request JSON");
    let names: Vec<String> = req["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(names, vec!["read"], "only listed tools, subagent stripped");
    let _ = std::fs::remove_dir_all(&dir);
}

// ── Brief-driven child (01-06, RT-09) ──

fn write_brief(dir: &std::path::Path, task: &str) -> std::path::PathBuf {
    let p = dir.join("agents/a1/brief.md");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    let body = format!("# Brief\n\n## Task\n\n{task}\n\n<!-- nanopi:amendments -->\n");
    std::fs::write(&p, body).unwrap();
    p
}

fn req_user_texts(req: &str) -> Vec<String> {
    let v: serde_json::Value = serde_json::from_str(req).expect("request JSON");
    v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| m["content"].to_string())
        .collect()
}

#[test]
fn brief_task_is_sent_as_the_user_task() {
    let (port, log) = spawn_recording_server(vec![vec![
        delta("content", "- [x] task — ok"),
        finish("stop"),
    ]]);
    let dir = fresh_dir("brieftask", port);
    let b = write_brief(&dir, "BRIEF-TASK-TEXT");
    let (status, v, stderr) = run_child(&dir, port, &["--brief", b.to_str().unwrap()], &[]);
    assert!(status.success(), "{stderr}");
    assert_eq!(v["status"], "completed", "{v}");
    let reqs = log.lock().unwrap().clone();
    assert!(
        req_user_texts(&reqs[0]).join("\n").contains("BRIEF-TASK-TEXT"),
        "{}",
        reqs[0]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn brief_amendment_is_injected_before_the_next_llm_call() {
    let (port, log) = spawn_recording_server_delayed(
        vec![
            vec![
                tool_call_delta(0, "c1", "ls", r#"{"path":"."}"#),
                finish("tool_calls"),
            ],
            vec![delta("content", "DONE"), finish("stop")],
        ],
        vec![2500],
    );
    let dir = fresh_dir("briefamend", port);
    let b = write_brief(&dir, "do the thing");
    let b2 = b.clone();
    let log2 = log.clone();
    let appender = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while log2.lock().unwrap().is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let mut f = std::fs::OpenOptions::new().append(true).open(&b2).unwrap();
        f.write_all(b"\n## Amendment 1\n\nAlso do X\n").unwrap();
    });
    let (status, _v, stderr) = run_child(&dir, port, &["--brief", b.to_str().unwrap()], &[]);
    appender.join().unwrap();
    assert!(status.success(), "{stderr}");
    let reqs = log.lock().unwrap().clone();
    assert!(reqs.len() >= 2, "{reqs:?}");
    assert!(!req_user_texts(&reqs[0]).join("\n").contains("Also do X"));
    let second = req_user_texts(&reqs[1]).join("\n");
    assert!(second.contains("Also do X"), "{second}");
    assert_eq!(
        second.matches("Amendment 1 to your brief").count(),
        1,
        "{second}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn brief_self_check_is_bounded_to_two_extra_turns() {
    let (port, log) = spawn_recording_server(vec![
        vec![delta("content", "ANSWER"), finish("stop")],
        vec![delta("content", "- [ ] a — todo"), finish("stop")],
    ]);
    let dir = fresh_dir("briefself", port);
    let b = write_brief(&dir, "SELF-CHECK-TASK");
    let (status, v, stderr) = run_child(&dir, port, &["--brief", b.to_str().unwrap()], &[]);
    assert!(status.success(), "{stderr}");
    assert_eq!(v["status"], "completed", "{v}");
    let reqs = log.lock().unwrap().clone();
    assert_eq!(reqs.len(), 3, "1 task + at most 2 self-check turns");
    let last_user = req_user_texts(&reqs[1]).pop().unwrap();
    assert!(last_user.contains("SELF-CHECK-TASK"), "{last_user}");
    assert!(last_user.contains("checklist"), "{last_user}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn brief_self_check_stops_early_when_all_done() {
    let (port, log) = spawn_recording_server(vec![
        vec![delta("content", "ANSWER"), finish("stop")],
        vec![delta("content", "- [x] a — done"), finish("stop")],
    ]);
    let dir = fresh_dir("briefselfdone", port);
    let b = write_brief(&dir, "t");
    let (status, _v, stderr) = run_child(&dir, port, &["--brief", b.to_str().unwrap()], &[]);
    assert!(status.success(), "{stderr}");
    assert_eq!(log.lock().unwrap().len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn brief_report_written_with_checklist() {
    let reply = vec![
        delta("content", "- [x] write code — done\n- [ ] docs — skipped"),
        finish("stop"),
    ];
    let (port, _log) = spawn_recording_server(vec![
        vec![delta("content", "ANSWER"), finish("stop")],
        reply,
    ]);
    let dir = fresh_dir("briefreport", port);
    let b = write_brief(&dir, "t");
    let (status, v, stderr) = run_child(&dir, port, &["--brief", b.to_str().unwrap()], &[]);
    assert!(status.success(), "{stderr}");
    let rp = b.parent().unwrap().join("report.md");
    assert_eq!(v["report_path"], rp.to_str().unwrap(), "{v}");
    let r = std::fs::read_to_string(&rp).expect("report.md");
    assert!(r.contains("## Checklist"), "{r}");
    assert!(r.contains("- [x] write code — done"), "{r}");
    assert!(r.contains("- [ ] docs — skipped"), "{r}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&rp).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn brief_report_on_limit_has_no_self_check() {
    let (port, log) = spawn_recording_server(vec![vec![
        tool_call_delta(0, "c1", "ls", r#"{"path":"."}"#),
        finish("tool_calls"),
    ]]);
    let dir = fresh_dir("brieflimit", port);
    let b = write_brief(&dir, "t");
    let (status, v, stderr) = run_child(
        &dir,
        port,
        &["--max-turns", "1", "--brief", b.to_str().unwrap()],
        &[],
    );
    assert_eq!(status.code(), Some(0), "{stderr}");
    assert_eq!(v["status"], "limit_reached", "{v}");
    assert_eq!(log.lock().unwrap().len(), 1, "no self-check after a limit");
    let r = std::fs::read_to_string(b.parent().unwrap().join("report.md")).expect("report.md");
    assert!(r.contains("limit_reached") && r.contains("## Checklist"), "{r}");
    assert!(r.contains("- [ ] checklist missing"), "{r}");
    let _ = std::fs::remove_dir_all(&dir);
}
