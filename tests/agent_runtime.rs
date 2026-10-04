//! End-to-end proof of the Phase 1 success criteria (01-07): a real parent
//! `nanopi -p` drives the `agent` tool, which spawns real `nanopi -p`
//! children (same binary). Parent and child traffic share one scripted
//! fake OpenAI endpoint; the handler tells them apart by whether the
//! request's tools array advertises `agent` (children never get it).
//!
//! Run in debug and with `--release` (panic = "abort").
#![cfg(unix)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type Handler = Arc<dyn Fn(&str) -> Vec<String> + Send + Sync>;
type Log = Arc<Mutex<Vec<String>>>;

// ── fake endpoint ──

/// One thread per connection so parallel children are served concurrently.
fn spawn_server(handler: Handler) -> (u16, Log) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().unwrap().port();
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let log2 = log.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let handler = handler.clone();
            let log = log2.clone();
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
                let _ = stream.read_exact(&mut req);
                let req = String::from_utf8_lossy(&req).to_string();
                log.lock().unwrap().push(req.clone());
                let chunks = handler(&req);
                let mut body = String::new();
                for c in &chunks {
                    body.push_str("data: ");
                    body.push_str(c);
                    body.push_str("\n\n");
                }
                body.push_str("data: [DONE]\n\n");
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
    (port, log)
}

fn text(s: &str) -> Vec<String> {
    vec![
        format!(
            r#"{{"id":"x","choices":[{{"index":0,"delta":{{"content":{}}},"finish_reason":null}}]}}"#,
            serde_json::Value::String(s.to_string())
        ),
        r#"{"id":"x","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#.to_string(),
    ]
}

fn call(name: &str, args: serde_json::Value) -> Vec<String> {
    vec![
        format!(
            r#"{{"id":"x","choices":[{{"index":0,"delta":{{"tool_calls":[{{"index":0,"id":"c{}","type":"function","function":{{"name":"{name}","arguments":{}}}}}]}},"finish_reason":null}}]}}"#,
            std::process::id(),
            serde_json::Value::String(args.to_string())
        ),
        r#"{"id":"x","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#.to_string(),
    ]
}

/// The parent is the only process whose tools include `agent`.
fn is_parent(req: &str) -> bool {
    let v: serde_json::Value = serde_json::from_str(req).unwrap_or_default();
    v["tools"]
        .as_array()
        .map(|a| a.iter().any(|t| t["function"]["name"] == "agent"))
        .unwrap_or(false)
}

fn tool_results(req: &str) -> usize {
    let v: serde_json::Value = serde_json::from_str(req).unwrap_or_default();
    v["messages"]
        .as_array()
        .map(|a| a.iter().filter(|m| m["role"] == "tool").count())
        .unwrap_or(0)
}

// ── sandbox ──

struct Sandbox {
    root: PathBuf,
    home: PathBuf,
    cwd: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "nanopi-rt-{tag}-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("home");
        let cwd = root.join("work");
        std::fs::create_dir_all(home.join("agents")).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        Sandbox { root, home, cwd }
    }

    fn agent(&self, name: &str, tools: &str) {
        std::fs::write(
            self.home.join("agents").join(format!("{name}.md")),
            format!("---\nname: {name}\ndescription: test agent\ntools: {tools}\n---\nYou are {name}.\n"),
        )
        .unwrap();
    }

    fn config(&self, body: &str) {
        std::fs::write(self.home.join("config.toml"), body).unwrap();
    }

    fn nanopi(&self, port: u16, msg: &str) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_nanopi"));
        c.current_dir(&self.cwd)
            .args(["-p", "--output", "json", "--base-url"])
            .arg(format!("http://127.0.0.1:{port}"))
            .args(["--model", "fake-model", "--approve"])
            .args(["--no-hooks", "--no-skills", "--no-context-files"])
            .arg(msg)
            .env("OPENAI_API_KEY", "not-a-real-key")
            .env("NANOPI_HOME", &self.home)
            .env("HOME", &self.root)
            .env_remove("NANOPI_AGENT_ID")
            .env_remove("NANOPI_PARENT_PID")
            .stdin(Stdio::null());
        c
    }

    fn run(&self, port: u16, msg: &str) -> (Output, serde_json::Value) {
        let out = self.nanopi(port, msg).output().expect("run nanopi");
        let v = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "parent envelope not JSON ({e}): stdout={} stderr={}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        });
        (out, v)
    }

    /// Every agent dir created by any run in this sandbox.
    fn agent_dirs(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let base = self.cwd.join(".nanopi/agents");
        for run in std::fs::read_dir(&base).into_iter().flatten().flatten() {
            for a in std::fs::read_dir(run.path()).into_iter().flatten().flatten() {
                if a.path().is_dir() {
                    out.push(a.path());
                }
            }
        }
        out.sort();
        out
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Tool-result texts in a parent envelope.
fn tool_texts(v: &serde_json::Value) -> Vec<String> {
    v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap_or("").to_string())
        .collect()
}

