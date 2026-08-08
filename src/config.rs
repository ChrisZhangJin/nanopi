//! TOML config loader.
//!
//! Three layers, in priority order (highest wins):
//!   1. CLI flags (handled by clap, not this module)
//!   2. Project-local: `<cwd>/.nanopi/config.toml`
//!   3. Global: `~/.nanopi/config.toml`
//!
//! Missing files → defaults. Invalid TOML → error with file path.
//! All fields optional; serde defaults fill them in.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

use crate::settings::HooksSection;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse TOML in {path}: {source}")]
    Toml {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Model id (used when --model is absent and OPENAI_MODEL is unset).
    pub model: Option<String>,

    /// Base URL (OpenAI-compatible provider root).
    pub base_url: Option<String>,

    /// Optional path to API key file (chmod 600). Recommended over
    /// inline `api_key` for anything committed to VCS.
    pub api_key_file: Option<PathBuf>,

    /// Inline api_key. Convenient for local dev; a stderr warning fires
    /// at load time so the user notices before committing.
    pub api_key: Option<String>,

    /// Which wire protocol to speak to `base_url`. Valid values are
    /// `"openai"` (default — most gateways: oneapi, newapi, litellm,
    /// OpenAI itself, DeepSeek, Groq via OpenAI-compat) and
    /// `"anthropic"` (talk to `/v1/messages` in Anthropic's native
    /// format — Anthropic direct, or a proxy that exposes the
    /// Anthropic API).
    ///
    /// Unrecognized values fall back to `"openai"`. The `--api-kind`
    /// CLI flag overrides this per-invocation.
    pub api_kind: Option<String>,

    /// Tool whitelist. Empty/absent = all standard tools.
    #[serde(default)]
    pub tools: Vec<String>,

    #[serde(default)]
    pub trust: TrustConfig,

    #[serde(default)]
    pub logging: LoggingConfig,

    /// v0.6+: hooks live in the same config.toml so users have one file
    /// to edit. Same shape as `~/.nanopi/settings.toml` (which is still
    /// loaded for backward compatibility — see `settings::load_settings`).
    #[serde(default)]
    pub hooks: HooksSection,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TrustConfig {
    /// One of: `ask`, `always`, `never`. Default `ask`.
    pub default: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    pub level: Option<String>,
    pub file: Option<PathBuf>,
}

impl Config {
    /// Built-in defaults when no config files exist.
    pub fn builtin_defaults() -> Self {
        Self {
            model: None,
            base_url: None,
            api_key_file: None,
            api_key: None,
            api_kind: None,
            tools: Vec::new(),
            trust: TrustConfig::default(),
            logging: LoggingConfig::default(),
            hooks: HooksSection::default(),
        }
    }
}

/// Load and merge configs from global + project-local. Missing files are
/// silently ignored (use defaults). Malformed TOML is a hard error.
pub fn load_config(cwd: &Path) -> Result<Config, ConfigError> {
    let global_path = global_config_path();
    let local_path = cwd.join(".nanopi").join("config.toml");

    let mut merged = Config::builtin_defaults();

    if let Some(path) = global_path {
        if path.exists() {
            merged = merge(merged, load_one(&path)?);
        }
    }

    if local_path.exists() {
        merged = merge(merged, load_one(&local_path)?);
    }

    Ok(merged)
}

/// Path to the global config file. Honors NANOPI_HOME for test
/// isolation; otherwise falls back to `$HOME/.nanopi/config.toml`.
pub fn global_config_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("NANOPI_HOME") {
        return Some(PathBuf::from(p).join("config.toml"));
    }
    let home = dirs::home_dir()?;
    Some(home.join(".nanopi").join("config.toml"))
}

fn load_one(path: &Path) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    toml::from_str::<Config>(&text).map_err(|e| ConfigError::Toml {
        path: path.to_path_buf(),
        source: e,
    })
}

