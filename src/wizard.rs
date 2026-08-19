//! First-run wizard for nanopi.
//!
//! Bootstraps `~/.nanopi/config.toml` when a user runs nanopi with no
//! config, no env vars, and no CLI creds. Also reachable explicitly via
//! `nanopi init`.
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
use std::path::{Path, PathBuf};

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
    let client = reqwest::Client::new();
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
            let snippet: String = body.chars().take(240).collect();
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
/// `api_key_file` is `Some("~/.nanopi/api_key")` for normal providers,
/// `None` for localhost (no key needed, no file to reference).
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
    let s = buf.trim();
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
    Ok(s.to_string())
}

/// Prompt with an optional default. Empty input returns the default.
/// `label` is the bare field name (e.g. "base_url"); we append the
/// bracketed default and the ": " ourselves.
fn read_line_default(label: &str, default: Option<&str>) -> anyhow::Result<String> {
    let p = match default {
        Some(d) if !d.is_empty() => format!("{label} [{d}]: "),
        _ => format!("{label}: "),
    };
    let v = read_line(&p)?;
    if v.is_empty() {
        Ok(default.unwrap_or("").to_string())
    } else {
        Ok(v)
    }
}

/// A partial wizard attempt, persisted between runs so the user can
/// resume after a probe failure or Ctrl-C without re-typing long URLs
/// and API keys.
#[derive(Debug, Default)]
struct WizardDraft {
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

/// The interactive entry point.
///
/// `force_overwrite_prompt=true`: coming from `nanopi init`; if
/// `~/.nanopi/config.toml` already exists we prompt for confirmation
/// before clobbering it.
///
/// `force_overwrite_prompt=false`: coming from the no-config
/// fallthrough in `main.rs`; there's no config to protect.
pub async fn run_wizard(force_overwrite_prompt: bool) -> anyhow::Result<()> {
    let home = paths::nanopi_home()
        .ok_or_else(|| anyhow!("cannot resolve ~/.nanopi (HOME unset and NANOPI_HOME unset)"))?;
    let cfg_path = home.join("config.toml");
    let key_path = home.join("api_key");
    let draft_path = home.join("wizard_draft.toml");
    let draft = load_draft(&draft_path);

    if force_overwrite_prompt && cfg_path.exists() {
        let ans = read_line(&format!(
            "{} already exists. Overwrite? [y/N]: ",
            cfg_path.display()
        ))?;
        let a = ans.to_ascii_lowercase();
        if a != "y" && a != "yes" {
            println!("aborted; existing config left untouched.");
            return Ok(());
        }
    }

    'outer: loop {
        // Provider selection.
        println!();
        println!("Select a provider:");
        for (i, p) in PROVIDER_PRESETS.iter().enumerate() {
            println!("  {}) {}", i + 1, p.label);
        }
        let preset = loop {
            let sel = read_line("Choice [1]: ")?;
            let sel = if sel.is_empty() { "1".to_string() } else { sel };
            match sel.parse::<usize>() {
                Ok(n) if n >= 1 && n <= PROVIDER_PRESETS.len() => break &PROVIDER_PRESETS[n - 1],
                _ => println!("invalid selection, try again."),
            }
        };

        // Fill (base_url, api_kind, model) — for Custom, prompt each.
        let (base_url, api_kind, model) = if preset.is_custom {
            let base_url = loop {
                let v = read_line_default("base_url", draft.base_url.as_deref())?;
                if !v.is_empty() {
                    break v;
                }
                println!("base_url is required.");
            };
            let api_kind = loop {
                let default = draft.api_kind.as_deref().unwrap_or("openai");
                let v = read_line_default(
                    "api_kind [openai/anthropic]",
                    Some(default),
                )?;
                if v == "openai" || v == "anthropic" {
                    break v;
                }
                println!("api_kind must be `openai` or `anthropic`.");
            };
            let model = loop {
                let v = read_line_default("model", draft.model.as_deref())?;
                if !v.is_empty() {
                    break v;
                }
                println!("model is required.");
            };
            (base_url, api_kind, model)
        } else {
            let model_prompt = format!("model [{}]: ", preset.default_model);
            let m = read_line(&model_prompt)?;
            let model = if m.is_empty() {
                preset.default_model.to_string()
            } else {
                m
            };
            (
                preset.base_url.to_string(),
                preset.api_kind.to_string(),
                model,
            )
        };

        // API key — skipped for localhost.
        let localhost = is_localhost(&base_url);
        // v1: single-line stdin, no echo-hide. Matches trust.rs simplicity.
        // Users on shared terminals can set OPENAI_API_KEY out-of-band instead.
        let api_key = if localhost {
            String::new()
        } else {
            loop {
                let v = read_line_default("api key", draft.api_key.as_deref())?;
                if !v.is_empty() {
                    break v;
                }
                println!("api key is required (or pick Ollama/localhost to skip).");
            }
        };

        // Save draft before probing so a Ctrl-C or aborted retry loop
        // leaves the filled fields on disk for the next run.
        let _ = save_draft(&draft_path, &WizardDraft {
            base_url: Some(base_url.clone()),
            api_kind: Some(api_kind.clone()),
            model: Some(model.clone()),
            api_key: if api_key.is_empty() { None } else { Some(api_key.clone()) },
        });

        // Probe.
        'probe: loop {
            println!("probing {}...", base_url);
            match probe_config(&base_url, &api_kind, &model, &api_key).await {
                Ok(()) => {
                    println!("probe OK.");
                    break 'probe;
                }
                Err(e) => {
                    println!("probe failed: {e}");
                    let ans = read_line("retry / edit / abort? [r/e/a]: ")?;
                    match ans.to_ascii_lowercase().as_str() {
                        "r" | "retry" => continue 'probe,
                        "e" | "edit" => continue 'outer,
                        "a" | "abort" | "" => {
                            println!("aborted; no config written.");
                            return Ok(());
                        }
                        _ => {
                            println!("unrecognized choice; aborting.");
                            return Ok(());
                        }
                    }
                }
            }
        }

        // Write key file (empty for localhost) then config.toml.
        // We always emit api_key_file so subsequent launches load
        // cleanly — the config loader accepts an empty key file for
        // no-auth localhost endpoints.
        write_key_file(&key_path, &api_key)?;
        write_config_toml(&cfg_path, &model, &base_url, &api_kind, Some("~/.nanopi/api_key"))?;
        let _ = std::fs::remove_file(&draft_path);

        println!();
        println!("wrote {}", cfg_path.display());
        println!("wrote {} (mode 0600)", key_path.display());
        return Ok(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            Some("~/.nanopi/api_key"),
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
            Some(std::path::Path::new("~/.nanopi/api_key"))
        );
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
            Some("~/.nanopi/api_key"),
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("api_key_file"), "got: {text}");
        let cfg: crate::config::Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.model.as_deref(), Some("llama3.2"));
        assert_eq!(
            cfg.api_key_file.as_deref(),
            Some(std::path::Path::new("~/.nanopi/api_key"))
        );
        let _ = std::fs::remove_file(&path);
    }
}

#[allow(dead_code)]
fn _keep_path_type_used() -> Option<PathBuf> {
    None
}
