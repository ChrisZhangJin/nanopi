//! Integration: the parent supervisor (01-05) spawns the real `nanopi`
//! binary as a child with `build_child_args` / `build_child_env` against a
//! hermetic fake OpenAI endpoint, and the D-03 agent-dir layout lands.
#![cfg(unix)]

use std::io::{Read, Write};
use std::path::PathBuf;

use nanopi::agent::agents::{AgentConfig, AgentSource};
use nanopi::config::AgentConfig as AgentLimits;
use nanopi::agent_registry::{AgentState, AgentRegistry};
use nanopi::tool::{Tool, ToolContext};
use nanopi::tool::agent::{
    build_child_args, build_child_env, run_single, ChildLaunchSpec, ChildProgram, AgentTool,
};
use nanopi::tool::agent_ctl::SendMessageTool;

/// Serve the same SSE body to every request (copied from print_mode_e2e).
fn spawn_sse_server(chunks: Vec<String>) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("local_addr").port();
    let mut body = String::new();
    for c in &chunks {
        body.push_str("data: ");
        body.push_str(c);
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut seen = Vec::new();
            let mut byte = [0u8; 1];
            while !seen.ends_with(b"\r\n\r\n") {
                match stream.read(&mut byte) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => seen.push(byte[0]),
                }
            }
            let head = String::from_utf8_lossy(&seen).to_ascii_lowercase();
            let len: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            let mut req = vec![0u8; len];
            let _ = stream.read_exact(&mut req);
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        }
    });
    port
}

fn delta(text: &str) -> String {
    format!(
        r#"{{"id":"x","choices":[{{"index":0,"delta":{{"content":{}}},"finish_reason":null}}]}}"#,
        serde_json::Value::String(text.to_string())
    )
}

fn finish(reason: &str) -> String {
    format!(r#"{{"id":"x","choices":[{{"index":0,"delta":{{}},"finish_reason":"{reason}"}}]}}"#)
}

#[tokio::test]
async fn spawn_real_child() {
    let port = spawn_sse_server(vec![delta("- [x] task — CHILD-ANSWER"), finish("stop")]);
    let cwd = std::env::temp_dir().join(format!("nanopi-agent-spawn-{port}"));
    let _ = std::fs::remove_dir_all(&cwd);
    let home = cwd.join("home");
    std::fs::create_dir_all(&home).unwrap();

    let spec = ChildLaunchSpec {
        model: Some("fake-model".into()),
        base_url: Some(format!("http://127.0.0.1:{port}")),
        api_kind: Some("openai".into()),
        api_key: Some("not-a-real-key".into()),
        trust: Some(false),
        timeout: std::time::Duration::from_secs(60),
        ..ChildLaunchSpec::default()
    };

    // Contract sanity on the pure builders the launcher uses.
    let argv = build_child_args(&spec, &cwd, &[], None);
    assert!(!argv.iter().any(|a| a.contains("not-a-real-key")));
    let env = build_child_env(&spec, "a1");
    assert!(env
        .iter()
        .any(|(k, v)| k == "OPENAI_API_KEY" && v == "not-a-real-key"));

    // Isolate HOME via `env`, then exec the real binary.
    let program = ChildProgram {
        program: PathBuf::from("env"),
        leading_args: vec![
            format!("HOME={}", home.display()),
            format!("NANOPI_HOME={}", home.join(".nanopi").display()),
            env!("CARGO_BIN_EXE_nanopi").to_string(),
        ],
    };
    let reg = AgentRegistry::new(&AgentLimits::default());
    let tool = AgentTool::with_parts(reg.clone(), spec, program);
    let agent = AgentConfig {
        name: "scout".into(),
        description: "test agent".into(),
        tools: Some(vec!["read".into(), "ls".into()]),
        model: None,
        system_prompt: "You are a scout.".into(),
        source: AgentSource::User,
        file_path: PathBuf::from("/nonexistent/scout.md"),
    };

    let out = run_single(&tool.launcher(), &agent, "SPAWN-TASK", &cwd, None, None).await;
    assert!(!out.is_error, "child failed: {}", out.content);

    let dir = cwd.join(".nanopi/agents").join(reg.run_id()).join("a1");
    assert!(dir.join("brief.md").is_file(), "brief.md in agent dir");
    assert!(
        dir.join("transcript.jsonl").is_file(),
        "transcript.jsonl in agent dir"
    );
    let report = dir.join("report.md");
    assert!(report.is_file(), "report.md in agent dir");
    let meta = out.metadata.as_ref().unwrap();
    assert_eq!(
        meta["report_path"].as_str().unwrap(),
        report.to_string_lossy()
    );
    assert_eq!(meta["status"], "completed");
    let report_text = std::fs::read_to_string(&report).unwrap();
    assert!(
        out.content.contains(report_text.trim()),
        "result text is report.md"
    );
    let t = std::fs::read_to_string(dir.join("transcript.jsonl")).unwrap();
    assert!(t.contains("SPAWN-TASK"), "{t}");

    let snap = reg.snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].state, AgentState::Completed);
    assert!(snap[0].pid.is_some());
    let _ = std::fs::remove_dir_all(&cwd);
}

