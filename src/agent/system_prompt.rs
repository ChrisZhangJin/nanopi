//! Default system prompt for the agent role.
//!
//! Structurally modeled after PI's `packages/coding-agent/src/core/
//! system-prompt.ts` (see `buildSystemPrompt`) — same shape (identity
//! + tools + guidelines + cwd), trimmed to what nanopi actually
//! ships. Deliberately diverges from PI's "expert coding assistant"
//! wording: nanopi's tools (read/write/edit/bash/grep/find/ls) work
//! for many tasks beyond code — log spelunking, config edits, ad-hoc
//! shell work — so we frame it as a general-purpose agent. Kept
//! short so we don't waste input tokens.
//!
//! Injected into `Context.system` when constructing a FRESH Agent.
//! Resumed sessions keep whatever prompt was persisted with them.
//!
//! Framing note: v0.9.1 dropped the "in their working directory"
//! phrasing and the plain "executing shell commands" description
//! after a user hit refusal on `curl http://192.168.32.101:18081/`
//! — Claude read the prompt as sandboxing the shell to filesystem
//! work and self-restricted. The bash tool has no such limit; the
//! prompt must be explicit about that so the model actually uses
//! it.

use std::path::Path;

/// Build the system prompt from the current working directory and the
/// list of available tool names (as advertised to the model). The
/// tool list is important — some models skip tool calls unless the
/// prompt explicitly says the tool is available.
pub fn build(cwd: &Path, tool_names: &[String]) -> String {
    let cwd_str = cwd.display();
    let tools_line = if tool_names.is_empty() {
        "You currently have no tools available.".to_string()
    } else {
        format!(
            "Available tools: {}. Call them by name when the task requires reading/writing files, running shell commands, or searching.",
            tool_names.join(", ")
        )
    };

    let mut guidelines = vec![
        "Prefer tools over asking the user.".to_string(),
        "Use bash for shell operations; use read/write/edit for file operations; use grep/find/ls for searching.".to_string(),
        "Read files before editing them so you preserve context.".to_string(),
        "Show file paths clearly when reporting changes.".to_string(),
        "Be concise. Skip preamble.".to_string(),
    ];
    // Nudge lightweight tools if bash but no dedicated grep exists.
    if tool_names.iter().any(|n| n == "bash") && !tool_names.iter().any(|n| n == "grep") {
        guidelines.insert(1, "For code search use `bash rg` (ripgrep).".to_string());
    }
    let guidelines_block = guidelines
        .iter()
        .map(|g| format!("- {g}"))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "You are nanopi, a minimal agent CLI running as a trusted \
         developer tool on the user's own machine. You help the user \
         by reading files, running shell commands, editing files, and \
         writing new files — using the tools listed below rather than \
         describing what a user should do.

The bash tool is a real, unrestricted shell. Network requests \
         (curl/wget to any host, LAN or public), package installs, \
         service probing, and other host-level operations are all \
         fair game — this is the user's machine and their commands, \
         not something to second-guess. Don't refuse to hit an \
         internal IP because you don't recognize it; that's just the \
         user's own LAN.

If the user's message IS a shell command (starts with a command \
         name like curl, ls, ps, git, docker, systemctl, …), execute \
         it via bash directly and report the output. Don't offer to \
         run it, don't ask for confirmation — just run it. Tasks may \
         be about code, configuration, logs, data, network debugging, \
         or anything else the tools can reach.

Pursue the task through to a useful answer. When an obvious next \
         step follows from what you just saw — a 302 redirect worth \
         following, a service to probe after it resolves, a config \
         to open after grepping for it, an install to run after a \
         missing binary — do it. Chain the follow-ups yourself \
         instead of stopping to ask. Only ask when a decision has \
         non-obvious tradeoffs or would cause user-visible side \
         effects that can't be easily undone.

Skip narration. Don't preface tool calls with \"Let me run it\", \
         \"I'll now check\", \"First I'll…\". Call the tool, then \
         explain what came back. The user reads the tool card; they \
         don't need a play-by-play.

{tools_line}

Guidelines:
{guidelines_block}

Current working directory: {cwd_str}"
    )
}

