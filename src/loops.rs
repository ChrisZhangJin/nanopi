//! `/loop [interval] [prompt]`: re-run a prompt while the session stays
//! open. Modelled on Claude Code's `/loop`:
//!
//! | input                     | behaviour                                    |
//! |---------------------------|----------------------------------------------|
//! | `/loop 5m check the CI`   | fixed interval                               |
//! | `/loop check the CI`      | self-paced: the model picks each delay       |
//! | `/loop` / `/loop 15m`     | `loop.md`, else the built-in maintenance prompt |
//!
//! The interval may lead (`5m …`) or trail (`… every 2 hours`); units
//! are s, m, h, d. Iterations fire only while the session is idle, never
//! mid-turn, and a missed fire runs once, not once per missed interval.
//!
//! Self-paced loops give the model a `schedule_wakeup` tool, present only
//! while such a loop runs. Each iteration ends by calling it with a delay
//! (clamped to 60 s – 1 h) or `stop: true`. An iteration that does
//! neither gets one fallback wakeup 20 minutes later; if that one does
//! not reschedule either, the loop ends.
//!
//! One loop per session; a new `/loop` replaces it. Loops expire seven
//! days after they start. `/loop stop` (or Esc on an empty prompt while
//! idle) ends one.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::agent::context::ToolSpec;
use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

pub const MIN_INTERVAL: Duration = Duration::from_secs(60);
pub const MAX_INTERVAL: Duration = Duration::from_secs(7 * 24 * 3600);
pub const EXPIRY: Duration = Duration::from_secs(7 * 24 * 3600);
pub const WAKEUP_MIN: u64 = 60;
pub const WAKEUP_MAX: u64 = 3600;
pub const FALLBACK_WAKEUP: Duration = Duration::from_secs(20 * 60);
/// `loop.md` beyond this is truncated.
pub const LOOP_MD_MAX_BYTES: usize = 25_000;
pub const TOOL_NAME: &str = "schedule_wakeup";
/// Ends any running loop; present whenever one runs.
pub const STOP_TOOL_NAME: &str = "stop_loop";

pub const USAGE: &str = "usage: /loop [interval] [prompt] · /loop status · /loop stop   \
(interval like 30s, 5m, 2h, 1d, or `… every 2 hours`)";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopCmd {
    Status,
    Stop,
    Start { interval: Option<Duration>, prompt: Option<String> },
}

pub fn parse_args(arg: &str) -> Result<LoopCmd, String> {
    let a = arg.trim();
    match a.to_ascii_lowercase().as_str() {
        "status" | "list" => return Ok(LoopCmd::Status),
        "stop" | "off" | "cancel" | "clear" | "end" => return Ok(LoopCmd::Stop),
        _ => {}
    }
    let (interval, rest) = split_interval(a)?;
    let prompt = (!rest.trim().is_empty()).then(|| rest.trim().to_string());
    Ok(LoopCmd::Start { interval, prompt })
}

/// Pull an interval off the front (`5m rest`) or the end
/// (`rest every 5 minutes`, `rest every hour`) of the argument.
fn split_interval(a: &str) -> Result<(Option<Duration>, String), String> {
    let mut words = a.split_whitespace();
    if let Some(first) = words.next() {
        if let Some(d) = parse_duration(first)? {
            let rest = a[first.len()..].to_string();
            return Ok((Some(d), rest));
        }
    }
    // Trailing "every N unit" / "every unit".
    let lower = a.to_ascii_lowercase();
    if let Some(pos) = lower.rfind("every ") {
        let at_word_start = pos == 0 || lower.as_bytes()[pos - 1].is_ascii_whitespace();
        let tail = lower[pos + 6..].trim().trim_end_matches(['.', '!']);
        if at_word_start {
            let parts: Vec<&str> = tail.split_whitespace().collect();
            let d = match parts.as_slice() {
                // "every 10m" / "every hour"
                [w] => parse_duration(w)?.or_else(|| unit_secs(w).map(Duration::from_secs)),
                // "every 2 hours"
                [n, unit] => match (n.parse::<u64>(), unit_secs(unit)) {
                    (Ok(n), Some(u)) => Some(Duration::from_secs(n.saturating_mul(u))),
                    _ => None,
                },
                _ => None,
            };
            if let Some(d) = d {
                if d.is_zero() {
                    return Err("the interval must be longer than zero".into());
                }
                let rest = a[..pos].trim_end().trim_end_matches(',').to_string();
                return Ok((Some(d), rest));
            }
        }
    }
    Ok((None, a.to_string()))
}

