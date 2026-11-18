//! First-run wizard for nanopi.
//!
//! Bootstraps `~/.nanopi/config.toml` when a user runs nanopi with no
//! config, no env vars, and no CLI creds. Also reachable explicitly via
//! `nanopi init`, and for a named profile via `nanopi --profile NAME
//! init` or the TUI's `/profile create NAME`, which write into
//! `~/.nanopi/profiles/NAME/` instead. The questions themselves are the
//! I/O-free [`Flow`], shared by both front ends.
//!
//! Design constraints (see `.planning/quick/260819-ayr-.../PLAN.md`):
//! - Zero new crates. Uses only `reqwest`, `serde_json`, `anyhow`,
//!   `tokio`, `toml`, and `std`.
//! - Follows `src/trust.rs` style for stdin prompts (plain
//!   `std::io::stdin().read_line`; no dialoguer, no echo-hiding for v1).
//! - Probes the provider BEFORE writing config so a bad key doesn't
//!   leave the user with a broken `~/.nanopi/config.toml`.
//! - Writes the api key to a separate `~/.nanopi/api_key` file with
//!   Unix mode 0600, never inline in TOML.
//! - Localhost base URLs (Ollama et al.) skip the key prompt and skip
//!   writing the key file entirely.

use std::io::{self, Write};
use std::path::Path;

use anyhow::{anyhow, Context as _};
use serde_json::json;

use crate::paths;

/// A canned provider entry the wizard offers on the pick-list.
#[derive(Debug, Clone)]
pub struct ProviderPreset {
    /// Human-facing label shown on the numbered list.
    pub label: &'static str,
    /// Default `base_url`. Empty for `Custom` (prompted).
    pub base_url: &'static str,
    /// Default wire protocol. `"openai"` or `"anthropic"`. Empty for Custom.
    pub api_kind: &'static str,
    /// Default model id. Empty for Custom.
    pub default_model: &'static str,
    /// True for the trailing "Custom" entry — the wizard prompts for
    /// each field instead of using presets.
    pub is_custom: bool,
}

/// The six canonical presets, in display order.
pub static PROVIDER_PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        label: "OpenAI",
        base_url: "https://api.openai.com/v1",
        api_kind: "openai",
        default_model: "gpt-4o-mini",
        is_custom: false,
    },
    ProviderPreset {
        label: "DeepSeek",
        base_url: "https://api.deepseek.com/v1",
        api_kind: "openai",
        default_model: "deepseek-chat",
        is_custom: false,
    },
    ProviderPreset {
        label: "Anthropic direct",
        base_url: "https://api.anthropic.com",
        api_kind: "anthropic",
        default_model: "claude-sonnet-4-5",
        is_custom: false,
    },
    ProviderPreset {
        label: "Gemini via gateway",
        base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
        api_kind: "openai",
        default_model: "gemini-2.5-flash",
        is_custom: false,
    },
    ProviderPreset {
        label: "Ollama (local)",
        base_url: "http://localhost:11434/v1",
        api_kind: "openai",
        default_model: "llama3.2",
        is_custom: false,
    },
    ProviderPreset {
        label: "Custom",
        base_url: "",
        api_kind: "",
        default_model: "",
        is_custom: true,
    },
];

/// Recognize localhost base URLs so the wizard can skip the api-key
/// prompt for local backends (Ollama et al.).
pub fn is_localhost(url: &str) -> bool {
    url.contains("localhost") || url.contains("127.0.0.1") || url.contains("0.0.0.0")
}

