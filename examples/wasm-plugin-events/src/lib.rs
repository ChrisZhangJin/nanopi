//! Example nanopi WASM event-subscriber plugin — the smallest thing
//! that watches lifecycle events and logs them.
//!
//! Targets `--world extension-events` (see `wit/nanopi-extension.wit`),
//! the top of the linear ladder: `extension` ⊂ `extension-commands` ⊂
//! `extension-events`. It requests three events via `list-events`:
//!
//!   - `tool_execution_start`
//!   - `turn_start`
//!   - `input`
//!
//! Requesting an event is not the same as *receiving* it — delivery
//! also needs the config's `[[extensions]].events` to name it
//! (`docs/v0.12-events.md` §4.2). See `README.md` for the two-list
//! grant this plugin needs to actually see anything.
//!
//! Three tools, chosen to make the event log observable and to give
//! the host something to hold a lock on:
//!   - `events_seen` — the running tally: `{"total": N, "by_event": {...}}`.
//!     This is the thing a shell hook cannot do — state that survives
//!     across calls (`docs/v0.12-events.md` §1's table, "hold state").
//!   - `busy`        — spins for about a second of guest time, so a
//!     test can hold the bridge lock long enough to prove the host's
//!     `try_lock` drops rather than waits.
//!   - `greet`       — carried over from `examples/wasm-plugin-minimal`
//!     so the fixture also proves ordinary tools keep working after an
//!     event trap.
//!
//! `list-commands` / `execute-command` are the ladder's accepted stub
//! cost (`docs/v0.12-events.md` §4.1): this plugin registers no
//! commands, but the exports must still exist because
//! `extension-events` includes `extension-commands`.
//!
//! `handle-event` has two deliberate test affordances, both load-bearing
//! for Task 3's integration tests, not accidents:
//!   - if `payload-json` contains the literal substring
//!     `"nanopi-test-trap"`, it traps via
//!     `core::arch::wasm32::unreachable()`, so a test can drive the
//!     "a trap in handle-event leaves tools callable" case;
//!   - its return value is deliberately NOT valid JSON (`not-json`),
//!     which is legal only because the host ignores it entirely
//!     (`docs/v0.12-events.md` §3) — this makes "the return value is
//!     ignored" a tested claim, not an assertion about code that
//!     happens to not be checked.
//!
//! Do NOT fetch from an event handler. The 2-second epoch budget
//! covers guest code only; a handler blocked in `host-http-get` is
//! bounded by that call's own 10-second timeout on top, so a slow
//! handler can cost the turn ~12 seconds even though the budget
//! nominally reads as 2 (`docs/v0.12-events.md` §5.3).
//!
//! Build (from the repo root, so `wit/` resolves) — see `README.md` for
//! the full three-step recipe with `--world extension-events`.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use core::alloc::{GlobalAlloc, Layout};

// ═══ BOILERPLATE — copy verbatim (see wasm-plugin-minimal) ══════════

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

const ARENA_SIZE: usize = 1 << 20; // 1 MiB — must exceed your largest payload
static mut ARENA: [u8; ARENA_SIZE] = [0; ARENA_SIZE];
static mut OFFSET: usize = 0;

struct BumpAlloc;

unsafe impl GlobalAlloc for BumpAlloc {
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
    // Nothing is reclaimed, same convention as the other examples. This
    // is also why the event counters below are plain static integers
    // rather than a heap `Vec`/`String`: `reset_arena` rewinds `OFFSET`
    // to 0 on every read-only call, so anything living IN the arena
    // (like a heap collection) would get silently overwritten by the
    // next call's allocations. State that must survive across calls
    // has to live outside the bump arena entirely.
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}

#[global_allocator]
static ALLOC: BumpAlloc = BumpAlloc;

#[link(wasm_import_module = "$root")]
extern "C" {
    #[link_name = "host-log"]
    fn host_log_raw(level: u32, ptr: *const u8, len: usize);
}

unsafe fn host_log(level: u32, msg: &str) {
    host_log_raw(level, msg.as_ptr(), msg.len());
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

unsafe fn reset_arena() {
    OFFSET = 0;
}

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
    ALLOC.alloc(Layout::from_size_align_unchecked(new_len, align))
}

unsafe fn string_result(s: String) -> *mut u8 {
    let bytes = s.into_bytes();
    let len = bytes.len();
    let ptr = if len == 0 {
        1 as *mut u8
    } else {
        let p = ALLOC.alloc(Layout::from_size_align_unchecked(len, 1));
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), p, len);
        p
    };
    core::mem::forget(bytes);
    let ret_area = ALLOC.alloc(Layout::from_size_align_unchecked(8, 4));
    ret_area.cast::<u32>().write(ptr as u32);
    ret_area.cast::<u32>().add(1).write(len as u32);
    ret_area
}

unsafe fn read_string(ptr: *const u8, len: usize) -> String {
    let slice = core::slice::from_raw_parts(ptr, len);
    String::from_utf8_unchecked(slice.to_vec())
}

fn ok(content: String) -> String {
    serde_json::json!({ "content": content, "is_error": false }).to_string()
}

fn err(content: String) -> String {
    serde_json::json!({ "content": content, "is_error": true }).to_string()
}

// ═══ Event log — plain static integers, deliberately not heap state ═

static mut TOTAL: u64 = 0;
static mut COUNT_TOOL_EXECUTION_START: u64 = 0;
static mut COUNT_TURN_START: u64 = 0;
static mut COUNT_INPUT: u64 = 0;

/// Marker the tests use to drive a deliberate trap. Not JSON syntax on
/// purpose — a plugin does not get to assume payload shape beyond
/// "some string", so the check is a plain substring match.
const TEST_TRAP_MARKER: &str = "nanopi-test-trap";

