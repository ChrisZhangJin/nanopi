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

    /// Escape hatch for the inline `<think>…</think>` splitter (see
    /// `provider::think_tags` for the position rule it applies). `None`
    /// (default) leaves it on — every OpenAI-wire vendor gets a LEADING
    /// `<think>` block reclassified as reasoning. `Some(true)` is a
    /// no-op. `Some(false)` forces it off entirely, so even a leading
    /// `<think>` renders as plain text.
    #[serde(default)]
    pub inline_think_tags: Option<bool>,

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
    /// This field is the GLOBAL setting. Per-tool overrides live in
    /// `tool_exec_overrides` below, and a tool's own declaration
    /// (`Tool::execution_mode`) sits between the two: global mode
    /// picks the batching strategy, a `Sequential` tool in the batch
    /// forces serial regardless, and an override outranks the tool.
    #[serde(default)]
    pub tool_exec_mode: ToolExecMode,
    /// v0.12: per-tool overrides of the tool's own declared
    /// `executionMode`.
    ///
    /// ```toml
    /// [tool_exec_overrides]
    /// bash = "parallel"    # I know what my commands touch
    /// read = "sequential"  # belt and braces
    /// ```
    ///
    /// Outranks `Tool::execution_mode` in both directions. A key naming
    /// a tool that does not exist is a load-time error, not a silent
    /// no-op — the same rule retired hook keys and `allow_tools`
    /// entries follow, and for the same reason: a setting that appears
    /// to do something and does nothing is worse than one that is
    /// refused.
    #[serde(default)]
    pub tool_exec_overrides: std::collections::BTreeMap<String, crate::tool::ExecutionMode>,
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
/// `deny_unknown_fields` is what turns a typo'd grant
/// (`allow_stroe = true`, `allow_contxt = true`) into a parse error
/// instead of a silently-ignored no-op. The failure it prevents is
/// specific and bad: the user believes they granted a capability, the
/// plugin is refused every call, and nothing anywhere says why — the
/// config looks right because the misspelled key is simply not a key.
/// A grant is exactly the kind of setting that must not fail quietly.
///
/// This was a DELIBERATE DEFERRED DECISION from stage 1, not an
/// oversight: the stage-1 plan pinned the gap with a test whose doc
/// comment said flipping it had to be deliberate and visible. This is
/// that flip, and it supersedes that pin.
///
/// The cost is acknowledged rather than hidden: a config carrying a
/// stray key that loads today will now fail loudly, which is a
/// behaviour change for existing users. No `alias` softens it, same as
/// the retired hook keys in `settings.rs`. Every `[[extensions]]` key
/// appearing in `config.toml.example`, both READMEs,
/// `docs/v0.12-events.md`, `docs/v0.12-manual-test-plan.md` and
/// `docs/pi-vs-nanopi.md` was surveyed and is a real field. (Stage 2's
/// survey found exactly one undefined key anywhere, `allow_tools` in
/// `docs/plugin-capabilities.md` §2.5's stage-3 example; stage 3 made it
/// a real field, so the survey now has no exceptions.)
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
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

    /// v0.12: enable `host-store-get` / `host-store-set` — this
    /// plugin's own keyed store at
    /// `~/.nanopi/extensions/<stem>/store.json`. Default: `false`.
    ///
    /// Gated rather than free because state that survives a restart is
    /// a different capability from state in the instance's memory:
    /// combined with `allow_network = true` it is a durable profile of
    /// the user that can leave the machine, which is why that
    /// combination warns at plugin load
    /// (`docs/plugin-capabilities.md` §3).
    ///
    /// The plugin supplies KEYS, never paths — the host owns the
    /// mapping to a file — so this grants no filesystem reach that
    /// `allow_fs` would otherwise cover. Identity is the `.wasm` file
    /// stem, so two plugins with the same stem where either sets this
    /// are a load error rather than a silent shared store.
    pub allow_store: bool,

    /// v0.12: enable `host-set-context` — text this plugin declares is
    /// folded into the system prompt at turn assembly, under a header
    /// naming the plugin. Default: `false`.
    ///
    /// Gated because the plugin is writing text the MODEL READS AS
    /// INSTRUCTION. That is a different kind of capability from
    /// `allow_fs` or `allow_store`: those let a plugin learn things,
    /// this one lets it change what the agent believes. Combined with
    /// `allow_network = true` it lets a remote source shape the agent's
    /// behaviour, which is why that combination warns at plugin load
    /// (`docs/plugin-capabilities.md` §3).
    ///
    /// Bounded at 4 KiB per plugin, replace-not-append, and every
    /// change is announced in the user's scrollback — a contribution is
    /// invisible by nature, since the user never sees the system
    /// prompt.
    pub allow_context: bool,
    /// Built-in tools this plugin may invoke through `host-call-tool`
    /// (`docs/plugin-capabilities.md` §2.5). Empty — the default —
    /// denies every tool.
    ///
    /// PER TOOL, not per plugin, and that is the whole point. `bash` is
    /// arbitrary execution: it walks straight past `allow_fs`'s cwd
    /// confinement and past `url_allowlist`'s per-host approval, so a
    /// single `allow_tools = true` would make every other grant on the
    /// plugin decorative. `allow_tools = ["find", "read"]` expresses
    /// "may walk and read, may not write or exec", which no per-plugin
    /// flag can. Listing `bash` warns at plugin load.
    ///
    /// Empty denies everything rather than allowing everything, the
    /// same direction `url_allowlist` and `events` already take.
    ///
    /// A name here that is not a built-in (`bash`, `edit`, `find`,
    /// `grep`, `ls`, `read`, `write`) is a LOAD ERROR for that plugin,
    /// not a silent no-op — the same rule a retired hook key follows.
    /// A plugin's own tools are not addressable: `host-call-tool`
    /// reaches built-in tools only.
    pub allow_tools: Vec<String>,

    /// v0.12: enable `host-send-user-message` — this plugin may start or
    /// steer a turn with text of its own choosing
    /// (`docs/plugin-capabilities.md` §2.4). Default: `false`.
    ///
    /// **This is the only grant that spends the user's money.** Every
    /// other capability lets a plugin learn something, change what the
    /// agent believes, or run a tool the user could have run; this one
    /// makes the agent take a turn against a provider and bill for it.
    /// A plugin subscribed to `turn_start` that calls it without a guard
    /// is an unbounded loop with a price tag, which is why §2.4 mandates
    /// two loop-guard rules and why stage 4 added a third — a per-session
    /// cap on turns one plugin may cause
    /// (`plugin_send::MAX_PLUGIN_TURNS_PER_SESSION`).
    ///
    /// The text is ALWAYS echoed to the user verbatim, attributed to the
    /// plugin. There is no silent path: a message the host accepts is a
    /// message the user sees, and a message the host refuses is refused
    /// in band with a reason, never dropped (invariant 9).
    ///
    /// Off in headless (`nanopi -p`) regardless of this flag — the whole
    /// path is TUI-only, and the call is refused rather than no-op'd.
    pub allow_send_message: bool,

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
            allow_store: false,
            allow_context: false,
            allow_tools: Vec::new(),
            allow_send_message: false,
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
            inline_think_tags: None,
            trust: TrustConfig::default(),
            hooks: HooksSection::default(),
            skills: SkillsConfig::default(),
            extensions: Vec::new(),
            tool_exec_mode: ToolExecMode::default(),
            tool_exec_overrides: Default::default(),
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

    validate_tool_exec_overrides(&merged)?;

    Ok(merged)
}

