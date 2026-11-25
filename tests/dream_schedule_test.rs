//! Public CLI checks: no scheduler or provider is contacted by an unscheduled run.
use std::process::Command;

#[test]
fn unscheduled_status_is_local_and_worker_refuses() {
    let home = tempfile::tempdir().unwrap();
    let run = |action: &str| {
        Command::new(env!("CARGO_BIN_EXE_nanopi"))
            .env("NANOPI_HOME", home.path())
            .args(["--dream-schedule", action])
            .output()
            .unwrap()
    };
    let out = run("status");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("not installed"));
    let out = run("run");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not installed"));
}

#[cfg(target_os = "linux")]
#[test]
fn schedule_lifecycle_and_endpoint_change() {
    use std::{
        fs,
        io::{Read, Write},
        net::TcpListener,
        os::unix::fs::PermissionsExt,
        time::Duration,
    };
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    let bin = p.join("bin");
    fs::create_dir(&bin).unwrap();
    for (name, script) in [
        ("loginctl", "#!/bin/sh\necho yes\n"),
        ("systemctl", "#!/bin/sh\ncase \"$3\" in\n is-enabled) echo enabled;;\n is-active) echo active;;\n show) echo 'tomorrow 03:00';;\nesac\n"),
    ] {
        let file = bin.join(name);
        fs::write(&file, script).unwrap();
        fs::set_permissions(file, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let config = p.join("config.toml");
    let write_config = |port| {
        fs::write(&config, format!("model = 'mock-model'\napi_key = 'dummy-key'\nbase_url = 'http://127.0.0.1:{port}'\n")).unwrap();
    };
    write_config(9);
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_nanopi"))
            .env("NANOPI_HOME", p)
            .env("XDG_CONFIG_HOME", p.join("xdg"))
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env_remove("NANOPI_PROFILE")
            .env("NO_PROXY", "127.0.0.1")
            .arg("--profile")
            .arg("default")
            .arg("--dream-schedule")
            .args(args)
            .output()
            .unwrap()
    };
    let ok = |args: &[&str]| {
        let out = run(args);
        assert!(
            out.status.success(),
            "{:?}: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    assert!(ok(&["enable", "03:17"]).contains("03:17"));
    let units = p.join("xdg/systemd/user");
    let timer = fs::read_dir(&units)
        .unwrap()
        .map(|x| x.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "timer"))
        .unwrap();
    let text = fs::read_to_string(timer).unwrap();
    assert!(text.contains("OnCalendar=*-*-* 03:17:00") && text.contains("Persistent=true"));
    assert!(ok(&["status"]).contains("tomorrow 03:00"));
    assert!(ok(&["run"]).contains("0 model call(s)"));
    write_config(10);
    let out = run(&["run"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr)
        .contains("endpoint, protocol, vendor, or model changed"));
    assert!(ok(&["status"]).contains("paused"));
    // The installed worker sends a real tool-free request to its pinned
    // endpoint. A redirect must not replay the notes to another server.
    let pinned = TcpListener::bind("127.0.0.1:0").unwrap();
    let other = TcpListener::bind("127.0.0.1:0").unwrap();
    other.set_nonblocking(true).unwrap();
    write_config(pinned.local_addr().unwrap().port());
    assert!(ok(&["enable"]).contains("03:00"));
    let daily = p.join("memory/daily/2026-01-01");
    fs::create_dir_all(&daily).unwrap();
    fs::write(daily.join("example.md"), "<!-- note:01800000-0000-7000-8000-000000000001 kind:method id-hint: -->\nKeep a short checklist for repeatable tests.\n").unwrap();
    let other_addr = other.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        pinned.set_nonblocking(true).unwrap();
        for response in [format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{other_addr}/elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 13\r\nConnection: close\r\n\r\nprivate-echo!".into()] {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match pinned.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
                    Err(e) => panic!("pinned endpoint did not receive a request: {e}"),
                }
            };
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut reader = std::io::BufReader::new(&mut stream);
            let mut n = 0;
            loop {
                use std::io::BufRead;
                let mut line = String::new();
                assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                if line == "\r\n" { break; }
                if let Some(size) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    n = size.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; n];
            reader.read_exact(&mut body).unwrap();
            let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(request["model"], "mock-model");
            assert!(request.get("tools").is_none_or(|v| v.as_array().is_some_and(Vec::is_empty)));
            assert!(request.to_string().contains("short checklist"));
            stream.write_all(response.as_bytes()).unwrap();
        }
    });
    assert!(!run(&["run"]).status.success());
    assert!(ok(&["status"]).contains("active"));
    assert_eq!(
        other.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    let rejected = run(&["run"]);
    server.join().unwrap();
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("rejected the pinned credentials"));
    assert!(ok(&["status"]).contains("paused"));
    let log = fs::read_to_string(p.join("memory/dream-log.md")).unwrap();
    assert!(log.contains("provider rejected the pinned credentials"));
    assert!(!log.contains("private-echo!"));
    assert!(ok(&["enable"]).contains("03:00"));
    assert!(ok(&["pause"]).contains("paused"));
    assert!(ok(&["remove"]).contains("removed"));
    assert!(ok(&["status"]).contains("not installed"));
}

#[cfg(target_os = "linux")]
#[test]
fn falls_back_to_user_cron_without_changing_other_jobs() {
    use std::{fs, os::unix::fs::PermissionsExt};
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let bin = home.join("bin");
    fs::create_dir(&bin).unwrap();
    for (name, script) in [
        ("loginctl", "#!/bin/sh\necho no\n"),
        ("ps", "#!/bin/sh\nprintf 'init\\ncron\\n'\n"),
        ("crontab", "#!/bin/sh\ncase \"$1\" in -l) cat \"$NANOPI_HOME/table\";; -) cat > \"$NANOPI_HOME/table\";; *) exit 2;; esac\n"),
    ] {
        let file = bin.join(name);
        fs::write(&file, script).unwrap();
        fs::set_permissions(file, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let original = "MAILTO=\"\"\n# existing job\n42 * * * * /bin/true\n";
    let table = home.join("table");
    fs::write(&table, original).unwrap();
    fs::write(
        home.join("config.toml"),
        "model='mock'\napi_key='dummy'\nbase_url='http://127.0.0.1:9'\n",
    )
    .unwrap();
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_nanopi"))
            .env("NANOPI_HOME", home)
            .env("XDG_CONFIG_HOME", home.join("xdg"))
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .args(["--profile", "default", "--dream-schedule"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    assert!(run(&["enable", "03:17"]).contains("crontab"));
    let installed = fs::read_to_string(&table).unwrap();
    assert!(installed.starts_with(original));
    assert!(installed.contains("17 3 * * * "));
    assert!(installed.contains("--dream-schedule run"));
    assert!(run(&["status"]).contains("active"));
    run(&["enable", "04:25"]);
    let updated = fs::read_to_string(&table).unwrap();
    assert!(updated.starts_with(original));
    assert_eq!(updated.matches("--dream-schedule run").count(), 1);
    assert!(updated.contains("25 4 * * * "));
    run(&["pause"]);
    assert_eq!(fs::read_to_string(&table).unwrap(), original);
    run(&["enable"]);
    run(&["remove"]);
    assert_eq!(fs::read_to_string(&table).unwrap(), original);
}