// ═══ Canonical ABI exports ═══════════════════════════════════════════

#[export_name = "list-tools"]
pub unsafe extern "C" fn list_tools() -> *mut u8 {
    reset_arena();
    let specs = r#"[
  {
    "name": "events_seen",
    "description": "Report how many lifecycle events this plugin has observed so far, broken down by event name.",
    "parameters": { "type": "object", "properties": {} }
  },
  {
    "name": "busy",
    "description": "Spin for about a second of guest time. Used to hold this plugin's execution lock so a test can prove concurrent event delivery is dropped, not queued.",
    "parameters": { "type": "object", "properties": {} }
  },
  {
    "name": "greet",
    "description": "Greet someone by name. Use when the user asks for a greeting.",
    "parameters": {
      "type": "object",
      "properties": {
        "name": { "type": "string", "description": "Who to greet." }
      },
      "required": ["name"]
    }
  }
]"#;
    string_result(specs.to_string())
}

#[export_name = "execute-tool"]
pub unsafe extern "C" fn execute_tool(
    name_ptr: *const u8,
    name_len: usize,
    args_ptr: *const u8,
    args_len: usize,
) -> *mut u8 {
    // Read args before allocating anything else — they live in our
    // arena, put there by the host's cabi_realloc calls.
    let name = read_string(name_ptr, name_len);
    let args_json = read_string(args_ptr, args_len);
    string_result(dispatch(&name, &args_json))
}

fn dispatch(name: &str, args_json: &str) -> String {
    match name {
        "events_seen" => {
            let (total, tes, ts, input) = unsafe {
                (
                    TOTAL,
                    COUNT_TOOL_EXECUTION_START,
                    COUNT_TURN_START,
                    COUNT_INPUT,
                )
            };
            // Only nonzero entries — an event never seen is absent,
            // not present with a 0.
            let mut by_event = alloc::vec::Vec::new();
            if tes > 0 {
                by_event.push(format!("\"tool_execution_start\":{tes}"));
            }
            if ts > 0 {
                by_event.push(format!("\"turn_start\":{ts}"));
            }
            if input > 0 {
                by_event.push(format!("\"input\":{input}"));
            }
            ok(format!(
                "{{\"total\":{total},\"by_event\":{{{}}}}}",
                by_event.join(",")
            ))
        }
        "busy" => {
            // Volatile writes so opt-level=z + lto cannot fold this
            // into nothing. Not an infinite loop: bounded, and well
            // under the 30s tool budget.
            let mut sink: u8 = 0;
            for i in 0..200_000_000u64 {
                unsafe {
                    core::ptr::write_volatile(&mut sink, (i & 0xff) as u8);
                }
            }
            ok(format!("done ({sink})"))
        }
        "greet" => {
            let args: serde_json::Value = match serde_json::from_str(args_json) {
                Ok(v) => v,
                Err(e) => return err(format!("bad arguments JSON: {e}")),
            };
            let who = match args.get("name").and_then(|v| v.as_str()) {
                Some(s) => s,
                None => return err("missing required argument: name".to_string()),
            };
            ok(format!("Hello, {who}! — from events-plugin"))
        }
        other => err(format!("unknown tool: {other}")),
    }
}

/// The ladder stub (`docs/v0.12-events.md` §4.1): `extension-events`
/// includes `extension-commands`, so this export must exist even
/// though this plugin registers no commands. Returning `[]` is the
/// accepted cost of the linear-ladder design over a world lattice.
#[export_name = "list-commands"]
pub unsafe extern "C" fn list_commands() -> *mut u8 {
    reset_arena();
    string_result("[]".to_string())
}

/// Unreachable in practice — the host never dispatches a name that
/// wasn't in `list-commands` — but answer rather than trap, and name
/// the ladder-stub reason in the message.
#[export_name = "execute-command"]
pub unsafe extern "C" fn execute_command(
    _name_ptr: *const u8,
    _name_len: usize,
    _args_ptr: *const u8,
    _args_len: usize,
) -> *mut u8 {
    string_result(
        serde_json::json!({ "error": "this plugin registers no commands (extension-events ladder stub)" })
            .to_string(),
    )
}

/// `list-events: func() -> string`
///
/// Three names on purpose: the host grants a subset in the tests, so
/// this fixture proves both directions of the granted ∩ requested
/// intersection with a single build.
#[export_name = "list-events"]
pub unsafe extern "C" fn list_events() -> *mut u8 {
    reset_arena();
    string_result(r#"["tool_execution_start", "turn_start", "input"]"#.to_string())
}

/// `handle-event: func(event: string, payload-json: string) -> string`
///
/// Observe-only: the return value is discarded by the host
/// (`docs/v0.12-events.md` §3), so this deliberately returns
/// `not-json` — legal only because nothing ever parses it.
#[export_name = "handle-event"]
pub unsafe extern "C" fn handle_event(
    event_ptr: *const u8,
    event_len: usize,
    payload_ptr: *const u8,
    payload_len: usize,
) -> *mut u8 {
    // Read args before allocating anything else — same reason as
    // execute_tool: they live in our arena via the host's
    // cabi_realloc, and reset_arena is deliberately NOT called here.
    let event = read_string(event_ptr, event_len);
    let payload = read_string(payload_ptr, payload_len);

    if payload.contains(TEST_TRAP_MARKER) {
        core::arch::wasm32::unreachable();
    }

    match event.as_str() {
        "tool_execution_start" => COUNT_TOOL_EXECUTION_START += 1,
        "turn_start" => COUNT_TURN_START += 1,
        "input" => COUNT_INPUT += 1,
        _ => {}
    }
    TOTAL += 1;

    host_log(0, &format!("events-plugin: observed {event}"));

    string_result("not-json".to_string())
}
