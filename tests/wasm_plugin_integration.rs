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
//!   -o /tmp/embedded.wasm --world extension
//! wasm-tools component new /tmp/embedded.wasm \
//!   -o tests/fixtures/example-plugin.component.wasm
//! ```

#![cfg(feature = "wasm")]

use std::path::PathBuf;

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
        .load(&fixture(), Vec::new(), std::env::temp_dir(), false)
        .expect("example component must load");

    // list-tools reached the host intact.
    let mut names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
    names.sort();
    assert_eq!(names, vec!["readfile", "rot13", "wordcount"]);

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
    let (bridge, _) = engine.load(&fixture(), Vec::new(), std::env::temp_dir(), false).expect("load");

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
    let (bridge, _) = engine.load(&fixture(), Vec::new(), std::env::temp_dir(), false).expect("load");

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
    let (bridge, _) = engine.load(&fixture(), Vec::new(), std::env::temp_dir(), false).expect("load");

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

    match engine.load(&p, Vec::new(), std::env::temp_dir(), false) {
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
        .load(&fixture(), Vec::new(), dir.clone(), false)
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
        .load(&fixture(), Vec::new(), dir.clone(), true)
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
        .load(&fixture(), Vec::new(), dir.clone(), true)
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
        .load(&fixture(), Vec::new(), dir.clone(), true)
        .expect("load");

    let out = bridge
        .execute_tool("readfile", r#"{"path":"sneaky"}"#)
        .expect("no trap");
    assert!(out.is_error, "symlink escape must be refused: {}", out.content);
    assert!(out.content.contains("escapes"), "{}", out.content);

    let _ = std::fs::remove_dir_all(&dir);
}