fn unit_secs(u: &str) -> Option<u64> {
    Some(match u {
        "s" | "sec" | "secs" | "second" | "seconds" => 1,
        "m" | "min" | "mins" | "minute" | "minutes" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600,
        "d" | "day" | "days" => 86_400,
        _ => return None,
    })
}

/// `30s`, `5m`, `2h`, `1d` (also `5min`, `2hours`). `Ok(None)` when the
/// word is not a duration at all; `Err` when it is one but unusable.
pub fn parse_duration(word: &str) -> Result<Option<Duration>, String> {
    let w = word.to_ascii_lowercase();
    let digits: String = w.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || digits.len() == w.len() {
        return Ok(None);
    }
    let Some(unit) = unit_secs(&w[digits.len()..]) else {
        return Ok(None);
    };
    let n: u64 = digits.parse().map_err(|_| format!("interval `{word}` is too large"))?;
    if n == 0 {
        return Err("the interval must be longer than zero".into());
    }
    Ok(Some(Duration::from_secs(n.saturating_mul(unit))))
}

/// Clamp a requested interval into what the loop supports. Returns the
/// interval and, when it changed, a note for the user.
pub fn clamp_interval(d: Duration) -> (Duration, Option<String>) {
    if d < MIN_INTERVAL {
        (MIN_INTERVAL, Some(format!("interval {} raised to the 1m minimum", human(d))))
    } else if d > MAX_INTERVAL {
        (MAX_INTERVAL, Some(format!("interval {} lowered to the 7d maximum", human(d))))
    } else {
        (d, None)
    }
}

