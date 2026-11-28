//! Built-in agent memory, Phase 1 (`docs/memory-design.md`).
//!
//! Layers: daily notes (append-only working notes, never rewritten by
//! nanopi), the distilled library (one Markdown file per entry), and the
//! `MEMORY.md` index. Everything lives under `<nanopi_home>/memory`,
//! dirs 0700, files 0600, plain Markdown.
//!
//! Trust rules enforced here rather than by prompt:
//! - all text entering the store is validated: no credentials, no
//!   recorded approvals, no host addresses or user paths;
//! - environment entries never leave the machine unless the active
//!   profile says `memory_disclosure = "full"` for the exact endpoint
//!   (see [`DisclosureGuard`]); everything else fails closed;
//! - files a user edited or deleted are never overwritten or recreated,
//!   and forgotten IDs are never regenerated.

mod dream;
pub mod dream_progress;
#[doc(hidden)]
pub mod schedule;
mod tools;

pub(crate) use dream::{plan as dream_plan, run as dream_run, DreamPlan, Report as DreamReport, STATUS_LABEL as DREAM_STATUS_LABEL};
pub(crate) use tools::{ForgetTool, MemorySearchTool, RememberTool};

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{bail, ensure, Context as _, Result};
use chrono::{Local, NaiveDate};
use regex::Regex;
use serde::{Deserialize, Serialize};

// ── disclosure policy ────────────────────────────────────────────────

/// Delimiters around environment content inside anything that might be
/// sent to a model. The guard removes everything between them unless
/// the endpoint is allowed full disclosure. Store validation rejects
/// these strings in memory text, so content cannot close a block early.
// Built with `concat!` so nanopi's own source never contains the literal
// marker (reading this file in a session must not trigger the scrub).
pub(crate) const ENV_OPEN: &str = concat!("<<nanopi-", "memory:environment>>");
pub(crate) const ENV_CLOSE: &str = concat!("<</nanopi-", "memory:environment>>");
const RESERVED: &[&str] = &[concat!("nanopi-", "memory"), "<!--", "-->"];
const WITHHELD: &str = "[environment memory withheld: this endpoint is not allowed full memory disclosure]";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Disclosure {
    #[default]
    General,
    Full,
}

/// Memory settings, read from the active profile's own config file only.
/// A project's `.nanopi/config.toml` is never consulted: a repository
/// must not be able to widen what memory is sent where.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub(crate) struct Policy {
    pub memory_disclosure: Disclosure,
    /// `full` only applies to this exact base URL. Written alongside
    /// `memory_disclosure` by `/memory disclosure full`, so an endpoint
    /// change in the same profile falls back to `general`.
    pub memory_full_endpoint: Option<String>,
    pub memory_env_review_days: i64,
    pub memory_review_days: i64,
    pub memory_dream_max_calls: usize,
    pub memory_dream_max_input_chars: usize,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            memory_disclosure: Disclosure::General,
            memory_full_endpoint: None,
            memory_env_review_days: 90,
            memory_review_days: 180,
            memory_dream_max_calls: 3,
            memory_dream_max_input_chars: 40_000,
        }
    }
}

impl Policy {
    /// Any read or parse failure yields the default (`general`).
    pub(crate) fn active() -> Self {
        crate::profile::current_config_path()
            .and_then(|p| read_limited(&p, 256 * 1024).ok())
            .and_then(|s| toml::from_str::<Policy>(&s).ok())
            .map(|mut p| {
                p.memory_dream_max_calls = p.memory_dream_max_calls.clamp(1, 20);
                p.memory_dream_max_input_chars = p.memory_dream_max_input_chars.clamp(2_000, 400_000);
                p.memory_env_review_days = p.memory_env_review_days.max(1);
                p.memory_review_days = p.memory_review_days.max(1);
                p
            })
            .unwrap_or_default()
    }

    pub(crate) fn full_for(&self, endpoint: &str) -> bool {
        self.memory_disclosure == Disclosure::Full
            && self
                .memory_full_endpoint
                .as_deref()
                .is_some_and(|e| e.trim_end_matches('/') == endpoint.trim_end_matches('/'))
    }

    /// Whether environment content is disclosed by tools that do not know
    /// the endpoint. The provider guard still checks the exact endpoint.
    fn env_allowed_somewhere(&self) -> bool {
        self.memory_disclosure == Disclosure::Full && self.memory_full_endpoint.is_some()
    }
}

