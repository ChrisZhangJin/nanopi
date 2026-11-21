//! `/goal <condition>`: keep working, turn after turn, until a separate
//! model judges the condition met.
//!
//! Modelled on Claude Code's `/goal` (a session-scoped, prompt-based
//! Stop hook): after every turn the condition and the conversation so
//! far go to an evaluator call with no tools, which returns one of
//! three verdicts with a short reason:
//!
//! - **met**: the goal is cleared and recorded as achieved;
//! - **not met**: another turn starts, with the reason as guidance;
//! - **impossible**: the goal is cleared and recorded as failed.
//!
//! The evaluator only sees what the conversation shows; it runs no
//! commands and reads no files. So a good condition names a check the
//! working model's own output can demonstrate ("`cargo test` passes").
//!
//! This module is the I/O-light half — parsing, prompts, verdicts,
//! error classification, the evaluator call. The TUI owns the state
//! machine's timing (when to evaluate, retry, pause).

use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::agent::context::{AssistantBlock, ContentBlock, Context, ContextMessage};
use crate::event::{AgentEvent, Usage};
use crate::agent::loop_::Provider;

/// Longest condition accepted, as in Claude Code.
pub const MAX_CONDITION_CHARS: usize = 4000;

/// Consecutive evaluated turns without a single tool call before the
/// goal pauses: the model is answering the evaluator instead of working.
pub const NO_PROGRESS_LIMIT: u32 = 3;

/// Automatic retries after a transient turn error before pausing.
pub const MAX_RETRIES: u32 = 3;

/// Transcript budget for one evaluation, in characters. The tail is
/// kept: the latest turns are what decide whether the goal now holds.
const TRANSCRIPT_BUDGET: usize = 60_000;
/// Cap on any one tool result inside that transcript.
const TOOL_RESULT_CAP: usize = 2_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalCmd {
    /// Bare `/goal`.
    Status,
    /// `/goal clear` and its aliases.
    Clear,
    Set(String),
}

pub const USAGE: &str = "usage: /goal <condition> · /goal (status) · /goal clear";

pub fn parse_args(arg: &str) -> Result<GoalCmd, String> {
    let a = arg.trim();
    if a.is_empty() {
        return Ok(GoalCmd::Status);
    }
    if matches!(
        a.to_ascii_lowercase().as_str(),
        "clear" | "stop" | "off" | "reset" | "none" | "cancel"
    ) {
        return Ok(GoalCmd::Clear);
    }
    let n = a.chars().count();
    if n > MAX_CONDITION_CHARS {
        return Err(format!(
            "goal condition is {n} characters; the limit is {MAX_CONDITION_CHARS}"
        ));
    }
    Ok(GoalCmd::Set(a.to_string()))
}

/// The active goal of a session.
#[derive(Debug, Clone)]
pub struct Goal {
    pub condition: String,
    pub started: Instant,
    /// Turns the evaluator has judged so far.
    pub turns_evaluated: u32,
    /// Session tokens (input + output) when the goal was set, so the
    /// status can show what the goal has cost.
    pub tokens_at_start: u64,
    /// Tokens the evaluator itself used.
    pub eval_tokens: u64,
    pub last_reason: Option<String>,
    /// Why the goal stopped driving turns, if it did. Cleared by the
    /// user's next prompt.
    pub paused: Option<String>,
    /// Evaluated turns in a row that made no tool call.
    pub no_progress: u32,
    /// Automatic retries spent since the last good turn.
    pub retries: u32,
    /// When a retry after a transient error is due.
    pub retry_at: Option<Instant>,
    /// Bumped on every change of goal, so a verdict that comes back for
    /// a goal since replaced or cleared is dropped.
    pub generation: u64,
}

impl Goal {
    pub fn new(condition: String, tokens_now: u64, generation: u64) -> Self {
        Self {
            condition,
            started: Instant::now(),
            turns_evaluated: 0,
            tokens_at_start: tokens_now,
            eval_tokens: 0,
            last_reason: None,
            paused: None,
            no_progress: 0,
            retries: 0,
            retry_at: None,
            generation,
        }
    }

    /// The user spoke: a paused goal drives turns again, with fresh
    /// counters.
    pub fn resume_on_user_prompt(&mut self) {
        self.paused = None;
        self.no_progress = 0;
        self.retries = 0;
        self.retry_at = None;
    }

    pub fn spent(&self, tokens_now: u64) -> u64 {
        tokens_now.saturating_sub(self.tokens_at_start) + self.eval_tokens
    }
}

