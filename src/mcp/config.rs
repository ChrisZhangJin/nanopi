//! MCP server configuration: `<nanopi_home>/mcp.json`.
//!
//! The file shape is the de-facto ecosystem one — a single `mcpServers`
//! object keyed by server name — so a block already working in Claude
//! Code or another client pastes in unchanged. Inventing a nicer schema
//! would buy nothing and cost every user a translation step.
//!
//! **Global only.** Deliberately not `<cwd>/.nanopi/mcp.json`:
//! `trust::check_trust_status()` returns `NoProjectResources` only while
//! `<cwd>/.nanopi` does not exist, so creating a project-scoped file
//! flips every repo that adopts it to `NeedsPrompt` — which resolves as
//! *untrusted* and silently disables that repo's skills, `SYSTEM.md`
//! overrides and project agents. A project-local server list is not
//! worth turning the rest of the project's configuration off.
//!
//! **Never written by nanopi.** This is the one place the skeleton
//! borrowed from `crate::model_catalogue` is deliberately not followed.
//! That module materialises the compiled model table because there *is*
//! a sensible default to write; there is no default set of MCP servers,
//! so there is nothing to materialise, nothing to refresh on a version
//! bump, and no backup to keep. Load-only means the malformed-file path
//! cannot destroy anything either.
//!
//! Nothing here aborts startup. Only `config.toml` and `settings.toml`
//! are fatal when malformed; a bad `mcp.json` degrades to "no servers"
//! and says so, naming the line.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

/// One validated server, ready for a transport to connect.
///
/// `name` is carried in the struct rather than left as a map key so the
/// loaded set is an ordered `Vec`: diagnostics, `/mcp` and connection
/// order then follow the order the user wrote in the file instead of a
/// hash or alphabetical order they never chose.
///
/// `Debug` is written by hand: a derived one prints every `env` and
/// `headers` value, and a `{:?}` of a config in a log line or a panic
/// message is exactly how a bearer token ends up in a bug report.
#[derive(Clone, PartialEq, Eq)]
pub struct ServerConfig {
    pub name: String,
    pub spec: ServerSpec,
}

/// How to reach a server. An enum rather than optional fields on
/// [`ServerConfig`]: `command` empty-means-HTTP would let a config that
/// is neither (or both) compile, and every consumer would have to
/// re-derive which one it holds.
#[derive(Clone, PartialEq, Eq)]
pub enum ServerSpec {
    Stdio {
        command: String,
        args: Vec<String>,
        /// Extra environment for the child process. Values are used
        /// **verbatim**: `${VAR}` is not interpolated — see `validate`
        /// for why that decision is deferred to the code that builds the
        /// child process rather than made here.
        env: BTreeMap<String, String>,
    },
    Http {
        url: String,
        /// Sent on every request. **Secrets** (`Authorization` lives
        /// here): never printed, never part of an endpoint string.
        headers: BTreeMap<String, String>,
    },
}

impl ServerConfig {
    pub fn stdio(
        name: impl Into<String>,
        command: impl Into<String>,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    ) -> Self {
        Self {
            name: name.into(),
            spec: ServerSpec::Stdio { command: command.into(), args, env },
        }
    }

    pub fn http(
        name: impl Into<String>,
        url: impl Into<String>,
        headers: BTreeMap<String, String>,
    ) -> Self {
        Self {
            name: name.into(),
            spec: ServerSpec::Http { url: url.into(), headers },
        }
    }
}

/// Keys shown, values hidden: which variables or headers are set is
/// what anyone debugging a config needs, and the values are the secret.
struct Redacted<'a>(&'a BTreeMap<String, String>);

impl std::fmt::Debug for Redacted<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map()
            .entries(self.0.keys().map(|k| (k, "<redacted>")))
            .finish()
    }
}

impl std::fmt::Debug for ServerSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // `args` stay verbatim, as they were under the derive:
            // unlike `env` they are not a conventional place for
            // secrets, and the heuristic masking `endpoint_of` applies
            // would make a debug dump misleading about what is run.
            ServerSpec::Stdio { command, args, env } => f
                .debug_struct("Stdio")
                .field("command", command)
                .field("args", args)
                .field("env", &Redacted(env))
                .finish(),
            ServerSpec::Http { url, headers } => f
                .debug_struct("Http")
                .field("url", &redact_url(url))
                .field("headers", &Redacted(headers))
                .finish(),
        }
    }
}

impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerConfig")
            .field("name", &self.name)
            .field("spec", &self.spec)
            .finish()
    }
}

/// A URL that is safe to print: userinfo (`user:pass@`) masked and any
/// query or fragment replaced, because tokens ride in both
/// (`?api_key=…`) and telling a credential from a harmless parameter by
/// name is a guess a log line should not depend on.
pub(crate) fn redact_url(url: &str) -> String {
    let (base, tail) = match url.find(['?', '#']) {
        Some(i) => (&url[..i], "?***"),
        None => (url, ""),
    };
    let masked = base.find("://").and_then(|i| {
        let (head, rest) = base.split_at(i + 3);
        let authority_end = rest.find('/').unwrap_or(rest.len());
        let at = rest[..authority_end].rfind('@')?;
        Some(format!("{head}***@{}", &rest[at + 1..]))
    });
    format!("{}{tail}", masked.unwrap_or_else(|| base.to_string()))
}

/// A problem that did not stop startup. Mirrors
/// `model_catalogue::CatalogueDiagnostic` and `resources::SkillDiagnostic`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigDiagnostic {
    pub message: String,
    pub path: PathBuf,
}

/// Outcome of [`load`]. A struct rather than a `Result` because a
/// partially usable config is the normal case: one bad entry must not
/// cost the user the other five servers, and no failure here is fatal.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LoadResult {
    pub servers: Vec<ServerConfig>,
    pub diagnostics: Vec<ConfigDiagnostic>,
}

// ── on-disk shape ───────────────────────────────────────────────────

/// The on-disk shape of `mcp.json`. Unknown top-level keys are ignored
/// (serde's default) so a file shared with another client that carries
/// its own extras still loads.
#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: RawServers,
}

/// `mcpServers` as written, in file order and **without** de-duplicating.
///
/// A `BTreeMap`/`HashMap` would silently drop all but the last entry for
/// a repeated name — the JSON spec permits duplicate keys and every serde
/// map impl resolves them last-wins — and a user who pasted the same
/// server twice would get no hint that half their edit was ignored. The
/// visitor keeps every pair so [`load`] can diagnose it.
#[derive(Debug, Default)]
struct RawServers(Vec<(String, RawServer)>);

impl<'de> Deserialize<'de> for RawServers {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = RawServers;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an object mapping server names to server definitions")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut m: M) -> Result<RawServers, M::Error> {
                let mut out = Vec::new();
                while let Some((k, v)) = m.next_entry::<String, RawServer>()? {
                    out.push((k, v));
                }
                Ok(RawServers(out))
            }
        }
        d.deserialize_map(V)
    }
}

/// One entry as written. Every field is optional here so that a bad
/// entry produces a diagnostic naming the server rather than a parse
/// error that takes the whole file down with it.
///
/// `url` and `transport_type` are captured only to tell a remote-server
/// entry apart from a plain typo; see [`validate`]. Other unknown keys
/// (`headers`, future transports) are ignored.
#[derive(Debug, Default, Deserialize)]
struct RawServer {
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default, rename = "type")]
    transport_type: Option<String>,
}

// ── loading ─────────────────────────────────────────────────────────