/// Remove environment blocks from `s`. An unterminated block is removed
/// to the end of the string (fail closed).
pub(crate) fn scrub(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.contains(ENV_OPEN) && !s.contains(ENV_CLOSE) {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(ENV_OPEN) {
        out.push_str(&rest[..i]);
        out.push_str(WITHHELD);
        rest = &rest[i + ENV_OPEN.len()..];
        match rest.find(ENV_CLOSE) {
            Some(j) => rest = &rest[j + ENV_CLOSE.len()..],
            None => rest = "",
        }
    }
    out.push_str(rest);
    // A stray close marker means the text was tampered with; drop what
    // precedes it rather than guess where the block began.
    if let Some(j) = out.rfind(ENV_CLOSE) {
        out = format!("{WITHHELD}{}", &out[j + ENV_CLOSE.len()..]);
    }
    std::borrow::Cow::Owned(out)
}

/// Scrub every text a provider may send: system prompt, user and
/// assistant text, tool arguments and tool results.
pub(crate) fn scrub_context(ctx: &crate::agent::context::Context) -> Option<crate::agent::context::Context> {
    let raw = serde_json::to_string(ctx).ok()?;
    // Fast path: the JSON encoding of the markers contains no escapes.
    if !raw.contains(ENV_OPEN) && !raw.contains(ENV_CLOSE) {
        return None;
    }
    let mut v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    fn walk(v: &mut serde_json::Value) {
        match v {
            serde_json::Value::String(s) => {
                if let std::borrow::Cow::Owned(o) = scrub(s) {
                    *s = o;
                }
            }
            serde_json::Value::Array(a) => a.iter_mut().for_each(walk),
            serde_json::Value::Object(m) => m.values_mut().for_each(walk),
            _ => {}
        }
    }
    walk(&mut v);
    serde_json::from_value(v).ok()
}

/// Wraps every provider built by `provider::build`, so compaction, goal
/// checks, branch summaries, Dream and normal turns all pass through the
/// same disclosure check immediately before the request is made.
pub(crate) struct DisclosureGuard {
    pub inner: Box<dyn crate::agent::loop_::Provider>,
    pub endpoint: String,
}

#[async_trait::async_trait]
impl crate::agent::loop_::Provider for DisclosureGuard {
    fn id(&self) -> &'static str {
        self.inner.id()
    }
    async fn stream_turn(
        &self,
        ctx: &crate::agent::context::Context,
        tx: tokio::sync::mpsc::Sender<crate::event::AgentEvent>,
    ) -> Result<crate::event::Usage, String> {
        let hidden = hide_search_when_empty(ctx);
        let ctx = hidden.as_ref().unwrap_or(ctx);
        if Policy::active().full_for(&self.endpoint) {
            return self.inner.stream_turn(ctx, tx).await;
        }
        match scrub_context(ctx) {
            Some(clean) => self.inner.stream_turn(&clean, tx).await,
            None => {
                // `None` also covers a context that could not be
                // re-encoded; only send it if it holds no marker at all.
                let raw = serde_json::to_string(ctx).map_err(|e| e.to_string())?;
                if raw.contains(ENV_OPEN) || raw.contains(ENV_CLOSE) {
                    return Err("memory disclosure check failed; request not sent".into());
                }
                self.inner.stream_turn(ctx, tx).await
            }
        }
    }
}

// ── validation ───────────────────────────────────────────────────────

fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    // ASCII mode: the crate is built without regex Unicode tables.
    cell.get_or_init(|| Regex::new(&format!("(?-u){pat}")).expect("memory regex"))
}

/// Why `text` may not enter memory at all, if it may not.
pub(crate) fn secret_problem(text: &str) -> Option<&'static str> {
    static CRED: OnceLock<Regex> = OnceLock::new();
    static TOKEN: OnceLock<Regex> = OnceLock::new();
    static APPROVAL: OnceLock<Regex> = OnceLock::new();
    static RANDOM: OnceLock<Regex> = OnceLock::new();
    if RESERVED.iter().any(|r| text.contains(r)) {
        return Some("contains a reserved memory marker");
    }
    if text.chars().any(|c| (c.is_control() && c != '\n') || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')) {
        return Some("contains control characters");
    }
    if re(&CRED, r"(?i)\b(pass(word|wd|phrase)?|pwd|secret|token|api[_ -]?key|access[_ -]?key|private[_ -]?key|credential|auth)s?\b\s*(is|was|[:=])").is_match(text)
        || text.contains("-----BEGIN")
    {
        return Some("looks like a credential");
    }
    if re(&TOKEN, r"\b(sk-[A-Za-z0-9_-]{12,}|gh[pousr]_[A-Za-z0-9]{20,}|github_pat_\w{20,}|xox[abprs]-[A-Za-z0-9-]{10,}|AKIA[0-9A-Z]{16}|eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,})|(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{12,}").is_match(text) {
        return Some("looks like an access token");
    }
    if re(&RANDOM, r"[A-Za-z0-9+/_=-]{32,}").find_iter(text).any(|m| {
        let s = m.as_str();
        s.chars().any(|c| c.is_ascii_digit()) && s.chars().any(|c| c.is_ascii_alphabetic())
    }) {
        return Some("contains a long random-looking string (possible secret)");
    }
    if re(&APPROVAL, r"(?i)\b(was|were|is|been)\s+(allowed|approved|authori[sz]ed|permitted)\b|\b(allowed|approved)\s+(last time|before|previously)\b|\bpermission (was )?granted\b").is_match(text) {
        return Some("records a past approval; approvals are never remembered");
    }
    None
}

/// Host addresses, account names and user paths: never stored.
pub(crate) fn address_problem(text: &str) -> Option<&'static str> {
    static IP4: OnceLock<Regex> = OnceLock::new();
    static IP6: OnceLock<Regex> = OnceLock::new();
    static USERHOST: OnceLock<Regex> = OnceLock::new();
    static URL: OnceLock<Regex> = OnceLock::new();
    static HOMEPATH: OnceLock<Regex> = OnceLock::new();
    if re(&IP4, r"\b\d{1,3}(\.\d{1,3}){3}\b").is_match(text)
        || re(&IP6, r"(?i)\b[0-9a-f]{1,4}(:[0-9a-f]{0,4}){3,7}\b").is_match(text)
    {
        return Some("contains an IP address");
    }
    if re(&USERHOST, r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)*").is_match(text) {
        return Some("contains a user@host or e-mail address");
    }
    if re(&URL, r"(?i)\b[a-z][a-z0-9+.-]*://").is_match(text) {
        return Some("contains a URL");
    }
    if re(&HOMEPATH, r"(/home/|/Users/|/root/|[A-Za-z]:\\Users\\)").is_match(text) {
        return Some("contains a user-specific path");
    }
    None
}

/// Additional rule for environment entries: no host names either.
fn hostname_problem(text: &str) -> Option<&'static str> {
    static FQDN: OnceLock<Regex> = OnceLock::new();
    re(&FQDN, r"(?i)\b[a-z0-9-]+(\.[a-z0-9-]+)+\.(com|net|org|io|dev|cloud|internal|local|lan|corp|intranet|cn|de|uk|jp|us|eu|app|ai|sh)\b")
        .is_match(text)
        .then_some("contains a host name; store an approved alias instead")
}

