//! Example nanopi WASM extension.
//!
//! Exports two tools the model can call:
//!   - `wordcount` — count words / lines / chars in a string
//!   - `rot13`     — the classic letter rotation
//!
//! Neither does anything a built-in tool could not, on purpose: the
//! point is to show the wiring end to end with nothing to install and
//! nothing to sandbox. Swap the bodies of `wordcount` / `rot13` for
//! whatever your plugin actually does.
//!
//! Build (from the repo root, so `wit/` resolves):
//!   cargo build --manifest-path examples/wasm-plugin/Cargo.toml \
//!     --target wasm32-wasip1 --release
//!   wasm-tools component embed wit/ \
//!     examples/wasm-plugin/target/wasm32-wasip1/release/nanopi_example_plugin.wasm \
//!     -o /tmp/embedded.wasm --world extension
//!   wasm-tools component new /tmp/embedded.wasm \
//!     -o nanopi-example-plugin.component.wasm
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

fn ok_result(content: &str) -> String {
    serde_json::json!({ "content": content, "is_error": false }).to_string()
}

fn err_result(msg: &str) -> String {
    serde_json::json!({ "content": msg, "is_error": true }).to_string()
}