/// Send ONE tiny non-streaming request to validate credentials +
/// base_url + model. `max_tokens=1`, single "hi" user message. Returns
/// Ok on any 2xx, Err with the HTTP status + body snippet otherwise.
///
/// We hand-roll a minimal JSON POST here instead of reusing
/// `OpenAiProvider` / `AnthropicProvider` — the real providers stream
/// and drag in the full agent context, which is way more machinery than
/// a bootstrap probe needs.
pub async fn probe_config(
    base_url: &str,
    api_kind: &str,
    model: &str,
    api_key: &str,
) -> Result<(), String> {
    // Bounded: an unreachable host must not hang the wizard (in the TUI
    // that would freeze the whole screen).
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_default();
    let base = base_url.trim_end_matches('/');
    let resp = if api_kind == "anthropic" {
        let url = format!("{base}/v1/messages");
        let body = json!({
            "model": model,
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "hi"}],
        });
        client
            .post(&url)
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
    } else {
        let url = format!("{base}/chat/completions");
        let body = json!({
            "model": model,
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "hi"}],
            "stream": false,
        });
        let mut req = client
            .post(&url)
            .header("content-type", "application/json")
            .json(&body);
        if !api_key.is_empty() {
            req = req.bearer_auth(api_key);
        }
        req.send().await
    };

    match resp {
        Ok(r) if r.status().is_success() => Ok(()),
        Ok(r) => {
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
            // A wrong base_url gets an HTML error page here, and 240
            // raw chars of that is unreadable — same flattening the
            // provider error path uses.
            let snippet = crate::provider::retry::flatten_error_body(&body, 240);
            Err(format!("HTTP {}: {snippet}", status.as_u16()))
        }
        Err(e) => {
            use std::error::Error;
            let mut msg = e.to_string();
            let mut src: Option<&dyn Error> = e.source();
            while let Some(s) = src {
                msg.push_str(&format!(": {s}"));
                src = s.source();
            }
            Err(msg)
        }
    }
}

/// Write the api key to `path` with Unix mode 0o600. Creates parent
/// directories if needed. Non-unix platforms fall back to a plain
/// write (no chmod).
pub fn write_key_file(path: &Path, key: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent dir for {}", path.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("open {} for write", path.display()))?;
        f.write_all(key.as_bytes())
            .with_context(|| format!("write {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, key)
            .with_context(|| format!("write {}", path.display()))?;
    }
    Ok(())
}

/// Emit a minimal `config.toml` containing just the four essentials.
///
/// `api_key_file` should be an absolute path — the wizard passes the
/// very path it wrote the key to. Backslashes are escaped by
/// `toml_escape`, so Windows paths survive the round-trip verbatim.
pub fn write_config_toml(
    path: &Path,
    model: &str,
    base_url: &str,
    api_kind: &str,
    api_key_file: Option<&str>,
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent dir for {}", path.display()))?;
    }
    let mut s = String::new();
    s.push_str("# generated by `nanopi init`\n");
    s.push_str(&format!("model = {}\n", toml_escape(model)));
    s.push_str(&format!("base_url = {}\n", toml_escape(base_url)));
    s.push_str(&format!("api_kind = {}\n", toml_escape(api_kind)));
    if let Some(kf) = api_key_file {
        s.push_str(&format!("api_key_file = {}\n", toml_escape(kf)));
    }
    std::fs::write(path, s).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Minimal TOML string-escape — quotes and backslashes. Enough for
/// file paths, URLs, and model ids.
fn toml_escape(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Read one trimmed line from stdin. `prompt` is written to stdout
/// first and flushed. Returns the line without its trailing newline.
fn read_line(prompt: &str) -> anyhow::Result<String> {
    print!("{prompt}");
    io::stdout().flush().ok();
    let mut buf = String::new();
    let n = io::stdin()
        .read_line(&mut buf)
        .context("read stdin")?;
    if n == 0 {
        // EOF — closed stdin (pipe finished, ^D). Bail so callers'
        // "required" retry loops don't spin forever on empty reads.
        anyhow::bail!("wizard: stdin closed before input was provided");
    }
    Ok(clean_input(&buf))
}

/// Trim an answer and drop one pair of surrounding quotes (people paste
/// `"sk-..."`). Shared by the stdin and TUI front ends.
pub fn clean_input(raw: &str) -> String {
    let s = raw.trim();
    let s = if s.len() >= 2 {
        let b = s.as_bytes();
        let first = b[0];
        let last = b[s.len() - 1];
        if (first == b'"' || first == b'\'') && first == last {
            &s[1..s.len() - 1]
        } else {
            s
        }
    } else {
        s
    };
    s.to_string()
}

/// A partial wizard attempt, persisted between runs so the user can
/// resume after a probe failure or Ctrl-C without re-typing long URLs
/// and API keys.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WizardDraft {
    base_url: Option<String>,
    api_kind: Option<String>,
    model: Option<String>,
    api_key: Option<String>,
}

/// Very small TOML reader for the 4 known keys. Missing file → empty draft.
fn load_draft(path: &Path) -> WizardDraft {
    let mut d = WizardDraft::default();
    let Ok(s) = std::fs::read_to_string(path) else { return d };
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        let Some((k, v)) = line.split_once('=') else { continue };
        let k = k.trim();
        let v = v.trim().trim_matches('"').to_string();
        match k {
            "base_url" => d.base_url = Some(v),
            "api_kind" => d.api_kind = Some(v),
            "model" => d.model = Some(v),
            "api_key" => d.api_key = Some(v),
            _ => {}
        }
    }
    d
}