/// CTL-01/CTL-05: `background: true` returns immediately while the
/// child is still live, then finishes in the background with a
/// report.md and a batched outbox entry once `wait_background` drains.
#[tokio::test]
async fn background_dispatch_does_not_block_caller() {
    let port = spawn_sse_server(vec![delta("- [x] task — BG-ANSWER"), finish("stop")]);
    let cwd = std::env::temp_dir().join(format!("nanopi-agent-bg-{port}"));
    let _ = std::fs::remove_dir_all(&cwd);
    let home = cwd.join("home");
    std::fs::create_dir_all(&home).unwrap();

    let spec = ChildLaunchSpec {
        model: Some("fake-model".into()),
        base_url: Some(format!("http://127.0.0.1:{port}")),
        api_kind: Some("openai".into()),
        api_key: Some("not-a-real-key".into()),
        trust: Some(false),
        timeout: std::time::Duration::from_secs(60),
        ..ChildLaunchSpec::default()
    };
    let program = ChildProgram {
        program: PathBuf::from("env"),
        leading_args: vec![
            format!("HOME={}", home.display()),
            format!("NANOPI_HOME={}", home.join(".nanopi").display()),
            env!("CARGO_BIN_EXE_nanopi").to_string(),
        ],
    };
    let reg = AgentRegistry::new(&AgentLimits::default());
    let tool = AgentTool::with_parts(reg.clone(), spec, program);

    let ctx = ToolContext { cwd: cwd.clone() };
    let args = serde_json::json!({"task": "BG-TASK", "background": true});
    let out = tool.execute(args, &ctx).await.expect("in-band dispatch");
    assert!(!out.is_error, "{}", out.content);
    let v: serde_json::Value = serde_json::from_str(&out.content).expect("json envelope");
    let id = v["id"].as_str().expect("id").to_string();
    assert_eq!(id, "a1");
    assert!(
        matches!(v["state"].as_str(), Some("queued") | Some("running")),
        "{v}"
    );
    assert!(v["archive_path"].as_str().unwrap().contains("a1"));

    // The caller gets its turn back immediately: the entry is still
    // non-terminal right after the call returns.
    let snap = reg.snapshot();
    assert_eq!(snap.len(), 1);
    assert!(!snap[0].state.is_terminal(), "must not block until done");

    reg.wait_background().await;

    let snap = reg.snapshot();
    assert!(snap[0].state.is_terminal(), "{:?}", snap[0].state);
    let dir = cwd.join(".nanopi/agents").join(reg.run_id()).join("a1");
    assert!(dir.join("report.md").is_file());

    let batch = reg.take_reports().expect("one pending report");
    assert!(
        batch.contains("[agent a1 finished:"),
        "outbox entry missing: {batch}"
    );
    let _ = std::fs::remove_dir_all(&cwd);
}

