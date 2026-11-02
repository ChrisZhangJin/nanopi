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

use crate::mcp::credentials::{self, redact_url, Lookup};

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
        /// As written, so it may hold `${VAR}` references; expanded only
        /// when connecting (see [`credentials::resolve_http`]).
        url: String,
        /// Sent on every request. Values are **templates** — `Bearer
        /// ${GATEWAY_TOKEN}` — and what they expand to is a secret
        /// (`Authorization` lives here): the expansion is never stored,
        /// printed or part of an endpoint string, and neither is a
        /// literal value, which is why `Debug` shows names only.
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

/// A problem that did not stop startup. Mirrors
/// `model_catalogue::CatalogueDiagnostic` and `resources::SkillDiagnostic`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigDiagnostic {
    pub message: String,
    pub path: PathBuf,
    /// The server whose entry this rejection is about, or `None` for a
    /// problem with the file as a whole (or a warning that did not drop
    /// anything). `/mcp` shows the first kind as a skipped row and the
    /// second as a note.
    pub rejected: Option<String>,
}

/// Outcome of [`load`]. A struct rather than a `Result` because a
/// partially usable config is the normal case: one bad entry must not
/// cost the user the other five servers, and no failure here is fatal.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LoadResult {
    pub servers: Vec<ServerConfig>,
    pub diagnostics: Vec<ConfigDiagnostic>,
    /// Entries that were rejected, in file order, with the same text the
    /// matching diagnostic carries. Kept as data (not just a printed
    /// line) so `/mcp` can show a configured-but-skipped server with its
    /// reason after the startup output has scrolled away.
    pub skipped: Vec<SkippedServer>,
}

/// A server that is in `mcp.json` but was not loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedServer {
    pub name: String,
    /// Why, as one diagnostic. Never contains a header or `env` value
    /// (`diagnostics_never_contain_header_values`).
    pub reason: String,
}

impl LoadResult {
    /// Diagnostics about the file as a whole (unreadable, malformed) or
    /// that are warnings rather than rejections (a repeated name), each
    /// formatted with its path like [`print_diagnostics`]. The per-entry
    /// rejections are in [`skipped`](Self::skipped) instead.
    pub fn file_notes(&self) -> Vec<String> {
        self.diagnostics
            .iter()
            .filter(|d| d.rejected.is_none())
            .map(|d| format!("{} ({})", d.message, d.path.display()))
            .collect()
    }
}

// ── on-disk shape ───────────────────────────────────────────────────

/// The on-disk shape of `mcp.json`. Unknown top-level keys are ignored
/// (serde's default) so a file shared with another client that carries
/// its own extras still loads.
#[derive(Default, Deserialize)]
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
#[derive(Default)]
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
/// `url`, `type` and `headers` select and configure a remote server; see
/// [`validate`]. Other unknown keys are ignored.
// No `Debug`: it would print `env` and `headers` values, and nothing
// needs to format an unvalidated entry.
#[derive(Default, Deserialize)]
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
    #[serde(default)]
    headers: BTreeMap<String, String>,
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
    load_with(path, &credentials::env_lookup)
}

/// [`load`] with the environment as a parameter: `${VAR}` in a remote
/// server's `url` and `headers` is checked against `lookup`.
pub fn load_with(path: &Path, lookup: Lookup) -> LoadResult {
    let mut out = LoadResult::default();

    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return out,
        Err(e) => {
            out.diagnostics.push(ConfigDiagnostic {
                message: format!("could not read ({e}); no MCP servers configured"),
                path: path.to_path_buf(),
                rejected: None,
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
                rejected: None,
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
                rejected: None,
            });
            out.servers.retain(|s| s.name != name);
            out.skipped.retain(|s| s.name != name);
        } else {
            seen.push(name.clone());
        }

        match validate(&name, raw, lookup) {
            Ok(cfg) => out.servers.push(cfg),
            Err(message) => {
                out.skipped.push(SkippedServer { name: name.clone(), reason: message.clone() });
                out.diagnostics.push(ConfigDiagnostic {
                    message,
                    path: path.to_path_buf(),
                    rejected: Some(name),
                });
            }
        }
    }

    out
}

