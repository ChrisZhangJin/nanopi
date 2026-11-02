//! SIGTERM / SIGHUP against the real TUI, end to end: the binary runs on
//! a pty (the TUI needs a terminal), with one stdio MCP server and one
//! HTTP MCP server configured.
//!
//! What this pins is that a signalled exit walks the same shutdown as
//! `/quit` and leaves nothing behind: the exit code is 128 + the signal,
//! the terminal modes are restored, the HTTP session is `DELETE`d, the
//! stdio server (and the helper it backgrounded) is gone, and the
//! parent-to-child channel directory is removed. And that it cannot hang:
//! a wedged shutdown is cut off by its bound, or at once by a second
//! signal.
//!
//! Nothing here sleeps to *wait for* something. Each wait is on an
//! observable fact (the terminal's cursor-position query, a pid file, a
//! recorded request), with a deadline that only fires when something is
//! genuinely broken. In particular the signal is sent only after the TUI
//! has asked the terminal for the cursor position, which it does after
//! installing its handlers; sent earlier, the process would die by the
//! signal (no exit code) and the test would be measuring startup.

#![cfg(unix)]

use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

// ------------------------------------------------------- fake MCP servers

/// A stdio server that records its own pid and that of a helper it
/// backgrounds (what `npx` does with node), then speaks just enough MCP.
const STDIO_SH: &str = r#"
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
    esac
done
"#;

/// A hand-rolled HTTP MCP server that records what it is asked, like the
/// one in `mcp_agent_e2e.rs`: one `Mcp-Session-Id`, a one-event SSE
/// answer to each POST.
struct FakeHttp {
    port: u16,
    log: Arc<Mutex<Vec<(String, String, Option<String>)>>>,
}

impl FakeHttp {
    fn spawn() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let log = Arc::new(Mutex::new(Vec::new()));
        let log2 = log.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let log = log2.clone();
                std::thread::spawn(move || Self::serve(stream, &log));
            }
        });
        FakeHttp { port, log }
    }

    fn serve(mut stream: std::net::TcpStream, log: &Mutex<Vec<(String, String, Option<String>)>>) {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            match stream.read(&mut byte) {
                Ok(0) | Err(_) => return,
                Ok(_) => head.push(byte[0]),
            }
        }
        let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
        let method = head.split_whitespace().next().unwrap_or("").to_ascii_uppercase();
        let header = |name: &str| {
            head.lines()
                .find_map(|l| l.strip_prefix(&format!("{name}:")))
                .map(|v| v.trim().to_string())
        };
        let len: usize = header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
        let mut body = vec![0u8; len];
        if stream.read_exact(&mut body).is_err() {
            return;
        }
        let rpc: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let rpc_method = rpc["method"].as_str().unwrap_or("").to_string();
        log.lock().unwrap().push((method.clone(), rpc_method.clone(), header("mcp-session-id")));

        let reply = |status: &str, extra: &str, body: &str| {
            let r = format!(
                "HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.try_clone().unwrap().write_all(r.as_bytes());
        };
        let message = |m: Value| format!("event: message\ndata: {m}\n\n");
        let sse = "Content-Type: text/event-stream\r\n";
        let id = rpc["id"].clone();
        if method == "DELETE" {
            return reply("200 OK", "", "");
        }
        match rpc_method.as_str() {
            "initialize" => {
                let result = json!({"jsonrpc":"2.0","id":id,"result":{
                    "protocolVersion":"2025-06-18","capabilities":{"tools":{}},
                    "serverInfo":{"name":"fake-http","version":"1"}}});
                reply("200 OK", &format!("{sse}Mcp-Session-Id: sess-1\r\n"), &message(result));
            }
            "notifications/initialized" => reply("202 Accepted", "", ""),
            "tools/list" => {
                let result = json!({"jsonrpc":"2.0","id":id,"result":{"tools":[{
                    "name":"echo","description":"echoes","inputSchema":{"type":"object"}}]}});
                reply("200 OK", sse, &message(result));
            }
            _ => reply("400 Bad Request", "", ""),
        }
    }

    fn count(&self, method: &str, rpc: &str) -> usize {
        self.log.lock().unwrap().iter().filter(|(m, r, _)| m == method && r == rpc).count()
    }

    fn deletes(&self) -> Vec<Option<String>> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _, _)| m == "DELETE")
            .map(|(_, _, s)| s.clone())
            .collect()
    }
}

