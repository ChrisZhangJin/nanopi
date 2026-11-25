//! User crontab fallback. Modify only this home's tagged job, passing the
//! table over stdin rather than invoking a shell to edit it.
use super::*;

fn table() -> Result<String> {
    let out = Command::new("crontab")
        .arg("-l")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .context("cannot run crontab -l")?;
    if out.status.success() {
        return String::from_utf8(out.stdout).context("crontab is not UTF-8; left unchanged");
    }
    let err = String::from_utf8_lossy(&out.stderr);
    // No existing table is normal. Do not mistake permission errors or
    // a damaged spool for an empty table and overwrite the user's jobs.
    if out.status.code() == Some(1)
        && out.stdout.is_empty()
        && (err.trim_start().starts_with("no crontab for ")
            || err.trim_start().starts_with("crontab: no crontab for "))
    {
        return Ok(String::new());
    }
    bail!("cannot read user crontab: {}", err.trim());
}

fn daemon() -> Result<()> {
    let is_cron = |s: &str| matches!(s.trim(), "cron" | "crond" | "fcron" | "bcron-exec");
    if command("ps", &["-e", "-o", "comm="]).is_ok_and(|s| s.lines().any(is_cron)) {
        return Ok(());
    }
    // BusyBox ps may not accept the procps flags.
    if fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
                && fs::read_to_string(e.path().join("comm")).is_ok_and(|s| is_cron(&s))
        })
    {
        return Ok(());
    }
    bail!("no running cron daemon detected");
}

pub(super) fn supported() -> Result<()> {
    daemon()?;
    table()?;
    Ok(())
}

fn marker(name: &str) -> String {
    format!(" # Managed by nanopi Dream: {name}")
}

fn managed(line: &str, tag: &str) -> bool {
    line.trim_end_matches(['\r', '\n'])
        .strip_suffix(tag)
        .is_some_and(|job| {
            !job.trim_start().starts_with('#') && job.ends_with(" --dream-schedule run >/dev/null")
        })
}

fn rewrite(name: &str, job: Option<&str>) -> Result<()> {
    let old = table()?;
    let tag = marker(name);
    let mut new: String = old
        .split_inclusive('\n')
        .filter(|line| !managed(line, &tag))
        .collect();
    if let Some(job) = job {
        if !new.is_empty() && !new.ends_with('\n') {
            new.push('\n');
        }
        new.push_str(job);
        new.push_str(&tag);
        new.push('\n');
    }
    if old == new {
        return Ok(());
    }
    ensure!(
        table()? == old,
        "crontab changed during setup; please retry"
    );
    let mut child = Command::new("crontab")
        .arg("-")
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("cannot update user crontab")?;
    let write = child
        .stdin
        .take()
        .context("missing crontab stdin")?
        .write_all(new.as_bytes());
    let out = child.wait_with_output()?;
    ensure!(
        out.status.success(),
        "crontab install failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    write?;
    let installed = table()?;
    let jobs: Vec<_> = installed
        .lines()
        .filter(|line| managed(line, &tag))
        .collect();
    match job {
        Some(job) => ensure!(
            jobs == [format!("{job}{tag}")],
            "could not verify installed Dream cron job"
        ),
        None => ensure!(
            jobs.is_empty(),
            "could not verify removal of Dream cron job"
        ),
    }
    Ok(())
}

fn shell_quote(path: &Path) -> Result<String> {
    let s = path.to_str().context("non-UTF-8 cron path")?;
    // Cron interprets '%' before the shell sees quotes. Reject it rather
    // than depending on implementation-specific escaping.
    ensure!(
        !s.chars().any(|c| c == '%' || c.is_control()),
        "cron paths cannot contain '%' or control characters"
    );
    Ok(format!("'{}'", s.replace('\'', "'\\''")))
}

pub(super) fn install(home: &Path, exe: &Path, name: &str, time: &str) -> Result<()> {
    let (hour, minute) = time.split_once(':').context("invalid cron time")?;
    let (hour, minute) = (hour.parse::<u8>()?, minute.parse::<u8>()?);
    let job = format!(
        "{minute} {hour} * * * {} --dream-home {} --dream-schedule run >/dev/null",
        shell_quote(exe)?,
        shell_quote(home)?
    );
    rewrite(name, Some(&job))
}

pub(super) fn remove(name: &str) -> Result<()> {
    rewrite(name, None)
}

pub(super) fn active(name: &str) -> Result<()> {
    daemon()?;
    let tag = marker(name);
    ensure!(
        table()?.lines().filter(|line| managed(line, &tag)).count() == 1,
        "Dream job is missing or duplicated in user crontab"
    );
    Ok(())
}
