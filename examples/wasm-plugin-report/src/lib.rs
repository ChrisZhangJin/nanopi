//! `nanopi-tools-report` — where the time went, and what keeps failing.
//!
//! Subscribes to `tool_execution_end` (the data) and `session_start`
//! (so that "this session" means this session), and keeps per-tool
//! counts, total duration and failures in `host-store`, so the numbers
//! survive the session that produced them.
//!
//! ## Why this exists
//!
//! `tool_execution_end`'s payload carries `tool_response.duration_ms`,
//! and that number is persisted NOWHERE. The TUI prints `Took 5ms` on
//! the card and it scrolls away; the session transcript does not record
//! it; the status bar counts tokens, not time. So two ordinary questions
//! have no answer today:
//!
//!   - why was this session slow — which tool ate the wall clock?
//!   - what keeps failing, and with what arguments?
//!
//! Both scroll past you during a long session and are gone. This plugin
//! is a few hundred lines that answers them.
//!
//! ## What it deliberately is NOT
//!
//! **Not an audit log.** Event delivery is drop-on-busy and does not
//! queue: if this plugin is already inside a guest call when the next
//! event fires, that event is dropped, not deferred. Two tools finishing
//! at the same instant in a parallel batch is exactly when that happens.
//!
//! So every count here is a LOWER BOUND. That is fine for "which tool is
//! slow" — statistics tolerate sampling loss — and it is fatal for
//! anything that has to be complete. If you need a record with no holes,
//! use a `[[hooks.tool_execution_end]]` shell hook: hooks are
//! synchronous and cannot be dropped. The report says "≥" rather than
//! claiming a total, because a number that quietly undercounts is worse
//! than one that admits it.
//!
//! ## Capability mix
//!
//! `allow_store` plus two event grants, and nothing else — no filesystem,
//! no network, no context contribution, no tool calls. Compare
//! `examples/wasm-plugin-memory/`, which uses the other half of the
//! surface (`allow_fs` + `allow_tools` + `allow_context`) and subscribes
//! to no events at all.
//!
//! Everything above the "YOUR TOOLS" line is boilerplate copied verbatim
//! from `examples/wasm-plugin-minimal/`.
//!
//! Build (from the repo root, so `wit/` resolves):
//!   make plugin-report

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
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
    #[link_name = "host-notify"]
    fn host_notify_raw(ptr: *const u8, len: usize, ret_area: *mut u8);
    #[link_name = "host-store-get"]
    fn host_store_get_raw(ptr: *const u8, len: usize, ret_area: *mut u8);
    #[link_name = "host-store-set"]
    fn host_store_set_raw(
        key_ptr: *const u8,
        key_len: usize,
        val_ptr: *const u8,
        val_len: usize,
        ret_area: *mut u8,
    );
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

unsafe fn call_host_str2(
    f: unsafe extern "C" fn(*const u8, usize, *const u8, usize, *mut u8),
    a: &str,
    b: &str,
) -> String {
    let ret_area = ALLOC.alloc(Layout::from_size_align_unchecked(8, 4));
    f(a.as_ptr(), a.len(), b.as_ptr(), b.len(), ret_area);
    let ptr = ret_area.cast::<u32>().read() as *const u8;
    let len = ret_area.cast::<u32>().add(1).read() as usize;
    read_string(ptr, len)
}

unsafe fn host_notify(text: &str) -> String {
    call_host_str(host_notify_raw, text)
}

unsafe fn host_store_get(key: &str) -> String {
    call_host_str(host_store_get_raw, key)
}

unsafe fn host_store_set(key: &str, value: &str) -> String {
    call_host_str2(host_store_set_raw, key, value)
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

fn ok(content: String) -> String {
    serde_json::json!({ "content": content, "is_error": false }).to_string()
}

fn err(content: String) -> String {
    serde_json::json!({ "content": content, "is_error": true }).to_string()
}

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
    let name = read_string(name_ptr, name_len);
    let args_json = read_string(args_ptr, args_len);
    string_result(dispatch(&name, &args_json))
}

#[export_name = "list-commands"]
pub unsafe extern "C" fn list_commands() -> *mut u8 {
    string_result(COMMAND_SPECS.to_string())
}

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

