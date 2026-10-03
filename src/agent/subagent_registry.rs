//! `SubagentRegistry` — shared foundations every in-process subagent
//! runs through (Phase 1, v0.13.0).
//!
//! Owns: ids ("a1", "a2", ...), a run id (uuid v7, fixed for the
//! registry's lifetime), a root `CancellationToken` whose children are
//! all cancelled by `stop_all()` (D-05/D-06), a global semaphore
//! bounding concurrency (D-08), a live-agent cap counting
//! queued+running+waiting agents (D-08), the parent's `SpawnTemplate`
//! for building fresh subagents, and a `PermissionBroker` queueing
//! permission requests FIFO (D-13/D-14).
//!
//! `ToolContext::new` constructs a `standalone()` registry so every
//! existing non-agent call site (tests, one-off tool invocations) gets
//! a trivially-usable default without an `Option` unwrap creeping in
//! (research open question 2).

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::{oneshot, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::agent::loop_::{HooksConfig, Provider};
use crate::agent::permission::PermissionGate;
use crate::config::SubagentConfig;
use crate::event::Usage;
use crate::tool::ExecutionMode;

/// Per-agent limits derived from `[subagent]` config (D-08).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentLimits {
    pub max_turns: u32,
    pub token_budget: u64,
}

impl From<&SubagentConfig> for AgentLimits {
    fn from(cfg: &SubagentConfig) -> Self {
        Self {
            max_turns: cfg.max_turns,
            token_budget: cfg.token_budget,
        }
    }
}

/// Lifecycle state of a single subagent, as tracked by the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Queued,
    Running,
    WaitingPermission,
    Done,
    Failed,
    LimitReached,
    Cancelled,
}

/// A point-in-time view of one agent's registry state. Phase 5's strip
/// reads a `Vec<AgentSnapshot>` from `SubagentRegistry::snapshot()`
/// (D-01); not rendered anywhere yet in this phase.
#[derive(Debug, Clone)]
pub struct AgentSnapshot {
    pub id: String,
    pub state: AgentState,
    pub started_at: Instant,
    pub usage: Usage,
    pub foreground: bool,
}

#[derive(Debug, Clone)]
struct AgentEntry {
    state: AgentState,
    started_at: Instant,
    usage: Usage,
    foreground: bool,
}

/// Everything a subagent needs inherited from its parent (D-04): same
/// provider/model unless the dispatch says otherwise, same hooks and
/// permission gate, same tool-exec settings. `provider_factory` takes
/// the model id and returns a fresh `Provider` instance — production
/// code wraps `crate::provider::build` with the parent's
/// api_kind/vendor/inline_think_tags already captured; tests inject a
/// fake.
#[derive(Clone)]
pub struct SpawnTemplate {
    pub cwd: PathBuf,
    pub model: String,
    pub base_url: String,
    pub api_key: String,
    pub hooks: HooksConfig,
    pub permission: PermissionGate,
    pub tool_exec_mode: crate::config::ToolExecMode,
    pub tool_exec_overrides: std::collections::BTreeMap<String, ExecutionMode>,
    pub provider_factory: Arc<dyn Fn(&str) -> Box<dyn Provider> + Send + Sync>,
}

impl std::fmt::Debug for SpawnTemplate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpawnTemplate")
            .field("cwd", &self.cwd)
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .field("tool_exec_mode", &self.tool_exec_mode)
            .finish_non_exhaustive()
    }
}

/// A reserved slot in the live-agent map (D-08). Removes its entry on
/// `Drop` so a failed/finished/cancelled agent can never leak capacity.
#[derive(Debug)]
pub struct LiveSlot {
    id: String,
    live: Arc<Mutex<HashMap<String, AgentEntry>>>,
}

impl Drop for LiveSlot {
    fn drop(&mut self) {
        if let Ok(mut map) = self.live.lock() {
            map.remove(&self.id);
        }
    }
}

/// One queued permission request (D-13).
#[derive(Debug, Clone)]
pub struct PermissionRequest {
    pub agent_id: String,
    pub summary: String,
}

/// How `PermissionBroker` resolves a request when nobody is actively
/// answering (D-14). `Deny` is the default — nanopi has no interactive
/// per-tool approval anywhere today, and `-p` never prompts, so the
/// non-interactive rule is deny.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BrokerMode {
    #[default]
    Deny,
    Interactive,
}

struct QueuedRequest {
    request: PermissionRequest,
    reply: oneshot::Sender<bool>,
}