pub(crate) fn check_text(text: &str, environment: bool) -> Result<(), String> {
    if let Some(p) = secret_problem(text).or_else(|| address_problem(text)) {
        return Err(p.to_string());
    }
    if environment {
        if let Some(p) = hostname_problem(text) {
            return Err(p.to_string());
        }
    }
    Ok(())
}

fn one_line(s: &str, max: usize) -> Result<String, String> {
    let s = s.trim();
    if s.contains('\n') || s.contains('\r') {
        return Err("must be a single line".into());
    }
    if s.chars().count() > max {
        return Err(format!("longer than {max} characters"));
    }
    Ok(s.to_string())
}

pub(crate) fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !id.starts_with('-')
        && !id.ends_with('-')
}

pub(crate) fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars().flat_map(|c| c.to_lowercase()) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
        if out.len() >= 48 {
            break;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() { "entry".into() } else { out }
}

pub(crate) fn digest(s: &str) -> String {
    ring::digest::digest(&ring::digest::SHA256, s.as_bytes())
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub(crate) fn today() -> NaiveDate {
    Local::now().date_naive()
}

// ── entries ──────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Kind {
    Troubleshooting,
    Method,
    Environment,
}

impl Kind {
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "troubleshooting" => Some(Self::Troubleshooting),
            "method" => Some(Self::Method),
            "environment" => Some(Self::Environment),
            _ => None,
        }
    }
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Troubleshooting => "troubleshooting",
            Self::Method => "method",
            Self::Environment => "environment",
        }
    }
    fn dir(self) -> &'static str {
        match self {
            Self::Troubleshooting => "troubleshooting",
            Self::Method => "methods",
            Self::Environment => "environments",
        }
    }
}

/// Structured, validated content for a new entry. Shared by `remember`
/// and Dream so both pass exactly the same checks.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Draft {
    pub kind: String,
    pub title: String,
    pub aliases: Vec<String>,
    pub projects: Vec<String>,
    pub tags: Vec<String>,
    pub symptom: String,
    pub conditions: String,
    pub fix: String,
    pub verification: String,
    pub skip: String,
    pub purpose: String,
    pub scope: String,
    pub summary: String,
    pub verified: bool,
    pub last_verified: Option<String>,
}

const SECTIONS: &[(&str, &str)] = &[
    ("symptom", "Symptom"),
    ("conditions", "Applies when"),
    ("fix", "Step that worked"),
    ("verification", "How it was verified"),
    ("skip", "Attempts to skip"),
    ("purpose", "Purpose"),
    ("scope", "Where it applies"),
    ("summary", "Summary"),
];

impl Draft {
    fn section(&self, key: &str) -> &str {
        match key {
            "symptom" => &self.symptom,
            "conditions" => &self.conditions,
            "fix" => &self.fix,
            "verification" => &self.verification,
            "skip" => &self.skip,
            "purpose" => &self.purpose,
            "scope" => &self.scope,
            _ => &self.summary,
        }
    }

    /// Validate and normalise. Returns the entry kind.
    pub(crate) fn validate(&mut self) -> Result<Kind, String> {
        let kind = Kind::parse(&self.kind).ok_or("kind must be troubleshooting, method or environment")?;
        let env = kind == Kind::Environment;
        self.title = one_line(&self.title, 120).map_err(|e| format!("title {e}"))?;
        if self.title.is_empty() {
            return Err("title is required".into());
        }
        for (name, list) in [("aliases", &mut self.aliases), ("projects", &mut self.projects), ("tags", &mut self.tags)] {
            if list.len() > 12 {
                return Err(format!("too many {name}"));
            }
            let mut clean = Vec::new();
            for item in list.iter() {
                let v = one_line(item, 60).map_err(|e| format!("{name} item {e}"))?;
                if v.contains(',') {
                    return Err(format!("{name} items may not contain commas"));
                }
                if !v.is_empty() && !clean.contains(&v) {
                    clean.push(v);
                }
            }
            *list = clean;
        }
        for (key, _) in SECTIONS {
            let v = self.section(key);
            if v.chars().count() > 1500 {
                return Err(format!("{key} is longer than 1500 characters"));
            }
        }
        if let Some(d) = &self.last_verified {
            if d != "unknown" && NaiveDate::parse_from_str(d, "%Y-%m-%d").is_err() {
                return Err("last_verified must be YYYY-MM-DD".into());
            }
        }
        match kind {
            Kind::Troubleshooting if self.symptom.trim().is_empty() || self.fix.trim().is_empty() => {
                return Err("troubleshooting entries need symptom and fix".into());
            }
            Kind::Environment if self.purpose.trim().is_empty() || self.aliases.is_empty() => {
                return Err("environment entries need an approved alias and a purpose".into());
            }
            Kind::Method if self.summary.trim().is_empty() => {
                return Err("method entries need a summary".into());
            }
            _ => {}
        }
        check_text(&self.all_text(), env).map_err(|p| format!("not stored: {p}"))?;
        Ok(kind)
    }

    fn all_text(&self) -> String {
        let mut t = vec![self.title.clone(), self.aliases.join(" "), self.projects.join(" "), self.tags.join(" ")];
        for (k, _) in SECTIONS {
            t.push(self.section(k).to_string());
        }
        t.join("\n")
    }

    pub(crate) fn render(&self, id: &str, kind: Kind, sources: &[String], pinned: &str) -> String {
        let status = if kind == Kind::Troubleshooting && !self.verified { "lead" } else { "active" };
        let last_verified = match kind {
            Kind::Method => "n/a".to_string(),
            _ => self.last_verified.clone().unwrap_or_else(|| "unknown".into()),
        };
        let mut s = format!(
            "---\nid: {id}\nkind: {}\ntitle: {}\nstatus: {status}\naliases: {}\nprojects: {}\ntags: {}\npinned: {pinned}\ncreated: {}\nlast_verified: {last_verified}\nsources: {}\n---\n",
            kind.as_str(),
            self.title,
            self.aliases.join(", "),
            self.projects.join(", "),
            self.tags.join(", "),
            today(),
            sources.join(", "),
        );
        for (key, heading) in SECTIONS {
            let v = self.section(key).trim();
            if !v.is_empty() {
                s.push_str(&format!("\n## {heading}\n{v}\n"));
            }
        }
        if status == "lead" {
            s.push_str("\n_Unverified: a lead, not a confirmed fix._\n");
        }
        s
    }
}

