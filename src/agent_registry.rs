//! Parent-side bookkeeping for orchestrator-spawned `nanopi -p` children.
//!
//! Tracks every child (id, pid, state, start time, agent dir), enforces the
//! `max_live` hard cap and the `max_concurrency` semaphore, and provides
//! [`ChildGuard`], which SIGKILLs a child's whole process group on drop.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::archive;
use crate::config::AgentConfig;

/// Lifecycle state of a tracked child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Queued,
    Running,
    Completed,
    LimitReached,
    Failed,
    Stopped,
    /// Found non-terminal by a later run's startup scan (D-06); never set
    /// by the process that owns the agent.
    Interrupted,
}

impl AgentState {
    /// Terminal states no longer count against `max_live`.
    pub fn is_terminal(self) -> bool {
        !matches!(self, AgentState::Queued | AgentState::Running)
    }

    /// On-disk state name (D-05). The Rust variant name intentionally
    /// differs from `Completed`'s on-disk `"done"` to avoid churn.
    pub fn as_str(self) -> &'static str {
        match self {
            AgentState::Queued => "queued",
            AgentState::Running => "running",
            AgentState::Completed => "done",
            AgentState::LimitReached => "limit_reached",
            AgentState::Failed => "failed",
            AgentState::Stopped => "stopped",
            AgentState::Interrupted => "interrupted",
        }
    }

    /// Inverse of [`as_str`](Self::as_str): parse an on-disk state token
    /// (CTL-06 `adopt_from_disk`). `None` for anything unrecognized
    /// (including the accepted-but-never-set `waiting_permission`).
    pub fn from_disk_str(s: &str) -> Option<Self> {
        match s {
            "queued" => Some(AgentState::Queued),
            "running" => Some(AgentState::Running),
            "done" => Some(AgentState::Completed),
            "limit_reached" => Some(AgentState::LimitReached),
            "failed" => Some(AgentState::Failed),
            "stopped" => Some(AgentState::Stopped),
            "interrupted" => Some(AgentState::Interrupted),
            _ => None,
        }
    }
}

/// One tracked child.
#[derive(Debug, Clone)]
pub struct AgentEntry {
    pub id: String,
    pub pid: Option<u32>,
    pub state: AgentState,
    pub started: Instant,
    pub dir: PathBuf,
}

/// A tracked background dispatch: the task driving it and the token
/// that cancels it (`stop`/`stop_all`).
struct Background {
    handle: JoinHandle<()>,
    token: CancellationToken,
}

/// Registry of all children spawned in this nanopi run.
pub struct AgentRegistry {
    run_id: String,
    counter: AtomicU64,
    entries: Mutex<Vec<AgentEntry>>,
    max_live: usize,
    semaphore: Arc<Semaphore>,
    /// Background dispatches tracked by id (CTL-01). Kept out of
    /// `AgentEntry` so the entry stays `Clone`.
    background: Mutex<HashMap<String, Background>>,
    /// Finished-background reports awaiting injection (CTL-05). Each
    /// entry is already `[agent aN finished: <state>] <capped report>`;
    /// `take_reports` joins and clears them as ONE batched string
    /// (D-06).
    reports: Mutex<Vec<String>>,
    /// Wakes a consumer when a report is pushed. Only wakes — the text
    /// itself is pulled via `take_reports` so batching holds.
    notify_sink: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    /// Set once `reserve` (or `adopt_from_disk`) has scanned an existing
    /// run dir on disk to seed `counter` past any on-disk ids (CTL-06):
    /// a joining process must never mint an id that collides with one
    /// already on disk.
    seeded: AtomicBool,
    /// Set once this process has written `run.pid` for its run dir
    /// (CTL-06 T-04-06-05): written exactly once, whether the run dir is
    /// new or joined via `NANOPI_RUN_ID`.
    run_pid_written: AtomicBool,
    /// Set by the TUI: a single-mode `agent` call without an explicit
    /// `background` runs in the background, so the foreground
    /// conversation stays free while the agent works.
    background_default: AtomicBool,
}

impl std::fmt::Debug for AgentRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentRegistry")
            .field("run_id", &self.run_id)
            .field("max_live", &self.max_live)
            .finish()
    }
}

impl AgentRegistry {
    /// `run_id` is this process's own fresh id unless `NANOPI_RUN_ID` is
    /// set and shape-valid, in which case this process joins that run
    /// instead (CTL-06, D-05: "also works for agents from an earlier
    /// nanopi process in the same run").
    pub fn new(cfg: &AgentConfig) -> Arc<Self> {
        let run_id = Self::run_id_from_env_value(std::env::var("NANOPI_RUN_ID").ok().as_deref())
            .unwrap_or_else(archive::new_run_id);
        Self::with_run_id(cfg, run_id)
    }