/// A goal that resolved earlier in the session, for `/goal` status.
#[derive(Debug, Clone)]
pub struct Finished {
    pub condition: String,
    pub achieved: bool,
    pub reason: String,
    pub elapsed: Duration,
    pub turns: u32,
    pub tokens: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Met,
    NotMet,
    Impossible,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evaluation {
    pub verdict: Verdict,
    pub reason: String,
}

pub const EVALUATOR_SYSTEM: &str = "You judge whether a coding assistant has met a goal. \
You are given the goal and a transcript of the work so far (USER, ASSISTANT, tool calls and \
TOOL results). Judge ONLY from evidence in the transcript — you cannot run commands or read \
files. A goal is met only when the transcript shows it (for example a test run that passed \
after the last relevant change); the assistant merely claiming success is not enough when \
the goal names a check that was not shown. If the goal includes a limit (a number of turns, \
a time), apply it from the transcript. Reply with ONE JSON object and nothing else:\n\
{\"verdict\": \"met\" | \"not_met\" | \"impossible\", \"reason\": \"<one or two sentences>\"}\n\
Use \"impossible\" only when the goal can never be satisfied (it contradicts itself, needs \
something that does not exist, or the user forbade the only way to reach it). For \
\"not_met\", the reason must say concretely what is still missing, as guidance for the next \
turn.";

pub fn evaluator_request(condition: &str, transcript: &str) -> String {
    format!("<goal>\n{condition}\n</goal>\n\n<transcript>\n{transcript}\n</transcript>")
}

/// The message that starts the first goal turn.
pub fn kickoff_prompt(condition: &str) -> String {
    format!(
        "Goal: {condition}\n\n\
         Work toward this goal until it is met. After each of your turns a separate check \
         reads the conversation and decides whether the goal holds; if not, you will be asked \
         to continue. It sees only what the conversation shows, so run the checks that prove \
         progress (tests, builds, commands) and let their output speak."
    )
}

/// The message that starts each later goal turn.
pub fn continue_prompt(condition: &str, reason: &str) -> String {
    format!("[goal not yet met] {reason}\n\nContinue working toward the goal: {condition}")
}

pub fn retry_prompt(condition: &str, error: &str) -> String {
    format!(
        "[goal: the last turn failed with an error: {error}]\n\nContinue working toward the \
         goal: {condition}"
    )
}

/// Pull the verdict out of the evaluator's reply. Tolerates prose or a
/// code fence around the JSON, and Claude Code's Stop-hook shape
/// (`{"ok": bool, "impossible": bool, "reason": …}`).
pub fn parse_verdict(text: &str) -> Result<Evaluation, String> {
    let start = text.find('{');
    let end = text.rfind('}');
    let (Some(s), Some(e)) = (start, end) else {
        return Err(format!("evaluator reply has no JSON: {}", clip(text, 200)));
    };
    if e < s {
        return Err(format!("evaluator reply has no JSON: {}", clip(text, 200)));
    }
    let v: serde_json::Value = serde_json::from_str(&text[s..=e])
        .map_err(|err| format!("evaluator reply is not valid JSON ({err}): {}", clip(text, 200)))?;
    let reason = v["reason"].as_str().unwrap_or("").trim().to_string();
    let verdict = if let Some(s) = v["verdict"].as_str() {
        match s.trim().to_ascii_lowercase().replace([' ', '-'], "_").as_str() {
            "met" | "achieved" | "done" | "yes" => Verdict::Met,
            "not_met" | "not_yet_met" | "notmet" | "no" | "continue" => Verdict::NotMet,
            "impossible" | "failed" => Verdict::Impossible,
            other => return Err(format!("unknown verdict `{other}`")),
        }
    } else if let Some(ok) = v["ok"].as_bool() {
        if v["impossible"].as_bool() == Some(true) {
            Verdict::Impossible
        } else if ok {
            Verdict::Met
        } else {
            Verdict::NotMet
        }
    } else {
        return Err(format!("evaluator reply has no verdict: {}", clip(text, 200)));
    };
    Ok(Evaluation { verdict, reason })
}

/// The conversation as the evaluator reads it: the tail that fits the
/// budget, tool results clipped, thinking left out. The message that
/// started the goal is always kept, even when it falls outside the
/// tail, so the evaluator sees any constraints written there.
pub fn transcript(ctx: &Context, goal_start: usize) -> String {
    let render = |m: &ContextMessage| -> String {
        let mut out = String::new();
        match m {
            ContextMessage::User { content } => {
                out.push_str("USER: ");
                for b in content {
                    match b {
                        ContentBlock::Text { text } => out.push_str(text),
                        ContentBlock::Image { .. } => out.push_str("[image]"),
                    }
                }
            }
            ContextMessage::Assistant { content } => {
                out.push_str("ASSISTANT: ");
                for b in content {
                    match b {
                        AssistantBlock::Text { text } => out.push_str(text),
                        AssistantBlock::Thinking { .. } => {}
                        AssistantBlock::ToolCall { call } => {
                            let args = call.arguments.to_string();
                            out.push_str(&format!("\n[tool_call {}({})]", call.name, clip(&args, 600)));
                        }
                    }
                }
            }
            ContextMessage::Tool { content, is_error, .. } => {
                out.push_str(if *is_error { "TOOL_ERROR: " } else { "TOOL: " });
                out.push_str(&clip_middle(content, TOOL_RESULT_CAP));
            }
        }
        out.push('\n');
        out
    };
    let rendered: Vec<String> = ctx.messages.iter().map(render).collect();
    let mut kept: Vec<&str> = Vec::new();
    let mut used = 0;
    let mut first_kept = rendered.len();
    for (i, r) in rendered.iter().enumerate().rev() {
        if used + r.len() > TRANSCRIPT_BUDGET && !kept.is_empty() {
            break;
        }
        used += r.len();
        kept.push(r);
        first_kept = i;
    }
    kept.reverse();
    let mut out = String::new();
    if goal_start < first_kept {
        if let Some(r) = rendered.get(goal_start) {
            out.push_str(&clip_middle(r, 8_000));
            out.push_str("[… earlier turns omitted …]\n");
        }
    } else if first_kept > 0 {
        out.push_str("[… earlier turns omitted …]\n");
    }
    for r in kept {
        out.push_str(r);
    }
    out
}

/// Index of the message that started the goal (its kickoff prompt,
/// possibly after text the user sent along with it), or
/// `usize::MAX` when it is gone (compacted away, or the goal was set
/// mid-turn). For [`transcript`]'s pinning.
pub fn kickoff_index(ctx: &Context, condition: &str) -> usize {
    let kickoff = kickoff_prompt(condition);
    ctx.messages
        .iter()
        .rposition(|m| match m {
            ContextMessage::User { content } => content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { text } if text.ends_with(&kickoff))),
            _ => false,
        })
        .unwrap_or(usize::MAX)
}

