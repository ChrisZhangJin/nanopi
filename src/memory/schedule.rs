//! Opt-in Linux user scheduling. The worker never loads project configuration,
//! shell hooks, or agent tools, and resolves credentials only from its pinned profile.

use super::*;
use std::process::{Command, Stdio};

mod cron;

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Backend {
    #[default]
    Systemd,
    Cron,
}

impl Backend {
    fn label(self) -> &'static str {
        match self {
            Self::Systemd => "systemd user timer",
            Self::Cron => "user crontab",
        }
    }

    fn check(self) -> Result<()> {
        match self {
            Self::Systemd => systemd_supported(),
            Self::Cron => cron::supported(),
        }
    }
}

fn detect() -> Result<Backend> {
    ensure!(
        cfg!(target_os = "linux"),
        "automatic Dream scheduling is only supported on Linux"
    );
    match Backend::Systemd.check() {
        Ok(()) => Ok(Backend::Systemd),
        Err(systemd) => Backend::Cron
            .check()
            .map(|()| Backend::Cron)
            .map_err(|cron| anyhow::anyhow!("systemd: {systemd:#}; crontab: {cron:#}")),
    }
}

#[derive(Deserialize, Serialize)]
struct Schedule {
    #[serde(default)]
    backend: Backend,
    time: String,
    profile: String,
    endpoint: String,
    model: String,
    protocol: String,
    vendor: String,
    paused: Option<String>,
}