/// FIFO queue of permission requests from subagents (D-13). In `Deny`
/// mode (the default), every request resolves to `false` immediately —
/// mirrors print-mode's existing non-interactive rule (D-14). In
/// `Interactive` mode, an answerer peeks the front request with
/// `front()` and resolves it with `answer_front()`; only the front
/// request is ever exposed, so a TUI renders one prompt at a time.
pub struct PermissionBroker {
    mode: Mutex<BrokerMode>,
    queue: Mutex<VecDeque<QueuedRequest>>,
    notify: tokio::sync::Notify,
}

impl std::fmt::Debug for PermissionBroker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PermissionBroker").finish_non_exhaustive()
    }
}

impl Default for PermissionBroker {
    fn default() -> Self {
        Self::new()
    }
}

impl PermissionBroker {
    pub fn new() -> Self {
        Self {
            mode: Mutex::new(BrokerMode::default()),
            queue: Mutex::new(VecDeque::new()),
            notify: tokio::sync::Notify::new(),
        }
    }

    /// Switch to interactive mode, so future (and already-queued)
    /// requests wait for `answer_front` instead of resolving to deny.
    pub fn set_interactive(&self) {
        if let Ok(mut m) = self.mode.lock() {
            *m = BrokerMode::Interactive;
        }
        self.notify.notify_waiters();
    }

    fn mode(&self) -> BrokerMode {
        self.mode.lock().map(|m| *m).unwrap_or(BrokerMode::Deny)
    }

    /// Submit a request and wait for an answer, a cancel, or (in Deny
    /// mode) resolve immediately to `false`.
    pub async fn request(&self, req: PermissionRequest, cancel: &CancellationToken) -> bool {
        if self.mode() == BrokerMode::Deny {
            return false;
        }

        let (tx, rx) = oneshot::channel();
        if let Ok(mut q) = self.queue.lock() {
            q.push_back(QueuedRequest {
                request: req,
                reply: tx,
            });
        } else {
            return false;
        }
        self.notify.notify_waiters();

        tokio::select! {
            res = rx => res.unwrap_or(false),
            _ = cancel.cancelled() => false,
        }
    }

    /// Peek the oldest queued request, if any.
    pub fn front(&self) -> Option<PermissionRequest> {
        self.queue
            .lock()
            .ok()
            .and_then(|q| q.front().map(|qr| qr.request.clone()))
    }

    /// Answer the oldest queued request (`true` = allow). A no-op if
    /// the queue is currently empty.
    pub fn answer_front(&self, allow: bool) {
        let front = self.queue.lock().ok().and_then(|mut q| q.pop_front());
        if let Some(qr) = front {
            let _ = qr.reply.send(allow);
        }
    }
}

/// Owns every live subagent: ids, run id, cancel-token tree,
/// concurrency semaphore, live cap, state map, spawn template and
/// permission broker (D-01).
pub struct SubagentRegistry {
    run_id: String,
    limits: AgentLimits,
    root: Mutex<CancellationToken>,
    semaphore: Mutex<Arc<Semaphore>>,
    max_concurrency: usize,
    max_live: usize,
    next_id: AtomicU32,
    live: Arc<Mutex<HashMap<String, AgentEntry>>>,
    template: Mutex<Option<Arc<SpawnTemplate>>>,
    permissions: PermissionBroker,
}

impl std::fmt::Debug for SubagentRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubagentRegistry")
            .field("run_id", &self.run_id)
            .field("max_concurrency", &self.max_concurrency)
            .field("max_live", &self.max_live)
            .finish_non_exhaustive()
    }
}

impl SubagentRegistry {
    /// Build a registry for one process/session run. Generates a fresh
    /// run id (uuid v7, D-03) and a fresh root cancel token.
    pub fn new(cfg: SubagentConfig) -> Self {
        Self {
            run_id: crate::util::uuid::v7().to_string(),
            limits: AgentLimits::from(&cfg),
            root: Mutex::new(CancellationToken::new()),
            semaphore: Mutex::new(Arc::new(Semaphore::new(cfg.max_concurrency.max(1)))),
            max_concurrency: cfg.max_concurrency.max(1),
            max_live: cfg.max_live.max(1),
            next_id: AtomicU32::new(1),
            live: Arc::new(Mutex::new(HashMap::new())),
            template: Mutex::new(None),
            permissions: PermissionBroker::new(),
        }
    }