/// Tool calls made in messages from `from` on — the "did this turn do
/// any work" signal behind [`NO_PROGRESS_LIMIT`].
pub fn tool_calls_since(ctx: &Context, from: usize) -> usize {
    ctx.messages
        .iter()
        .skip(from)
        .map(|m| match m {
            ContextMessage::Assistant { content } => content
                .iter()
                .filter(|b| matches!(b, AssistantBlock::ToolCall { .. }))
                .count(),
            _ => 0,
        })
        .sum()
}

/// What a failed turn means for the goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Won't clear until the user fixes something: the goal is cleared.
    Fatal,
    /// Repeating it now would fail the same way: the goal pauses.
    Pause,
    /// Tends to clear on its own: retried after a wait.
    Transient,
}

pub fn classify_error(e: &str) -> ErrorKind {
    let l = e.to_ascii_lowercase();
    let any = |ws: &[&str]| ws.iter().any(|w| l.contains(w));
    if any(&[
        "401",
        "403",
        "unauthorized",
        "forbidden",
        "invalid api key",
        "invalid_api_key",
        "authentication",
        "insufficient_quota",
        "insufficient balance",
        "credit balance",
        "billing",
        "model_not_found",
        "model not found",
        "does not exist",
        "context_length_exceeded",
        "context length",
        "maximum context",
        "prompt is too long",
    ]) {
        ErrorKind::Fatal
    } else if any(&["429", "rate limit", "rate_limit", "too many requests", "usage limit", "hook"]) {
        ErrorKind::Pause
    } else {
        ErrorKind::Transient
    }
}

/// Wait before automatic retry `n` (1-based): 30 s, 60 s, 120 s.
pub fn retry_delay(n: u32) -> Duration {
    Duration::from_secs(30 * (1u64 << n.saturating_sub(1).min(4)))
}