    /// Build a registry bound to an explicit `run_id` (tests, and `new`'s
    /// env-join path).
    pub fn with_run_id(cfg: &AgentConfig, run_id: String) -> Arc<Self> {
        Arc::new(Self {
            run_id,
            counter: AtomicU64::new(0),
            entries: Mutex::new(Vec::new()),
            max_live: cfg.max_live,
            semaphore: Arc::new(Semaphore::new(cfg.max_concurrency.max(1))),
            background: Mutex::new(HashMap::new()),
            reports: Mutex::new(Vec::new()),
            notify_sink: Mutex::new(None),
            seeded: AtomicBool::new(false),
            background_default: AtomicBool::new(false),
            run_pid_written: AtomicBool::new(false),
        })
    }

    /// Validate a candidate `NANOPI_RUN_ID` value (pure, env-mutation-free
    /// so it's directly unit-testable): `Some` only when shape-valid per
    /// `archive::is_run_id_shaped` (T-04-06-02 path-traversal mitigation —
    /// a malformed value is never trusted as a directory component).
    /// A non-empty, rejected value logs a debug note without echoing more
    /// than 64 chars of it back.
    pub fn run_id_from_env_value(v: Option<&str>) -> Option<String> {
        let v = v?.trim();
        if v.is_empty() {
            return None;
        }
        if archive::is_run_id_shaped(v) {
            return Some(v.to_string());
        }
        let shown: String = v.chars().take(64).collect();
        crate::note!("nanopi: debug: ignoring malformed NANOPI_RUN_ID: {shown:?}");
        None
    }

    fn bg_lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Background>> {
        self.background.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn reports_lock(&self) -> std::sync::MutexGuard<'_, Vec<String>> {
        self.reports.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Register a background dispatch's task + cancellation token
    /// (CTL-01). Called by `spawn_background` right after
    /// `tokio::spawn`, without awaiting the handle.
    pub fn track_background(&self, id: &str, handle: JoinHandle<()>, token: CancellationToken) {
        self.bg_lock()
            .insert(id.to_string(), Background { handle, token });
    }

    /// Cancel `id`'s background run and kill its process group.
    /// `Err` for an unknown id or one already terminal.
    pub fn stop(&self, id: &str) -> Result<(), String> {
        let pid = {
            let entries = self.lock();
            let Some(e) = entries.iter().find(|e| e.id == id) else {
                return Err(format!("no such agent: {id}"));
            };
            if e.state.is_terminal() {
                return Err(format!("agent {id} is already {}", e.state.as_str()));
            }
            e.pid
        };
        if let Some(bg) = self.bg_lock().get(id) {
            bg.token.cancel();
        }
        if let Some(pid) = pid {
            kill_group(pid);
        }
        Ok(())
    }

    /// Stop every non-terminal entry (re-snapshotted under the lock).
    /// Returns the ids stopped. Races with agents that transition to
    /// terminal between the snapshot and the stop call are an accepted
    /// gap (research A3).
    pub fn stop_all(&self) -> Vec<String> {
        let ids: Vec<String> = self
            .lock()
            .iter()
            .filter(|e| !e.state.is_terminal())
            .map(|e| e.id.clone())
            .collect();
        ids.into_iter().filter(|id| self.stop(id).is_ok()).collect()
    }

    /// Move a terminal entry back to `Queued` in place (same dir), so it
    /// can be re-dispatched under the same id. `Err` for an unknown or
    /// non-terminal id.
    pub fn reactivate(&self, id: &str) -> Result<(), String> {
        let mut entries = self.lock();
        let Some(e) = entries.iter_mut().find(|e| e.id == id) else {
            return Err(format!("no such agent: {id}"));
        };
        if !e.state.is_terminal() {
            return Err(format!("agent {id} is not terminal ({})", e.state.as_str()));
        }
        e.state = AgentState::Queued;
        e.pid = None;
        Ok(())
    }

    /// Queue a finished-background report (D-06). `capped` must already
    /// be through `cap_report`. Wakes the installed sink exactly once.
    pub fn push_report(&self, id: &str, state: AgentState, capped: &str) {
        let entry = format!("[agent {id} finished: {}] {capped}", state.as_str());
        self.reports_lock().push(entry);
        if let Some(sink) = self.notify_sink.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            sink();
        }
    }

    /// Drain every pending report as ONE blank-line-joined string
    /// (D-06 batching). `None` when nothing is pending.
    pub fn take_reports(&self) -> Option<String> {
        let mut reports = self.reports_lock();
        if reports.is_empty() {
            return None;
        }
        let joined = reports.join("\n\n");
        reports.clear();
        Some(joined)
    }

    pub fn has_pending_reports(&self) -> bool {
        !self.reports_lock().is_empty()
    }

