//! Task steps of a delegated agent, and the progress it reports on them.
//!
//! A brief whose task lists its work as numbered items (`1.` / `2)` /
//! `1、`) or checkboxes (`- [ ]`) has steps. Otherwise the child declares
//! its own plan first (`[plan N]`). The child is asked to write
//! `[progress k/N]` after finishing step k; the TUI reads the newest such
//! marker published by the worker. Progress only ever comes from the agent's
//! own report: without steps or markers there is no number to show.

/// Fewer items than this is not a plan worth a progress bar.
const MIN_STEPS: usize = 2;
/// More than this is not a step list (or a bar would be meaningless).
const MAX_STEPS: usize = 30;

/// The `## Task` section of a rendered brief (or the whole text when it
/// has no such section). The task body may carry its own `## ` headings,
/// so the section ends at the brief's own `## Role` heading (the last one:
/// `render_brief` writes it right after the task).
fn task_section(brief: &str) -> &str {
    let start = match brief.find("\n## Task\n") {
        Some(i) => i + "\n## Task\n".len(),
        None if brief.starts_with("## Task\n") => "## Task\n".len(),
        None => return brief,
    };
    let rest = &brief[start..];
    match rest.rfind("\n## Role\n") {
        Some(end) => &rest[..end],
        None => rest,
    }
}

/// The text of a top-level step item, or `None` for any other line.
fn step_item(line: &str) -> Option<&str> {
    // Top level only: nested items are details of a step.
    if line.starts_with(' ') || line.starts_with('\t') {
        return None;
    }
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if (1..=3).contains(&digits) {
        let rest = &line[digits..];
        if let Some(text) = rest
            .strip_prefix(". ")
            .or_else(|| rest.strip_prefix(") "))
            .or_else(|| rest.strip_prefix('、'))
            .or_else(|| rest.strip_prefix('．'))
        {
            return Some(text.trim()).filter(|t| !t.is_empty());
        }
        return None;
    }
    for prefix in ["- [ ] ", "* [ ] ", "- [x] ", "- [X] ", "* [x] ", "* [X] "] {
        if let Some(text) = line.strip_prefix(prefix) {
            return Some(text.trim()).filter(|t| !t.is_empty());
        }
    }
    None
}

/// The steps in a brief's task, in order. Empty when the task does not
/// list its work as steps. Lines inside code fences are ignored.
pub fn task_steps(brief: &str) -> Vec<String> {
    let mut steps = Vec::new();
    let mut fenced = false;
    for line in task_section(brief).lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        if let Some(text) = step_item(line) {
            steps.push(text.to_string());
        }
    }
    if (MIN_STEPS..=MAX_STEPS).contains(&steps.len()) {
        steps
    } else {
        Vec::new()
    }
}

/// Added to a child's task when it has steps.
pub fn progress_instruction(total: usize) -> String {
    format!(
        "\n\n---\nProgress reporting: this task has {total} steps. Each time you finish one, \
         write `[progress k/{total}]` (k = steps finished so far) on its own line in your reply, \
         so the user can follow along. Report only steps that are really done."
    )
}

/// Added to a child's task that has no step list: the agent declares its
/// own plan first, then reports against it.
pub fn plan_instruction() -> String {
    "\n\n---\nProgress reporting: before you start working, decide your main steps (2-30) and \
     write `[plan N]` (N = how many) on its own line in your reply. Then each time you finish \
     one, write `[progress k/N]` (k = steps finished so far) on its own line, so the user can \
     follow along. Report only steps that are really done."
        .to_string()
}

/// A whole-line `[plan N]` marker with a usable step count.
pub fn plan_marker(line: &str) -> Option<usize> {
    let n: usize = line.trim().strip_prefix("[plan ")?.strip_suffix(']')?.trim().parse().ok()?;
    (MIN_STEPS..=MAX_STEPS).contains(&n).then_some(n)
}

/// The sidecar's `k/N`, as (done, total).
pub fn parse_state(state: &str) -> Option<(usize, usize)> {
    let (k, n) = state.trim().split_once('/')?;
    let (k, n) = (k.parse().ok()?, n.parse().ok()?);
    (n > 0 && k <= n).then_some((k, n))
}

/// Parse whole-line markers. Reject mismatched totals and invalid counts.
pub fn latest_marker(text: &str, total: usize) -> Option<usize> {
    text.lines().filter_map(|line| {
        let (done, count) = line.trim().strip_prefix("[progress ")?.strip_suffix(']')?.split_once('/')?;
        let done = done.parse().ok()?;
        (count.parse::<usize>().ok()? == total && done <= total).then_some(done)
    }).last()
}