/// Run one evaluation. The error string is for the user.
pub async fn evaluate(
    provider: &dyn Provider,
    condition: &str,
    transcript: &str,
) -> Result<(Evaluation, Usage), String> {
    let ctx = Context {
        system: Some(EVALUATOR_SYSTEM.into()),
        messages: vec![ContextMessage::User {
            content: vec![ContentBlock::Text { text: evaluator_request(condition, transcript) }],
        }],
        tools: vec![],
        thinking: None,
    };
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
    let collect = tokio::spawn(async move {
        let mut text = String::new();
        let mut usage = Usage::default();
        while let Some(ev) = rx.recv().await {
            match ev {
                AgentEvent::TextDelta { text: t, .. } => text.push_str(&t),
                AgentEvent::Done { usage: u, .. } => usage = u,
                _ => {}
            }
        }
        (text, usage)
    });
    let streamed = provider.stream_turn(&ctx, tx).await;
    let (text, usage) = collect.await.map_err(|e| format!("evaluator task failed: {e}"))?;
    streamed.map_err(|e| format!("goal check failed: {e}"))?;
    let eval = parse_verdict(&text)?;
    Ok((eval, usage))
}

pub fn format_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Keep the start and the end; for command output the end (exit status,
/// test summary) usually matters most.
fn clip_middle(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let head = max / 3;
    let tail = max - head;
    let a: String = s.chars().take(head).collect();
    let b: String = s.chars().skip(n - tail).collect();
    format!("{a}\n[… {} chars omitted …]\n{b}", n - max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::context::ToolCallBlock;

    #[test]
    fn args() {
        assert_eq!(parse_args(""), Ok(GoalCmd::Status));
        assert_eq!(parse_args("  "), Ok(GoalCmd::Status));
        for a in ["clear", "stop", "OFF", "reset", "none", "cancel"] {
            assert_eq!(parse_args(a), Ok(GoalCmd::Clear), "{a}");
        }
        assert_eq!(
            parse_args(" all tests pass "),
            Ok(GoalCmd::Set("all tests pass".into()))
        );
        assert!(parse_args(&"x".repeat(MAX_CONDITION_CHARS)).is_ok());
        assert!(parse_args(&"x".repeat(MAX_CONDITION_CHARS + 1)).is_err());
    }

    #[test]
    fn verdicts() {
        let e = parse_verdict(r#"{"verdict":"met","reason":"tests pass"}"#).unwrap();
        assert_eq!(e, Evaluation { verdict: Verdict::Met, reason: "tests pass".into() });
        let e = parse_verdict("Sure.\n```json\n{\"verdict\": \"not_met\", \"reason\": \"2 tests fail\"}\n```").unwrap();
        assert_eq!(e.verdict, Verdict::NotMet);
        assert_eq!(e.reason, "2 tests fail");
        assert_eq!(parse_verdict(r#"{"verdict":"Not met"}"#).unwrap().verdict, Verdict::NotMet);
        assert_eq!(parse_verdict(r#"{"verdict":"impossible","reason":"x"}"#).unwrap().verdict, Verdict::Impossible);
        // Claude Code's Stop-hook shape.
        assert_eq!(parse_verdict(r#"{"ok":true}"#).unwrap().verdict, Verdict::Met);
        assert_eq!(parse_verdict(r#"{"ok":false,"reason":"r"}"#).unwrap().verdict, Verdict::NotMet);
        assert_eq!(parse_verdict(r#"{"ok":false,"impossible":true}"#).unwrap().verdict, Verdict::Impossible);
        assert!(parse_verdict("the goal is met").is_err());
        assert!(parse_verdict(r#"{"verdict":"maybe"}"#).is_err());
        assert!(parse_verdict("} {").is_err());
    }

    fn ctx_with(msgs: Vec<ContextMessage>) -> Context {
        Context { system: None, messages: msgs, tools: vec![], thinking: None }
    }

    fn user(t: &str) -> ContextMessage {
        ContextMessage::User { content: vec![ContentBlock::Text { text: t.into() }] }
    }

    fn call(name: &str) -> ContextMessage {
        ContextMessage::Assistant {
            content: vec![
                AssistantBlock::Thinking { text: "secret thoughts".into() },
                AssistantBlock::ToolCall {
                    call: ToolCallBlock { id: "1".into(), name: name.into(), arguments: serde_json::json!({"command": "cargo test"}) },
                },
            ],
        }
    }

    fn tool(t: &str) -> ContextMessage {
        ContextMessage::Tool { tool_call_id: "1".into(), content: t.into(), is_error: false, images: vec![] }
    }

    #[test]
    fn transcript_shows_work_not_thinking() {
        let c = ctx_with(vec![user("Goal: tests pass"), call("bash"), tool("test result: ok")]);
        let t = transcript(&c, 0);
        assert!(t.contains("USER: Goal: tests pass"));
        assert!(t.contains("[tool_call bash("));
        assert!(t.contains("TOOL: test result: ok"));
        assert!(!t.contains("secret thoughts"));
        assert!(!t.contains("omitted"));
    }

    #[test]
    fn transcript_keeps_the_tail_and_the_goal_message() {
        let mut msgs = vec![user("old chatter"), user("Goal: the special constraint")];
        for i in 0..200 {
            msgs.push(user(&format!("filler {i} {}", "y".repeat(1000))));
        }
        msgs.push(tool(&format!("{}FINAL SUMMARY", "z".repeat(10_000))));
        let t = transcript(&ctx_with(msgs), 1);
        assert!(t.len() < TRANSCRIPT_BUDGET + 10_000, "{}", t.len());
        assert!(t.starts_with("USER: Goal: the special constraint"));
        assert!(!t.contains("old chatter"));
        assert!(t.contains("filler 199"));
        assert!(!t.contains("filler 0 "));
        // A long tool result keeps its end.
        assert!(t.contains("FINAL SUMMARY"));
        assert!(t.contains("chars omitted"));
    }

    #[test]
    fn counts_tool_calls_since_a_point() {
        let c = ctx_with(vec![call("bash"), user("x"), call("read"), call("edit")]);
        assert_eq!(tool_calls_since(&c, 0), 3);
        assert_eq!(tool_calls_since(&c, 1), 2);
        assert_eq!(tool_calls_since(&c, 4), 0);
    }

    #[test]
    fn error_classes() {
        assert_eq!(classify_error("API error 401: invalid api key"), ErrorKind::Fatal);
        assert_eq!(classify_error("model_not_found"), ErrorKind::Fatal);
        assert_eq!(classify_error("context_length_exceeded"), ErrorKind::Fatal);
        assert_eq!(classify_error("HTTP 429 Too Many Requests"), ErrorKind::Pause);
        assert_eq!(classify_error("error sending request: connection reset"), ErrorKind::Transient);
        assert_eq!(classify_error("the response stream was closed after 300s"), ErrorKind::Transient);
        assert_eq!(retry_delay(1), Duration::from_secs(30));
        assert_eq!(retry_delay(2), Duration::from_secs(60));
        assert_eq!(retry_delay(3), Duration::from_secs(120));
    }

    #[test]
    fn prompts_carry_the_condition() {
        assert!(kickoff_prompt("C1").contains("Goal: C1"));
        let p = continue_prompt("C2", "R");
        assert!(p.contains("C2") && p.contains("R"));
        assert!(evaluator_request("C3", "T").contains("<goal>\nC3\n</goal>"));
    }

    #[test]
    fn spent_counts_turns_and_evaluator() {
        let mut g = Goal::new("c".into(), 100, 1);
        g.eval_tokens = 5;
        assert_eq!(g.spent(150), 55);
        g.paused = Some("x".into());
        g.no_progress = 2;
        g.resume_on_user_prompt();
        assert!(g.paused.is_none());
        assert_eq!(g.no_progress, 0);
    }

    #[test]
    fn durations() {
        assert_eq!(format_duration(Duration::from_secs(5)), "5s");
        assert_eq!(format_duration(Duration::from_secs(125)), "2m05s");
        assert_eq!(format_duration(Duration::from_secs(3725)), "1h02m");
    }

    /// A provider that answers with a fixed reply.
    struct Canned(&'static str);

    #[async_trait::async_trait]
    impl Provider for Canned {
        fn id(&self) -> &'static str {
            "canned"
        }
        async fn stream_turn(&self, _ctx: &Context, tx: mpsc::Sender<AgentEvent>) -> Result<Usage, String> {
            let _ = tx.send(AgentEvent::TextDelta { content_index: 0, text: self.0.into() }).await;
            let usage = Usage { input_tokens: 7, output_tokens: 3, ..Usage::default() };
            let _ = tx
                .send(AgentEvent::Done { finish_reason: crate::event::FinishReason::Stop, usage: usage.clone() })
                .await;
            Ok(usage)
        }
    }

    #[tokio::test]
    async fn evaluate_end_to_end_with_a_canned_provider() {
        let (e, u) = evaluate(&Canned(r#"{"verdict":"met","reason":"ok"}"#), "c", "t").await.unwrap();
        assert_eq!(e.verdict, Verdict::Met);
        assert_eq!(u.input_tokens + u.output_tokens, 10);
        assert!(evaluate(&Canned("no idea"), "c", "t").await.is_err());
    }
}