    /// Whether any background task is currently tracked (D-07: `-p`
    /// with zero background agents must skip the drain entirely rather
    /// than pay for a no-op `select!` and an unused `ctrl_c` listener).
    pub fn has_background(&self) -> bool {
        !self.bg_lock().is_empty()
    }

    /// Make background the default for single-mode dispatches.
    pub fn set_background_default(&self, on: bool) {
        self.background_default.store(on, Ordering::Relaxed);
    }

    pub fn background_default(&self) -> bool {
        self.background_default.load(Ordering::Relaxed)
    }

    /// Install the sink that wakes a consumer when a report arrives.
    /// Modelled on `plugin_send`'s installed-sink pattern.
    pub fn install_report_sink(&self, sink: Box<dyn Fn() + Send + Sync>) {
        *self.notify_sink.lock().unwrap_or_else(|e| e.into_inner()) = Some(sink);
    }

    /// Await every tracked background task, including ones registered
    /// while this call is running (loops until the map stays empty).
    pub async fn wait_background(&self) {
        loop {
            let handles: Vec<JoinHandle<()>> = {
                let mut bg = self.bg_lock();
                bg.drain().map(|(_, b)| b.handle).collect()
            };
            if handles.is_empty() {
                break;
            }
            for h in handles {
                let _ = h.await;
            }
        }
    }

