//! Example nanopi WASM extension.
//!
//! Exports four tools the model can call:
//!   - `wordcount` — count words / lines / chars in a string
//!   - `rot13`     — the classic letter rotation
//!   - `readfile`  — read a file through the gated `host-fs-read`
//!   - `fetch`     — get a URL through the gated `host-http-get`
//!
//! The first two are pure computation, deliberately trivial: the point
//! is the wiring, with nothing to install. `readfile` and `fetch`
//! exist to show the other half — how a plugin reaches outside itself,
//! and what happens when the user hasn't granted that capability.
//!
//! Plus two slash commands the *user* can type:
//!   - `/todo`    — prints to the scrollback, never seen by the model
//!   - `/explain` — starts a turn on the user's behalf
//!
//! Build (from the repo root, so `wit/` resolves):
//!   cargo build --manifest-path examples/wasm-plugin/Cargo.toml \
//!     --target wasm32-wasip1 --release
//!   wasm-tools component embed wit/ \
//!     examples/wasm-plugin/target/wasm32-wasip1/release/nanopi_example_plugin.wasm \
//!     -o /tmp/embedded.wasm --world extension-commands
//!   wasm-tools component new /tmp/embedded.wasm \
//!     -o nanopi-example-plugin.component.wasm
//!
//! Note `--world extension-commands`, not `extension`: this plugin
//! registers slash commands. A tool-only plugin keeps using
//! `--world extension` — see the two-world note in `wit/`.
//!
//! The `embed` step is not optional — `component new` on a bare module
//! yields a component with an empty world, and the host then fails to
//! find `list-tools`.
//!
//! Install:
//!   cp nanopi-example-plugin.component.wasm ~/.nanopi/extensions/
//!   # then in ~/.nanopi/config.toml:
//!   #   [[extensions]]
//!   #   path = "~/.nanopi/extensions/nanopi-example-plugin.component.wasm"
//!
//! Note the host must be built with `--features wasm`; the stock
//! release binary has no WASM runtime.
//!
//! ## About the ABI
//!
//! The canonical ABI passes strings as (ptr, len) pairs into linear
//! memory and returns them through a caller-supplied area. The three
//! `#[no_mangle]` shims below implement exactly that by hand, which is
//! tractable only because nanopi's interface is deliberately two
//! functions with one primitive type. For a wider interface, generate
//! this with `wit-bindgen` instead of copying these shims.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use core::alloc::{GlobalAlloc, Layout};

// ── libc shims ──────────────────────────────────────────────────────
// `#![no_std]` on wasm32 links against no libc, but rustc still emits
// calls to a couple of `mem*` intrinsics — `memcmp` here, from string
// comparison. Provide it rather than pulling in a compiler-builtins
// dependency for one function.

#[no_mangle]
pub unsafe extern "C" fn memcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    for i in 0..n {
        let (x, y) = (*a.add(i), *b.add(i));
        if x != y {
            return x as i32 - y as i32;
        }
    }
    0
}

// ── Allocator ───────────────────────────────────────────────────────
// `#![no_std]` still needs a heap for String/Vec. wasm32 is
// single-threaded, so a bump allocator over a static arena is enough
// and keeps the module tiny — no dlmalloc, no free.

const ARENA_SIZE: usize = 1 << 20; // 1 MiB
static mut ARENA: [u8; ARENA_SIZE] = [0; ARENA_SIZE];
static mut OFFSET: usize = 0;

struct BumpAlloc;

unsafe impl core::alloc::GlobalAlloc for BumpAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let align = layout.align();
        let start = (OFFSET + align - 1) & !(align - 1);
        let end = start + layout.size();
        if end > ARENA_SIZE {
            return core::ptr::null_mut();
        }
        OFFSET = end;
        core::ptr::addr_of_mut!(ARENA).cast::<u8>().add(start)
    }
    // Bump allocator: nothing is reclaimed. Each tool call is short
    // and the arena is reset in `reset_arena` below, so this is fine
    // for a plugin. Do not copy this into a long-running program.
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}

#[global_allocator]
static ALLOC: BumpAlloc = BumpAlloc;

// ── Host imports ────────────────────────────────────────────────────
// Declared at the component root (`$root`), with `link_name` giving
// the hyphenated WIT name that Rust identifiers can't express.
//
// The string return comes back as a pointer to an 8-byte (ptr, len)
// pair in our own memory, same convention as our exports use.