/// Write the draft with 0600 perms (it contains the api key).
fn save_draft(path: &Path, d: &WizardDraft) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent dir for {}", path.display()))?;
    }
    let mut s = String::new();
    s.push_str("# nanopi wizard draft — deleted on successful config write\n");
    if let Some(v) = &d.base_url { s.push_str(&format!("base_url = {}\n", toml_escape(v))); }
    if let Some(v) = &d.api_kind { s.push_str(&format!("api_kind = {}\n", toml_escape(v))); }
    if let Some(v) = &d.model { s.push_str(&format!("model = {}\n", toml_escape(v))); }
    if let Some(v) = &d.api_key { s.push_str(&format!("api_key = {}\n", toml_escape(v))); }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true).create(true).truncate(true).mode(0o600)
            .open(path)
            .with_context(|| format!("open {} for write", path.display()))?;
        f.write_all(s.as_bytes())
            .with_context(|| format!("write {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, s)
            .with_context(|| format!("write {}", path.display()))?;
    }
    Ok(())
}

/// Where one wizard run writes: a config, its key file and the resume
/// draft, all in one folder. For the `default` profile that folder is
/// the nanopi home (`~/.nanopi/config.toml`, `~/.nanopi/api_key`, as
/// always); for any other profile it is `~/.nanopi/profiles/<name>/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WizardTarget {
    pub cfg_path: std::path::PathBuf,
    pub key_path: std::path::PathBuf,
    pub draft_path: std::path::PathBuf,
}

impl WizardTarget {
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            cfg_path: dir.join("config.toml"),
            key_path: dir.join("api_key"),
            draft_path: dir.join("wizard_draft.toml"),
        }
    }

    /// The target for profile `name` under `home`.
    pub fn for_profile(home: &Path, name: &str) -> Self {
        Self::in_dir(&crate::profile::dir_in(home, name))
    }

    pub fn load_draft(&self) -> WizardDraft {
        load_draft(&self.draft_path)
    }
}

/// What the user answered, ready to probe and write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answers {
    pub base_url: String,
    pub api_kind: String,
    pub model: String,
    /// Empty for localhost endpoints.
    pub api_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Provider,
    BaseUrl,
    ApiKind,
    Model,
    ApiKey,
    ProbeFailed,
}

/// What the front end should do after an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    /// Show this prompt and feed the reply to [`Flow::answer`].
    Ask(String),
    /// Everything is answered: probe [`Flow::answers`], then call
    /// [`finish`] on success or [`Flow::probe_failed`] on failure.
    Probe,
    /// The user aborted; nothing is written.
    Abort,
}

/// Lines to show (in order), then what to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub lines: Vec<String>,
    pub next: Next,
}

impl Reply {
    fn ask(lines: Vec<String>, prompt: String) -> Self {
        Self { lines, next: Next::Ask(prompt) }
    }
}

