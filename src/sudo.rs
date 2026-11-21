//! Per-command `sudo` policy for the `bash` tool.
//!
//! nanopi never runs a whole session as root. Instead, when the user's
//! account can use sudo, the model may prefix the individual commands
//! that need root with `sudo -n`, and this module decides — per command,
//! before the shell is spawned — whether that command may run:
//!
//! - `[sudo] mode = "ask"` (default): in the TUI every sudo command is
//!   shown to the user and runs only after an explicit `y`.
//! - `mode = "auto"`: sudo commands run without confirmation.
//! - `mode = "never"`: sudo commands are refused.
//! - Agents (child `nanopi -p` processes) can never ask; they may use
//!   sudo only with `[sudo] agents = true`. Print mode (`-p`) cannot ask
//!   either, so there `"ask"` refuses and only `"auto"` allows.
//!
//! Only passwordless sudo is supported: availability is probed once at
//! startup with `sudo -n true`. A sudo that needs a password is refused
//! with an explanation rather than run, because the `bash` tool has no
//! stdin and sudo would otherwise fight the TUI for the terminal.
//!
//! **This is a guard against accidents, not a sandbox.** Spotting sudo
//! in a shell string is heuristic ([`invokes_sudo`]); a command that
//! hides it (a script file, `eval`, string building) is not caught. The
//! `never` mode plus a sudoers policy is the real boundary.

use serde::Deserialize;
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use tokio::sync::oneshot;

/// `[sudo] mode`. Ordered from most to least restrictive, so the
/// global/project merge can take the minimum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SudoMode {
    Never,
    Ask,
    Auto,
}

impl SudoMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SudoMode::Never => "never",
            SudoMode::Ask => "ask",
            SudoMode::Auto => "auto",
        }
    }
}

/// `[sudo]` as written in a config file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct SudoConfig {
    pub mode: Option<SudoMode>,
    pub agents: Option<bool>,
}

impl SudoConfig {
    /// Global (`a`) + project (`b`). Unlike every other section, a
    /// project can only make sudo STRICTER: a cloned repository's
    /// `.nanopi/config.toml` must never be able to turn on unattended
    /// root. So the mode is the more restrictive of the two, and agents
    /// are allowed only if the global file allows them and the project
    /// does not forbid them.
    pub fn merge(a: Self, b: Self) -> Self {
        let mode = match (a.mode, b.mode) {
            (Some(x), Some(y)) => Some(x.min(y)),
            (Some(x), None) => Some(x),
            // A project may lower the default, never raise it.
            (None, Some(y)) => Some(y.min(SudoMode::Ask)),
            (None, None) => None,
        };
        let agents = match (a.agents, b.agents) {
            (Some(true), Some(false)) => Some(false),
            (Some(x), _) => Some(x),
            (None, Some(_)) => Some(false),
            (None, None) => None,
        };
        Self { mode, agents }
    }

    pub fn mode(&self) -> SudoMode {
        self.mode.unwrap_or(SudoMode::Ask)
    }

    pub fn agents(&self) -> bool {
        self.agents.unwrap_or(false)
    }
}

/// What `sudo` can do for this user, probed once at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    /// nanopi already runs as root; sudo adds nothing and is not gated.
    Root,
    /// `sudo -n true` succeeded.
    Passwordless,
    /// sudo exists and this user may use it, but only with a password.
    NeedsPassword,
    /// No sudo binary, or this user is not allowed to use it.
    Unavailable,
}

/// Which kind of nanopi process this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The interactive TUI: can ask the user.
    Tui,
    /// `nanopi -p` run by the user: cannot ask.
    Print,
    /// A child agent (`NANOPI_AGENT_ID` set): cannot ask.
    Agent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub mode: SudoMode,
    pub agents: bool,
    pub availability: Availability,
    pub role: Role,
}

static POLICY: OnceLock<Policy> = OnceLock::new();

/// Install the process-wide policy. First call wins; later calls are
/// ignored (startup sets it exactly once).
pub fn set_policy(p: Policy) {
    let _ = POLICY.set(p);
}

pub fn policy() -> Option<Policy> {
    POLICY.get().copied()
}