#[link(wasm_import_module = "$root")]
extern "C" {
    #[link_name = "host-log"]
    fn host_log_raw(level: u32, ptr: *const u8, len: usize);
    // Note the shape: an *import* returning a string takes the return
    // area as a trailing out-param, whereas an *export* returns a
    // pointer to it. The canonical ABI is not symmetric here, and
    // getting it backwards fails at `wasm-tools component new` with a
    // type mismatch rather than at runtime.
    #[link_name = "host-fs-read"]
    fn host_fs_read_raw(ptr: *const u8, len: usize, ret_area: *mut u8);
    // Same trailing-`ret_area` shape as `host-fs-read` above, for the
    // same reason: this is an import returning a string.
    #[link_name = "host-http-get"]
    fn host_http_get_raw(ptr: *const u8, len: usize, ret_area: *mut u8);
}

/// Log to nanopi's stderr. 0=trace 1=info 2=warn 3=error.
unsafe fn host_log(level: u32, msg: &str) {
    host_log_raw(level, msg.as_ptr(), msg.len());
}

/// Read a file through the host. Returns whatever the host gives us,
/// including its `error: ` strings — the caller decides what to do
/// with a refusal.
unsafe fn host_fs_read(path: &str) -> String {
    // 8 bytes, 4-aligned: the (ptr, len) pair the host writes back.
    let ret_area = ALLOC.alloc(Layout::from_size_align_unchecked(8, 4));
    host_fs_read_raw(path.as_ptr(), path.len(), ret_area);
    let ptr = ret_area.cast::<u32>().read() as *const u8;
    let len = ret_area.cast::<u32>().add(1).read() as usize;
    read_string(ptr, len)
}

/// Fetch a URL through the host. Returns whatever the host gives us,
/// `error: ` strings included — a refusal (gate closed, host not on
/// the allowlist, request failed) is the caller's to interpret.
unsafe fn host_http_get(url: &str) -> String {
    // 8 bytes, 4-aligned: the (ptr, len) pair the host writes back.
    let ret_area = ALLOC.alloc(Layout::from_size_align_unchecked(8, 4));
    host_http_get_raw(url.as_ptr(), url.len(), ret_area);
    let ptr = ret_area.cast::<u32>().read() as *const u8;
    let len = ret_area.cast::<u32>().add(1).read() as usize;
    read_string(ptr, len)
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    // A trap here surfaces to the model as a failed tool call.
    core::arch::wasm32::unreachable()
}

/// Called at the top of each export so one call's garbage doesn't
/// starve the next. Safe because nothing survives across calls: every
/// return value is copied out by the host before it returns.
unsafe fn reset_arena() {
    OFFSET = 0;
}

// ── Canonical ABI plumbing ──────────────────────────────────────────

/// The host calls this to allocate space in our memory when it needs
/// to pass us a string. Required by the canonical ABI.
#[no_mangle]
pub unsafe extern "C" fn cabi_realloc(
    _old_ptr: *mut u8,
    _old_len: usize,
    align: usize,
    new_len: usize,
) -> *mut u8 {
    if new_len == 0 {
        return align as *mut u8;
    }
    let layout = Layout::from_size_align_unchecked(new_len, align);
    ALLOC.alloc(layout)
}

/// Leak a String into linear memory, then return a pointer to an
/// 8-byte area holding its `(ptr, len)`.
///
/// This is the canonical ABI's return convention for a `string`
/// result: the export returns `i32` pointing at the pair, rather than
/// taking a caller-provided out-param. Both the string bytes and the
/// pair live in our arena; the host copies them out before the next
/// call resets it.
unsafe fn string_result(s: String) -> *mut u8 {
    let bytes = s.into_bytes();
    let len = bytes.len();
    let ptr = if len == 0 {
        1 as *mut u8 // non-null dangling is fine for a 0-length string
    } else {
        let layout = Layout::from_size_align_unchecked(len, 1);
        let p = ALLOC.alloc(layout);
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), p, len);
        p
    };
    core::mem::forget(bytes);

    let ret_layout = Layout::from_size_align_unchecked(8, 4);
    let ret_area = ALLOC.alloc(ret_layout);
    ret_area.cast::<u32>().write(ptr as u32);
    ret_area.cast::<u32>().add(1).write(len as u32);
    ret_area
}