/// The wizard's questions as a state machine, with no I/O, so the
/// stdin wizard (`nanopi init`) and the TUI's `/profile create` ask
/// exactly the same things in the same order.
#[derive(Debug, Clone)]
pub struct Flow {
    step: Step,
    preset: usize,
    base_url: String,
    api_kind: String,
    model: String,
    api_key: String,
    draft: WizardDraft,
}

fn prompt_default(label: &str, default: Option<&str>) -> String {
    match default {
        Some(d) if !d.is_empty() => format!("{label} [{d}]: "),
        _ => format!("{label}: "),
    }
}

impl Flow {
    /// Begin, with a saved draft's values as defaults.
    pub fn start(draft: WizardDraft) -> (Self, Reply) {
        let mut f = Self {
            step: Step::Provider,
            preset: 0,
            base_url: String::new(),
            api_kind: String::new(),
            model: String::new(),
            api_key: String::new(),
            draft,
        };
        let r = f.provider_menu();
        (f, r)
    }

    fn provider_menu(&mut self) -> Reply {
        self.step = Step::Provider;
        let mut lines = vec![String::new(), "Select a provider:".to_string()];
        for (i, p) in PROVIDER_PRESETS.iter().enumerate() {
            lines.push(format!("  {}) {}", i + 1, p.label));
        }
        Reply::ask(lines, "Choice [1]: ".to_string())
    }

    /// True while the question is the API key, so a front end that
    /// echoes answers can mask this one.
    pub fn asking_secret(&self) -> bool {
        self.step == Step::ApiKey
    }

    pub fn answers(&self) -> Answers {
        Answers {
            base_url: self.base_url.clone(),
            api_kind: self.api_kind.clone(),
            model: self.model.clone(),
            api_key: self.api_key.clone(),
        }
    }

    /// The answers as a draft, saved before probing so an abort or a
    /// failed probe can be resumed without retyping.
    pub fn draft(&self) -> WizardDraft {
        WizardDraft {
            base_url: Some(self.base_url.clone()),
            api_kind: Some(self.api_kind.clone()),
            model: Some(self.model.clone()),
            api_key: (!self.api_key.is_empty()).then(|| self.api_key.clone()),
        }
    }

    fn after_model(&mut self) -> Reply {
        if is_localhost(&self.base_url) {
            self.api_key.clear();
            return Reply { lines: vec![], next: Next::Probe };
        }
        self.step = Step::ApiKey;
        Reply::ask(vec![], self.api_key_prompt())
    }

