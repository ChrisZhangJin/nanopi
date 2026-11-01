//! End-to-end MCP tests: a real `nanopi -p` process, a fake
//! OpenAI-compatible model, and a fake stdio MCP server (a shell script).
//!
//! What these pin is process hygiene, which no in-process test can: that
//! a server is gone once nanopi is, on every way nanopi can end — normal
//! completion, Ctrl-C, SIGTERM, SIGKILL — and that a spawned child agent
//! never starts one. Modelled on `print_mode_ctrl_c_stops_all_background_agents`
//! in `print_mode_e2e.rs`, including its lesson: that test was flaky for
//! as long as it slept a fixed 800 ms before signalling, because on a
//! loaded machine the process it meant to interrupt did not exist yet.
//! Nothing here sleeps to *wait for* something. Each wait is on an
//! observable fact — a pid file appearing, the model request being in
//! flight, `ps` no longer finding the process — with a deadline that only
//! ever fires when something is genuinely broken.

#![cfg(unix)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ------------------------------------------------------- fake MCP server

/// A server that records its own pid and that of a helper it
/// backgrounds (what `npx` does with node), then speaks just enough MCP:
/// handshake, one tool `ping`, and `tools/call` answering `pong`.
///
/// The helper is the point of recording a second pid: killing only the
/// server's own pid would pass a test that leaves the helper running,
/// and the helper is exactly the orphan `process_group(0)` exists for.
const SERVER_SH: &str = r#"
echo $$ > "$PIDFILE"
sleep 300 &
echo $! > "$HELPERFILE"
while IFS= read -r line; do
    id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
    case "$line" in
        *'"method":"initialize"'*)
            printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"1"}}}\n' "$id" ;;
        *'"method":"tools/list"'*)
            printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"ping","description":"answers pong","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
        *'"method":"tools/call"'*)
            printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"pong"}]}}\n' "$id" ;;
    esac
done
"#;