/// Deep-merge two Configs. `b` wins on field-level conflicts.
///
/// Both sides already have serde defaults applied, so a field present
/// only in `a` keeps its value. `Option<String>` semantics: `None` means
/// "unset" (don't overwrite `b`'s value), `Some` means "use this".
/// Hook vectors are concatenated (a first, then b) — hooks are policies,
/// not overrides.
fn merge(a: Config, b: Config) -> Config {
    let mut hooks = HooksSection {
        pre_tool_use: a.hooks.pre_tool_use,
        post_tool_use: a.hooks.post_tool_use,
        user_prompt_submit: a.hooks.user_prompt_submit,
        session_start: a.hooks.session_start,
        session_end: a.hooks.session_end,
    };
    hooks.pre_tool_use.extend(b.hooks.pre_tool_use);
    hooks.post_tool_use.extend(b.hooks.post_tool_use);
    hooks.user_prompt_submit.extend(b.hooks.user_prompt_submit);
    hooks.session_start.extend(b.hooks.session_start);
    hooks.session_end.extend(b.hooks.session_end);
    // tools whitelist: `b` (project) wins if it declares any; else keep `a`.
    let tools = if b.tools.is_empty() { a.tools } else { b.tools };
    Config {
        model: b.model.or(a.model),
        base_url: b.base_url.or(a.base_url),
        api_key_file: b.api_key_file.or(a.api_key_file),
        api_key: b.api_key.or(a.api_key),
        api_kind: b.api_kind.or(a.api_kind),
        tools,
        trust: TrustConfig {
            default: b.trust.default.or(a.trust.default),
        },
        logging: LoggingConfig {
            level: b.logging.level.or(a.logging.level),
            file: b.logging.file.or(a.logging.file),
        },
        hooks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::Write;

    /// Build a fresh temp dir; auto-cleaned on drop.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let mut p = std::env::temp_dir();
            p.push(format!("nanopi-test-{}", uuid_v7()));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
        fn write(&self, name: &str, content: &str) -> PathBuf {
            let p = self.0.join(name);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            let mut f = std::fs::File::create(&p).unwrap();
            f.write_all(content.as_bytes()).unwrap();
            p
        }
        fn _unused(&self) -> HashMap<(), ()> {
            HashMap::new()
        }
    }

    fn uuid_v7() -> String {
        crate::util::uuid::v7().to_string()
    }

    /// Test guard: point NANOPI_HOME at an empty temp dir so tests
    /// don't pick up the real ~/.nanopi/config.toml. Restore on drop.
    struct HomeGuard {
        prev: Option<std::ffi::OsString>,
        _dir: TempDir,
    }
    impl HomeGuard {
        fn new() -> Self {
            let dir = TempDir::new();
            let prev = std::env::var_os("NANOPI_HOME");
            std::env::set_var("NANOPI_HOME", dir.path());
            Self { prev, _dir: dir }
        }
    }
    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(p) => std::env::set_var("NANOPI_HOME", p),
                None => std::env::remove_var("NANOPI_HOME"),
            }
        }
    }

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        crate::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn builtin_defaults() {
        let c = Config::builtin_defaults();
        assert_eq!(c.model, None);
        assert_eq!(c.base_url, None);
        assert!(c.tools.is_empty());
    }

    #[test]
    fn missing_files_use_defaults() {
        let _g = lock();
        let _h = HomeGuard::new();
        let tmp = TempDir::new();
        let c = load_config(tmp.path()).unwrap();
        assert_eq!(c.model, None);
    }

    #[test]
    fn project_overrides_global() {
        let _g = lock();
        let _h = HomeGuard::new();
        let tmp = TempDir::new();
        tmp.write(
            ".nanopi/config.toml",
            r#"
model = "project-model"
base_url = "https://project.example/v1"
"#,
        );
        let c = load_config(tmp.path()).unwrap();
        assert_eq!(c.model.as_deref(), Some("project-model"));
        assert_eq!(
            c.base_url.as_deref(),
            Some("https://project.example/v1")
        );
    }

    #[test]
    fn invalid_toml_is_error() {
        let _g = lock();
        let _h = HomeGuard::new();
        let tmp = TempDir::new();
        tmp.write(
            ".nanopi/config.toml",
            "this is not valid toml = === =",
        );
        let r = load_config(tmp.path());
        assert!(matches!(r, Err(ConfigError::Toml { .. })));
    }

    #[test]
    fn merge_preserves_unset_fields() {
        let a = Config {
            model: Some("a-model".into()),
            base_url: None,
            api_key_file: Some("/tmp/key".into()),
            api_key: None,
            api_kind: None,
            tools: Vec::new(),
            trust: TrustConfig::default(),
            logging: LoggingConfig::default(),
            hooks: HooksSection::default(),
        };
        let b = Config {
            model: None,
            base_url: Some("https://b".into()),
            api_key_file: None,
            api_key: None,
            api_kind: None,
            tools: Vec::new(),
            trust: TrustConfig::default(),
            logging: LoggingConfig::default(),
            hooks: HooksSection::default(),
        };
        let m = merge(a, b);
        assert_eq!(m.model.as_deref(), Some("a-model"));
        assert_eq!(m.base_url.as_deref(), Some("https://b"));
        assert_eq!(m.api_key_file.as_deref(), Some(Path::new("/tmp/key")));
    }

    #[test]
    fn config_loads_inline_api_key_and_hooks() {
        let _g = lock();
        let _h = HomeGuard::new();
        let tmp = TempDir::new();
        tmp.write(
            ".nanopi/config.toml",
            r#"
model = "cfg-model"
base_url = "https://cfg.example/v1"
api_key = "sk-inline-secret"
tools = ["read", "write", "grep"]

[[hooks.pre_tool_use]]
matcher = "bash"
type = "command"
command = "/bin/true"
timeout = 5000

[[hooks.session_start]]
matcher = "*"
type = "command"
command = "/bin/true"
"#,
        );
        let c = load_config(tmp.path()).unwrap();
        assert_eq!(c.model.as_deref(), Some("cfg-model"));
        assert_eq!(c.api_key.as_deref(), Some("sk-inline-secret"));
        assert_eq!(c.tools, vec!["read", "write", "grep"]);
        assert_eq!(c.hooks.pre_tool_use.len(), 1);
        assert_eq!(c.hooks.pre_tool_use[0].matcher, "bash");
        assert_eq!(c.hooks.session_start.len(), 1);
    }

    #[test]
    fn merge_concatenates_hooks() {
        use crate::agent::hook::HookConfig;
        let a = Config {
            hooks: HooksSection {
                pre_tool_use: vec![HookConfig {
                    matcher: "a".into(),
                    kind: "command".into(),
                    command: "x".into(),
                    timeout: 1000,
                }],
                ..HooksSection::default()
            },
            ..Config::builtin_defaults()
        };
        let b = Config {
            hooks: HooksSection {
                pre_tool_use: vec![HookConfig {
                    matcher: "b".into(),
                    kind: "command".into(),
                    command: "y".into(),
                    timeout: 1000,
                }],
                ..HooksSection::default()
            },
            ..Config::builtin_defaults()
        };
        let m = merge(a, b);
        assert_eq!(m.hooks.pre_tool_use.len(), 2);
        assert_eq!(m.hooks.pre_tool_use[0].matcher, "a");
        assert_eq!(m.hooks.pre_tool_use[1].matcher, "b");
    }

    #[test]
    fn merge_lets_b_override_a() {
        let a = Config {
            model: Some("a-model".into()),
            ..Config::builtin_defaults()
        };
        let b = Config {
            model: Some("b-model".into()),
            ..Config::builtin_defaults()
        };
        let m = merge(a, b);
        assert_eq!(m.model.as_deref(), Some("b-model"));
    }
}