    /// A registry with default config — used by `ToolContext::new` and
    /// anywhere a tool context is built without a real agent run behind
    /// it (tests, one-off invocations).
    pub fn standalone() -> Self {
        Self::new(SubagentConfig::default())
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn limits(&self) -> AgentLimits {
        self.limits
    }

    /// Allocate the next short id: "a1", "a2", ...
    pub fn next_id(&self) -> String {
        let n = self.next_id.fetch_add(1, Ordering::SeqCst);
        format!("a{n}")
    }

    pub fn set_template(&self, template: SpawnTemplate) {
        if let Ok(mut t) = self.template.lock() {
            *t = Some(Arc::new(template));
        }
    }

    pub fn template(&self) -> Option<Arc<SpawnTemplate>> {
        self.template.lock().ok().and_then(|t| t.clone())
    }

    /// Reserve a live slot for a new agent. Counts queued, running and
    /// waiting-permission agents already in the map; rejects beyond
    /// `max_live` with an in-band error (D-08, T-01-01). The returned
    /// `LiveSlot` removes its own entry from the map on `Drop`, so a
    /// failure anywhere downstream cannot leak capacity.
    pub fn reserve(&self, id: &str, foreground: bool) -> Result<LiveSlot, String> {
        let mut map = self
            .live
            .lock()
            .map_err(|_| "subagent registry lock poisoned".to_string())?;
        let live_count = map
            .values()
            .filter(|e| {
                matches!(
                    e.state,
                    AgentState::Queued | AgentState::Running | AgentState::WaitingPermission
                )
            })
            .count();
        if live_count >= self.max_live {
            return Err(format!(
                "subagent limit reached: max_live = {} agents already alive",
                self.max_live
            ));
        }
        map.insert(
            id.to_string(),
            AgentEntry {
                state: AgentState::Queued,
                started_at: Instant::now(),
                usage: Usage::default(),
                foreground,
            },
        );
        Ok(LiveSlot {
            id: id.to_string(),
            live: Arc::clone(&self.live),
        })
    }

    /// Acquire a global concurrency permit (D-08). Never panics on a
    /// closed semaphore; maps it to an in-band error instead (D-11).
    pub async fn acquire_permit(&self) -> Result<OwnedSemaphorePermit, String> {
        let sem = self
            .semaphore
            .lock()
            .map_err(|_| "subagent registry lock poisoned".to_string())?
            .clone();
        sem.acquire_owned()
            .await
            .map_err(|_| "subagent concurrency semaphore closed".to_string())
    }

    /// A background subagent's cancel token: a child of the registry
    /// root, so "stop all" (`stop_all`) cancels it but the main turn's
    /// Esc does not (D-05).
    pub fn background_token(&self) -> CancellationToken {
        self.root
            .lock()
            .map(|r| r.child_token())
            .unwrap_or_else(|_| CancellationToken::new())
    }

    /// Cancel every agent descended from the current root, then swap in
    /// a fresh root so subsequent `background_token()` calls are not
    /// already cancelled (D-06).
    pub fn stop_all(&self) {
        if let Ok(mut root) = self.root.lock() {
            root.cancel();
            *root = CancellationToken::new();
        }
    }

    pub fn set_state(&self, id: &str, state: AgentState) {
        if let Ok(mut map) = self.live.lock() {
            if let Some(entry) = map.get_mut(id) {
                entry.state = state;
            }
        }
    }

    pub fn add_usage(&self, id: &str, usage: &Usage) {
        if let Ok(mut map) = self.live.lock() {
            if let Some(entry) = map.get_mut(id) {
                entry.usage.input_tokens += usage.input_tokens;
                entry.usage.output_tokens += usage.output_tokens;
                entry.usage.cache_read_tokens += usage.cache_read_tokens;
                entry.usage.cache_write_tokens += usage.cache_write_tokens;
            }
        }
    }

    pub fn snapshot(&self) -> Vec<AgentSnapshot> {
        self.live
            .lock()
            .map(|map| {
                map.iter()
                    .map(|(id, e)| AgentSnapshot {
                        id: id.clone(),
                        state: e.state,
                        started_at: e.started_at,
                        usage: e.usage.clone(),
                        foreground: e.foreground,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Count of currently live (queued/running/waiting) agents.
    pub fn live_count(&self) -> usize {
        self.live
            .lock()
            .map(|map| {
                map.values()
                    .filter(|e| {
                        matches!(
                            e.state,
                            AgentState::Queued
                                | AgentState::Running
                                | AgentState::WaitingPermission
                        )
                    })
                    .count()
            })
            .unwrap_or(0)
    }

    /// `<cwd>/.nanopi/agents/<run_id>` — root dir for this run's
    /// subagent transcripts (D-12).
    pub fn agents_dir(&self, cwd: &Path) -> PathBuf {
        cwd.join(".nanopi").join("agents").join(&self.run_id)
    }

    pub fn permissions(&self) -> &PermissionBroker {
        &self.permissions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(max_concurrency: usize, max_live: usize) -> SubagentConfig {
        SubagentConfig {
            max_concurrency,
            max_live,
            max_turns: 50,
            token_budget: 300_000,
        }
    }

    #[test]
    fn default_config_matches_d08() {
        let c = SubagentConfig::default();
        assert_eq!(c.max_concurrency, 4);
        assert_eq!(c.max_live, 8);
        assert_eq!(c.max_turns, 50);
        assert_eq!(c.token_budget, 300_000);
    }

    #[test]
    fn next_id_yields_a1_a2_a3_in_order() {
        let reg = SubagentRegistry::new(cfg(4, 8));
        assert_eq!(reg.next_id(), "a1");
        assert_eq!(reg.next_id(), "a2");
        assert_eq!(reg.next_id(), "a3");
    }

    #[test]
    fn run_id_is_nonempty_uuid_and_fixed() {
        let reg = SubagentRegistry::new(cfg(4, 8));
        let first = reg.run_id().to_string();
        assert!(!first.is_empty());
        assert_eq!(first.len(), 36);
        assert_eq!(reg.run_id(), first);
    }

    #[test]
    fn reserve_succeeds_max_live_times_then_errors() {
        let reg = SubagentRegistry::new(cfg(4, 2));
        let _s1 = reg.reserve("a1", true).expect("slot 1");
        let _s2 = reg.reserve("a2", true).expect("slot 2");
        let err = reg.reserve("a3", true).unwrap_err();
        assert!(err.contains("max_live"), "error was: {err}");
    }

    #[test]
    fn dropping_a_reservation_frees_a_slot() {
        let reg = SubagentRegistry::new(cfg(4, 1));
        {
            let _s1 = reg.reserve("a1", true).expect("slot 1");
            assert_eq!(reg.live_count(), 1);
        }
        assert_eq!(reg.live_count(), 0);
        let _s2 = reg.reserve("a2", true).expect("slot after drop");
    }

    #[tokio::test]
    async fn stop_all_cancels_children_but_not_fresh_tokens() {
        let reg = SubagentRegistry::new(cfg(4, 8));
        let child = reg.background_token();
        assert!(!child.is_cancelled());
        reg.stop_all();
        assert!(child.is_cancelled());

        let fresh = reg.background_token();
        assert!(!fresh.is_cancelled());
    }

    #[tokio::test]
    async fn permission_broker_fifo_order() {
        let broker = PermissionBroker::new();
        broker.set_interactive();

        let cancel_a = CancellationToken::new();
        let cancel_b = CancellationToken::new();
        let broker = Arc::new(broker);

        let b1 = Arc::clone(&broker);
        let req_a = tokio::spawn(async move {
            b1.request(
                PermissionRequest {
                    agent_id: "a1".into(),
                    summary: "run rm".into(),
                },
                &cancel_a,
            )
            .await
        });
        // Ensure a1 is queued before a2 submits.
        tokio::task::yield_now().await;
        let b2 = Arc::clone(&broker);
        let req_b = tokio::spawn(async move {
            b2.request(
                PermissionRequest {
                    agent_id: "a2".into(),
                    summary: "run ls".into(),
                },
                &cancel_b,
            )
            .await
        });
        tokio::task::yield_now().await;

        let front = broker.front().expect("front request present");
        assert_eq!(front.agent_id, "a1", "a1 submitted first, must be front");
        broker.answer_front(true);
        assert_eq!(req_a.await.unwrap(), true);

        let front2 = broker.front().expect("second front request present");
        assert_eq!(front2.agent_id, "a2");
        broker.answer_front(false);
        assert_eq!(req_b.await.unwrap(), false);
    }

    #[tokio::test]
    async fn permission_broker_deny_mode_resolves_immediately() {
        let broker = PermissionBroker::new();
        let cancel = CancellationToken::new();
        let allowed = broker
            .request(
                PermissionRequest {
                    agent_id: "a1".into(),
                    summary: "run rm".into(),
                },
                &cancel,
            )
            .await;
        assert!(!allowed);
    }

    #[tokio::test]
    async fn permission_broker_cancel_resolves_false() {
        let broker = Arc::new(PermissionBroker::new());
        broker.set_interactive();
        let cancel = CancellationToken::new();
        let b = Arc::clone(&broker);
        let cancel_task = cancel.clone();
        let handle = tokio::spawn(async move {
            b.request(
                PermissionRequest {
                    agent_id: "a1".into(),
                    summary: "run rm".into(),
                },
                &cancel_task,
            )
            .await
        });
        tokio::task::yield_now().await;
        cancel.cancel();
        assert_eq!(handle.await.unwrap(), false);
    }
}
