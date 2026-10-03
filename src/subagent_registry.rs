//! Parent-side bookkeeping for orchestrator-spawned `nanopi -p` children.
//!
//! Tracks every child (id, pid, state, start time, agent dir), enforces the
//! `max_live` hard cap and the `max_concurrency` semaphore, and provides
//! [`ChildGuard`], which SIGKILLs a child's whole process group on drop.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::config::SubagentConfig;

/// Lifecycle state of a tracked child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Queued,
    Running,
    Completed,
    LimitReached,
    Failed,
    Stopped,
}

impl AgentState {
    /// Terminal states no longer count against `max_live`.
    pub fn is_terminal(self) -> bool {
        !matches!(self, AgentState::Queued | AgentState::Running)
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

/// Registry of all children spawned in this nanopi run.
#[derive(Debug)]
pub struct SubagentRegistry {
    run_id: String,
    counter: AtomicU64,
    entries: Mutex<Vec<AgentEntry>>,
    max_live: usize,
    semaphore: Arc<Semaphore>,
}

impl SubagentRegistry {
    pub fn new(cfg: &SubagentConfig) -> Arc<Self> {
        Arc::new(Self {
            run_id: uuid::Uuid::now_v7().to_string(),
            counter: AtomicU64::new(0),
            entries: Mutex::new(Vec::new()),
            max_live: cfg.max_live,
            semaphore: Arc::new(Semaphore::new(cfg.max_concurrency.max(1))),
        })
    }

    /// Run directory name (uuid v7) shared by every child of this run.
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<AgentEntry>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Reserve a live slot: allocates the next id (`a1`, `a2`, ...) and
    /// creates `<agents_root>/<run_id>/<id>/` (mode 0700 on unix).
    pub fn reserve(&self, agents_root: &Path) -> Result<(String, PathBuf), String> {
        let mut entries = self.lock();
        let live = entries.iter().filter(|e| !e.state.is_terminal()).count();
        if live >= self.max_live {
            return Err(format!(
                "subagent limit reached: {} live agents (max_live)",
                self.max_live
            ));
        }
        let n = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
        let id = format!("a{n}");
        let dir = agents_root.join(&self.run_id).join(&id);
        create_private_dir(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        entries.push(AgentEntry {
            id: id.clone(),
            pid: None,
            state: AgentState::Queued,
            started: Instant::now(),
            dir: dir.clone(),
        });
        Ok((id, dir))
    }

    /// Wait for a concurrency permit; hold it while the child runs.
    pub async fn acquire_run(&self) -> OwnedSemaphorePermit {
        Arc::clone(&self.semaphore)
            .acquire_owned()
            .await
            .expect("subagent semaphore never closed")
    }

    pub fn set_pid(&self, id: &str, pid: u32) {
        if let Some(e) = self.lock().iter_mut().find(|e| e.id == id) {
            e.pid = Some(pid);
        }
    }

    pub fn set_state(&self, id: &str, state: AgentState) {
        if let Some(e) = self.lock().iter_mut().find(|e| e.id == id) {
            e.state = state;
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

static GLOBAL: OnceLock<Arc<SubagentRegistry>> = OnceLock::new();

/// Install the process-wide registry (first call wins).
pub fn set_global(reg: Arc<SubagentRegistry>) {
    let _ = GLOBAL.set(reg);
}

/// Process-wide registry, if one was installed (for exit-time kill_all).
pub fn global() -> Option<Arc<SubagentRegistry>> {
    GLOBAL.get().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn cfg(max_live: usize, max_concurrency: usize) -> SubagentConfig {
        SubagentConfig {
            max_live,
            max_concurrency,
            ..SubagentConfig::default()
        }
    }

    #[test]
    fn run_id_is_uuid_v7_and_ids_sequential() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = SubagentRegistry::new(&cfg(8, 4));
        let u = uuid::Uuid::parse_str(reg.run_id()).unwrap();
        assert_eq!(u.get_version_num(), 7);
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
        let reg = SubagentRegistry::new(&cfg(2, 4));
        reg.reserve(tmp.path()).unwrap();
        reg.reserve(tmp.path()).unwrap();
        let err = reg.reserve(tmp.path()).unwrap_err();
        assert_eq!(err, "subagent limit reached: 2 live agents (max_live)");
    }

    #[test]
    fn finishing_frees_live_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = SubagentRegistry::new(&cfg(1, 4));
        let (id, _) = reg.reserve(tmp.path()).unwrap();
        assert!(reg.reserve(tmp.path()).is_err());
        reg.set_state(&id, AgentState::Completed);
        let (id2, _) = reg.reserve(tmp.path()).unwrap();
        assert_eq!(id2, "a2");
        let snap = reg.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].state, AgentState::Completed);
    }

    #[tokio::test]
    async fn concurrency_semaphore_blocks_second() {
        let reg = SubagentRegistry::new(&cfg(8, 1));
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
        let reg = SubagentRegistry::new(&cfg(8, 4));
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

    #[test]
    fn global_roundtrip() {
        let reg = SubagentRegistry::new(&cfg(8, 4));
        set_global(Arc::clone(&reg));
        assert!(global().is_some());
    }
}