/// A parsed library file. Unknown front-matter keys are kept as text in
/// `raw` and never lost, because nanopi only rewrites files it owns.
#[derive(Clone, Debug)]
pub(crate) struct Entry {
    pub id: String,
    pub kind: Kind,
    pub meta: BTreeMap<String, String>,
    pub body: String,
    pub raw: String,
    pub path: PathBuf,
}

impl Entry {
    pub(crate) fn parse(raw: &str, path: PathBuf) -> Option<Self> {
        let rest = raw.strip_prefix("---\n")?;
        let end = rest.find("\n---\n")?;
        let mut meta = BTreeMap::new();
        for line in rest[..end].lines() {
            if let Some((k, v)) = line.split_once(':') {
                meta.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        let id = meta.get("id")?.clone();
        let kind = Kind::parse(meta.get("kind")?)?;
        (valid_id(&id) && path.file_stem().and_then(|s| s.to_str()) == Some(id.as_str())).then(|| Self {
            id,
            kind,
            body: rest[end + 5..].to_string(),
            meta,
            raw: raw.to_string(),
            path,
        })
    }
    pub(crate) fn get(&self, k: &str) -> &str {
        self.meta.get(k).map(String::as_str).unwrap_or("")
    }
    pub(crate) fn list(&self, k: &str) -> Vec<String> {
        self.get(k).split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
    }
    pub(crate) fn title(&self) -> &str {
        self.get("title")
    }
    pub(crate) fn pinned(&self) -> &str {
        match self.get("pinned") {
            "user" => "user",
            "agent" => "agent",
            _ => "none",
        }
    }
    /// Status as presented for recall, including age-based review.
    pub(crate) fn effective_status(&self, state: &State, policy: &Policy) -> String {
        let written = self.get("status");
        let today = today();
        let age = |d: Option<NaiveDate>| d.map(|d| today.signed_duration_since(d).num_days());
        if self.kind == Kind::Environment {
            match age(NaiveDate::parse_from_str(self.get("last_verified"), "%Y-%m-%d").ok()) {
                None => return "needs_review (last verification unknown)".into(),
                Some(a) if a > policy.memory_env_review_days => {
                    return format!("needs_review (not verified for {a} days)");
                }
                _ => {}
            }
        } else if self.pinned() == "none" {
            let used = state
                .recalled
                .get(&self.id)
                .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
                .or_else(|| NaiveDate::parse_from_str(self.get("created"), "%Y-%m-%d").ok());
            if let Some(a) = age(used).filter(|a| *a > policy.memory_review_days) {
                return format!("needs_review (not used for {a} days)");
            }
        }
        if written.is_empty() { "active".into() } else { written.into() }
    }
}

// ── state ────────────────────────────────────────────────────────────

/// nanopi's own bookkeeping. Never holds memory text.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct State {
    /// Content hash of every file nanopi last wrote, by entry id. A file
    /// whose hash differs was edited by the user and is left alone.
    pub hashes: BTreeMap<String, String>,
    /// Last date an entry was recalled for an actual task.
    pub recalled: BTreeMap<String, String>,
    /// Daily-note ids Dream has finished with.
    pub processed: BTreeSet<String>,
    /// Date each forgotten entry went to trash (for the 30-day window).
    pub trashed: BTreeMap<String, String>,
    pub last_run: Option<RunSummary>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct RunSummary {
    pub at: String,
    pub endpoint: String,
    pub model: String,
    pub result: String,
    pub failure: Option<String>,
    pub range: String,
    pub calls: usize,
    pub input_bytes: usize,
    pub notes_processed: usize,
    pub pending: usize,
    pub withheld: usize,
}

// ── filesystem ───────────────────────────────────────────────────────

fn open_opts(o: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let _ = o;
}

pub(crate) fn read_limited(path: &Path, limit: usize) -> io::Result<String> {
    let m = fs::symlink_metadata(path)?;
    if !m.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    let mut o = OpenOptions::new();
    o.read(true);
    open_opts(&mut o);
    let f = o.open(path)?;
    let mut s = String::new();
    f.take(limit as u64 + 1).read_to_string(&mut s)?;
    if s.len() > limit {
        return Err(io::Error::other("file exceeds memory size limit"));
    }
    Ok(s)
}

fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn private_dir(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() || !m.is_dir() => {
            return Err(io::Error::other(format!("{} is not a plain directory", path.display())));
        }
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // Only unix sets a mode on the builder.
            #[cfg_attr(not(unix), allow(unused_mut))]
            let mut b = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                b.mode(0o700);
            }
            b.create(path)?;
        }
        Err(e) => return Err(e),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub(crate) struct Store {
    pub root: PathBuf,
}

const ENTRY_LIMIT: usize = 32 * 1024;
const DAILY_LIMIT: usize = 512 * 1024;

impl Store {
    pub(crate) fn active() -> Result<Self> {
        Ok(Self::at(&crate::paths::nanopi_home().context("cannot resolve the nanopi home directory")?))
    }
    pub(crate) fn at(home: &Path) -> Self {
        Self { root: home.join("memory") }
    }

    fn init(&self) -> Result<()> {
        if let Some(parent) = self.root.parent() {
            ensure!(parent.is_dir(), "nanopi home {} does not exist", parent.display());
        }
        private_dir(&self.root)?;
        for d in ["daily", "library", "library/environments", "library/troubleshooting", "library/methods", "archive", "trash"] {
            private_dir(&self.root.join(d))?;
        }
        Ok(())
    }

