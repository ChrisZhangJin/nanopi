---
project: nanopi
created: 2026-08-19
status: active
---

# nanopi Roadmap

nanopi is a tiny Rust port of the Pi coding-agent CLI — a ~4 MB static binary that runs on old / low-resource Linux boxes. Development is well underway (v0.9.3 shipped); GSD tracking is being retrofitted onto an existing codebase and this roadmap captures ongoing work rather than a greenfield plan.

## Milestones

### M1 · UX Polish (in progress)

Improve first-time and everyday console UX so nanopi is usable without hand-editing TOML.

**Quick tasks (see STATE.md for the running log):**

- First-run wizard for config bootstrap.
- (future work — added as it comes up)

### M2 · Extensions (v0.11.0) — complete

Pi-parity extension capabilities. See `docs/pi-vs-nanopi.md` for the
comparison that scoped this. All four phases shipped on `v0.11.0`.

- **P0** ✅ Shell-hook event coverage extended to `before_agent_start`,
  `turn_start`, `turn_end`, `message_end` (`410d12c`). Later joined by
  `session_before_compact` / `session_compact` (`675cd02`).

- **P1** ✅ `post_tool_use` can transform the tool result, not just
  observe it (`a2e3994`) — enables redaction / scrubbing hooks.

- **P2** ✅ WASM plugin system behind `--features wasm`
  (`667bf30`, `d4aff4b`, `82d36d7`). Components declared in
  `[[extensions]]` are compiled, instantiated, and their exported
  tools registered alongside the built-ins.

- **P3** ✅ `steer` / `follow-up` injection (`1767238`), wired to the
  TUI so mid-stream typing steers the running turn (`d45bb42`) and
  queued follow-ups auto-start the next one (`7064b10`).

Also landed in this milestone:

- `tool_exec_mode` (parallel vs sequential tool execution). Pi has a
  per-tool override too, which is deferred.

- **Capability-gated host functions** — `host-fs-read` (`788705c`,
  read-only, cwd-confined, symlink-aware) and `host-http-get`
  (`83bbe68`…`28c2e75`, gated on `allow_network` then a deny-by-default
  host-matching `url_allowlist`; 10 s timeout, 1 MiB cap, redirects not
  followed). Both return in-band `error: ` strings rather than
  trapping. Declared at `wit/nanopi-extension.wit:36,57`, implemented
  at `src/wasm/loader.rs:447,478`.
  *(This was listed as deferred until 2026-09-01; it had in fact
  shipped on 2026-08-28.)*

- A hardening pass over the whole milestone — ~20 `fix(...)` commits
  covering plugin epoch deadlines, trap isolation, allowlist bypass,
  cwd-guard escapes, cancel-safety of parallel tool batches, and
  session-file corruption on cancel.

- VERSION centralization (`VERSION` + `make bump` + a tag-vs-VERSION
  gate in `release.yml`), per `.planning/PLAN-VERSION.md` — that plan
  is **done**, not pending.

