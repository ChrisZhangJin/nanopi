//! Phase 1 end-to-end success-criteria tests (01-06).
//!
//! Each test exercises the *real* in-process subagent runtime — a
//! parent `Agent` (or a `ToolContext` standing in for one) driving the
//! real `SubagentTool` through fake providers — rather than re-testing
//! `run_subagent`'s internals, which `src/tool/subagent.rs`'s own test
//! module already covers exhaustively. These tests exist to lock in
//! the five ROADMAP Phase 1 success criteria as a group, so a future
//! change that breaks one in isolation (while every unit test near it
//! stays green) still fails here.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::mpsc;

use crate::agent::agents::AgentScope;
use crate::agent::context::Context;
use crate::agent::loop_::{Agent, HooksConfig, Provider, StopReason};
use crate::agent::permission::PermissionGate;
use crate::agent::subagent_registry::{
    AgentState, PermissionRequest, SpawnTemplate, SubagentRegistry,
};
use crate::config::SubagentConfig;
use crate::event::{AgentEvent, FinishReason, ToolCall, Usage};
use crate::tool::file_state::FileStateTracker;
use crate::tool::subagent::SubagentTool;
use crate::tool::{Tool, ToolContext, ToolRegistry};

fn tmpdir(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("nanopi-e2e-{tag}-{}", crate::util::uuid::v7()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn write_user_agent(home: &Path, name: &str) {
    let dir = home.join("agents");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{name}.md")),
        format!("---\nname: {name}\ndescription: test agent\n---\nYou are a test agent.\n"),
    )
    .unwrap();
}

fn default_cfg() -> SubagentConfig {
    SubagentConfig {
        max_concurrency: 4,
        max_live: 8,
        max_turns: 50,
        token_budget: 300_000,
    }
}

fn template(
    cwd: &Path,
    factory: Arc<dyn Fn(&str) -> Box<dyn Provider> + Send + Sync>,
) -> SpawnTemplate {
    SpawnTemplate {
        cwd: cwd.to_path_buf(),
        model: "test-model".into(),
        base_url: String::new(),
        api_key: String::new(),
        hooks: HooksConfig::default(),
        permission: PermissionGate::from_cli(true, Some(true)),
        tool_exec_mode: crate::config::ToolExecMode::default(),
        tool_exec_overrides: Default::default(),
        provider_factory: factory,
    }
}

fn ctx_for(cwd: PathBuf, registry: Arc<SubagentRegistry>) -> ToolContext {
    ToolContext {
        cwd,
        registry,
        agent_id: None,
        turn_cancel: None,
        file_state: Arc::new(FileStateTracker::default()),
    }
}

/// Count direct child processes of the current process (Linux `ps
/// --ppid`). Used to prove SC1 — no child `nanopi` process (or any
/// other child process) is spawned by a subagent dispatch.
fn child_process_count() -> usize {
    let pid = std::process::id().to_string();
    let out = std::process::Command::new("ps")
        .args(["--no-headers", "--ppid", &pid])
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count(),
        Err(_) => 0,
    }
}

/// Replies with fixed text and stops normally.
struct TextProvider(String);
#[async_trait]
impl Provider for TextProvider {
    fn id(&self) -> &'static str {
        "text"
    }
    async fn stream_turn(&self, _ctx: &Context, tx: mpsc::Sender<AgentEvent>) -> Result<Usage, String> {
        let _ = tx.send(AgentEvent::Start { message_id: "m".into() }).await;
        let _ = tx
            .send(AgentEvent::TextDelta { content_index: 0, text: self.0.clone() })
            .await;
        let _ = tx
            .send(AgentEvent::Done { finish_reason: FinishReason::Stop, usage: Usage::default() })
            .await;
        Ok(Usage::default())
    }
}

