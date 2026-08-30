//! Minimal nanopi WASM plugin — a starting point to copy.
//!
//! Two tools, chosen to show both halves of the interface:
//!   - `greet`      — pure computation, needs no capabilities
//!   - `fetch_head` — first 200 chars of a URL, through the gated
//!                    `host-http-get`, including what a refusal
//!                    looks like when the user hasn't opted in
//!
//! See `examples/wasm-plugin/` for the fuller four-tool version. This
//! one is deliberately the smallest thing that still exercises a
//! capability gate.
//!
//! Everything above the "YOUR TOOLS" line is boilerplate to copy
//! verbatim; everything below it is yours.
//!
//! Build (from the repo root, so `wit/` resolves):
//!   cargo build --manifest-path examples/wasm-plugin-minimal/Cargo.toml \
//!     --target wasm32-wasip1 --release
//!   wasm-tools component embed wit/ \
//!     examples/wasm-plugin-minimal/target/wasm32-wasip1/release/nanopi_minimal_plugin.wasm \
//!     -o /tmp/embedded.wasm --world extension
//!   wasm-tools component new /tmp/embedded.wasm \
//!     -o nanopi-minimal-plugin.component.wasm
//!
//! The `embed` step is not optional — `component new` on a bare module
//! yields a component with an empty world, and the host then fails to
//! find `list-tools`.
//!
//! The host must be built with `--features wasm`; the stock release
//! binary has no WASM runtime.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use core::alloc::{GlobalAlloc, Layout};

// ═══ BOILERPLATE — copy verbatim ════════════════════════════════════

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
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}

#[global_allocator]
static ALLOC: BumpAlloc = BumpAlloc;

// Host imports. NOTE the asymmetry: an *import* returning a string
// takes the return area as a TRAILING out-param. Getting this backwards
// fails at `wasm-tools component new` with a type mismatch.
#[link(wasm_import_module = "$root")]
extern "C" {
    #[link_name = "host-log"]
    fn host_log_raw(level: u32, ptr: *const u8, len: usize);
    #[link_name = "host-fs-read"]
    fn host_fs_read_raw(ptr: *const u8, len: usize, ret_area: *mut u8);
    #[link_name = "host-http-get"]
    fn host_http_get_raw(ptr: *const u8, len: usize, ret_area: *mut u8);
}

unsafe fn host_log(level: u32, msg: &str) {
    host_log_raw(level, msg.as_ptr(), msg.len());
}

unsafe fn call_host_str(
    f: unsafe extern "C" fn(*const u8, usize, *mut u8),
    arg: &str,
) -> String {
    let ret_area = ALLOC.alloc(Layout::from_size_align_unchecked(8, 4));
    f(arg.as_ptr(), arg.len(), ret_area);
    let ptr = ret_area.cast::<u32>().read() as *const u8;
    let len = ret_area.cast::<u32>().add(1).read() as usize;
    read_string(ptr, len)
}

#[allow(dead_code)]
unsafe fn host_fs_read(path: &str) -> String {
    call_host_str(host_fs_read_raw, path)
}

unsafe fn host_http_get(url: &str) -> String {
    call_host_str(host_http_get_raw, url)
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
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

/// Build the JSON the host expects back from `execute-tool`.
fn ok(content: String) -> String {
    serde_json::json!({ "content": content, "is_error": false }).to_string()
}

fn err(content: String) -> String {
    serde_json::json!({ "content": content, "is_error": true }).to_string()
}

// Export names must be the WIT names VERBATIM, hyphens and all —
// hence `#[export_name]`, not `#[no_mangle]`.
#[export_name = "list-tools"]
pub unsafe extern "C" fn list_tools() -> *mut u8 {
    string_result(TOOL_SPECS.to_string())
}

#[export_name = "execute-tool"]
pub unsafe extern "C" fn execute_tool(
    name_ptr: *const u8,
    name_len: usize,
    args_ptr: *const u8,
    args_len: usize,
) -> *mut u8 {
    // Read args BEFORE allocating anything else; they live in our arena.
    let name = read_string(name_ptr, name_len);
    let args_json = read_string(args_ptr, args_len);
    string_result(dispatch(&name, &args_json))
}

// ═══ YOUR TOOLS — everything below is yours ═════════════════════════

/// What the model sees. The `description` fields ARE the prompt — the
/// model picks tools by reading these, so vague text means unused tools.
const TOOL_SPECS: &str = r#"[
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
  },
  {
    "name": "fetch_head",
    "description": "Fetch a URL and return the first 200 characters of the response body. Requires allow_network = true and the host in url_allowlist on this plugin's [[extensions]] entry.",
    "parameters": {
      "type": "object",
      "properties": {
        "url": { "type": "string", "description": "The http/https URL to fetch." }
      },
      "required": ["url"]
    }
  }
]"#;

fn dispatch(name: &str, args_json: &str) -> String {
    let args: serde_json::Value = match serde_json::from_str(args_json) {
        Ok(v) => v,
        Err(e) => return err(format!("bad arguments JSON: {e}")),
    };

    match name {
        "greet" => {
            let who = match args.get("name").and_then(|v| v.as_str()) {
                Some(s) => s,
                None => return err("missing required argument: name".to_string()),
            };
            ok(format!("Hello, {who}! — from my-plugin"))
        }

        "fetch_head" => {
            let url = match args.get("url").and_then(|v| v.as_str()) {
                Some(s) => s,
                None => return err("missing required argument: url".to_string()),
            };
            unsafe { host_log(1, "fetch_head: calling host-http-get") };
            let body = unsafe { host_http_get(url) };

            // The host signals refusal/failure in-band with this prefix.
            // Pass it through as an error so the model can react.
            if body.starts_with("error: ") {
                return err(body);
            }
            let head: String = body.chars().take(200).collect();
            ok(format!("{} bytes; first 200 chars:\n{head}", body.len()))
        }

        other => err(format!("unknown tool: {other}")),
    }
}
