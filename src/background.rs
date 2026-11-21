//! Background mode: hand a session that has a `/goal` or `/loop` to a
//! headless worker process, and bring it back later.
//!
//! - `/background` (TUI) writes a state file, starts
//!   `nanopi --worker <session-id>` detached (own session, output to a
//!   log), and quits the TUI.
//! - The worker is print mode resuming that session: instead of one
//!   turn it keeps running goal turns and loop iterations, appending to
//!   the same session file, and records its progress in the state file.
//!   It exits when nothing is left to do (goal met / impossible /
//!   paused, loop ended) or when asked to stop: the first SIGTERM stops
//!   it after the current turn, a second one cancels that turn.
//! - `nanopi --foreground [ID]` stops the worker that way, then opens
//!   the TUI on the session with the goal and loop restored, so the
//!   work simply continues in the foreground.
//! - `nanopi --background` / `/background list` list the workers.
//!
//! Files: `<nanopi home>/background/<session-id>.json` and `.log`.
//!
//! Nothing can be asked of the user in the background: sudo in "ask"
//! mode is refused there, as in any non-interactive run.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Notify};
use tokio_util::sync::CancellationToken;

use crate::agent::loop_::{Agent, Provider};
use crate::event::AgentEvent;
use crate::goal::{Goal, Verdict};
use crate::loops::{LoopState, Pace};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Spawned, not yet started its first step.
    Starting,
    /// Running a turn or an evaluation.
    Running,
    /// Waiting for the loop's next iteration (or a goal retry).
    Waiting,
    /// Nothing left to do: the goal resolved and/or the loop ended.
    Done,
    /// The goal paused (no progress, an error); needs the user.
    Paused,
    /// Stopped on request (`--foreground`), work left to continue.
    Stopped,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Starting => "starting",
            Status::Running => "running",
            Status::Waiting => "waiting",
            Status::Done => "done",
            Status::Paused => "paused",
            Status::Stopped => "stopped",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalSnap {
    pub condition: String,
    pub elapsed_secs: u64,
    pub turns_evaluated: u32,
    pub spent_tokens: u64,
    pub last_reason: Option<String>,
    pub paused: Option<String>,
    pub no_progress: u32,
    pub retries: u32,
    pub retry_in_secs: Option<u64>,
}

impl GoalSnap {
    pub fn of(g: &Goal, now: Instant, tokens_now: u64) -> Self {
        Self {
            condition: g.condition.clone(),
            elapsed_secs: now.duration_since(g.started).as_secs(),
            turns_evaluated: g.turns_evaluated,
            spent_tokens: g.spent(tokens_now),
            last_reason: g.last_reason.clone(),
            paused: g.paused.clone(),
            no_progress: g.no_progress,
            retries: g.retries,
            retry_in_secs: g.retry_at.map(|t| t.saturating_duration_since(now).as_secs()),
        }
    }

