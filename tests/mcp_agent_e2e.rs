//! MCP for dispatched agents, end to end (M10): a print-mode parent with
//! the `agent` tool, a fake stdio MCP server, and a scripted fake model.
//!
//! Orchestrator mode is TUI-only, so it cannot be driven through `-p`.
//! What can be driven is everything it relies on: the parent holds the one
//! connection to each server, a dispatched agent is granted tools by name,
//! and the agent — a separate `nanopi -p` process that never connects to a
//! server itself — calls them through the parent.
//!
//! Nothing here sleeps to *wait for* something. Every wait is on an
//! observable fact (a file, a pid, `ps` no longer finding a process) with
//! a deadline that only fires when something is genuinely broken, because
//! `print_mode_ctrl_c_stops_all_background_agents` was flaky for exactly
//! that reason.

#![cfg(unix)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

// ------------------------------------------------------- fake MCP server

/// One tool, `echo`. Every call is logged and then *held until two calls
/// have arrived*, so the server only ever answers if two agents are
/// calling it at the same time — which makes "two agents, one server" a
/// property the test needs rather than one it hopes for. The bound on the
/// wait only exists to turn a broken run into a failed assertion instead
/// of a hang.
const SERVER_SH: &str = r#"
echo $$ >> "$PIDS"
handle_call() {
    text=$(printf '%s' "$1" | sed -n 's/.*"text":"\([^"]*\)".*/\1/p')
    printf '%s\n' "$text" >> "$CALLS"
    n=0
    while [ "$(( $(wc -l < "$CALLS") ))" -lt 2 ] && [ "$n" -lt 1500 ]; do sleep 0.01; n=$((n+1)); done
    if [ "$(( $(wc -l < "$CALLS") ))" -ge 2 ]; then r="echo:$text"; else r="TIMEOUT:$text"; fi
    printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"%s"}]}}\n' "$2" "$r"
}
while IFS= read -r line; do
    id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
    case "$line" in
        *'"method":"initialize"'*)
            printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"demo","version":"1"}}}\n' "$id" ;;
        *'"method":"tools/list"'*)
            printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"echoes its text argument","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}]}}\n' "$id" ;;
        *'"method":"tools/call"'*)
            handle_call "$line" "$id" & ;;
    esac
done
"#;

const SECRET: &str = "s3cr3t-do-not-print-e2e";

struct Fixture {
    dir: PathBuf,
    pids: PathBuf,
    calls: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("nanopi-mcp-agent-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("home/.nanopi");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(dir.join("tmp")).expect("tmp");
        let script = dir.join("server.sh");
        std::fs::write(&script, SERVER_SH).expect("script");
        let (pids, calls) = (dir.join("server.pids"), dir.join("calls.log"));
        let config = json!({
            "mcpServers": { "demo": {
                "command": "sh",
                "args": [script],
                "env": { "PIDS": pids, "CALLS": calls, "API_TOKEN": SECRET }
            }}
        });
        std::fs::write(home.join("mcp.json"), config.to_string()).expect("mcp.json");
        Self { dir, pids, calls }
    }

    fn spawn(&self, port: u16) -> std::process::Child {
        Command::new(env!("CARGO_BIN_EXE_nanopi"))
            .current_dir(&self.dir)
            .args(["-p", "--output", "json", "--base-url"])
            .arg(format!("http://127.0.0.1:{port}"))
            .args(["--model", "fake-model", "--api-key", "not-a-real-key"])
            .args(["--no-hooks", "--no-skills", "--no-context-files"])
            .arg("go")
            .env("HOME", self.dir.join("home"))
            .env("NANOPI_HOME", self.dir.join("home/.nanopi"))
            // Its own `$TMPDIR`, so this run's channel directory is
            // findable and nothing else's is in the way.
            .env("TMPDIR", self.dir.join("tmp"))
            .env_remove("NANOPI_AGENT_ID")
            .env_remove("NANOPI_PARENT_PID")
            .env_remove("NANOPI_RUN_ID")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn nanopi")
    }

    fn server_pids(&self) -> Vec<u32> {
        std::fs::read_to_string(&self.pids)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect()
    }