struct ErrProvider;
#[async_trait]
impl Provider for ErrProvider {
    fn id(&self) -> &'static str {
        "err"
    }
    async fn stream_turn(&self, _ctx: &Context, _tx: mpsc::Sender<AgentEvent>) -> Result<Usage, String> {
        Err("boom from child provider".into())
    }
}

/// Always emits a tool call, never stops naturally — only a turn limit
/// ends it.
struct AlwaysToolCallProvider;
#[async_trait]
impl Provider for AlwaysToolCallProvider {
    fn id(&self) -> &'static str {
        "always-tool-call"
    }
    async fn stream_turn(&self, _ctx: &Context, tx: mpsc::Sender<AgentEvent>) -> Result<Usage, String> {
        let _ = tx.send(AgentEvent::Start { message_id: "m".into() }).await;
        let _ = tx
            .send(AgentEvent::ToolCall {
                content_index: 0,
                call: ToolCall {
                    id: format!("call_{}", crate::util::uuid::v7()),
                    name: "ls".into(),
                    arguments: json!({"path": "."}),
                },
            })
            .await;
        let _ = tx
            .send(AgentEvent::Done { finish_reason: FinishReason::ToolCalls, usage: Usage::default() })
            .await;
        Ok(Usage::default())
    }
}

/// Never completes until its cancel token fires.
struct HangingProvider;
#[async_trait]
impl Provider for HangingProvider {
    fn id(&self) -> &'static str {
        "hanging"
    }
    async fn stream_turn(&self, _ctx: &Context, tx: mpsc::Sender<AgentEvent>) -> Result<Usage, String> {
        let _ = tx.send(AgentEvent::Start { message_id: "m".into() }).await;
        std::future::pending::<()>().await;
        unreachable!()
    }
}

/// Parent-side stepped provider: iteration 0 issues a `subagent` tool
/// call, iteration 1 finishes with plain text. Mirrors the
/// `SteppedProvider` pattern already used throughout
/// `src/agent/loop_.rs`'s own test module.
struct ParentSubagentCallProvider {
    step: std::sync::atomic::AtomicUsize,
    call_args: serde_json::Value,
}
#[async_trait]
impl Provider for ParentSubagentCallProvider {
    fn id(&self) -> &'static str {
        "parent-stepped"
    }
    async fn stream_turn(&self, _ctx: &Context, tx: mpsc::Sender<AgentEvent>) -> Result<Usage, String> {
        use std::sync::atomic::Ordering;
        let step = self.step.fetch_add(1, Ordering::SeqCst);
        let _ = tx.send(AgentEvent::Start { message_id: "m".into() }).await;
        match step {
            0 => {
                let _ = tx
                    .send(AgentEvent::ToolCall {
                        content_index: 0,
                        call: ToolCall {
                            id: "call_1".into(),
                            name: "subagent".into(),
                            arguments: self.call_args.clone(),
                        },
                    })
                    .await;
                let _ = tx
                    .send(AgentEvent::Done { finish_reason: FinishReason::ToolCalls, usage: Usage::default() })
                    .await;
            }
            _ => {
                let _ = tx
                    .send(AgentEvent::TextDelta { content_index: 0, text: "all done".into() })
                    .await;
                let _ = tx
                    .send(AgentEvent::Done { finish_reason: FinishReason::Stop, usage: Usage::default() })
                    .await;
            }
        }
        Ok(Usage::default())
    }
}