- **Plugin slash-command registration** (`4df493b`…`e9ce962`,
  2026-09-02) — the last plugin capability. A component may export
  `list-commands` / `execute-command` from a second WIT world,
  `extension-commands`, which `include`s the first so tool-only
  plugins keep building unchanged. A command returns an action rather
  than calling back into the host, keeping the import list at three.
  Pi's dispatch shape was followed
  (`.planning/reference/pi-slash-commands.md`, since verified against
  Pi's source); its collision rules deliberately were not — nanopi
  refuses rather than renaming. Shipped alongside two adjacent fixes:
  plugins now load on resumed sessions, and a leading space no longer
  routes a slash command to the model as chat text.

**Deferred to a later milestone** (from the parity review) — **all but
one shipped in v0.12.0, 2026-09-07**:

- ~~Per-tool `executionMode` override.~~ ✅ `003399f`. Not just
  configurability: `bash` now declares itself Sequential, which fixed
  the concurrent-bash data loss that had been sitting `#[ignore]`d as a
  known bug. `[tool_exec_overrides]` takes the speed back.

- ~~**Plugin hot reload.**~~ ✅ `34866aa`…`0a2f10c`. Both prerequisites
  named here were built: `ToolRegistry::unregister_plugin` (keyed on the
  plugin, never a tool name, so removing a built-in is unwritable) and
  per-plugin instance ids. A call in flight when the swap lands is
  refused in-band, with different wording depending on whether it had
  already entered the guest — one says the result is discarded, the
  other says side effects already stand.

- ~~Richer session metadata.~~ ✅ `483aec8`, and it was three things:
  **labels were already done** (`/name`); **`ModelChange` was a bug, not
  a feature** — reader, replay, `/export` and a roundtrip test all
  existed with NO WRITER since the session format did; **thinking-level
  changes** were genuinely missing and are now `ThinkingChange`.
  *Custom entries deliberately NOT built* — under-specified, and a
  plugin-written entry runs into invariant 15. Needs a decision about
  who writes and who reads.

- **Provider registration from plugins** — still deferred, and moved to
  `docs/BACKLOG.md` with the full argument. Short version: it is a new
  ABI shape (streaming inverts the guest-calls-host flow every one of
  the nine imports uses), it needs a `Provider` trait signature change,
  and it requires deciding whether a plugin provider is exempt from
  `url_allowlist`. **That last one is a decision for the project owner
  and blocks the other two.**

- Session-management hooks beyond compaction — untouched, no demand yet.

### M3 · Bugfix Line (v0.10.1) — shipped 2026-09-01

Patch line on top of v0.10.0. Lives on the `v0.10.1` branch; nothing
new is developed there. **Released:** tag `v0.10.1` == `d19d3c2`, also
fast-forwarded into `main`, four platform assets published.

Contents:

- `fix(config)` — the Windows first run died with `cannot read
  api_key_file ~/.nanopi/api_key`. The wizard now writes an absolute
  `api_key_file`, and `paths::expand_home` is the single expansion
  point shared by `main` and `agent::hook` (falls back to
  `dirs::home_dir()` when `$HOME` is unset, accepts `\`, honors
  `NANOPI_HOME`).

- `fix(vendor)` — MiniMax default base_url → `api.minimaxi.com`.
- `fix(provider)` — gateway HTML error pages flattened to one line
  (`retry::flatten_error_body`) instead of being shredded by the TUI
  redraw.

All three were cherry-picked onto `v0.11.0` on 2026-09-01
(`e9425b8`, `777eb8a`, `642f696`) — different SHAs, same patches, so
expect patch-id dedup when v0.11.0 eventually merges to `main`.

### M4 · Orchestrator & Dynamic Agents (v0.13.0) — planning

Let nanopi dispatch agents on its own and add an experimental
orchestrator mode. Phases derived from `.planning/REQUIREMENTS.md`
(39 REQ-IDs) in the order reconciled in `research/SUMMARY.md`.

| Phase | Name | Goal | Requirements | Depends on |
|-------|------|------|--------------|------------|
| 1 | Child-process runtime | 7/7 | Complete   | 2026-10-03 |
| 2 | Archive & lifecycle | 7/7 | Complete   | 2026-10-04 |
| 3 | Dynamic agents | 3/3 | Complete   | 2026-10-04 |
| 4 | Background launch & control | 6/6 | Complete   | 2026-10-04 |
| 5 | TUI agents strip | 3/3 | Complete   | 2026-10-04 |
| 6 | Orchestrator mode | 3/4 | In Progress|  |

- [x] **Phase 1: Child-process runtime** - isolated `nanopi -p` children, process tracking/kill, caps, brief-file amendments and final self-check (completed 2026-10-03)
- [x] **Phase 2: Archive & lifecycle** - brief.md / report.md, state machine, interrupted marking, cleanup (completed 2026-10-04)
- [x] **Phase 3: Dynamic agents** - optional agent name, inline role/tools/model, capped report (completed 2026-10-04)
- [x] **Phase 4: Background launch & control** - background ids, amend/stop/list/continue, report injection, print-mode drain, worktrees (completed 2026-10-04)
- [x] **Phase 5: TUI agents strip** - collapsible, display-only bottom strip with states (completed 2026-10-04)
- [ ] **Phase 6: Orchestrator mode** - `/orchestrator` toggle, restricted tools, coordinator prompt, release gates

### Phase 1: Child-process runtime

**Goal**: Agents run as isolated `nanopi -p` child processes, controlled only by the orchestrator, driven by a brief file that can be amended mid-run, and can never crash nanopi or leak into the parent session.
**Depends on**: Nothing (first phase)
**Requirements**: RT-01, RT-02, RT-03, RT-04, RT-05, RT-06, RT-07, RT-08, RT-09, ISO-03
**Success Criteria** (what must be TRUE):

  1. A single / parallel / chain `agent` call runs each agent as a `nanopi -p` child; a child that panics or is killed is reported as failed and the main process keeps running (tested with a release build).
  2. Stopping an agent, cancelling the parent turn, or exiting nanopi kills its children; no orphan `nanopi` processes remain.
  3. Each child writes its own transcript in its agent directory; the parent session contains only the tool call and its result.
  4. A child gets exactly the tools the dispatch allows, never the agent/control tools, never prompts; turn limit and token budget end it with a partial report; the global live cap is enforced.
  5. Appending an amendment to a running child's brief is applied at its next turn boundary; before finishing, the child re-reads the brief and its report lists every item as done or not done.
  6. Two agents (two processes) editing the same file: the second edit is refused because the file changed on disk since it was read.

**Plans**: 7 plans
Plans:

- [x] 01-01-PLAN.md — ISO-03 cross-process stale-write guard + atomic writes
- [x] 01-02-PLAN.md — loop turn/token limits, hook agent_id, brief text model
- [x] 01-03-PLAN.md — AgentRegistry, caps, process-group ChildGuard, [agent] config
- [x] 01-04-PLAN.md — child `-p` flags, session file, recursion strip, PDEATHSIG, envelope status
- [x] 01-05-PLAN.md — parent supervisor: brief, registry, inherited provider, failure mapping
- [x] 01-06-PLAN.md — brief watcher amendments, self-check, report.md checklist
- [x] 01-07-PLAN.md — exit-path kill_all, docs, end-to-end success-criteria tests

**Research flags**: needs research — `-p` flags for tools/limits/brief/session, process-group kill, cross-process stale-write guard.
**Superseded**: the in-process design (2026-10-03) was executed then rolled back (`2bd0343`); its plans are kept under `phases/01-child-process-runtime/superseded-inprocess/`.

### Phase 2: Archive & lifecycle

**Goal**: Every agent leaves an inspectable, loss-proof `.md` trail with a clear lifecycle state.
**Depends on**: Phase 1
**Requirements**: ARC-01, ARC-02, ARC-03, ARC-04, ARC-05
**Success Criteria** (what must be TRUE):

  1. Starting an agent creates `.nanopi/agents/<run>/<id>/brief.md` with task, role, tools and model; amendments are appended to it.
  2. `report.md` exists on disk before the parent sees the result.
  3. `.nanopi/agents/` appears in `.gitignore` and never shows up in agents' grep/glob results.
  4. After killing nanopi mid-run, the next start marks those agents `interrupted` without re-running them.
  5. `/agents clean` keeps the most recent N runs or removes all.

**Plans**: 7 plans

Plans:
**Wave 1**

- [x] 02-01-PLAN.md — brief.md/report.md front-matter, timestamped amendments
- [x] 02-02-PLAN.md — project_agents_dir + grep/find archive exclusion

**Wave 2** *(blocked on Wave 1 completion)*

- [x] 02-03-PLAN.md — src/archive.rs: run id, durable state, index.md, interrupted scan, .gitignore
- [x] 02-04-PLAN.md — durable child report.md with turns/tokens/files changed

**Wave 3** *(blocked on Wave 2 completion)*

- [x] 02-05-PLAN.md — archive_keep_days, auto-prune, clean_runs

**Wave 4** *(blocked on Wave 3 completion)*

- [x] 02-06-PLAN.md — wire registry, dispatch, fallback report, startup scan/prune

**Wave 5** *(blocked on Wave 4 completion)*

- [x] 02-07-PLAN.md — /agents clean command + integration tests

**Research flags**: standard patterns.

### Phase 3: Dynamic agents

**Goal**: The model can dispatch an agent just by describing the task, with optional ad-hoc role, tools and model.
**Depends on**: Phase 1, Phase 2
**Requirements**: DYN-01, DYN-02, DYN-03, DYN-04, DYN-05
**Success Criteria** (what must be TRUE):

  1. A `agent` call with only a task runs a general-purpose agent and returns a result.
  2. A call with an inline role prompt, toolset and model runs with exactly those (validated against allowlist/deny-list; disallowed tools rejected with a clear error).
  3. Existing predefined agent files and single / parallel / chain modes behave as in v0.12.
  4. The parent receives a capped summary, never the full child transcript.

**Plans**: 3 plans

Plans:

- [x] 03-01-PLAN.md — general-purpose AgentConfig, models::model_vendor, brief label
- [x] 03-02-PLAN.md — optional agent + inline role/tools/model/description, pre-spawn validation
- [x] 03-03-PLAN.md — 8 KB parent report cap, schema + delegation guidance, regression gate

**Research flags**: standard patterns.

### Phase 4: Background launch & control

**Goal**: The model can launch agents in the background and amend, stop, list and continue them, with reports delivered back automatically.
**Depends on**: Phase 3
**Requirements**: CTL-01, CTL-02, CTL-03, CTL-04, CTL-05, CTL-06, CTL-07, ISO-01, ISO-02
**Success Criteria** (what must be TRUE):

  1. The model launches a background agent, gets its id immediately and keeps working; `list_agents` shows its status.
  2. Amending a running agent appends to its brief and takes effect at its next turn boundary (never mid tool call); stopping it yields a partial report.
  3. A finished background report starts a new main turn when idle, or is queued as a follow-up while streaming; a finished agent can be continued by a new `-p` on its session.
  4. In `-p` mode, nanopi waits for (or stops) background agents before exiting — no orphans.
  5. A writer dispatched with worktree isolation reports its worktree path and branch; unchanged worktrees are removed, changed ones kept and listed.

**Plans**: 5 plans
Plans:

- [x] 04-01-PLAN.md — registry background tracking/stop/reactivate/outbox + `background: true` dispatch
- [x] 04-02-PLAN.md — git worktree module (create, commit, cleanup, auto-merge/conflict)
- [x] 04-03-PLAN.md — control tools: send_message (amend/continue), stop_agent, list_agents
- [x] 04-04-PLAN.md — report injection (TUI follow-up path) + print-mode drain with Ctrl-C stop_all
- [x] 04-05-PLAN.md — worktree isolation wired into dispatch

**Research flags**: needs research — report injection path and print-mode exit.

### Phase 5: TUI agents strip

**Goal**: The user can see every agent's state at a glance without leaving the conversation (display-only).
**Depends on**: Phase 4
**Requirements**: UI-01, UI-02, UI-03, UI-04
**Success Criteria** (what must be TRUE):

  1. With agents present, a 1–3 line strip above the input shows id, role, short task, state and elapsed time; it disappears when none exist.
  2. Ctrl+G (or the chosen free key) expands/collapses the strip; the expanded view shows latest activity and report path.
  3. The strip is display-only: no approve/stop/message actions (control goes through the orchestrator).
  4. The strip updates on the TUI tick from a registry snapshot, with no flicker or redraw storm under many agent events.

**Plans**: 3 plans

Plans:

- [x] 05-01-PLAN.md — agents strip model + pure renderer (src/mode/agents_strip.rs), TDD
- [x] 05-02-PLAN.md — Ctrl+G ToggleAgentsStrip keybinding + docs/manual test row
- [x] 05-03-PLAN.md — wire strip into TUI dock, tick refresh, Ctrl+G/Esc, expanded detail

**UI hint**: yes
**Research flags**: standard patterns.

### Phase 6: Orchestrator mode

**Goal**: The user can opt into an experimental mode where the main agent only plans, delegates, monitors and summarises — and the default flow is untouched.
**Depends on**: Phase 3, Phase 4, Phase 5
**Requirements**: ORC-01, ORC-02, ORC-03, ORC-04, ORC-05, QA-01, QA-02
**Success Criteria** (what must be TRUE):

  1. `/orchestrator` (or `experimental.orchestrator`) toggles the mode; it is off by default and the status line shows when it is on.
  2. In orchestrator mode the main agent has only read/grep/glob plus dispatch/amend/stop/list/continue; write, edit and bash are absent (tested).
  3. Given a multi-part task, the orchestrator splits it, dispatches agents, and presents a synthesised summary of their reports.
  4. With the mode off, prompts and tool specs are byte-identical to v0.12 (tested).
  5. The manual E2E plan has rows for amend, stop, expand, toggle and clean; the release binary grew by no more than ~150 KB with no unjustified new crates.

**Plans**: 4 plans
Plans:

- [x] 06-01-PLAN.md — orchestrator registry, coordinator prompt, mode-aware composer, config key, ORC-04 snapshots
- [x] 06-02-PLAN.md — TUI /orchestrator toggle, in-place swap, rebuild sites, status-line segment
- [x] 06-03-PLAN.md — print mode ignores key with one-line stderr note (real-binary tests)
- [ ] 06-04-PLAN.md — consolidated v0.13 manual test plan (incl. Ctrl+G), agents.md, binary size gate

**UI hint**: yes

## Notes

This project uses `/gsd:quick` for the majority of ongoing work. New planned phases (if any) will be added as milestones above.