/// Probe sudo without ever prompting: `sudo -n` fails instead of asking
/// for a password. Skipped (reported `Unavailable`) when the policy
/// could never allow sudo anyway, so no process is spawned for nothing.
pub fn detect(mode: SudoMode, role: Role, agents: bool) -> Availability {
    // No sudo outside unix (and `libc` is a unix-only dependency).
    #[cfg(not(unix))]
    {
        let _ = (mode, role, agents);
        Availability::Unavailable
    }
    #[cfg(unix)]
    detect_unix(mode, role, agents)
}

#[cfg(unix)]
fn detect_unix(mode: SudoMode, role: Role, agents: bool) -> Availability {
    // SAFETY: geteuid has no preconditions and cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        return Availability::Root;
    }
    if mode == SudoMode::Never || (role == Role::Agent && !agents) {
        return Availability::Unavailable;
    }
    let out = std::process::Command::new("sudo")
        .args(["-n", "true"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output();
    match out {
        Ok(o) if o.status.success() => Availability::Passwordless,
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr).to_ascii_lowercase();
            if err.contains("password is required") {
                Availability::NeedsPassword
            } else {
                Availability::Unavailable
            }
        }
        Err(_) => Availability::Unavailable,
    }
}

/// Words that run their arguments as a command, so a `sudo` right after
/// one is still in command position (`env FOO=1 sudo …`, `xargs sudo …`).
const WRAPPERS: &[&str] = &[
    "env", "exec", "nohup", "command", "builtin", "nice", "xargs", "stdbuf", "setsid", "time",
    "timeout", "ionice", "chrt", "!", "then", "do", "else", "if", "elif", "while", "until",
];

/// Characters that end one simple command and start the next. Quotes
/// are included on purpose so `bash -c "sudo …"` is caught; the price is
/// that `echo "sudo …"` asks too, which errs on the safe side.
fn is_separator(c: char) -> bool {
    matches!(c, ';' | '&' | '|' | '(' | ')' | '`' | '\n' | '{' | '}' | '"' | '\'' | '$')
}