    /// Short-lived exclusive lock for every mutation.
    pub(crate) fn lock(&self, name: &str) -> Result<File> {
        let f = self.open_lock(name)?;
        f.lock()?;
        Ok(f)
    }

    fn try_lock(&self, name: &str) -> Result<File> {
        let f = self.open_lock(name)?;
        f.try_lock()?;
        Ok(f)
    }

    fn open_lock(&self, name: &str) -> Result<File> {
        self.init()?;
        let mut o = OpenOptions::new();
        o.read(true).write(true).create(true).truncate(false);
        open_opts(&mut o);
        Ok(o.open(self.root.join(name))?)
    }

    /// Write a file that must not already exist.
    fn create_new(&self, path: &Path, content: &str) -> Result<()> {
        let mut o = OpenOptions::new();
        o.write(true).create_new(true);
        open_opts(&mut o);
        let mut f = o.open(path).with_context(|| format!("create {}", path.display()))?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
        Ok(())
    }

    /// Replace a nanopi-owned file atomically. Rename never follows a
    /// symlink at the target.
    fn replace(&self, path: &Path, content: &str) -> Result<()> {
        let dir = path.parent().context("no parent")?;
        let tmp = dir.join(format!(".tmp-{}", uuid::Uuid::now_v7()));
        self.create_new(&tmp, content)?;
        fs::rename(&tmp, path).inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })?;
        Ok(())
    }

    pub(crate) fn state(&self) -> State {
        read_limited(&self.root.join("state.json"), 4 * 1024 * 1024)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }
    pub(crate) fn save_state(&self, st: &State) -> Result<()> {
        self.replace(&self.root.join("state.json"), &serde_json::to_string_pretty(st)?)
    }

    pub(crate) fn forgotten(&self) -> BTreeSet<String> {
        read_limited(&self.root.join(".forgotten"), 4 * 1024 * 1024)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .filter(|id| valid_id(id))
            .map(str::to_string)
            .collect()
    }
    fn add_forgotten(&self, id: &str) -> Result<()> {
        if self.forgotten().contains(id) {
            return Ok(());
        }
        let mut o = OpenOptions::new();
        o.append(true).create(true);
        open_opts(&mut o);
        let mut f = o.open(self.root.join(".forgotten"))?;
        writeln!(f, "{id} {}", today())?;
        Ok(())
    }
    fn remove_forgotten(&self, id: &str) -> Result<()> {
        let keep: Vec<String> = read_limited(&self.root.join(".forgotten"), 4 * 1024 * 1024)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.split_whitespace().next() != Some(id))
            .map(str::to_string)
            .collect();
        self.replace(&self.root.join(".forgotten"), &(keep.join("\n") + if keep.is_empty() { "" } else { "\n" }))
    }

    fn scan(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten().take(10_000) {
            let p = e.path();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() && !ft.is_symlink() {
                Self::scan(&p, out);
            } else if ft.is_file() && p.extension().is_some_and(|x| x == "md") {
                out.push(p);
            }
        }
    }

    /// Entries in `library/` (or `archive/` / `trash/`).
    pub(crate) fn entries(&self, area: &str) -> Vec<Entry> {
        let mut paths = Vec::new();
        Self::scan(&self.root.join(area), &mut paths);
        paths.sort();
        paths
            .into_iter()
            .filter_map(|p| read_limited(&p, ENTRY_LIMIT).ok().and_then(|raw| Entry::parse(&raw, p)))
            .collect()
    }

    pub(crate) fn find(&self, id: &str) -> Option<(String, Entry)> {
        ["library", "archive", "trash"]
            .into_iter()
            .find_map(|a| self.entries(a).into_iter().find(|e| e.id == id).map(|e| (a.to_string(), e)))
    }

    /// Whether nanopi may still modify this entry (it is unchanged since
    /// nanopi last wrote it).
    pub(crate) fn owned(&self, st: &State, e: &Entry) -> bool {
        st.hashes.get(&e.id).is_some_and(|h| *h == digest(&e.raw))
    }

    /// An id that is free: not in use anywhere and never forgotten or
    /// deleted by the user.
    fn id_free(&self, st: &State, id: &str) -> bool {
        valid_id(id) && !st.hashes.contains_key(id) && !self.forgotten().contains(id) && self.find(id).is_none()
    }
    pub(crate) fn fresh_id(&self, st: &State, base: &str) -> String {
        let base = slug(base);
        if self.id_free(st, &base) {
            return base;
        }
        (2..1000).map(|n| format!("{base}-{n}")).find(|c| self.id_free(st, c)).unwrap_or_else(|| format!("{base}-{}", uuid::Uuid::now_v7().simple()))
    }

    /// Create a library entry. Caller holds the lock.
    pub(crate) fn create_entry(&self, st: &mut State, id: &str, kind: Kind, text: &str) -> Result<PathBuf> {
        ensure!(self.id_free(st, id), "memory id `{id}` is taken or was forgotten");
        let path = self.root.join("library").join(kind.dir()).join(format!("{id}.md"));
        self.create_new(&path, text)?;
        st.hashes.insert(id.to_string(), digest(text));
        Ok(path)
    }

    /// Rewrite an entry nanopi owns. Refuses user-edited files.
    pub(crate) fn rewrite_entry(&self, st: &mut State, e: &Entry, text: &str) -> Result<()> {
        ensure!(self.owned(st, e), "`{}` was edited by the user; left unchanged", e.id);
        let now = read_limited(&e.path, ENTRY_LIMIT)?;
        ensure!(now == e.raw, "`{}` changed on disk; left unchanged", e.id);
        self.replace(&e.path, text)?;
        st.hashes.insert(e.id.clone(), digest(text));
        Ok(())
    }

    /// Move an entry between areas. Never overwrites.
    fn move_entry(&self, e: &Entry, area: &str) -> Result<PathBuf> {
        let dest_dir = match area {
            "library" => self.root.join("library").join(e.kind.dir()),
            a => self.root.join(a),
        };
        let dest = dest_dir.join(format!("{}.md", e.id));
        ensure!(!exists(&dest), "{} already exists; not overwritten", dest.display());
        self.create_new(&dest, &e.raw)?;
        fs::remove_file(&e.path)?;
        Ok(dest)
    }

    /// Forget: out of recall immediately, body to trash for 30 days, an
    /// ID-only marker kept for good. Caller holds the lock.
    pub(crate) fn forget_locked(&self, st: &mut State, id: &str) -> Result<()> {
        let e = self.entries("library").into_iter().chain(self.entries("archive")).find(|e| e.id == id).with_context(|| format!("no library or archived entry `{id}`"))?;
        self.add_forgotten(id)?;
        let owned = self.owned(st, &e);
        let moved = self.move_entry(&e, "trash")?;
        // The trash date lives in the state, not in the file, so restore
        // moves the original bytes back unchanged.
        if owned {
            st.hashes.insert(id.to_string(), digest(&read_limited(&moved, ENTRY_LIMIT)?));
        }
        st.recalled.remove(id);
        st.trashed.insert(id.to_string(), today().to_string());
        Ok(())
    }

    pub(crate) fn archive_locked(&self, st: &mut State, id: &str) -> Result<()> {
        let e = self.entries("library").into_iter().find(|e| e.id == id).with_context(|| format!("no library entry `{id}`"))?;
        let owned = self.owned(st, &e);
        let moved = self.move_entry(&e, "archive")?;
        if owned {
            st.hashes.insert(id.to_string(), digest(&read_limited(&moved, ENTRY_LIMIT)?));
        }
        Ok(())
    }

    /// Restore an archived entry, or a forgotten one still in its window.
    pub(crate) fn restore(&self, id: &str) -> Result<String> {
        let _g = self.lock(".lock")?;
        let mut st = self.state();
        let (area, e) = self.find(id).with_context(|| format!("no archived or recently forgotten entry `{id}` (purged entries cannot be restored)"))?;
        match area.as_str() {
            "library" => bail!("`{id}` is already active"),
            "trash" => {
                ensure!(!trash_expired(&st, id), "`{id}` is past its 30-day recovery window");
                self.move_entry(&e, "library")?;
                self.remove_forgotten(id)?;
                st.trashed.remove(id);
            }
            _ => {
                self.move_entry(&e, "library")?;
            }
        }
        self.save_state(&st)?;
        self.write_index(&st)?;
        Ok(format!("restored `{id}` to the library"))
    }

    /// Entries the user deleted by hand: known to nanopi, gone from every
    /// area. They get a forget marker so Dream never recreates them.
    pub(crate) fn record_user_deletions(&self, st: &mut State) -> Result<Vec<String>> {
        let present: BTreeSet<String> = ["library", "archive", "trash"].iter().flat_map(|a| self.entries(a)).map(|e| e.id).collect();
        let gone: Vec<String> = st.hashes.keys().filter(|id| !present.contains(*id)).cloned().collect();
        for id in &gone {
            self.add_forgotten(id)?;
        }
        Ok(gone)
    }

    /// Purge trash bodies older than 30 days, keeping the marker.
    pub(crate) fn purge_trash(&self, st: &mut State) -> Result<Vec<String>> {
        let mut purged = Vec::new();
        for e in self.entries("trash") {
            if trash_expired(st, &e.id) {
                fs::remove_file(&e.path)?;
                st.trashed.remove(&e.id);
                purged.push(e.id);
            }
        }
        Ok(purged)
    }

    /// Regenerate `MEMORY.md`: the active library index.
    pub(crate) fn write_index(&self, st: &State) -> Result<()> {
        let policy = Policy::active();
        let mut s = String::from("# Memory index\n\nGenerated by nanopi from `library/`. Edit entries, not this file.\n\n");
        for e in self.entries("library") {
            s.push_str(&format!(
                "- `{}` · {} · {} · {}{}\n",
                e.id,
                e.kind.as_str(),
                e.effective_status(st, &policy),
                e.title(),
                if e.get("aliases").is_empty() { String::new() } else { format!(" (aka {})", e.get("aliases")) }
            ));
        }
        self.replace(&self.root.join("MEMORY.md"), &s)
    }

    // ── daily notes ──

    pub(crate) fn append_daily(&self, project: &str, note: &DailyNote) -> Result<String> {
        let date = today();
        let dir = self.root.join("daily").join(date.to_string());
        private_dir(&dir)?;
        let file = dir.join(format!("{}.md", slug(project)));
        let mut o = OpenOptions::new();
        o.append(true).create(true);
        open_opts(&mut o);
        let mut f = o.open(&file)?;
        ensure!(f.metadata()?.is_file(), "daily note is not a regular file");
        let id = uuid::Uuid::now_v7().to_string();
        f.write_all(note.render(&id).as_bytes())?;
        Ok(id)
    }

    /// Every note in `daily/`, oldest date first.
    pub(crate) fn daily_notes(&self) -> Vec<ParsedNote> {
        let mut out = Vec::new();
        let Ok(rd) = fs::read_dir(self.root.join("daily")) else { return out };
        let mut dates: Vec<(NaiveDate, PathBuf)> = rd
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|e| Some((NaiveDate::parse_from_str(e.file_name().to_str()?, "%Y-%m-%d").ok()?, e.path())))
            .collect();
        dates.sort();
        for (date, dir) in dates {
            let mut files: Vec<PathBuf> = fs::read_dir(&dir).into_iter().flatten().flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_file())).map(|e| e.path()).collect();
            files.sort();
            for f in files {
                let project = f.file_stem().and_then(|s| s.to_str()).unwrap_or("general").to_string();
                if let Ok(text) = read_limited(&f, DAILY_LIMIT) {
                    out.extend(ParsedNote::split(&text, date, &project));
                }
            }
        }
        out
    }
}