pub fn human(d: Duration) -> String {
    let s = d.as_secs();
    if s.is_multiple_of(86_400) && s >= 86_400 {
        format!("{}d", s / 86_400)
    } else if s.is_multiple_of(3600) && s >= 3600 {
        format!("{}h", s / 3600)
    } else if s.is_multiple_of(60) && s >= 60 {
        format!("{}m", s / 60)
    } else if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

pub const MAINTENANCE_PROMPT: &str = "Loop maintenance pass. Work through these in order and \
stop at the first that has something to do:\n\
1. Continue any unfinished work from this conversation.\n\
2. Tend to the current branch: failing tests or builds, merge conflicts, review comments you \
can see locally.\n\
3. If nothing else is pending, do one small cleanup (a bug hunt or a simplification) and \
report it.\n\
Ask before anything destructive (pushing, deleting). If everything is quiet, say so in one \
line.";

/// `loop.md` for a bare `/loop`: the project's `.nanopi/loop.md`, else
/// `<nanopi home>/loop.md`. Re-read every iteration, so edits apply to
/// the next one.
pub fn loop_md_path(cwd: &Path) -> Option<PathBuf> {
    let project = cwd.join(".nanopi").join("loop.md");
    if project.is_file() {
        return Some(project);
    }
    let user = crate::paths::nanopi_home()?.join("loop.md");
    user.is_file().then_some(user)
}

/// The prompt a bare `/loop` runs right now, and where it came from.
pub fn default_prompt(cwd: &Path) -> (String, String) {
    if let Some(p) = loop_md_path(cwd) {
        if let Ok(mut s) = std::fs::read_to_string(&p) {
            if s.len() > LOOP_MD_MAX_BYTES {
                let mut cut = LOOP_MD_MAX_BYTES;
                while !s.is_char_boundary(cut) {
                    cut -= 1;
                }
                s.truncate(cut);
            }
            if !s.trim().is_empty() {
                return (s.trim().to_string(), p.display().to_string());
            }
        }
    }
    (MAINTENANCE_PROMPT.to_string(), "built-in maintenance prompt".to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pace {
    Fixed(Duration),
    SelfPaced,
}

#[derive(Debug, Clone)]
pub struct LoopState {
    /// `None`: a bare `/loop`, resolved from `loop.md` per iteration.
    pub prompt: Option<String>,
    pub pace: Pace,
    pub created: Instant,
    /// When the next iteration is due. `None` while one is running.
    pub next_fire: Option<Instant>,
    pub iterations: u32,
    /// Self-paced: the pending wakeup is the 20-minute fallback, so an
    /// iteration that again neither reschedules nor stops ends the loop.
    pub on_fallback: bool,
    pub last_reason: Option<String>,
}

impl LoopState {
    /// A new loop; its first iteration is due now.
    pub fn new(prompt: Option<String>, pace: Pace, now: Instant) -> Self {
        Self {
            prompt,
            pace,
            created: now,
            next_fire: Some(now),
            iterations: 0,
            on_fallback: false,
            last_reason: None,
        }
    }

    pub fn expired(&self, now: Instant) -> bool {
        now.duration_since(self.created) >= EXPIRY
    }

    pub fn due(&self, now: Instant) -> bool {
        self.next_fire.is_some_and(|t| now >= t)
    }

    /// Mark an iteration as started. Fixed loops schedule the next one
    /// from now, so a long iteration or a busy session never queues a
    /// backlog of fires.
    pub fn fired(&mut self, now: Instant) {
        self.iterations += 1;
        self.next_fire = match self.pace {
            Pace::Fixed(d) => Some(now + d),
            Pace::SelfPaced => None,
        };
    }

    /// A self-paced iteration finished. `req` is what the model asked
    /// for through `schedule_wakeup`, if anything. Returns `false` when
    /// the loop is over.
    pub fn after_self_paced_iteration(&mut self, req: Option<WakeupRequest>, now: Instant) -> bool {
        match req {
            Some(WakeupRequest::Stop { reason }) => {
                self.last_reason = reason;
                false
            }
            Some(WakeupRequest::After { delay, reason }) => {
                self.next_fire = Some(now + delay);
                self.on_fallback = false;
                self.last_reason = Some(reason);
                true
            }
            None if self.on_fallback => false,
            None => {
                self.next_fire = Some(now + FALLBACK_WAKEUP);
                self.on_fallback = true;
                self.last_reason = Some("no wakeup scheduled — fallback check in 20m".into());
                true
            }
        }
    }

    pub fn describe_pace(&self) -> String {
        match self.pace {
            Pace::Fixed(d) => format!("every {}", human(d)),
            Pace::SelfPaced => "self-paced".to_string(),
        }
    }
}

/// The user message for iteration `n`.
///
/// A `/skill:<name> …` prompt must stay first in the message — the
/// agent only expands a skill invocation at the start — so for those
/// the loop's header is left out and any self-paced instruction rides
/// along as part of the skill's arguments.
pub fn iteration_prompt(prompt: &str, pace: Pace, n: u32) -> String {
    let instruction = format!(
        "When you are done with this iteration, call `{TOOL_NAME}`: either with \
         `delay_seconds` ({WAKEUP_MIN}–{WAKEUP_MAX}) and a short `reason` for when to check \
         again — short while something is changing, long when nothing is pending — or with \
         `stop: true` once the task is complete or there is nothing left to watch."
    );
    let skill = prompt.trim_start().starts_with("/skill:");
    match (pace, skill) {
        (Pace::Fixed(_), true) => prompt.to_string(),
        (Pace::SelfPaced, true) => format!("{prompt}\n\n(/loop iteration {n}) {instruction}"),
        (Pace::Fixed(d), false) => format!(
            "[/loop every {} · iteration {n}]\n{prompt}\n\n(If this loop's task is complete or no \
             longer needed, call `{STOP_TOOL_NAME}` to end the loop.)",
            human(d)
        ),
        (Pace::SelfPaced, false) => {
            format!("[/loop self-paced · iteration {n}]\n{prompt}\n\n{instruction}")
        }
    }
}

/// What the model asked for in a self-paced iteration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WakeupRequest {
    After { delay: Duration, reason: String },
    Stop { reason: Option<String> },
}

static WAKEUP: Mutex<Option<WakeupRequest>> = Mutex::new(None);

/// Take the request the last iteration made (and forget it).
pub fn take_wakeup() -> Option<WakeupRequest> {
    WAKEUP.lock().unwrap_or_else(|e| e.into_inner()).take()
}

fn set_wakeup(r: WakeupRequest) {
    *WAKEUP.lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
}

static STOP: Mutex<Option<Option<String>>> = Mutex::new(None);

/// Take a `stop_loop` request (with its reason, if given).
pub fn take_stop_request() -> Option<Option<String>> {
    STOP.lock().unwrap_or_else(|e| e.into_inner()).take()
}

/// Keep the loop tools on a registry in step with the loop: `stop_loop`
/// while any loop runs, `schedule_wakeup` while a self-paced one does.
/// Returns whether anything changed (the caller then refreshes the
/// tool specs it sends).
pub fn sync_tools(reg: &mut crate::tool::ToolRegistry, pace: Option<Pace>) -> bool {
    let mut changed = false;
    let mut want = |name: &str, on: bool, make: &dyn Fn() -> std::sync::Arc<dyn Tool>| {
        let has = reg.get(name).is_some();
        if on && !has {
            changed |= reg.register_external(make()).is_ok() && reg.get(name).is_some();
        } else if !on && has {
            reg.remove(name);
            changed = true;
        }
    };
    want(STOP_TOOL_NAME, pace.is_some(), &|| std::sync::Arc::new(StopLoopTool));
    want(TOOL_NAME, pace == Some(Pace::SelfPaced), &|| std::sync::Arc::new(ScheduleWakeupTool));
    changed
}

/// The `stop_loop` tool: lets the model end a loop — when its task is
/// done, or when the user asks it to.
pub struct StopLoopTool;

#[async_trait]
impl Tool for StopLoopTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: STOP_TOOL_NAME.into(),
            description: "End the running /loop (fixed or self-paced) after this turn. Call it when \
                          the loop's task is complete or no longer needed, or when the user asks \
                          to stop the loop."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "reason": {"type": "string", "description": "One short sentence: why the loop ends."}
                }
            }),
        }
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let reason = args["reason"].as_str().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        *STOP.lock().unwrap_or_else(|e| e.into_inner()) = Some(reason);
        Ok(ToolOutput {
            content: "The loop will end when this turn finishes.".into(),
            is_error: false,
            metadata: None,
            images: Vec::new(),
        })
    }
}