/// A zombie counts as dead (the container's PID 1 does not reap).
fn alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => false,
        Ok(s) => {
            let state = s.rsplit(") ").next().and_then(|r| r.chars().next());
            !matches!(state, Some('Z') | Some('X'))
        }
    }
}

fn pgrep_live(marker: &str) -> Vec<u32> {
    let out = Command::new("pgrep").args(["-f", marker]).output();
    let Ok(out) = out else { return Vec::new() };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .filter(|p| alive(*p))
        .collect()
}

fn wait_file(p: &Path, secs: u64) -> String {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Ok(s) = std::fs::read_to_string(p) {
            if !s.trim().is_empty() {
                return s;
            }
        }
        assert!(Instant::now() < deadline, "timed out waiting for {}", p.display());
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_exit(child: &mut Child, secs: u64) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(s) = child.try_wait().unwrap() {
            return s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("process did not exit");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Parent answers: dispatch `args` first, then a final text once a tool
/// result is in. Children are answered by `child`.
fn handler(
    args: serde_json::Value,
    child: impl Fn(&str) -> Vec<String> + Send + Sync + 'static,
) -> Handler {
    Arc::new(move |req: &str| {
        if is_parent(req) {
            if tool_results(req) == 0 {
                call("agent", args.clone())
            } else {
                text("PARENT-DONE")
            }
        } else {
            child(req)
        }
    })
}

// ── SC1: single / parallel / chain, failure isolation ──

#[test]
fn sc1_single_parallel_chain() {
    let child = |req: &str| {
        if req.contains("T-GAMMA") {
            text("- [x] task GAMMA-RESULT")
        } else if req.contains("T-ALPHA") {
            text("- [x] task ALPHA-RESULT")
        } else if req.contains("T-BETA") {
            text("- [x] task BETA-RESULT")
        } else {
            text("- [x] task OTHER")
        }
    };
    let cases = [
        (
            serde_json::json!({"agent":"scout","task":"T-ALPHA sc1-single"}),
            vec!["ALPHA-RESULT"],
        ),
        (
            serde_json::json!({"tasks":[
                {"agent":"scout","task":"T-ALPHA sc1-par"},
                {"agent":"scout","task":"T-BETA sc1-par"}]}),
            vec!["ALPHA-RESULT", "BETA-RESULT"],
        ),
        (
            serde_json::json!({"chain":[
                {"agent":"scout","task":"T-ALPHA sc1-chain"},
                {"agent":"scout","task":"{previous} then T-GAMMA"}]}),
            vec!["GAMMA-RESULT"],
        ),
    ];
    for (args, expect) in cases {
        let sb = Sandbox::new("sc1");
        sb.agent("scout", "read, ls");
        let (port, _log) = spawn_server(handler(args.clone(), child));
        let (out, v) = sb.run(port, "dispatch sc1");
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(v["status"], "completed", "{v}");
        let tools = tool_texts(&v).join("\n");
        for e in &expect {
            assert!(tools.contains(e), "{args}: missing {e} in {tools}");
        }
        let n = match expect.len() {
            2 => 2,
            _ if args.get("chain").is_some() => 2,
            _ => 1,
        };
        assert_eq!(sb.agent_dirs().len(), n, "{args}");
        for d in sb.agent_dirs() {
            assert!(d.join("report.md").is_file(), "{}", d.display());
        }
    }
}

#[test]
fn sc1_child_panic_isolated() {
    // The child SIGKILLs itself mid-turn: the same observable outcome as a
    // release-build panic (panic = abort) — the process dies on a signal.
    let sb = Sandbox::new("sc1kill");
    sb.agent("worker", "bash");
    let (port, _log) = spawn_server(handler(
        serde_json::json!({"agent":"worker","task":"crash sc1-kill"}),
        |req: &str| {
            if tool_results(req) == 0 {
                call("bash", serde_json::json!({"command":"kill -9 $PPID; sleep 5"}))
            } else {
                text("- [x] should not get here")
            }
        },
    ));
    let (out, v) = sb.run(port, "dispatch crash");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(v["status"], "completed", "{v}");
    let tools = tool_texts(&v).join("\n");
    assert!(tools.contains("Agent failed"), "{tools}");
    assert!(tools.contains("signal 9"), "{tools}");
    let last = v["messages"].as_array().unwrap().last().unwrap().to_string();
    assert!(last.contains("PARENT-DONE"), "parent kept running: {last}");
}

// ── SC2: cancelling / exiting the parent kills the whole child group ──

#[test]
fn sc2_cancel_kills_group() {
    let sb = Sandbox::new("sc2");
    sb.agent("worker", "bash");
    let marker = format!("SC2MARK{}", std::process::id());
    let cmd = format!(
        "bash -c 'sleep 300; true {marker}' >/dev/null 2>&1 & echo $! > gc.pid; echo $PPID > child.pid; sleep 300"
    );
    let (port, _log) = spawn_server(handler(
        serde_json::json!({"agent":"worker","task":"hang sc2"}),
        move |req: &str| {
            if tool_results(req) == 0 {
                call("bash", serde_json::json!({"command": cmd}))
            } else {
                text("- [x] done")
            }
        },
    ));
    let mut parent = sb
        .nanopi(port, "dispatch sc2")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let gc: u32 = wait_file(&sb.cwd.join("gc.pid"), 60).trim().parse().unwrap();
    let ch: u32 = wait_file(&sb.cwd.join("child.pid"), 60).trim().parse().unwrap();
    assert!(alive(gc) && alive(ch));
    assert!(!pgrep_live(&marker).is_empty(), "grandchild is findable");

    unsafe {
        libc::kill(parent.id() as libc::pid_t, libc::SIGTERM);
    }
    let status = wait_exit(&mut parent, 10);
    assert_eq!(status.code(), Some(143), "{status:?}");

    let deadline = Instant::now() + Duration::from_secs(2);
    while (alive(gc) || alive(ch) || !pgrep_live(&marker).is_empty()) && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!alive(ch), "child {ch} survived");
    assert!(!alive(gc), "grandchild {gc} survived");
    assert!(pgrep_live(&marker).is_empty(), "marker processes survived");
}

// ── SC3: transcripts stay in the agent dir ──

#[test]
fn sc3_transcript_isolated() {
    let sb = Sandbox::new("sc3");
    sb.agent("scout", "bash");
    let (port, _log) = spawn_server(handler(
        serde_json::json!({"agent":"scout","task":"look sc3"}),
        |req: &str| {
            if tool_results(req) == 0 {
                call("bash", serde_json::json!({"command":"echo CHILD-INTERNAL-SC3"}))
            } else {
                text("- [x] looked")
            }
        },
    ));
    // Persist the parent session (default), so it can be inspected.
    let (out, _v) = sb.run(port, "dispatch sc3");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let dirs = sb.agent_dirs();
    assert_eq!(dirs.len(), 1);
    assert!(dirs[0].ends_with("a1"), "{}", dirs[0].display());
    let t = std::fs::read_to_string(dirs[0].join("transcript.jsonl")).unwrap();
    assert!(t.contains("CHILD-INTERNAL-SC3"), "child transcript has its work");

    let mut parent_sessions = Vec::new();
    let mut stack = vec![sb.home.clone()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "jsonl") {
                parent_sessions.push(p);
            }
        }
    }
    assert!(!parent_sessions.is_empty(), "parent session persisted");
    for p in parent_sessions {
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(s.contains("agent"), "parent holds the tool call");
        assert!(
            !s.contains("CHILD-INTERNAL-SC3"),
            "child messages leaked into {}",
            p.display()
        );
    }
}