unsafe fn read_string(ptr: *const u8, len: usize) -> String {
    let slice = core::slice::from_raw_parts(ptr, len);
    // The host only ever sends us valid UTF-8 (WIT `string`).
    String::from_utf8_unchecked(slice.to_vec())
}

// ── Exports ─────────────────────────────────────────────────────────

/// `list-tools: func() -> string`
///
/// The exported symbol must be the WIT name *verbatim*, hyphens and
/// all — `wasm-tools component embed` looks for `list-tools`, not
/// `list_tools`, and Rust identifiers can't contain hyphens. Hence
/// `#[export_name]` rather than `#[no_mangle]`.
///
/// The JSON Schema in `parameters` is what the model sees, so its
/// `description` fields matter as much as the tool's own — that text
/// is the entire spec the model gets for how to call this.
#[export_name = "list-tools"]
pub unsafe extern "C" fn list_tools() -> *mut u8 {
    reset_arena();
    let specs = r#"[
  {
    "name": "wordcount",
    "description": "Count words, lines, and characters in a piece of text.",
    "parameters": {
      "type": "object",
      "properties": {
        "text": { "type": "string", "description": "The text to measure." }
      },
      "required": ["text"]
    }
  },
  {
    "name": "readfile",
    "description": "Read a UTF-8 text file from the working directory and report its size. Requires allow_fs = true on this plugin's [[extensions]] entry.",
    "parameters": {
      "type": "object",
      "properties": {
        "path": { "type": "string", "description": "Path relative to the working directory." }
      },
      "required": ["path"]
    }
  },
  {
    "name": "fetch",
    "description": "Fetch a URL over HTTP(S) and return the response body. Requires allow_network = true and a matching url_allowlist entry on this plugin's [[extensions]] entry.",
    "parameters": {
      "type": "object",
      "properties": {
        "url": { "type": "string", "description": "An http:// or https:// URL." }
      },
      "required": ["url"]
    }
  },
  {
    "name": "rot13",
    "description": "Apply the ROT13 letter substitution to a string.",
    "parameters": {
      "type": "object",
      "properties": {
        "text": { "type": "string", "description": "The text to transform." }
      },
      "required": ["text"]
    }
  }
]"#;
    string_result(specs.to_string())
}

/// `execute-tool: func(name: string, args-json: string) -> string`
#[export_name = "execute-tool"]
pub unsafe extern "C" fn execute_tool(
    name_ptr: *const u8,
    name_len: usize,
    args_ptr: *const u8,
    args_len: usize,
) -> *mut u8 {
    // Read arguments before allocating anything else — they live in
    // our arena, put there by the host's cabi_realloc calls, and
    // `reset_arena` is deliberately NOT called here for that reason.
    let name = read_string(name_ptr, name_len);
    let args_json = read_string(args_ptr, args_len);

    string_result(dispatch(&name, &args_json))
}

fn dispatch(name: &str, args_json: &str) -> String {
    let args: serde_json::Value = match serde_json::from_str(args_json) {
        Ok(v) => v,
        Err(e) => return err_result(&format!("arguments were not valid JSON: {e}")),
    };
    // `readfile` takes `path` and `fetch` takes `url`; the other two
    // take `text`. Both must be handled ahead of the `text`-extracting
    // tail, or a call missing `url` would report a missing `text`.
    if name == "fetch" {
        let url = match args.get("url").and_then(|v| v.as_str()) {
            Some(u) => u,
            None => return err_result("missing required string field `url`"),
        };
        let body = unsafe {
            host_log(1, "fetch: asking the host for the URL");
            host_http_get(url)
        };
        // The host signals refusal in-band with an `error: ` prefix.
        if let Some(reason) = body.strip_prefix("error: ") {
            return err_result(reason);
        }
        // Hand the body back verbatim — passing the content to the
        // model is the entire point of a fetch tool.
        return ok_result(&body);
    }
    if name == "readfile" {
        let path = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return err_result("missing required string field `path`"),
        };
        let contents = unsafe {
            host_log(1, "readfile: asking the host for the file");
            host_fs_read(path)
        };
        // The host signals refusal in-band with an `error: ` prefix.
        if let Some(reason) = contents.strip_prefix("error: ") {
            return err_result(reason);
        }
        let lines = contents.lines().count();
        return ok_result(&format!(
            "{} bytes, {lines} lines",
            contents.len()
        ));
    }

    let text = match args.get("text").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return err_result("missing required string field `text`"),
    };

    match name {
        "wordcount" => {
            let words = text.split_whitespace().count();
            let lines = if text.is_empty() {
                0
            } else {
                text.lines().count()
            };
            let chars = text.chars().count();
            ok_result(&format!(
                "{words} words, {lines} lines, {chars} characters"
            ))
        }
        "rot13" => {
            let rotated: String = text
                .chars()
                .map(|c| match c {
                    'a'..='z' => (((c as u8 - b'a' + 13) % 26) + b'a') as char,
                    'A'..='Z' => (((c as u8 - b'A' + 13) % 26) + b'A') as char,
                    other => other,
                })
                .collect();
            ok_result(&rotated)
        }
        // Unreachable in practice — the host checks the name against
        // list-tools before dispatching — but a plugin should still
        // answer rather than trap.
        other => err_result(&format!("unknown tool {other:?}")),
    }
}