    /// Run directory name (D-01: `YYYYMMDD-HHMMSS-<8 hex>`) shared by every
    /// child of this run.
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<AgentEntry>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Reserve a live slot: allocates the next id (`a1`, `a2`, ...) and
    /// creates `<agents_root>/<run_id>/<id>/` (mode 0700 on unix).
    ///
    /// On-disk aware (CTL-06): the first call seeds `counter` from the
    /// largest valid agent id already on disk under the run dir, so a
    /// process that joined an existing run (via `NANOPI_RUN_ID`) never
    /// mints an id that collides with one dispatched by an earlier
    /// process in the same run.
    pub fn reserve(&self, agents_root: &Path) -> Result<(String, PathBuf), String> {
        let mut entries = self.lock();
        let live = entries.iter().filter(|e| !e.state.is_terminal()).count();
        if live >= self.max_live {
            return Err(format!(
                "agent limit reached: {} live agents (max_live)",
                self.max_live
            ));
        }
        let run_dir = agents_root.join(&self.run_id);
        self.seed_counter_from_disk(&run_dir);
        let n = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
        let id = format!("a{n}");
        let dir = run_dir.join(&id);
        create_private_dir(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        self.write_run_pid_once(&run_dir);
        entries.push(AgentEntry {
            id: id.clone(),
            pid: None,
            state: AgentState::Queued,
            started: Instant::now(),
            dir: dir.clone(),
        });
        Ok((id, dir))
    }

    /// Scan `run_dir`'s existing subdirs once and `fetch_max` the id
    /// counter past the largest valid agent id found. No-op if `run_dir`
    /// doesn't exist yet or this has already run once for this registry.
    fn seed_counter_from_disk(&self, run_dir: &Path) {
        if self.seeded.swap(true, Ordering::SeqCst) {
            return;
        }
        let Ok(rd) = std::fs::read_dir(run_dir) else {
            return;
        };
        let mut max_n = 0u64;
        for e in rd.filter_map(|e| e.ok()) {
            if let Some(name) = e.file_name().to_str() {
                if let Some(n) = valid_agent_id(name) {
                    max_n = max_n.max(n);
                }
            }
        }
        self.counter.fetch_max(max_n, Ordering::SeqCst);
    }

    /// Write `run_dir/run.pid` exactly once per registry (first caller —
    /// `reserve` or `adopt_from_disk` — wins), so a process that only
    /// adopts (never reserves) still marks a joined run live (T-04-06-05).
    fn write_run_pid_once(&self, run_dir: &Path) {
        if self.run_pid_written.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Err(e) = archive::write_run_pid(run_dir) {
            crate::note!("nanopi: debug: write_run_pid({}): {e}", run_dir.display());
        }
    }

    /// Reconstruct an `AgentEntry` for a finished agent dispatched by an
    /// earlier process in this same run (CTL-06/D-05):
    /// `agents_root/<run_id>/<id>/brief.md` (falling back to
    /// `report.md`'s front matter for `state` if brief.md lacks it) must
    /// show a terminal, non-`Interrupted` state. Idempotent: an id
    /// already tracked in memory is returned unchanged, no disk access.
    ///
    /// `id` must match `a[1-9][0-9]{0,18}` (or bare `a0`, i.e. `valid_agent_id`)
    /// before any path is built from it (T-04-06-01): this is the only
    /// path through which a model-supplied string reaches a filesystem
    /// path here, so validation happens first and unconditionally.
    pub fn adopt_from_disk(&self, agents_root: &Path, id: &str) -> Result<AgentEntry, String> {
        if let Some(e) = self.lock().iter().find(|e| e.id == id) {
            return Ok(e.clone());
        }
        let Some(n) = valid_agent_id(id) else {
            return Err(format!("no such agent: {id}"));
        };
        let run_dir = agents_root.join(&self.run_id);
        let dir = run_dir.join(id);
        let Ok(brief) = std::fs::read_to_string(dir.join("brief.md")) else {
            return Err(format!("no such agent: {id}"));
        };
        if crate::agent::brief::front_matter_get(&brief, "id").as_deref() != Some(id) {
            return Err(format!("no such agent: {id}"));
        }
        let state_str = crate::agent::brief::front_matter_get(&brief, "state").or_else(|| {
            std::fs::read_to_string(dir.join("report.md"))
                .ok()
                .and_then(|r| crate::agent::brief::front_matter_get(&r, "state"))
        });
        let Some(state_str) = state_str else {
            return Err(format!("no such agent: {id}"));
        };
        let Some(state) = AgentState::from_disk_str(&state_str) else {
            return Err(format!("no such agent: {id}"));
        };
        if !state.is_terminal() || state == AgentState::Interrupted {
            return Err(format!("agent {id} is not continuable (state: {state_str})"));
        }

        let entry = {
            let mut entries = self.lock();
            if let Some(e) = entries.iter().find(|e| e.id == id) {
                e.clone()
            } else {
                let e = AgentEntry {
                    id: id.to_string(),
                    pid: None,
                    state,
                    started: Instant::now(),
                    dir: dir.clone(),
                };
                entries.push(e.clone());
                e
            }
        };
        // Full disk scan (not just this id): a joined run dir may already
        // hold other on-disk ids (e.g. a1..a3) that were never adopted —
        // `reserve` must still never reuse any of them.
        self.seed_counter_from_disk(&run_dir);
        self.counter.fetch_max(n, Ordering::SeqCst);
        self.write_run_pid_once(&run_dir);
        Ok(entry)
    }

    /// Wait for a concurrency permit; hold it while the child runs.
    pub async fn acquire_run(&self) -> OwnedSemaphorePermit {
        Arc::clone(&self.semaphore)
            .acquire_owned()
            .await
            .expect("agent semaphore never closed")
    }

    pub fn set_pid(&self, id: &str, pid: u32) {
        if let Some(e) = self.lock().iter_mut().find(|e| e.id == id) {
            e.pid = Some(pid);
        }
    }

    pub fn set_state(&self, id: &str, state: AgentState) {
        let dir = {
            let mut entries = self.lock();
            let Some(e) = entries.iter_mut().find(|e| e.id == id) else {
                return;
            };
            e.state = state;
            e.dir.clone()
        };
        if let Err(e) = archive::set_agent_state(&dir, state.as_str()) {
            crate::note!("nanopi: debug: set_agent_state({}): {e}", dir.display());
        }
    }

    pub fn snapshot(&self) -> Vec<AgentEntry> {
        self.lock().clone()
    }

    /// SIGKILL the process group of every Running child (used on exit).
    pub fn kill_all(&self) {
        let mut entries = self.lock();
        for e in entries.iter_mut() {
            if e.state == AgentState::Running {
                if let Some(pid) = e.pid {
                    kill_group(pid);
                }
                e.state = AgentState::Stopped;
            }
        }
    }
}

/// Parse a registry agent id (`a` + 1-19 ASCII digits, no leading zero)
/// into its numeric suffix. `None` for anything else, including path
/// traversal attempts like `"../x"`, `"a1/../a2"`, bare `"a"`, or a
/// non-`a`-prefixed name (T-04-06-01): the only place a model-supplied
/// id is turned into a path component, so this must reject before any
/// `Path::join`.
fn valid_agent_id(id: &str) -> Option<u64> {
    let rest = id.strip_prefix('a')?;
    if rest.is_empty() || rest.len() > 19 || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if rest.len() > 1 && rest.as_bytes()[0] == b'0' {
        return None;
    }
    rest.parse::<u64>().ok()
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

/// SIGKILL a process group. ESRCH (already gone) is ignored. The pid-reuse
/// window between child exit and this call is accepted (research A1).
fn kill_group(pgid: u32) {
    #[cfg(unix)]
    {
        if pgid > 1 {
            // SAFETY: plain syscall; no memory is touched.
            unsafe {
                libc::killpg(pgid as libc::pid_t, libc::SIGKILL);
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pgid;
    }
}

/// Kills the child's whole process group when dropped.
#[derive(Debug)]
pub struct ChildGuard {
    pgid: Option<u32>,
}

impl ChildGuard {
    /// `pgid` is the child's pid when spawned with `process_group(0)`.
    pub fn new(pgid: Option<u32>) -> Self {
        Self { pgid }
    }

    /// Disarm after the child has been reaped normally.
    pub fn disarm(&mut self) {
        self.pgid = None;
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(pgid) = self.pgid.take() {
            kill_group(pgid);
        }
    }
}

static GLOBAL: OnceLock<Arc<AgentRegistry>> = OnceLock::new();

/// Install the process-wide registry (first call wins).
pub fn set_global(reg: Arc<AgentRegistry>) {
    let _ = GLOBAL.set(reg);
}

/// Process-wide registry, if one was installed (for exit-time kill_all).
pub fn global() -> Option<Arc<AgentRegistry>> {
    GLOBAL.get().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn cfg(max_live: usize, max_concurrency: usize) -> AgentConfig {
        AgentConfig {
            max_live,
            max_concurrency,
            ..AgentConfig::default()
        }
    }

    #[test]
    fn run_id_matches_archive_format_and_ids_sequential() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = AgentRegistry::new(&cfg(8, 4));
        let id = reg.run_id();
        assert!(
            id.len() == 24
                && id.as_bytes()[8] == b'-'
                && id[..8].bytes().all(|b| b.is_ascii_digit())
                && id[9..15].bytes().all(|b| b.is_ascii_digit())
                && id[16..].bytes().all(|b| b.is_ascii_hexdigit()),
            "run id {id} does not match YYYYMMDD-HHMMSS-<8 hex>"
        );
        for want in ["a1", "a2", "a3"] {
            let (id, dir) = reg.reserve(tmp.path()).unwrap();
            assert_eq!(id, want);
            assert!(dir.is_dir());
            assert_eq!(dir, tmp.path().join(reg.run_id()).join(want));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let m = std::fs::metadata(tmp.path().join(reg.run_id()).join("a1"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(m & 0o777, 0o700);
        }
    }

    #[test]
    fn max_live_cap_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = AgentRegistry::new(&cfg(2, 4));
        reg.reserve(tmp.path()).unwrap();
        reg.reserve(tmp.path()).unwrap();
        let err = reg.reserve(tmp.path()).unwrap_err();
        assert_eq!(err, "agent limit reached: 2 live agents (max_live)");
    }

    #[test]
    fn finishing_frees_live_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = AgentRegistry::new(&cfg(1, 4));
        let (id, _) = reg.reserve(tmp.path()).unwrap();
        assert!(reg.reserve(tmp.path()).is_err());
        reg.set_state(&id, AgentState::Completed);
        let (id2, _) = reg.reserve(tmp.path()).unwrap();
        assert_eq!(id2, "a2");
        let snap = reg.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].state, AgentState::Completed);
    }

    #[test]
    fn reserve_writes_run_pid_once() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = AgentRegistry::new(&cfg(8, 4));
        let (_, _) = reg.reserve(tmp.path()).unwrap();
        let run_dir = tmp.path().join(reg.run_id());
        assert!(run_dir.join("run.pid").is_file());
        let pid: u32 = std::fs::read_to_string(run_dir.join("run.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(pid, std::process::id());
    }

    #[test]
    fn set_state_persists_to_brief_and_index() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = AgentRegistry::new(&cfg(8, 4));
        let (id, dir) = reg.reserve(tmp.path()).unwrap();
        let brief = crate::agent::brief::render_brief_with_meta(
            &crate::agent::brief::BriefSpec::default(),
            &crate::agent::brief::BriefMeta {
                id: id.clone(),
                state: "queued".into(),
                started: "2024-01-01T00:00:00Z".into(),
                parent: reg.run_id().to_string(),
                label: None,
                worktree: None,
                branch: None,
            },
        );
        std::fs::write(dir.join("brief.md"), brief).unwrap();
        reg.set_state(&id, AgentState::Completed);
        let content = std::fs::read_to_string(dir.join("brief.md")).unwrap();
        assert_eq!(
            crate::agent::brief::front_matter_get(&content, "state").as_deref(),
            Some("done")
        );
        let index = std::fs::read_to_string(tmp.path().join(reg.run_id()).join("index.md"))
            .unwrap();
        assert!(index.contains(&id));
        assert!(index.contains("done"));
    }

    #[tokio::test]
    async fn concurrency_semaphore_blocks_second() {
        let reg = AgentRegistry::new(&cfg(8, 1));
        let p1 = reg.acquire_run().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), reg.acquire_run())
                .await
                .is_err()
        );
        drop(p1);
        assert!(
            tokio::time::timeout(Duration::from_millis(500), reg.acquire_run())
                .await
                .is_ok()
        );
    }

    #[cfg(unix)]
    fn spawn_group(pidfile: &Path) -> std::process::Child {
        use std::os::unix::process::CommandExt;
        std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("sleep 300 & echo $! > {}; wait", pidfile.display()))
            .process_group(0)
            .spawn()
            .unwrap()
    }

    #[cfg(unix)]
    fn read_pid(pidfile: &Path) -> i32 {
        for _ in 0..200 {
            if let Ok(s) = std::fs::read_to_string(pidfile) {
                if let Ok(p) = s.trim().parse() {
                    return p;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("pidfile never written");
    }

    #[cfg(unix)]
    fn wait_gone(pid: i32) -> bool {
        for _ in 0..200 {
            // SAFETY: signal 0 only probes existence.
            if unsafe { libc::kill(pid, 0) } != 0 {
                return true;
            }
            // An orphaned, killed process may linger as a zombie if
            // PID 1 doesn't reap (containers); that still counts as dead.
            if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                if stat
                    .rsplit(')')
                    .next()
                    .map(|r| r.trim_start().starts_with('Z'))
                    == Some(true)
                {
                    return true;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    #[cfg(unix)]
    #[test]
    fn child_guard_drop_kills_group() {
        let tmp = tempfile::tempdir().unwrap();
        let pf = tmp.path().join("pid");
        let mut child = spawn_group(&pf);
        let sleep_pid = read_pid(&pf);
        drop(ChildGuard::new(Some(child.id())));
        child.wait().unwrap();
        assert!(wait_gone(sleep_pid), "backgrounded sleep survived");
    }

    #[cfg(unix)]
    #[test]
    fn kill_all_kills_registered_groups() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = AgentRegistry::new(&cfg(8, 4));
        let mut kids = Vec::new();
        for i in 0..2 {
            let (id, _) = reg.reserve(tmp.path()).unwrap();
            let pf = tmp.path().join(format!("pid{i}"));
            let child = spawn_group(&pf);
            reg.set_pid(&id, child.id());
            reg.set_state(&id, AgentState::Running);
            kids.push((child, read_pid(&pf)));
        }
        reg.kill_all();
        for (mut child, sleep_pid) in kids {
            child.wait().unwrap();
            assert!(wait_gone(sleep_pid), "backgrounded sleep survived");
        }
        assert!(reg
            .snapshot()
            .iter()
            .all(|e| e.state == AgentState::Stopped));
    }

    // --- CTL-06 gap closure (plan 04-06) ---

    fn write_brief_state(dir: &Path, id: &str, state: &str) {
        std::fs::create_dir_all(dir).unwrap();
        let brief = crate::agent::brief::render_brief_with_meta(
            &crate::agent::brief::BriefSpec::default(),
            &crate::agent::brief::BriefMeta {
                id: id.into(),
                state: state.into(),
                started: "2026-01-01T00:00:00Z".into(),
                parent: "run".into(),
                label: None,
                worktree: None,
                branch: None,
            },
        );
        std::fs::write(dir.join("brief.md"), brief).unwrap();
    }

    #[test]
    fn with_run_id_uses_exact_string() {
        let reg = AgentRegistry::with_run_id(&cfg(8, 4), "20261004-101010-deadbeef".to_string());
        assert_eq!(reg.run_id(), "20261004-101010-deadbeef");
    }

    #[test]
    fn run_id_from_env_value_accepts_shaped_rejects_garbage() {
        assert_eq!(
            AgentRegistry::run_id_from_env_value(Some("20261004-101010-deadbeef")),
            Some("20261004-101010-deadbeef".to_string())
        );
        assert_eq!(AgentRegistry::run_id_from_env_value(Some("garbage")), None);
        assert_eq!(AgentRegistry::run_id_from_env_value(Some("")), None);
        assert_eq!(AgentRegistry::run_id_from_env_value(Some("   ")), None);
        assert_eq!(AgentRegistry::run_id_from_env_value(None), None);
        assert_eq!(
            AgentRegistry::run_id_from_env_value(Some("../../etc/passwd")),
            None
        );
    }

    #[test]
    fn adopt_from_disk_reconstructs_terminal_entry_then_reactivate_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = AgentRegistry::with_run_id(&cfg(8, 4), "run1".to_string());
        let run_dir = tmp.path().join("run1");
        write_brief_state(&run_dir.join("a1"), "a1", "done");

        let entry = reg.adopt_from_disk(tmp.path(), "a1").unwrap();
        assert_eq!(entry.id, "a1");
        assert_eq!(entry.state, AgentState::Completed);
        assert_eq!(entry.pid, None);
        assert_eq!(entry.dir, run_dir.join("a1"));

        let snap = reg.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].id, "a1");

        reg.reactivate("a1").unwrap();
    }

    #[test]
    fn adopt_from_disk_rejects_non_terminal_states() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = AgentRegistry::with_run_id(&cfg(8, 4), "run1".to_string());
        let run_dir = tmp.path().join("run1");
        for (id, state) in [("a1", "running"), ("a2", "queued"), ("a3", "interrupted")] {
            write_brief_state(&run_dir.join(id), id, state);
            let err = reg.adopt_from_disk(tmp.path(), id).unwrap_err();
            assert!(err.contains("not continuable"), "{err}");
            assert!(err.contains(state), "{err}");
            assert!(reg.snapshot().iter().all(|e| e.id != id), "must not insert {id}");
        }
    }

    #[test]
    fn adopt_from_disk_missing_or_malformed_is_no_such_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = AgentRegistry::with_run_id(&cfg(8, 4), "run1".to_string());
        let run_dir = tmp.path().join("run1");

        // missing dir entirely
        assert_eq!(
            reg.adopt_from_disk(tmp.path(), "a9").unwrap_err(),
            "no such agent: a9"
        );

        // dir exists, no brief.md
        std::fs::create_dir_all(run_dir.join("a8")).unwrap();
        assert_eq!(
            reg.adopt_from_disk(tmp.path(), "a8").unwrap_err(),
            "no such agent: a8"
        );

        // brief.md without front matter
        std::fs::create_dir_all(run_dir.join("a7")).unwrap();
        std::fs::write(run_dir.join("a7").join("brief.md"), "no front matter here").unwrap();
        assert_eq!(
            reg.adopt_from_disk(tmp.path(), "a7").unwrap_err(),
            "no such agent: a7"
        );

        // front-matter id mismatch (brief for a6 claims to be a5)
        write_brief_state(&run_dir.join("a6"), "a5", "done");
        assert_eq!(
            reg.adopt_from_disk(tmp.path(), "a6").unwrap_err(),
            "no such agent: a6"
        );

        assert!(reg.snapshot().is_empty());
    }

    #[test]
    fn adopt_from_disk_rejects_path_traversal_shaped_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = AgentRegistry::with_run_id(&cfg(8, 4), "run1".to_string());
        for bad in ["../x", "a", "b1", "a1/../a2", "", "a01", "a-1"] {
            let err = reg.adopt_from_disk(tmp.path(), bad).unwrap_err();
            assert_eq!(err, format!("no such agent: {bad}"), "id={bad:?}");
        }
    }

    #[test]
    fn adopt_from_disk_is_idempotent_for_already_tracked_id() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = AgentRegistry::with_run_id(&cfg(8, 4), "run1".to_string());
        let run_dir = tmp.path().join("run1");
        write_brief_state(&run_dir.join("a1"), "a1", "done");

        let e1 = reg.adopt_from_disk(tmp.path(), "a1").unwrap();
        let e2 = reg.adopt_from_disk(tmp.path(), "a1").unwrap();
        assert_eq!(e1.id, e2.id);
        assert_eq!(reg.snapshot().len(), 1, "no duplicate insert");
    }

    #[test]
    fn reserve_after_adopt_never_reuses_on_disk_id() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = AgentRegistry::with_run_id(&cfg(8, 4), "run1".to_string());
        let run_dir = tmp.path().join("run1");
        write_brief_state(&run_dir.join("a1"), "a1", "done");
        write_brief_state(&run_dir.join("a2"), "a2", "done");
        write_brief_state(&run_dir.join("a3"), "a3", "done");

        reg.adopt_from_disk(tmp.path(), "a3").unwrap();
        let (id, _) = reg.reserve(tmp.path()).unwrap();
        assert_eq!(id, "a4", "must skip every on-disk id, not just the adopted one");
    }

    #[test]
    fn reserve_seeds_from_joined_run_dir_without_any_adopt() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = AgentRegistry::with_run_id(&cfg(8, 4), "run1".to_string());
        let run_dir = tmp.path().join("run1");
        write_brief_state(&run_dir.join("a1"), "a1", "done");
        write_brief_state(&run_dir.join("a2"), "a2", "running");

        let (id, _) = reg.reserve(tmp.path()).unwrap();
        assert_eq!(id, "a3");
    }

    #[test]
    fn adopt_from_disk_falls_back_to_report_state_when_brief_lacks_it() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = AgentRegistry::with_run_id(&cfg(8, 4), "run1".to_string());
        let run_dir = tmp.path().join("run1").join("a1");
        std::fs::create_dir_all(&run_dir).unwrap();
        // brief.md has the id but no state (legacy-shaped front matter).
        std::fs::write(run_dir.join("brief.md"), "---\nid: a1\n---\ntask\n").unwrap();
        std::fs::write(run_dir.join("report.md"), "---\nstate: done\n---\nreport body\n").unwrap();

        let entry = reg.adopt_from_disk(tmp.path(), "a1").unwrap();
        assert_eq!(entry.state, AgentState::Completed);
    }

    #[test]
    fn global_roundtrip() {
        let reg = AgentRegistry::new(&cfg(8, 4));
        set_global(Arc::clone(&reg));
        assert!(global().is_some());
    }

    #[tokio::test]
    async fn push_report_batches_into_one_string_then_drains() {
        let reg = AgentRegistry::new(&cfg(8, 4));
        reg.push_report("a1", AgentState::Completed, "first report");
        reg.push_report("a2", AgentState::Failed, "second report");
        let batch = reg.take_reports().expect("one batched string");
        assert!(batch.contains("[agent a1 finished: done] first report"));
        assert!(batch.contains("[agent a2 finished: failed] second report"));
        assert!(reg.take_reports().is_none(), "drained, so None next time");
    }

    #[test]
    fn push_report_notifies_sink_once_per_push() {
        let reg = AgentRegistry::new(&cfg(8, 4));
        let count = Arc::new(AtomicU64::new(0));
        let c2 = Arc::clone(&count);
        reg.install_report_sink(Box::new(move || {
            c2.fetch_add(1, Ordering::SeqCst);
        }));
        reg.push_report("a1", AgentState::Completed, "r1");
        reg.push_report("a2", AgentState::Completed, "r2");
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn reactivate_moves_terminal_entry_back_to_queued() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = AgentRegistry::new(&cfg(8, 4));
        let (id, dir) = reg.reserve(tmp.path()).unwrap();
        reg.set_state(&id, AgentState::Completed);
        reg.reactivate(&id).unwrap();
        let snap = reg.snapshot();
        let e = snap.iter().find(|e| e.id == id).unwrap();
        assert_eq!(e.state, AgentState::Queued);
        assert_eq!(e.dir, dir, "reactivated in place, same dir");
    }

    #[test]
    fn reactivate_rejects_unknown_or_non_terminal() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = AgentRegistry::new(&cfg(8, 4));
        assert!(reg.reactivate("a1").is_err(), "unknown id");
        let (id, _) = reg.reserve(tmp.path()).unwrap();
        assert!(reg.reactivate(&id).is_err(), "still Queued, non-terminal");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stop_cancels_token_and_kills_group() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = AgentRegistry::new(&cfg(8, 4));
        let (id, _) = reg.reserve(tmp.path()).unwrap();
        let pf = tmp.path().join("pid");
        let child = spawn_group(&pf);
        let sleep_pid = read_pid(&pf);
        reg.set_pid(&id, child.id());
        reg.set_state(&id, AgentState::Running);

        let token = CancellationToken::new();
        let t2 = token.clone();
        let handle = tokio::spawn(async move {
            t2.cancelled().await;
        });
        reg.track_background(&id, handle, token.clone());

        reg.stop(&id).unwrap();
        assert!(token.is_cancelled(), "stop must cancel the token");
        assert!(wait_gone(sleep_pid as i32), "stop must kill the group");

        let mut child = child;
        let _ = child.wait();
    }

    #[test]
    fn stop_rejects_unknown_or_terminal_id() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = AgentRegistry::new(&cfg(8, 4));
        assert!(reg.stop("a1").is_err(), "unknown id");
        let (id, _) = reg.reserve(tmp.path()).unwrap();
        reg.set_state(&id, AgentState::Completed);
        assert!(reg.stop(&id).is_err(), "already terminal");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stop_all_stops_every_non_terminal_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = AgentRegistry::new(&cfg(8, 4));
        let mut pids = Vec::new();
        for i in 0..2 {
            let (id, _) = reg.reserve(tmp.path()).unwrap();
            let pf = tmp.path().join(format!("pid{i}"));
            let child = spawn_group(&pf);
            let sleep_pid = read_pid(&pf);
            reg.set_pid(&id, child.id());
            reg.set_state(&id, AgentState::Running);
            let token = CancellationToken::new();
            let t2 = token.clone();
            let handle = tokio::spawn(async move {
                t2.cancelled().await;
            });
            reg.track_background(&id, handle, token);
            pids.push((child, sleep_pid));
        }
        let stopped = reg.stop_all();
        assert_eq!(stopped.len(), 2);
        for (mut child, sleep_pid) in pids {
            assert!(wait_gone(sleep_pid as i32));
            let _ = child.wait();
        }
    }

    #[tokio::test]
    async fn wait_background_awaits_tracked_and_newly_registered_handles() {
        let reg = AgentRegistry::new(&cfg(8, 4));
        let ran = Arc::new(AtomicU64::new(0));
        let r2 = Arc::clone(&ran);
        let token1 = CancellationToken::new();
        let h1 = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            r2.fetch_add(1, Ordering::SeqCst);
        });
        reg.track_background("a1", h1, token1);

        // Register a second handle slightly later, from another task, to
        // exercise the "registered during the wait" guarantee.
        let reg2 = Arc::clone(&reg);
        let r3 = Arc::clone(&ran);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            let token2 = CancellationToken::new();
            let h2 = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(30)).await;
                r3.fetch_add(1, Ordering::SeqCst);
            });
            reg2.track_background("a2", h2, token2);
        });

        reg.wait_background().await;
        assert_eq!(ran.load(Ordering::SeqCst), 2, "both handles must be awaited");
    }
}