/// A `[tool_exec_overrides]` key must name a real built-in tool.
///
/// Same rule as a retired hook key and an `allow_tools` entry naming a
/// tool that does not exist: refused at load, naming the valid set.
/// A misspelled `bahs = "sequential"` would otherwise parse clean and
/// change nothing, and the user would conclude that concurrent bash is
/// simply still broken.
///
/// Checked after the merge, not inside `load_one`, so a key set
/// globally and corrected locally is judged on the value that actually
/// takes effect.
///
/// Deliberately limited to BUILT-INS. Plugin tools are not known until
/// `[[extensions]]` load, which happens after this, and a plugin's own
/// `Tool::execution_mode` already governs it; accepting arbitrary names
/// here would trade this error for the silent no-op it exists to
/// prevent.
fn validate_tool_exec_overrides(cfg: &Config) -> Result<(), ConfigError> {
    let known = crate::tool::ToolRegistry::standard().names();
    let unknown: Vec<&str> = cfg
        .tool_exec_overrides
        .keys()
        .map(String::as_str)
        .filter(|k| !known.iter().any(|n| n == k))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    let path = global_config_path().unwrap_or_else(|| PathBuf::from("config.toml"));
    Err(ConfigError::Toml {
        path,
        source: toml::de::Error::custom(format!(
            "[tool_exec_overrides] names {}, which {} not a built-in tool. \
             Valid names: {}. (Plugin tools are governed by their own \
             executionMode and cannot be overridden here.)",
            unknown
                .iter()
                .map(|u| format!("`{u}`"))
                .collect::<Vec<_>>()
                .join(", "),
            if unknown.len() == 1 { "is" } else { "are" },
            known.join(", ")
        )),
    })
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
        inline_think_tags: b.inline_think_tags.or(a.inline_think_tags),
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
        // Per-tool overrides merge per KEY, project winning on a
        // collision. Not concatenated like `extensions`: one tool
        // cannot hold two modes at once, so "both additive" has no
        // meaning here — and not whole-table override either, which
        // would make a project setting one tool silently discard every
        // global setting for the others.
        tool_exec_overrides: {
            let mut m = a.tool_exec_overrides;
            m.extend(b.tool_exec_overrides);
            m
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

    #[test]
    fn allow_store_defaults_off_and_parses() {
        let cfg: Config = toml::from_str(
            "[[extensions]]\npath = \"a.wasm\"\n\n\
             [[extensions]]\npath = \"b.wasm\"\nallow_store = true\n",
        )
        .expect("valid config");
        assert!(
            !cfg.extensions[0].allow_store,
            "a durable store must be opt-in, like every other grant"
        );
        assert!(cfg.extensions[1].allow_store);
    }

    /// SUPERSEDES stage 1's
    /// `a_typod_extension_grant_is_currently_ignored_not_refused`,
    /// which pinned the opposite behaviour and said in its own doc
    /// comment that flipping it had to be deliberate and visible. This
    /// is the flip: `ExtensionConfig` now carries
    /// `deny_unknown_fields`, so a typo'd grant is a load error.
    ///
    /// The error must NAME the offending field and the valid
    /// alternatives, because "unknown field" alone leaves a user
    /// staring at a key that looks correct.
    #[test]
    fn a_typod_extension_grant_is_a_load_error_naming_the_valid_fields() {
        let err = toml::from_str::<Config>(
            "[[extensions]]\npath = \"a.wasm\"\nallow_contxt = true\n",
        )
        .expect_err(
            "a misspelled grant must be REFUSED — silently ignoring it leaves \
             the user believing they granted a capability they did not",
        );
        let msg = err.to_string();
        assert!(msg.contains("allow_contxt"), "must name the typo: {msg}");
        assert!(
            msg.contains("allow_context"),
            "must offer the valid spelling: {msg}"
        );
    }

    /// The other half, and the half that catches an accidentally
    /// renamed or removed field: a config using EVERY valid
    /// `[[extensions]]` key must still load. Without this,
    /// `deny_unknown_fields` plus a rename would turn working configs
    /// into load errors and only this test would notice.
    #[test]
    fn a_config_using_every_valid_extension_key_still_loads() {
        let cfg: Config = toml::from_str(
            "[[extensions]]\n\
             path = \"a.wasm\"\n\
             max_files = 8\n\
             allow_network = true\n\
             allow_fs = true\n\
             allow_store = true\n\
             allow_context = true\n\
             allow_tools = [\"read\"]\n\
             url_allowlist = [\"example.com\"]\n\
             events = [\"input\"]\n",
        )
        .expect("every documented key must remain valid");
        let e = &cfg.extensions[0];
        assert_eq!(e.max_files, 8);
        assert!(e.allow_network && e.allow_fs && e.allow_store && e.allow_context);
        assert_eq!(e.allow_tools, vec!["read".to_string()]);
        assert_eq!(e.url_allowlist, vec!["example.com".to_string()]);
        assert_eq!(e.events, vec!["input".to_string()]);
    }

    /// The new grant parses and is OFF unless asked for — every
    /// capability grant defaults closed.
    #[test]
    fn allow_context_parses_and_defaults_off() {
        let cfg: Config = toml::from_str(
            "[[extensions]]\npath = \"a.wasm\"\n\n\
             [[extensions]]\npath = \"b.wasm\"\nallow_context = true\n",
        )
        .expect("parses");
        assert!(
            !cfg.extensions[0].allow_context,
            "writing the model's instructions must be opt-in, like every \
             other grant"
        );
        assert!(cfg.extensions[1].allow_context);
    }

    /// The grant that spends money. Same default-closed rule, and worth
    /// its own test rather than folding into the every-grant one above:
    /// a regression that flipped this default on would let any
    /// installed plugin bill the user.
    #[test]
    fn allow_send_message_parses_and_defaults_off() {
        let cfg: Config = toml::from_str(
            "[[extensions]]\npath = \"a.wasm\"\n\n\
             [[extensions]]\npath = \"b.wasm\"\nallow_send_message = true\n",
        )
        .expect("parses");
        assert!(
            !cfg.extensions[0].allow_send_message,
            "spending the user's money must be opt-in — this is the one \
             grant whose default being wrong costs cash"
        );
        assert!(cfg.extensions[1].allow_send_message);
    }

    /// `ExtensionConfig` carries `deny_unknown_fields` since stage 2,
    /// so the near-miss spellings a user is likeliest to write are load
    /// ERRORS naming the key, not grants that parse and do nothing.
    #[test]
    fn a_typod_send_grant_is_a_load_error_not_a_silent_no_grant() {
        let err = toml::from_str::<Config>(
            "[[extensions]]\npath = \"a.wasm\"\nallow_send_messages = true\n",
        )
        .expect_err("a typo must not parse into a silently ungranted plugin");
        let msg = err.to_string();
        assert!(
            msg.contains("allow_send_messages"),
            "and it must name the offending key: {msg}"
        );
    }

    #[test]
    fn builtin_defaults() {
        let c = Config::builtin_defaults();
        assert_eq!(c.model, None);
        assert_eq!(c.base_url, None);
    }

    #[test]
    fn missing_files_use_defaults() {
        let _h = crate::TempNanopiHome::new();
        let tmp = TempDir::new();
        let c = load_config(tmp.path()).unwrap();
        assert_eq!(c.model, None);
    }

    #[test]
    fn project_overrides_global() {
        let _h = crate::TempNanopiHome::new();
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
        let _h = crate::TempNanopiHome::new();
        let tmp = TempDir::new();
        tmp.write(".nanopi/config.toml", "this is not valid toml = === =");
        let r = load_config(tmp.path());
        assert!(matches!(r, Err(ConfigError::Toml { .. })));
    }

    /// v0.12.0 §2.3: a retired hook key is a hard load error naming both
    /// the retired key and its replacement.
    #[test]
    fn retired_hook_key_is_a_hard_error_naming_the_replacement() {
        let _h = crate::TempNanopiHome::new();
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

    /// A `[tool_exec_overrides]` key naming a tool that does not exist
    /// is refused at load, naming the valid set — the same rule as a
    /// retired hook key. `bahs = "sequential"` would otherwise parse
    /// clean, change nothing, and leave the user concluding that
    /// concurrent bash is simply still broken.
    #[test]
    fn an_unknown_tool_exec_override_is_a_load_error_naming_the_valid_set() {
        let _h = crate::TempNanopiHome::new();
        let cwd = std::env::temp_dir().join(format!("np-teo-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(cwd.join(".nanopi")).unwrap();
        std::fs::write(
            cwd.join(".nanopi").join("config.toml"),
            "[tool_exec_overrides]\nbahs = \"sequential\"\n",
        )
        .unwrap();

        let err = load_config(&cwd).expect_err("must refuse an unknown tool name");
        let msg = err.to_string();
        assert!(msg.contains("bahs"), "must name the offending key: {msg}");
        assert!(
            msg.contains("bash"),
            "must list the valid names so the typo is obvious: {msg}"
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    /// And a correct one loads.
    #[test]
    fn a_valid_tool_exec_override_loads() {
        let _h = crate::TempNanopiHome::new();
        let cwd = std::env::temp_dir().join(format!("np-teo-ok-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(cwd.join(".nanopi")).unwrap();
        std::fs::write(
            cwd.join(".nanopi").join("config.toml"),
            "[tool_exec_overrides]\nbash = \"parallel\"\n",
        )
        .unwrap();

        let cfg = load_config(&cwd).expect("valid override must load");
        assert_eq!(
            cfg.tool_exec_overrides.get("bash"),
            Some(&crate::tool::ExecutionMode::Parallel)
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    /// A merely-misspelled hook key (never a shipped name) is still a
    /// hard error — it just isn't rewritten by the retired-key table.
    #[test]
    fn misspelled_hook_key_is_still_an_error() {
        let _h = crate::TempNanopiHome::new();
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
            inline_think_tags: None,
            trust: TrustConfig::default(),
            hooks: HooksSection::default(),
            skills: SkillsConfig::default(),
            extensions: Vec::new(),
            tool_exec_mode: ToolExecMode::default(),
            tool_exec_overrides: Default::default(),
        };
        let b = Config {
            model: None,
            base_url: Some("https://b".into()),
            api_key_file: None,
            api_key: None,
            api_kind: None,
            provider: None,
            inline_think_tags: None,
            trust: TrustConfig::default(),
            hooks: HooksSection::default(),
            skills: SkillsConfig::default(),
            extensions: Vec::new(),
            tool_exec_mode: ToolExecMode::default(),
            tool_exec_overrides: Default::default(),
        };
        let m = merge(a, b);
        assert_eq!(m.model.as_deref(), Some("a-model"));
        assert_eq!(m.base_url.as_deref(), Some("https://b"));
        assert_eq!(m.api_key_file.as_deref(), Some(Path::new("/tmp/key")));
    }

    #[test]
    fn config_loads_inline_api_key_and_hooks() {
        let _h = crate::TempNanopiHome::new();
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