// ── SC4: tool allowlist, turn limit, live cap ──

#[test]
fn sc4_tools_limits_cap() {
    // Allowlist: agent has only `read`; its bash call is refused in-band.
    {
        let sb = Sandbox::new("sc4tools");
        sb.agent("reader", "read");
        let (port, log) = spawn_server(handler(
            serde_json::json!({"agent":"reader","task":"sc4 tools"}),
            |req: &str| {
                if tool_results(req) == 0 {
                    call("bash", serde_json::json!({"command":"echo nope"}))
                } else {
                    text("- [x] done")
                }
            },
        ));
        let (out, _v) = sb.run(port, "dispatch sc4");
        assert!(out.status.success());
        let reqs = log.lock().unwrap().clone();
        let child_after = reqs
            .iter()
            .find(|r| !is_parent(r) && tool_results(r) > 0)
            .expect("child continued after the denied call");
        assert!(child_after.contains("unknown tool"), "{child_after}");
    }
    // max_turns = 1: a looping child stops with limit_reached.
    {
        let sb = Sandbox::new("sc4turns");
        sb.agent("looper", "ls");
        sb.config("[agent]\nmax_turns = 1\n");
        let (port, _log) = spawn_server(handler(
            serde_json::json!({"agent":"looper","task":"sc4 loop"}),
            |_req: &str| call("ls", serde_json::json!({"path":"."})),
        ));
        let (out, v) = sb.run(port, "dispatch sc4 loop");
        assert!(out.status.success());
        let tools = tool_texts(&v).join("\n");
        assert!(tools.contains("max_turns reached"), "{tools}");
    }
    // max_live = 1 with two parallel tasks: one in-band refusal.
    {
        let sb = Sandbox::new("sc4cap");
        sb.agent("scout", "read");
        sb.config("[agent]\nmax_live = 1\n");
        let (port, _log) = spawn_server(handler(
            serde_json::json!({"tasks":[
                {"agent":"scout","task":"sc4 one"},
                {"agent":"scout","task":"sc4 two"}]}),
            |_req: &str| {
                std::thread::sleep(Duration::from_millis(300));
                text("- [x] ok")
            },
        ));
        let (out, v) = sb.run(port, "dispatch sc4 cap");
        assert!(out.status.success());
        let tools = tool_texts(&v).join("\n");
        assert_eq!(
            tools.matches("agent limit reached").count(),
            1,
            "{tools}"
        );
    }
}