// ── Slash commands ──────────────────────────────────────────────────
//
// A tool is something the *model* decides to call; a command is
// something the *user* types. Commands live in the
// `extension-commands` world — a plugin that only provides tools
// should keep targeting `extension` and skip this section entirely.

/// `list-commands: func() -> string`
///
/// Optional from the host's side: omit it and the plugin still loads
/// with zero commands. Names are refused, not renamed, when they
/// collide with a built-in or with another plugin — so pick something
/// specific.
#[export_name = "list-commands"]
pub unsafe extern "C" fn list_commands() -> *mut u8 {
    reset_arena();
    let specs = r#"[
  {
    "name": "todo",
    "description": "Show this plugin's TODO list, or echo a note back."
  },
  {
    "name": "explain",
    "description": "Ask the model to explain something, from a canned prompt."
  }
]"#;
    string_result(specs.to_string())
}

/// `execute-command: func(name: string, args: string) -> string`
///
/// Note `args` is raw text, not JSON — and, exactly as in
/// `execute_tool`, `reset_arena()` must NOT be called here: both
/// strings live in our arena, put there by the host's `cabi_realloc`.
#[export_name = "execute-command"]
pub unsafe extern "C" fn execute_command(
    name_ptr: *const u8,
    name_len: usize,
    args_ptr: *const u8,
    args_len: usize,
) -> *mut u8 {
    let name = read_string(name_ptr, name_len);
    let args = read_string(args_ptr, args_len);

    string_result(dispatch_command(&name, &args))
}

fn dispatch_command(name: &str, args: &str) -> String {
    match name {
        // Returns `print`: the text lands in the user's scrollback and
        // is never shown to the model. Echoing `args` back verbatim
        // also demonstrates that the host does not tokenize it.
        "todo" => {
            if args.is_empty() {
                print_action("todo: (1) wire the plugin (2) ship it")
            } else {
                print_action(&format!("todo: noted — {args}"))
            }
        }
        // Returns `send_user_message`: nanopi starts a turn with this
        // text as if the user had typed it, echoing it verbatim first.
        // Mid-stream it steers the running turn instead.
        "explain" => {
            if args.is_empty() {
                error_action("usage: /explain <topic>")
            } else {
                send_user_message_action(&format!(
                    "Explain {args} to me, briefly, and cite the source files."
                ))
            }
        }
        // Unreachable in practice — the host checks the name against
        // list-commands first — but answer rather than trap.
        other => error_action(&format!("unknown command {other:?}")),
    }
}

fn print_action(text: &str) -> String {
    serde_json::json!({ "print": text }).to_string()
}

fn send_user_message_action(text: &str) -> String {
    serde_json::json!({ "send_user_message": text }).to_string()
}

fn error_action(text: &str) -> String {
    serde_json::json!({ "error": text }).to_string()
}

fn ok_result(content: &str) -> String {
    serde_json::json!({ "content": content, "is_error": false }).to_string()
}

fn err_result(msg: &str) -> String {
    serde_json::json!({ "content": msg, "is_error": true }).to_string()
}
