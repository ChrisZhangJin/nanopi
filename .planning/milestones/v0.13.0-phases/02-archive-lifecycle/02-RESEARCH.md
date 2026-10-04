# Phase 2: Archive & lifecycle - Research

**Researched:** 2026-10-04
**Domain:** Rust CLI agent runtime — markdown-file archive, lifecycle state machine, startup recovery, cleanup command
**Confidence:** HIGH (this is almost entirely a codebase-reading task; no new external library is needed)

<user_constraints>
## User Constraints (from CONTEXT.md)

### Locked Decisions

**Layout**
- D-01: Each agent gets a directory `.nanopi/agents/<run>/<id>/`, under the project root, containing `brief.md`, `report.md` and `transcript.jsonl` (from Phase 1). `<run>` is `YYYYMMDD-HHMMSS-<short-uuid>`, one per nanopi process. `.nanopi/agents/<run>/index.md` lists the agents in that run with their state.
- D-02: `brief.md` has a small hand-written front-matter block: id, role, model, tools, state, started, parent. The body holds the task text. Each amendment is appended as `## Amendment N (<time>)`. No YAML crate is used.
- D-03: `report.md` front-matter: id, final state, ended, turns, tokens, and the worktree / branch if there is one. The body is the agent's final summary, then files changed, then open issues.

**Durability**
- D-04: Write `report.md` (fsync, write to a temp file then rename) before returning the result to the parent or emitting the done event. This applies to every terminal state: done, failed, stopped, limit_reached. On failure, the report is the error text plus whatever partial summary exists.

**State machine**
- D-05: The states are `queued → running ⇄ waiting_permission → done | failed | stopped | limit_reached`, plus `interrupted`. `state` in the brief front-matter and `index.md` is kept current.
- D-06: At startup, every archived agent whose state is not terminal is rewritten as `interrupted`. It is never re-run.

**Git / search hygiene**
- D-07: When an archive is first created, add `.nanopi/agents/` to the project `.gitignore` if it is a git repo and the entry is missing. This happens once and is idempotent.
- D-08: The built-in grep and glob tools skip `.nanopi/agents/`.

**Cleanup**
- D-09: Auto-prune at startup: delete run directories older than **2 days**. This is configurable as `agent.archive_keep_days` (default 2; 0 disables auto-prune). Never delete the current run, or a run that still has live agents.
- D-10: `/agents clean` removes all runs except the current one. `/agents clean --older <days>` removes only older runs. It reports what it removed (count and size).

### Claude's Discretion
- Exact markdown wording and layout; temp-file naming.

### Deferred Ideas (OUT OF SCOPE)
- None listed beyond the above; see REQUIREMENTS.md "Out of Scope" table for project-wide deferrals (automatic recovery/re-run of interrupted agents is explicitly deferred — ARC-04 only marks `interrupted`, never resumes).

### Revision 2026-10-03 (supersedes conflicting decisions above)
Phase 1 changed to a child-process runtime. Agents are `nanopi -p` children controlled only by the orchestrator; the user never controls them directly. brief.md is the amendment channel: the orchestrator appends `## Amendment N`; the child reads it between turns and self-checks it before writing report.md (P1 D-09..D-11). report.md carries a per-item checklist.

**Note on D-05 state names:** the Phase 1 code already implements an `AgentState` enum with variants `Queued, Running, Completed, LimitReached, Failed, Stopped` (see `src/agent_registry.rs`). D-05 asks for `queued, running, waiting_permission, done, failed, stopped, limit_reached, interrupted`. The existing names are close but not identical (`Completed` vs `done`, no `waiting_permission`, no `interrupted`). Reconcile naming during planning — `waiting_permission` is likely not reachable in the current architecture because RT-07 already resolves permissions at dispatch time and a child never prompts (see Open Questions).
</user_constraints>

<phase_requirements>
## Phase Requirements

| ID | Description | Research Support |
|----|-------------|------------------|
| ARC-01 | Each agent writes `.nanopi/agents/<run>/<id>/brief.md` (task, role, tools, model) when it starts. Amendments are appended to that file. | Mostly built. `agent::brief::render_brief` (`src/agent/brief.rs`) already emits `## Task`, `## Role`, `## Tools`, `## Model` sections and `append_amendment` already appends `## Amendment N`. Missing: the D-02 front-matter block (id, role, model, tools, state, started, parent) and the `(<time>)` suffix on amendment headings. |
| ARC-02 | `report.md` is written before the result is returned to the parent, so a report cannot be lost. | Already true in sequence (`src/mode/print.rs` writes report.md before the child process exits; `src/tool/agent.rs::run_single` only reads it back after `spawn_and_collect` returns). Missing: D-04's temp-file+fsync+rename durability (current `write_private` is a plain truncate+write) and D-03's report front-matter. |
| ARC-03 | `.nanopi/agents/` is added to `.gitignore` automatically and is excluded from the agents' own searches. | Not built for arbitrary projects (nanopi's own repo has a hand-committed `/.nanopi/` line in its own `.gitignore`, but there is no code that writes this into a project being orchestrated). `grep.rs`/`find.rs` skip dotdirs by default but that protection evaporates under `all=true` — needs an unconditional, path-based exclusion of the resolved agents root. |
| ARC-04 | Agents that were still running when nanopi exited are marked `interrupted` on the next start; they are not re-run. | Not built. State currently lives only in the in-process `AgentRegistry` (a `Mutex<Vec<AgentEntry>>`), which is thrown away on exit. There is no startup scan of `.nanopi/agents/*/*/brief.md` (or index.md) to detect and rewrite non-terminal states. |
| ARC-05 | The user can clean up the archive with one command, e.g. `/agents clean`. Keep most recent N runs or remove everything. | Not built. No `/agents` slash command exists. Needs a new `SlashCmd` variant, palette entry, dispatch arm, and a registry-side or standalone deletion routine that respects "never delete the current run or a run with live agents." |