    fn calls(&self) -> Vec<String> {
        let mut c: Vec<String> = std::fs::read_to_string(&self.calls)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect();
        c.sort();
        c
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Alive = exists and is not a zombie.
fn alive(pid: u32) -> bool {
    let out = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("ps");
    let stat = String::from_utf8_lossy(&out.stdout);
    let stat = stat.trim();
    !stat.is_empty() && !stat.starts_with('Z')
}

fn assert_dies(what: &str, pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while alive(pid) {
        assert!(Instant::now() < deadline, "{what} (pid {pid}) is still running");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Wait for the run to end, failing with the process's own output rather
/// than a bare timeout.
fn wait_exit(child: &mut std::process::Child) -> (std::process::ExitStatus, String, String) {
    let deadline = Instant::now() + Duration::from_secs(90);
    let status = loop {
        if let Some(s) = child.try_wait().expect("try_wait") {
            break s;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("nanopi hung");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let (mut out, mut err) = (String::new(), String::new());
    if let Some(mut s) = child.stdout.take() {
        let _ = s.read_to_string(&mut out);
    }
    if let Some(mut s) = child.stderr.take() {
        let _ = s.read_to_string(&mut err);
    }
    (status, out, err)
}

// ------------------------------------------------------- scripted model

fn sse(chunks: &[String]) -> String {
    let mut body = String::new();
    for c in chunks {
        body.push_str("data: ");
        body.push_str(c);
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    body
}

fn text(t: &str) -> Vec<String> {
    vec![
        json!({"id":"x","choices":[{"index":0,"delta":{"content":t},"finish_reason":null}]}).to_string(),
        json!({"id":"x","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}).to_string(),
    ]
}

fn call(id: &str, name: &str, arguments: &Value) -> Vec<String> {
    vec![
        json!({"id":"x","choices":[{"index":0,"delta":{"tool_calls":[{
            "index":0,"id":id,"type":"function",
            "function":{"name":name,"arguments":arguments.to_string()}
        }]},"finish_reason":null}]})
        .to_string(),
        json!({"id":"x","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}).to_string(),
    ]
}

fn tool_names(req: &Value) -> Vec<String> {
    req["tools"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|t| t["function"]["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn roles(req: &Value) -> Vec<String> {
    req["messages"]
        .as_array()
        .map(|a| a.iter().filter_map(|m| m["role"].as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

/// The content of the last `tool` message, however the provider shaped it.
fn last_tool_result(req: &Value) -> String {
    req["messages"]
        .as_array()
        .and_then(|a| a.iter().rev().find(|m| m["role"] == "tool"))
        .map(|m| match &m["content"] {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default()
}

/// What the scripted model says to one request. Parent and child are
/// told apart by the tool set they were offered: only the parent has
/// `agent`.
///
/// Parent: dispatch two agents at once → continue one of them → finish →
/// acknowledge the continued agent's report.
/// Child: call `echo` with a text taken from its brief, then report
/// whatever came back.
fn respond(raw: &str) -> Vec<String> {
    let req: Value = serde_json::from_str(raw).expect("request is JSON");
    let tools = tool_names(&req);
    let roles = roles(&req);
    let last = roles.last().cloned().unwrap_or_default();

    if tools.iter().any(|t| t == "agent") {
        let tool_msgs = roles.iter().filter(|r| *r == "tool").count();
        return match (tool_msgs, last.as_str()) {
            (0, _) => call(
                "p1",
                "agent",
                &json!({"tasks": [
                    {"task": "MARK-ONE call the echo tool", "tools": ["mcp__demo__echo"]},
                    {"task": "MARK-TWO call the echo tool", "tools": ["mcp__demo__echo"]},
                ]}),
            ),
            (1, "tool") => call(
                "p2",
                "send_message",
                &json!({"id": "a1", "message": "CONTINUE-NOW call the echo tool again"}),
            ),
            (_, "tool") => text("continuing a1"),
            // The report of the continued agent, injected after the drain.
            _ => text("ack"),
        };
    }

    if last == "tool" {
        return text(&format!("REPORT {}", last_tool_result(&req)));
    }
    // Has the tool already been called since the task this request is
    // working on began? The brief's self-check turn is a user message
    // that arrives *after* the report was given (and, for a continued
    // agent, repeats the amendment), so "last message is a user message"
    // alone would call the tool a second time. The task began at the
    // first message naming CONTINUE-NOW, or at the start.
    let messages = req["messages"].as_array().cloned().unwrap_or_default();
    let continued = messages
        .iter()
        .position(|m| m["role"] == "user" && m["content"].to_string().contains("CONTINUE-NOW"));
    let task_began = continued.unwrap_or(0);
    let answered = messages[task_began..].iter().any(|m| m["role"] == "tool");
    if answered {
        return text(&format!("REPORT {}", last_tool_result(&req)));
    }
    let continued = continued.is_some();
    let body = req.to_string();
    let say = if continued {
        "cont"
    } else if body.contains("MARK-ONE") {
        "one"
    } else {
        "two"
    };
    call("c1", "mcp__demo__echo", &json!({ "text": say }))
}

struct Model {
    port: u16,
    requests: Arc<Mutex<Vec<String>>>,
}

fn spawn_model(respond: fn(&str) -> Vec<String>) -> Model {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let requests2 = requests.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let requests = requests2.clone();
            // A thread per connection: parent and children talk to this
            // server at the same time.
            std::thread::spawn(move || {
                let mut seen = Vec::new();
                let mut byte = [0u8; 1];
                while !seen.ends_with(b"\r\n\r\n") {
                    match stream.read(&mut byte) {
                        Ok(0) | Err(_) => return,
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
                if stream.read_exact(&mut req).is_err() {
                    return;
                }
                let req = String::from_utf8_lossy(&req).into_owned();
                requests.lock().unwrap().push(req.clone());
                let body = sse(&respond(&req));
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            });
        }
    });
    Model { port, requests }
}

/// Every `.nanopi/agents/<run>/<id>/report.md` under `dir`, by id.
fn report_of(dir: &Path, id: &str) -> String {
    let agents = dir.join(".nanopi/agents");
    for run in std::fs::read_dir(&agents).expect("agents dir").flatten() {
        let p = run.path().join(id).join("report.md");
        if let Ok(t) = std::fs::read_to_string(p) {
            return t;
        }
    }
    panic!("no report for {id} under {}", agents.display());
}

// ------------------------------------------------------------- the test

/// The whole feature through the default dispatch path: two agents named
/// `mcp__demo__echo` in their `tools`, running at the same time, each
/// calling the one server through the parent; the results reach their
/// reports and the parent; then one agent is continued and still has the
/// tool; and afterwards nothing of ours is left.
#[test]
fn dispatched_agents_call_mcp_tools_through_the_parent_and_leave_nothing_behind() {
    let fx = Fixture::new("main");
    let model = spawn_model(respond);

    let mut parent = fx.spawn(model.port);
    let parent_pid = parent.id();
    let (status, stdout, stderr) = wait_exit(&mut parent);
    assert!(status.success(), "{status:?}\nstdout={stdout}\nstderr={stderr}");

    // One server for the parent and both agents: the second agent did
    // not start one, and neither did the continued one.
    let pids = fx.server_pids();
    assert_eq!(pids.len(), 1, "exactly one MCP server process was ever started: {pids:?}");

    // Both agents' calls reached the server — and had to be in flight
    // together for it to answer at all (see `SERVER_SH`) — and the
    // continued agent called it a third time.
    assert_eq!(fx.calls(), vec!["cont", "one", "two"], "stderr={stderr}");

    let requests: Vec<Value> = model
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|r| serde_json::from_str(r).unwrap())
        .collect();
    let (parent_reqs, child_reqs): (Vec<&Value>, Vec<&Value>) =
        requests.iter().partition(|r| tool_names(r).iter().any(|t| t == "agent"));

    // The child's registry is exactly what it was granted: no built-in,
    // no `agent`, nothing else the parent offers.
    assert!(!child_reqs.is_empty());
    for r in &child_reqs {
        assert_eq!(tool_names(r), vec!["mcp__demo__echo"], "a child's tools are its grant and nothing more");
    }

    // The tool's answer reached each child's report, and the parent saw
    // those reports.
    for (id, answer) in [("a1", "echo:one"), ("a2", "echo:two")] {
        let report = report_of(&fx.dir, id);
        assert!(report.contains(answer), "{id}'s report lacks {answer}: {report}");
    }
    assert!(
        report_of(&fx.dir, "a1").contains("echo:cont"),
        "the continued agent called the tool again: {}",
        report_of(&fx.dir, "a1")
    );
    let second_parent_request = parent_reqs[1].to_string();
    assert!(
        second_parent_request.contains("echo:one") && second_parent_request.contains("echo:two"),
        "the agents' reports must reach the parent: {second_parent_request}"
    );

    // A server's `env` is its own: neither a model nor any output ever
    // saw it.
    assert!(!stdout.contains(SECRET) && !stderr.contains(SECRET));
    assert!(!requests.iter().any(|r| r.to_string().contains(SECRET)));

    // Nothing of ours survives: the server is gone and so is the
    // channel's socket directory.
    assert_dies("MCP server", pids[0]);
    assert!(
        !fx.ipc_dir_for(parent_pid).exists(),
        "the channel directory survived the run: {}",
        fx.ipc_dir_for(parent_pid).display()
    );
}

impl Fixture {
    fn ipc_dir_for(&self, parent_pid: u32) -> PathBuf {
        self.dir.join("tmp").join(format!("nanopi-{parent_pid}"))
    }
}

/// A name the parent's channel does not serve is refused before anything
/// is spawned, and the refusal lists what does exist — the model's way
/// back from a typo. No agent, so no child process and no grant.
#[test]
fn an_unknown_mcp_tool_is_refused_at_dispatch_with_the_available_names() {
    fn respond_unknown(raw: &str) -> Vec<String> {
        let req: Value = serde_json::from_str(raw).expect("request is JSON");
        if roles(&req).iter().any(|r| r == "tool") {
            return text("done");
        }
        call("p1", "agent", &json!({"task": "go", "tools": ["mcp__demo__nope"]}))
    }
    let fx = Fixture::new("unknown");
    let model = spawn_model(respond_unknown);

    let mut parent = fx.spawn(model.port);
    let (status, stdout, stderr) = wait_exit(&mut parent);
    assert!(status.success(), "{status:?}\nstdout={stdout}\nstderr={stderr}");

    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2, "no agent was dispatched, so no child ever spoke to the model");
    assert!(
        requests[1].contains("unknown MCP tool(s): mcp__demo__nope")
            && requests[1].contains("Available MCP tools: mcp__demo__echo"),
        "{}",
        requests[1]
    );
    assert!(!fx.dir.join(".nanopi/agents").exists(), "nothing was reserved or spawned");
    for pid in fx.server_pids() {
        assert_dies("MCP server", pid);
    }
}