/// CTL-06/D-05: `send_message` to a finished agent re-runs it under the
/// same id and dir; the previous report.md is kept, with the new run's
/// report appended after a `## Continued` marker.
#[tokio::test]
async fn continue_finished_agent_same_id() {
    let port = spawn_sse_server(vec![delta("- [x] task — FIRST-ANSWER"), finish("stop")]);
    let cwd = std::env::temp_dir().join(format!("nanopi-agent-continue-{port}"));
    let _ = std::fs::remove_dir_all(&cwd);
    let home = cwd.join("home");
    std::fs::create_dir_all(&home).unwrap();

    let spec = ChildLaunchSpec {
        model: Some("fake-model".into()),
        base_url: Some(format!("http://127.0.0.1:{port}")),
        api_kind: Some("openai".into()),
        api_key: Some("not-a-real-key".into()),
        trust: Some(false),
        timeout: std::time::Duration::from_secs(60),
        ..ChildLaunchSpec::default()
    };
    let program = ChildProgram {
        program: PathBuf::from("env"),
        leading_args: vec![
            format!("HOME={}", home.display()),
            format!("NANOPI_HOME={}", home.join(".nanopi").display()),
            env!("CARGO_BIN_EXE_nanopi").to_string(),
        ],
    };
    let reg = AgentRegistry::new(&AgentLimits::default());
    let tool = AgentTool::with_parts(reg.clone(), spec, program);
    let agent = AgentConfig {
        name: "scout".into(),
        description: "test agent".into(),
        tools: Some(vec!["read".into()]),
        model: None,
        system_prompt: "You are a scout.".into(),
        source: AgentSource::User,
        file_path: PathBuf::from("/nonexistent/scout.md"),
    };

    let out = run_single(&tool.launcher(), &agent, "FIRST-TASK", &cwd, None, None).await;
    assert!(!out.is_error, "first run failed: {}", out.content);
    let dir = cwd.join(".nanopi/agents").join(reg.run_id()).join("a1");
    let first_report = std::fs::read_to_string(dir.join("report.md")).unwrap();
    assert!(!first_report.trim().is_empty());

    // Second run's SSE response is served by a fresh listener on a new
    // port (the first listener only answered one request shape).
    let port2 = spawn_sse_server(vec![delta("- [x] task — SECOND-ANSWER"), finish("stop")]);

    let ctl_tool = SendMessageTool::with_registry(reg.clone());
    let ctx = ToolContext { cwd: cwd.clone() };
    let args = serde_json::json!({"id": "a1", "message": "do more, hit the new port"});
    // Point this agent's next run at port2 by rewriting the spec baked
    // into the shared Launcher (re-create the tool with the new port).
    let spec2 = ChildLaunchSpec {
        model: Some("fake-model".into()),
        base_url: Some(format!("http://127.0.0.1:{port2}")),
        api_kind: Some("openai".into()),
        api_key: Some("not-a-real-key".into()),
        trust: Some(false),
        timeout: std::time::Duration::from_secs(60),
        ..ChildLaunchSpec::default()
    };
    nanopi::tool::agent::set_launch_spec(spec2);

    let out = ctl_tool.execute(args, &ctx).await.expect("send_message");
    assert!(!out.is_error, "{}", out.content);
    let v: serde_json::Value = serde_json::from_str(&out.content).unwrap();
    assert_eq!(v["delivered"], "continuing");

    reg.wait_background().await;

    let brief = std::fs::read_to_string(dir.join("brief.md")).unwrap();
    assert!(brief.contains("## Amendment 1"), "{brief}");
    assert!(brief.contains("do more, hit the new port"), "{brief}");

    let final_report = std::fs::read_to_string(dir.join("report.md")).unwrap();
    assert!(
        final_report.contains("## Continued"),
        "missing continuation marker: {final_report}"
    );
    assert!(
        final_report.contains(first_report.trim()),
        "previous report lost: {final_report}"
    );

    let batch = reg.take_reports().expect("continuation outbox entry");
    assert!(batch.contains("[agent a1 finished:"), "{batch}");

    let _ = std::fs::remove_dir_all(&cwd);
}

// --- ISO-01/ISO-02: worktree isolation (plan 04-05) ---

const OK_ENV: &str = r#"{"session_id":"s","model":"m","finish_reason":"stop","duration_ms":1,"usage":{},"messages":[{"role":"assistant","content":"DONE"}],"status":"completed"}"#;