</phase_requirements>

## Summary

Phase 1 already built most of the plumbing this phase formalizes: `.nanopi/agents/<run>/<id>/` directories exist, `brief.md` and `report.md` are written, amendments are appended and parsed, and `report.md` is written before the tool result returns to the parent (ARC-02's ordering already holds). What Phase 2 adds is the *contract* around that existing machinery: richer front-matter on both files, `index.md`, startup interruption-marking, auto-prune + `/agents clean`, per-project `.gitignore` registration, and grep/find exclusion that holds even under `--all`.

The single biggest structural gap is the run-id format: `AgentRegistry::new` (`src/agent_registry.rs`) currently names the run directory with a bare UUIDv7 string, not the user-decided `YYYYMMDD-HHMMSS-<short-uuid>` (D-01). This is a one-line format change plus a rewrite of the test that asserts the directory name parses as a UUID.

The second structural gap is durability of state across process restarts: state today lives only in an in-memory `Mutex<Vec<AgentEntry>>` inside `AgentRegistry`, which does not survive the parent process exiting. ARC-04 requires reading state back from disk at the next startup — this means `index.md` (or brief.md's front-matter) must become the durable source of truth, written by both the parent (on state transitions it observes: queued, running, completed/failed/stopped) and read once at startup before any new dispatch happens.

**Primary recommendation:** Extend `agent_registry.rs` (run-id format, index.md writer, startup interrupted-scan, prune-by-age) and `agent::brief` (front-matter rendering/parsing for both brief.md and report.md), add a small git-root-detection + `.gitignore`-append helper (new function, e.g. in `paths.rs` or a new `archive.rs`), patch the hard-coded `IGNORE_DIRS` exclusion logic in `grep.rs`/`find.rs` to unconditionally hide the resolved agents root even when `all=true`, and add a new `/agents` slash command following the existing `/tools`/`/name` pattern in `src/mode/tui.rs` + `src/command.rs`. No new crates are required — `chrono` (already a dependency, `features = ["clock"]`) covers the `YYYYMMDD-HHMMSS` formatting, and `uuid` (already a dependency) covers the short-uuid suffix.

## Architectural Responsibility Map

| Capability | Primary Tier | Secondary Tier | Rationale |
|------------|-------------|----------------|-----------|
| Agent dir / run-id naming | Backend (`agent_registry.rs`) | — | Already owns reservation + dir creation |
| brief.md front-matter + amendments | Backend (`agent::brief`) | Child process (`mode/print.rs`, `mode/brief_watch.rs`) | Written by the parent (`tool/agent.rs`), read by the child |
| report.md front-matter + durability | Child process (`mode/print.rs`) | Backend (`tool/agent.rs` reads it back) | Child writes its own report before exiting; parent only reads the file afterward |
| index.md | Backend (`agent_registry.rs`) | Child process (indirectly, via state it reports back through the JSON envelope) | Single-writer discipline needed — see Pitfalls |
| Startup interrupted-marking | Backend (`main.rs`, before any dispatch) | — | Must run once, before the TUI/print loop starts, scanning the archive on disk |
| `.gitignore` registration | Backend (`agent_registry.rs` or new `archive.rs`, on first archive creation in a project) | — | One-time, idempotent, project-root scoped — NOT nanopi's own repo's `.gitignore` |
| grep/glob exclusion | Tool tier (`tool/grep.rs`, `tool/find.rs`) | — | The only two "search" built-ins; must exclude unconditionally |
| `/agents clean` | TUI/CLI command tier (`src/command.rs`, `src/mode/tui.rs`) | Backend (`agent_registry.rs` does the actual filesystem deletion) | Follows the existing `/tools`, `/name` slash-command pattern |

## What Already Exists vs. What's Missing (by file)

| File | What's already there | What Phase 2 must add |
|------|----------------------|------------------------|
| `src/agent_registry.rs` | `AgentRegistry` (run_id = bare UUIDv7, sequential `a1..` ids), `AgentState` enum (Queued/Running/Completed/LimitReached/Failed/Stopped), `reserve()` creates `0700` dirs, `kill_all()` on exit, `ChildGuard` SIGKILLs process groups | Run-id format → `YYYYMMDD-HHMMSS-<short-uuid>`; `AgentState::Interrupted` variant; `index.md` writer (one list entry per agent, updated on every `set_state`); startup scan function that reads prior run dirs and rewrites non-terminal states to `Interrupted`; prune-by-age function (`archive_keep_days`, default 2, 0 disables); never touch the current run or a run with live agents |
| `src/agent/brief.rs` | `render_brief` (Task/Role/Tools/Model sections + amendments marker), `append_amendment`/`parse_amendments_with`, `render_report` (Status/Summary/Checklist, no front-matter) | Front-matter block ahead of `# Brief` (id, role, model, tools, state, started, parent per D-02); `## Amendment N (<time>)` heading (currently just `## Amendment N`); front-matter block in `render_report` (id, final state, ended, turns, tokens, worktree/branch per D-03); body reorder to summary → files changed → open issues |
| `src/mode/print.rs` | Writes `report.md` via `write_private` (plain truncate+write, 0600) before the child process returns its JSON envelope; builds checklist items from the self-check reply | Switch `write_private` (or a shared helper) to temp-file + fsync + rename (D-04); populate the new report.md front-matter fields (turns, tokens — likely already tracked somewhere in `Agent`/loop stats; worktree/branch — not tracked at all yet, see Open Questions since ISO-01 worktrees are Phase 4) |
| `src/tool/agent.rs` | Reserves registry slot, writes `brief.md` via `render_brief`, reads `report.md` back, maps JSON `status` to `AgentState` | Needs to also record `parent` in the brief (if nested agents become relevant later — currently N/A since agents cannot spawn agents per RT-05, so `parent` is always the orchestrator's own run/id, not another agent) |
| `src/config.rs` | `AgentConfig { max_live, max_concurrency, max_turns, token_budget, timeout_secs }` | Add `archive_keep_days: u64` (default 2) to the `[agent]` section |
| `src/main.rs` | Installs `AgentRegistry::new(&cfg)` as the global registry at startup; calls `kill_all()` at the single exit point | Call the new startup-scan (mark interrupted) and prune-by-age functions here, before the TUI/print loop starts and before any new dispatch is possible |
| `src/tool/grep.rs`, `src/tool/find.rs` | `IGNORE_DIRS` constant (`.git`, `node_modules`, `target`, `.venv`, `dist`, `build`, `.direnv`) skipped by *name* only when `all=false`; dotdirs (anything starting with `.`) skipped by default, but that guard is bypassed entirely when `all=true` | Add an unconditional check (independent of `all`) that excludes the resolved project agents root (`cwd.join(".nanopi").join("agents")`) — matching by resolved path, not by bare directory name, so a differently-rooted `.nanopi` (e.g. via `NANOPI_HOME` override) isn't accidentally matched/missed |
| `src/paths.rs` | `user_agents_dir()` (`~/.nanopi/agents`), `project_skills_dir(cwd)` pattern to copy | Add `project_agents_dir(cwd) -> PathBuf` (`cwd.join(".nanopi").join("agents")`) mirroring `project_skills_dir`; no existing git-root helper anywhere in the codebase (confirmed by grep) — a new one is needed for D-07 |
| `src/mode/tui.rs`, `src/command.rs` | Slash command pattern: `SlashCmd` enum variant → `slash_items()` palette entry → `dispatch_slash` match arm → `KeyAction` variant handled in the main event loop (see `/tools`, `/name`) | New `SlashCmd::CleanAgents(Option<String>)` or similar, palette entry `/agents`, `KeyAction::CleanAgents { older_days: Option<u64> }`, handled by calling into `agent_registry`'s deletion routine and printing a summary (count + size freed) |
| `.gitignore` (nanopi's own repo) | Already has `/.nanopi/` — this is unrelated to ARC-03, which is about *projects nanopi orchestrates in*, not nanopi's own source tree | N/A — do not confuse the two |

## Don't Hand-Roll

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| `YYYYMMDD-HHMMSS` timestamp formatting | Manual `format!` with `SystemTime` arithmetic | `chrono::Local::now().format("%Y%m%d-%H%M%S")` (already a dependency with `features = ["clock"]`) | Correct, leap-second-safe, no manual calendar math |
| Short UUID suffix | A hand-rolled random-hex generator | `uuid::Uuid::new_v4()` (or keep `now_v7()`) truncated to its first 8 hex chars via `.simple().to_string()[..8]` | `uuid` crate is already linked; v7 is already used elsewhere in this codebase for run ids, so reusing it keeps one RNG-backed id source |
| Atomic file write with fsync | A second temp-file-plus-rename implementation | `src/tool/file_state.rs::atomic_write` (from Phase 1, ISO-03) — check whether it already fsyncs; if not, extend it rather than writing a third copy (the codebase already notes "Session-file corruption on cancel was fixed in v0.11. Reuse that temp-file-then-rename pattern.") | Avoids a fourth independent atomic-write implementation in one codebase; Phase 1 already established the canonical pattern here |
| Markdown front-matter parsing | A YAML parser (explicitly forbidden by D-02: "No YAML crate is used") | A small hand-rolled `key: value` line parser, matching the existing `parse_amendments_with` style (plain string splitting, no serde) | D-02 is explicit; this is a locked decision, not a style preference |
| `.gitignore` idempotent append | Reimplementing gitignore semantics (globs, negation) | A plain string containment check against the file's existing lines before appending `.nanopi/agents/\n` | D-07 only asks for an idempotent single-line append, not full gitignore engine compliance |

**Key insight:** every piece of machinery Phase 2 needs (atomic writes, path helpers, slash-command registration, state enums) already has one canonical implementation somewhere in this codebase from Phase 1 or earlier. The main risk is building a second, slightly different one instead of extending the existing one.

## Architecture Patterns

### System Architecture Diagram

```
 nanopi startup (main.rs)
        │
        ▼
 ┌────────────────────────────┐
 │ 1. Install AgentRegistry   │  (existing)
 │    global + launch spec    │
 └──────────┬─────────────────┘
            ▼
 ┌────────────────────────────┐
 │ 2. NEW: scan archive root  │  reads every <run>/<id>/brief.md
 │    for non-terminal state  │  (or index.md) under project
 │    → rewrite "interrupted" │  .nanopi/agents/*/*
 └──────────┬─────────────────┘
            ▼
 ┌────────────────────────────┐
 │ 3. NEW: prune runs older   │  skip current run, skip any run
 │    than archive_keep_days  │  with a live (non-terminal) agent
 └──────────┬─────────────────┘
            ▼
      TUI / print loop runs normally
            │
            ▼ (user dispatches an agent)
 ┌────────────────────────────┐
 │ tool/agent.rs: reserve()   │  (existing) allocates <run>/<id>/
 │ writes brief.md (+front-   │  NEW: front-matter block
 │ matter), spawns child      │
 └──────────┬─────────────────┘
            ▼
 ┌────────────────────────────┐
 │ child (nanopi -p): runs    │  (existing) brief_watch polls
 │ turns, watches brief.md    │  for amendments
 │ for amendments             │
 └──────────┬─────────────────┘
            ▼
 ┌────────────────────────────┐
 │ child: writes report.md    │  NEW: front-matter + atomic
 │ BEFORE returning JSON      │  (temp+fsync+rename) write
 │ envelope (ARC-02, already  │
 │ true in sequence)          │
 └──────────┬─────────────────┘
            ▼
 ┌────────────────────────────┐
 │ parent: reads report.md,   │  (existing) sets AgentState
 │ updates registry state     │  NEW: also updates index.md
 └────────────────────────────┘

 Orthogonal, on first archive creation in ANY project:
 ┌────────────────────────────┐
 │ NEW: is this a git repo?   │  walk up from cwd looking for .git
 │ → append .nanopi/agents/   │  idempotent (check before append)
 │   to project .gitignore    │
 └────────────────────────────┘

 Orthogonal, every grep/find call:
 ┌────────────────────────────┐
 │ NEW: unconditionally skip  │  independent of --all, matched by
 │ resolved agents root path  │  resolved path not bare dir name
 └────────────────────────────┘

 On demand, /agents clean:
 ┌────────────────────────────┐
 │ TUI: SlashCmd::CleanAgents │
 │ → KeyAction → registry     │  deletes run dirs (all-but-current,
 │ deletion routine → summary │  or only those older than N days)
 │ printed to scrollback      │  never the current run or a live one
 └────────────────────────────┘
```

### Recommended Project Structure

No new top-level modules are strictly required; the cleanest fit is to extend existing files:

```
src/
├── agent_registry.rs   # + run-id format, index.md, startup scan, prune
├── agent/
│   └── brief.rs         # + front-matter render/parse for brief.md & report.md
├── paths.rs             # + project_agents_dir(cwd), git_root(cwd) helper
├── mode/
│   ├── print.rs         # + atomic report.md write, front-matter fields
│   └── tui.rs           # + /agents slash command dispatch
├── command.rs           # (if /agents needs to be reachable outside TUI too)
├── tool/
│   ├── grep.rs          # + unconditional agents-root exclusion
│   └── find.rs          # + unconditional agents-root exclusion
└── main.rs              # + call startup scan + prune before the main loop
```

A new `src/archive.rs` is a reasonable alternative if the planner prefers not to grow `agent_registry.rs` further — it would hold: run-id formatting, index.md read/write, startup scan, prune, and the `.gitignore` idempotent-append helper. This keeps `agent_registry.rs` focused on live-process bookkeeping and puts "durable archive on disk" in one place. **Recommendation: create `src/archive.rs`** — `agent_registry.rs` is already doing a lot (live registry + process-group kill) and mixing in disk-scanning/pruning/`.gitignore` logic would make it harder to review. Precedent: `src/paths.rs` was split out for exactly this kind of "stop reimplementing this in five places" reason.

### Pattern 1: Startup disk scan before any new dispatch
**What:** On every `main()` invocation, before installing anything that could dispatch a new agent, walk `.nanopi/agents/*/*/` (both project and — per D-01's "under the project root" wording, only project-scoped; `~/.nanopi/agents` existence from Phase 1's `user_agents_dir()` should be double-checked against D-01, see Open Questions) looking for brief.md/index.md entries whose state is not in `{done, failed, stopped, limit_reached, interrupted}`, and rewrite them to `interrupted`.
**When to use:** Exactly once, at the very top of `main()`, before the registry can accept any new `reserve()` call.
**Example:**
```rust
// Source: pattern inferred from existing agent_registry.rs::kill_all style
// (iterate entries, mutate state, no network/IO beyond the filesystem)
pub fn mark_interrupted_on_startup(agents_root: &Path, archive_keep_days: u64) -> io::Result<()> {
    for run_dir in list_run_dirs(agents_root)? {
        for agent_dir in list_agent_dirs(&run_dir)? {
            let brief = agent_dir.join("brief.md");
            if let Some(state) = read_state_field(&brief)? {
                if !state_is_terminal(&state) {
                    rewrite_state_field(&brief, "interrupted")?;
                    // also update <run>/index.md's row for this id
                }
            }
        }
    }
    Ok(())
}
```

### Pattern 2: Single-writer front-matter update (no YAML)
**What:** `brief.md`'s front-matter is a handful of `key: value` lines. Updating `state` means rewriting just that line, not the whole file, while amendments are strictly append-only below the marker.
**When to use:** Any time `state` changes (parent transitions on dispatch/completion; startup interrupted-scan).
**Example:**
```rust
// Source: this codebase's existing style (agent/brief.rs has no serde
// dependency for its own format; mirror that, do not add toml/serde_yaml)
fn rewrite_front_matter_field(content: &str, key: &str, new_value: &str) -> String {
    content
        .lines()
        .map(|l| {
            if let Some(rest) = l.strip_prefix(&format!("{key}: ")) {
                let _ = rest; // old value discarded
                format!("{key}: {new_value}")
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}
```
This must be written back atomically (temp+rename), same as report.md, since a half-written brief.md front-matter would be just as bad as a lost report.

### Anti-Patterns to Avoid
- **Re-deriving state from `report.md` existence alone:** a crashed child may have a `report.md` partially written (if durability is skipped) — always check the front-matter `state` field, not just file presence.
- **Matching `.nanopi` by directory *name* in grep/find:** this would also hide `.nanopi/skills` from an agent's own search, which is not what D-08 asks for (only `.nanopi/agents/` should be hidden). Match by resolved path, scoped precisely to the agents root.
- **Writing index.md and brief.md state independently without a single source of truth:** pick one as canonical (recommend: brief.md's front-matter, since each child already writes/updates its own) and have index.md be a derived, rebuildable cache — see Pitfalls.

## Common Pitfalls

### Pitfall 1: index.md and brief.md state drifting apart
**What goes wrong:** If both files are updated independently (e.g., parent writes index.md on state transition, child writes brief.md's front-matter on its own transitions), a crash between the two writes leaves them disagreeing, and the startup scan doesn't know which one to trust.
**Why it happens:** Two writers, two files, no single commit point.
**How to avoid:** Make brief.md's `state` field (owned by whichever process currently controls the agent — parent while queued/running from the launcher's perspective, but the *child* is actually the one that knows when it's done, since it writes report.md) the single source of truth, and have index.md be entirely regenerated from a scan of brief.md files in that run directory whenever it's displayed or updated — never hand-maintained incrementally. This also makes the startup scan simpler: it only has to read brief.md files to decide what to rewrite, then regenerate index.md once per run directory touched.
**Warning signs:** Any test that updates index.md and brief.md as two separate assertions without a round-trip check that they agree.

### Pitfall 2: Run-id format change breaking existing tests/paths
**What goes wrong:** `agent_registry.rs`'s own test (`run_id_is_uuid_v7_and_ids_sequential`) asserts `uuid::Uuid::parse_str(reg.run_id())` succeeds. Changing the format to `YYYYMMDD-HHMMSS-<short-uuid>` breaks this parse.
**Why it happens:** The format is currently load-bearing in a test, not just documentation.
**How to avoid:** Rewrite the test to check the new format with a regex or manual split, and grep for any other place that calls `Uuid::parse_str` on a run_id (checked: only this one test site currently does).
**Warning signs:** `cargo test agent_registry` failing after the format change — expected, make sure the fix is in the test, not a workaround in the format.

### Pitfall 3: `.gitignore` append targeting the wrong repo root
**What goes wrong:** nanopi itself is a git repo. If the git-root-detection walks up from `cwd` without stopping at the actual project boundary, or if it's tested from inside nanopi's own working tree, it's easy to accidentally write to nanopi's own `.gitignore` during a test run instead of a tmpdir fixture's.
**Why it happens:** No existing git-root helper in this codebase (confirmed absent) — this is new code with no precedent to copy exactly.
**How to avoid:** Write the helper to take an explicit `cwd: &Path` (never implicitly use `std::env::current_dir()` inside the helper itself — let the caller pass it, matching the `agents_root: &Path` style already used in `AgentRegistry::reserve`), and have every test use a `tempfile::tempdir()` fixture, never the real repo.
**Warning signs:** A test modifies `/root/workspace/nanopi/.gitignore` — instant red flag, revert and fix the fixture.

### Pitfall 4: grep/find exclusion by name re-hiding `.nanopi/skills`
**What goes wrong:** The quick fix "just add `.nanopi` to `IGNORE_DIRS`" would also hide project skills (`.nanopi/skills`) from an agent's own grep/find, which is a regression — skills need to stay searchable.
**Why it happens:** `IGNORE_DIRS` currently matches bare directory *names* at any depth, not full paths.
**How to avoid:** Add a path-based check (compare the canonicalized full path against the resolved `project_agents_dir(cwd)`, independent of the by-name `IGNORE_DIRS` loop and independent of the `all` flag).
**Warning signs:** A test searching with `all: true` still finds skill files — good; a test searching with `all: true` still finds `.nanopi/agents/*/brief.md` — bad, fix needed.

### Pitfall 5: report.md durability upgrade breaking the "D-04 applies to every terminal state including failure" requirement
**What goes wrong:** `mode/print.rs` currently writes report.md via `write_private` on every exit path already ("on every exit path (D-11)" per its own comment) — but if the atomic-write upgrade introduces an early-return-on-error path that skips the write when, say, the temp file can't be created in a read-only directory, a failed agent could silently lose its report, which is explicitly what D-04 forbids ("On failure, the report is the error text plus whatever partial summary exists").
**Why it happens:** Atomic writes have more failure modes (temp-file creation, fsync, rename) than a plain write, and it's tempting to `?`-propagate them all the same way.
**How to avoid:** Keep `write_private`'s current "best effort" framing (the existing comment literally says "Best effort") — log a warning on atomic-write failure but still attempt a non-atomic fallback write rather than giving up, since *some* report beats no report.
**Warning signs:** A test that makes the temp directory read-only and checks that report.md still ends up with content (even if not perfectly atomic) rather than being empty or absent.

## Code Examples

### Existing atomic write pattern to reuse/extend (Phase 1, ISO-03)
```rust
// Source: src/tool/file_state.rs (Phase 1, 01-01-SUMMARY.md: "atomic_write")
// Check this function's current fsync behavior before assuming it's
// sufficient for D-04 — Phase 1's summary says "temp-file-plus-rename"
// but does not explicitly confirm an fsync call. Verify during planning
// with: grep -n "fsync\|sync_all" src/tool/file_state.rs
```

### Existing front-matter-free brief rendering (to extend)
```rust
// Source: src/agent/brief.rs (this repo, read directly in this research pass)
pub fn render_brief(spec: &BriefSpec) -> String {
    let mut out = String::from("# Brief\n\n## Task\n\n");
    // ... Task / Role / Tools / Model sections already exist ...
    out.push_str(AMENDMENTS_MARKER);
    out.push('\n');
    out
}
// Phase 2 needs a front-matter block BEFORE "# Brief", e.g.:
//   ---
//   id: a1
//   role: reviewer
//   model: claude-sonnet-5
//   tools: read, grep
//   state: running
//   started: 2026-10-04T10:22:03Z
//   parent: 20261004-102200-ab12cd34
//   ---
// Rendered with plain string formatting (D-02: "No YAML crate is used"),
// matching this file's existing no-serde style exactly.
```

## State of the Art

| Old Approach | Current Approach | When Changed | Impact |
|--------------|------------------|---------------|--------|
| In-process subagents (shared memory, no isolation) | Child-process agents (`nanopi -p`), isolated by OS process | Phase 1, 2026-10-03 | Archive now needs to survive the PARENT process dying too — ARC-04 did not exist conceptually under the old in-process model (there was nothing to "interrupt" on disk) |
| `subagent` naming throughout | `agent` naming throughout | commit `f979904`, 2026-10-04 | All CONTEXT.md and REQUIREMENTS.md language already uses the post-rename terms; double-check no planning artifact still says `subagent_registry.rs` (it's `agent_registry.rs` now) |
| report.md Summary overwritten by self-check turns | report.md Summary holds the child's FINAL task answer | commit `ee8c664`, 2026-10-04 | Already fixed; D-03's "body is the agent's final summary, then files changed, then open issues" ordering should be checked against the current `checklist_items`/`report_summary` implementation in `print.rs` during planning — files-changed and open-issues sections do not appear to exist yet as distinct sections |

**Deprecated/outdated:** nothing in this phase's domain is deprecated; this is net-new archival discipline on top of a very recent (same-week) runtime rewrite.

## Assumptions Log

| # | Claim | Section | Risk if Wrong |
|---|-------|---------|---------------|
| ~~A1~~ | RESOLVED (no longer assumed): confirmed by direct read that `src/tool/file_state.rs::atomic_write` creates a temp file in the same dir, `write_all` + `f.sync_all()` (fsync), copies the existing target's permissions onto the temp file, then `rename`s over it, removing the temp file on any error. This fully satisfies D-04 as-is. | Code Examples, Don't Hand-Roll | None — verified, not assumed. `mode/print.rs`'s `write_private` and `agent/brief.rs`'s front-matter rewrite should call this function directly instead of reimplementing a plain write. |
| A2 | `uuid::Uuid::new_v4()` (or truncated `now_v7()`) is an acceptable source for D-01's "short-uuid" suffix | Don't Hand-Roll | Low risk — any UUID truncation satisfies "short-uuid" informally; if the owner wants a specific length/format this needs confirming in planning or discuss-phase, not assumed here |
| A3 | `index.md` should be a derived/regenerable cache rather than independently hand-maintained, to avoid drift with brief.md | Common Pitfalls #1 | If the planner instead treats index.md as independently authoritative, extra reconciliation logic will be needed that this research does not design |
| A4 | D-01's `.nanopi/agents/` is project-root-scoped only (not `~/.nanopi/agents`, which Phase 1's `user_agents_dir()` already defines but which no current code path actually uses — `tool/agent.rs` reserves under `cwd.join(".nanopi").join("agents")` exclusively) | Architecture Patterns, Pattern 1 | If user-scope agents dirs are ever populated by some other path, the startup scan and prune logic designed here (project-root-only) would miss them — confirm `user_agents_dir()` is genuinely dead/unused code before scoping Phase 2 to project-root only |

**If this table is empty:** N/A — see rows above.

## Open Questions

1. **Is `waiting_permission` (D-05) reachable in the current architecture?**
   - What we know: RT-07 says "Permissions are decided by the orchestrator at dispatch time... A child never prompts; anything outside the list is denied in-band." The child-process runtime (Phase 1) appears to have fully eliminated any in-flight permission-prompt state.
   - What's unclear: Whether `waiting_permission` is a vestige of the pre-Phase-1 CONTEXT.md decisions (written before the "Revision 2026-10-03" note that changed the architecture) and should be dropped from the state machine, or whether some other legitimate "paused, needs attention" state will exist later (e.g., Phase 4's CTL-03 stop/continue).
   - Recommendation: Flag this for the discuss-phase/planner to confirm with the owner — likely the state machine should be `queued → running → done | failed | stopped | limit_reached`, plus `interrupted`, with `waiting_permission` either dropped or redefined for a future phase (CTL).

2. ~~Does `src/tool/file_state.rs::atomic_write` already fsync?~~ **RESOLVED during this research pass.** Confirmed by direct read (`src/tool/file_state.rs:122-151`): yes — temp file in the same dir, `write_all`, `f.sync_all()`, permission-copy from the existing target, then `rename`, with temp-file cleanup on any error path. D-04 is satisfied by calling this existing function; no new fsync logic needs to be written. The only remaining task is to make `mode/print.rs`'s report.md write (currently `write_private`, a plain truncate+write) and `agent/brief.rs`'s front-matter rewrite both call `file_state::atomic_write` instead.

3. **Where should `turns`/`tokens`/`worktree`/`branch` for report.md's front-matter (D-03) come from?**
   - What we know: turn/token tracking almost certainly exists somewhere in `Agent`'s loop (Phase 1 added `TurnLimits` and `last_limit_hit()`), so turns-used and tokens-used counters likely already exist in some form.
   - What's unclear: Whether a running total (not just the limit-hit check) is exposed anywhere the child's print-mode report-writer can read it. Worktree/branch (ISO-01) is explicitly Phase 4 — so Phase 2's report.md front-matter should probably render `worktree: (none)` / `branch: (none)` unconditionally for now, OR the field should simply be omitted until Phase 4, which the planner must decide since D-03 was written assuming ISO-01 already existed.
   - Recommendation: Planner should grep for turn/token counters in `src/agent/loop_.rs` and decide whether to expose them to `print.rs`'s report-writing code, and should treat the worktree/branch field as "omit or placeholder until Phase 4" rather than inventing worktree detection early.

## Environment Availability

Skip — no external tools/services beyond the Rust toolchain already used throughout this project. `cargo build`/`cargo test` are confirmed working per every Phase 1 SUMMARY.md.

## Validation Architecture

### Test Framework
| Property | Value |
|----------|-------|
| Framework | Rust built-in `#[test]` / `#[tokio::test]`, via `cargo test` |
| Config file | none — standard `cargo test`, workspace is a single crate (`Cargo.toml` at repo root) |
| Quick run command | `cargo test --lib agent_registry:: ` / `cargo test --lib agent::brief::` (targeted to touched modules) |
| Full suite command | `cargo test --lib` (848+ lib tests as of Phase 1) plus `cargo test --test subagent_runtime` / `tests/print_mode_e2e.rs` for end-to-end child-process behavior |

### Phase Requirements → Test Map
| Req ID | Behavior | Test Type | Automated Command | File Exists? |
|--------|----------|-----------|---------------------|-------------|
| ARC-01 | brief.md front-matter present at creation; amendment appended with timestamp heading | unit | `cargo test --lib agent::brief::` | ✅ existing file, needs new test cases |
| ARC-02 | report.md on disk before parent sees result, for every terminal status (completed/failed/stopped/limit_reached) | integration (real child process) | `cargo test --test subagent_runtime sc1_single_parallel_chain` (extend) or a new `sc_report_before_result` | ✅ `tests/subagent_runtime.rs` exists, extend it |
| ARC-03 | `.gitignore` gets `.nanopi/agents/` appended once, idempotently, only in a git repo; grep/find under `all=true` still can't see `.nanopi/agents/*` | unit (gitignore helper) + existing-style grep/find test (`tests the ignore list agree on...`) | `cargo test --lib tool::grep:: ` / a new `paths::` or `archive::` test module | ❌ new tests needed |
| ARC-04 | A run directory left non-terminal is rewritten `interrupted` on next startup, and is never re-dispatched | integration | new test in `tests/subagent_runtime.rs` or a new `tests/agent_archive.rs`, simulating a leftover brief.md with `state: running`, then invoking the startup scan function directly | ❌ new test file likely needed |
| ARC-05 | `/agents clean` and `/agents clean --older N` remove the right directories and report count/size | unit (registry-level deletion logic) + a TUI dispatch-level test matching the existing `slash_command_while_streaming_resolves_as_a_command`-style tests in `mode/tui.rs` | `cargo test --lib mode::tui::` | ❌ new tests needed |

### Sampling Rate
- **Per task commit:** targeted module test (e.g. `cargo test --lib agent_registry::`)
- **Per wave merge:** `cargo test --lib` (full lib suite)
- **Phase gate:** `cargo test --lib && cargo test --test subagent_runtime && cargo test --test print_mode_e2e` green before `/gsd:verify-work`

### Wave 0 Gaps
- [ ] A new `tests/agent_archive.rs` (or extension of `tests/subagent_runtime.rs`) covering ARC-04's startup-scan and ARC-05's `/agents clean` behavior end-to-end against real directories
- [ ] Unit tests for the new git-root/`.gitignore`-append helper (framework already present, just needs the test module)
- [ ] Unit tests for the path-based (not name-based) grep/find exclusion of the resolved agents root under `all=true`

## Security Domain

> `security_enforcement` not found set to `false` in `.planning/config.json` as of this research pass (not inspected directly — treat as enabled per the default-enabled rule) — included per protocol.

### Applicable ASVS Categories

| ASVS Category | Applies | Standard Control |
|---------------|---------|-------------------|
| V2 Authentication | no | N/A — single-user local CLI, no auth surface touched by this phase |
| V3 Session Management | no | N/A — this is about the agent archive, not the chat session transcript format (unchanged) |
| V4 Access Control | yes | Archive directories already created at `0700` (owner-only) and brief/report files at `0600` (Phase 1 precedent, `agent/brief.rs::append_amendment`) — Phase 2 must keep this discipline for every new file (index.md) and every rewrite (front-matter updates must preserve mode, not silently reset to default via a careless `OpenOptions`) |
| V5 Input Validation | yes | Front-matter parsing (no YAML) must not be vulnerable to the same amendment-marker-injection class of bug Phase 1 already fixed (WR-02: "every interpolated field is escaped, not just the task") — any new front-matter field interpolated from user/model-controlled text (e.g. `role`, `task`) must go through the same `escape_body`-style treatment so a crafted task string can't forge a fake `state: done` line |
| V6 Cryptography | no | No crypto in this phase |

### Known Threat Patterns for this stack

| Pattern | STRIDE | Standard Mitigation |
|---------|--------|----------------------|
| A crafted task/role string injecting a fake front-matter `state:` line to make an interrupted/failed agent falsely report `done` | Tampering | Apply the existing `escape_body` escaping (already used for amendment markers in `brief.rs`) to any front-matter value that interpolates user/model-controlled text; parse front-matter only from the designated block (before the first `---`/marker), never from the free-form body |
| A world-readable `index.md` or front-matter leaking task content from another user on a shared machine | Information Disclosure | Keep the existing `0700` dir / `0600` file discipline for every new file this phase introduces (index.md included — it currently does not exist, so there's no precedent to accidentally regress) |
| `.gitignore` append path escaping the intended project root (e.g. writing into `/` or a parent directory due to a bad git-root walk-up) | Tampering / Elevation of Privilege | Bound the walk-up to a small max depth (mirror `MAX_DEPTH` conventions already used in `grep.rs`/`find.rs`) and only ever append, never truncate/overwrite, an existing `.gitignore` |

## Sources

### Primary (HIGH confidence — direct codebase reads in this session)
- `src/agent_registry.rs` — current `AgentRegistry`, `AgentState`, `ChildGuard`, run-id generation
- `src/agent/brief.rs` — current `render_brief`, `append_amendment`, `parse_amendments_with`, `render_report`
- `src/mode/print.rs` — current report.md write path (`write_private`, `report_path_for`, `checklist_items`)
- `src/tool/agent.rs` — current dispatch flow, brief write, report read-back, state mapping
- `src/tool/grep.rs`, `src/tool/find.rs` — current `IGNORE_DIRS` and dotfile-skip logic, confirmed bypassed under `all=true`
- `src/paths.rs` — current `user_agents_dir`, `project_skills_dir` (pattern to mirror), confirmed no existing git-root helper
- `src/config.rs` — current `AgentConfig` struct and defaults
- `src/mode/tui.rs` — current slash-command registration pattern (`slash_items`, `dispatch_slash`, `SlashCmd`)
- `Cargo.toml` — confirmed `uuid 1.10` (v7, std features) and `chrono 0.4` (clock feature) already present, no new crate needed
- `.planning/phases/02-archive-lifecycle/02-CONTEXT.md` — locked decisions D-01..D-10
- `.planning/REQUIREMENTS.md` — ARC-01..05 definitions, traceability table, out-of-scope list
- `.planning/phases/01-child-process-runtime/01-01..07-SUMMARY.md` — what Phase 1 actually shipped (ISO-03 atomic writes, TurnLimits, brief/amendment model, registry, print-mode CLI surface, brief-driven child, shutdown kill paths)
- `.planning/STATE.md` — confirms Phase 1 complete, subagent→agent rename (`f979904`), report.md Summary fix (`ee8c664`)

### Secondary (MEDIUM confidence)
- None — this research required no external web lookups; the entire domain is internal codebase conventions.

### Tertiary (LOW confidence)
- None.

## Metadata

**Confidence breakdown:**
- Standard stack: HIGH — no new dependency decisions; `chrono`/`uuid` already linked and verified in `Cargo.toml`
- Architecture: HIGH — directly read every file this phase touches; gaps identified by diffing CONTEXT.md decisions against actual code
- Pitfalls: HIGH — inferred from Phase 1's own documented pitfalls (atomic-write history, escape_body precedent) plus direct reading of the current grep/find `all` bypass; the fsync behavior of `atomic_write` was directly confirmed by reading `src/tool/file_state.rs`, closing the one gap this research initially left open

**Research date:** 2026-10-04
**Valid until:** This codebase is moving fast (multiple commits per day); treat this research as valid for ~3-5 days or until another Phase 1/2-adjacent commit lands, whichever is sooner.
