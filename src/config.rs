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

use serde::de::Error as _;
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
    /// Setting this explicitly is final: it overrides the vendor sniff,
    /// so `api_kind = "anthropic"` with a `/anthropic` base_url is
    /// honored even for a vendor whose primary transport is OpenAI.
    /// Leaving it unset lets the vendor choose from the base_url, which
    /// is the right default for dual-protocol gateways.
    ///
    /// Unrecognized values warn on stderr and are treated as unset. The
    /// `--api-kind` CLI flag overrides this per-invocation.
    pub api_kind: Option<String>,

    /// v0.9.3: explicit vendor id (e.g. "deepseek", "anthropic",
    /// "zai"). Overrides base_url/model sniff in
    /// `vendor::pick_vendor`. Unknown values fall through to sniff.
    #[serde(default)]
    pub provider: Option<String>,

    #[serde(default)]
    pub trust: TrustConfig,

    /// v0.6+: hooks live in the same config.toml so users have one file
    /// to edit. Same shape as `~/.nanopi/settings.toml` (which is still
    /// loaded for backward compatibility — see `settings::load_settings`).
    #[serde(default)]
    pub hooks: HooksSection,

    /// v0.9: skill configuration. `disabled` hides named skills after
    /// discovery. Mirrors the intent of PI's package-manager
    /// enable/disable toggles.
    #[serde(default)]
    pub skills: SkillsConfig,

    /// v0.11.0: WASM plugin extensions. Each entry points at a `.wasm`
    /// file (or a directory of `.wasm` files); the wasmtime runtime
    /// loads them and any `register-tool` / `register-command` calls
    /// they make during init() flow into the agent's registry.
    ///
    /// Loading happens on `Agent::build_fresh` / `hydrate_resumed` —
    /// the binary stays `~4 MB` when no entries are present (wasmtime
    /// is feature-gated).
    #[serde(default)]
    pub extensions: Vec<ExtensionConfig>,

    /// v0.11.0: tool execution mode within a single turn.
    ///
    /// `"parallel"` (default) — all tool calls from one LLM response
    /// run concurrently via `tokio::join_all`. Matches Pi's default.
    ///
    /// `"sequential"` — tool calls run one at a time, in the order
    /// the LLM emitted them. Some scripts or stateful tools require
    /// this (e.g. `write` then `read` where the read must see the
    /// write's output).
    ///
    /// The `[[extensions]]` tool's `executionMode` (Pi's per-tool
    /// override) is reserved for a future release; today this field
    /// applies globally.
    #[serde(default)]
    pub tool_exec_mode: ToolExecMode,
}

/// Global tool execution mode. Deserialized from
/// `tool_exec_mode = "parallel" | "sequential"` in config.toml.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecMode {
    Parallel,
    Sequential,
}

impl Default for ToolExecMode {
    fn default() -> Self {
        Self::Parallel
    }
}

/// v0.11.0: one WASM extension declaration.
///
/// ```toml
/// [[extensions]]
/// path = "~/.nanopi/extensions/query_tool.wasm"
/// ```
///
/// `path` may also point at a directory; every `*.wasm` inside (one
/// level) is loaded as an extension. When `allow_network` /
/// `allow_fs` are true, the plugin's `host-http-get` / `host-fs-read`
/// calls are forwarded to the host's actual network / filesystem;
/// otherwise those host functions return an in-band `error: `-prefixed
/// string. In-band rather than a trap so a plugin can handle a denied
/// capability as an ordinary failure instead of dying.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ExtensionConfig {
    /// `.wasm` file or directory of `.wasm` files. Supports `~/` and
    /// `$HOME/` expansion.
    pub path: PathBuf,
    /// If `path` is a directory, load at most this many files
    /// (default: 64). Cheap protection against a misconfigured glob.
    pub max_files: usize,
    /// Enable the `host-http-get` host function for this plugin.
    /// Default: `false` — most plugins can do their work without it.
    pub allow_network: bool,
    /// Enable the `host-fs-read` host function (read-only).
    /// Default: `false`.
    pub allow_fs: bool,
    /// Hosts `host-http-get` may reach. Empty denies every URL, so
    /// `allow_network = true` alone reaches nothing. Compared against
    /// the URL's parsed host, never a substring.
    ///
    /// Three spellings: `example.com` covers the host and its
    /// subdomains (and any port); `*.example.com` covers only the
    /// subdomains; `*` covers any `http`/`https` host, for plugins
    /// whose host set isn't knowable in advance. `*` is warned about
    /// at load — it leaves `allow_network` as the only gate. A star
    /// elsewhere (`api.*.com`) is refused, not widened.
    ///
    /// Effective only when `allow_network = true`.
    pub url_allowlist: Vec<String>,

    /// v0.12.0: lifecycle events this plugin is GRANTED, as PI's names
    /// (`tool_execution_start`, not `pre_tool_use` — see
    /// `docs/v0.12-events.md` §2.1). Absent or empty grants nothing.
    ///
    /// Delivery requires the event to be in BOTH this list and the
    /// plugin's own `list-events` — the config grants, the plugin only
    /// asks (§4.2). Validation of the names themselves happens at plugin
    /// load, not here (see `agent::hook::parse_event_grants`), so an
    /// unknown or retired name in this list is never a config-load error
    /// in the stock (non-wasm) binary.
    ///
    /// This grant is a LARGER capability jump than `allow_fs` or
    /// `allow_network`: a `tool_execution_start` subscriber sees every
    /// tool call's arguments, and an `input` subscriber sees every
    /// prompt the user types — previously a plugin saw only what the
    /// model chose to hand it as tool arguments. Combined with
    /// `allow_network = true` this is an exfiltration channel, which is
    /// why that combination warns at plugin load (§5.1).
    pub events: Vec<String>,
}