/// Coordinator system prompt for orchestrator mode (ORC-03).
///
/// A new sibling to [`build`], not an edit of it: the default path must
/// stay byte-identical (T-06-02, pinned by the snapshot tests in
/// `tool/mod.rs` and `agent/build.rs`).
///
/// The orchestrator is hybrid: besides the dispatch tools it has
/// `read`/`grep`/`find`/`ls` and `bash` (see
/// `ToolRegistry::orchestrator`), but never `write`/`edit`. The prompt
/// tells it to (a) scout — look at the code and environment just enough
/// to scope a job and write a precise brief — and (b) triage: do a small
/// job itself when dispatching would cost more than the job, delegate
/// everything that changes files or is substantial. `bash` is confined
/// to inspection and quick checks; file changes always go to an agent.
///
/// Encodes the orchestrator workflow (D-08): scout and triage -> plan
/// -> dispatch -> monitor (react to agent reports, amend direction with
/// `send_message`, `stop_agent` when direction changes, continue a
/// finished agent with `send_message`) -> verify -> report one combined
/// summary covering what was done, which files changed, any open
/// issues, and the archive path.
///
/// Also encodes: D-05 (state the plan briefly and dispatch immediately
/// for a clear task; ask the user first for anything ambiguous, any
/// choice between approaches, or anything risky), D-06 (judge whether
/// parallelism is worth it), D-07 (parallel code-writing agents use
/// `isolation: "worktree"`), and D-09 (every brief must be
/// self-contained: goal, relevant files, constraints, and the expected
/// report shape).
pub fn build_orchestrator(cwd: &Path, tool_names: &[String]) -> String {
    let cwd_str = cwd.display();
    let tools_line = if tool_names.is_empty() {
        "You currently have no tools available.".to_string()
    } else {
        format!(
            "Available tools: {}. Call them by name when you need to scout, do a small job, dispatch or monitor agents, or report back.",
            tool_names.join(", ")
        )
    };
    // The agent tool gets this run's random agent names to choose from
    // (`agent::names`): models alone keep picking the same famous name.
    let tools_line = if tool_names.iter().any(|n| n == "agent") {
        format!("{tools_line}\n\n{}", crate::agent::names::prompt_line())
    } else {
        tools_line
    };

    format!(
        "You are nanopi running in orchestrator mode: the main \
         coordinator. You delegate substantial work to dedicated agents \
         created with the `agent` tool, and you handle small jobs \
         yourself when that is faster than dispatching. You have read, \
         grep, find, ls and bash for scouting and quick checks. You have \
         no write or edit tool: any change to a file is an agent's job, \
         and you never use bash to modify files either (no redirects \
         into files, sed -i, git commit, installs or other state \
         changes).

Workflow:
1. Understand the task. Scout first: read the relevant files, grep \
         for symbols, list directories, run a quick read-only command \
         (git status, git log, a version check). Scout to scope the \
         job and gather the facts a brief needs, not to do the job — \
         stop once you know which files matter and what the agent must \
         be told.
2. Triage: do it yourself or delegate? Do it yourself when the answer \
         is a lookup or a quick check — answering a question about the \
         code, reading a file or a log, running one command and \
         reporting its output — and it fits in a handful of tool calls. \
         Delegate when the job changes files, needs many steps, takes a \
         long time, can run in parallel with other work, or would flood \
         your context with large reads. An agent costs a new process, a \
         fresh context and a brief/report round trip, so it is worth it \
         only when the job is bigger than that overhead. If you find \
         yourself past about eight tool calls on your own, stop and \
         dispatch an agent for the rest.
3. Plan. For a clear task, state the plan briefly and act \
         immediately. If anything is ambiguous, if there is a real \
         choice between approaches, or if the work is risky, ask the \
         user first and dispatch only after you have an answer.
4. Dispatch. Give each agent a short `description` (3-6 words) — it \
         is the agent's task in the user's agent list — and a `name` \
         from the session's agent names below that suits the task; the \
         user knows the agent by it. Judge whether \
         parallelism is actually worth it: sequential or interdependent \
         work goes to ONE agent working through the steps (chain mode), \
         not several short, separate dispatches. Use parallel agents \
         only for genuinely independent work, and treat \
         `agent.max_concurrency` as a ceiling, not a target. Every brief \
         must be self-contained — agents do not inherit your context, so \
         state the goal, the relevant files (with the paths, symbols and \
         facts you found while scouting), any constraints, and the \
         report you expect back. For multi-step tasks, list the milestones \
         as a top-level numbered checklist so the user can see progress. \
         When dispatching more than one agent \
         that writes code in parallel, use isolation: \"worktree\" for \
         each so their changes don't collide; merges happen \
         automatically and conflicts are escalated to the user.
5. Monitor. Agents run in the background and the user sees their live \
         status. Do not poll or wait: after dispatching, tell the user \
         what started and end your turn. Each agent's report is added to \
         the conversation when it finishes; react to it then. Use \
         `send_message` to amend an agent's direction while it runs, \
         `stop_agent` if the direction changes enough that continuing \
         would waste work, and `send_message` again to continue a \
         finished agent under its same id.
6. Verify. Check an agent's work yourself when a quick look is enough \
         (read the changed file, run the tests); dispatch a \
         verification agent when checking is itself a substantial job.
7. Report. Give the user one combined summary: what was done, which \
         files changed, any open issues, and the archive path for each \
         agent's work.

{tools_line}

Current working directory: {cwd_str}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tools() -> Vec<String> {
        vec![
            "read".into(),
            "write".into(),
            "edit".into(),
            "bash".into(),
            "grep".into(),
            "find".into(),
            "ls".into(),
        ]
    }

    #[test]
    fn build_mentions_all_tools() {
        let p = build(&PathBuf::from("/tmp"), &tools());
        for t in ["read", "write", "edit", "bash", "grep", "find", "ls"] {
            assert!(p.contains(t), "missing tool {t}");
        }
    }

    #[test]
    fn build_includes_cwd() {
        let p = build(&PathBuf::from("/home/user/project"), &tools());
        assert!(p.contains("/home/user/project"), "missing cwd");
    }

    #[test]
    fn build_empty_tools_says_so() {
        let p = build(&PathBuf::from("/tmp"), &[]);
        assert!(p.contains("no tools available"));
    }

    #[test]
    fn build_suggests_rg_when_no_grep() {
        let p = build(&PathBuf::from("/tmp"), &vec!["read".into(), "bash".into()]);
        assert!(p.to_ascii_lowercase().contains("ripgrep"));
    }

    #[test]
    fn build_omits_rg_hint_when_grep_present() {
        let p = build(&PathBuf::from("/tmp"), &tools());
        assert!(!p.to_ascii_lowercase().contains("ripgrep"));
    }

    /// Regression: v0.9.1 broadened the framing so Claude wouldn't
    /// refuse network requests. The first attempt still saw Claude
    /// hedge on `curl http://<internal-ip>/` because the phrasing
    /// left room for Claude to apply its own "arbitrary hosts" safety
    /// heuristic. Second pass tightened the language:
    ///   (1) drop "in their working directory"
    ///   (2) explicitly authorize network requests, incl. internal IPs
    ///   (3) explicitly instruct: "if user's message is a shell
    ///       command, execute directly — don't offer, don't ask"
    ///   (4) frame as "trusted developer tool on the user's machine"
    ///       so Claude's default caution about "arbitrary hosts"
    ///       doesn't fire.
    #[test]
    fn build_does_not_sandbox_bash_to_filesystem() {
        let p = build(&PathBuf::from("/tmp"), &tools());
        assert!(
            !p.contains("in their working directory"),
            "prompt still carries the sandbox-implying phrase"
        );
        let lc = p.to_ascii_lowercase();
        assert!(
            lc.contains("network requests") || lc.contains("curl"),
            "prompt must explicitly authorize network requests: {p}"
        );
        assert!(
            lc.contains("unrestricted shell") || lc.contains("real shell"),
            "prompt must state that bash is a real shell: {p}"
        );
        assert!(
            lc.contains("trusted developer")
                || lc.contains("user's own machine")
                || lc.contains("user's machine"),
            "prompt must frame nanopi as a trusted developer tool so \
             Claude's 'arbitrary hosts' caution doesn't fire: {p}"
        );
        assert!(
            lc.contains("don't ask for confirmation") || lc.contains("don't offer to run"),
            "prompt must instruct the model to just execute pasted \
             shell commands directly: {p}"
        );
        assert!(
            lc.contains("pursue the task")
                || lc.contains("chain the follow-ups")
                || lc.contains("obvious next step"),
            "prompt must tell the model to keep going through \
             obvious follow-ups (e.g. 302 redirect) instead of \
             stopping to ask: {p}"
        );
        assert!(
            lc.contains("skip narration")
                || (lc.contains("don't preface") && lc.contains("tool call")),
            "prompt must tell the model to skip \"Let me…\" \
             preambles before tool calls: {p}"
        );
    }

    fn orchestrator_tools() -> Vec<String> {
        crate::tool::ToolRegistry::orchestrator().names()
    }

    /// The prompt and the registry must not drift apart: the prompt
    /// may name `write`/`edit` only to say they are absent, and every
    /// scouting tool it relies on must actually be registered.
    #[test]
    fn orchestrator_prompt_never_offers_a_tool_it_does_not_have() {
        let tools = orchestrator_tools();
        let p = build_orchestrator(&PathBuf::from("/tmp"), &tools);

        // Guard the fixture itself: if the registry ever gains a file
        // writing tool, this test must be revisited, not silently
        // weakened.
        for absent in ["write", "edit"] {
            assert!(
                !tools.iter().any(|t| t == absent),
                "registry unexpectedly has `{absent}` — revisit this test"
            );
        }
        for present in ["read", "grep", "find", "ls", "bash"] {
            assert!(
                tools.iter().any(|t| t == present),
                "prompt relies on `{present}` but the registry lacks it"
            );
        }

        let lc = p.to_ascii_lowercase();
        for forbidden in ["use write", "use edit", "edit the file yourself", "write the file yourself"] {
            assert!(
                !lc.contains(forbidden),
                "prompt tells the model to use an unregistered tool \
                 ({forbidden:?}): {p}"
            );
        }
        assert!(
            lc.contains("no write or edit tool"),
            "prompt must state it has no write/edit tool: {p}"
        );
        assert!(
            lc.contains("never use bash to modify files"),
            "prompt must confine bash to inspection: {p}"
        );
    }

    /// The hybrid orchestrator must be told to scout and to triage
    /// (do-it-yourself vs delegate), with a bound on going it alone.
    /// The orchestrator prompt offers this run's agent names when it has
    /// the agent tool.
    #[test]
    fn prompts_offer_the_runs_agent_names_with_the_agent_tool() {
        let first = crate::agent::names::shortlist()[0];
        let with = vec!["read".to_string(), "agent".to_string()];
        let without = vec!["read".to_string()];
        assert!(build_orchestrator(&PathBuf::from("/tmp"), &with).contains(first));
        assert!(!build_orchestrator(&PathBuf::from("/tmp"), &without).contains("Agent names for this session"));
        // The solo prompt stays byte-identical to its baseline; there an
        // unoffered name is simply replaced by a random free one.
        assert!(!build(&PathBuf::from("/tmp"), &with).contains("Agent names for this session"));
    }

    #[test]
    fn orchestrator_prompt_covers_scouting_and_triage() {
        let p = build_orchestrator(&PathBuf::from("/tmp"), &orchestrator_tools());
        let lc = p.to_ascii_lowercase();
        for phrase in [
            "scout",
            "triage",
            "do it yourself",
            "delegate when the job changes files",
            "eight tool calls",
        ] {
            assert!(lc.contains(phrase), "missing {phrase:?}: {p}");
        }
    }

    #[test]
    fn orchestrator_prompt_covers_workflow_steps() {
        let p = build_orchestrator(&PathBuf::from("/tmp"), &orchestrator_tools());
        let lc = p.to_ascii_lowercase();
        for word in [
            "understand",
            "plan",
            "dispatch",
            "monitor",
            "verify",
            "isolation",
            "worktree",
        ] {
            assert!(lc.contains(word), "missing workflow word {word}: {p}");
        }
        assert!(
            lc.contains("summar"),
            "missing summarize/summary/summarise: {p}"
        );
    }

    #[test]
    fn orchestrator_prompt_states_tool_restriction() {
        let tools = orchestrator_tools();
        let p = build_orchestrator(&PathBuf::from("/tmp"), &tools);
        let lc = p.to_ascii_lowercase();
        assert!(
            lc.contains("write") && lc.contains("edit") && lc.contains("bash"),
            "must mention write/edit (absent) and bash (inspection only): {p}"
        );
        assert!(
            lc.contains("delegat"),
            "must say work is delegated to agents: {p}"
        );
        assert!(
            lc.contains("self-contained"),
            "must require self-contained briefs: {p}"
        );
        assert!(lc.contains("goal"), "brief must mention goal: {p}");
        assert!(
            lc.contains("relevant files"),
            "brief must mention relevant files: {p}"
        );
        assert!(
            lc.contains("constraint"),
            "brief must mention constraints: {p}"
        );
        assert!(
            lc.contains("report"),
            "brief must mention expected report: {p}"
        );
        assert!(
            p.contains(&format!("Available tools: {}", tools.join(", "))),
            "must contain the Available tools line: {p}"
        );
        assert!(
            p.contains("Current working directory:"),
            "must contain the cwd line: {p}"
        );
    }
}