/// Load `path`.
///
/// - absent → empty result, **no diagnostic**: having no MCP servers is
///   the common case, not a problem to report
/// - unparseable → **never rewritten or removed**, diagnose with the
///   line number, continue with no servers
/// - a bad entry → diagnose that entry, keep the rest
pub fn load(path: &Path) -> LoadResult {
    let mut out = LoadResult::default();

    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return out,
        Err(e) => {
            out.diagnostics.push(ConfigDiagnostic {
                message: format!("could not read ({e}); no MCP servers configured"),
                path: path.to_path_buf(),
            });
            return out;
        }
    };

    let parsed: ConfigFile = match serde_json::from_str(&text) {
        Ok(f) => f,
        Err(e) => {
            // Nothing in this module writes, so the file is safe by
            // construction — the line number is what the user needs.
            out.diagnostics.push(ConfigDiagnostic {
                message: format!(
                    "invalid JSON at line {} ({e}); leaving it untouched and starting with no \
                     MCP servers",
                    e.line()
                ),
                path: path.to_path_buf(),
            });
            return out;
        }
    };

    let mut seen: Vec<String> = Vec::new();
    for (name, raw) in parsed.mcp_servers.0 {
        // Last-wins matches what every JSON parser does with a repeated
        // key, so the only thing missing is the user being told.
        if seen.iter().any(|s| s == &name) {
            out.diagnostics.push(ConfigDiagnostic {
                message: format!("server \"{name}\" is defined more than once; using the last one"),
                path: path.to_path_buf(),
            });
            out.servers.retain(|s| s.name != name);
        } else {
            seen.push(name.clone());
        }

        match validate(&name, raw) {
            Ok(cfg) => out.servers.push(cfg),
            Err(message) => out.diagnostics.push(ConfigDiagnostic {
                message,
                path: path.to_path_buf(),
            }),
        }
    }

    out
}

/// Turn one raw entry into a [`ServerConfig`], or into the diagnostic
/// text explaining why it was dropped.
///
/// **A `url` with no `command` is an error, not a silent skip.** It is
/// the one malformed case where we can tell exactly what the user meant
/// — a remote server, which nanopi cannot speak to until the Streamable
/// HTTP transport lands — and where guessing is actively dangerous: the
/// only other reading is "run it as stdio", which would execute a URL
/// string as a command line. Claude Code treats a `url` without a
/// `type` as a hard configuration error for the same reason. Staying
/// quiet would leave the user watching a server they configured never
/// appear, with nothing anywhere saying why.
fn validate(name: &str, raw: RawServer) -> Result<ServerConfig, String> {
    let command = raw.command.unwrap_or_default();
    let command = command.trim();

    if command.is_empty() {
        if raw.url.is_some() {
            let kind = raw.transport_type.as_deref().unwrap_or("remote");
            return Err(format!(
                "server \"{name}\" is a {kind} server (\"url\"), which is not supported yet; \
                 only stdio servers with a \"command\" are"
            ));
        }
        return Err(format!("server \"{name}\" has no \"command\"; skipped"));
    }

    Ok(ServerConfig::stdio(
        name,
        command,
        raw.args,
        // Not expanded. `${VAR}` is left verbatim for now: expansion
        // needs a decided syntax (`${VAR}` vs `$VAR`), an escape for a
        // literal `$` — Postgres and GitHub tokens contain one — and a
        // rule for an unset variable (empty, error, or leave as-is),
        // and every wrong answer there either leaks or silently blanks
        // a credential. It also belongs next to the code that actually
        // builds the child's environment and decides whether the parent
        // env is inherited at all, which is M2's spawn path, not here.
        raw.env,
    ))
}

/// Print load diagnostics, mirroring `model_catalogue::print_diagnostics`.
pub fn print_diagnostics(diags: &[ConfigDiagnostic]) {
    for d in diags {
        crate::note!("nanopi: mcp.json: {} ({})", d.message, d.path.display());
    }
}

