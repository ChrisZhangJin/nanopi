# Agent memory for nanopi — design (agreed, not yet implemented)

Background: <https://www.anthropic.com/research/components-of-agent-memory>.
Decisions Q1–Q42 from the design discussion of 2026-11-23.

## Purpose

**Experience reuse** across sessions, for one person, without being told
again (Q1):

1. **Resources** — "the test server we used two days ago": found again
   with its purpose and where it applies.
2. **Troubleshooting** — a hard problem solved once: the next similar
   symptom recalls the diagnosis, the fix that worked, how it was
   verified, and which attempts to skip.

Not a copy of the repository, not a diary of every turn.

## Principles

- **Memory is a lead, not authority** (Q2). Current instructions and the
  current state win; remembered operational facts are re-checked before
  use. A remembered approval never authorizes a push, release, deletion,
  connection or sudo command.
- **Hybrid capture** (Q3): "remember this" saves directly; nanopi also
  notices durable lessons itself.
- **Bounded cost** (Q4): maintenance is visible, budgeted and stoppable.
  Success = fewer repeated explanations and mistakes, not more notes.
- **One store per person** (Q5, Q6): not split by project or profile.
  Project is a tag and a matching signal, not a boundary; Dream looks
  across projects to find what they share.
- **Methods travel, resources carry their scope** (Q15): a troubleshooting
  method can be recalled in any project; a machine, account or
  environment is recorded with where it applies and when it was last
  verified. Elsewhere it is only a candidate. Finding it is not
  permission to use it.

## Layers

| Layer | What | Who writes |
|---|---|---|
| Session transcripts | existing JSONL — ground truth | the session |
| Daily notes | working notes per day, per project/category; pointers to evidence, not copies of it (Q11) | the main session (Q22) |
| Experience library | per-topic entries distilled and kept current by Dream (Q12) | Dream, `remember` |
| Index | `MEMORY.md`, the search entry point; never injected whole (Q8) | Dream |

## Storage (Q27)

```
~/.nanopi/memory/                     dir 0700, files 0600, plain Markdown
  daily/2026-11-23/nanopi.md          daily notes, local date, by project/category
  daily/2026-11-23/general.md
  library/environments/test-box.md    distilled entries, by topic
  library/troubleshooting/docker-build-cache.md
  archive/                            archived entries, not recalled
  trash/                              forgotten entries, recoverable for 30 days
  MEMORY.md                           active library index
  .forgotten                          forget markers (ids only, no text)
```

No database, no encryption; editable and git-able by the user. Dream
changes the library, its index, and maintenance state (archive, trash,
forget markers, and run logs). It never rewrites daily notes or transcripts.

## Capture (Q9, Q10, Q22)

- One daily entry per finished piece of work with a reusable result — not
  per turn, no extra model call per turn to decide. Chat and repeated
  "all quiet" loop iterations are not recorded. Sub-agent results enter
  through the main session once it has their report.
- **May be saved automatically:** low-sensitivity, evidenced lessons.
- **Environments:** by default only an alias the user approved and its
  purpose — no host addresses, user names or sensitive paths lifted from
  logs.
- **Never:** passwords, keys, tokens, or "was allowed last time".
- **Troubleshooting entries** state: symptom, conditions where it applies,
  the step that worked, how that was verified, attempts to skip, source
  session. Anything unverified is marked as a lead, not a fix.

## Dream (Q7, Q12, Q16, Q19, Q24)

Consolidates daily notes into the library:

- dedupes and categorizes; merges repeats; keeps conflicting results
  apart with their conditions; never turns two successes into a general
  rule on its own;
- checks entries against the transcripts; on conflict marks the old
  entry **needs review** instead of overwriting it (and stops treating it
  as confirmed);
- links every entry to its dates and evidence;
- records a short change summary, including maintenance decisions and their reasons.

Once enabled and configured, Dream is unattended. The agent decides what to
keep, mark for review, pin, unpin, archive, or forget; no per-entry approval
or user follow-up is required. nanopi enforces the storage, disclosure, and
retention rules. Console commands are for inspection and optional recovery,
not steps in the maintenance workflow.

Rules:

- Only one Dream at a time (manual and scheduled never overlap). It works
  on a snapshot; entries written meanwhile wait for the next run.
- User edits and deletions are never overwritten or brought back.
- It only reorganizes memory: it never re-runs commands from transcripts
  or connects to servers to verify.
- **Backfill** (first use): an explicit date range, default the last 7
  days, confirmed by the user. **Daily runs:** only new or unprocessed
  dates. Both show their range and respect a configurable input and
  model-call budget; at the limit they save progress and continue next
  time.

## Selective forgetting (Q33–Q42)

- Maintenance applies only to distilled experiences, not daily notes or
  source transcripts. Age is a signal for review, never a deletion rule.
- By default, environment entries not verified for 90 days become **needs
  review** and are no longer presented as available resources. Other entries
  enter review after 180 days without being recalled for an actual task (or
  after 180 days in the library if never recalled). These thresholds are
  configurable. Dream scans and unselected search matches do not count as
  use; missing verification dates are marked unknown, not inferred from file
  timestamps.
