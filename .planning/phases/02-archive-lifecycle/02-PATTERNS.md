# Phase 2: Archive & Lifecycle - Pattern Map

**Mapped:** 2026-10-04
**Files analyzed:** 9 (modified) + 1-2 new
**Analogs found:** 9 / 9 (all are extensions of existing files; this phase is almost entirely "extend what's there," per RESEARCH.md)

## File Classification

| New/Modified File | Role | Data Flow | Closest Analog | Match Quality |
|---|---|---|---|---|
| `src/archive.rs` (new, recommended) | service/utility | batch (disk scan, prune) | `src/agent_registry.rs` (style: iterate entries, mutate state, plain fs) | role-match (new file, mirrors existing module's idioms) |
| `src/agent_registry.rs` (run-id format, `Interrupted` state, index.md hooks) | service/state-machine | CRUD + event-driven (state transitions) | itself (extend in place) | exact (self) |
| `src/agent/brief.rs` (front-matter render/parse for brief.md + report.md) | utility/transform | transform (string <-> struct, no serde) | itself (extend in place) | exact (self) |
| `src/paths.rs` (`project_agents_dir`, git-root helper) | utility/config | request-response (pure path functions) | itself, mirroring `project_skills_dir` | exact |
| `src/mode/print.rs` (atomic report.md write, front-matter fields) | controller (child-process exit path) | file-I/O | itself; delegate atomic write to `src/tool/file_state.rs::atomic_write` | exact (self) + CRUD analog for the atomic-write call |
| `src/tool/grep.rs`, `src/tool/find.rs` (unconditional agents-root exclusion) | utility/filter | transform (path filtering during a walk) | itself (extend `walk`/`ripgrep_args`) | exact (self) |
| `src/config.rs` (`archive_keep_days`) | config | CRUD (struct field + default) | itself (`AgentConfig`) | exact |
| `src/main.rs` (call startup scan + prune) | controller (startup) | event-driven (one-shot at boot) | itself, near existing `AgentRegistry::new` + `set_global` install | exact |
| `src/mode/tui.rs`, `src/command.rs` (`/agents clean` slash command) | controller/route (slash command) | request-response | `/name` command (`SlashCmd::Name` → `KeyAction::ApplyName`/`ShowCurrentName`) | exact |

## Pattern Assignments

### `src/archive.rs` (new file — startup scan, prune, index.md, gitignore helper)

**Analog:** `src/agent_registry.rs` (for module style/idioms) + `src/tool/file_state.rs` (for the atomic-write primitive to call)

**Style to copy** — plain-fs iteration, no external crates beyond what's already linked, `Result<(), String>`-or-`io::Result` error style matching `agent_registry.rs::reserve`:
```rust
// src/agent_registry.rs:76-97 (reserve) — the iterate/create/push idiom to mirror
pub fn reserve(&self, agents_root: &Path) -> Result<(String, PathBuf), String> {
    let mut entries = self.lock();
    let live = entries.iter().filter(|e| !e.state.is_terminal()).count();
    if live >= self.max_live {
        return Err(format!("agent limit reached: {} live agents (max_live)", self.max_live));
    }
    let n = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
    let id = format!("a{n}");
    let dir = agents_root.join(&self.run_id).join(&id);
    create_private_dir(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    entries.push(AgentEntry { id: id.clone(), pid: None, state: AgentState::Queued, started: Instant::now(), dir: dir.clone() });
    Ok((id, dir))
}
```

**Atomic write to call, not reimplement** (`src/tool/file_state.rs:119-156`):
```rust
pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    // temp file in same dir -> write_all -> f.sync_all() (fsync) ->
    // copy permissions from existing target -> rename over it ->
    // remove temp file on any error path
}
```
Use this directly from `mode/print.rs`'s report.md write and from any brief.md front-matter rewrite (state transitions, startup `interrupted` rewrite). Do not write a second temp+rename implementation.

**Private-dir / private-file permission precedent to mirror** (0700 dirs, 0600 files):
```rust
// src/agent_registry.rs:137-150
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)] {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
    }
    #[cfg(not(unix))] { std::fs::create_dir_all(dir) }
}
```
```rust
// src/agent/brief.rs:64-78 (append_amendment) — 0o600 new-file mode pattern
let mut opts = std::fs::OpenOptions::new();
opts.create(true).append(true);
#[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; opts.mode(0o600); }
```
index.md must be created with the same 0600/0700 discipline — no existing precedent for index.md itself, so copy this exactly.

**Path helper pattern to mirror for `project_agents_dir`** (already exists, confirm it matches D-01 exactly):
```rust
// src/paths.rs:136-143
pub fn project_skills_dir(cwd: &Path) -> PathBuf {
    cwd.join(".nanopi").join("skills")
}
pub fn user_agents_dir() -> Option<PathBuf> {
    nanopi_home().map(|h| h.join("agents"))
}
```
Add `project_agents_dir(cwd: &Path) -> PathBuf { cwd.join(".nanopi").join("agents") }` right next to these — note `src/tool/agent.rs:667` currently inlines `cwd.join(".nanopi").join("agents")`; once the helper exists, replace that inline call too.

**No existing git-root helper** (confirmed absent by RESEARCH.md's own grep) — this is genuinely new code. Follow the explicit-`cwd: &Path`-parameter style used throughout `paths.rs` (e.g. `expand_against` takes injected roots instead of reading env/cwd internally) so tests can use `tempfile::tempdir()` fixtures without touching the real repo's `.gitignore` (Pitfall 3 in RESEARCH.md).

---

### `src/agent_registry.rs` (run-id format, `AgentState::Interrupted`, index.md wiring)

**Analog:** itself — extend the existing enum and `new()`.

**Current enum to extend** (`src/agent_registry.rs:16-32`):
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Queued, Running, Completed, LimitReached, Failed, Stopped,
}
impl AgentState {
    pub fn is_terminal(self) -> bool {
        !matches!(self, AgentState::Queued | AgentState::Running)
    }
}
```
Add `Interrupted` as a terminal variant (update `is_terminal` match arm accordingly — it already excludes only `Queued | Running`, so `Interrupted` falls into "terminal" automatically once added to the enum, no match-arm edit needed beyond the variant itself).

**Current run-id generation to change** (`src/agent_registry.rs:55-63`):
```rust
pub fn new(cfg: &AgentConfig) -> Arc<Self> {
    Arc::new(Self {
        run_id: uuid::Uuid::now_v7().to_string(),
        counter: AtomicU64::new(0),
        entries: Mutex::new(Vec::new()),
        max_live: cfg.max_live,
        semaphore: Arc::new(Semaphore::new(cfg.max_concurrency.max(1))),
    })
}
```
Replace `uuid::Uuid::now_v7().to_string()` with `format!("{}-{}", chrono::Local::now().format("%Y%m%d-%H%M%S"), short_uuid())` per D-01 / RESEARCH.md "Don't Hand-Roll" table (`chrono` + `uuid` already linked, no new crate).

**Test that will break and must be rewritten, not worked around** (`src/agent_registry.rs:221-242`):
```rust
#[test]
fn run_id_is_uuid_v7_and_ids_sequential() {
    let reg = AgentRegistry::new(&cfg(8, 4));
    let u = uuid::Uuid::parse_str(reg.run_id()).unwrap();
    assert_eq!(u.get_version_num(), 7);
    ...
}
```
This is Pitfall 2 from RESEARCH.md verbatim — fix the test's assertion (regex/split check for `YYYYMMDD-HHMMSS-<hex>`), not the new format.

---

### `src/agent/brief.rs` (front-matter for brief.md and report.md)

**Analog:** itself — the existing escape/render functions are the exact pattern to extend.

**Escaping pattern that MUST be reused for any new interpolated front-matter field** (`src/agent/brief.rs:24-36`, WR-02 precedent referenced in RESEARCH.md's Security Domain table):
```rust
fn escape_body(text: &str) -> String {
    text.lines()
        .map(|l| {
            if l.trim() == AMENDMENTS_MARKER || l.starts_with(AMENDMENT_PREFIX) {
                format!("\\{l}")
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}
```
Any new front-matter value built from user/model-controlled text (role, task-derived fields) must go through `escape_body` (or an equivalent front-matter-specific escaper that neutralizes a crafted `state: done` line) — this is the direct mitigation for the Tampering threat RESEARCH.md's Security Domain section calls out.

**Current `render_brief` to extend with a front-matter block ahead of `# Brief`** (`src/agent/brief.rs:38-59`):
```rust
pub fn render_brief(spec: &BriefSpec) -> String {
    let mut out = String::from("# Brief\n\n## Task\n\n");
    out.push_str(escape_body(spec.task.trim_end()).as_str());
    out.push_str("\n\n## Role\n\n");
    out.push_str(&escape_body(spec.role.as_deref().unwrap_or("(default)")));
    out.push_str("\n\n## Tools\n\n");
    if spec.tools.is_empty() { out.push_str("(all)\n"); }
    else { for t in &spec.tools { out.push_str(&format!("- {}\n", escape_body(t))); } }
    out.push_str("\n## Model\n\n");
    out.push_str(&escape_body(spec.model.as_deref().unwrap_or("(inherit)")));
    out.push_str("\n\n");
    out.push_str(AMENDMENTS_MARKER);
    out.push('\n');
    out
}
```
Prepend a `---\nid: ...\nrole: ...\nmodel: ...\ntools: ...\nstate: ...\nstarted: ...\nparent: ...\n---\n\n` block (D-02), hand-rolled `key: value` lines — no YAML crate, matching this file's existing no-serde discipline exactly.

**Current amendment append to extend with `(<time>)` suffix** (`src/agent/brief.rs:61-78`):
```rust
pub fn append_amendment(path: &Path, n: u32, text: &str) -> io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; opts.mode(0o600); }
    let mut f = opts.open(path)?;
    let chunk = format!("\n{AMENDMENT_PREFIX}{n}\n\n{}\n", escape_body(text.trim_end()));
    f.write_all(chunk.as_bytes())
}
```
Change the heading format string to `## Amendment {n} ({time})` — note `AMENDMENT_PREFIX = "## Amendment "` is matched by `strip_prefix` in `parse_amendments_with` (line 107); confirm the parser still finds the numeric prefix correctly when a trailing `(<time>)` is appended after the number (parser only needs `num.trim().parse::<u32>()` to succeed on the leading digits before the parenthetical — check whether `strip_prefix` + `.parse()` needs a split on whitespace first).

**Current `render_report` to extend with front-matter + reordered body** (`src/agent/brief.rs:160-174`):
```rust
pub fn render_report(status: &str, summary: &str, items: &[ChecklistItem]) -> String {
    let mut out = format!(
        "# Report\n\n## Status\n\n{status}\n\n## Summary\n\n{}\n\n## Checklist\n\n",
        summary.trim_end()
    );
    for it in items {
        let mark = if it.done { "x" } else { " " };
        if it.note.is_empty() { out.push_str(&format!("- [{mark}] {}\n", it.label)); }
        else { out.push_str(&format!("- [{mark}] {} — {}\n", it.label, it.note)); }
    }
    out
}
```
D-03 wants front-matter (id, final state, ended, turns, tokens, worktree/branch) ahead of the body, and body order summary → files changed → open issues. Per RESEARCH.md Open Question 3: render `worktree: (none)` / `branch: (none)` placeholders for now (ISO-01 worktrees are Phase 4) rather than inventing detection early.

---

### `src/paths.rs` (new `project_agents_dir`, git-root helper)

**Analog:** itself — direct copy of the `project_skills_dir` shape.

```rust
// src/paths.rs:136-143 — exact shape to mirror for project_agents_dir
pub fn project_skills_dir(cwd: &Path) -> PathBuf {
    cwd.join(".nanopi").join("skills")
}
pub fn user_agents_dir() -> Option<PathBuf> {
    nanopi_home().map(|h| h.join("agents"))
}
```
New function: `pub fn project_agents_dir(cwd: &Path) -> PathBuf { cwd.join(".nanopi").join("agents") }`, placed immediately after `project_skills_dir`. Corresponding test to mirror (`src/paths.rs:166-170`):
```rust
#[test]
fn project_skills_is_dot_nanopi_skills() {
    let d = project_skills_dir(Path::new("/tmp/proj"));
    assert_eq!(d, PathBuf::from("/tmp/proj/.nanopi/skills"));
}
```

For the git-root walk-up helper (genuinely new, no existing analog — RESEARCH.md confirms none exists), follow this file's injected-parameter testing style used in `expand_against` (`src/paths.rs:63-78` and its test harness at `src/paths.rs:174-181`) so tests never touch the real repo:
```rust
fn expand(s: &str) -> PathBuf {
    expand_against(s, Some(PathBuf::from("/np-root")), Some(PathBuf::from("/home/u")))
}
```
Write the git-root helper to take `cwd: &Path` explicitly (never call `std::env::current_dir()` internally) — this is Pitfall 3 in RESEARCH.md verbatim.

---

### `src/mode/print.rs` (atomic report.md write + front-matter fields)

**Analog:** itself, delegating to `src/tool/file_state.rs::atomic_write`.

**Current write to replace** (`src/mode/print.rs:603-613`):
```rust
fn write_private(path: &std::path::Path, body: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).write(true).truncate(true);
    #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; opts.mode(0o600); }
    opts.open(path)?.write_all(body.as_bytes())
}
```
**Call site** (`src/mode/print.rs:443-452`), which must keep its current best-effort framing per Pitfall 5 — do not let an atomic-write failure skip the write entirely:
```rust
// report.md on every exit path (D-11). Best effort.
let report_path = brief_path.as_deref().map(|p| {
    let rp = report_path_for(p);
    let summary = report_summary(answer.as_deref(), &turn_result);
    let items = checklist_items(checklist_reply.as_deref(), status);
    let body = crate::agent::brief::render_report(status, &summary, &items);
    if let Err(e) = write_private(&rp, &body) {
        eprintln!("nanopi: cannot write report {}: {e}", rp.display());
    }
    rp
});
```
Change `write_private` to call `crate::tool::file_state::atomic_write(path, body.as_bytes())` first; on error, fall back to the current plain-write (truncate+write) rather than giving up, per Pitfall 5 ("some report beats no report"). Preserve the existing 0600 mode behavior — `atomic_write` copies permissions from the *existing* target file, which is None for a first-ever report.md, so the fallback path (or a post-write `set_permissions`) must still land on 0600 for a brand-new file.

---

### `src/tool/grep.rs`, `src/tool/find.rs` (unconditional agents-root exclusion)

**Analog:** itself — extend the existing `walk`/`ripgrep_args` functions in place; this is Pitfall 4 in RESEARCH.md ("match by resolved path, not by bare directory name").

**Current by-name exclusion, NOT to copy for this feature** (`src/tool/grep.rs:385-394`, the fallback walker):
```rust
let name = e.file_name().to_string_lossy().into_owned();
let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
if !all {
    if name.starts_with('.') { continue; }
    if is_dir && IGNORE_DIRS.contains(&name.as_str()) { continue; }
}
```
This block is gated on `!all` — exactly the bug D-08 must close. Add a second, unconditional check right above it (independent of the `all` flag):
```rust
let full = e.path();
if is_dir && full.canonicalize().map(|c| c == resolved_agents_root).unwrap_or(false) {
    continue; // always skip, even under all=true
}
```
**ripgrep path** (`src/tool/grep.rs:224-267`) needs the equivalent: when `all` is true, `--hidden` is added unconditionally (lines 249-251) with no path-scoped exclusion for the agents root — add `a.push(format!("--glob=!{}", resolved_agents_root_glob));` unconditionally (outside the `if all {} else {}` branch), matching the `--glob=!{d}` syntax already used for `IGNORE_DIRS` (lines 255-257) but scoped to the resolved absolute path, not a bare name, so `.nanopi/skills` is never caught by the same glob.

---

### `src/config.rs` (`archive_keep_days`)

**Analog:** itself — `AgentConfig` struct, extend with one field + default, mirroring existing fields like `max_live`/`max_concurrency`/`token_budget`. (Not re-read in full this pass — RESEARCH.md's file-by-file table at line 88 already specifies the exact addition: `archive_keep_days: u64` default 2, in the `[agent]` section, same style as the other `AgentConfig` fields consumed by `AgentRegistry::new(&cfg)` at `agent_registry.rs:55`.)

---

### `src/mode/tui.rs`, `src/command.rs` (`/agents clean` slash command)

**Analog:** `/name` (`SlashCmd::Name`)

**Palette entry pattern** (`src/mode/tui.rs:188-217`):
```rust
MenuItem::new("/name", "Set the current session's name", SlashCmd::Name),
MenuItem::new("/skills", "List all loaded skills", SlashCmd::ListSkills),
```
Add: `MenuItem::new("/agents", "Clean up the agent archive", SlashCmd::CleanAgents)`.

**Dispatch pattern with an argument** (`src/mode/tui.rs:1740-1761`):
```rust
fn dispatch_slash(cmd: SlashCmd, arg: String) -> KeyAction {
    match cmd {
        SlashCmd::Compact => KeyAction::Compact,
        ...
        SlashCmd::Name => {
            if arg.is_empty() { KeyAction::ShowCurrentName } else { KeyAction::ApplyName(arg) }
        }
        ...
    }
}
```
Mirror this exactly for `SlashCmd::CleanAgents`: no-arg form removes all runs except current (D-10 first sentence), `--older <days>` parses `arg` into a day count and removes only older runs (D-10 second sentence) — e.g. `SlashCmd::CleanAgents => if let Some(days) = parse_older_arg(&arg) { KeyAction::CleanAgents { older_days: Some(days) } } else { KeyAction::CleanAgents { older_days: None } }`.

`KeyAction` handling should call into the new `archive.rs` deletion routine (never delete current run or a run with live agents — same invariant as the startup prune) and print a summary (count + size), matching how other command results are surfaced to the scrollback (follow whatever existing `KeyAction::ApplyName`/`KeyAction::ShowCurrentName` handler does for printing feedback — not re-read this pass, but same event-loop arm pattern applies).

## Shared Patterns

### Atomic, fsynced file writes (D-04, D-02 front-matter rewrites, index.md)
**Source:** `src/tool/file_state.rs:119-156` (`atomic_write`)
**Apply to:** `mode/print.rs` report.md write, `agent/brief.rs` front-matter state rewrites, `archive.rs` index.md writes, startup `interrupted` rewrite.
```rust
pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    // temp file same dir -> write_all -> f.sync_all() -> copy perms -> rename; cleans up temp on error
}
```
Do not write a second implementation anywhere in this phase.

### Front-matter-field escaping (no YAML, WR-02 precedent)
**Source:** `src/agent/brief.rs:24-36` (`escape_body`)
**Apply to:** every new interpolated front-matter value (role, any user/model-controlled string) in both `brief.md` and `report.md`.

### 0700 dir / 0600 file permission discipline
**Source:** `src/agent_registry.rs:137-150` (`create_private_dir`), `src/agent/brief.rs:64-71` (`append_amendment`'s `OpenOptions` + `mode(0o600)`)
**Apply to:** every new file this phase introduces — specifically `index.md`, which has no prior precedent and must not regress to default (world-readable) permissions.

### Path-scoped (not name-scoped) exclusion
**Source:** Pitfall 4 analysis of `src/tool/grep.rs:385-394` / `:255-257`
**Apply to:** both `grep.rs` and `find.rs`'s agents-root exclusion — compare canonicalized full paths against `project_agents_dir(cwd)`, never bare directory names, so `.nanopi/skills` stays searchable.

### Explicit-parameter testability (no hidden env/cwd reads inside helpers)
**Source:** `src/paths.rs:63 expand_against` (roots injected as parameters) and its test harness at `:174-181`
**Apply to:** the new git-root-detection helper (Pitfall 3) — take `cwd: &Path` as a parameter, let callers inject a `tempfile::tempdir()` in tests.

## No Analog Found

| File | Role | Data Flow | Reason |
|---|---|---|---|
| git-root / `.gitignore`-append helper (new fn, likely in `archive.rs` or `paths.rs`) | utility | file-I/O | RESEARCH.md confirms (by direct grep) no git-root-walking code exists anywhere in this codebase yet — this is genuinely new, follow the "Don't Hand-Roll" guidance (plain containment check before append, bounded walk-up depth mirroring `MAX_DEPTH` in grep.rs/find.rs) rather than a full gitignore-engine |
| `index.md` renderer/parser | model/service | CRUD (derived cache) | No existing "derived index of other files" pattern in this codebase; treat as a new, simple regenerate-from-scan function per RESEARCH.md's Pitfall 1 recommendation (regenerate from brief.md front-matter scans, never hand-maintain incrementally) |

## Metadata

**Analog search scope:** `src/agent_registry.rs`, `src/agent/brief.rs`, `src/paths.rs`, `src/mode/print.rs`, `src/mode/tui.rs`, `src/tool/grep.rs`, `src/tool/file_state.rs`, `src/tool/agent.rs`, `src/config.rs` (all read directly in this session or in the prior research pass)
**Files scanned:** 9 primary + targeted greps across `src/tool/find.rs`, `src/command.rs`
**Pattern extraction date:** 2026-10-04