/// The `schedule_wakeup` tool. Registered on the main agent only while a
/// self-paced loop runs.
pub struct ScheduleWakeupTool;

#[async_trait]
impl Tool for ScheduleWakeupTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: TOOL_NAME.into(),
            description: format!(
                "Schedule the next iteration of the running self-paced /loop, or end it. Call \
                 once at the end of each iteration. delay_seconds is clamped to \
                 {WAKEUP_MIN}–{WAKEUP_MAX}. Use stop=true when the loop's task is complete."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "delay_seconds": {"type": "integer", "description": "Seconds until the next iteration (60–3600)."},
                    "reason": {"type": "string", "description": "One short sentence: why this delay, or why stopping."},
                    "stop": {"type": "boolean", "description": "End the loop instead of scheduling another iteration."}
                }
            }),
        }
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let reason = args["reason"].as_str().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let text = if args["stop"].as_bool() == Some(true) {
            set_wakeup(WakeupRequest::Stop { reason: reason.clone() });
            "Loop will stop after this iteration.".to_string()
        } else {
            let Some(raw) = args["delay_seconds"]
                .as_u64()
                .or_else(|| args["delay_seconds"].as_f64().map(|f| f.max(0.0) as u64))
            else {
                return Err(ToolError::InvalidArgs(
                    "give delay_seconds (60–3600), or stop: true".into(),
                ));
            };
            let secs = raw.clamp(WAKEUP_MIN, WAKEUP_MAX);
            set_wakeup(WakeupRequest::After {
                delay: Duration::from_secs(secs),
                reason: reason.unwrap_or_else(|| "no reason given".into()),
            });
            if secs == raw {
                format!("Next iteration in {}.", human(Duration::from_secs(secs)))
            } else {
                format!("Next iteration in {} (clamped from {raw}s).", human(Duration::from_secs(secs)))
            }
        };
        Ok(ToolOutput { content: text, is_error: false, metadata: None, images: Vec::new() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(i: Option<u64>, p: Option<&str>) -> LoopCmd {
        LoopCmd::Start { interval: i.map(Duration::from_secs), prompt: p.map(str::to_string) }
    }

    #[test]
    fn args() {
        assert_eq!(parse_args(""), Ok(start(None, None)));
        assert_eq!(parse_args("stop"), Ok(LoopCmd::Stop));
        assert_eq!(parse_args("Cancel"), Ok(LoopCmd::Stop));
        assert_eq!(parse_args("status"), Ok(LoopCmd::Status));
        assert_eq!(parse_args("5m check the deploy"), Ok(start(Some(300), Some("check the deploy"))));
        assert_eq!(parse_args("15m"), Ok(start(Some(900), None)));
        assert_eq!(parse_args("30s ping"), Ok(start(Some(30), Some("ping"))));
        assert_eq!(parse_args("1d x"), Ok(start(Some(86_400), Some("x"))));
        assert_eq!(parse_args("2hours x"), Ok(start(Some(7200), Some("x"))));
        assert_eq!(parse_args("check the deploy"), Ok(start(None, Some("check the deploy"))));
        assert_eq!(
            parse_args("check the deploy every 2 hours"),
            Ok(start(Some(7200), Some("check the deploy")))
        );
        assert_eq!(parse_args("check CI, every hour."), Ok(start(Some(3600), Some("check CI"))));
        assert_eq!(parse_args("run tests every 10m"), Ok(start(Some(600), Some("run tests"))));
        // "every" that is not an interval stays in the prompt.
        assert_eq!(
            parse_args("fix every failing test"),
            Ok(start(None, Some("fix every failing test")))
        );
        assert_eq!(parse_args("/review-pr 12"), Ok(start(None, Some("/review-pr 12"))));
        assert!(parse_args("0m x").is_err());
        // A bare number or a word with digits is prompt text.
        assert_eq!(parse_args("5 things"), Ok(start(None, Some("5 things"))));
        assert_eq!(parse_args("v2 rollout"), Ok(start(None, Some("v2 rollout"))));
    }

    #[test]
    fn clamping_and_names() {
        assert_eq!(clamp_interval(Duration::from_secs(30)).0, MIN_INTERVAL);
        assert!(clamp_interval(Duration::from_secs(30)).1.is_some());
        assert_eq!(clamp_interval(Duration::from_secs(300)), (Duration::from_secs(300), None));
        assert_eq!(clamp_interval(Duration::from_secs(30 * 86_400)).0, MAX_INTERVAL);
        assert_eq!(human(Duration::from_secs(300)), "5m");
        assert_eq!(human(Duration::from_secs(7200)), "2h");
        assert_eq!(human(Duration::from_secs(86_400)), "1d");
        assert_eq!(human(Duration::from_secs(90)), "1m30s");
        assert_eq!(human(Duration::from_secs(45)), "45s");
    }

    #[test]
    fn fixed_loop_schedules_from_the_fire_time() {
        let t0 = Instant::now();
        let mut l = LoopState::new(Some("p".into()), Pace::Fixed(Duration::from_secs(300)), t0);
        assert!(l.due(t0), "first iteration runs at once");
        // Fired late: the next one counts from the actual fire, no backlog.
        let late = t0 + Duration::from_secs(1000);
        l.fired(late);
        assert_eq!(l.iterations, 1);
        assert!(!l.due(late + Duration::from_secs(299)));
        assert!(l.due(late + Duration::from_secs(300)));
        assert!(!l.expired(t0 + Duration::from_secs(3600)));
        assert!(l.expired(t0 + EXPIRY));
    }

    #[test]
    fn self_paced_wakeups_stop_and_fallback() {
        let t0 = Instant::now();
        let mut l = LoopState::new(Some("p".into()), Pace::SelfPaced, t0);
        l.fired(t0);
        assert!(l.next_fire.is_none(), "nothing due while the iteration runs");
        let req = WakeupRequest::After { delay: Duration::from_secs(120), reason: "build".into() };
        assert!(l.after_self_paced_iteration(Some(req), t0));
        assert_eq!(l.next_fire, Some(t0 + Duration::from_secs(120)));
        // No request: one fallback, then the loop ends.
        assert!(l.after_self_paced_iteration(None, t0));
        assert_eq!(l.next_fire, Some(t0 + FALLBACK_WAKEUP));
        assert!(!l.after_self_paced_iteration(None, t0));
        // A request after a fallback resets it.
        let mut l = LoopState::new(None, Pace::SelfPaced, t0);
        assert!(l.after_self_paced_iteration(None, t0));
        let req = WakeupRequest::After { delay: Duration::from_secs(60), reason: "r".into() };
        assert!(l.after_self_paced_iteration(Some(req), t0));
        assert!(!l.on_fallback);
        assert!(!l.after_self_paced_iteration(Some(WakeupRequest::Stop { reason: None }), t0));
    }

    #[test]
    fn iteration_prompts() {
        let p = iteration_prompt("check CI", Pace::Fixed(Duration::from_secs(300)), 3);
        assert!(p.starts_with("[/loop every 5m · iteration 3]\ncheck CI"));
        assert!(!p.contains(TOOL_NAME));
        assert!(p.contains(STOP_TOOL_NAME), "a fixed loop is told how to end itself");
        let p = iteration_prompt("check CI", Pace::SelfPaced, 1);
        assert!(p.contains(TOOL_NAME) && p.contains("stop: true"));
        // A skill invocation stays first so it still expands.
        let five = Pace::Fixed(Duration::from_secs(300));
        assert_eq!(iteration_prompt("/skill:review 12", five, 2), "/skill:review 12");
        let p = iteration_prompt("/skill:review 12", Pace::SelfPaced, 2);
        assert!(p.starts_with("/skill:review 12\n") && p.contains(TOOL_NAME));
    }

    #[test]
    fn loop_md_project_then_user_then_builtin() {
        let g = crate::TempNanopiHome::new();
        let cwd = tempfile::tempdir().unwrap();
        assert_eq!(default_prompt(cwd.path()).0, MAINTENANCE_PROMPT);
        std::fs::write(g.path().join("loop.md"), "user loop\n").unwrap();
        assert_eq!(default_prompt(cwd.path()).0, "user loop");
        std::fs::create_dir(cwd.path().join(".nanopi")).unwrap();
        std::fs::write(cwd.path().join(".nanopi/loop.md"), "x".repeat(LOOP_MD_MAX_BYTES + 50)).unwrap();
        let (p, src) = default_prompt(cwd.path());
        assert_eq!(p.len(), LOOP_MD_MAX_BYTES);
        assert!(src.ends_with(".nanopi/loop.md"));
    }

    #[tokio::test]
    async fn stop_tool_and_tool_sync() {
        let ctx = ToolContext { cwd: PathBuf::from(".") };
        let _ = take_stop_request();
        StopLoopTool.execute(json!({"reason": "service is healthy"}), &ctx).await.unwrap();
        assert_eq!(take_stop_request(), Some(Some("service is healthy".into())));
        assert_eq!(take_stop_request(), None);

        let mut reg = crate::tool::ToolRegistry::standard();
        let names = |r: &crate::tool::ToolRegistry| {
            (r.get(STOP_TOOL_NAME).is_some(), r.get(TOOL_NAME).is_some())
        };
        assert!(!sync_tools(&mut reg, None));
        assert_eq!(names(&reg), (false, false));
        assert!(sync_tools(&mut reg, Some(Pace::Fixed(Duration::from_secs(600)))));
        assert_eq!(names(&reg), (true, false), "a fixed loop can be stopped, not rescheduled");
        assert!(!sync_tools(&mut reg, Some(Pace::Fixed(Duration::from_secs(600)))));
        assert!(sync_tools(&mut reg, Some(Pace::SelfPaced)));
        assert_eq!(names(&reg), (true, true));
        assert!(sync_tools(&mut reg, None));
        assert_eq!(names(&reg), (false, false));
    }

    #[tokio::test]
    async fn wakeup_tool_records_and_clamps() {
        let ctx = ToolContext { cwd: PathBuf::from(".") };
        let _ = take_wakeup();
        let out = ScheduleWakeupTool
            .execute(json!({"delay_seconds": 5, "reason": "soon"}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("clamped"));
        assert_eq!(
            take_wakeup(),
            Some(WakeupRequest::After { delay: Duration::from_secs(60), reason: "soon".into() })
        );
        assert_eq!(take_wakeup(), None, "taken once");
        ScheduleWakeupTool.execute(json!({"stop": true}), &ctx).await.unwrap();
        assert_eq!(take_wakeup(), Some(WakeupRequest::Stop { reason: None }));
        assert!(ScheduleWakeupTool.execute(json!({}), &ctx).await.is_err());
    }
}