fn trash_expired(st: &State, id: &str) -> bool {
    st.trashed
        .get(id)
        .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
        // A trashed body with no recorded date counts as expired: fail
        // toward purging the body (the marker stays).
        .is_none_or(|d| today().signed_duration_since(d).num_days() >= 30)
}

// ── daily notes ──────────────────────────────────────────────────────

pub(crate) struct DailyNote {
    pub kind: Kind,
    pub title: String,
    pub id_hint: String,
    pub body: String,
}

impl DailyNote {
    fn render(&self, id: &str) -> String {
        format!(
            "\n<!-- note:{id} kind:{} id-hint:{} -->\n### {} · {}\n{}\n",
            self.kind.as_str(),
            self.id_hint,
            Local::now().format("%H:%M"),
            self.title,
            self.body.trim()
        )
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ParsedNote {
    pub id: String,
    pub date: NaiveDate,
    pub project: String,
    pub kind: Kind,
    pub id_hint: String,
    pub text: String,
}

impl ParsedNote {
    fn split(text: &str, date: NaiveDate, project: &str) -> Vec<Self> {
        static HEAD: OnceLock<Regex> = OnceLock::new();
        let head = re(&HEAD, r"(?m)^<!-- note:([0-9a-f-]{36}) kind:([a-z]+) id-hint:([a-z0-9-]*) -->");
        let caps: Vec<_> = head.captures_iter(text).collect();
        let mut out = Vec::new();
        for (i, c) in caps.iter().enumerate() {
            let m = c.get(0).unwrap();
            let end = caps.get(i + 1).map(|n| n.get(0).unwrap().start()).unwrap_or(text.len());
            // Unknown kinds are treated as environment: the strictest
            // disclosure class.
            let kind = Kind::parse(&c[2]).unwrap_or(Kind::Environment);
            out.push(Self {
                id: c[1].to_string(),
                date,
                project: project.to_string(),
                kind,
                id_hint: c[3].to_string(),
                text: text[m.end()..end].trim().to_string(),
            });
        }
        out
    }
}

/// Project tag for the current directory.
pub(crate) fn project_of(cwd: &Path) -> String {
    let name = cwd.file_name().and_then(|s| s.to_str()).unwrap_or("general");
    let s = slug(name);
    if check_text(&s, true).is_ok() { s } else { "general".into() }
}

// ── search ───────────────────────────────────────────────────────────

const STOP: &[&str] = &[
    "the", "a", "an", "to", "and", "or", "of", "in", "on", "for", "with", "like", "ago", "days", "day", "two", "three", "we", "used", "use",
    "last", "time", "it", "this", "that", "is", "was", "how", "do", "i", "my", "me", "again", "please", "can", "you", "what", "when", "did",
    "before", "same", "as", "at", "be", "from", "by", "our", "us", "run", "get", "got", "fix", "help", "make", "some", "there", "here", "just",
];

const SYNONYMS: &[&[&str]] = &[
    &["box", "server", "machine", "host", "environment", "env", "vm", "node"],
    &["test", "tests", "testing", "ci"],
    &["remote", "remotely"],
    &["fail", "fails", "failed", "failing", "failure", "error", "errors", "broken", "breaks"],
    &["build", "builds", "building", "compile", "compiles", "compiling"],
    &["cache", "cached", "caching", "stale"],
    &["slow", "slowly", "hang", "hangs", "hanging", "timeout", "stuck"],
];

fn tokens(s: &str) -> BTreeSet<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 2 && !STOP.contains(w))
        .map(|w| w.trim_end_matches("ing").to_string())
        .map(|w| if w.len() > 3 && w.ends_with('s') && !w.ends_with("ss") { w[..w.len() - 1].to_string() } else { w })
        .collect()
}

