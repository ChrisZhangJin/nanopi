//! Credentials for remote MCP servers: `${VAR}` expansion, the cleartext
//! refusal, and the one place that decides what is safe to print.
//! Design and the owner decisions behind it: `docs/mcp-http.md`.
//!
//! **A secret lives in exactly two places: the environment and the
//! outgoing request.** `mcp.json` keeps the *template* (`Bearer
//! ${GATEWAY_TOKEN}`), [`ServerConfig`](crate::mcp::config::ServerConfig)
//! keeps the template, and every `Debug`, `/tools` row, notice and
//! diagnostic shows the template or a masked form of it. The expanded
//! value exists only inside [`resolve_http`]'s result, which the HTTP
//! transport turns into a `HeaderMap` of sensitive values.
//!
//! Nothing here is `cfg(unix)`: HTTP MCP is available on Windows.

use std::collections::{BTreeMap, BTreeSet};

use reqwest::header::{HeaderName, HeaderValue};
use reqwest::Url;
use url::Host;

/// Where `${NAME}` is looked up. A parameter rather than a direct
/// `std::env::var`, so tests inject a map instead of mutating the
/// process environment under each other's feet.
pub type Lookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// The real environment. A variable that is not valid Unicode counts as
/// unset: there is no way to put it in a header or a URL.
pub fn env_lookup(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

// ----------------------------------------------------------- expansion

/// Why a template could not be expanded. Never carries a value, and
/// carries template text only when it is a valid variable name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpandError {
    /// `${NAME}` named a variable that is not set.
    Unset(String),
    /// `${` with no closing `}`.
    Unterminated,
    /// `${…}` whose content is not `[A-Za-z_][A-Za-z0-9_]*` — which
    /// includes `${}`, a default (`${A:-b}`) and a nested `${A${B}}`.
    BadName,
}

impl std::fmt::Display for ExpandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unset(name) => write!(f, "environment variable {name} is not set"),
            Self::Unterminated => f.write_str("`${` has no closing `}`"),
            Self::BadName => f.write_str(
                "a `${…}` reference must be `${NAME}` with NAME made of letters, digits and `_`",
            ),
        }
    }
}

fn is_name(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Expand `${NAME}` in `template`.
///
/// The whole grammar:
///
/// - `${NAME}` → the variable's value. **Unset is an error; set but
///   empty is allowed** and expands to nothing — an empty value is a
///   statement ("no token here"), an unset one is almost always a typo
///   or a forgotten `export`, and sending `Bearer ` instead of failing
///   would be the quiet version of the bug.
/// - `$NAME` is **not** expanded: it stays literal, so the `$` in a
///   password or a pasted token is not a trap.
/// - `$${` is the escape: it produces a literal `${` and the rest is
///   plain text (`$${HOME}` → `${HOME}`). It is the *only* escape, so
///   every other `$` — including `$$` — stays as written. Read left to
///   right, so `$$${X}` is `$` then the escape, giving `$${X}`.
/// - `${` with no `}`, or with anything but a name inside, is an error:
///   guessing where a reference ends is how a literal `${…` ends up on
///   the wire.
/// - The result is **not** expanded again: a value that itself contains
///   `${OTHER}` is copied as text.
pub fn expand(template: &str, lookup: Lookup) -> Result<String, ExpandError> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        if let Some(r) = after.strip_prefix("${") {
            out.push_str("${");
            rest = r;
        } else if let Some(r) = after.strip_prefix('{') {
            let end = r.find('}').ok_or(ExpandError::Unterminated)?;
            let name = &r[..end];
            if !is_name(name) {
                return Err(ExpandError::BadName);
            }
            match lookup(name) {
                Some(value) => out.push_str(&value),
                None => return Err(ExpandError::Unset(name.to_string())),
            }
            rest = &r[end + 1..];
        } else {
            out.push('$');
            rest = after;
        }
    }
    out.push_str(rest);
    Ok(out)
}

// ------------------------------------------------- referenced variables

/// The names `${NAME}` mentions in `template`, in order, under the same
/// grammar as [`expand`] (so `$${NAME}` is the escape and names nothing)
/// but without ever failing: a malformed reference simply names nothing.
/// Pure, and never looks anything up, so it cannot see a value.
pub fn referenced_names(template: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = template;
    while let Some(i) = rest.find('$') {
        let after = &rest[i + 1..];
        if let Some(r) = after.strip_prefix("${") {
            rest = r;
        } else if let Some(r) = after.strip_prefix('{') {
            let Some(end) = r.find('}') else { break };
            if is_name(&r[..end]) {
                out.push(r[..end].to_string());
            }
            rest = &r[end + 1..];
        } else {
            rest = after;
        }
    }
    out
}

/// Every environment variable an `mcp.json` has *referenced* so far in
/// this process, by name only. See [`record_referenced_env`].
static REFERENCED_ENV: std::sync::Mutex<BTreeSet<String>> =
    std::sync::Mutex::new(BTreeSet::new());