/// Build a bare-bones parent `Agent` directly (struct literal, same
/// pattern `loop_.rs`'s own tests use) rather than through
/// `Agent::build_fresh`, so no real provider/skill/plugin discovery
/// runs. `session_path` must already contain a valid header line.
fn build_parent_agent(
    provider: Box<dyn Provider>,
    cwd: PathBuf,
    session_path: PathBuf,
    subagents: Arc<SubagentRegistry>,
) -> Agent {
    Agent {
        context: Context::default(),
        provider,
        registry: ToolRegistry::standard(),
        session_path,
        session_id: crate::util::uuid::v7().to_string(),
        cwd,
        permission: PermissionGate::from_cli(true, Some(true)),
        hooks: HooksConfig::default(),
        model: "parent-stepped".into(),
        base_url: String::new(),
        api_key: String::new(),
        usage_total: Usage::default(),
        turn_count: 0,
        skills: Vec::new(),
        pending_follow_ups: Default::default(),
        tool_exec_mode: crate::config::ToolExecMode::default(),
        tool_exec_overrides: Default::default(),
        plugin_commands: Vec::new(),
        plugin_grants: Vec::new(),
        event_subscribers: Default::default(),
        no_context_files: true,
        prompt_overrides: crate::agent::prompt_override::PromptOverrides::default(),
        system_base: None,
        agent_id: None,
        limits: None,
        stop_reason: None,
        subagents,
        file_state: Arc::new(FileStateTracker::default()),
    }
}

fn write_session_header(path: &Path) {
    std::fs::write(
        path,
        "{\"type\":\"session\",\"version\":2,\"id\":\"019fe000-0000-7000-8000-000000000001\",\"timestamp\":\"2026-10-03T00:00:00Z\",\"cwd\":\"/tmp\",\"model\":\"parent-stepped\",\"base_url\":\"\"}\n",
    )
    .unwrap();
}

/// SC1 (RT-01) + SC3 (RT-04): a parent `Agent` driving the real
/// `subagent` tool through a fake provider completes with no child OS
/// process spawned, and the parent's own session file holds only the
/// tool call + its result — the child's own turn never leaks in.
#[tokio::test(flavor = "multi_thread")]
async fn no_child_process_and_parent_session_isolated() {
    let _h = crate::TempNanopiHome::new();
    let home = _h.path().to_path_buf();
    write_user_agent(&home, "scout");

    let cwd = tmpdir("isolated");
    let factory: Arc<dyn Fn(&str) -> Box<dyn Provider> + Send + Sync> =
        Arc::new(|_model| Box::new(TextProvider("hello from child".into())));
    let registry = Arc::new(SubagentRegistry::new(default_cfg()));
    registry.set_template(template(&cwd, factory));

    let session_path = cwd.join("parent.jsonl");
    write_session_header(&session_path);

    let mut agent = build_parent_agent(
        Box::new(ParentSubagentCallProvider {
            step: std::sync::atomic::AtomicUsize::new(0),
            call_args: json!({"agent": "scout", "task": "say hi"}),
        }),
        cwd.clone(),
        session_path.clone(),
        Arc::clone(&registry),
    );

    let before = child_process_count();

    let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let final_text = agent.run_turn("start", &tx, None, None).await.expect("parent turn");
    drop(tx);
    drain.await.unwrap();

    let after = child_process_count();
    assert_eq!(before, after, "no OS child process must be left behind by the dispatch");
    assert_eq!(final_text, "all done", "parent's own final text, not the child's");

    // Child wrote its own transcript, separate from the parent's.
    let transcript = registry.agents_dir(&cwd).join("a1").join("transcript.jsonl");
    assert!(transcript.exists(), "missing child transcript at {transcript:?}");
    let child_content = std::fs::read_to_string(&transcript).unwrap();
    assert!(child_content.contains("hello from child"));

    // Parent session holds exactly one tool_call, one tool_result, and
    // exactly one assistant message — its own "all done" — never the
    // child's internal turn.
    let parent_content = std::fs::read_to_string(&session_path).unwrap();
    let entries: Vec<serde_json::Value> = parent_content
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let tool_calls: Vec<_> = entries
        .iter()
        .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("tool_call"))
        .collect();
    let tool_results: Vec<_> = entries
        .iter()
        .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
        .collect();
    let assistant_messages: Vec<_> = entries
        .iter()
        .filter(|v| {
            v.get("type").and_then(|t| t.as_str()) == Some("message")
                && v.get("role").and_then(|r| r.as_str()) == Some("assistant")
        })
        .collect();
    assert_eq!(tool_calls.len(), 1, "{entries:?}");
    assert_eq!(tool_calls[0]["tool_name"], "subagent");
    assert_eq!(tool_results.len(), 1, "{entries:?}");
    assert_eq!(assistant_messages.len(), 1, "{entries:?}");
    assert_eq!(assistant_messages[0]["content"], "all done");

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&cwd).ok();
}