/// Turn one raw entry into a [`ServerConfig`], or into the diagnostic
/// text explaining why it was dropped.
///
/// **A `url` is never read as a command line.** Treating a malformed
/// remote entry as stdio would execute a URL string, so every
/// combination that is not clearly one transport is a diagnostic, never
/// a guess. `type` is optional when `url` is present and means `http`:
/// that is the common case and a pasted block often omits it.
///
/// **`command` together with `url` is rejected, even with a `type`.**
/// Both alternatives lose something: preferring the `command` runs a
/// local process the user may have meant as a remote endpoint, and
/// preferring the `url` sends their `headers` to a host while ignoring a
/// command they wrote on purpose. `type` could break the tie, but then
/// the other field is silently dead config, which is how a stale line
/// survives an edit unnoticed. One line to delete is a better price.
fn validate(name: &str, raw: RawServer, lookup: Lookup) -> Result<ServerConfig, String> {
    let command = raw.command.as_deref().map(str::trim).unwrap_or("");
    let url = raw.url.as_deref().map(str::trim).filter(|u| !u.is_empty());
    let kind = raw.transport_type.as_deref().map(|t| t.trim().to_ascii_lowercase());
    let kind = kind.as_deref();

    if !command.is_empty() && url.is_some() {
        return Err(format!(
            "server \"{name}\" has both \"command\" and \"url\"; it is either a stdio server \
             or a remote one, so remove one of them"
        ));
    }

    // Named by transport rather than "remote": the user needs to know
    // which of the things they might have meant is the unsupported one.
    match kind {
        Some("sse") => {
            return Err(format!(
                "server \"{name}\": SSE (legacy HTTP+SSE) is not supported yet; use \
                 \"type\": \"http\" if the server offers Streamable HTTP"
            ));
        }
        Some("ws" | "websocket") => {
            return Err(format!("server \"{name}\": WebSocket is not supported"));
        }
        None | Some("stdio" | "http") => {}
        Some(other) => {
            return Err(format!(
                "server \"{name}\" has unknown \"type\" {other:?}; expected \"stdio\" or \"http\""
            ));
        }
    }

    if let Some(url) = url {
        if kind == Some("stdio") {
            return Err(format!("server \"{name}\" has \"type\": \"stdio\" but a \"url\""));
        }
        // Resolved here, and the result thrown away, so that a typo, an
        // unset `${VAR}`, an unusable header or cleartext credentials
        // are a config diagnostic naming the server — that one server
        // is skipped and the others load — rather than a failure at
        // connect time. The *template* is what is kept: the transport
        // resolves again when it connects, and nothing expanded lives
        // in the config where a `Debug` or an echo could print it.
        credentials::resolve_http(name, url, &raw.headers, lookup)?;
        return Ok(ServerConfig::http(name, url, raw.headers));
    }

    if kind == Some("http") {
        return Err(format!("server \"{name}\" has \"type\": \"http\" but no \"url\"; skipped"));
    }
    if command.is_empty() {
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
    fn tmp() -> crate::ScratchPath {
        crate::ScratchPath::file("nanopi-mcp-", "mcp.json")
    }

    /// The stdio fields of a server these tests expect to be stdio.
    fn stdio(c: &ServerConfig) -> (&str, &[String], &BTreeMap<String, String>) {
        match &c.spec {
            ServerSpec::Stdio { command, args, env } => (command, args, env),
            other => panic!("expected a stdio server, got {other:?}"),
        }
    }

    fn write(body: &str) -> crate::ScratchPath {
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

    /// Loading must not fail on keys this version has no use for, or a
    /// shared file breaks on the older client. (`headers` means nothing
    /// on a stdio entry.)
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

    fn load_one(entry: &str) -> LoadResult {
        load(&write(&format!(r#"{{ "mcpServers": {{ "x": {entry} }} }}"#)))
    }

    /// The single diagnostic of a load that must have produced no server.
    fn only_diagnostic(res: &LoadResult) -> &str {
        assert!(res.servers.is_empty(), "{:?}", res.servers);
        assert_eq!(res.diagnostics.len(), 1, "{:?}", res.diagnostics);
        &res.diagnostics[0].message
    }

    #[test]
    fn an_http_entry_parses_to_an_http_spec_with_headers() {
        let res = load_one(
            r#"{ "type": "http", "url": "https://example.com/mcp",
                  "headers": { "Authorization": "Bearer t0ken" } }"#,
        );
        assert!(res.diagnostics.is_empty(), "{:?}", res.diagnostics);
        assert_eq!(
            res.servers,
            vec![ServerConfig::http(
                "x",
                "https://example.com/mcp",
                BTreeMap::from([("Authorization".to_string(), "Bearer t0ken".to_string())]),
            )]
        );
    }

    /// A pasted block rarely carries `type`; a `url` alone is the
    /// common spelling of an HTTP server.
    #[test]
    fn type_is_optional_when_url_is_present() {
        let res = load_one(r#"{ "url": "http://127.0.0.1:8931/mcp" }"#);
        assert!(res.diagnostics.is_empty(), "{:?}", res.diagnostics);
        assert_eq!(
            res.servers,
            vec![ServerConfig::http("x", "http://127.0.0.1:8931/mcp", BTreeMap::new())]
        );
    }

    /// The diagnostics name what was asked for, so the user can tell
    /// "not yet" (SSE) from "not at all" (WebSocket).
    #[test]
    fn sse_and_ws_are_diagnosed_by_name() {
        let sse = load_one(r#"{ "type": "sse", "url": "https://example.com/sse" }"#);
        let m = only_diagnostic(&sse);
        assert!(m.contains("SSE (legacy HTTP+SSE) is not supported yet"), "{m}");
        assert!(m.contains("\"x\""), "names the server: {m}");

        let ws = load_one(r#"{ "type": "ws", "url": "wss://example.com/mcp" }"#);
        let m = only_diagnostic(&ws);
        assert!(m.contains("WebSocket is not supported"), "{m}");
        assert!(m.contains("\"x\""), "names the server: {m}");
    }

    /// Ambiguous, so neither is guessed: running the command or sending
    /// headers to the url would each be a surprise. `type` does not
    /// break the tie either — see `validate`.
    #[test]
    fn command_together_with_url_is_rejected_even_with_a_type() {
        for entry in [
            r#"{ "command": "run-me", "url": "https://example.com/mcp" }"#,
            r#"{ "type": "http", "command": "run-me", "url": "https://example.com/mcp" }"#,
            r#"{ "type": "stdio", "command": "run-me", "url": "https://example.com/mcp" }"#,
        ] {
            let res = load_one(entry);
            let m = only_diagnostic(&res);
            assert!(m.contains("both") && m.contains("command") && m.contains("url"), "{m}");
        }
    }

    #[test]
    fn malformed_remote_entries_are_diagnosed_not_guessed() {
        let m = only_diagnostic(&load_one(r#"{ "type": "http" }"#)).to_string();
        assert!(m.contains("no \"url\""), "{m}");
        let m = only_diagnostic(&load_one(r#"{ "type": "carrier-pigeon", "url": "http://h" }"#)).to_string();
        assert!(m.contains("carrier-pigeon"), "{m}");
        let m = only_diagnostic(&load_one(r#"{ "url": "example.com/mcp" }"#)).to_string();
        assert!(m.contains("http://"), "{m}");
        let m = only_diagnostic(&load_one(r#"{ "type": "stdio", "url": "http://h" }"#)).to_string();
        assert!(m.contains("stdio"), "{m}");
    }

    /// Header values are secrets and diagnostics are printed at startup
    /// and pasted into bug reports.
    #[test]
    fn diagnostics_never_contain_header_values() {
        let res = load_one(
            r#"{ "type": "sse", "url": "https://example.com/sse",
                  "headers": { "Authorization": "Bearer s3cret" } }"#,
        );
        assert!(!only_diagnostic(&res).contains("s3cret"));
        let res = load_one(
            r#"{ "command": "c", "url": "https://example.com/mcp",
                  "headers": { "Authorization": "Bearer s3cret" } }"#,
        );
        assert!(!only_diagnostic(&res).contains("s3cret"));
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

    #[test]
    fn rejected_entries_are_data_with_their_reason_and_file_problems_are_not() {
        let path = tmp();
        std::fs::write(
            &path,
            r#"{"mcpServers": {
                "good": {"command": "x"},
                "ws": {"type": "ws", "url": "wss://h.example/mcp"},
                "empty": {}
            }}"#,
        )
        .unwrap();
        let res = load(&path);
        assert_eq!(res.servers.len(), 1);
        let names: Vec<_> = res.skipped.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["ws", "empty"]);
        assert!(res.skipped[0].reason.contains("WebSocket"), "{:?}", res.skipped);
        // Every rejection is also the printed diagnostic, word for word.
        for s in &res.skipped {
            assert!(res.diagnostics.iter().any(|d| d.message == s.reason));
        }
        assert!(res.file_notes().is_empty(), "{:?}", res.file_notes());

        // A malformed file is a file-level note and has no rows.
        std::fs::write(&path, "{ not json").unwrap();
        let bad = load(&path);
        assert!(bad.servers.is_empty() && bad.skipped.is_empty());
        let notes = bad.file_notes();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("invalid JSON at line 1"), "{notes:?}");
        assert!(notes[0].contains("mcp.json"), "the path is included: {notes:?}");
    }

    #[test]
    fn a_repeated_name_leaves_one_skipped_row_and_a_file_note() {
        let path = tmp();
        std::fs::write(
            &path,
            r#"{"mcpServers": {"a": {}, "a": {"type": "ws"}}}"#,
        )
        .unwrap();
        let res = load(&path);
        assert_eq!(res.skipped.len(), 1, "{:?}", res.skipped);
        assert_eq!(res.file_notes().len(), 1, "{:?}", res.file_notes());
        assert!(res.file_notes()[0].contains("more than once"));
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

    // ── H4: credentials ─────────────────────────────────────────────

    /// A lookup over a fixed map: `load_with` takes the environment as a
    /// parameter, so no test here touches the process's.
    fn vars(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| pairs.iter().find(|(k, _)| *k == name).map(|(_, v)| v.to_string())
    }

    fn http_of(c: &ServerConfig) -> (&str, &BTreeMap<String, String>) {
        match &c.spec {
            ServerSpec::Http { url, headers } => (url, headers),
            other => panic!("expected an http server, got {other:?}"),
        }
    }

    /// The config keeps what was written. The expansion exists only
    /// where a request is built, so nothing that prints a config — a
    /// `Debug`, `/mcp`, a diagnostic — can show it.
    #[test]
    fn a_set_variable_loads_and_the_config_keeps_the_template() {
        let res = load_with(
            &write(
                r#"{ "mcpServers": { "gw": { "url": "https://${HOST}/mcp",
                     "headers": { "Authorization": "Bearer ${GW_TOKEN}" } } } }"#,
            ),
            &vars(&[("HOST", "mcp.example.com"), ("GW_TOKEN", "s3cr3t-value")]),
        );
        assert!(res.diagnostics.is_empty(), "{:?}", res.diagnostics);
        let (url, headers) = http_of(&res.servers[0]);
        assert_eq!(url, "https://${HOST}/mcp");
        assert_eq!(headers["Authorization"], "Bearer ${GW_TOKEN}");
        for text in [format!("{:?}", res.servers[0]), format!("{:#?}", res.servers)] {
            assert!(!text.contains("s3cr3t-value") && !text.contains("mcp.example.com"), "{text}");
        }
    }

    /// The owner decision: that one server is skipped, naming the
    /// variable; the rest load. The literal never becomes a header.
    #[test]
    fn an_unset_variable_skips_only_that_server() {
        let res = load_with(
            &write(
                r#"{ "mcpServers": {
                     "before": { "command": "run-me" },
                     "gw":     { "url": "https://h.example/mcp",
                                 "headers": { "Authorization": "Bearer ${GW_TOKEN}" } },
                     "after":  { "url": "https://ok.example/mcp",
                                 "headers": { "X-Key": "${HAVE}" } }
                   } }"#,
            ),
            &vars(&[("HAVE", "v"), ("UNRELATED", "unrelated-secret")]),
        );
        let names: Vec<&str> = res.servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["before", "after"], "{:?}", res.diagnostics);
        assert_eq!(res.diagnostics.len(), 1, "{:?}", res.diagnostics);
        let m = &res.diagnostics[0].message;
        assert!(m.contains("\"gw\"") && m.contains("GW_TOKEN") && m.contains("Authorization"), "{m}");
        assert!(m.contains("skipped"), "{m}");
        assert!(!m.contains("unrelated-secret") && !m.contains("Bearer"), "{m}");
    }

    #[test]
    fn an_unset_variable_in_the_url_skips_that_server_too() {
        let res = load_with(
            &write(r#"{ "mcpServers": { "gw": { "url": "https://${GW_HOST}/mcp" } } }"#),
            &vars(&[]),
        );
        let m = only_diagnostic(&res);
        assert!(m.contains("\"gw\"") && m.contains("GW_HOST"), "{m}");
    }

    #[test]
    fn an_empty_variable_loads_and_an_unset_one_does_not() {
        let body = r#"{ "mcpServers": { "gw": { "url": "https://h.example/mcp",
                          "headers": { "X-Opt": "${V}" } } } }"#;
        assert_eq!(load_with(&write(body), &vars(&[("V", "")])).servers.len(), 1);
        assert!(load_with(&write(body), &vars(&[])).servers.is_empty());
    }

    #[test]
    fn escapes_and_dollar_names_load_as_text() {
        let res = load_with(
            &write(
                r#"{ "mcpServers": { "gw": { "url": "https://h.example/mcp",
                     "headers": { "X-A": "$${NOT_A_VAR}", "X-B": "$HOME", "X-C": "pa$$word" } } } }"#,
            ),
            &vars(&[]),
        );
        assert!(res.diagnostics.is_empty(), "{:?}", res.diagnostics);
    }

    #[test]
    fn malformed_references_skip_the_server_without_quoting_them() {
        for header in ["Bearer ${GW_TOKEN", "${}", "${A${B}}", "${A:-d}"] {
            let res = load_with(
                &write(&format!(
                    r#"{{ "mcpServers": {{ "gw": {{ "url": "https://h.example/mcp",
                         "headers": {{ "Authorization": "{header}" }} }} }} }}"#
                )),
                &vars(&[("A", "x"), ("B", "y")]),
            );
            let m = only_diagnostic(&res);
            assert!(m.contains("\"gw\"") && m.contains("Authorization"), "{m}");
            assert!(!m.contains("Bearer") && !m.contains("GW_TOKEN"), "{m}");
        }
    }

    /// `${…}` is for `headers` and `url`. Everything else is verbatim —
    /// M4 decided that for `env`, and `command` and `args` are no
    /// different.
    #[test]
    fn expansion_applies_to_headers_and_url_only() {
        let res = load_with(
            &write(
                r#"{ "mcpServers": { "s": { "command": "${CMD}", "args": ["--t", "${TOK}"],
                     "env": { "T": "${TOK}" } } } }"#,
            ),
            &vars(&[("CMD", "expanded"), ("TOK", "expanded")]),
        );
        assert!(res.diagnostics.is_empty(), "{:?}", res.diagnostics);
        let (command, args, env) = stdio(&res.servers[0]);
        assert_eq!(command, "${CMD}");
        assert_eq!(args, ["--t", "${TOK}"]);
        assert_eq!(env["T"], "${TOK}");
        // And an unset variable there is not an error: nothing is expanded.
        let res = load_with(
            &write(r#"{ "mcpServers": { "s": { "command": "c", "env": { "T": "${NOPE}" } } } }"#),
            &vars(&[]),
        );
        assert!(res.diagnostics.is_empty() && res.servers.len() == 1);
    }

    #[test]
    fn a_url_that_only_exists_after_expansion_is_accepted() {
        // The scheme check is on the expanded text, so a template that
        // starts with a variable is not rejected for "not starting with http".
        let res = load_with(
            &write(r#"{ "mcpServers": { "s": { "url": "${BASE}/mcp" } } }"#),
            &vars(&[("BASE", "https://h.example")]),
        );
        assert!(res.diagnostics.is_empty(), "{:?}", res.diagnostics);
        let res = load_with(
            &write(r#"{ "mcpServers": { "s": { "url": "${BASE}/mcp" } } }"#),
            &vars(&[("BASE", "h.example")]),
        );
        assert!(only_diagnostic(&res).contains("http://"));
    }

    #[test]
    fn a_header_that_is_invalid_after_expansion_skips_the_server() {
        let res = load_with(
            &write(
                r#"{ "mcpServers": { "gw": { "url": "https://h.example/mcp",
                     "headers": { "Authorization": "Bearer ${TOK}" } } } }"#,
            ),
            &vars(&[("TOK", "line1\nline2-s3cr3t")]),
        );
        let m = only_diagnostic(&res);
        assert!(m.contains("Authorization") && m.contains("\"gw\""), "{m}");
        assert!(!m.contains("line1") && !m.contains("s3cr3t"), "{m}");
    }

    /// The refusal, at load: skipped with a diagnostic, others kept.
    #[test]
    fn cleartext_credentials_to_a_remote_host_skip_that_server() {
        let res = load_with(
            &write(
                r#"{ "mcpServers": {
                     "bad":   { "url": "http://mcp.example.com/mcp",
                                "headers": { "Authorization": "Bearer s3cr3t" } },
                     "trick": { "url": "http://127.0.0.1@evil.com/mcp" },
                     "local": { "url": "http://127.0.0.1:8931/mcp",
                                "headers": { "Authorization": "Bearer s3cr3t" } },
                     "tls":   { "url": "https://mcp.example.com/mcp",
                                "headers": { "Authorization": "Bearer s3cr3t" } },
                     "plain": { "url": "http://mcp.example.com/mcp" }
                   } }"#,
            ),
            &vars(&[]),
        );
        let names: Vec<&str> = res.servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["local", "tls", "plain"], "{:?}", res.diagnostics);
        assert_eq!(res.diagnostics.len(), 2, "{:?}", res.diagnostics);
        for (d, server) in res.diagnostics.iter().zip(["bad", "trick"]) {
            assert!(d.message.contains(&format!("\"{server}\"")), "{}", d.message);
            assert!(d.message.contains("https://") && d.message.contains("cleartext"), "{}", d.message);
            assert!(!d.message.contains("s3cr3t"), "{}", d.message);
        }
    }

    /// The raw template is what an echo shows; a secret in a literal
    /// header never is.
    #[test]
    fn debug_of_a_templated_config_shows_the_template_not_a_value() {
        let http = ServerConfig::http(
            "gw",
            "https://${HOST}/mcp?token=${TOK}",
            BTreeMap::from([("Authorization".to_string(), "Bearer ${TOK}".to_string())]),
        );
        let shown = format!("{http:?}");
        assert!(shown.contains("${HOST}"), "{shown}");
        assert!(!shown.contains("Bearer"), "{shown}");
        assert!(!shown.contains("${TOK}"), "the query is masked: {shown}");
        assert!(shown.contains("Authorization"), "{shown}");
    }
}