/// Remember that `mcp.json` references `names`. Called by the config
/// loader for every configured server, including ones that end up
/// skipped: the name alone identifies the secret, and a server that is
/// skipped today may be fixed by an edit tomorrow.
///
/// Process-wide and only ever growing, in the style of
/// `mcp::ipc::global()`: the code that spawns child agents has no handle
/// on the session's manager. Names only, never a value.
pub fn record_referenced_env<I: IntoIterator<Item = String>>(names: I) {
    REFERENCED_ENV.lock().unwrap_or_else(|e| e.into_inner()).extend(names);
}

/// The names recorded by [`record_referenced_env`], sorted. What a child
/// agent must not inherit: see `tool::agent::child_env_removals`.
pub fn referenced_env_names() -> Vec<String> {
    REFERENCED_ENV.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect()
}

// ------------------------------------------------------------ loopback

/// Whether `url` points at this machine: `127.0.0.0/8`, `::1` (and its
/// IPv4-mapped form) and the exact name `localhost`.
///
/// Decided on the parsed host, never on the text of the URL, so
/// `localhost.evil.com`, `127.0.0.1.nip.io` and `http://127.0.0.1@evil.com/`
/// (whose host is `evil.com`) are all remote. `*.localhost` is **not**
/// accepted: RFC 6761 makes it loopback, but a resolver that does not
/// implement that sends it to DNS, and this answer gates sending
/// credentials in the clear.
pub fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(d)) => d.trim_end_matches('.').eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        // `::ffff:127.0.0.1` reaches the same socket as `127.0.0.1`.
        Some(Host::Ipv6(ip)) => {
            ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
        None => false,
    }
}

// ------------------------------------------------------------- masking

/// Words that make a name (a query parameter, a `KEY=value` argument)
/// look like it carries a credential. Substring matches, so `api_key`,
/// `X-Api-Key`, `accessToken` and `client_secret` all hit; it errs
/// toward masking too much, which costs a word of display, rather than
/// too little.
const CREDENTIAL_WORDS: [&str; 12] = [
    "token", "secret", "password", "passwd", "pwd", "key", "auth", "credential", "signature",
    "bearer", "cookie", "jwt",
];

/// Whether `name` looks like it carries a credential. `sig` is matched
/// as a whole word only (`X-Amz-Sig`, `sig`), because as a substring it
/// would flag `design` and `signal`.
pub fn is_credential_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    CREDENTIAL_WORDS.iter().any(|w| lower.contains(w))
        || lower.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| w == "sig")
}

/// A URL that is safe to print: userinfo (`user:pass@`) masked and any
/// query or fragment replaced, because tokens ride in both
/// (`?api_key=…`) and telling a credential from a harmless parameter by
/// name is a guess a log line should not depend on. This is stricter
/// than masking only [`is_credential_name`] parameters, on purpose: a
/// parameter called `s` or `sid` is just as secret.
///
/// Works on text, not on a parsed `Url`, so it is total: it masks a
/// malformed URL too, and a template (`https://${HOST}/mcp`) is shown as
/// written. The path is shown as is — a secret there is what `${VAR}` is
/// for, since the template is what gets printed.
pub fn redact_url(url: &str) -> String {
    let (head, rest) = match url.find("://") {
        Some(i) => url.split_at(i + 3),
        None => ("", url),
    };
    let mut host_and_path = rest;
    let mut masked = false;
    if !head.is_empty() {
        // The authority ends at the first `/`; an `@` inside it closes
        // the userinfo.
        let authority_end = rest.find('/').unwrap_or(rest.len());
        if let Some(at) = rest[..authority_end].rfind('@') {
            host_and_path = &rest[at + 1..];
            masked = true;
        }
    }
    let (base, tail) = match host_and_path.find(['?', '#']) {
        Some(i) => (&host_and_path[..i], "?***"),
        None => (host_and_path, ""),
    };
    let userinfo = if masked { "***@" } else { "" };
    format!("{head}{userinfo}{base}{tail}")
}

/// An argument that is safe to print: a `KEY=value` whose key names a
/// credential has its value masked, and a URL has its userinfo and
/// query masked ([`redact_url`]). Anything else is shown as written.
///
/// A secret in the *next* argument (`--api-key abc`) cannot be told
/// apart from any other word, so it is not caught.
pub fn redact_arg(arg: &str) -> String {
    if let Some((key, _)) = arg.split_once('=') {
        if !key.contains("://") && is_credential_name(key) {
            return format!("{key}=***");
        }
    }
    if arg.contains("://") {
        return redact_url(arg);
    }
    arg.to_string()
}

// ---------------------------------------------------- resolving a server

/// An HTTP server's URL and headers with every `${VAR}` expanded and
/// every check passed. Deliberately without `Debug`.
pub struct ResolvedHttp {
    pub url: Url,
    pub headers: Vec<(HeaderName, HeaderValue)>,
    /// Text a server might echo back in an error page: the header
    /// values and the URL's own credentials. The transport scrubs these
    /// out of whatever it keeps or shows from a response.
    pub secrets: Vec<String>,
}