// ── SC5: amendments reach a running child; report lists every item ──

#[test]
fn sc5_amendment_and_checklist() {
    let sb = Sandbox::new("sc5");
    sb.agent("scout", "ls");
    let amended = "AMEND-SC5-MARK";
    let (port, log) = spawn_server(handler(
        serde_json::json!({"agent":"scout","task":"sc5 base task"}),
        move |req: &str| {
            if tool_results(req) == 0 {
                // Delay the first child turn so the amendment lands mid-run.
                std::thread::sleep(Duration::from_millis(2500));
                call("ls", serde_json::json!({"path":"."}))
            } else if req.contains(amended) {
                text(&format!("- [x] sc5 base task\n- [x] {amended}"))
            } else {
                text("- [x] sc5 base task")
            }
        },
    ));
    let mut parent = sb
        .nanopi(port, "dispatch sc5")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Wait until the child's first (delayed) request is in flight, then
    // append an amendment to its brief.
    let deadline = Instant::now() + Duration::from_secs(30);
    let brief = loop {
        let child_started = log.lock().unwrap().iter().any(|r| !is_parent(r));
        if let (true, Some(d)) = (child_started, sb.agent_dirs().first()) {
            break d.join("brief.md");
        }
        assert!(Instant::now() < deadline, "no brief");
        std::thread::sleep(Duration::from_millis(20));
    };
    {
        let mut f = std::fs::OpenOptions::new().append(true).open(&brief).unwrap();
        f.write_all(format!("\n## Amendment 1\n\n{amended}\n").as_bytes())
            .unwrap();
    }
    let status = wait_exit(&mut parent, 60);
    assert!(status.success());

    let reqs = log.lock().unwrap().clone();
    let child_reqs: Vec<&String> = reqs.iter().filter(|r| !is_parent(r)).collect();
    assert!(!child_reqs[0].contains(amended), "not in the first turn");
    assert!(
        child_reqs[1..].iter().any(|r| r.contains(amended)),
        "amendment reached the next turn"
    );
    let report = std::fs::read_to_string(brief.with_file_name("report.md")).unwrap();
    assert!(report.contains("sc5 base task"), "{report}");
    assert!(
        report.lines().any(|l| l.contains("- [") && l.contains(amended)),
        "{report}"
    );
}

