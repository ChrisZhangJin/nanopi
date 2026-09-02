//! End-to-end WASM extension test.
//!
//! Loads a real component — the one built from `examples/wasm-plugin`,
//! checked in at `tests/fixtures/` — and calls its tools through the
//! same code path the agent uses.
//!
//! The unit tests in `src/wasm/` cover the pieces in isolation and all
//! passed while three separate integration bugs were live: exports
//! namespaced under a WIT `interface` so the host could never resolve
//! them, `reference-types` left disabled so no component would
//! compile, and an error chain flattened by `{}` so the cause was
//! invisible. Only loading an actual `.wasm` catches that class of
//! bug, which is why the fixture is committed rather than built on
//! demand — the toolchain to produce it (`wasm32-wasip1` std plus
//! `wasm-tools`) is a heavier ask than running the tests.
//!
//! To regenerate the fixture after changing `examples/wasm-plugin`:
//!
//! ```bash
//! cargo build --manifest-path examples/wasm-plugin/Cargo.toml \
//!   --target wasm32-wasip1 --release
//! wasm-tools component embed wit/ \
//!   examples/wasm-plugin/target/wasm32-wasip1/release/nanopi_example_plugin.wasm \
//!   -o /tmp/embedded.wasm --world extension-commands
//! wasm-tools component new /tmp/embedded.wasm \
//!   -o tests/fixtures/example-plugin.component.wasm
//! ```
//!
//! Note `--world extension-commands` — this fixture registers slash
//! commands. `wit/` now declares two worlds, so `--world` is no longer
//! optional here.
//!
//! TWO FIXTURES, TWO WORLDS, ON PURPOSE. `runaway-plugin` stays on
//! `--world extension` and must keep doing so: besides being the
//! hang-breaker fixture, it is the only committed component WITHOUT
//! `list-commands`, and therefore the only end-to-end proof that the
//! host resolves that export optionally. Retarget it and the
//! backward-compatibility test below silently stops testing anything.

#![cfg(feature = "wasm")]

use std::path::PathBuf;

use nanopi::command::CommandAction;
use nanopi::wasm::loader::PluginEngine;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/example-plugin.component.wasm")
}