/// Load the global `mcp.json`. The one call startup makes.
///
/// A `None` path (no resolvable home) is not an error — it just means no
/// servers, exactly like an absent file.
pub fn load_global() -> LoadResult {
    match crate::paths::global_mcp_path() {
        Some(p) => load(&p),
        None => LoadResult::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory per test: these cases turn on whether a file exists,
    /// so they must not share one.
    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("nanopi-mcp-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&d).unwrap();
        d.join("mcp.json")
    }

    /// The stdio fields of a server these tests expect to be stdio.
    fn stdio(c: &ServerConfig) -> (&str, &[String], &BTreeMap<String, String>) {
        match &c.spec {
            ServerSpec::Stdio { command, args, env } => (command, args, env),
            other => panic!("expected a stdio server, got {other:?}"),
        }
    }

    fn write(body: &str) -> PathBuf {
        let p = tmp();
        std::fs::write(&p, body).unwrap();
        p
    }

    /// Having no MCP servers is the common case, so it must be silent —
    /// a startup warning on every run for every user who never
    /// configured one would train people to ignore the channel.
    #[test]
    fn absent_file_is_a_silent_no_op() {
        let res = load(&tmp());
        assert!(res.servers.is_empty());
        assert!(res.diagnostics.is_empty(), "{:?}", res.diagnostics);
    }

    #[test]
    fn empty_object_is_a_silent_no_op() {
        let res = load(&write("{}"));
        assert!(res.servers.is_empty());
        assert!(res.diagnostics.is_empty(), "{:?}", res.diagnostics);
    }

    #[test]
    fn empty_mcp_servers_is_a_silent_no_op() {
        let res = load(&write(r#"{ "mcpServers": {} }"#));
        assert!(res.servers.is_empty());
        assert!(res.diagnostics.is_empty(), "{:?}", res.diagnostics);
    }

    /// The paste-from-another-client case, verbatim from `docs/mcp.md`.
    #[test]
    fn a_valid_entry_loads_with_args_and_env() {
        let res = load(&write(
            r#"{ "mcpServers": {
                 "github": {
                   "command": "npx",
                   "args": ["-y", "@modelcontextprotocol/server-github"],
                   "env": { "GITHUB_TOKEN": "t0ken" }
                 }
               } }"#,
        ));
        assert!(res.diagnostics.is_empty(), "{:?}", res.diagnostics);
        assert_eq!(
            res.servers,
            vec![ServerConfig::stdio(
                "github",
                "npx",
                vec!["-y".into(), "@modelcontextprotocol/server-github".into()],
                BTreeMap::from([("GITHUB_TOKEN".to_string(), "t0ken".to_string())]),
            )]
        );
    }

    #[test]
    fn args_and_env_default_to_empty() {
        let res = load(&write(r#"{ "mcpServers": { "x": { "command": "run-me" } } }"#));
        assert!(res.diagnostics.is_empty(), "{:?}", res.diagnostics);
        assert_eq!(res.servers.len(), 1);
        assert!(stdio(&res.servers[0]).1.is_empty());
        assert!(stdio(&res.servers[0]).2.is_empty());
    }

    /// Later transports put `type`, `url` and `headers` here. Loading
    /// must not fail on keys this version has no use for, or a shared
    /// file breaks on the older client.
    #[test]
    fn unknown_keys_are_ignored() {
        let res = load(&write(
            r#"{ "schema": 1, "mcpServers": {
                 "x": { "command": "run-me", "headers": { "A": "b" }, "future": [1,2] }
               } }"#,
        ));
        assert!(res.diagnostics.is_empty(), "{:?}", res.diagnostics);
        assert_eq!(res.servers.len(), 1);
    }

    #[test]
    fn entry_without_command_is_dropped_with_a_diagnostic() {
        let res = load(&write(
            r#"{ "mcpServers": {
                 "broken": { "args": ["-y"] },
                 "fine":   { "command": "run-me" }
               } }"#,
        ));
        assert_eq!(res.servers.len(), 1, "one bad entry must not cost the others");
        assert_eq!(res.servers[0].name, "fine");
        assert_eq!(res.diagnostics.len(), 1);
        assert!(res.diagnostics[0].message.contains("broken"), "{:?}", res.diagnostics);
        assert!(res.diagnostics[0].message.contains("command"), "{:?}", res.diagnostics);
    }

    /// A blank command is the same mistake with a space in it.
    #[test]
    fn blank_command_is_treated_as_missing() {
        let res = load(&write(r#"{ "mcpServers": { "x": { "command": "   " } } }"#));
        assert!(res.servers.is_empty());
        assert_eq!(res.diagnostics.len(), 1);
    }

    /// A remote entry is diagnosed, never guessed at — reading a `url`
    /// as a command line would execute it.
    #[test]
    fn url_without_command_reports_an_unsupported_transport() {
        let res = load(&write(
            r#"{ "mcpServers": {
                 "remote": { "type": "http", "url": "https://example.com/mcp" }
               } }"#,
        ));
        assert!(res.servers.is_empty());
        assert_eq!(res.diagnostics.len(), 1);
        let m = &res.diagnostics[0].message;
        assert!(m.contains("remote"), "{m}");
        assert!(m.contains("not supported yet"), "{m}");
        assert!(m.contains("http"), "names the transport it found: {m}");
    }

    /// JSON allows a repeated key and every parser resolves it
    /// last-wins, so the only real risk is the user not being told half
    /// their edit was discarded.
    #[test]
    fn duplicate_server_name_keeps_the_last_and_says_so() {
        let res = load(&write(
            r#"{ "mcpServers": {
                 "dup": { "command": "first" },
                 "dup": { "command": "second" }
               } }"#,
        ));
        assert_eq!(res.servers.len(), 1);
        assert_eq!(stdio(&res.servers[0]).0, "second", "last definition wins");
        assert_eq!(res.diagnostics.len(), 1);
        assert!(res.diagnostics[0].message.contains("more than once"), "{:?}", res.diagnostics);
    }

    /// Order is the user's, not the map's: `/mcp` and connection order
    /// should match the file they edited.
    #[test]
    fn servers_keep_file_order() {
        let res = load(&write(
            r#"{ "mcpServers": {
                 "zebra": { "command": "a" },
                 "alpha": { "command": "b" }
               } }"#,
        ));
        let names: Vec<&str> = res.servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["zebra", "alpha"]);
    }

    /// A typo must never cost the user their config, and the message
    /// must point at the line so it can be fixed without bisecting.
    #[test]
    fn malformed_json_is_left_byte_identical_and_names_the_line() {
        const BODY: &str = "{\n  \"mcpServers\": {\n    \"x\": { \"command\": }\n  }\n}\n";
        let p = write(BODY);
        let res = load(&p);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), BODY, "must not rewrite");
        assert!(res.servers.is_empty(), "startup continues with no servers");
        assert_eq!(res.diagnostics.len(), 1);
        let m = &res.diagnostics[0].message;
        assert!(m.contains("invalid JSON at line 3"), "{m}");
    }

    /// Recorded decision, not a feature: values reach the child exactly
    /// as written. If expansion ever lands it must be a deliberate
    /// change with its own tests, not something this test quietly
    /// stopped noticing.
    #[test]
    fn env_values_are_not_expanded() {
        let res = load(&write(
            r#"{ "mcpServers": { "x": { "command": "run-me", "env": { "T": "${HOME}" } } } }"#,
        ));
        assert_eq!(stdio(&res.servers[0]).2["T"], "${HOME}");
    }

    /// A `{:?}` of a config lands in logs and panic messages, so a
    /// derived `Debug` would print every token. Keys stay: which
    /// variables or headers are set is what a debugger needs.
    #[test]
    fn debug_hides_env_and_header_values_but_keeps_the_keys() {
        let stdio = ServerConfig::stdio(
            "db",
            "npx",
            vec!["-y".into()],
            BTreeMap::from([("API_KEY".to_string(), "sk-live-xyz".to_string())]),
        );
        let http = ServerConfig::http(
            "gw",
            "https://user:hunter2@example.com/mcp?api_key=abc123",
            BTreeMap::from([("Authorization".to_string(), "Bearer s3cret".to_string())]),
        );
        for (cfg, secrets, key) in [
            (&stdio, ["sk-live-xyz", "", ""], "API_KEY"),
            (&http, ["s3cret", "hunter2", "abc123"], "Authorization"),
        ] {
            for text in [format!("{cfg:?}"), format!("{cfg:#?}"), format!("{:?}", cfg.spec)] {
                for secret in secrets.iter().filter(|s| !s.is_empty()) {
                    assert!(!text.contains(secret), "{secret} leaked: {text}");
                }
                assert!(text.contains(key), "{text}");
            }
        }
    }

    #[test]
    fn redact_url_masks_userinfo_and_query_and_leaves_the_rest() {
        assert_eq!(redact_url("http://127.0.0.1:8931/mcp"), "http://127.0.0.1:8931/mcp");
        assert_eq!(redact_url("https://u:p@h.example/mcp"), "https://***@h.example/mcp");
        assert_eq!(redact_url("https://h.example/mcp?token=abc#frag"), "https://h.example/mcp?***");
    }
}
