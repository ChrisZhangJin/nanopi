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
    if tool_names.iter().any(|n| n == "bash")
        && !tool_names.iter().any(|n| n == "grep")
    {
        guidelines.insert(1, "For code search use `bash rg` (ripgrep).".to_string());
    }
    let guidelines_block = guidelines
        .iter()
        .map(|g| format!("- {g}"))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "You are nanopi, a minimal agent CLI. You help the user accomplish \
         tasks in their working directory — reading files, executing shell \
         commands, editing files, and writing new files — using the tools \
         listed below rather than describing what a user should do. Tasks \
         may be about code, configuration, logs, data, or anything else \
         the tools can reach.

{tools_line}

Guidelines:
{guidelines_block}

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
        let p = build(
            &PathBuf::from("/tmp"),
            &vec!["read".into(), "bash".into()],
        );
        assert!(p.to_ascii_lowercase().contains("ripgrep"));
    }

    #[test]
    fn build_omits_rg_hint_when_grep_present() {
        let p = build(&PathBuf::from("/tmp"), &tools());
        assert!(!p.to_ascii_lowercase().contains("ripgrep"));
    }
}