/// SC2 (RT-02): cancelling the parent turn's own cancel token stops a
/// hanging foreground subagent promptly, with no orphaned task.
#[tokio::test(flavor = "multi_thread")]
async fn parent_cancel_stops_foreground_subagent() {
    let _h = crate::TempNanopiHome::new();
    let home = _h.path().to_path_buf();
    write_user_agent(&home, "hangs");

    let cwd = tmpdir("cancel");
    let factory: Arc<dyn Fn(&str) -> Box<dyn Provider> + Send + Sync> =
        Arc::new(|_model| Box::new(HangingProvider));
    let registry = Arc::new(SubagentRegistry::new(default_cfg()));
    registry.set_template(template(&cwd, factory));

    let turn_cancel = tokio_util::sync::CancellationToken::new();
    let ctx = ToolContext {
        cwd: cwd.clone(),
        registry: Arc::clone(&registry),
        agent_id: None,
        turn_cancel: Some(turn_cancel.clone()),
        file_state: Arc::new(FileStateTracker::default()),
    };

    let tool = SubagentTool;
    let run = tokio::spawn(async move {
        tool.execute(json!({"agent": "hangs", "task": "go"}), &ctx).await
    });

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    turn_cancel.cancel();

    let out = tokio::time::timeout(std::time::Duration::from_secs(2), run)
        .await
        .expect("subagent must stop within 2s of parent cancel")
        .unwrap()
        .unwrap();
    assert!(out.is_error);
    assert_eq!(out.metadata.unwrap()["status"], "cancelled");
    assert_eq!(registry.live_count(), 0, "cancelled subagent must not leak a live slot");

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&cwd).ok();
}

/// SC2 (RT-03): `stop_all` cancels every background subagent; a
/// foreground subagent on an uncancelled turn token is untouched.
#[tokio::test(flavor = "multi_thread")]
async fn stop_all_cancels_background_tokens() {
    let _h = crate::TempNanopiHome::new();
    let home = _h.path().to_path_buf();
    write_user_agent(&home, "hangs");

    let cwd = tmpdir("stopall");
    let factory: Arc<dyn Fn(&str) -> Box<dyn Provider> + Send + Sync> =
        Arc::new(|_model| Box::new(HangingProvider));
    let mut cfg = default_cfg();
    cfg.max_live = 8;
    let registry = Arc::new(SubagentRegistry::new(cfg));
    registry.set_template(template(&cwd, factory));

    // Two background dispatches (no turn_cancel => registry background token).
    let ctx_bg1 = ctx_for(cwd.clone(), Arc::clone(&registry));
    let ctx_bg2 = ctx_for(cwd.clone(), Arc::clone(&registry));
    let tool = SubagentTool;
    let bg1 = tokio::spawn(async move {
        tool.execute(json!({"agent": "hangs", "task": "a"}), &ctx_bg1).await
    });
    let tool = SubagentTool;
    let bg2 = tokio::spawn(async move {
        tool.execute(json!({"agent": "hangs", "task": "b"}), &ctx_bg2).await
    });

    // One foreground dispatch on its own, separate turn token.
    let fg_token = tokio_util::sync::CancellationToken::new();
    let ctx_fg = ToolContext {
        cwd: cwd.clone(),
        registry: Arc::clone(&registry),
        agent_id: None,
        turn_cancel: Some(fg_token.clone()),
        file_state: Arc::new(FileStateTracker::default()),
    };
    let tool = SubagentTool;
    let fg = tokio::spawn(async move {
        tool.execute(json!({"agent": "hangs", "task": "c"}), &ctx_fg).await
    });

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(registry.live_count(), 3, "all three should be running before stop_all");

    registry.stop_all();

    let out1 = tokio::time::timeout(std::time::Duration::from_secs(2), bg1).await.unwrap().unwrap().unwrap();
    let out2 = tokio::time::timeout(std::time::Duration::from_secs(2), bg2).await.unwrap().unwrap().unwrap();
    assert_eq!(out1.metadata.unwrap()["status"], "cancelled");
    assert_eq!(out2.metadata.unwrap()["status"], "cancelled");

    // Give the foreground task a moment to prove it's still running,
    // then clean it up via its own token.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!fg.is_finished(), "stop_all must not touch a foreground subagent's own token");
    fg_token.cancel();
    let out_fg = tokio::time::timeout(std::time::Duration::from_secs(2), fg).await.unwrap().unwrap().unwrap();
    assert_eq!(out_fg.metadata.unwrap()["status"], "cancelled");

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&cwd).ok();
}