fn load(store: &Store) -> Result<Option<Schedule>> {
    match read_limited(&store.root.join("schedule.json"), 16 * 1024) {
        Ok(s) => Ok(Some(
            serde_json::from_str(&s).context("invalid Dream schedule")?,
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn save(store: &Store, s: &Schedule) -> Result<()> {
    store.replace(
        &store.root.join("schedule.json"),
        &serde_json::to_string_pretty(s)?,
    )
}

fn home(store: &Store) -> Result<PathBuf> {
    Ok(store
        .root
        .parent()
        .context("missing nanopi home")?
        .canonicalize()?)
}

fn connection(home: &Path, profile: &str) -> Result<crate::profile::Endpoint> {
    crate::profile::validate_name(profile).map_err(anyhow::Error::msg)?;
    let path = crate::profile::config_path_in(home, profile);
    let cfg: crate::config::Config = toml::from_str(&read_limited(&path, 256 * 1024)?)
        .map_err(|_| anyhow::anyhow!("cannot parse pinned profile configuration"))?;
    if let Some(path) = &cfg.api_key_file {
        ensure!(
            crate::paths::expand_home(&path.to_string_lossy()).is_absolute(),
            "scheduled Dream requires an absolute or ~/ credential file path"
        );
    }
    let ep = crate::profile::resolve_endpoint(&cfg, |_| None).map_err(anyhow::Error::msg)?;
    ensure!(
        !ep.api_key.trim().is_empty(),
        "empty credential in pinned profile"
    );
    let url = url::Url::parse(&ep.base_url).context("invalid pinned base URL")?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "scheduled Dream requires an HTTP(S) base URL without credentials, query or fragment"
    );
    Ok(ep)
}

fn transport(
    ep: &crate::profile::Endpoint,
) -> (crate::provider::ApiKind, Box<dyn crate::vendor::Vendor>) {
    let vendor = crate::vendor::pick_vendor(ep.provider.as_deref(), Some(&ep.base_url), &ep.model);
    let kind = crate::provider::effective_kind(
        crate::provider::ApiKind::from_config_opt(ep.api_kind.as_deref()),
        Some(vendor.as_ref()),
        &ep.base_url,
    );
    (kind, vendor)
}

fn command(program: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("cannot run {program}"))?;
    ensure!(
        out.status.success(),
        "{program}: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn systemctl(args: &[&str]) -> Result<String> {
    let mut all = vec!["--user", "--no-pager"];
    all.extend_from_slice(args);
    command("systemctl", &all)
}

fn systemd_supported() -> Result<()> {
    ensure!(
        cfg!(target_os = "linux"),
        "automatic Dream requires Linux systemd; use /dream manually"
    );
    let uid = command("id", &["-u"])?;
    let linger = command(
        "loginctl",
        &["show-user", &uid, "--property=Linger", "--value"],
    )
    .context("cannot verify unattended Linux user scheduling; use /dream manually")?;
    ensure!(linger == "yes", "user lingering is not enabled; cannot guarantee Dream after logout. Use /dream manually or ask your system administrator about enabling user lingering");
    systemctl(&["show-environment"]).context("user systemd is unavailable; use /dream manually")?;
    Ok(())
}

fn unit_name(home: &Path) -> String {
    format!("nanopi-dream-{}", &digest(&home.to_string_lossy())[..16])
}

const HEADER: &str = "# Managed by nanopi Dream\n";

fn quote(s: &str) -> Result<String> {
    ensure!(
        !s.chars().any(char::is_control),
        "invalid control character in scheduler path"
    );
    Ok(format!(
        "\"{}\"",
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
    ))
}

fn unit_paths(name: &str) -> Result<(PathBuf, PathBuf)> {
    let config = dirs::config_dir().context("cannot locate systemd user configuration")?;
    ensure!(
        config.is_absolute(),
        "systemd user configuration path must be absolute"
    );
    let dir = config.join("systemd/user");
    Ok((
        dir.join(format!("{name}.service")),
        dir.join(format!("{name}.timer")),
    ))
}

fn owned(path: &Path) -> Result<bool> {
    match read_limited(path, 64 * 1024) {
        Ok(s) => {
            ensure!(
                s.starts_with(HEADER),
                "refusing to change unmanaged unit {}",
                path.display()
            );
            Ok(true)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn clock(time: &str) -> Result<()> {
    let b = time.as_bytes();
    ensure!(
        b.len() == 5
            && b[2] == b':'
            && b.iter()
                .enumerate()
                .all(|(i, c)| i == 2 || c.is_ascii_digit())
            && time[..2].parse::<u8>()? < 24
            && time[3..].parse::<u8>()? < 60,
        "use local time HH:MM (00:00–23:59)"
    );
    Ok(())
}

fn install(store: &Store, time: &str, approved: Option<(&str, &str)>) -> Result<String> {
    clock(time)?;

    let h = home(store)?;
    let profile = crate::profile::current();
    let ep = connection(&h, &profile)
        .context("configure the selected profile on disk before enabling scheduled Dream")?;
    ensure!(
        approved.is_none_or(|(model, endpoint)| model == ep.model && endpoint == ep.base_url),
        "Dream connection changed during setup; run setup again"
    );
    let (kind, vendor) = transport(&ep);
    let _g = store
        .try_lock(".schedule.lock")
        .context("Dream schedule is busy; try again after the current run finishes")?;
    // Keep existing jobs on their recorded backend. Switching blindly
    // could leave a second job behind on the old backend.
    let backend = match load(store)? {
        Some(existing) => {
            existing.backend.check().context(
                "existing backend unavailable; remove the existing schedule before switching",
            )?;
            existing.backend
        }
        None => detect()?,
    };
    let s = Schedule {
        backend,
        time: time.into(),
        profile,
        endpoint: ep.base_url,
        model: ep.model,
        protocol: format!("{kind:?}"),
        vendor: vendor.id().into(),
        paused: None,
    };
    let exe = std::env::current_exe()?.canonicalize()?;
    let name = unit_name(&h);
    // Save before arming: a persistent timer may fire right away. If
    // setup fails, leave a paused record so a partial job cannot run.
    save(store, &s)?;
    let result = match backend {
        Backend::Systemd => install_systemd(store, &h, &exe, &name, time),
        Backend::Cron => cron::install(&h, &exe, &name, time),
    };
    if let Err(e) = result {
        let mut s = s;
        s.paused = Some("activation failed; run setup again or remove the schedule".into());
        save(store, &s)?;
        return Err(e.context("Dream schedule could not be activated"));
    }
    store.replace(&store.root.join("schedule-choice"), "configured\n")?;
    Ok(format!(
        "Dream scheduled at {time} local time via {} · {} at {} ({})",
        backend.label(),
        s.model,
        s.endpoint,
        s.profile
    ))
}

fn install_systemd(store: &Store, h: &Path, exe: &Path, name: &str, time: &str) -> Result<()> {
    let exe = quote(exe.to_str().context("non-UTF-8 executable path")?)?;
    let home_arg = quote(h.to_str().context("non-UTF-8 nanopi home")?)?;
    let (service, timer) = unit_paths(name)?;
    owned(&service)?;
    owned(&timer)?;
    fs::create_dir_all(timer.parent().context("missing unit directory")?)?;
    let timer_name = format!("{name}.timer");
    let svc = format!("{HEADER}[Unit]\nDescription=NanoPi Dream maintenance\n[Service]\nType=oneshot\nTimeoutStartSec=2h\nExecStart=:{exe} --dream-home {home_arg} --dream-schedule run\n");
    let tim = format!("{HEADER}[Unit]\nDescription=Daily NanoPi Dream\n[Timer]\nOnCalendar=*-*-* {time}:00\nPersistent=true\n[Install]\nWantedBy=timers.target\n");
    store.replace(&service, &svc)?;
    store.replace(&timer, &tim)?;
    let start = (|| -> Result<()> {
        systemctl(&["daemon-reload"])?;
        systemctl(&["enable", "--now", &timer_name])?;
        systemctl(&["restart", &timer_name])?;
        ensure!(
            systemctl(&["is-enabled", &timer_name])? == "enabled",
            "Dream timer was not enabled"
        );
        ensure!(
            systemctl(&["is-active", &timer_name])? == "active",
            "Dream timer did not become active"
        );
        Ok(())
    })();
    if start.is_err() {
        let _ = systemctl(&["disable", "--now", &timer_name]);
    }
    start
}

fn stop_backend(backend: Backend, name: &str, remove: bool) -> Result<()> {
    match backend {
        Backend::Systemd => {
            let (service, timer) = unit_paths(name)?;
            let has_service = owned(&service)?;
            let has_timer = owned(&timer)?;
            if has_timer {
                systemctl(&["disable", "--now", &format!("{name}.timer")])?;
            }
            if remove {
                if has_timer {
                    fs::remove_file(timer)?;
                }
                if has_service {
                    fs::remove_file(service)?;
                }
                if has_timer || has_service {
                    systemctl(&["daemon-reload"])?;
                }
            }
            Ok(())
        }
        Backend::Cron => cron::remove(name),
    }
}

fn change(store: &Store, remove: bool) -> Result<String> {
    let h = home(store)?;
    let _g = store
        .try_lock(".schedule.lock")
        .context("Dream schedule is busy; try again after the current run finishes")?;
    let mut s = load(store)?.context("Dream schedule not installed")?;
    s.paused = Some("paused by user".into());
    save(store, &s)?;
    let name = unit_name(&h);
    stop_backend(s.backend, &name, remove)?;
    if remove {
        fs::remove_file(store.root.join("schedule.json"))?;
        Ok("Dream schedule removed; memories were kept".into())
    } else {
        Ok("Dream schedule paused; an in-flight run may finish".into())
    }
}

pub(super) fn status(store: &Store) -> String {
    let s = match load(store) {
        Ok(Some(s)) => s,
        Ok(None) => {
            let base = "Dream schedule: not installed; use /dream or /dream schedule enable";
            return match read_limited(&store.root.join("schedule-unavailable"), 16 * 1024) {
                Ok(reason) => format!("{base}\nLast startup check: {}", reason.trim()),
                Err(_) => base.into(),
            };
        }
        Err(e) => return format!("Dream schedule: {e}"),
    };
    let desc = format!(
        "{} local · {} · {} via {} ({})",
        s.time,
        s.backend.label(),
        s.model,
        s.endpoint,
        s.profile
    );
    if let Some(reason) = s.paused {
        return format!("Dream schedule: paused ({reason}) · {desc}\nnext scheduled run: none");
    }
    if matches!(s.backend, Backend::Cron) {
        let active = home(store).and_then(|h| cron::active(&unit_name(&h)));
        return match active {
            Ok(()) => format!("Dream schedule: active · {desc}\nnext scheduled run: daily at {} local (cron does not report the next firing)", s.time),
            Err(e) => format!("Dream schedule: configured but unavailable · {desc}\nnext scheduled run: unknown ({e:#})"),
        };
    }
    let next = home(store).and_then(|h| {
        let name = format!("{}.timer", unit_name(&h));
        ensure!(
            systemctl(&["is-active", &name])? == "active",
            "timer inactive"
        );
        systemctl(&[
            "show",
            &name,
            "--property=NextElapseUSecRealtime",
            "--value",
        ])
    });
    match next {
        Ok(next) => format!("Dream schedule: active · {desc}\nnext scheduled run: {next}"),
        Err(_) => format!("Dream schedule: configured but timer unavailable · {desc}\nnext scheduled run: unknown; check user systemd or run /dream manually"),
    }
}

pub(super) fn manage(store: &Store, args: &[&str]) -> Result<String> {
    match args {
        ["status"] | [] => Ok(status(store)),
        ["enable"] => install(store, "03:00", None),
        ["enable", time] => install(store, time, None),
        ["pause"] => change(store, false),
        ["remove"] => change(store, true),
        _ => bail!("Usage: /dream schedule [status | enable [HH:MM] | pause | remove]"),
    }
}

fn pause_after_error(store: &Store, s: &mut Schedule, reason: &str) -> Result<String> {
    s.paused = Some(reason.into());
    save(store, s)?;
    let log_result = super::dream::append_log(
        store,
        &format!(
            "\n## Scheduled Dream {}\n- paused: {reason}\n",
            crate::util::time::now_iso8601()
        ),
    );
    if let Ok(h) = home(store) {
        let _ = stop_backend(s.backend, &unit_name(&h), false);
    }
    log_result.context("Dream schedule paused, but could not record its log")?;
    bail!("Dream schedule paused: {reason}")
}

async fn run(store: &Store) -> Result<String> {
    // Keep changes to the schedule serialized with this run. Manual
    // Dream and scheduled Dream also share the existing Dream run lock.
    ensure!(load(store)?.is_some(), "Dream schedule not installed");
    // A persistent timer can fire immediately during installation. Allow
    // the installer to release the lock before reading its final state.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let _g = loop {
        match store.try_lock(".schedule.lock") {
            Ok(lock) => break lock,
            Err(e) if tokio::time::Instant::now() >= deadline => {
                return Err(e.context("Dream schedule is busy"));
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
        }
    };
    let mut s = load(store)?.context("Dream schedule not installed")?;
    if let Some(reason) = &s.paused {
        bail!("Dream schedule paused: {reason}");
    }
    let ep = match connection(&home(store)?, &s.profile) {
        Ok(ep) => ep,
        Err(_) => {
            return pause_after_error(store, &mut s, "pinned profile or credentials unavailable")
        }
    };
    let (kind, vendor) = transport(&ep);
    if ep.base_url != s.endpoint
        || ep.model != s.model
        || format!("{kind:?}") != s.protocol
        || vendor.id() != s.vendor
    {
        return pause_after_error(store, &mut s, "pinned endpoint, protocol, vendor, or model changed; enable again to approve the new configuration");
    }
    crate::profile::set_current(&s.profile);
    let provider = crate::provider::build_pinned(kind, &ep, vendor)?;
    match dream::run(
        provider,
        ep.base_url,
        ep.model,
        dream::DreamOpts { since: None },
    )
    .await
    {
        Ok(r) => Ok(r.text),
        Err(e) => {
            let msg = e.to_string();
            if ["HTTP 401", "HTTP 403", "status 401", "status 403"]
                .iter()
                .any(|p| msg.contains(p))
            {
                return pause_after_error(
                    store,
                    &mut s,
                    "provider rejected the pinned credentials or access",
                );
            }
            Err(e)
        }
    }
}

/// Standalone CLI. No agent, project config or workspace resources are loaded.
pub async fn cli(args: &[String]) -> Result<String> {
    let store = Store::active()?;
    let words: Vec<_> = args.iter().map(String::as_str).collect();
    if words == ["run"] {
        run(&store).await
    } else {
        manage(&store, &words)
    }
}

/// Called only before a standalone interactive TUI is started.
pub fn offer() -> Result<()> {
    if !cfg!(target_os = "linux") {
        return Ok(());
    }
    let store = Store::active()?;
    let choice = store.root.join("schedule-choice");
    if exists(&choice) || load(&store)?.is_some() {
        return Ok(());
    }
    let unavailable = store.root.join("schedule-unavailable");
    let backend = match detect() {
        Ok(backend) => backend,
        Err(e) => {
            let _g = store.try_lock(".schedule.lock")?;
            let first = !exists(&unavailable);
            store.replace(&unavailable, &format!("{e:#}\n"))?;
            if first {
                eprintln!("Dream scheduling unavailable: {e:#}. Manual /dream remains available.");
            }
            return Ok(());
        }
    };
    if exists(&unavailable) {
        let _g = store.try_lock(".schedule.lock")?;
        if exists(&unavailable) {
            fs::remove_file(&unavailable)?;
        }
    }
    let ep = connection(&home(&store)?, &crate::profile::current())
        .context("configure a profile on disk to schedule Dream")?;
    eprint!(
        "Enable daily Dream at 03:00 local time using {} at {} via {}? [y]es / [n]ever / [Enter] later: ",
        ep.model, ep.base_url, backend.label()
    );
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => eprintln!(
            "{}",
            install(&store, "03:00", Some((&ep.model, &ep.base_url)))?
        ),
        "n" | "no" | "never" => {
            let _g = store
                .try_lock(".schedule.lock")
                .context("Dream schedule is busy; try again after the current run finishes")?;
            store.replace(&choice, "never\n")?;
        }
        _ => {}
    }
    Ok(())
}