/// Shortest text that is scrubbed from a response: anything shorter
/// would blank out ordinary words.
const MIN_SCRUBBED: usize = 4;

/// Expand and validate one HTTP server's `url` and `headers`.
///
/// Used twice on purpose: by `config::load` so a bad server is skipped
/// with a diagnostic at startup (the others keep loading), and by the
/// transport when it connects, which is the only thing that sends
/// anything and so must not trust that a config object was ever loaded
/// from a file.
///
/// Every `Err` names the server and, where it applies, the header or the
/// variable *name*. None contains an expanded value or the text of a
/// header value or URL.
pub fn resolve_http(
    server: &str,
    url: &str,
    headers: &BTreeMap<String, String>,
    lookup: Lookup,
) -> Result<ResolvedHttp, String> {
    let expanded = expand(url, lookup)
        .map_err(|e| format!("server \"{server}\": \"url\": {e}; server skipped"))?;
    // `Url::parse` would call `example.com:8080/mcp` a URL with scheme
    // `example.com`; say what the user got wrong instead.
    if !expanded.contains("://") {
        return Err(format!(
            "server \"{server}\" has a \"url\" that does not start with http:// or https://"
        ));
    }
    // `ParseError` names the problem, never the input.
    let url = Url::parse(&expanded)
        .map_err(|e| format!("server \"{server}\" has an invalid \"url\" ({e})"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "server \"{server}\": url scheme `{}` is not http or https",
            url.scheme()
        ));
    }
    let Some(host) = url.host_str() else {
        return Err(format!("server \"{server}\": url has no host"));
    };

    // Before the headers are expanded: the cause to report is the
    // configuration, not a variable that may well be set.
    if url.scheme() == "http" && !is_loopback(&url) {
        let carries = credentials_in(&url, !headers.is_empty());
        if !carries.is_empty() {
            return Err(format!(
                "server \"{server}\": refusing to send credentials ({}) in cleartext to \
                 remote host {host:?}; use an https:// url (plain http:// is only allowed for \
                 localhost, 127.0.0.0/8 and ::1); server skipped",
                carries.join(", ")
            ));
        }
    }

    let mut secrets = Vec::new();
    let mut out = Vec::with_capacity(headers.len());
    for (name, template) in headers {
        let n = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
            format!("server \"{server}\": {name:?} is not a valid header name; server skipped")
        })?;
        let value = expand(template, lookup)
            .map_err(|e| format!("server \"{server}\": header {name:?}: {e}; server skipped"))?;
        let mut v = HeaderValue::from_str(&value).map_err(|_| {
            format!(
                "server \"{server}\": the value of header {name:?} is not valid after \
                 expansion (control characters, such as a trailing newline in the variable); \
                 server skipped"
            )
        })?;
        v.set_sensitive(true);
        remember_secret(&mut secrets, &value);
        out.push((n, v));
    }
    if let Some(p) = url.password() {
        remember_secret(&mut secrets, p);
    }
    for (k, v) in url.query_pairs() {
        if is_credential_name(&k) {
            remember_secret(&mut secrets, &v);
        }
    }
    Ok(ResolvedHttp { url, headers: out, secrets })
}

/// What in this request is a credential, for the cleartext refusal. Any
/// configured header counts: telling `Authorization` from a harmless one
/// by name is a guess, and refusing wrongly costs a switch to `https://`.
fn credentials_in(url: &Url, has_headers: bool) -> Vec<&'static str> {
    let mut found = Vec::new();
    if has_headers {
        found.push("headers");
    }
    if !url.username().is_empty() || url.password().is_some() {
        found.push("URL userinfo");
    }
    if url.query_pairs().any(|(k, _)| is_credential_name(&k)) {
        found.push("a credential-looking query parameter");
    }
    found
}

/// A header value, and — when it has the shape `<scheme> <credential>`
/// — the credential alone, since that is what a server quotes back.
fn remember_secret(secrets: &mut Vec<String>, value: &str) {
    if value.chars().count() >= MIN_SCRUBBED {
        secrets.push(value.to_string());
    }
    if let Some((_, credential)) = value.rsplit_once(char::is_whitespace) {
        if credential.chars().count() >= MIN_SCRUBBED {
            secrets.push(credential.to_string());
        }
    }
}

/// `text` with every one of `secrets` replaced by `***`, longest first
/// so a secret that contains another is masked whole.
pub fn scrub(text: &str, secrets: &[String]) -> String {
    let mut ordered: Vec<&String> = secrets.iter().collect();
    ordered.sort_by_key(|s| std::cmp::Reverse(s.len()));
    let mut out = text.to_string();
    for s in ordered {
        out = out.replace(s.as_str(), "***");
    }
    out
}

#[cfg(test)]
mod tests;