fn agent_fixture() -> AgentConfig {
    AgentConfig {
        name: "scout".into(),
        description: "d".into(),
        tools: Some(vec!["read".into()]),
        model: None,
        system_prompt: "You are a scout.".into(),
        source: AgentSource::User,
        file_path: PathBuf::from("/nonexistent/scout.md"),
    }
}

/// `sh -c script` as the child; argv after the script becomes $0.., so
/// `$4` is the brief path.
fn iso_launcher(script: &str) -> nanopi::tool::agent::Launcher {
    let cfg = AgentLimits::default();
    AgentTool::with_parts(
        AgentRegistry::new(&cfg),
        ChildLaunchSpec::default(),
        ChildProgram {
            program: "sh".into(),
            leading_args: vec!["-c".into(), script.into()],
        },
    )
    .launcher()
}

fn tmp_repo(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "nanopi-iso-{tag}-{}",
        nanopi::util::uuid::v7()
    ));
    std::fs::create_dir_all(&d).unwrap();
    let run = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&d)
            .args(args)
            .output()
            .expect("spawn git");
        assert!(out.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&out.stderr));
    };
    run(&["init", "-q"]);
    run(&["config", "user.name", "nanopi-test"]);
    run(&["config", "user.email", "nanopi-test@example.com"]);
    std::fs::write(d.join("README.md"), "init\n").unwrap();
    run(&["add", "-A"]);
    run(&["commit", "-q", "-m", "init"]);
    d
}

/// ISO-01: `isolation: "worktree"` runs the child with cwd inside
/// `.nanopi/worktrees/<run>-<id>` on branch `nanopi/<run>/<id>`; the
/// brief carries both, and an unchanged worktree is removed on finish
/// with "removed" recorded in both report.md and brief.md.
#[tokio::test]
async fn worktree_isolation_creates_branch_and_path_and_removes_when_unchanged() {
    let repo = tmp_repo("create");
    let script = format!("pwd > \"$(dirname \"$4\")/where\"; echo '{OK_ENV}'");
    let l = iso_launcher(&script);
    let out = run_single(&l, &agent_fixture(), "t", &repo, None, Some("worktree")).await;
    assert!(!out.is_error, "{}", out.content);

    let dir = repo
        .join(".nanopi/agents")
        .join(l.registry.run_id())
        .join("a1");
    let where_path = std::fs::read_to_string(dir.join("where")).unwrap();
    let want = repo.join(".nanopi/worktrees").join(format!("{}-a1", l.registry.run_id()));
    let repo_canon = std::fs::canonicalize(&repo).unwrap();
    let want_canon = repo_canon
        .join(".nanopi/worktrees")
        .join(format!("{}-a1", l.registry.run_id()));
    // The worktree is already removed by the time we get here (unchanged
    // -> finish() cleans it up), so compare the path the child recorded
    // textually rather than via `canonicalize` (which requires the path
    // to still exist).
    assert_eq!(
        where_path.trim(),
        want_canon.to_string_lossy(),
        "child did not run inside the worktree"
    );

    let brief = std::fs::read_to_string(dir.join("brief.md")).unwrap();
    assert_eq!(
        nanopi::agent::brief::front_matter_get(&brief, "worktree").as_deref(),
        Some(want.to_string_lossy().into_owned()).as_deref()
    );
    let branch_want = format!("nanopi/{}/a1", l.registry.run_id());
    assert_eq!(
        nanopi::agent::brief::front_matter_get(&brief, "branch").as_deref(),
        Some(branch_want.as_str())
    );
    assert_eq!(
        nanopi::agent::brief::front_matter_get(&brief, "worktree_outcome").as_deref(),
        Some("removed")
    );
    assert!(!want.exists(), "unchanged worktree should be removed");

    let report = std::fs::read_to_string(dir.join("report.md")).unwrap();
    assert!(report.contains("worktree: no changes, removed"), "{report}");

    let _ = std::fs::remove_dir_all(&repo);
}