/// Whether `cmd` runs `sudo` in command position anywhere. `grep sudo
/// /etc/group` does not count (there `sudo` is an argument); `apt update
/// && sudo apt install x`, `/usr/bin/sudo x` and `env A=1 sudo x` do.
pub fn invokes_sudo(cmd: &str) -> bool {
    for segment in cmd.split(is_separator) {
        for word in segment.split_whitespace() {
            let base = word.rsplit('/').next().unwrap_or(word);
            if base == "sudo" {
                return true;
            }
            let is_assignment = word
                .split_once('=')
                .is_some_and(|(k, _)| !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
            let is_option = word.starts_with('-');
            let is_number = word.chars().all(|c| c.is_ascii_digit() || c == '.' || c == 's' || c == 'm');
            if WRAPPERS.contains(&base) || is_assignment || is_option || is_number {
                continue;
            }
            // First real word of this simple command and it is not sudo.
            break;
        }
    }
    false
}

/// A pending "may this sudo command run?" question for the TUI.
pub struct SudoRequest {
    pub command: String,
    reply: oneshot::Sender<bool>,
}

impl SudoRequest {
    pub fn answer(self, allow: bool) {
        let _ = self.reply.send(allow);
    }

    /// The tool call that asked is gone (its turn was cancelled), so
    /// nobody is waiting for an answer any more.
    pub fn is_abandoned(&self) -> bool {
        self.reply.is_closed()
    }
}

static QUEUE: Mutex<VecDeque<SudoRequest>> = Mutex::new(VecDeque::new());

/// Next pending request, oldest first. Drained by the TUI's tick.
pub fn take_request() -> Option<SudoRequest> {
    QUEUE.lock().unwrap_or_else(|e| e.into_inner()).pop_front()
}

async fn ask(command: &str) -> bool {
    let (tx, rx) = oneshot::channel();
    QUEUE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push_back(SudoRequest { command: command.to_string(), reply: tx });
    rx.await.unwrap_or(false)
}

/// Decide whether `cmd` may run. `Err` carries the refusal, written for
/// the model, which the `bash` tool returns as an in-band error.
pub async fn check(cmd: &str) -> Result<(), String> {
    let Some(p) = policy() else { return Ok(()) };
    if !invokes_sudo(cmd) {
        return Ok(());
    }
    if let Some(refusal) = static_refusal(&p) {
        return Err(refusal);
    }
    match (p.role, p.mode) {
        (Role::Tui, SudoMode::Ask) => {
            if ask(cmd).await {
                Ok(())
            } else {
                Err("the user declined this sudo command. Do not retry it or work around it; \
                     ask the user how they want to proceed."
                    .to_string())
            }
        }
        _ => Ok(()),
    }
}

/// The refusal that applies regardless of what the user would answer,
/// or `None` when the command may run (possibly after asking).
fn static_refusal(p: &Policy) -> Option<String> {
    const GIVE_COMMAND: &str = "If the step really needs root, give the user the exact command \
                                to run themselves instead.";
    if p.availability == Availability::Root {
        return None;
    }
    if p.mode == SudoMode::Never {
        return Some(format!(
            "sudo is disabled in the user's nanopi config ([sudo] mode = \"never\"). {GIVE_COMMAND}"
        ));
    }
    if p.role == Role::Agent && !p.agents {
        return Some(
            "agents may not use sudo ([sudo] agents = false). Do not retry; list the exact \
             command that needs root under Open issues in your report."
                .to_string(),
        );
    }
    match p.availability {
        Availability::NeedsPassword => {
            return Some(format!(
                "sudo needs a password for this user, and nanopi cannot supply one. {GIVE_COMMAND}"
            ))
        }
        Availability::Unavailable => {
            return Some(format!("sudo is not available to this user. {GIVE_COMMAND}"))
        }
        _ => {}
    }
    if p.role == Role::Print && p.mode == SudoMode::Ask {
        return Some(format!(
            "sudo needs the user's confirmation, which a non-interactive run cannot ask for \
             ([sudo] mode = \"ask\"; \"auto\" would allow it). {GIVE_COMMAND}"
        ));
    }
    None
}

/// The system-prompt paragraph describing sudo for this process, or
/// `None` when there is nothing worth saying (no policy installed —
/// tests and snapshots — running as root, or no sudo at all).
pub fn prompt_note() -> Option<String> {
    prompt_note_for(&policy()?)
}

fn prompt_note_for(p: &Policy) -> Option<String> {
    if p.availability == Availability::Root || p.availability == Availability::Unavailable {
        return None;
    }
    if let Some(refusal) = static_refusal(p) {
        return Some(format!("Do not use sudo: {refusal}"));
    }
    let confirm = if p.mode == SudoMode::Ask {
        " Each sudo command is shown to the user, who must approve it before it runs; if \
         they decline, do not retry or work around it."
    } else {
        ""
    };
    Some(format!(
        "sudo is available without a password. Use it only for the individual commands that \
         genuinely need root (system packages, system files, services), never for work inside \
         the project, and never to start a root shell (no `sudo su`, `sudo -i`, `sudo bash`). \
         Always write it as `sudo -n <command>` so it fails instead of waiting for a \
         password.{confirm}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pol(mode: SudoMode, availability: Availability, role: Role, agents: bool) -> Policy {
        Policy { mode, agents, availability, role }
    }

    #[test]
    fn detects_sudo_in_command_position() {
        for cmd in [
            "sudo apt install x",
            "sudo -n systemctl restart nginx",
            "apt update && sudo apt install x",
            "cd /tmp; sudo rm -rf foo",
            "/usr/bin/sudo ls /root",
            "env DEBIAN_FRONTEND=noninteractive sudo apt-get install -y x",
            "FOO=1 sudo make install",
            "find . -name '*.o' | xargs sudo rm",
            "bash -c \"sudo whoami\"",
            "echo $(sudo cat /etc/shadow)",
            "timeout 30 sudo journalctl -n 5",
            "if true; then sudo true; fi",
        ] {
            assert!(invokes_sudo(cmd), "should detect: {cmd}");
        }
    }

    #[test]
    fn ignores_sudo_as_an_argument() {
        for cmd in [
            "grep sudo /etc/group",
            "ls /etc/sudoers.d",
            "man sudoers",
            "git log --grep=pseudo",
            "cargo test sudo::tests",
            "id -nG | tr ' ' '\\n'",
        ] {
            assert!(!invokes_sudo(cmd), "should not detect: {cmd}");
        }
    }

    #[test]
    fn project_config_can_only_tighten() {
        let g = |mode, agents| SudoConfig { mode, agents };
        // Project cannot raise the mode above the global one…
        let m = SudoConfig::merge(g(Some(SudoMode::Ask), None), g(Some(SudoMode::Auto), None));
        assert_eq!(m.mode(), SudoMode::Ask);
        // …nor above the default when the global file says nothing.
        let m = SudoConfig::merge(g(None, None), g(Some(SudoMode::Auto), Some(true)));
        assert_eq!(m.mode(), SudoMode::Ask);
        assert!(!m.agents());
        // It can lower it.
        let m = SudoConfig::merge(g(Some(SudoMode::Auto), Some(true)), g(Some(SudoMode::Never), Some(false)));
        assert_eq!(m.mode(), SudoMode::Never);
        assert!(!m.agents());
        // Global alone is honoured.
        let m = SudoConfig::merge(g(Some(SudoMode::Auto), Some(true)), g(None, None));
        assert_eq!(m.mode(), SudoMode::Auto);
        assert!(m.agents());
        // Defaults.
        let m = SudoConfig::merge(g(None, None), g(None, None));
        assert_eq!(m.mode(), SudoMode::Ask);
        assert!(!m.agents());
    }

    #[test]
    fn parses_from_toml() {
        let c: SudoConfig = toml::from_str("mode = \"auto\"\nagents = true").unwrap();
        assert_eq!(c.mode, Some(SudoMode::Auto));
        assert_eq!(c.agents, Some(true));
        assert!(toml::from_str::<SudoConfig>("mode = \"always\"").is_err());
    }

    #[test]
    fn refusals_follow_the_policy_table() {
        use Availability::*;
        use Role::*;
        use SudoMode::*;
        // Allowed without a static refusal (TUI+ask still asks at runtime).
        for p in [
            pol(Ask, Passwordless, Tui, false),
            pol(Auto, Passwordless, Tui, false),
            pol(Auto, Passwordless, Print, false),
            pol(Ask, Passwordless, Agent, true),
            pol(Never, Root, Tui, false),
        ] {
            assert_eq!(static_refusal(&p), None, "{p:?}");
        }
        // Refused.
        for (p, needle) in [
            (pol(Never, Passwordless, Tui, false), "never"),
            (pol(Auto, Passwordless, Agent, false), "agents may not"),
            (pol(Auto, NeedsPassword, Tui, false), "password"),
            (pol(Auto, Unavailable, Tui, false), "not available"),
            (pol(Ask, Passwordless, Print, false), "confirmation"),
        ] {
            let r = static_refusal(&p).unwrap_or_else(|| panic!("expected refusal for {p:?}"));
            assert!(r.contains(needle), "{p:?}: {r}");
        }
    }

    #[test]
    fn prompt_note_matches_policy() {
        use Availability::*;
        use Role::*;
        use SudoMode::*;
        assert_eq!(prompt_note_for(&pol(Ask, Root, Tui, false)), None);
        assert_eq!(prompt_note_for(&pol(Ask, Unavailable, Tui, false)), None);
        let ask = prompt_note_for(&pol(Ask, Passwordless, Tui, false)).unwrap();
        assert!(ask.contains("sudo -n") && ask.contains("approve"), "{ask}");
        assert!(ask.contains("sudo su"), "must forbid root shells: {ask}");
        let auto = prompt_note_for(&pol(Auto, Passwordless, Tui, false)).unwrap();
        assert!(auto.contains("sudo -n") && !auto.contains("approve"), "{auto}");
        let never = prompt_note_for(&pol(Never, Passwordless, Tui, false)).unwrap();
        assert!(never.starts_with("Do not use sudo"), "{never}");
    }

    /// One test, not two: the queue is process-global, and two tests
    /// draining it in parallel would steal each other's requests.
    #[tokio::test]
    async fn ask_round_trips_and_a_dropped_asker_is_abandoned() {
        async fn next() -> SudoRequest {
            loop {
                if let Some(r) = take_request() {
                    return r;
                }
                tokio::task::yield_now().await;
            }
        }

        let asker = tokio::spawn(async { ask("sudo -n true").await });
        let req = next().await;
        assert_eq!(req.command, "sudo -n true");
        assert!(!req.is_abandoned());
        req.answer(true);
        assert!(asker.await.unwrap());

        let asker = tokio::spawn(async { ask("sudo -n false").await });
        let req = next().await;
        asker.abort();
        let _ = asker.await;
        assert!(req.is_abandoned());
    }
}