fn concept(w: &str) -> String {
    for group in SYNONYMS {
        if group.iter().any(|g| tokens(g).contains(w) || *g == w) {
            return group[0].to_string();
        }
    }
    w.to_string()
}

pub(crate) struct Hit {
    pub score: u32,
    pub entry: Entry,
}

/// Rank active library entries for `query`. Needs at least two distinct
/// matching concepts (or one in a title/alias plus one elsewhere), so an
/// unrelated task loads nothing.
pub(crate) fn search(store: &Store, query: &str) -> Vec<Hit> {
    let q: BTreeSet<String> = tokens(query).iter().map(|w| concept(w)).collect();
    if q.is_empty() {
        return Vec::new();
    }
    let mut hits = Vec::new();
    for e in store.entries("library") {
        let fields = [
            (4u32, format!("{} {}", e.get("aliases"), e.title())),
            (2, format!("{} {}", e.get("projects"), e.get("tags"))),
            (1, e.body.clone()),
        ];
        let mut score = 0;
        let mut matched = BTreeSet::new();
        for (w, text) in &fields {
            let c: BTreeSet<String> = tokens(text).iter().map(|t| concept(t)).collect();
            for t in q.intersection(&c) {
                score += w;
                matched.insert(t.clone());
            }
        }
        if matched.len() >= 2 && score >= 5 {
            hits.push(Hit { score, entry: e });
        }
    }
    hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.entry.id.cmp(&b.entry.id)));
    hits.truncate(3);
    hits
}

// ── user commands ────────────────────────────────────────────────────

pub(crate) const MEMORY_USAGE: &str =
    "Usage: /memory [show <id> | forget <id> | restore <id> | disclosure [general|full]]";