- Agent decisions are based on evidence and likely future value. For example,
  a superseded entry with a verified replacement may be forgotten; a rarely
  needed recovery procedure may be kept or pinned. Archive removes an entry
  from ordinary recall without an expiry. Forget removes it from recall
  immediately and moves its body to trash for 30 days. Dream then purges the
  body automatically, without approval, and retains the ID-only forget
  marker to prevent regeneration from old records. Restore during that
  window reverses the forget marker; archived entries can also be restored.
- The agent may set or remove its own pins but cannot remove a user's pin.
  Pins protect against age- or usage-based cleanup, not against review when
  evidence conflicts or an environment's last verification becomes stale.
- The run log records the reason for each archive, forget, pin, or unpin
  decision. Reviewing that log or restoring an entry is optional; neither is
  required for maintenance to continue.

## Recall (Q8, Q20, Q23)

- At the start of a related task the model searches the library and
  recalls only a few entries, with a short note ("found the test
  environment from last time"); it then checks the current situation
  before relying on them. Unrelated tasks load nothing.
- Only the distilled library is searched in daily work, never the raw
  transcripts. Nothing found → say so and suggest `/dream`; never answer
  from an unverified raw note, never trigger a paid Dream silently.
  `remember` makes an entry searchable at once.
- Local text search over fields Dream maintains — aliases, symptoms,
  purpose, project tags — so "the box we ran tests on" finds "remote test
  environment". Two equally likely matches → ask, don't guess. Vectors
  only if the acceptance scenarios show text search falls short.

## Disclosure (Q14, Q18, Q28)

The store is shared; what reaches a model endpoint is not.

- Per profile: `memory_disclosure = "full" | "general"` (default
  `general`). `general` sends only general methods; for environment
  entries the user is told locally that one exists and is not sent.
- Asked once per profile when memory is first enabled. nanopi never
  infers trust from a URL.
- The same check applies to recall and to the excerpts Dream sends.
  Content that cannot be confirmed safe for the endpoint is not sent.

## Scheduling (Q13, Q17, Q26)

- The first interactive start (and the first start of the new version
  for existing users) asks: enable / not now / never. Not asked again
  after "never".
- Linux first: a user-level scheduled task (no sudo), default 03:00 local
  time, changeable, verified active after setup. Runs without the TUI as
  long as the machine is on. Missed runs: one catch-up run, oldest
  unprocessed date first, within budget.
- The scheduled Dream uses the model and endpoint chosen at setup, shown
  to the user. If that changes or credentials fail, it records why and
  pauses — it never falls back to another endpoint.
- Where unattended runs cannot be guaranteed: say so, keep manual
  `/dream`; no silent fallback to an in-TUI loop.
- Status shows last result, next run time and backlog; pause and remove
  are available.

## Interfaces (Q29, Q30, Q31)

- **Model tools:** `memory_search`, `remember` (follows the capture
  rules), `forget` (on the user's request). No general write tool for the
  library.
- **User commands:** `/memory` shows the index and recent additions;
  `/memory show <id>` shows an entry's status and sources;
  `/memory forget <id>` forgets an entry; `/memory restore <id>` restores
  an archived entry or one still in its recovery window.
- **Dream commands:** `/dream` runs maintenance manually (`--since 7d`
  for backfill). `/dream status` shows the last result, next scheduled run,
  backlog, and any failure reason. `/dream log` shows what changed and why.
  Inspection and recovery are optional, not approval steps.
- **Prompt:** a few lines on what memory is for, plus "search it before a
  related task" — no rules on how to use it.
- **Sub-agents:** `memory_search` only; their lessons go in their report.
- **Forget vs purge** (Q25): forgetting affects the distilled entry only
  and leaves an ID-only marker so Dream cannot regenerate it. Purging
  source text is a separate, user-initiated operation that lists the
  affected daily notes and sessions and asks first.
- `AGENTS.md` / `CLAUDE.md` stay user-written; Dream only suggests
  additions in its summary. `examples/wasm-plugin-memory` stays as an
  example; the README points to the built-in feature.

## Delivery (Q21, Q32)

**Phase 1:** daily notes, `remember` / `forget` / `memory_search`, manual
`/dream` with backfill, `/memory`, disclosure setting, read-only sub-agents.

**Phase 2:** first-start scheduling prompt and the Linux user-level task,
backlog and budget handling, Dream change summary.

**Acceptance** (each phase), in fresh sessions:

1. *Resource reuse* — "run the tests remotely like two days ago", no
   server named: nanopi finds the saved alias, purpose, scope and source;
   it does not connect on that basis alone.
2. *Troubleshooting reuse* — a similar symptom described, no hint that it
   was fixed before: nanopi recalls the verified diagnosis, conditions and
   skipped attempts, and checks that they apply before following them.
3. *Negative* — an unrelated task loads neither entry.
4. *Negative* — on an endpoint with `general` disclosure, the environment
   entry is not sent.