    /// Feed one answer (already trimmed of surrounding quotes by
    /// [`clean_input`]).
    pub fn answer(&mut self, input: &str) -> Reply {
        let or_default = |v: &str, d: Option<&str>| {
            if v.is_empty() { d.unwrap_or("").to_string() } else { v.to_string() }
        };
        match self.step {
            Step::Provider => {
                let sel = if input.is_empty() { "1" } else { input };
                match sel.parse::<usize>() {
                    Ok(n) if n >= 1 && n <= PROVIDER_PRESETS.len() => {
                        self.preset = n - 1;
                        let p = &PROVIDER_PRESETS[self.preset];
                        if p.is_custom {
                            self.step = Step::BaseUrl;
                            Reply::ask(vec![], prompt_default("base_url", self.draft.base_url.as_deref()))
                        } else {
                            self.step = Step::Model;
                            Reply::ask(vec![], format!("model [{}]: ", p.default_model))
                        }
                    }
                    _ => Reply::ask(
                        vec!["invalid selection, try again.".to_string()],
                        "Choice [1]: ".to_string(),
                    ),
                }
            }
            Step::BaseUrl => {
                let v = or_default(input, self.draft.base_url.as_deref());
                if v.is_empty() {
                    return Reply::ask(
                        vec!["base_url is required.".to_string()],
                        prompt_default("base_url", self.draft.base_url.as_deref()),
                    );
                }
                self.base_url = v;
                self.step = Step::ApiKind;
                Reply::ask(vec![], self.api_kind_prompt())
            }
            Step::ApiKind => {
                let v = or_default(input, Some(self.draft.api_kind.as_deref().unwrap_or("openai")));
                if v != "openai" && v != "anthropic" {
                    return Reply::ask(
                        vec!["api_kind must be `openai` or `anthropic`.".to_string()],
                        self.api_kind_prompt(),
                    );
                }
                self.api_kind = v;
                self.step = Step::Model;
                Reply::ask(vec![], prompt_default("model", self.draft.model.as_deref()))
            }
            Step::Model => {
                let p = &PROVIDER_PRESETS[self.preset];
                if p.is_custom {
                    let v = or_default(input, self.draft.model.as_deref());
                    if v.is_empty() {
                        return Reply::ask(
                            vec!["model is required.".to_string()],
                            prompt_default("model", self.draft.model.as_deref()),
                        );
                    }
                    self.model = v;
                } else {
                    self.model = or_default(input, Some(p.default_model));
                    self.base_url = p.base_url.to_string();
                    self.api_kind = p.api_kind.to_string();
                }
                self.after_model()
            }
            Step::ApiKey => {
                let v = or_default(input, self.draft.api_key.as_deref());
                if v.is_empty() {
                    return Reply::ask(
                        vec!["api key is required (or pick Ollama/localhost to skip).".to_string()],
                        self.api_key_prompt(),
                    );
                }
                self.api_key = v;
                Reply { lines: vec![], next: Next::Probe }
            }
            Step::ProbeFailed => match input.to_ascii_lowercase().as_str() {
                "r" | "retry" => Reply { lines: vec![], next: Next::Probe },
                "e" | "edit" => {
                    // Start over with what was typed as the defaults.
                    self.draft = self.draft();
                    self.provider_menu()
                }
                "a" | "abort" | "" => Reply {
                    lines: vec!["aborted; no config written.".to_string()],
                    next: Next::Abort,
                },
                _ => Reply {
                    lines: vec!["unrecognized choice; aborting.".to_string()],
                    next: Next::Abort,
                },
            },
        }
    }

    /// The key prompt. A saved key (from the draft) is offered as the
    /// default but never shown: the prompt lands in the terminal's
    /// scrollback, and in the TUI also in the status line.
    fn api_key_prompt(&self) -> String {
        match self.draft.api_key.as_deref() {
            Some(k) if !k.is_empty() => "api key [•••• saved, Enter to reuse]: ".to_string(),
            _ => "api key: ".to_string(),
        }
    }

    fn api_kind_prompt(&self) -> String {
        let d = self.draft.api_kind.as_deref().unwrap_or("openai");
        prompt_default("api_kind [openai/anthropic]", Some(d))
    }

    /// The probe failed with `err`: offer retry / edit / abort.
    pub fn probe_failed(&mut self, err: &str) -> Reply {
        self.step = Step::ProbeFailed;
        Reply::ask(
            vec![format!("probe failed: {err}")],
            "retry / edit / abort? [r/e/a]: ".to_string(),
        )
    }
}

/// Write the key file (empty for localhost) and the config, then drop
/// the draft. Returns the lines to report.
///
/// We always emit api_key_file so subsequent launches load cleanly —
/// the config loader accepts an empty key file for no-auth localhost
/// endpoints. The path we emit is the same absolute `key_path` we just
/// wrote to, never a `~/…` literal: the two sides used to disagree
/// about where `~` points, which broke every Windows first run.
pub fn finish(target: &WizardTarget, a: &Answers) -> anyhow::Result<Vec<String>> {
    write_key_file(&target.key_path, &a.api_key)?;
    write_config_toml(
        &target.cfg_path,
        &a.model,
        &a.base_url,
        &a.api_kind,
        Some(&target.key_path.to_string_lossy()),
    )?;
    let _ = std::fs::remove_file(&target.draft_path);
    Ok(vec![
        format!("wrote {}", target.cfg_path.display()),
        format!("wrote {} (mode 0600)", target.key_path.display()),
    ])
}