/// SC4 (RT-06, RT-08): a subagent hitting its turn limit, and another
/// hitting a provider error, both yield a reported result rather than
/// a panic or a hung batch — the parent keeps going either way.
#[tokio::test(flavor = "multi_thread")]
async fn limit_and_error_are_reported_not_fatal() {
    let _h = crate::TempNanopiHome::new();
    let home = _h.path().to_path_buf();
    write_user_agent(&home, "looper");
    write_user_agent(&home, "flaky");

    let cwd = tmpdir("limiterr");
    let mut cfg = default_cfg();
    cfg.max_turns = 1;
    let registry_limit = Arc::new(SubagentRegistry::new(cfg));
    registry_limit.set_template(template(
        &cwd,
        Arc::new(|_model| Box::new(AlwaysToolCallProvider)),
    ));
    let ctx_limit = ctx_for(cwd.clone(), registry_limit);
    let tool = SubagentTool;
    let out_limit = tool
        .execute(json!({"agent": "looper", "task": "go"}), &ctx_limit)
        .await
        .unwrap();
    let meta = out_limit.metadata.unwrap();
    assert_eq!(meta["status"], "limit_reached");
    assert_eq!(meta["limit"], "max_turns");

    let registry_err = Arc::new(SubagentRegistry::new(default_cfg()));
    registry_err.set_template(template(&cwd, Arc::new(|_model| Box::new(ErrProvider))));
    let ctx_err = ctx_for(cwd.clone(), registry_err);
    let out_err = tool
        .execute(json!({"agent": "flaky", "task": "go"}), &ctx_err)
        .await
        .unwrap();
    assert!(out_err.is_error);
    assert_eq!(out_err.metadata.unwrap()["status"], "failed");
    assert!(out_err.content.contains("boom from child provider"));

    // Reaching here at all — rather than a panic unwinding the test
    // process — is itself the proof nanopi keeps running (D-11).
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&cwd).ok();
}