impl Default for ExtensionConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::new(),
            max_files: 64,
            allow_network: false,
            allow_fs: false,
            url_allowlist: Vec::new(),
            events: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SkillsConfig {
    /// Names to hide even if discovered on disk.
    pub disabled: Vec<String>,
    /// Extra directories to scan for skills, in addition to
    /// `~/.nanopi/skills` and `<cwd>/.nanopi/skills`. Reserved for a
    /// future release — currently unused by the loader (callers pass
    /// dirs directly through `LoadSkillsOptions`).
    pub extra_dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TrustConfig {
    /// One of: `ask`, `always`, `never`. Default `ask`.
    pub default: Option<String>,
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
            provider: None,
            trust: TrustConfig::default(),
            hooks: HooksSection::default(),
            skills: SkillsConfig::default(),
            extensions: Vec::new(),
            tool_exec_mode: ToolExecMode::default(),
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
    crate::paths::global_config_path()
}

fn load_one(path: &Path) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    toml::from_str::<Config>(&text).map_err(|e| {
        // A retired [[hooks.*]] key (§2.3) surfaces here as a plain
        // "unknown field" toml error. Rewrite it to name the
        // replacement key, keeping the file path so the user knows
        // which file to edit; any other parse error passes through
        // verbatim.
        if let Some(msg) = crate::agent::hook::retired_hook_key_error(&e.to_string()) {
            return ConfigError::Toml {
                path: path.to_path_buf(),
                source: toml::de::Error::custom(msg),
            };
        }
        ConfigError::Toml {
            path: path.to_path_buf(),
            source: e,
        }
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
        tool_execution_start: a.hooks.tool_execution_start,
        tool_execution_end: a.hooks.tool_execution_end,
        input: a.hooks.input,
        session_start: a.hooks.session_start,
        session_shutdown: a.hooks.session_shutdown,
        before_agent_start: a.hooks.before_agent_start,
        turn_start: a.hooks.turn_start,
        turn_end: a.hooks.turn_end,
        message_end: a.hooks.message_end,
        session_before_compact: a.hooks.session_before_compact,
        session_compact: a.hooks.session_compact,
    };
    hooks.tool_execution_start.extend(b.hooks.tool_execution_start);
    hooks.tool_execution_end.extend(b.hooks.tool_execution_end);
    hooks.input.extend(b.hooks.input);
    hooks.session_start.extend(b.hooks.session_start);
    hooks.session_shutdown.extend(b.hooks.session_shutdown);
    hooks.before_agent_start.extend(b.hooks.before_agent_start);
    hooks.turn_start.extend(b.hooks.turn_start);
    hooks.turn_end.extend(b.hooks.turn_end);
    hooks.message_end.extend(b.hooks.message_end);
    hooks
        .session_before_compact
        .extend(b.hooks.session_before_compact);
    hooks.session_compact.extend(b.hooks.session_compact);
    // skills: disabled + extra_dirs concatenate (both sides additive).
    let mut skills = SkillsConfig {
        disabled: a.skills.disabled,
        extra_dirs: a.skills.extra_dirs,
    };
    skills.disabled.extend(b.skills.disabled);
    skills.extra_dirs.extend(b.skills.extra_dirs);
    Config {
        model: b.model.or(a.model),
        base_url: b.base_url.or(a.base_url),
        api_key_file: b.api_key_file.or(a.api_key_file),
        api_key: b.api_key.or(a.api_key),
        api_kind: b.api_kind.or(a.api_kind),
        provider: b.provider.or(a.provider),
        trust: TrustConfig {
            default: b.trust.default.or(a.trust.default),
        },
        hooks,
        skills,
        // Extensions concatenate (both additive, never override).
        extensions: {
            let mut ext = a.extensions;
            ext.extend(b.extensions);
            ext
        },
        // tool_exec_mode: b wins if explicitly set; default otherwise.
        // Default::default() == Parallel; users opt into Sequential.
        // tool_exec_mode: last explicit (non-Parallel) wins; else default.
        tool_exec_mode: if b.tool_exec_mode != ToolExecMode::default() {
            b.tool_exec_mode
        } else {
            a.tool_exec_mode
        },
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
        assert_eq!(c.base_url.as_deref(), Some("https://project.example/v1"));
    }

    #[test]
    fn invalid_toml_is_error() {
        let _g = lock();
        let _h = HomeGuard::new();
        let tmp = TempDir::new();
        tmp.write(".nanopi/config.toml", "this is not valid toml = === =");
        let r = load_config(tmp.path());
        assert!(matches!(r, Err(ConfigError::Toml { .. })));
    }

    /// v0.12.0 §2.3: a retired hook key is a hard load error naming both
    /// the retired key and its replacement.
    #[test]
    fn retired_hook_key_is_a_hard_error_naming_the_replacement() {
        let _g = lock();
        let _h = HomeGuard::new();
        let tmp = TempDir::new();
        tmp.write(
            ".nanopi/config.toml",
            r#"
[[hooks.pre_tool_use]]
matcher = "*"
command = "echo hi"
"#,
        );
        let r = load_config(tmp.path());
        let err = match r {
            Err(ConfigError::Toml { source, .. }) => source.to_string(),
            other => panic!("expected a Toml error, got {other:?}"),
        };
        assert!(err.contains("pre_tool_use"), "error should name the retired key: {err}");
        assert!(
            err.contains("tool_execution_start"),
            "error should name the replacement: {err}"
        );
    }

    /// A merely-misspelled hook key (never a shipped name) is still a
    /// hard error — it just isn't rewritten by the retired-key table.
    #[test]
    fn misspelled_hook_key_is_still_an_error() {
        let _g = lock();
        let _h = HomeGuard::new();
        let tmp = TempDir::new();
        tmp.write(
            ".nanopi/config.toml",
            r#"
[[hooks.turn_startt]]
matcher = "*"
command = "echo hi"
"#,
        );
        let r = load_config(tmp.path());
        assert!(matches!(r, Err(ConfigError::Toml { .. })));
    }

    #[test]
    fn config_loads_provider_field() {
        let text = "provider = \"deepseek\"\n";
        let c: Config = toml::from_str(text).unwrap();
        assert_eq!(c.provider.as_deref(), Some("deepseek"));
    }

    #[test]
    fn merge_preserves_unset_fields() {
        let a = Config {
            model: Some("a-model".into()),
            base_url: None,
            api_key_file: Some("/tmp/key".into()),
            api_key: None,
            api_kind: None,
            provider: None,
            trust: TrustConfig::default(),
            hooks: HooksSection::default(),
            skills: SkillsConfig::default(),
            extensions: Vec::new(),
            tool_exec_mode: ToolExecMode::default(),
        };
        let b = Config {
            model: None,
            base_url: Some("https://b".into()),
            api_key_file: None,
            api_key: None,
            api_kind: None,
            provider: None,
            trust: TrustConfig::default(),
            hooks: HooksSection::default(),
            skills: SkillsConfig::default(),
            extensions: Vec::new(),
            tool_exec_mode: ToolExecMode::default(),
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

[[hooks.tool_execution_start]]
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
        assert_eq!(c.hooks.tool_execution_start.len(), 1);
        assert_eq!(c.hooks.tool_execution_start[0].matcher, "bash");
        assert_eq!(c.hooks.session_start.len(), 1);
    }

    #[test]
    fn merge_concatenates_hooks() {
        use crate::agent::hook::HookConfig;
        let a = Config {
            hooks: HooksSection {
                tool_execution_start: vec![HookConfig {
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
                tool_execution_start: vec![HookConfig {
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
        assert_eq!(m.hooks.tool_execution_start.len(), 2);
        assert_eq!(m.hooks.tool_execution_start[0].matcher, "a");
        assert_eq!(m.hooks.tool_execution_start[1].matcher, "b");
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