/// Save the draft before probing (it holds the key, so 0600).
pub fn save_target_draft(target: &WizardTarget, flow: &Flow) {
    let _ = save_draft(&target.draft_path, &flow.draft());
}

/// The interactive entry point, for the ACTIVE profile (so
/// `nanopi --profile work init` configures `work`).
///
/// `force_overwrite_prompt=true`: coming from `nanopi init`; if the
/// config already exists we prompt for confirmation before clobbering it.
///
/// `force_overwrite_prompt=false`: coming from the no-config
/// fallthrough in `main.rs`; there's no config to protect.
pub async fn run_wizard(force_overwrite_prompt: bool) -> anyhow::Result<()> {
    let home = paths::nanopi_home()
        .ok_or_else(|| anyhow!("cannot resolve ~/.nanopi (HOME unset and NANOPI_HOME unset)"))?;
    let target = WizardTarget::for_profile(&home, &crate::profile::current());
    run_wizard_at(&target, force_overwrite_prompt).await
}

/// The stdin front end over [`Flow`].
pub async fn run_wizard_at(target: &WizardTarget, force_overwrite_prompt: bool) -> anyhow::Result<()> {
    if force_overwrite_prompt && target.cfg_path.exists() {
        let ans = read_line(&format!(
            "{} already exists. Overwrite? [y/N]: ",
            target.cfg_path.display()
        ))?;
        let a = ans.to_ascii_lowercase();
        if a != "y" && a != "yes" {
            println!("aborted; existing config left untouched.");
            return Ok(());
        }
    }

    let (mut flow, mut reply) = Flow::start(target.load_draft());
    loop {
        for l in &reply.lines {
            println!("{l}");
        }
        reply = match reply.next {
            Next::Ask(prompt) => flow.answer(&read_line(&prompt)?),
            Next::Probe => {
                save_target_draft(target, &flow);
                let a = flow.answers();
                println!("probing {}...", a.base_url);
                match probe_config(&a.base_url, &a.api_kind, &a.model, &a.api_key).await {
                    Ok(()) => {
                        println!("probe OK.");
                        println!();
                        for l in finish(target, &a)? {
                            println!("{l}");
                        }
                        return Ok(());
                    }
                    Err(e) => flow.probe_failed(&e),
                }
            }
            Next::Abort => return Ok(()),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(r: &Reply) -> &str {
        match &r.next {
            Next::Ask(p) => p,
            other => panic!("expected a question, got {other:?}"),
        }
    }

    #[test]
    fn flow_preset_path_asks_model_then_key_then_probes() {
        let (mut f, r) = Flow::start(WizardDraft::default());
        assert!(r.lines.iter().any(|l| l.contains("1) OpenAI")));
        assert_eq!(ask(&r), "Choice [1]: ");
        assert!(f.answer("9").lines[0].contains("invalid"));
        let r = f.answer("2");
        assert_eq!(ask(&r), "model [deepseek-chat]: ");
        let r = f.answer("");
        assert_eq!(ask(&r), "api key: ");
        assert!(f.asking_secret());
        assert!(f.answer("").lines[0].contains("required"));
        assert_eq!(f.answer("sk-1").next, Next::Probe);
        assert_eq!(
            f.answers(),
            Answers {
                base_url: "https://api.deepseek.com/v1".into(),
                api_kind: "openai".into(),
                model: "deepseek-chat".into(),
                api_key: "sk-1".into(),
            }
        );
    }

    #[test]
    fn flow_custom_localhost_skips_the_key_and_uses_draft_defaults() {
        let draft = WizardDraft {
            base_url: Some("http://localhost:8080/v1".into()),
            api_kind: None,
            model: Some("qwen".into()),
            api_key: None,
        };
        let (mut f, _) = Flow::start(draft);
        let r = f.answer("6");
        assert_eq!(ask(&r), "base_url [http://localhost:8080/v1]: ");
        let r = f.answer("");
        assert_eq!(ask(&r), "api_kind [openai/anthropic] [openai]: ");
        assert!(f.answer("grpc").lines[0].contains("must be"));
        let r = f.answer("");
        assert_eq!(ask(&r), "model [qwen]: ");
        assert_eq!(f.answer("").next, Next::Probe, "localhost needs no key");
        let a = f.answers();
        assert_eq!((a.model.as_str(), a.api_key.as_str()), ("qwen", ""));
    }

    #[test]
    fn flow_probe_failure_offers_retry_edit_abort() {
        let (mut f, _) = Flow::start(WizardDraft::default());
        f.answer("1");
        f.answer("my-model");
        f.answer("sk-x");
        let r = f.probe_failed("HTTP 401");
        assert!(r.lines[0].contains("HTTP 401"));
        assert_eq!(f.answer("r").next, Next::Probe);
        f.probe_failed("again");
        let r = f.answer("e");
        assert_eq!(ask(&r), "Choice [1]: ", "edit starts over");
        f.answer("1");
        let r = f.answer("");
        assert_eq!(ask(&r), "api key [•••• saved, Enter to reuse]: ");
        assert!(!ask(&r).contains("sk-x"), "a saved key is never displayed");
        let r = f.answer("");
        assert_eq!(r.next, Next::Probe, "the key typed before is now the default");
        assert_eq!(f.answers().api_key, "sk-x");
        f.probe_failed("x");
        assert_eq!(f.answer("a").next, Next::Abort);
    }

    #[test]
    fn finish_writes_config_and_key_into_the_profile_folder() {
        let home = tempfile::tempdir().unwrap();
        let dir = crate::profile::prepare_dir_in(home.path(), "office").unwrap();
        let t = WizardTarget::for_profile(home.path(), "office");
        assert_eq!(t.cfg_path, dir.join("config.toml"));
        assert_eq!(t.key_path, dir.join("api_key"));
        let a = Answers {
            base_url: "https://gw.example/v1".into(),
            api_kind: "openai".into(),
            model: "office-model".into(),
            api_key: "sk-office".into(),
        };
        std::fs::write(&t.draft_path, "x").unwrap();
        finish(&t, &a).unwrap();
        assert!(!t.draft_path.exists(), "draft removed on success");
        assert_eq!(std::fs::read_to_string(&t.key_path).unwrap(), "sk-office");
        let cfg = crate::config::load_config_from(Some(&t.cfg_path), home.path()).unwrap();
        assert_eq!(cfg.model.as_deref(), Some("office-model"));
        let ep = crate::profile::resolve_endpoint(&cfg, |_| None).unwrap();
        assert_eq!(ep.api_key, "sk-office", "the profile's own key file is used");
        assert_eq!(crate::profile::list_in(home.path()), vec!["default", "office"]);
        // default keeps its historical locations.
        let d = WizardTarget::for_profile(home.path(), crate::profile::DEFAULT);
        assert_eq!(d.cfg_path, home.path().join("config.toml"));
        assert_eq!(d.key_path, home.path().join("api_key"));
    }

    #[test]
    fn clean_input_strips_one_pair_of_quotes() {
        assert_eq!(clean_input("  \"sk-1\" \n"), "sk-1");
        assert_eq!(clean_input("'a'"), "a");
        assert_eq!(clean_input("\"a"), "\"a");
    }

    #[test]
    fn presets_has_six_entries_with_custom_last() {
        assert_eq!(PROVIDER_PRESETS.len(), 6);
        assert!(PROVIDER_PRESETS.last().unwrap().is_custom);
        // Non-Custom entries have non-empty fields and valid api_kind.
        for p in &PROVIDER_PRESETS[..5] {
            assert!(!p.is_custom, "{} should not be custom", p.label);
            assert!(!p.base_url.is_empty(), "{} has empty base_url", p.label);
            assert!(!p.default_model.is_empty(), "{} has empty model", p.label);
            assert!(
                p.api_kind == "openai" || p.api_kind == "anthropic",
                "{} has bad api_kind {}",
                p.label,
                p.api_kind
            );
        }
    }

    #[test]
    fn preset_order_matches_spec() {
        let labels: Vec<&str> = PROVIDER_PRESETS.iter().map(|p| p.label).collect();
        assert_eq!(labels[0], "OpenAI");
        assert_eq!(labels[1], "DeepSeek");
        assert_eq!(labels[2], "Anthropic direct");
        assert_eq!(labels[3], "Gemini via gateway");
        assert!(labels[4].starts_with("Ollama"));
        assert_eq!(labels[5], "Custom");
    }

    #[test]
    fn is_localhost_recognizes_local_forms() {
        assert!(is_localhost("http://localhost:11434/v1"));
        assert!(is_localhost("http://127.0.0.1:8080"));
        assert!(is_localhost("http://0.0.0.0:1234/v1"));
        assert!(!is_localhost("https://api.openai.com/v1"));
        assert!(!is_localhost("https://api.deepseek.com/v1"));
    }

    #[cfg(unix)]
    #[test]
    fn write_key_file_sets_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let mut path = std::env::temp_dir();
        path.push(format!("nanopi-wizard-key-{}", crate::util::uuid::v7()));
        write_key_file(&path, "sk-xxxx").unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0600, got {:o}", mode);
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "sk-xxxx");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_config_toml_round_trips_through_loader() {
        let mut path = std::env::temp_dir();
        path.push(format!("nanopi-wizard-cfg-{}.toml", crate::util::uuid::v7()));
        write_config_toml(
            &path,
            "gpt-4o-mini",
            "https://api.openai.com/v1",
            "openai",
            Some("/home/u/.nanopi/api_key"),
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# generated by `nanopi init`"));
        let cfg: crate::config::Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.model.as_deref(), Some("gpt-4o-mini"));
        assert_eq!(cfg.base_url.as_deref(), Some("https://api.openai.com/v1"));
        assert_eq!(cfg.api_kind.as_deref(), Some("openai"));
        assert_eq!(
            cfg.api_key_file.as_deref(),
            Some(std::path::Path::new("/home/u/.nanopi/api_key"))
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_config_toml_round_trips_a_windows_path() {
        // Regression: the wizard used to emit the literal
        // "~/.nanopi/api_key", which the reader couldn't expand on
        // Windows (no $HOME) — every first run died with os error 3.
        // Now it emits the absolute path, so backslashes must survive
        // TOML escaping intact.
        let mut path = std::env::temp_dir();
        path.push(format!(
            "nanopi-wizard-cfg-win-{}.toml",
            crate::util::uuid::v7()
        ));
        let key = r"C:\Users\dell\.nanopi\api_key";
        write_config_toml(
            &path,
            "gpt-4o-mini",
            "https://api.openai.com/v1",
            "openai",
            Some(key),
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(r"C:\\Users\\dell"), "unescaped: {text}");
        let cfg: crate::config::Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.api_key_file.as_deref(), Some(std::path::Path::new(key)));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_config_toml_localhost_still_emits_api_key_file() {
        // Regression: wizard used to omit api_key_file for localhost,
        // which broke the next `nanopi` launch with "no api_key" error.
        // Now it always points at the (possibly empty) key file.
        let mut path = std::env::temp_dir();
        path.push(format!(
            "nanopi-wizard-cfg-ollama-{}.toml",
            crate::util::uuid::v7()
        ));
        write_config_toml(
            &path,
            "llama3.2",
            "http://localhost:11434/v1",
            "openai",
            Some("/home/u/.nanopi/api_key"),
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("api_key_file"), "got: {text}");
        let cfg: crate::config::Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.model.as_deref(), Some("llama3.2"));
        assert_eq!(
            cfg.api_key_file.as_deref(),
            Some(std::path::Path::new("/home/u/.nanopi/api_key"))
        );
        let _ = std::fs::remove_file(&path);
    }
}