/// Per-worker sidecar so a resumed agent never inherits an earlier run's progress.
pub fn progress_path(dir: &std::path::Path, pid: u32) -> std::path::PathBuf {
    dir.join(format!("progress-{pid}.txt"))
}

/// Track complete lines across streamed chunks, with a bounded buffer.
pub struct Reporter {
    total: usize,
    done: usize,
    line: String,
    overflow: bool,
    path: std::path::PathBuf,
}

impl Reporter {
    pub fn new(dir: &std::path::Path, total: usize) -> Self {
        let path = progress_path(dir, std::process::id());
        if total > 0 {
            let _ = crate::tool::file_state::atomic_write(&path, format!("0/{total}").as_bytes());
        }
        Self { path, total, done: 0, line: String::new(), overflow: false }
    }

    pub fn push(&mut self, text: &str) {
        for part in text.split_inclusive('\n') {
            if !self.overflow && self.line.len() + part.len() <= 80 {
                self.line.push_str(part);
            } else {
                self.overflow = true;
            }
            if part.ends_with('\n') {
                self.end_line();
            }
        }
    }

    pub fn end_line(&mut self) {
        if !self.overflow && self.total == 0 {
            // No step list in the brief: wait for the agent's own plan.
            if let Some(n) = plan_marker(&self.line) {
                if crate::tool::file_state::atomic_write(&self.path, format!("0/{n}").as_bytes()).is_ok() {
                    self.total = n;
                }
            }
        } else if !self.overflow {
            if let Some(done) = latest_marker(&self.line, self.total).filter(|&n| n > self.done) {
                if crate::tool::file_state::atomic_write(&self.path, format!("{done}/{}", self.total).as_bytes()).is_ok() {
                    self.done = done;
                }
            }
        }
        self.line.clear();
        self.overflow = false;
    }
}

impl Drop for Reporter {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BRIEF: &str = "---\nid: a1\n---\n\n# Brief\n\n## Task\n\nFix the MCP bug.\n\n\
1. Check the index is clean\n   - nested detail\n2. Run the tests\n3) Commit the four files\n\n\
```\n1. not a step\n```\n\n## Role\n\n1. not a step either\n";

    #[test]
    fn numbered_items_of_the_task_are_its_steps() {
        assert_eq!(
            task_steps(BRIEF),
            ["Check the index is clean", "Run the tests", "Commit the four files"]
        );
    }

    #[test]
    fn checkboxes_count_and_prose_does_not() {
        let b = "## Task\n\n- [ ] write the parser\n- [x] add tests\n- a plain bullet\n";
        assert_eq!(task_steps(b), ["write the parser", "add tests"]);
        assert!(task_steps("## Task\n\nJust fix it.\n").is_empty());
        assert!(task_steps("## Task\n\n1. only one step\n").is_empty(), "one item is not a plan");
        assert!(task_steps("## Task\n\n2024. was a year\n").is_empty());
    }

    /// The orchestrator's task often has its own headings; the steps
    /// under them still count.
    #[test]
    fn steps_under_the_tasks_own_headings_count() {
        let b = "---\nid: a1\n---\n\n# Brief\n\n## Task\n\n## 背景\n\n修复时段准入。\n\n## 步骤\n\n\
1、阅读 list_calls 实现\n2、补充测试\n3、运行 cargo test\n\n## 约束\n\n不提交。\n\n## Role\n\n(default)\n";
        assert_eq!(task_steps(b), ["阅读 list_calls 实现", "补充测试", "运行 cargo test"]);
    }

    #[test]
    fn an_agent_without_a_step_list_declares_its_own_plan() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = Reporter::new(dir.path(), 0);
        let path = progress_path(dir.path(), std::process::id());
        r.push("[progress 1/4]\n");
        assert!(!path.exists(), "no plan yet: nothing to show");
        r.push("I will work in four steps.\n[pla");
        r.push("n 4]\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "0/4");
        r.push("[progress 2/4]\n");
        assert_eq!(parse_state(&std::fs::read_to_string(&path).unwrap()), Some((2, 4)));
        assert_eq!(plan_marker("[plan 1]"), None);
        assert!(plan_instruction().contains("[plan N]"));
        drop(r);
        assert!(!path.exists());
    }

    #[test]
    fn the_newest_marker_for_this_plan_wins() {
        let t = "[progress 1/3]\nworking\n[progress 2/3]\n[progress 9/5]\n";
        assert_eq!(latest_marker(t, 3), Some(2));
        assert_eq!(latest_marker("[progress 7/3]", 3), None, "invalid counts are ignored");
        assert_eq!(latest_marker("no markers [progress x/3]", 3), None);
        assert!(progress_instruction(3).contains("[progress k/3]"));
    }
}