/// `/memory …`. Local only; never reaches a model. `endpoint` is the
/// active base URL, needed to bind `disclosure full` to it.
pub(crate) fn command(args: &str, endpoint: &str) -> Result<String> {
    let store = Store::active()?;
    let mut parts = args.split_whitespace();
    let verb = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("");
    ensure!(parts.next().is_none(), "{MEMORY_USAGE}");
    let policy = Policy::active();
    match verb {
        "" => {
            let st = store.state();
            let lib = store.entries("library");
            let mut out = vec![format!(
                "memory: {} active · {} archived · {} in trash · disclosure for this endpoint: {}",
                lib.len(),
                store.entries("archive").len(),
                store.entries("trash").len(),
                if policy.full_for(endpoint) { "full" } else { "general" }
            )];
            out.push(format!("store: {}", store.root.display()));
            let mut recent = lib.clone();
            recent.sort_by(|a, b| b.get("created").cmp(a.get("created")).then_with(|| a.id.cmp(&b.id)));
            for e in recent.iter().take(30) {
                out.push(format!("  {} · {} · {} · {} · {}", e.get("created"), e.id, e.kind.as_str(), e.effective_status(&st, &policy), e.title()));
            }
            if lib.is_empty() {
                out.push("  (no library entries yet — ask nanopi to remember something, or run /dream)".into());
            }
            let pending = store.daily_notes().iter().filter(|n| !st.processed.contains(&n.id)).count();
            out.push(format!("daily notes waiting for /dream: {pending}"));
            Ok(out.join("\n"))
        }
        "show" => {
            ensure!(valid_id(arg), "{MEMORY_USAGE}");
            let st = store.state();
            if store.forgotten().contains(arg) && store.find(arg).is_none() {
                return Ok(format!("`{arg}` was forgotten and purged; only its ID marker remains"));
            }
            let (area, e) = store.find(arg).with_context(|| format!("no entry `{arg}`"))?;
            let status = match area.as_str() {
                "trash" => "forgotten (in trash; /memory restore within 30 days)".to_string(),
                "archive" => "archived".to_string(),
                _ => e.effective_status(&st, &policy),
            };
            Ok(format!(
                "{} · {} · {}\nstatus: {status}\npinned: {}\nsources: {}\nedited by you: {}\nfile: {}\n\n{}",
                e.id,
                e.kind.as_str(),
                e.title(),
                e.pinned(),
                e.get("sources"),
                if store.owned(&st, &e) { "no" } else { "yes (nanopi will not modify it)" },
                e.path.display(),
                e.body.trim()
            ))
        }
        "forget" => {
            ensure!(valid_id(arg), "{MEMORY_USAGE}");
            let _g = store.lock(".lock")?;
            let mut st = store.state();
            store.forget_locked(&mut st, arg)?;
            store.save_state(&st)?;
            store.write_index(&st)?;
            Ok(format!("forgot `{arg}`: out of recall now; recoverable for 30 days with /memory restore {arg}"))
        }
        "restore" => {
            ensure!(valid_id(arg), "{MEMORY_USAGE}");
            store.restore(arg)
        }
        "disclosure" => match arg {
            "" => Ok(format!(
                "memory disclosure for this profile: {} (applies to {})\n\
                 general: only general methods are sent; environment entries stay local.\n\
                 full: environment entries may be sent to this exact endpoint.\n\
                 Set with /memory disclosure general|full",
                if policy.full_for(endpoint) { "full" } else { "general" },
                endpoint
            )),
            "general" | "full" => {
                set_disclosure(arg, endpoint)?;
                Ok(format!("memory disclosure set to {arg} for this profile ({endpoint})"))
            }
            _ => bail!("{MEMORY_USAGE}"),
        },
        _ => bail!("{MEMORY_USAGE}"),
    }
}

fn set_disclosure(value: &str, endpoint: &str) -> Result<()> {
    let path = crate::profile::current_config_path().context("cannot resolve the profile config")?;
    let text = match read_limited(&path, 256 * 1024) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let mut doc: toml_edit::DocumentMut = text.parse().context("profile config is not valid TOML; not changed")?;
    doc["memory_disclosure"] = toml_edit::value(value);
    if value == "full" {
        doc["memory_full_endpoint"] = toml_edit::value(endpoint);
    } else {
        doc.remove("memory_full_endpoint");
    }
    let tmp = path.with_extension(format!("tmp-{}", uuid::Uuid::now_v7().simple()));
    let mut o = OpenOptions::new();
    o.write(true).create_new(true);
    open_opts(&mut o);
    o.open(&tmp)?.write_all(doc.to_string().as_bytes())?;
    fs::rename(&tmp, &path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })?;
    Ok(())
}

/// The few lines of system prompt about memory (Q31).
pub(crate) const PROMPT: &str = "\n\n# Memory\nYou have a personal memory of past work (resources such as test environments, and troubleshooting that worked). \
Call `memory_search` only when the task points back at earlier work: the user refers to it (\"last time\", \"again\", \"like before\", \"the server we used\"), \
or you meet a resource or a failure that looks like one you have handled before. Search at most once per task, with a short query of the \
resource or symptom (not IDs or dates); if nothing matches, carry on without memory. \
A memory is a lead, not authority: say briefly what you found, then check it still applies before relying on it. \
When the user asks you to remember something, or a hard problem is solved and verified, call `remember`.";

/// [`PROMPT`] while the library is empty: nothing to search yet.
const PROMPT_EMPTY: &str = "\n\n# Memory\nYou have a personal memory of past work; it holds nothing yet. \
When the user asks you to remember something, or a hard problem is solved and verified, call `remember`.";

/// While the library holds no entry, searching can only come back empty:
/// drop `memory_search` and the instruction to use it from a request.
/// `None` when there is nothing to change.
pub(crate) fn hide_search_when_empty(ctx: &crate::agent::context::Context) -> Option<crate::agent::context::Context> {
    if !ctx.tools.iter().any(|t| t.name == "memory_search") {
        return None;
    }
    let store = Store::active().ok()?;
    let mut found = Vec::new();
    Store::scan(&store.root.join("library"), &mut found);
    if !found.is_empty() {
        return None;
    }
    let mut c = ctx.clone();
    c.tools.retain(|t| t.name != "memory_search");
    if let Some(sys) = c.system.as_mut() {
        *sys = sys.replace(PROMPT, PROMPT_EMPTY);
    }
    Some(c)
}

#[cfg(test)]
mod tests;