#[export_name = "list-events"]
pub unsafe extern "C" fn list_events() -> *mut u8 {
    // Two events, and the second one is not optional for correctness.
    //
    // `tool_execution_end` is the data. `session_start` is what makes
    // "this session" MEAN this session: the per-session table can only
    // roll over when something tells the plugin a new session began,
    // and rolling over on the first tool event is not enough — a
    // session that calls no tools then leaves the PREVIOUS session's
    // numbers sitting under a "This session" heading. That was a real
    // bug here, caught by running three sessions and reading the store.
    //
    // Both still have to be granted in the config; the host warns about
    // a requested event the user did not grant, and the report degrades
    // to an honest label rather than a wrong one.
    string_result(r#"["tool_execution_end", "session_start"]"#.to_string())
}

#[export_name = "handle-event"]
pub unsafe extern "C" fn handle_event(
    event_ptr: *const u8,
    event_len: usize,
    payload_ptr: *const u8,
    payload_len: usize,
) -> *mut u8 {
    // NO `reset_arena()` here: the host already placed the payload in
    // this arena via `cabi_realloc`, so rewinding would free the very
    // bytes about to be read.
    let event = read_string(event_ptr, event_len);
    let payload = read_string(payload_ptr, payload_len);
    match event.as_str() {
        "tool_execution_end" => record(&payload),
        "session_start" => start_session(&payload),
        _ => {}
    }
    // The host discards this. Returning something valid anyway costs
    // nothing and keeps the export honest.
    string_result("{}".to_string())
}

// ═══ YOUR TOOLS — everything below is this plugin ═══════════════════

const KEY: &str = "stats";

/// How many recent failures to keep. The store is capped at 1 MiB total
/// and this is the only unbounded-by-nature field, so it needs a bound
/// that is not the quota — hitting the quota would make `host-store-set`
/// refuse, and then the stats would stop updating with only a log line
/// to say so.
const MAX_FAILURES: usize = 20;

/// Bytes of the failing call's arguments and error kept per failure.
/// Enough to recognise which call it was, short enough that 20 of them
/// stay small.
const SNIPPET: usize = 200;

const TOOL_SPECS: &str = r#"[
  {
    "name": "tool_stats",
    "description": "Report how long each tool has taken and which calls failed, for this session and all sessions. Use when asked why something was slow, what has been failing, or how much work has been done.",
    "parameters": {
      "type": "object",
      "properties": {
        "scope": { "type": "string", "description": "`session` for this session only, `all` for every recorded session. Defaults to `all`." }
      }
    }
  }
]"#;

const COMMAND_SPECS: &str = r#"[
  { "name": "tools-report", "description": "Show per-tool time and recent tool failures for this session and all time." }
]"#;

/// One tool's tally. Kept as a flat triple rather than a struct because
/// the whole state is a JSON blob in the store either way.
struct Tally {
    name: String,
    n: u64,
    ms: u64,
    fail: u64,
}

struct Stats {
    /// Folded totals for sessions that have ended.
    alltime: Vec<Tally>,
    /// The session currently being recorded.
    cur: Vec<Tally>,
    cur_session: String,
    sessions: u64,
    /// Whether `cur` belongs to a session we were told had STARTED, as
    /// opposed to one inferred from the first tool call. Only the former
    /// may be labelled "this session"; see `list-events`.
    cur_from_start: bool,
    /// Most recent failures, newest last.
    failures: Vec<(String, String, String)>,
}

fn empty_stats() -> Stats {
    Stats {
        alltime: Vec::new(),
        cur: Vec::new(),
        cur_session: String::new(),
        sessions: 0,
        cur_from_start: false,
        failures: Vec::new(),
    }
}

fn parse_tallies(v: Option<&serde_json::Value>) -> Vec<Tally> {
    let mut out = Vec::new();
    let Some(obj) = v.and_then(|v| v.as_object()) else {
        return out;
    };
    for (name, t) in obj {
        out.push(Tally {
            name: name.clone(),
            n: t.get("n").and_then(|x| x.as_u64()).unwrap_or(0),
            ms: t.get("ms").and_then(|x| x.as_u64()).unwrap_or(0),
            fail: t.get("fail").and_then(|x| x.as_u64()).unwrap_or(0),
        });
    }
    out
}

fn tallies_to_json(t: &[Tally]) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    for x in t {
        m.insert(
            x.name.clone(),
            serde_json::json!({ "n": x.n, "ms": x.ms, "fail": x.fail }),
        );
    }
    serde_json::Value::Object(m)
}

/// Load the stats blob.
///
/// A missing key and a malformed one are both treated as "start over",
/// but only the malformed case is worth a word: a store this plugin
/// cannot parse is a store it wrote in an older shape, and refusing to
/// work would be worse than losing counters. It logs rather than
/// notifying, because this is not something the user can act on.
fn load() -> Stats {
    let raw = unsafe { host_store_get(KEY) };
    if raw.starts_with("error: ") || raw.is_empty() {
        return empty_stats();
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        unsafe { host_log(2, "tools-report: stored stats did not parse; starting over") };
        return empty_stats();
    };
    let mut failures = Vec::new();
    if let Some(arr) = v.get("failures").and_then(|f| f.as_array()) {
        for f in arr {
            let g = |k: &str| {
                f.get(k)
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string()
            };
            failures.push((g("tool"), g("args"), g("err")));
        }
    }
    Stats {
        alltime: parse_tallies(v.get("alltime")),
        cur: parse_tallies(v.get("cur")),
        cur_session: v
            .get("cur_session")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        sessions: v.get("sessions").and_then(|x| x.as_u64()).unwrap_or(0),
        cur_from_start: v
            .get("cur_from_start")
            .and_then(|x| x.as_bool())
            .unwrap_or(false),
        failures,
    }
}