struct Fixture {
    dir: PathBuf,
    server_pid: PathBuf,
    helper_pid: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        // Process id + tag: the tests of one binary share a pid and
        // differ by tag, so the directories cannot collide.
        let dir = std::env::temp_dir().join(format!("nanopi-mcp-e2e-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("home/.nanopi");
        std::fs::create_dir_all(&home).expect("home");
        // Its own `$TMPDIR`, so the parent↔child socket directory this
        // run creates is findable and nothing else's is in the way.
        std::fs::create_dir_all(dir.join("tmp")).expect("tmp");
        let script = dir.join("server.sh");
        std::fs::write(&script, SERVER_SH).expect("script");
        let (server_pid, helper_pid) = (dir.join("server.pid"), dir.join("helper.pid"));
        // Credentials go in `env`, as real configs do; the assertions
        // below check they never reach output.
        let config = serde_json::json!({
            "mcpServers": { "fake": {
                "command": "sh",
                "args": [script],
                "env": {
                    "PIDFILE": server_pid,
                    "HELPERFILE": helper_pid,
                    "API_TOKEN": "s3cr3t-do-not-print",
                }
            }}
        });
        std::fs::write(home.join("mcp.json"), config.to_string()).expect("mcp.json");
        Self { dir, server_pid, helper_pid }
    }

    fn spawn(&self, port: u16, extra_args: &[&str], envs: &[(&str, &str)]) -> std::process::Child {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_nanopi"));
        cmd.current_dir(&self.dir)
            .args(["-p", "--output", "json", "--base-url"])
            .arg(format!("http://127.0.0.1:{port}"))
            .args(["--model", "fake-model", "--api-key", "not-a-real-key"])
            .args(["--no-hooks", "--no-skills", "--no-context-files"])
            .args(extra_args)
            .arg("go")
            .env("HOME", self.dir.join("home"))
            .env("NANOPI_HOME", self.dir.join("home/.nanopi"))
            .env("TMPDIR", self.dir.join("tmp"))
            .env_remove("NANOPI_AGENT_ID")
            .env_remove("NANOPI_PARENT_PID")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in envs {
            cmd.env(k, v);
        }
        cmd.spawn().expect("spawn nanopi")
    }

    /// The parent↔child channel's directory for this process
    /// (`mcp::ipc`): `$TMPDIR/nanopi-<pid>`.
    fn ipc_dir(&self, child: &std::process::Child) -> PathBuf {
        self.dir.join("tmp").join(format!("nanopi-{}", child.id()))
    }

    /// The channel is created before the first model request, so a run
    /// held mid-turn must already have it; this is what makes the
    /// "gone afterwards" assertions mean something.
    fn assert_ipc_socket_exists(&self, child: &std::process::Child) {
        let socket = self.ipc_dir(child).join("mcp.sock");
        assert!(socket.exists(), "no channel socket at {} while the run is live", socket.display());
    }

    /// Pids once the server has started. Waits on the file, not a clock.
    fn pids(&self) -> (u32, u32) {
        (wait_for_pid(&self.server_pid), wait_for_pid(&self.helper_pid))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn wait_for_pid(path: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            // The server writes `echo $$ > file`: the number and a
            // newline arrive together, but a read racing the write can
            // still see an empty file.
            if let Ok(pid) = text.trim().parse() {
                return pid;
            }
        }
        assert!(Instant::now() < deadline, "{} never appeared", path.display());
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Alive = exists and is not a zombie. A grandchild killed after its
/// parent is reparented to the container's init, which may never reap
/// it; it is gone for every purpose that matters but `kill(pid, 0)`
/// still succeeds.
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

fn wait_exit(child: &mut std::process::Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(s) = child.try_wait().expect("try_wait") {
            return s;
        }
        assert!(Instant::now() < deadline, "nanopi hung");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn output_of(child: &mut std::process::Child) -> (String, String) {
    let (mut out, mut err) = (String::new(), String::new());
    if let Some(mut s) = child.stdout.take() {
        let _ = s.read_to_string(&mut out);
    }
    if let Some(mut s) = child.stderr.take() {
        let _ = s.read_to_string(&mut err);
    }
    (out, err)
}

// ------------------------------------------------------- fake model

struct Model {
    port: u16,
    /// Set the instant a request has been read and is being held (or
    /// answered): the observable "the run is now mid-turn".
    in_flight: Arc<AtomicBool>,
    /// Holds the FIRST response until set, so a signal can be sent while
    /// the run is provably mid-turn rather than after a guessed delay.
    release: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<String>>>,
}

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
        format!(
            r#"{{"id":"x","choices":[{{"index":0,"delta":{{"content":{}}},"finish_reason":null}}]}}"#,
            serde_json::Value::String(t.to_string())
        ),
        r#"{"id":"x","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#.to_string(),
    ]
}

fn call(name: &str) -> Vec<String> {
    vec![
        format!(
            r#"{{"id":"x","choices":[{{"index":0,"delta":{{"tool_calls":[{{"index":0,"id":"c1","type":"function","function":{{"name":"{name}","arguments":"{{}}"}}}}]}},"finish_reason":null}}]}}"#
        ),
        r#"{"id":"x","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#.to_string(),
    ]
}

/// Serves `script` in order (the last entry repeats). Each connection
/// gets its own thread so a held first response never blocks a later one.
fn spawn_model(script: Vec<Vec<String>>, hold_first: bool) -> Model {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let in_flight = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(!hold_first));
    let requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let bodies: Arc<Vec<String>> = Arc::new(script.iter().map(|c| sse(c)).collect());
    let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let (in_flight2, release2, requests2) = (in_flight.clone(), release.clone(), requests.clone());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let (bodies, counter) = (bodies.clone(), counter.clone());
            let (in_flight, release, requests) =
                (in_flight2.clone(), release2.clone(), requests2.clone());
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
                requests.lock().unwrap().push(String::from_utf8_lossy(&req).into_owned());

                let n = counter.fetch_add(1, Ordering::SeqCst);
                in_flight.store(true, Ordering::SeqCst);
                if n == 0 {
                    while !release.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
                let body = &bodies[n.min(bodies.len() - 1)];
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
    Model { port, in_flight, release, requests }
}

/// Wait for the run to be mid-turn, failing fast with the process's own
/// output if it exits instead — the likeliest regression is nanopi
/// dying at startup, and a 30 s timeout would blame the model fixture.
fn wait_in_flight(model: &Model, child: &mut std::process::Child) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !model.in_flight.load(Ordering::SeqCst) {
        if let Some(status) = child.try_wait().expect("try_wait") {
            let (out, err) = output_of(child);
            panic!("nanopi exited ({status:?}) before reaching the model\nstdout={out}\nstderr={err}");
        }
        assert!(Instant::now() < deadline, "the model request never arrived");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn signal(child: &std::process::Child, sig: libc::c_int) {
    // SAFETY: `child.id()` is a live pid owned by this test.
    unsafe {
        libc::kill(child.id() as i32, sig);
    }
}

// ------------------------------------------------------------- tests

/// The feature, end to end: the model sees the tool under its
/// registered name, calls it, gets the server's answer — and once the
/// run completes, neither the server nor what it backgrounded survives.
#[test]
fn a_print_run_calls_an_mcp_tool_and_leaves_no_server_behind() {
    let fx = Fixture::new("normal");
    let model = spawn_model(vec![call("mcp__fake__ping"), text("all done")], false);

    let mut child = fx.spawn(model.port, &[], &[]);
    let status = wait_exit(&mut child);
    let (stdout, stderr) = output_of(&mut child);
    assert!(status.success(), "{status:?}\nstdout={stdout}\nstderr={stderr}");

    let requests = model.requests.lock().unwrap().clone();
    assert!(requests.len() >= 2, "{requests:?}");
    assert!(
        requests[0].contains("mcp__fake__ping"),
        "the tool must be offered to the model: {}",
        requests[0]
    );
    assert!(
        requests[1].contains("pong"),
        "the server's answer must reach the model: {}",
        requests[1]
    );

    // Credentials in `env` are the server's business alone.
    assert!(!stdout.contains("s3cr3t-do-not-print") && !stderr.contains("s3cr3t-do-not-print"));
    assert!(!requests.iter().any(|r| r.contains("s3cr3t-do-not-print")));

    let (server, helper) = fx.pids();
    assert_dies("MCP server", server);
    assert_dies("the helper the server backgrounded", helper);
    // The socket and its directory go with the run.
    assert!(!fx.ipc_dir(&child).exists(), "the channel directory survived a normal exit");
}

/// The Ctrl-C path. The run is held mid-turn by the model, so the
/// signal provably lands while the server is up. The servers must die
/// from the interrupt itself — checked while the parent is still alive —
/// not merely when the parent eventually exits.
#[test]
fn ctrl_c_stops_every_mcp_server_before_the_run_ends() {
    let fx = Fixture::new("ctrlc");
    let model = spawn_model(vec![text("finished")], true);

    let mut child = fx.spawn(model.port, &[], &[]);
    wait_in_flight(&model, &mut child);
    let (server, helper) = fx.pids();
    assert!(alive(server) && alive(helper));
    fx.assert_ipc_socket_exists(&child);

    signal(&child, libc::SIGINT);
    assert_dies("MCP server", server);
    assert_dies("the helper the server backgrounded", helper);
    // Removed by the interrupt itself, like the servers: the run is
    // still alive here. Ordered before the servers stop, so by now it
    // has happened.
    assert!(!fx.ipc_dir(&child).exists(), "the channel directory survived Ctrl-C");
    assert!(
        child.try_wait().unwrap().is_none(),
        "the run is still mid-turn, so it was the interrupt that stopped the servers"
    );

    model.release.store(true, Ordering::SeqCst);
    let status = wait_exit(&mut child);
    let (stdout, stderr) = output_of(&mut child);
    assert!(status.code().is_some(), "{status:?}\nstdout={stdout}\nstderr={stderr}");
}

/// SIGTERM drops the whole print future (`main.rs`); the servers must
/// go with it.
#[test]
fn sigterm_leaves_no_mcp_server_behind() {
    let fx = Fixture::new("sigterm");
    let model = spawn_model(vec![text("finished")], true);

    let mut child = fx.spawn(model.port, &[], &[]);
    wait_in_flight(&model, &mut child);
    let (server, helper) = fx.pids();
    fx.assert_ipc_socket_exists(&child);

    signal(&child, libc::SIGTERM);
    let status = wait_exit(&mut child);
    assert_eq!(status.code(), Some(143), "{status:?}");
    assert_dies("MCP server", server);
    assert_dies("the helper the server backgrounded", helper);
    // Only a destructor runs on this path (the whole future is dropped).
    assert!(!fx.ipc_dir(&child).exists(), "the channel directory survived SIGTERM");
    model.release.store(true, Ordering::SeqCst);
}

/// A hard kill runs no destructor at all; only the kernel's
/// parent-death signal (set in `stdio.rs`) can reap the server. Linux
/// has it; elsewhere this is the documented gap.
#[cfg(target_os = "linux")]
#[test]
fn a_hard_killed_nanopi_does_not_orphan_its_mcp_server() {
    let fx = Fixture::new("sigkill");
    let model = spawn_model(vec![text("finished")], true);

    let mut child = fx.spawn(model.port, &[], &[]);
    wait_in_flight(&model, &mut child);
    let (server, _helper) = fx.pids();

    signal(&child, libc::SIGKILL);
    let _ = wait_exit(&mut child);
    // The server itself is signalled by the kernel. Its backgrounded
    // helper is not in the parent-death signal's reach (it has no
    // `PDEATHSIG` of its own), which is why only the server is asserted.
    assert_dies("MCP server", server);
    model.release.store(true, Ordering::SeqCst);
}

/// Trap 3. A spawned child agent is told apart by `--brief` /
/// `NANOPI_AGENT_ID`, exactly what `tool::agent` sets. With the same
/// config, a scripted run connects (the control) and a child connects
/// to nothing and is not offered any MCP tool.
#[test]
fn a_spawned_child_agent_connects_to_no_mcp_server() {
    let fx = Fixture::new("child");

    // Control: without it, "no pid file" below could just mean the
    // config was never read.
    let model = spawn_model(vec![text("hello")], false);
    let mut control = fx.spawn(model.port, &[], &[]);
    assert!(wait_exit(&mut control).success());
    let (server, helper) = fx.pids();
    assert_dies("control run's server", server);
    assert_dies("control run's helper", helper);
    std::fs::remove_file(&fx.server_pid).unwrap();
    std::fs::remove_file(&fx.helper_pid).unwrap();

    let brief = fx.dir.join("brief.md");
    std::fs::write(&brief, "---\nlabel: t\n---\nsay hi\n").unwrap();
    let transcript = fx.dir.join("transcript.jsonl");
    let model = spawn_model(vec![text("done")], false);
    let mut child = fx.spawn(
        model.port,
        &[
            "--brief",
            brief.to_str().unwrap(),
            "--session-file",
            transcript.to_str().unwrap(),
            "--max-turns",
            "3",
        ],
        &[("NANOPI_AGENT_ID", "a1")],
    );
    let status = wait_exit(&mut child);
    let (stdout, stderr) = output_of(&mut child);
    assert!(status.success(), "{status:?}\nstdout={stdout}\nstderr={stderr}");

    // The process has exited, so startup is entirely in the past: if it
    // were going to spawn the server it already has.
    assert!(!fx.server_pid.exists(), "a child agent started an MCP server");
    // Nor does a child listen for other children: it has nothing to proxy.
    assert!(!fx.ipc_dir(&child).exists(), "a child agent created a channel directory");
    let requests = model.requests.lock().unwrap().clone();
    assert!(!requests.is_empty());
    assert!(
        requests.iter().all(|r| !r.contains("mcp__")),
        "a child must not be offered MCP tools"
    );
}

/// Trap 4. `--tools` is a restriction: it must never gain MCP tools,
/// and there is no point starting servers whose tools would be
/// discarded.
#[test]
fn a_tools_allowlist_starts_no_mcp_server_and_offers_no_mcp_tool() {
    let fx = Fixture::new("allow");
    let model = spawn_model(vec![text("ok")], false);
    let mut child = fx.spawn(model.port, &["--tools", "read,grep"], &[]);
    let status = wait_exit(&mut child);
    let (stdout, stderr) = output_of(&mut child);
    assert!(status.success(), "{status:?}\nstdout={stdout}\nstderr={stderr}");
    assert!(!fx.server_pid.exists(), "--tools must not start MCP servers");
    let requests = model.requests.lock().unwrap().clone();
    assert!(requests.iter().all(|r| !r.contains("mcp__")));
}