/// SC4 (RT-05): a subagent never sees the `subagent` tool itself
/// (depth is 1), and dispatching beyond `max_live` fails in-band
/// rather than spawning past the cap.
#[tokio::test(flavor = "multi_thread")]
async fn subagent_cannot_dispatch_subagent() {
    let _h = crate::TempNanopiHome::new();
    let home = _h.path().to_path_buf();
    write_user_agent(&home, "worker");

    let cwd = tmpdir("depth1");

    // Captures the tool specs the child provider was actually offered,
    // via a channel rather than shared mutable state (the factory is
    // `Fn`, called fresh per dispatch).
    let (record_tx, mut record_rx) = mpsc::unbounded_channel::<Vec<String>>();
    struct ChannelRecordingProvider {
        tx: mpsc::UnboundedSender<Vec<String>>,
    }
    #[async_trait]
    impl Provider for ChannelRecordingProvider {
        fn id(&self) -> &'static str {
            "chan-recording"
        }
        async fn stream_turn(&self, ctx: &Context, tx: mpsc::Sender<AgentEvent>) -> Result<Usage, String> {
            let _ = self.tx.send(ctx.tools.iter().map(|t| t.name.clone()).collect());
            let _ = tx.send(AgentEvent::Start { message_id: "m".into() }).await;
            let _ = tx
                .send(AgentEvent::TextDelta { content_index: 0, text: "ok".into() })
                .await;
            let _ = tx
                .send(AgentEvent::Done { finish_reason: FinishReason::Stop, usage: Usage::default() })
                .await;
            Ok(Usage::default())
        }
    }
    let registry = Arc::new(SubagentRegistry::new(default_cfg()));
    registry.set_template(template(
        &cwd,
        Arc::new(move |_model| {
            Box::new(ChannelRecordingProvider { tx: record_tx.clone() }) as Box<dyn Provider>
        }),
    ));
    let ctx = ctx_for(cwd.clone(), Arc::clone(&registry));
    let tool = SubagentTool;
    let out = tool
        .execute(json!({"agent": "worker", "task": "go"}), &ctx)
        .await
        .unwrap();
    assert!(!out.is_error, "{:?}", out);
    let offered = record_rx.recv().await.expect("provider must have run");
    assert!(
        !offered.iter().any(|n| n == "subagent"),
        "a subagent must never be offered the subagent tool itself: {offered:?}"
    );

    // max_live=1, two parallel items: the second must fail in-band
    // rather than spawn past the cap.
    let mut cfg2 = default_cfg();
    cfg2.max_live = 1;
    let registry2 = Arc::new(SubagentRegistry::new(cfg2));
    registry2.set_template(template(
        &cwd,
        Arc::new(|_model| {
            Box::new(SlowTextProvider("slow".into())) as Box<dyn Provider>
        }),
    ));
    let ctx2 = ctx_for(cwd.clone(), registry2);
    let out2 = tool
        .execute(
            json!({"tasks": [
                {"agent": "worker", "task": "a"},
                {"agent": "worker", "task": "b"},
            ]}),
            &ctx2,
        )
        .await
        .unwrap();
    assert!(out2.is_error, "{:?}", out2);
    assert!(out2.content.contains("max_live"), "{:?}", out2.content);

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&cwd).ok();
}

/// A text provider with a short artificial delay, so two concurrent
/// dispatches reliably overlap long enough for a `max_live` race to be
/// observable.
struct SlowTextProvider(String);
#[async_trait]
impl Provider for SlowTextProvider {
    fn id(&self) -> &'static str {
        "slow-text"
    }
    async fn stream_turn(&self, _ctx: &Context, tx: mpsc::Sender<AgentEvent>) -> Result<Usage, String> {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let _ = tx.send(AgentEvent::Start { message_id: "m".into() }).await;
        let _ = tx
            .send(AgentEvent::TextDelta { content_index: 0, text: self.0.clone() })
            .await;
        let _ = tx
            .send(AgentEvent::Done { finish_reason: FinishReason::Stop, usage: Usage::default() })
            .await;
        Ok(Usage::default())
    }
}