// ── SC6: cross-process stale write ──

#[test]
fn sc6_cross_process_stale_write() {
    let sb = Sandbox::new("sc6");
    let f = sb.cwd.join("shared.txt");
    std::fs::write(&f, "hello original\n").unwrap();
    let path = f.to_string_lossy().to_string();
    let b_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let b_done2 = b_done.clone();
    let p2 = path.clone();
    let h: Handler = Arc::new(move |req: &str| {
        let n = tool_results(req);
        if req.contains("PROC-A") {
            match n {
                0 => call("read", serde_json::json!({"path": p2})),
                1 => {
                    let deadline = Instant::now() + Duration::from_secs(60);
                    while !b_done2.load(std::sync::atomic::Ordering::SeqCst)
                        && Instant::now() < deadline
                    {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    call(
                        "edit",
                        serde_json::json!({"path": p2, "oldText":"original","newText":"from A"}),
                    )
                }
                _ => text("A done"),
            }
        } else {
            match n {
                0 => call("read", serde_json::json!({"path": p2})),
                1 => call(
                    "edit",
                    serde_json::json!({"path": p2, "oldText":"original","newText":"from B"}),
                ),
                _ => text("B done"),
            }
        }
    });
    let (port, _log) = spawn_server(h);
    let a = sb
        .nanopi(port, "PROC-A edit")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Let A get through its read before B starts.
    std::thread::sleep(Duration::from_millis(500));
    let (b_out, b_v) = sb.run(port, "PROC-B edit");
    assert!(b_out.status.success());
    assert!(
        !tool_texts(&b_v).join("\n").contains("changed since"),
        "B's edit is fine"
    );
    assert!(std::fs::read_to_string(&f).unwrap().contains("from B"));
    b_done.store(true, std::sync::atomic::Ordering::SeqCst);

    let a_out = a.wait_with_output().unwrap();
    let a_v: serde_json::Value = serde_json::from_slice(&a_out.stdout).unwrap();
    let a_tools = tool_texts(&a_v);
    assert!(
        a_tools
            .last()
            .unwrap()
            .contains("file changed since you read it"),
        "{a_tools:?}"
    );
    assert!(
        std::fs::read_to_string(&f).unwrap().contains("from B"),
        "A's stale edit was refused"
    );
}

// ── SC6 (roadmap #6): no agent controls in the TUI ──

#[test]
fn sc_no_agent_ui_controls() {
    let keys = include_str!("../src/keys.rs");
    assert!(!keys.to_lowercase().contains("agent"));
}