/// The whole path: compile the component, read its tool list, call
/// both tools, get correct answers back.
#[test]
fn loads_real_component_and_executes_its_tools() {
    let engine = PluginEngine::new().expect("engine init");
    let (bridge, specs) = engine
        .load(&fixture(), Vec::new(), std::env::temp_dir(), false, false)
        .expect("example component must load");

    // list-tools reached the host intact.
    let mut names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
    names.sort();
    assert_eq!(names, vec!["fetch", "readfile", "rot13", "wordcount"]);

    // Descriptions and schemas survive too — these are what the model
    // sees, so an empty or mangled one is a silent quality failure.
    let rot13 = specs.iter().find(|s| s.name == "rot13").unwrap();
    assert!(rot13.description.contains("ROT13"), "{}", rot13.description);
    assert_eq!(rot13.parameters["type"], "object");
    assert_eq!(rot13.parameters["properties"]["text"]["type"], "string");

    // execute-tool round-trips arguments and results.
    let out = bridge
        .execute_tool("rot13", r#"{"text":"Hello, World"}"#)
        .expect("rot13 call");
    assert_eq!(out.content, "Uryyb, Jbeyq");
    assert!(!out.is_error);

    let out = bridge
        .execute_tool("wordcount", r#"{"text":"one two three"}"#)
        .expect("wordcount call");
    assert!(out.content.starts_with("3 words"), "{}", out.content);
    assert!(!out.is_error);
}

/// Calling repeatedly must keep working — the plugin resets its arena
/// per call, and a bug there would show up as garbage on call two
/// rather than call one.
#[test]
fn repeated_calls_stay_correct() {
    let engine = PluginEngine::new().expect("engine init");
    let (bridge, _) = engine.load(&fixture(), Vec::new(), std::env::temp_dir(), false, false).expect("load");

    for _ in 0..20 {
        let out = bridge
            .execute_tool("rot13", r#"{"text":"abc"}"#)
            .expect("call");
        assert_eq!(out.content, "nop");
    }
}

/// A name the plugin never advertised is rejected by the host before
/// it reaches the guest.
#[test]
fn unknown_tool_name_is_rejected() {
    let engine = PluginEngine::new().expect("engine init");
    let (bridge, _) = engine.load(&fixture(), Vec::new(), std::env::temp_dir(), false, false).expect("load");

    let err = bridge
        .execute_tool("definitely_not_a_tool", "{}")
        .unwrap_err();
    assert!(err.contains("does not export"), "{err}");
}

/// The plugin reports bad input as a failed tool call rather than
/// trapping — the model gets something it can correct from.
#[test]
fn plugin_reports_bad_arguments_as_tool_error() {
    let engine = PluginEngine::new().expect("engine init");
    let (bridge, _) = engine.load(&fixture(), Vec::new(), std::env::temp_dir(), false, false).expect("load");

    // `text` missing entirely.
    let out = bridge.execute_tool("rot13", r#"{}"#).expect("no trap");
    assert!(out.is_error, "missing field should be a tool error");
    assert!(out.content.contains("text"), "{}", out.content);

    // Not even JSON.
    let out = bridge
        .execute_tool("rot13", "this is not json")
        .expect("no trap");
    assert!(out.is_error);
}

/// A core module (not a component) must be refused with a message
/// that says why, not a bare "translation error".
#[test]
fn core_module_is_rejected_with_a_useful_message() {
    let engine = PluginEngine::new().expect("engine init");
    let mut p = std::env::temp_dir();
    p.push(format!("nanopi-core-mod-{}.wasm", std::process::id()));
    // Smallest valid core module: magic + version.
    std::fs::write(&p, [0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00]).unwrap();

    match engine.load(&p, Vec::new(), std::env::temp_dir(), false, false) {
        Ok(_) => panic!("a core module is not a component and must be refused"),
        Err(e) => assert!(e.contains("compile") || e.contains("list-tools"), "{e}"),
    }
    let _ = std::fs::remove_file(&p);
}

// ── host-fs-read capability gate ────────────────────────────────────

fn scratch_dir(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("nanopi-fs-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// With `allow_fs = false` (the default), the host refuses before it
/// ever touches the filesystem — and says which knob to turn.
#[test]
fn fs_read_denied_without_allow_fs() {
    let dir = scratch_dir("denied");
    std::fs::write(dir.join("secret.txt"), "classified").unwrap();

    let engine = PluginEngine::new().expect("engine");
    let (bridge, _) = engine
        .load(&fixture(), Vec::new(), dir.clone(), false, false)
        .expect("load");

    let out = bridge
        .execute_tool("readfile", r#"{"path":"secret.txt"}"#)
        .expect("no trap");
    assert!(out.is_error, "denied read must be a tool error");
    assert!(out.content.contains("allow_fs"), "{}", out.content);

    let _ = std::fs::remove_dir_all(&dir);
}

/// With the gate open, a file inside cwd reads fine.
#[test]
fn fs_read_allowed_inside_cwd() {
    let dir = scratch_dir("allowed");
    std::fs::write(dir.join("notes.txt"), "line one\nline two\n").unwrap();

    let engine = PluginEngine::new().expect("engine");
    let (bridge, _) = engine
        .load(&fixture(), Vec::new(), dir.clone(), true, false)
        .expect("load");

    let out = bridge
        .execute_tool("readfile", r#"{"path":"notes.txt"}"#)
        .expect("no trap");
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("18 bytes"), "{}", out.content);
    assert!(out.content.contains("2 lines"), "{}", out.content);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Traversal out of cwd is refused even with the gate open. The guard
/// canonicalizes first, so `../` cannot slip past a prefix check the
/// way it does against a raw string comparison.
#[test]
fn fs_read_refuses_traversal_out_of_cwd() {
    let dir = scratch_dir("traversal");
    let engine = PluginEngine::new().expect("engine");
    let (bridge, _) = engine
        .load(&fixture(), Vec::new(), dir.clone(), true, false)
        .expect("load");

    for probe in [
        r#"{"path":"../../../../etc/hostname"}"#,
        r#"{"path":"/etc/hostname"}"#,
    ] {
        let out = bridge.execute_tool("readfile", probe).expect("no trap");
        assert!(out.is_error, "{probe} should be refused, got {}", out.content);
        // Either containment refused it, or the path didn't resolve —
        // both are correct refusals; what matters is no contents leak.
        assert!(
            !out.content.contains("bytes,"),
            "{probe} leaked file contents: {}",
            out.content
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// A symlink pointing outside cwd is refused too — canonicalization
/// resolves it before the containment check, which a raw prefix test
/// would miss.
#[test]
#[cfg(unix)]
fn fs_read_refuses_symlink_escape() {
    let dir = scratch_dir("symlink");
    std::os::unix::fs::symlink("/etc/hostname", dir.join("sneaky")).unwrap();

    let engine = PluginEngine::new().expect("engine");
    let (bridge, _) = engine
        .load(&fixture(), Vec::new(), dir.clone(), true, false)
        .expect("load");

    let out = bridge
        .execute_tool("readfile", r#"{"path":"sneaky"}"#)
        .expect("no trap");
    assert!(out.is_error, "symlink escape must be refused: {}", out.content);
    assert!(out.content.contains("escapes"), "{}", out.content);

    let _ = std::fs::remove_dir_all(&dir);
}

// ── host-http-get capability gate ───────────────────────────────────

/// A throwaway HTTP server on an ephemeral loopback port.
///
/// The tests need a real socket — the gate is only meaningfully tested
/// by watching a request either arrive or not — but they must not need
/// the internet. A test that depends on `api.github.com` being
/// reachable fails for reasons that have nothing to do with nanopi,
/// and on a machine behind a filtering firewall it hangs rather than
/// failing fast.
///
/// Accepts in a loop rather than once, so a retried or probing request
/// does not leave a later one hanging on a dead listener. The thread
/// is deliberately never joined: it dies with the test process, and
/// shutdown plumbing would be more machinery than the fixture is
/// worth.
fn spawn_test_server(body: &'static str) -> u16 {
    use std::io::{Read, Write};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("local_addr").port();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = match stream {
                Ok(s) => s,
                Err(_) => continue,
            };
            // Drain the request head. Without this the client can see
            // a reset instead of the response.
            let mut seen = Vec::new();
            let mut byte = [0u8; 1];
            while !seen.ends_with(b"\r\n\r\n") {
                match stream.read(&mut byte) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => seen.push(byte[0]),
                }
            }
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\
                 Content-Type: text/plain\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        }
    });

    port
}

/// With `allow_network = false` (the default), the host refuses before
/// it opens a socket — and names the knob to turn.
///
/// The allowlist is deliberately NON-empty and *would* permit this
/// URL, so the refusal can only have come from the `allow_network`
/// gate. With an empty allowlist this test would pass even if the
/// first gate were deleted.
#[test]
fn http_get_denied_without_allow_network() {
    let port = spawn_test_server("SERVED-BODY-DENIED-CASE");
    let engine = PluginEngine::new().expect("engine");
    let (bridge, _) = engine
        .load(
            &fixture(),
            vec!["127.0.0.1".to_string()],
            std::env::temp_dir(),
            false,
            false,
        )
        .expect("load");

    let out = bridge
        .execute_tool("fetch", &format!(r#"{{"url":"http://127.0.0.1:{port}/"}}"#))
        .expect("no trap");
    assert!(out.is_error, "denied fetch must be a tool error");
    assert!(out.content.contains("allow_network"), "{}", out.content);
    // Cheapest proof no request was made: the server's distinctive
    // body never appears.
    assert!(
        !out.content.contains("SERVED-BODY-DENIED-CASE"),
        "the request should never have been sent: {}",
        out.content
    );
}

/// Gate open, but the URL's host is not covered by the allowlist.
#[test]
fn http_get_denied_when_host_not_in_allowlist() {
    let port = spawn_test_server("SERVED-BODY-ALLOWLIST-CASE");
    let engine = PluginEngine::new().expect("engine");
    let (bridge, _) = engine
        .load(
            &fixture(),
            vec!["api.github.com".to_string()],
            std::env::temp_dir(),
            false,
            true,
        )
        .expect("load");

    let out = bridge
        .execute_tool("fetch", &format!(r#"{{"url":"http://127.0.0.1:{port}/"}}"#))
        .expect("no trap");
    assert!(out.is_error, "unlisted host must be refused");
    assert!(out.content.contains("url_allowlist"), "{}", out.content);
    assert!(
        !out.content.contains("SERVED-BODY-ALLOWLIST-CASE"),
        "the request should never have been sent: {}",
        out.content
    );
}

/// An empty allowlist denies everything, even with the capability
/// switched on. This is what `config.toml.example` promises, and it is
/// what makes the capability opt-in per host rather than only per
/// plugin — do not "fix" empty to mean allow-all.
#[test]
fn http_get_empty_allowlist_denies_everything() {
    let port = spawn_test_server("SERVED-BODY-EMPTY-CASE");
    let engine = PluginEngine::new().expect("engine");
    let (bridge, _) = engine
        .load(&fixture(), Vec::new(), std::env::temp_dir(), false, true)
        .expect("load");

    let out = bridge
        .execute_tool("fetch", &format!(r#"{{"url":"http://127.0.0.1:{port}/"}}"#))
        .expect("no trap");
    assert!(
        out.is_error,
        "empty allowlist must deny, got: {}",
        out.content
    );
    assert!(
        !out.content.contains("SERVED-BODY-EMPTY-CASE"),
        "the request should never have been sent: {}",
        out.content
    );
}

/// Both gates open: the body reaches the guest verbatim.
///
/// The allowlist entry is a bare `127.0.0.1` while the server is on an
/// ephemeral port — matching is on the host, so the port is
/// irrelevant, which is exactly why the entry can be written without
/// knowing the port ahead of time.
#[test]
fn http_get_allowed_host_reaches_server() {
    let port = spawn_test_server("SERVED-BODY-ALLOWED-CASE");
    let engine = PluginEngine::new().expect("engine");
    let (bridge, _) = engine
        .load(
            &fixture(),
            vec!["127.0.0.1".to_string()],
            std::env::temp_dir(),
            false,
            true,
        )
        .expect("load");

    let out = bridge
        .execute_tool("fetch", &format!(r#"{{"url":"http://127.0.0.1:{port}/"}}"#))
        .expect("no trap");
    assert!(!out.is_error, "allowed fetch failed: {}", out.content);
    assert!(
        out.content.contains("SERVED-BODY-ALLOWED-CASE"),
        "body did not reach the guest: {}",
        out.content
    );
}

/// `url_allowlist = ["*"]` reaches a host it never named.
///
/// The unit tests cover the matcher; this one covers the whole path —
/// config value, `PluginState`, host function, real socket — because
/// the failure mode users actually hit is a pattern that parses but
/// never reaches the gate. `localhost` rather than `127.0.0.1` on
/// purpose: a wildcard that only worked for entries resembling
/// something in the list would still pass a same-string test.
#[test]
fn http_get_wildcard_allowlist_reaches_an_unnamed_host() {
    let port = spawn_test_server("SERVED-BODY-WILDCARD-CASE");
    let engine = PluginEngine::new().expect("engine");
    let (bridge, _) = engine
        .load(
            &fixture(),
            vec!["*".to_string()],
            std::env::temp_dir(),
            false,
            true,
        )
        .expect("load");

    let out = bridge
        .execute_tool("fetch", &format!(r#"{{"url":"http://localhost:{port}/"}}"#))
        .expect("no trap");
    assert!(!out.is_error, "wildcard fetch failed: {}", out.content);
    assert!(
        out.content.contains("SERVED-BODY-WILDCARD-CASE"),
        "body did not reach the guest: {}",
        out.content
    );
}

/// The wildcard widens hosts, not schemes. `file://` under `*` would
/// hand the plugin the filesystem read that `allow_fs` exists to gate
/// — a capability it was never granted here.
#[test]
fn wildcard_allowlist_still_refuses_non_http_schemes() {
    let engine = PluginEngine::new().expect("engine");
    let (bridge, _) = engine
        .load(
            &fixture(),
            vec!["*".to_string()],
            std::env::temp_dir(),
            false,
            true,
        )
        .expect("load");

    let out = bridge
        .execute_tool("fetch", r#"{"url":"file:///etc/hostname"}"#)
        .expect("no trap");
    assert!(out.is_error, "file:// was allowed: {}", out.content);
    assert!(out.content.contains("url_allowlist"), "{}", out.content);
}

/// Serve a single `302` pointing at `location`, forever. Same
/// never-joined-thread shape as `spawn_test_server`.
fn spawn_redirect_server(location: String) -> u16 {
    use std::io::{Read, Write};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("local_addr").port();

    std::thread::spawn(move || {
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
            let resp = format!(
                "HTTP/1.1 302 Found\r\nLocation: {location}\r\n\
                 Content-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        }
    });

    port
}

/// A `3xx` must not walk the fetch off the allowlist.
///
/// The allowlist covers `127.0.0.1`, which permits the *first* hop. That
/// server then redirects to `localhost` — a different host string, and so
/// NOT allowlisted, even though it resolves to the same loopback interface.
/// That asymmetry is what makes this test both hermetic and sharp: the
/// redirect target is a real, reachable, in-process server, so if
/// `redirect::Policy::none()` were ever dropped the client would follow the
/// hop, succeed, and hand the guest a body it was never allowed to see —
/// failing this test immediately and by name rather than hanging on an
/// unroutable address until the 10s timeout.
///
/// The allowlist is checked once, against the URL the guest supplied. Nothing
/// re-checks the `Location` header, which is precisely why not following it is
/// the control.
#[test]
fn http_get_does_not_follow_redirect_off_the_allowlist() {
    let target_port = spawn_test_server("SERVED-BODY-REDIRECT-TARGET");
    let redirect_port = spawn_redirect_server(format!("http://localhost:{target_port}/"));

    let engine = PluginEngine::new().expect("engine");
    let (bridge, _) = engine
        .load(
            &fixture(),
            vec!["127.0.0.1".to_string()],
            std::env::temp_dir(),
            false,
            true,
        )
        .expect("load");

    let out = bridge
        .execute_tool(
            "fetch",
            &format!(r#"{{"url":"http://127.0.0.1:{redirect_port}/"}}"#),
        )
        .expect("no trap");

    assert!(out.is_error, "an unfollowed 3xx must surface as an error");
    assert!(
        out.content.contains("302"),
        "the redirect should be reported as its status: {}",
        out.content
    );
    // The load-bearing assertion: the body behind the redirect must never
    // reach the guest.
    assert!(
        !out.content.contains("SERVED-BODY-REDIRECT-TARGET"),
        "redirect was followed off the allowlist: {}",
        out.content
    );
}

// ── Slash commands ──────────────────────────────────────────────────

/// The whole command path against a real component: `list-commands`
/// reaches the host, and each of the three action shapes round-trips.
#[test]
fn loads_and_executes_slash_commands() {
    let engine = PluginEngine::new().expect("engine init");
    let (bridge, _specs) = engine
        .load(&fixture(), Vec::new(), std::env::temp_dir(), false, false)
        .expect("example component must load");

    let mut cmds: Vec<String> = bridge.command_specs().into_iter().map(|c| c.name).collect();
    cmds.sort();
    assert_eq!(cmds, vec!["explain", "todo"]);

    // Descriptions are what the palette shows, so an empty one is a
    // silent quality failure the same way a tool description is.
    let todo = bridge
        .command_specs()
        .into_iter()
        .find(|c| c.name == "todo")
        .unwrap();
    assert!(todo.description.contains("TODO"), "{}", todo.description);

    match bridge.execute_command("todo", "").expect("todo call") {
        CommandAction::Print(t) => assert!(t.contains("todo:"), "{t}"),
        other => panic!("expected Print, got {other:?}"),
    }
    match bridge
        .execute_command("explain", "lifetimes")
        .expect("explain call")
    {
        CommandAction::SendUserMessage(t) => assert!(t.contains("lifetimes"), "{t}"),
        other => panic!("expected SendUserMessage, got {other:?}"),
    }
    // A user-level failure is an in-band action, not an Err.
    match bridge.execute_command("explain", "").expect("explain call") {
        CommandAction::Error(t) => assert!(t.contains("usage"), "{t}"),
        other => panic!("expected Error, got {other:?}"),
    }
}

/// `args` is raw text, deliberately not JSON. Interior runs of spaces
/// must survive — proving the host hands the line through rather than
/// tokenizing it.
#[test]
fn command_args_reach_the_guest_verbatim() {
    let engine = PluginEngine::new().expect("engine init");
    let (bridge, _) = engine
        .load(&fixture(), Vec::new(), std::env::temp_dir(), false, false)
        .expect("load");

    match bridge
        .execute_command("todo", "a b  c   d")
        .expect("todo call")
    {
        CommandAction::Print(t) => assert!(t.ends_with("a b  c   d"), "{t}"),
        other => panic!("expected Print, got {other:?}"),
    }
}

/// An unknown name is refused host-side, without entering the guest —
/// the command namespace is checked against `list-commands`, never
/// against the tool list.
#[test]
fn an_unknown_command_is_refused_without_entering_the_guest() {
    let engine = PluginEngine::new().expect("engine init");
    let (bridge, _) = engine
        .load(&fixture(), Vec::new(), std::env::temp_dir(), false, false)
        .expect("load");

    let err = bridge.execute_command("nope", "").unwrap_err();
    assert!(err.contains("nope"), "{err}");

    // A *tool* name must not be reachable as a command, or the two
    // namespaces would leak into each other.
    let err = bridge.execute_command("rot13", "").unwrap_err();
    assert!(err.contains("rot13"), "{err}");
}

/// The backward-compatibility guarantee, end to end: `runaway-plugin`
/// targets `--world extension` and exports no `list-commands`, so the
/// host's optional resolution must let it load with zero commands
/// rather than rejecting it. If this ever fails, check that the fixture
/// was not regenerated against `extension-commands`.
#[test]
fn a_component_without_list_commands_still_loads() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/runaway-plugin.component.wasm");
    let engine = PluginEngine::new().expect("engine init");
    let (bridge, specs) = engine
        .load(&path, Vec::new(), std::env::temp_dir(), false, false)
        .expect("a command-less component must still load");

    assert!(!specs.is_empty(), "its tools still register");
    assert!(
        bridge.command_specs().is_empty(),
        "and it advertises no commands"
    );
}

/// Tools and commands share one instance and one mutex, so a trap on
/// either side must leave the other usable. Both directions, because
/// only the tool→command direction catches a `PluginRebuild::build`
/// that forgot to re-resolve `execute-command` — whose failure mode is
/// silent: the palette keeps advertising a command that can no longer
/// run.
#[test]
fn a_trap_on_either_side_leaves_the_other_callable() {
    let engine = PluginEngine::new().expect("engine init");
    let (bridge, _) = engine
        .load(&fixture(), Vec::new(), std::env::temp_dir(), false, false)
        .expect("load");

    // Baseline.
    assert!(bridge.execute_command("todo", "").is_ok());

    // Blow the guest's 1 MiB arena from the tool side.
    let huge = "x".repeat(3 * 1024 * 1024);
    let err = bridge
        .execute_tool("rot13", &format!(r#"{{"text":"{huge}"}}"#))
        .unwrap_err();
    assert!(err.contains("trapped"), "{err}");

    // …a command still works, which only holds if the rebuild
    // re-resolved `execute-command`.
    match bridge.execute_command("todo", "after tool trap") {
        Ok(CommandAction::Print(t)) => assert!(t.contains("after tool trap"), "{t}"),
        other => panic!("command must survive a tool trap, got {other:?}"),
    }

    // Now the other direction: trap from the command side.
    let err = bridge.execute_command("todo", &huge).unwrap_err();
    assert!(err.contains("trapped"), "{err}");

    let out = bridge
        .execute_tool("rot13", r#"{"text":"abc"}"#)
        .expect("tool must survive a command trap");
    assert_eq!(out.content, "nop");
}

/// The epoch deadline is consumed when reached, so every guest entry
/// has to re-arm it. Calling a tool and then a command proves the
/// command path does its own arming — without it, the second call
/// enters a store whose deadline already elapsed and traps instantly.
#[test]
fn the_command_path_rearms_the_epoch_deadline() {
    let engine = PluginEngine::new().expect("engine init");
    let (bridge, _) = engine
        .load(&fixture(), Vec::new(), std::env::temp_dir(), false, false)
        .expect("load");

    bridge
        .execute_tool("rot13", r#"{"text":"abc"}"#)
        .expect("tool call");
    bridge
        .execute_command("todo", "")
        .expect("a command after a tool must not trap on a stale deadline");
}