fn save(s: &Stats) {
    let failures: Vec<serde_json::Value> = s
        .failures
        .iter()
        .map(|(t, a, e)| serde_json::json!({ "tool": t, "args": a, "err": e }))
        .collect();
    let blob = serde_json::json!({
        "alltime": tallies_to_json(&s.alltime),
        "cur": tallies_to_json(&s.cur),
        "cur_session": s.cur_session,
        "sessions": s.sessions,
        "cur_from_start": s.cur_from_start,
        "failures": failures,
    })
    .to_string();
    let r = unsafe { host_store_set(KEY, &blob) };
    if r.starts_with("error: ") {
        // The quota is the realistic cause, and it is the one case where
        // the numbers silently stop moving. Say so once rather than let
        // the report keep looking live.
        unsafe {
            host_log(3, &format!("tools-report: could not save stats: {r}"));
            let _ = host_notify(&format!("tool stats are no longer being recorded: {r}"));
        }
    }
}

fn bump(t: &mut Vec<Tally>, name: &str, ms: u64, failed: bool) {
    if let Some(x) = t.iter_mut().find(|x| x.name == name) {
        x.n += 1;
        x.ms += ms;
        if failed {
            x.fail += 1;
        }
        return;
    }
    t.push(Tally {
        name: name.to_string(),
        n: 1,
        ms,
        fail: u64::from(failed),
    });
}

fn clip(s: &str, n: usize) -> String {
    // Char-wise, not byte-wise: slicing a multi-byte character in half
    // would produce a string the host has to reject.
    let mut out: String = s.chars().take(n).collect();
    if out.chars().count() < s.chars().count() {
        out.push('…');
    }
    out
}

/// Roll `cur` into `alltime` when the session id changes.
///
/// Keyed on the id, so calling it from both `session_start` and the
/// first tool event is idempotent — whichever arrives first does the
/// work and the other is a no-op. That redundancy is deliberate:
/// `session_start` gives the correct label, and the tool-event path is
/// the fallback if that delivery was dropped.
fn roll_over(s: &mut Stats, session: &str, from_start: bool) -> bool {
    if s.cur_session == session {
        // Already this session's table. A `session_start` arriving for
        // a session we inferred from a tool call still upgrades the
        // label, which is the one thing worth changing here.
        if from_start {
            s.cur_from_start = true;
        }
        return false;
    }
    for t in s.cur.drain(..) {
        bump_by(&mut s.alltime, &t);
    }
    s.cur_session = session.to_string();
    s.cur_from_start = from_start;
    s.sessions += 1;
    true
}

/// A new session began. Roll the table over so "this session" is true.
fn start_session(payload: &str) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) else {
        return;
    };
    let session = v
        .get("session_id")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    if session.is_empty() {
        return;
    }
    let mut s = load();
    if roll_over(&mut s, &session, true) || s.cur_from_start {
        save(&s);
    }
}

