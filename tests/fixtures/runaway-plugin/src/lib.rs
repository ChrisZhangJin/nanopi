//! Test fixture: a plugin whose `execute-tool` never returns.
//!
//! Exists to prove nanopi's epoch-based hang breaker actually fires
//! (`wasm::loader::tests::runaway_plugin_is_cut_off_by_the_epoch_deadline`).
//! Deliberately NOT in `examples/` — nothing here is worth copying.
//! No host imports and no serde_json: the smallest component that
//! loads cleanly and then spins forever.
//!
//! The built `.wasm` is committed for the same reason the example
//! fixture is — the toolchain that produces it is a heavier ask than
//! running the tests. To regenerate, from the repo root (never from
//! inside this directory; `wit/` resolves relative to the root):
//!
//! ```bash
//! cargo build --manifest-path tests/fixtures/runaway-plugin/Cargo.toml \
//!   --target wasm32-wasip1 --release
//! wasm-tools component embed wit/ \
//!   tests/fixtures/runaway-plugin/target/wasm32-wasip1/release/nanopi_runaway_plugin.wasm \
//!   -o /tmp/runaway-embedded.wasm --world extension
//! wasm-tools component new /tmp/runaway-embedded.wasm \
//!   -o tests/fixtures/runaway-plugin.component.wasm
//! ```
//!
//! KEEP `--world extension`, NOT `extension-commands`. This component
//! now does double duty: hang breaker, and the only committed plugin
//! WITHOUT `list-commands` — which makes it the only end-to-end proof
//! that the host resolves that export optionally rather than requiring
//! it. Retarget it and that backward-compatibility guarantee stops
//! being tested, silently.
//!
//! See `.planning/reference/wasm-toolchain-notes.md` for the
//! box-specific gotchas (`wasm32-wasip1` is installed by hand and does
//! not appear in `rustup target list`).

#![no_std]

extern crate alloc;

use alloc::string::{String, ToString};
use core::alloc::{GlobalAlloc, Layout};

const ARENA_SIZE: usize = 1 << 16;
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

const TOOL_SPECS: &str = r#"[{"name":"spin","description":"Never returns.","parameters":{"type":"object"}}]"#;

#[export_name = "list-tools"]
pub unsafe extern "C" fn list_tools() -> *mut u8 {
    string_result(TOOL_SPECS.to_string())
}

/// Spins on a volatile write so no optimizer can prove the loop dead.
/// The back-edge is where wasmtime's epoch check lands.
static mut SINK: u64 = 0;

#[export_name = "execute-tool"]
pub unsafe extern "C" fn execute_tool(
    _name_ptr: *const u8,
    _name_len: usize,
    _args_ptr: *const u8,
    _args_len: usize,
) -> *mut u8 {
    let mut i: u64 = 0;
    loop {
        core::ptr::write_volatile(core::ptr::addr_of_mut!(SINK), i);
        i = i.wrapping_add(1);
    }
}