// ------------------------------------------------------------------- pty

/// The TUI running on a pty. A reader thread plays the terminal: it keeps
/// everything the program wrote, and answers its cursor-position query
/// (without which `Terminal::with_options` waits two seconds and fails).
struct Tui {
    child: std::process::Child,
    master: RawFd,
    out: Arc<Mutex<Vec<u8>>>,
    cpr_queries: Arc<AtomicUsize>,
}

impl Tui {
    fn spawn(fx: &Fixture) -> Self {
        let (mut master, mut slave) = (0, 0);
        let ws = libc::winsize { ws_row: 30, ws_col: 100, ws_xpixel: 0, ws_ypixel: 0 };
        // SAFETY: out-pointers to two ints and a winsize that outlives the call.
        let rc = unsafe {
            libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null(), &ws)
        };
        assert_eq!(rc, 0, "openpty");
        // SAFETY: both fds were just created and are owned here.
        let slave = unsafe { OwnedFd::from_raw_fd(slave) };
        let dup = |fd: &OwnedFd| Stdio::from(fd.try_clone().expect("dup slave"));

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_nanopi"));
        cmd.current_dir(&fx.dir)
            .args(["--base-url", "http://127.0.0.1:9"])
            .args(["--model", "fake-model", "--api-key", "not-a-real-key"])
            .args(["--no-skills", "--no-context-files"])
            .args(&fx.extra_args)
            .env("HOME", fx.dir.join("home"))
            .env("NANOPI_HOME", fx.dir.join("home/.nanopi"))
            .env("TMPDIR", fx.dir.join("tmp"))
            .env("TERM", "xterm-256color")
            .env_remove("NANOPI_AGENT_ID")
            .env_remove("NANOPI_PARENT_PID")
            .env_remove("NANOPI_RUN_ID")
            .stdin(dup(&slave))
            .stdout(dup(&slave))
            .stderr(dup(&slave));
        // SAFETY: only async-signal-safe calls between fork and exec. Its
        // own session with the pty as controlling terminal, as a shell
        // would give it.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().expect("spawn nanopi");
        // Ours is closed so the reader sees EIO when the child is gone.
        drop(slave);

        let out = Arc::new(Mutex::new(Vec::new()));
        let cpr_queries = Arc::new(AtomicUsize::new(0));
        let (out2, cpr2) = (out.clone(), cpr_queries.clone());
        // `master` stays ours (for tcgetattr, closed on drop); the thread
        // gets its own dup to read and answer through.
        // SAFETY: plain dup of a live fd, whose result is then owned.
        let mut reader = unsafe { std::fs::File::from_raw_fd(libc::dup(master)) };
        let mut writer = reader.try_clone().expect("dup master");
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            let mut answered = 0;
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        let mut out = out2.lock().unwrap();
                        out.extend_from_slice(&buf[..n]);
                        let queries = out.windows(4).filter(|w| *w == b"\x1b[6n").count();
                        drop(out);
                        while answered < queries {
                            let _ = writer.write_all(b"\x1b[1;1R");
                            answered += 1;
                            cpr2.store(answered, Ordering::SeqCst);
                        }
                    }
                }
            }
        });
        Tui { child, master, out, cpr_queries }
    }

    /// Up, handlers installed, drawing: it has asked where the cursor is.
    fn wait_ready(&mut self, fx: &Fixture) {
        wait_until("the TUI to ask for the cursor position", || {
            if let Some(s) = self.child.try_wait().expect("try_wait") {
                panic!("nanopi exited early: {s:?}\n{}", self.output());
            }
            self.cpr_queries.load(Ordering::SeqCst) > 0
        });
        wait_until("the stdio MCP server to start", || {
            std::fs::read_to_string(&fx.helper_pid).is_ok_and(|s| s.trim().parse::<u32>().is_ok())
        });
    }

    fn output(&self) -> String {
        String::from_utf8_lossy(&self.out.lock().unwrap()).into_owned()
    }

    fn signal(&self, sig: libc::c_int) {
        // SAFETY: a plain syscall.
        assert_eq!(unsafe { libc::kill(self.child.id() as libc::pid_t, sig) }, 0);
    }

    /// `(ICANON, ECHO)` as the terminal currently has them. Raw mode
    /// clears both.
    fn line_discipline(&self) -> (bool, bool) {
        // SAFETY: a zeroed termios is a valid out-parameter.
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::tcgetattr(self.master, &mut t) }, 0, "tcgetattr");
        (t.c_lflag & libc::ICANON != 0, t.c_lflag & libc::ECHO != 0)
    }

    fn wait_exit(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(s) = self.child.try_wait().expect("try_wait") {
                return s;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                panic!("nanopi hung after the signal\n{}", self.output());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // SAFETY: the fd from `openpty`, closed once.
        unsafe { libc::close(self.master) };
    }
}