/// ISO-01/D-08: outside a git repo, isolation is ignored and the tool
/// result carries a warning rather than failing the dispatch.
#[tokio::test]
async fn worktree_isolation_warns_outside_git_repo() {
    let non_repo = std::env::temp_dir().join(format!(
        "nanopi-iso-norepo-{}",
        nanopi::util::uuid::v7()
    ));
    std::fs::create_dir_all(&non_repo).unwrap();
    let script = format!("pwd > \"$(dirname \"$4\")/where\"; echo '{OK_ENV}'");
    let l = iso_launcher(&script);
    let out = run_single(&l, &agent_fixture(), "t", &non_repo, None, Some("worktree")).await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("Warning:"), "{}", out.content);
    assert!(out.content.contains("not a git repository"), "{}", out.content);

    let dir = non_repo
        .join(".nanopi/agents")
        .join(l.registry.run_id())
        .join("a1");
    let where_path = std::fs::read_to_string(dir.join("where")).unwrap();
    assert_eq!(
        std::fs::canonicalize(where_path.trim()).unwrap(),
        std::fs::canonicalize(&non_repo).unwrap(),
        "child should run in the normal cwd, unisolated"
    );

    let _ = std::fs::remove_dir_all(&non_repo);
}

/// Invalid isolation value is an in-band error before anything spawns.
#[tokio::test]
async fn worktree_isolation_invalid_value_is_in_band_error() {
    let cwd = std::env::temp_dir().join(format!("nanopi-iso-bad-{}", nanopi::util::uuid::v7()));
    std::fs::create_dir_all(&cwd).unwrap();
    let ctx = ToolContext { cwd: cwd.clone() };
    let tool = AgentTool::with_parts(
        AgentRegistry::new(&AgentLimits::default()),
        ChildLaunchSpec::default(),
        ChildProgram {
            program: "sh".into(),
            leading_args: vec!["-c".into(), format!("echo '{OK_ENV}'")],
        },
    );
    let args = serde_json::json!({"task": "t", "isolation": "bogus"});
    let err = tool.execute(args, &ctx).await.expect_err("should reject before spawn");
    let msg = format!("{err:?}");
    assert!(msg.contains("worktree"), "{msg}");
    assert!(!cwd.join(".nanopi").exists(), "no agent dir should have been reserved");

    let _ = std::fs::remove_dir_all(&cwd);
}

/// ISO-02: a worktree with a change is merged into the main tree and
/// cleaned up; the outcome is recorded in report.md and brief.md, and
/// (for background dispatch) injected into the outbox text reaching
/// the main agent.
#[tokio::test]
async fn worktree_merge_no_conflict_auto_merges_and_cleans_up_background() {
    let repo = tmp_repo("merge");
    let script = "echo feature > \"$PWD/feature.txt\"; echo '{OK_ENV}'".replace("{OK_ENV}", OK_ENV);
    let l = iso_launcher(&script);
    let reg = l.registry.clone();

    let tool = AgentTool::with_parts(reg.clone(), l.spec.clone(), l.program.clone());
    let ctx = ToolContext { cwd: repo.clone() };
    let args = serde_json::json!({"task": "t", "background": true, "isolation": "worktree"});
    let out = tool.execute(args, &ctx).await.expect("dispatch");
    assert!(!out.is_error, "{}", out.content);

    reg.wait_background().await;

    assert!(repo.join("feature.txt").exists(), "merged file missing from main tree");
    let dir = repo.join(".nanopi/agents").join(reg.run_id()).join("a1");
    let report = std::fs::read_to_string(dir.join("report.md")).unwrap();
    assert!(report.contains("worktree: merged"), "{report}");
    let brief = std::fs::read_to_string(dir.join("brief.md")).unwrap();
    assert_eq!(
        nanopi::agent::brief::front_matter_get(&brief, "worktree_outcome").as_deref(),
        Some("merged")
    );

    let batch = reg.take_reports().expect("one pending report");
    assert!(batch.contains("worktree: merged"), "{batch}");

    let _ = std::fs::remove_dir_all(&repo);
}

// A deterministic merge-conflict scenario needs the main tree to move
// *after* the worktree's base is captured but *before* `finish()`'s
// merge attempt — control only available with direct access to
// `prepare_run`/`run_body` (`pub(crate)`). See
// `tool::agent::tests::worktree_merge_conflict_aborts_and_keeps_branch`
// in src/tool/agent.rs for that scenario.