    /// Back to a live goal. `since_write` is how long ago the snapshot
    /// was written: the goal kept running (or waiting) meanwhile.
    pub fn to_goal(&self, now: Instant, since_write: Duration, tokens_now: u64, generation: u64) -> Goal {
        let elapsed = Duration::from_secs(self.elapsed_secs) + since_write;
        let mut g = Goal::new(self.condition.clone(), tokens_now, generation);
        g.started = now.checked_sub(elapsed).unwrap_or(now);
        g.turns_evaluated = self.turns_evaluated;
        g.eval_tokens = self.spent_tokens;
        g.last_reason = self.last_reason.clone();
        g.paused = self.paused.clone();
        g.no_progress = self.no_progress;
        g.retries = self.retries;
        g.retry_at = self
            .retry_in_secs
            .map(|s| now + Duration::from_secs(s).saturating_sub(since_write));
        g
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinishedSnap {
    pub condition: String,
    pub achieved: bool,
    pub reason: String,
    pub elapsed_secs: u64,
    pub turns: u32,
    pub tokens: u64,
}

impl FinishedSnap {
    pub fn of(f: &crate::goal::Finished) -> Self {
        Self {
            condition: f.condition.clone(),
            achieved: f.achieved,
            reason: f.reason.clone(),
            elapsed_secs: f.elapsed.as_secs(),
            turns: f.turns,
            tokens: f.tokens,
        }
    }

    pub fn to_finished(&self) -> crate::goal::Finished {
        crate::goal::Finished {
            condition: self.condition.clone(),
            achieved: self.achieved,
            reason: self.reason.clone(),
            elapsed: Duration::from_secs(self.elapsed_secs),
            turns: self.turns,
            tokens: self.tokens,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopSnap {
    pub prompt: Option<String>,
    /// `None` = self-paced.
    pub fixed_secs: Option<u64>,
    pub age_secs: u64,
    /// `None` while an iteration runs.
    pub next_in_secs: Option<u64>,
    pub iterations: u32,
    pub on_fallback: bool,
    pub last_reason: Option<String>,
}

impl LoopSnap {
    pub fn of(l: &LoopState, now: Instant) -> Self {
        Self {
            prompt: l.prompt.clone(),
            fixed_secs: match l.pace {
                Pace::Fixed(d) => Some(d.as_secs()),
                Pace::SelfPaced => None,
            },
            age_secs: now.duration_since(l.created).as_secs(),
            next_in_secs: l.next_fire.map(|t| t.saturating_duration_since(now).as_secs()),
            iterations: l.iterations,
            on_fallback: l.on_fallback,
            last_reason: l.last_reason.clone(),
        }
    }

    pub fn to_state(&self, now: Instant, since_write: Duration) -> LoopState {
        let pace = match self.fixed_secs {
            Some(s) => Pace::Fixed(Duration::from_secs(s)),
            None => Pace::SelfPaced,
        };
        let age = Duration::from_secs(self.age_secs) + since_write;
        let mut l = LoopState::new(self.prompt.clone(), pace, now);
        l.created = now.checked_sub(age).unwrap_or(now);
        // An iteration that was running when the snapshot was taken is
        // over by now; the next one is due at once.
        l.next_fire = Some(match self.next_in_secs {
            Some(s) => now + Duration::from_secs(s).saturating_sub(since_write),
            None => now,
        });
        l.iterations = self.iterations;
        l.on_fallback = self.on_fallback;
        l.last_reason = self.last_reason.clone();
        l
    }
}

/// Everything about one backgrounded session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BgState {
    pub session_id: String,
    pub session_path: PathBuf,
    pub cwd: PathBuf,
    pub profile: String,
    pub pid: Option<u32>,
    pub started_unix: u64,
    pub updated_unix: u64,
    pub status: Status,
    /// The latest progress line, for listings.
    pub note: Option<String>,
    pub goal: Option<GoalSnap>,
    pub goal_done: Option<FinishedSnap>,
    #[serde(rename = "loop")]
    pub loop_: Option<LoopSnap>,
    /// The last turn finished and has not been judged yet.
    pub needs_eval: bool,
    /// Tool calls of that turn (the goal's progress signal).
    pub last_tool_calls: usize,
    /// The next goal turn's prompt, when already decided.
    pub next_goal_prompt: Option<String>,
    /// Turns the worker has run.
    pub worker_turns: u32,
    pub log_path: PathBuf,
}

impl BgState {
    /// A goal that still drives turns.
    pub fn goal_live(&self) -> bool {
        self.goal.as_ref().is_some_and(|g| g.paused.is_none())
    }

    pub fn has_work(&self) -> bool {
        self.goal_live() || self.loop_.is_some()
    }

    pub fn since_write(&self) -> Duration {
        Duration::from_secs(unix_now().saturating_sub(self.updated_unix))
    }

    pub fn short_id(&self) -> &str {
        &self.session_id[..8.min(self.session_id.len())]
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn dir_in(home: &Path) -> PathBuf {
    home.join("background")
}

pub fn state_path_in(home: &Path, session_id: &str) -> PathBuf {
    dir_in(home).join(format!("{session_id}.json"))
}

pub fn log_path_in(home: &Path, session_id: &str) -> PathBuf {
    dir_in(home).join(format!("{session_id}.log"))
}

/// Write atomically (temp file + rename), mode 0600 in a 0700 folder.
pub fn write_in(home: &Path, st: &BgState) -> Result<(), String> {
    let dir = dir_in(home);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let path = state_path_in(home, &st.session_id);
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let mut st = st.clone();
    st.updated_unix = unix_now();
    let body = serde_json::to_string_pretty(&st).map_err(|e| e.to_string())?;
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        std::io::Write::write_all(&mut f, body.as_bytes()).map_err(|e| e.to_string())?;
    }
    std::fs::rename(&tmp, &path).map_err(|e| format!("write {}: {e}", path.display()))
}

pub fn read_in(home: &Path, session_id: &str) -> Result<BgState, String> {
    let p = state_path_in(home, session_id);
    let s = std::fs::read_to_string(&p).map_err(|e| format!("read {}: {e}", p.display()))?;
    serde_json::from_str(&s).map_err(|e| format!("parse {}: {e}", p.display()))
}

pub fn remove_in(home: &Path, session_id: &str) {
    let _ = std::fs::remove_file(state_path_in(home, session_id));
}

/// Every state file, newest first. Unreadable ones are skipped.
pub fn list_in(home: &Path) -> Vec<BgState> {
    let Ok(rd) = std::fs::read_dir(dir_in(home)) else { return Vec::new() };
    let mut out: Vec<BgState> = rd
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("json") {
                return None;
            }
            serde_json::from_str(&std::fs::read_to_string(&p).ok()?).ok()
        })
        .collect();
    out.sort_by_key(|s| std::cmp::Reverse(s.started_unix));
    out
}

/// Whether the worker process is still there.
pub fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: kill with signal 0 only checks for existence.
        let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
        r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

pub fn is_running(st: &BgState) -> bool {
    st.pid.is_some_and(pid_alive) && !matches!(st.status, Status::Done | Status::Paused | Status::Stopped)
}

/// Ask a worker to stop (first call: after the current turn; second:
/// at once).
pub fn signal_stop(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: plain kill(2).
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// Pick the state `--foreground` means: the given id (or prefix), else
/// the newest one started from `cwd`, else the only one there is.
pub fn pick(states: &[BgState], id: Option<&str>, cwd: &Path) -> Result<BgState, String> {
    if states.is_empty() {
        return Err("no background sessions".into());
    }
    if let Some(id) = id.filter(|s| !s.is_empty()) {
        let hits: Vec<&BgState> = states.iter().filter(|s| s.session_id.starts_with(id)).collect();
        return match hits.as_slice() {
            [one] => Ok((*one).clone()),
            [] => Err(format!("no background session matches `{id}`")),
            _ => Err(format!("`{id}` matches {} background sessions; give more of the id", hits.len())),
        };
    }
    if let Some(s) = states.iter().find(|s| s.cwd == cwd) {
        return Ok(s.clone());
    }
    if states.len() == 1 {
        return Ok(states[0].clone());
    }
    Err("several background sessions, none started in this directory; pick one with \
         `nanopi --foreground <id>` (list them with `nanopi --background`)"
        .into())
}

/// The listing for `nanopi --background` and `/background list`.
pub fn list_lines(states: &[BgState], now_unix: u64, alive: impl Fn(&BgState) -> bool) -> Vec<String> {
    if states.is_empty() {
        return vec!["no background sessions".into()];
    }
    let mut out = vec!["background sessions:".to_string()];
    for s in states {
        let status = if matches!(s.status, Status::Starting | Status::Running | Status::Waiting) && !alive(s) {
            "exited".to_string()
        } else {
            s.status.as_str().to_string()
        };
        let what = match (&s.goal, &s.loop_, &s.goal_done) {
            (Some(g), _, _) => format!("goal: {}", g.condition),
            (None, Some(l), _) => format!(
                "loop {}: {}",
                l.fixed_secs
                    .map(|x| format!("every {}", crate::loops::human(Duration::from_secs(x))))
                    .unwrap_or_else(|| "self-paced".into()),
                l.prompt.as_deref().unwrap_or("(loop.md / maintenance)")
            ),
            (None, None, Some(d)) => format!(
                "goal {}: {}",
                if d.achieved { "achieved" } else { "failed" },
                d.condition
            ),
            _ => "nothing left to run".into(),
        };
        out.push(format!(
            "  {}  {:<8} {}  ({} ago, {} turn(s)) {}",
            s.short_id(),
            status,
            clip(&what, 70),
            crate::goal::format_duration(Duration::from_secs(now_unix.saturating_sub(s.started_unix))),
            s.worker_turns,
            s.cwd.display()
        ));
        if let Some(n) = &s.note {
            out.push(format!("            {}", clip(n, 110)));
        }
    }
    out.push("  nanopi --foreground [id] brings one back · logs in ~/.nanopi/background/".into());
    out
}

fn clip(s: &str, n: usize) -> String {
    let one_line = s.replace('\n', " ");
    if one_line.chars().count() <= n {
        one_line
    } else {
        format!("{}…", one_line.chars().take(n).collect::<String>())
    }
}

/// Start the worker for `st` (already written) and return its pid. The
/// worker gets its own session (setsid), so closing the terminal does
/// not reach it, and writes everything to the log.
pub fn spawn_worker(st: &BgState) -> Result<u32, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find the nanopi binary: {e}"))?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&st.log_path)
        .map_err(|e| format!("open {}: {e}", st.log_path.display()))?;
    let log2 = log.try_clone().map_err(|e| e.to_string())?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--worker")
        .arg(&st.session_id)
        .current_dir(&st.cwd)
        .env(crate::profile::ENV, &st.profile)
        .env_remove("NANOPI_AGENT_ID")
        .env_remove("NANOPI_PARENT_PID")
        .stdin(std::process::Stdio::null())
        .stdout(log)
        .stderr(log2);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe and touches nothing else.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    let child = cmd.spawn().map_err(|e| format!("start the background worker: {e}"))?;
    Ok(child.id())
}

static RESTORE: Mutex<Option<BgState>> = Mutex::new(None);

/// `--foreground` hands the stopped worker's state to the TUI this way.
pub fn set_restore(st: BgState) {
    *RESTORE.lock().unwrap_or_else(|e| e.into_inner()) = Some(st);
}

pub fn take_restore() -> Option<BgState> {
    RESTORE.lock().unwrap_or_else(|e| e.into_inner()).take()
}

// ── The worker ─────────────────────────────────────────────────────────

/// SIGTERM handling: the first asks to stop after the current turn, the
/// second cancels it.
struct StopSignal {
    stop: Arc<AtomicBool>,
    notify: Arc<Notify>,
    cancel: CancellationToken,
}

impl StopSignal {
    fn install() -> Self {
        let s = StopSignal {
            stop: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(Notify::new()),
            cancel: CancellationToken::new(),
        };
        #[cfg(unix)]
        {
            let (stop, notify, cancel) = (s.stop.clone(), s.notify.clone(), s.cancel.clone());
            tokio::spawn(async move {
                use tokio::signal::unix::{signal, SignalKind};
                let Ok(mut term) = signal(SignalKind::terminate()) else { return };
                // Hang-ups are ignored: there is no terminal to lose.
                let _hup = signal(SignalKind::hangup());
                term.recv().await;
                eprintln!("[background] stop requested — finishing the current turn");
                stop.store(true, Ordering::SeqCst);
                notify.notify_waiters();
                term.recv().await;
                eprintln!("[background] stop requested again — cancelling the current turn");
                cancel.cancel();
                notify.notify_waiters();
            });
        }
        s
    }

    fn stopping(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// Sleep until `t`, returning early (false) on a stop request.
    async fn sleep_until(&self, t: Instant) -> bool {
        if self.stopping() {
            return false;
        }
        let d = t.saturating_duration_since(Instant::now());
        tokio::select! {
            _ = tokio::time::sleep(d) => !self.stopping(),
            _ = self.notify.notified() => false,
        }
    }
}

struct TurnOutcome {
    error: Option<String>,
    cancelled: bool,
    tool_calls: usize,
}

fn tokens(agent: &Agent) -> u64 {
    u64::from(agent.usage_total.input_tokens) + u64::from(agent.usage_total.output_tokens)
}

fn note(st: &mut BgState, text: impl Into<String>) {
    let text = text.into();
    eprintln!("[background] {text}");
    st.note = Some(text);
}

/// Run goal turns and loop iterations until nothing is left or a stop
/// is requested. `st` is the handed-over state; it is kept current in
/// the state file under `home` and returned at the end.
pub async fn run_worker(
    agent: &mut Agent,
    tx: &mpsc::Sender<AgentEvent>,
    evaluator: &dyn Provider,
    home: &Path,
    mut st: BgState,
) -> BgState {
    let sig = StopSignal::install();
    let now = Instant::now();
    let since = st.since_write();
    let mut goal: Option<Goal> = st.goal.take().map(|g| g.to_goal(now, since, tokens(agent), 1));
    let mut lp: Option<LoopState> = st.loop_.take().map(|l| l.to_state(now, since));
    st.pid = Some(std::process::id());

    let save = |st: &mut BgState, goal: &Option<Goal>, lp: &Option<LoopState>, agent: &Agent| {
        let now = Instant::now();
        st.goal = goal.as_ref().map(|g| GoalSnap::of(g, now, tokens(agent)));
        st.loop_ = lp.as_ref().map(|l| LoopSnap::of(l, now));
        if let Err(e) = write_in(home, st) {
            eprintln!("[background] cannot save state: {e}");
        }
    };

    st.status = Status::Running;
    note(&mut st, format!("worker {} started", std::process::id()));
    save(&mut st, &goal, &lp, agent);

    loop {
        if sig.stopping() {
            st.status = Status::Stopped;
            break;
        }
        // The loop tools follow the loop (`stop_loop` works from goal
        // turns too).
        if crate::loops::sync_tools(&mut agent.registry, lp.as_ref().map(|l| l.pace)) {
            agent.context.tools = agent.registry.all_specs();
        }
        if let Some(reason) = crate::loops::take_stop_request() {
            if let Some(l) = lp.take() {
                let why = reason.map(|r| format!(" — {r}")).unwrap_or_default();
                note(&mut st, format!("loop ended by the model after {} iteration(s){why}", l.iterations));
                save(&mut st, &goal, &lp, agent);
                continue;
            }
        }
        let goal_live = goal.as_ref().is_some_and(|g| g.paused.is_none());
        if goal_live {
            if st.needs_eval {
                st.needs_eval = false;
                st.status = Status::Running;
                judge_goal(agent, evaluator, &mut st, &mut goal).await;
                save(&mut st, &goal, &lp, agent);
                continue;
            }
            let g = goal.as_mut().expect("goal_live");
            if let Some(t) = g.retry_at {
                st.status = Status::Waiting;
                save(&mut st, &goal, &lp, agent);
                if !sig.sleep_until(t).await {
                    continue;
                }
                goal.as_mut().expect("goal_live").retry_at = None;
            }
            let g = goal.as_mut().expect("goal_live");
            let prompt = st.next_goal_prompt.take().unwrap_or_else(|| match &g.last_reason {
                Some(r) => crate::goal::continue_prompt(&g.condition, r),
                None => crate::goal::kickoff_prompt(&g.condition),
            });
            st.status = Status::Running;
            save(&mut st, &goal, &lp, agent);
            let out = run_one(agent, tx, &sig, &prompt).await;
            st.worker_turns += 1;
            if out.cancelled {
                // Redo this turn when the goal continues.
                st.next_goal_prompt = Some(prompt);
                st.status = Status::Stopped;
                break;
            }
            let g = goal.as_mut().expect("goal_live");
            match out.error {
                Some(e) => goal_turn_failed(&mut st, &mut goal, &e),
                None => {
                    st.needs_eval = true;
                    st.last_tool_calls = out.tool_calls;
                    g.retries = 0;
                }
            }
            save(&mut st, &goal, &lp, agent);
            continue;
        }
        if let Some(l) = lp.as_mut() {
            let now = Instant::now();
            if l.expired(now) {
                note(&mut st, format!("loop expired after 7 days ({} iterations)", l.iterations));
                lp = None;
                save(&mut st, &goal, &lp, agent);
                continue;
            }
            if !l.due(now) {
                let t = l.next_fire.unwrap_or(now);
                st.status = Status::Waiting;
                save(&mut st, &goal, &lp, agent);
                sig.sleep_until(t).await;
                continue;
            }
            let prompt = match &l.prompt {
                Some(p) => p.clone(),
                None => crate::loops::default_prompt(&st.cwd).0,
            };
            let self_paced = l.pace == Pace::SelfPaced;
            let _ = crate::loops::take_wakeup();
            l.fired(now);
            let text = crate::loops::iteration_prompt(&prompt, l.pace, l.iterations);
            let n = l.iterations;
            st.status = Status::Running;
            note(&mut st, format!("loop iteration {n}"));
            save(&mut st, &goal, &lp, agent);
            let out = run_one(agent, tx, &sig, &text).await;
            st.worker_turns += 1;
            let l = lp.as_mut().expect("loop");
            if out.cancelled {
                st.status = Status::Stopped;
                break;
            }
            if let Some(e) = &out.error {
                note(&mut st, format!("loop iteration {n} failed: {e}"));
            }
            if self_paced {
                let req = crate::loops::take_wakeup();
                if l.after_self_paced_iteration(req, Instant::now()) {
                    let left = l.next_fire.map(|t| t.saturating_duration_since(Instant::now())).unwrap_or_default();
                    let why = l.last_reason.clone().unwrap_or_default();
                    note(&mut st, format!("next iteration in {} — {why}", crate::loops::human(Duration::from_secs(left.as_secs()))));
                } else {
                    let why = l.last_reason.clone().map(|r| format!(" — {r}")).unwrap_or_default();
                    note(&mut st, format!("loop ended after {} iteration(s){why}", l.iterations));
                    lp = None;
                }
            } else if let Some(t) = l.next_fire {
                let left = t.saturating_duration_since(Instant::now());
                note(&mut st, format!("loop iteration {n} done; next in {}", crate::goal::format_duration(left)));
            }
            save(&mut st, &goal, &lp, agent);
            continue;
        }
        break;
    }
    if st.status != Status::Stopped {
        st.status = if goal.as_ref().is_some_and(|g| g.paused.is_some()) { Status::Paused } else { Status::Done };
        let msg = match (&goal, &st.goal_done) {
            (Some(g), _) => format!("goal paused: {}", g.paused.clone().unwrap_or_default()),
            (None, Some(d)) if d.achieved => format!("goal achieved: {}", d.reason),
            (None, Some(d)) => format!("goal failed: {}", d.reason),
            _ => st.note.clone().unwrap_or_else(|| "nothing left to run".into()),
        };
        note(&mut st, msg);
    } else {
        note(&mut st, "stopped on request");
    }
    save(&mut st, &goal, &lp, agent);
    st
}

/// One turn, plus one more for background-agent reports that arrive
/// before the next step (as print mode does).
async fn run_one(agent: &mut Agent, tx: &mpsc::Sender<AgentEvent>, sig: &StopSignal, prompt: &str) -> TurnOutcome {
    let start = agent.context.messages.len();
    let r = agent.run_turn(prompt, tx, Some(sig.cancel.child_token()), None).await;
    let mut error = r.err().map(|e| e.to_string());
    if !sig.cancel.is_cancelled() && error.is_none() {
        if let Some(reg) = crate::agent_registry::global() {
            if reg.has_background() {
                tokio::select! {
                    _ = reg.wait_background() => {}
                    _ = sig.notify.notified() => {}
                }
                if let Some(text) = reg.take_reports() {
                    if let Err(e) = agent.run_turn(&text, tx, Some(sig.cancel.child_token()), None).await {
                        error = Some(e.to_string());
                    }
                }
            }
        }
    }
    let from = if start <= agent.context.messages.len() { start } else { 0 };
    TurnOutcome {
        error,
        cancelled: sig.cancel.is_cancelled(),
        tool_calls: crate::goal::tool_calls_since(&agent.context, from),
    }
}

fn goal_turn_failed(st: &mut BgState, goal: &mut Option<Goal>, e: &str) {
    use crate::goal::{classify_error, retry_delay, ErrorKind, MAX_RETRIES};
    let Some(g) = goal.as_mut() else { return };
    match classify_error(e) {
        ErrorKind::Fatal => {
            note(st, format!("goal cleared after an unrecoverable error: {e}"));
            *goal = None;
        }
        ErrorKind::Pause => {
            g.paused = Some(format!("error: {e}"));
        }
        ErrorKind::Transient => {
            g.retries += 1;
            if g.retries > MAX_RETRIES {
                g.paused = Some(format!("{MAX_RETRIES} retries failed: {e}"));
            } else {
                let d = retry_delay(g.retries);
                g.retry_at = Some(Instant::now() + d);
                g.last_reason = Some(format!("the last turn failed: {e}"));
                st.next_goal_prompt = Some(crate::goal::retry_prompt(&g.condition, e));
                note(st, format!("turn failed, retrying in {} ({}/{MAX_RETRIES}): {e}", crate::goal::format_duration(d), g.retries));
            }
        }
    }
}

async fn judge_goal(agent: &mut Agent, evaluator: &dyn Provider, st: &mut BgState, goal: &mut Option<Goal>) {
    use crate::goal::NO_PROGRESS_LIMIT;
    let Some(g) = goal.as_mut() else { return };
    let start = crate::goal::kickoff_index(&agent.context, &g.condition);
    let transcript = crate::goal::transcript(&agent.context, start);
    match crate::goal::evaluate(evaluator, &g.condition, &transcript).await {
        Err(e) => {
            g.paused = Some(e.clone());
            note(st, format!("goal paused — {e}"));
        }
        Ok((eval, usage)) => {
            g.eval_tokens += u64::from(usage.input_tokens) + u64::from(usage.output_tokens);
            g.turns_evaluated += 1;
            let reason = if eval.reason.is_empty() { "(no reason given)".to_string() } else { eval.reason };
            g.last_reason = Some(reason.clone());
            match eval.verdict {
                Verdict::Met | Verdict::Impossible => {
                    let achieved = eval.verdict == Verdict::Met;
                    let done = crate::goal::Finished {
                        condition: g.condition.clone(),
                        achieved,
                        reason: reason.clone(),
                        elapsed: Instant::now().duration_since(g.started),
                        turns: g.turns_evaluated,
                        tokens: g.spent(tokens(agent)),
                    };
                    note(st, format!("goal {}: {reason}", if achieved { "achieved" } else { "judged impossible" }));
                    st.goal_done = Some(FinishedSnap::of(&done));
                    *goal = None;
                }
                Verdict::NotMet => {
                    g.no_progress = if st.last_tool_calls == 0 { g.no_progress + 1 } else { 0 };
                    if g.no_progress >= NO_PROGRESS_LIMIT {
                        g.paused = Some(format!("{NO_PROGRESS_LIMIT} turns in a row without tool use"));
                        note(st, format!("goal paused: no progress for {NO_PROGRESS_LIMIT} turns. Last check: {reason}"));
                    } else {
                        note(st, format!("not yet: {reason}"));
                        st.next_goal_prompt = Some(crate::goal::continue_prompt(&g.condition, &reason));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(id: &str, cwd: &str, started: u64) -> BgState {
        BgState {
            session_id: id.into(),
            session_path: PathBuf::from("/s"),
            cwd: PathBuf::from(cwd),
            profile: "default".into(),
            pid: None,
            started_unix: started,
            updated_unix: started,
            status: Status::Running,
            note: None,
            goal: None,
            goal_done: None,
            loop_: None,
            needs_eval: false,
            last_tool_calls: 0,
            next_goal_prompt: None,
            worker_turns: 0,
            log_path: PathBuf::from("/l"),
        }
    }

    #[test]
    fn goal_snapshot_round_trips_and_keeps_running_time() {
        let now = Instant::now();
        let mut g = Goal::new("tests pass".into(), 100, 3);
        g.started = now - Duration::from_secs(50);
        g.turns_evaluated = 2;
        g.eval_tokens = 7;
        g.last_reason = Some("r".into());
        g.no_progress = 1;
        g.retry_at = Some(now + Duration::from_secs(30));
        let snap = GoalSnap::of(&g, now, 150);
        assert_eq!(snap.elapsed_secs, 50);
        assert_eq!(snap.spent_tokens, 57);
        assert_eq!(snap.retry_in_secs, Some(30));
        // Restored 10 s later, in a process whose token count is 1000.
        let back = snap.to_goal(now, Duration::from_secs(10), 1000, 9);
        assert_eq!(now.duration_since(back.started).as_secs(), 60);
        assert_eq!(back.spent(1000), 57);
        assert_eq!(back.turns_evaluated, 2);
        assert_eq!(back.no_progress, 1);
        assert_eq!(back.generation, 9);
        assert_eq!(back.retry_at.unwrap().duration_since(now).as_secs(), 20);
    }

    #[test]
    fn loop_snapshot_round_trips() {
        let now = Instant::now();
        let mut l = LoopState::new(Some("p".into()), Pace::Fixed(Duration::from_secs(300)), now);
        l.fired(now);
        let snap = LoopSnap::of(&l, now);
        assert_eq!((snap.fixed_secs, snap.next_in_secs, snap.iterations), (Some(300), Some(300), 1));
        let back = snap.to_state(now, Duration::from_secs(100));
        assert_eq!(back.pace, Pace::Fixed(Duration::from_secs(300)));
        assert_eq!(back.next_fire.unwrap().duration_since(now).as_secs(), 200);
        assert_eq!(back.iterations, 1);
        // Overdue: due at once. Mid-iteration (None): due at once.
        assert!(snap.to_state(now, Duration::from_secs(1000)).due(now));
        let mut sp = LoopState::new(None, Pace::SelfPaced, now);
        sp.fired(now);
        let s = LoopSnap::of(&sp, now);
        assert_eq!((s.fixed_secs, s.next_in_secs), (None, None));
        assert!(s.to_state(now, Duration::ZERO).due(now));
    }

    #[test]
    fn state_file_write_read_list_remove() {
        let home = tempfile::tempdir().unwrap();
        let mut a = state("aaaa1111-x", "/w1", 10);
        a.goal = Some(GoalSnap::of(&Goal::new("c".into(), 0, 1), Instant::now(), 0));
        write_in(home.path(), &a).unwrap();
        write_in(home.path(), &state("bbbb2222-y", "/w2", 20)).unwrap();
        let back = read_in(home.path(), "aaaa1111-x").unwrap();
        assert_eq!(back.goal, a.goal);
        assert!(back.updated_unix > 0);
        let all = list_in(home.path());
        assert_eq!(all.iter().map(|s| s.short_id()).collect::<Vec<_>>(), ["bbbb2222", "aaaa1111"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let m = std::fs::metadata(state_path_in(home.path(), "aaaa1111-x")).unwrap().permissions().mode();
            assert_eq!(m & 0o777, 0o600);
        }
        remove_in(home.path(), "aaaa1111-x");
        assert_eq!(list_in(home.path()).len(), 1);
    }

    #[test]
    fn picking_for_foreground() {
        let states = vec![state("aaaa1111", "/w1", 30), state("aaab2222", "/w2", 20), state("cccc3333", "/w1", 10)];
        assert_eq!(pick(&states, Some("cccc"), Path::new("/x")).unwrap().session_id, "cccc3333");
        assert!(pick(&states, Some("aaa"), Path::new("/x")).unwrap_err().contains("matches 2"));
        assert!(pick(&states, Some("zz"), Path::new("/x")).is_err());
        // Newest from this directory.
        assert_eq!(pick(&states, None, Path::new("/w1")).unwrap().session_id, "aaaa1111");
        assert!(pick(&states, None, Path::new("/x")).is_err());
        assert_eq!(pick(&states[..1], None, Path::new("/x")).unwrap().session_id, "aaaa1111");
        assert!(pick(&[], None, Path::new("/x")).is_err());
    }

    #[test]
    fn listing() {
        assert_eq!(list_lines(&[], 0, |_| true), ["no background sessions"]);
        let mut s = state("aaaa1111-zz", "/w", 100);
        s.goal = Some(GoalSnap::of(&Goal::new("tests pass".into(), 0, 1), Instant::now(), 0));
        s.note = Some("not yet: 2 fail".into());
        let lines = list_lines(std::slice::from_ref(&s), 160, |_| true);
        assert!(lines[1].contains("aaaa1111") && lines[1].contains("running") && lines[1].contains("goal: tests pass"), "{lines:?}");
        assert!(lines[1].contains("1m00s ago"));
        assert!(lines[2].contains("not yet: 2 fail"));
        // A dead worker that never wrote a final status.
        assert!(list_lines(&[s], 160, |_| false)[1].contains("exited"));
    }

    #[test]
    fn work_left() {
        let mut s = state("a", "/w", 0);
        assert!(!s.has_work());
        let mut g = GoalSnap::of(&Goal::new("c".into(), 0, 1), Instant::now(), 0);
        s.goal = Some(g.clone());
        assert!(s.has_work());
        g.paused = Some("x".into());
        s.goal = Some(g);
        assert!(!s.has_work());
        s.loop_ = Some(LoopSnap::of(&LoopState::new(None, Pace::SelfPaced, Instant::now()), Instant::now()));
        assert!(s.has_work());
    }

    #[test]
    fn restore_slot_is_taken_once() {
        set_restore(state("r", "/w", 0));
        assert_eq!(take_restore().unwrap().session_id, "r");
        assert!(take_restore().is_none());
    }
}