// --------------------------------------------------------------- fixture

struct Fixture {
    dir: PathBuf,
    server_pid: PathBuf,
    helper_pid: PathBuf,
    extra_args: Vec<String>,
}

impl Fixture {
    fn new(tag: &str, http_port: u16, config_toml: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("nanopi-tui-sig-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("home/.nanopi");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(dir.join("tmp")).expect("tmp");
        let script = dir.join("server.sh");
        std::fs::write(&script, STDIO_SH).expect("script");
        let (server_pid, helper_pid) = (dir.join("server.pid"), dir.join("helper.pid"));
        let config = json!({"mcpServers": {
            "stdio": {"command": "sh", "args": [script],
                      "env": {"PIDFILE": server_pid, "HELPERFILE": helper_pid}},
            "web": {"type": "http", "url": format!("http://127.0.0.1:{http_port}/mcp")},
        }});
        std::fs::write(home.join("mcp.json"), config.to_string()).expect("mcp.json");
        std::fs::write(home.join("config.toml"), config_toml).expect("config.toml");
        Self { dir, server_pid, helper_pid, extra_args: vec!["--no-hooks".into()] }
    }

    fn with_hooks(mut self) -> Self {
        self.extra_args.clear();
        self
    }

    /// The parent-to-child channel's directory: `$TMPDIR/nanopi-<pid>`.
    fn ipc_dir(&self, pid: u32) -> PathBuf {
        self.dir.join("tmp").join(format!("nanopi-{pid}"))
    }

    fn pids(&self) -> (u32, u32) {
        (read_pid(&self.server_pid), read_pid(&self.helper_pid))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn read_pid(path: &Path) -> u32 {
    std::fs::read_to_string(path).expect("pid file").trim().parse().expect("pid")
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Alive = exists and is not a zombie.
fn alive(pid: u32) -> bool {
    let out = Command::new("ps").args(["-o", "stat=", "-p", &pid.to_string()]).output().expect("ps");
    let stat = String::from_utf8_lossy(&out.stdout);
    !stat.trim().is_empty() && !stat.trim().starts_with('Z')
}

fn assert_dies(what: &str, pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while alive(pid) {
        assert!(Instant::now() < deadline, "{what} (pid {pid}) is still running");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Everything a signalled exit must leave true, for either signal.
fn run_signal_case(tag: &str, sig: libc::c_int, expect_code: i32) {
    let http = FakeHttp::spawn();
    let fx = Fixture::new(tag, http.port, "");
    let mut tui = Tui::spawn(&fx);
    tui.wait_ready(&fx);
    wait_until("the HTTP session to be open", || http.count("POST", "tools/list") > 0);

    let pid = tui.child.id();
    let (server, helper) = fx.pids();
    assert!(fx.ipc_dir(pid).join("mcp.sock").exists(), "no channel socket while live");
    assert!(http.deletes().is_empty(), "nothing has ended the session yet");
    // Raw mode while it runs, so "restored" below means something.
    assert_eq!(tui.line_discipline(), (false, false), "not in raw mode while live");

    tui.signal(sig);
    let status = tui.wait_exit();
    assert_eq!(status.code(), Some(expect_code), "{status:?}\n{}", tui.output());

    // The session was ended, by id.
    assert_eq!(http.deletes(), vec![Some("sess-1".to_string())], "{:?}", http.log.lock().unwrap());
    // No stdio server and no helper it left running.
    assert_dies("stdio MCP server", server);
    assert_dies("the helper the server backgrounded", helper);
    // No channel directory.
    assert!(!fx.ipc_dir(pid).exists(), "the channel directory survived");
    // Terminal modes back: cooked again, bracketed paste turned off.
    assert_eq!(tui.line_discipline(), (true, true), "terminal left in raw mode");
    let out = tui.output();
    let on = out.rfind("\x1b[?2004h").expect("bracketed paste was enabled");
    let off = out.rfind("\x1b[?2004l").expect("bracketed paste was never turned off");
    assert!(off > on, "bracketed paste left on");
    // The orderly path's last line, same as `/quit`.
    assert!(out.contains("To resume:"), "{out}");
}

#[test]
fn sigterm_quits_the_tui_in_order() {
    run_signal_case("term", libc::SIGTERM, 143);
}

#[test]
fn sighup_quits_the_tui_in_order() {
    run_signal_case("hup", libc::SIGHUP, 129);
}

/// A session-end hook that never finishes is the wedge: the orderly path
/// runs it last. The exit must still come, by the shutdown bound, with
/// the signal's code, and the servers must still be gone.
const WEDGED_HOOK: &str = r#"
[[hooks.session_shutdown]]
command = "echo $$ > $NANOPI_HOME/hook.pid; exec sleep 120"
timeout = 120000
"#;

fn run_wedged(tag: &str) -> (Fixture, Tui, FakeHttp) {
    let http = FakeHttp::spawn();
    let fx = Fixture::new(tag, http.port, WEDGED_HOOK).with_hooks();
    let mut tui = Tui::spawn(&fx);
    tui.wait_ready(&fx);
    wait_until("the HTTP session to be open", || http.count("POST", "tools/list") > 0);
    (fx, tui, http)
}

fn kill_hook(fx: &Fixture) {
    if let Ok(s) = std::fs::read_to_string(fx.dir.join("home/.nanopi/hook.pid")) {
        if let Ok(pid) = s.trim().parse::<libc::pid_t>() {
            // SAFETY: a plain syscall; the pid is the hook's own.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

#[test]
fn a_wedged_shutdown_is_cut_off_by_its_bound() {
    let (fx, mut tui, http) = run_wedged("bound");
    let (server, helper) = fx.pids();

    tui.signal(libc::SIGTERM);
    let signalled = Instant::now();
    wait_until("the shutdown hook to start", || fx.dir.join("home/.nanopi/hook.pid").exists());
    let status = tui.wait_exit();
    let took = signalled.elapsed();
    kill_hook(&fx);

    assert_eq!(status.code(), Some(143), "{status:?}");
    // The hook alone would take 120 s; the bound is 7 s.
    assert!(took >= Duration::from_secs(5), "cut off too early to be the bound: {took:?}");
    assert!(took < Duration::from_secs(20), "the bound did not hold: {took:?}");
    // The MCP half of shutdown ran before the hook, so it was not skipped.
    assert_eq!(http.deletes().len(), 1);
    assert_dies("stdio MCP server", server);
    assert_dies("the helper the server backgrounded", helper);
    assert!(!fx.ipc_dir(tui.child.id()).exists());
    assert_eq!(tui.line_discipline(), (true, true), "terminal left in raw mode");
}

#[test]
fn a_second_signal_during_shutdown_exits_at_once() {
    let (fx, mut tui, http) = run_wedged("second");
    let (server, helper) = fx.pids();

    tui.signal(libc::SIGTERM);
    wait_until("the shutdown hook to start", || fx.dir.join("home/.nanopi/hook.pid").exists());
    let second = Instant::now();
    tui.signal(libc::SIGTERM);
    let status = tui.wait_exit();
    let took = second.elapsed();
    kill_hook(&fx);

    assert_eq!(status.code(), Some(143), "{status:?}");
    assert!(took < Duration::from_secs(3), "a second signal did not cut shutdown short: {took:?}");
    assert_eq!(http.deletes().len(), 1);
    assert_dies("stdio MCP server", server);
    assert_dies("the helper the server backgrounded", helper);
    assert!(!fx.ipc_dir(tui.child.id()).exists());
    assert_eq!(tui.line_discipline(), (true, true), "terminal left in raw mode");
}
