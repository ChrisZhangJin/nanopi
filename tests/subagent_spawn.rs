//! Integration: the parent supervisor (01-05) spawns the real `nanopi`
//! binary as a child with `build_child_args` / `build_child_env` against a
//! hermetic fake OpenAI endpoint, and the D-03 agent-dir layout lands.
#![cfg(unix)]

use std::io::{Read, Write};
use std::path::PathBuf;

use nanopi::agent::agents::{AgentConfig, AgentSource};
use nanopi::config::SubagentConfig;
use nanopi::subagent_registry::{AgentState, SubagentRegistry};
use nanopi::tool::subagent::{
    build_child_args, build_child_env, run_single, ChildLaunchSpec, ChildProgram, SubagentTool,
};

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
    let cwd = std::env::temp_dir().join(format!("nanopi-subagent-spawn-{port}"));
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
    let reg = SubagentRegistry::new(&SubagentConfig::default());
    let tool = SubagentTool::with_parts(reg.clone(), spec, program);
    let agent = AgentConfig {
        name: "scout".into(),
        description: "test agent".into(),
        tools: Some(vec!["read".into(), "ls".into()]),
        model: None,
        system_prompt: "You are a scout.".into(),
        source: AgentSource::User,
        file_path: PathBuf::from("/nonexistent/scout.md"),
    };

    let out = run_single(&tool.launcher(), &agent, "SPAWN-TASK", &cwd).await;
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