/// Fold one `tool_execution_end` payload into the stats.
///
/// Must stay cheap: this runs on the critical path of every tool call,
/// inside a 2s guest budget. One store read and one store write, no
/// parsing of the tool's output beyond its error flag.
fn record(payload: &str) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) else {
        return;
    };
    let tool = v
        .get("tool_name")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    if tool.is_empty() {
        return;
    }
    let session = v
        .get("session_id")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let args = v.get("arguments").cloned().unwrap_or(serde_json::Value::Null);
    let resp = args.get("tool_response");
    let ms = resp
        .and_then(|r| r.get("duration_ms"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    let failed = resp
        .and_then(|r| r.get("is_error"))
        .and_then(|x| x.as_bool())
        .unwrap_or(false);

    let mut s = load();

    // Normally `session_start` has already done this; the `false` says
    // a rollover happening HERE was inferred from a tool call rather
    // than announced, which is what downgrades the report's label.
    // `alltime` lags by one session either way, which is why every
    // reader below sums `alltime + cur`.
    roll_over(&mut s, &session, false);

    bump(&mut s.cur, &tool, ms, failed);

    if failed {
        let input = args
            .get("tool_input")
            .map(|x| x.to_string())
            .unwrap_or_default();
        let e = resp
            .and_then(|r| r.get("content"))
            .and_then(|x| x.as_str())
            .unwrap_or("");
        s.failures
            .push((tool, clip(&input, SNIPPET), clip(e, SNIPPET)));
        while s.failures.len() > MAX_FAILURES {
            s.failures.remove(0);
        }
    }

    save(&s);
}

fn bump_by(t: &mut Vec<Tally>, src: &Tally) {
    if let Some(x) = t.iter_mut().find(|x| x.name == src.name) {
        x.n += src.n;
        x.ms += src.ms;
        x.fail += src.fail;
        return;
    }
    t.push(Tally {
        name: src.name.clone(),
        n: src.n,
        ms: src.ms,
        fail: src.fail,
    });
}

/// Merge the lagging all-time table with the live one.
fn combined(s: &Stats) -> Vec<Tally> {
    let mut out: Vec<Tally> = Vec::new();
    for t in s.alltime.iter().chain(s.cur.iter()) {
        bump_by(&mut out, t);
    }
    out
}

fn fmt_ms(ms: u64) -> String {
    if ms < 1000 {
        return format!("{ms}ms");
    }
    format!("{}.{}s", ms / 1000, (ms % 1000) / 100)
}

/// Render a table, slowest first — the order that answers "where did
/// the time go" without the reader doing arithmetic.
fn render(title: &str, t: &[Tally]) -> String {
    if t.is_empty() {
        return format!("{title}: nothing recorded yet\n");
    }
    let mut v: Vec<&Tally> = t.iter().collect();
    // No `sort_by_key` on a borrowed comparator in this no_std setup;
    // an insertion sort over a handful of tools is not worth more.
    for i in 1..v.len() {
        let mut j = i;
        while j > 0 && v[j - 1].ms < v[j].ms {
            v.swap(j - 1, j);
            j -= 1;
        }
    }
    let total: u64 = v.iter().map(|x| x.ms).sum();
    let calls: u64 = v.iter().map(|x| x.n).sum();
    let fails: u64 = v.iter().map(|x| x.fail).sum();
    let mut out = format!(
        "{title}: ≥{calls} call(s), {} total, {fails} failed\n",
        fmt_ms(total)
    );
    for x in v {
        let avg = if x.n > 0 { x.ms / x.n } else { 0 };
        out.push_str(&format!(
            "  {:<14} {:>4} call(s)  {:>8} total  {:>7} avg{}\n",
            x.name,
            x.n,
            fmt_ms(x.ms),
            fmt_ms(avg),
            if x.fail > 0 {
                format!("  ({} failed)", x.fail)
            } else {
                String::new()
            }
        ));
    }
    out
}

fn report(scope_all: bool) -> String {
    let s = load();
    let mut out = String::new();
    if scope_all {
        out.push_str(&render(
            &format!("All time ({} session(s))", s.sessions.max(1)),
            &combined(&s),
        ));
        out.push('\n');
    }
    // Only claim "this session" when a `session_start` said so. Without
    // that grant the table is whichever session last called a tool,
    // which may not be the one you are sitting in — saying so is the
    // difference between a report and a wrong report.
    let label = if s.cur_from_start {
        "This session"
    } else {
        "Most recent session that called a tool (grant `session_start` to track the current one)"
    };
    out.push_str(&render(label, &s.cur));
    if !s.failures.is_empty() {
        out.push_str(&format!("\nLast {} failure(s), newest last:\n", s.failures.len()));
        for (t, a, e) in &s.failures {
            out.push_str(&format!("  {t}  {a}\n    → {e}\n"));
        }
    }
    // Not a footnote. Counts here are a floor, and a reader who assumes
    // otherwise will draw wrong conclusions from a parallel batch.
    out.push_str(
        "\nCounts are a LOWER BOUND: event delivery is dropped (not queued) while this \
         plugin is busy, which is most likely when two tools finish at once. For a record \
         with no holes use a [[hooks.tool_execution_end]] shell hook.\n",
    );
    out
}

fn dispatch(name: &str, args_json: &str) -> String {
    match name {
        "tool_stats" => {
            let scope_all = match serde_json::from_str::<serde_json::Value>(args_json) {
                Ok(v) => v
                    .get("scope")
                    .and_then(|x| x.as_str())
                    .map(|s| s != "session")
                    .unwrap_or(true),
                Err(_) => true,
            };
            ok(report(scope_all))
        }
        other => err(format!("unknown tool: {other}")),
    }
}

fn dispatch_command(name: &str, _args: &str) -> String {
    match name {
        "tools-report" => serde_json::json!({ "print": report(true) }).to_string(),
        other => serde_json::json!({ "error": format!("unknown command: {other}") }).to_string(),
    }
}