/// SC5 (RT-07): a subagent's own tool call triggers a `ToolExecutionStart`
/// hook that returns `{"decision":"ask"}`; the subagent blocks in
/// `WaitingPermission` until the test answers the queued request.
#[tokio::test(flavor = "multi_thread")]
async fn queued_permission_waits_for_answer() {
    let _h = crate::TempNanopiHome::new();
    let home = _h.path().to_path_buf();
    write_user_agent(&home, "asker");

    let cwd = tmpdir("permission");
    let registry = Arc::new(SubagentRegistry::new(default_cfg()));
    registry.permissions().set_interactive();

    let hooks = HooksConfig {
        tool_execution_start: vec![crate::agent::hook::HookConfig {
            matcher: "*".into(),
            kind: "command".into(),
            command: r#"echo '{"decision":"ask"}'"#.into(),
            timeout: 5_000,
        }],
        ..Default::default()
    };

    let mut t = template(&cwd, Arc::new(|_model| Box::new(AlwaysToolCallProvider)));
    t.hooks = hooks;
    t.permission = PermissionGate::from_cli(false, Some(true));
    registry.set_template(t);

    let ctx = ctx_for(cwd.clone(), Arc::clone(&registry));
    let tool = SubagentTool;
    let run = tokio::spawn(async move {
        tool.execute(json!({"agent": "asker", "task": "go"}), &ctx).await
    });

    // Poll for the queued request, confirming the subagent is
    // genuinely blocked waiting on it before answering.
    let mut front = None;
    for _ in 0..100 {
        if let Some(req) = registry.permissions().front() {
            front = Some(req);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let front: PermissionRequest = front.expect("permission request must be queued");
    assert_eq!(front.agent_id, "a1");
    assert!(!run.is_finished(), "subagent must still be waiting on the permission answer");

    registry.permissions().answer_front(true);

    // Allowed: the `ls` call runs, AlwaysToolCallProvider loops
    // forever calling it again, so bound this with the registry's
    // turn limit instead of waiting for a natural stop — answering
    // `true` once is enough to prove the queue unblocks the agent;
    // cancel the rest so the test doesn't hang on an infinite loop.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    registry.stop_all();
    let out = tokio::time::timeout(std::time::Duration::from_secs(2), run)
        .await
        .expect("must resolve once unblocked and then stopped")
        .unwrap()
        .unwrap();
    // Either cancelled (stop_all) or limit_reached — both prove the
    // agent moved past WaitingPermission instead of hanging forever.
    let status = out.metadata.unwrap()["status"].as_str().unwrap().to_string();
    assert!(
        status == "cancelled" || status == "limit_reached",
        "expected the agent to have progressed past the permission wait, got {status}"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&cwd).ok();
}

/// SC5 (ISO-03): two subagents (distinct `FileStateTracker`s,
/// simulating two independent agents) each read, then both edit the
/// same file — the second edit is refused as stale and the file keeps
/// the first writer's content.
#[tokio::test]
async fn two_agents_same_file_second_refused() {
    let dir = tmpdir("sharedfile");
    std::fs::write(dir.join("f.txt"), "hello world\n").unwrap();

    let ctx_a = ToolContext::new(dir.clone());
    crate::tool::read::ReadTool
        .execute(json!({"path": "f.txt"}), &ctx_a)
        .await
        .unwrap();

    let ctx_b = ToolContext::new(dir.clone());
    crate::tool::read::ReadTool
        .execute(json!({"path": "f.txt"}), &ctx_b)
        .await
        .unwrap();

    // Agent A writes first.
    crate::tool::write::WriteTool
        .execute(json!({"path": "f.txt", "content": "written by agent a\n"}), &ctx_a)
        .await
        .unwrap();

    // Agent B's edit, based on its now-stale read, must be refused.
    let r = crate::tool::edit::EditTool
        .execute(
            json!({"path": "f.txt", "oldText": "hello", "newText": "pwned by b"}),
            &ctx_b,
        )
        .await;
    match r {
        Err(crate::tool::ToolError::Execution(msg)) => {
            assert!(msg.contains("file changed since you read it — re-read first"), "{msg:?}");
        }
        other => panic!("expected stale-write refusal, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(dir.join("f.txt")).unwrap(),
        "written by agent a\n",
        "the refused edit must not have touched the file"
    );

    std::fs::remove_dir_all(&dir).ok();
}

// Silence an unused-import warning when AgentScope ends up only
// referenced through the SubagentTool's own argument parsing.
#[allow(dead_code)]
fn _touch(_: AgentScope) {}
#[allow(dead_code)]
fn _touch_state(_: AgentState) {}
#[allow(dead_code)]
fn _touch_stop(_: StopReason) {}
