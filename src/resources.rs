//! Skills + prompt templates loader.
//!
//! v0.9: PI-parity skill loader. Mirrors
//! `pi/packages/coding-agent/src/core/skills.ts`:
//! - directory containing `SKILL.md` → skill root, no further recursion
//! - otherwise root-level `*.md` are picked up (only at top level)
//! - subdirectories are recursed looking for `SKILL.md`
//! - name/description validated per Agent Skills spec (warnings, not errors,
//!   except missing description which drops the skill)
//! - collisions keep the first-loaded, warn on the loser
//! - symlinks followed; dedup by canonical path
//!
//! `.gitignore`/`.ignore`/`.fdignore` awareness is intentionally NOT
//! carried over — the `ignore` crate would balloon the musl binary
//! for a rarely-exercised code path. Track as v0.10 candidate.
//!
//! Prompt-template loading is unchanged from v0.5 (not in v0.9 scope).
//!
//! v0.9.2: frontmatter is parsed by a hand-rolled line reader, not
//! `serde_yaml`. Agent Skills fields are all flat scalars, and strict
//! YAML rejects the very common `description: something: with colons`
//! pattern that PI / Claude Code accept in the wild. See
//! [`parse_flat_frontmatter`] for the exact grammar.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Where the skill was discovered — used for autocomplete labels and
/// diagnostic messages. Cli beats Project beats User in collisions
/// only if that order is what the caller passes to `load_skills` (the
/// helper preserves insertion order and warns on later duplicates).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillSource {
    User,
    Project,
    Cli,
}

impl SkillSource {
    pub fn label(&self) -> &'static str {
        match self {
            SkillSource::User => "user",
            SkillSource::Project => "project",
            SkillSource::Cli => "cli",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// Absolute path to the SKILL.md (or flat `.md`) file.
    pub file_path: PathBuf,
    /// Directory containing `file_path`. Relative paths in the SKILL.md
    /// body resolve against this.
    pub base_dir: PathBuf,
    pub source: SkillSource,
    /// When true, the skill is hidden from the system prompt. It can
    /// still be invoked explicitly via `/skill:name`.
    pub disable_model_invocation: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticLevel {
    Warning,
    Collision,
}

#[derive(Debug, Clone)]
pub struct SkillDiagnostic {
    pub level: DiagnosticLevel,
    pub message: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct LoadSkillsResult {
    pub skills: Vec<Skill>,
    pub diagnostics: Vec<SkillDiagnostic>,
}

// -- skills --

const MAX_NAME_LENGTH: usize = 64;
const MAX_DESCRIPTION_LENGTH: usize = 1024;

/// Load skills from a single directory. Missing dir → empty result.
///
/// Recursively walks per PI's rules (SKILL.md root stops recursion;
/// flat `.md` accepted only at the top level; skip dotdirs and
/// `node_modules`; follow symlinks).
pub fn load_skills_from_dir(dir: &Path, source: SkillSource) -> LoadSkillsResult {
    load_from_dir_inner(dir, source, true)
}

fn load_from_dir_inner(
    dir: &Path,
    source: SkillSource,
    include_root_files: bool,
) -> LoadSkillsResult {
    let mut out = LoadSkillsResult::default();
    if !dir.exists() {
        return out;
    }

    let entries: Vec<_> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.flatten().collect(),
        Err(_) => return out,
    };

    // First pass: if this dir has SKILL.md, load it and stop.
    for entry in &entries {
        let name = entry.file_name();
        if name != "SKILL.md" {
            continue;
        }
        let full = entry.path();
        let meta = match std::fs::metadata(&full) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.is_file() {
            continue;
        }
        parse_and_push(&full, source, &mut out);
        return out;
    }

    // Second pass: recurse into subdirs; also accept root-level .md
    // files if the caller allowed it (top-level scan only).
    for entry in &entries {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') || name_str == "node_modules" {
            continue;
        }
        let full = entry.path();
        let meta = match std::fs::metadata(&full) {
            Ok(m) => m,
            Err(_) => continue, // broken symlink or perms issue
        };
        if meta.is_dir() {
            let sub = load_from_dir_inner(&full, source, false);
            out.skills.extend(sub.skills);
            out.diagnostics.extend(sub.diagnostics);
            continue;
        }
        if !meta.is_file() || !include_root_files {
            continue;
        }
        if full.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        parse_and_push(&full, source, &mut out);
    }

    out
}

fn parse_and_push(path: &Path, source: SkillSource, out: &mut LoadSkillsResult) {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            out.diagnostics.push(SkillDiagnostic {
                level: DiagnosticLevel::Warning,
                message: format!("failed to read skill file: {e}"),
                path: path.to_path_buf(),
            });
            return;
        }
    };
    let (fm_text, _) = split_frontmatter(&content);
    let fm = parse_flat_frontmatter(&fm_text);

    let base_dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    // description is hard-required
    let description = match fm.get("description") {
        Some(d) if !d.trim().is_empty() => d.clone(),
        _ => {
            out.diagnostics.push(SkillDiagnostic {
                level: DiagnosticLevel::Warning,
                message: "description is required".into(),
                path: path.to_path_buf(),
            });
            return;
        }
    };
    if description.chars().count() > MAX_DESCRIPTION_LENGTH {
        out.diagnostics.push(SkillDiagnostic {
            level: DiagnosticLevel::Warning,
            message: format!(
                "description exceeds {MAX_DESCRIPTION_LENGTH} characters ({})",
                description.chars().count()
            ),
            path: path.to_path_buf(),
        });
    }

    // name falls back to parent dir name (matches PI behavior)
    let parent_dir_name = base_dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    let name = fm
        .get("name")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or(parent_dir_name);

    for msg in validate_name(&name) {
        out.diagnostics.push(SkillDiagnostic {
            level: DiagnosticLevel::Warning,
            message: msg,
            path: path.to_path_buf(),
        });
    }

    let disable_model_invocation = fm
        .get("disable-model-invocation")
        .map(|v| v == "true")
        .unwrap_or(false);

    out.skills.push(Skill {
        name,
        description,
        file_path: path.to_path_buf(),
        base_dir,
        source,
        disable_model_invocation,
    });
}

/// Line-based frontmatter reader: extracts flat `key: value` pairs and
/// returns them as a map. Deliberately more permissive than strict YAML
/// so an unquoted `description: Foo: bar` yields `Foo: bar` (splits on
/// the FIRST `:` per line). Mirrors what PI / Claude Code effectively
/// accept from the wild-caught SKILL.md files on their skill hubs,
/// where descriptions with `:` are common and rarely quoted.
///
/// Supports:
/// - blank lines and `#` comments (skipped)
/// - `key: value` where value runs to end-of-line
/// - surrounding single or double quotes on the value (stripped)
///
/// Not supported (returns pair verbatim / doesn't parse):
/// - block scalars (`|`, `>`), sequences, nested maps.
/// The Agent Skills spec fields (`name`, `description`,
/// `disable-model-invocation`) are all flat scalars, so this covers
/// every real-world SKILL.md we care about.
fn parse_flat_frontmatter(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(colon) = line.find(':') else {
            continue;
        };
        let key = line[..colon].trim();
        if key.is_empty() {
            continue;
        }
        let raw = line[colon + 1..].trim();
        let value = strip_wrap_quotes(raw);
        out.insert(key.to_string(), value.to_string());
    }
    out
}

fn strip_wrap_quotes(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

fn validate_name(name: &str) -> Vec<String> {
    let mut errs = Vec::new();
    if name.is_empty() {
        errs.push("name is required".into());
        return errs;
    }
    let len = name.chars().count();
    if len > MAX_NAME_LENGTH {
        errs.push(format!("name exceeds {MAX_NAME_LENGTH} characters ({len})"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        errs.push(
            "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)".into(),
        );
    }
    if name.starts_with('-') || name.ends_with('-') {
        errs.push("name must not start or end with a hyphen".into());
    }
    if name.contains("--") {
        errs.push("name must not contain consecutive hyphens".into());
    }
    errs
}

/// Options passed to [`load_skills`].
#[derive(Debug, Default)]
pub struct LoadSkillsOptions {
    /// User-scope dir (e.g. `~/.nanopi/skills`). None → skipped.
    pub user_dir: Option<PathBuf>,
    /// Project-scope dir (e.g. `<cwd>/.nanopi/skills`). Callers should
    /// pass `None` when the project is distrusted.
    pub project_dir: Option<PathBuf>,
    /// Explicit `--skill <path>` entries (files or directories).
    /// Additive: still loaded even when discovery is otherwise disabled.
    pub cli_paths: Vec<PathBuf>,
    /// If true, skip user + project discovery. `cli_paths` still load.
    pub no_discovery: bool,
    /// Names to explicitly hide from load results (e.g. from config).
    pub disabled: Vec<String>,
}

/// Load skills from all configured locations. Order: user → project →
/// cli. Collisions keep the first-loaded; the loser gets a warning.
pub fn load_skills(opts: LoadSkillsOptions) -> LoadSkillsResult {
    let mut acc = LoadSkillsResult::default();
    let mut seen_names: HashMap<String, PathBuf> = HashMap::new();
    let mut seen_real_paths: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    let disabled: std::collections::HashSet<String> = opts.disabled.into_iter().collect();

    if !opts.no_discovery {
        if let Some(dir) = &opts.user_dir {
            merge_batch(
                &mut acc,
                &mut seen_names,
                &mut seen_real_paths,
                &disabled,
                load_skills_from_dir(dir, SkillSource::User),
            );
        }
        if let Some(dir) = &opts.project_dir {
            merge_batch(
                &mut acc,
                &mut seen_names,
                &mut seen_real_paths,
                &disabled,
                load_skills_from_dir(dir, SkillSource::Project),
            );
        }
    }

    for raw in opts.cli_paths {
        if !raw.exists() {
            acc.diagnostics.push(SkillDiagnostic {
                level: DiagnosticLevel::Warning,
                message: "skill path does not exist".into(),
                path: raw.clone(),
            });
            continue;
        }
        let md = match std::fs::metadata(&raw) {
            Ok(m) => m,
            Err(e) => {
                acc.diagnostics.push(SkillDiagnostic {
                    level: DiagnosticLevel::Warning,
                    message: format!("failed to stat skill path: {e}"),
                    path: raw.clone(),
                });
                continue;
            }
        };
        if md.is_dir() {
            merge_batch(
                &mut acc,
                &mut seen_names,
                &mut seen_real_paths,
                &disabled,
                load_skills_from_dir(&raw, SkillSource::Cli),
            );
        } else if md.is_file() && raw.extension().and_then(|s| s.to_str()) == Some("md") {
            let mut one = LoadSkillsResult::default();
            parse_and_push(&raw, SkillSource::Cli, &mut one);
            merge_batch(
                &mut acc,
                &mut seen_names,
                &mut seen_real_paths,
                &disabled,
                one,
            );
        } else {
            acc.diagnostics.push(SkillDiagnostic {
                level: DiagnosticLevel::Warning,
                message: "skill path is not a markdown file".into(),
                path: raw,
            });
        }
    }

    acc
}

fn merge_batch(
    acc: &mut LoadSkillsResult,
    seen_names: &mut HashMap<String, PathBuf>,
    seen_real_paths: &mut std::collections::HashSet<PathBuf>,
    disabled: &std::collections::HashSet<String>,
    batch: LoadSkillsResult,
) {
    acc.diagnostics.extend(batch.diagnostics);
    for skill in batch.skills {
        if disabled.contains(&skill.name) {
            continue;
        }
        let real =
            std::fs::canonicalize(&skill.file_path).unwrap_or_else(|_| skill.file_path.clone());
        if !seen_real_paths.insert(real) {
            continue;
        }
        if let Some(existing) = seen_names.get(&skill.name) {
            acc.diagnostics.push(SkillDiagnostic {
                level: DiagnosticLevel::Collision,
                message: format!(
                    "skill name \"{}\" already loaded from {}",
                    skill.name,
                    existing.display()
                ),
                path: skill.file_path.clone(),
            });
            continue;
        }
        seen_names.insert(skill.name.clone(), skill.file_path.clone());
        acc.skills.push(skill);
    }
}

// -- system-prompt injection --

/// Format all visible skills as an `<available_skills>` block to
/// inject into the system prompt. Skills with
/// `disable_model_invocation = true` are hidden.
pub fn format_skills_for_prompt(skills: &[Skill]) -> String {
    let visible: Vec<&Skill> = skills
        .iter()
        .filter(|s| !s.disable_model_invocation)
        .collect();
    if visible.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "\n\nThe following skills provide specialized instructions for specific tasks.\n\
         Use the read tool to load a skill's file when the task matches its description.\n\
         When a skill file references a relative path, resolve it against the skill directory \
         (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.\n\n\
         <available_skills>\n",
    );
    for s in visible {
        out.push_str("  <skill>\n");
        out.push_str(&format!("    <name>{}</name>\n", xml_escape(&s.name)));
        out.push_str(&format!(
            "    <description>{}</description>\n",
            xml_escape(&s.description)
        ));
        out.push_str(&format!(
            "    <location>{}</location>\n",
            xml_escape(&s.file_path.display().to_string())
        ));
        out.push_str("  </skill>\n");
    }
    out.push_str("</available_skills>");
    out
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

// -- slash-command expansion --

/// Result of expanding `/skill:name [args]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpandedSkill {
    pub name: String,
    pub location: PathBuf,
    pub base_dir: PathBuf,
    /// SKILL.md body with frontmatter stripped, trimmed.
    pub body: String,
    /// Extra user text following the command, if any.
    pub user_args: Option<String>,
    /// The full string that should replace the raw `/skill:` input in
    /// the outgoing user message.
    pub expanded_text: String,
}

/// If `text` starts with `/skill:name`, look up `name` in `skills` and
/// return the expansion. Returns `None` for non-skill input or when
/// the named skill is unknown (caller passes the original text
/// through in the second case, matching PI's behavior).
pub fn expand_skill_command(text: &str, skills: &[Skill]) -> Option<ExpandedSkill> {
    let rest = text.strip_prefix("/skill:")?;
    let (name, args) = match rest.find(char::is_whitespace) {
        Some(i) => (&rest[..i], rest[i..].trim().to_string()),
        None => (rest, String::new()),
    };
    let skill = skills.iter().find(|s| s.name == name)?;

    let raw = std::fs::read_to_string(&skill.file_path).ok()?;
    let (_, body) = split_frontmatter(&raw);
    let body = body.trim().to_string();

    let block = format!(
        "<skill name=\"{}\" location=\"{}\">\n\
         References are relative to {}.\n\n\
         {}\n\
         </skill>",
        skill.name,
        skill.file_path.display(),
        skill.base_dir.display(),
        body,
    );
    let expanded = if args.is_empty() {
        block.clone()
    } else {
        format!("{block}\n\n{args}")
    };

    Some(ExpandedSkill {
        name: skill.name.clone(),
        location: skill.file_path.clone(),
        base_dir: skill.base_dir.clone(),
        body,
        user_args: (!args.is_empty()).then_some(args),
        expanded_text: expanded,
    })
}

/// Strip YAML frontmatter delimited by `---` lines. Returns
/// `(frontmatter_text, body_text)`. If no frontmatter, first tuple
/// element is empty and body is the full input.
pub(crate) fn split_frontmatter(content: &str) -> (String, String) {
    let mut lines = content.lines();
    if lines.next() != Some("---") {
        return (String::new(), content.to_string());
    }
    let mut fm = String::new();
    let mut body_start = None;
    for (i, line) in content.lines().enumerate().skip(1) {
        if line == "---" {
            body_start = Some(i + 1);
            break;
        }
        fm.push_str(line);
        fm.push('\n');
    }
    let body = match body_start {
        Some(start) => content.lines().skip(start).collect::<Vec<_>>().join("\n"),
        None => String::new(),
    };
    (fm, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("nanopi-res-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write_skill(dir: &Path, name: &str, desc: &str, body: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join("SKILL.md");
        std::fs::write(
            &p,
            format!("---\nname: {name}\ndescription: {desc}\n---\n{body}\n"),
        )
        .unwrap();
        p
    }

    #[test]
    fn missing_dir_returns_empty() {
        let r = load_skills_from_dir(Path::new("/nonexistent/xyz"), SkillSource::User);
        assert!(r.skills.is_empty());
        assert!(r.diagnostics.is_empty());
    }

    #[test]
    fn flat_md_root_is_loaded_at_top_level() {
        let dir = tmp();
        std::fs::write(
            dir.join("greet.md"),
            "---\nname: greet\ndescription: greet the user\n---\nBody.\n",
        )
        .unwrap();
        let r = load_skills_from_dir(&dir, SkillSource::User);
        assert_eq!(r.skills.len(), 1);
        assert_eq!(r.skills[0].name, "greet");
        assert_eq!(r.skills[0].source, SkillSource::User);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skill_md_dir_wins_over_flat_md() {
        let dir = tmp();
        // Flat sibling file that should still be picked up too.
        std::fs::write(
            dir.join("flat.md"),
            "---\nname: flat\ndescription: d\n---\n",
        )
        .unwrap();
        // A subdir with SKILL.md — the subdir stops recursion.
        write_skill(&dir.join("subskill"), "sub", "d", "body");
        let r = load_skills_from_dir(&dir, SkillSource::User);
        let names: Vec<_> = r.skills.iter().map(|s| s.name.clone()).collect();
        assert!(names.contains(&"flat".to_string()));
        assert!(names.contains(&"sub".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nested_recursion_finds_skill_md() {
        let dir = tmp();
        write_skill(&dir.join("outer").join("inner"), "deep", "d", "body");
        let r = load_skills_from_dir(&dir, SkillSource::User);
        assert_eq!(r.skills.len(), 1);
        assert_eq!(r.skills[0].name, "deep");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skill_md_dir_stops_recursion() {
        let dir = tmp();
        // outer/SKILL.md exists AND outer/inner/SKILL.md also exists.
        // Only outer should be picked; inner is not recursed into.
        write_skill(&dir.join("outer"), "outer", "d", "body");
        write_skill(&dir.join("outer").join("inner"), "inner", "d", "body");
        let r = load_skills_from_dir(&dir, SkillSource::User);
        let names: Vec<_> = r.skills.iter().map(|s| s.name.clone()).collect();
        assert_eq!(names, vec!["outer".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dotdirs_and_node_modules_skipped() {
        let dir = tmp();
        write_skill(&dir.join(".hidden"), "hidden", "d", "body");
        write_skill(&dir.join("node_modules").join("pkg"), "pkg", "d", "body");
        write_skill(&dir.join("real"), "real", "d", "body");
        let r = load_skills_from_dir(&dir, SkillSource::User);
        let names: Vec<_> = r.skills.iter().map(|s| s.name.clone()).collect();
        assert_eq!(names, vec!["real".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_description_drops_skill_with_diagnostic() {
        let dir = tmp();
        std::fs::write(dir.join("bad.md"), "---\nname: bad\n---\nno description\n").unwrap();
        let r = load_skills_from_dir(&dir, SkillSource::User);
        assert!(r.skills.is_empty());
        assert!(!r.diagnostics.is_empty());
        assert!(r.diagnostics[0].message.contains("description"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_name_chars_warns_but_loads() {
        let dir = tmp();
        std::fs::write(
            dir.join("x.md"),
            "---\nname: BadName\ndescription: d\n---\nbody\n",
        )
        .unwrap();
        let r = load_skills_from_dir(&dir, SkillSource::User);
        assert_eq!(r.skills.len(), 1);
        assert!(r
            .diagnostics
            .iter()
            .any(|d| d.message.contains("invalid characters")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn consecutive_hyphens_warns() {
        let dir = tmp();
        std::fs::write(
            dir.join("x.md"),
            "---\nname: a--b\ndescription: d\n---\nbody\n",
        )
        .unwrap();
        let r = load_skills_from_dir(&dir, SkillSource::User);
        assert!(r
            .diagnostics
            .iter()
            .any(|d| d.message.contains("consecutive")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn name_falls_back_to_parent_dir() {
        let dir = tmp();
        let sub = dir.join("fallback-name");
        std::fs::create_dir_all(&sub).unwrap();
        // no `name:` field
        std::fs::write(sub.join("SKILL.md"), "---\ndescription: d\n---\nbody\n").unwrap();
        let r = load_skills_from_dir(&dir, SkillSource::User);
        assert_eq!(r.skills.len(), 1);
        assert_eq!(r.skills[0].name, "fallback-name");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disable_model_invocation_hidden_from_prompt() {
        let dir = tmp();
        std::fs::write(
            dir.join("hidden.md"),
            "---\nname: hidden\ndescription: d\ndisable-model-invocation: true\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("visible.md"),
            "---\nname: visible\ndescription: d\n---\nbody\n",
        )
        .unwrap();
        let r = load_skills_from_dir(&dir, SkillSource::User);
        let s = format_skills_for_prompt(&r.skills);
        assert!(s.contains("<name>visible</name>"));
        assert!(!s.contains("<name>hidden</name>"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn collision_keeps_first_and_warns() {
        let user = tmp();
        let proj = tmp();
        std::fs::write(
            user.join("dup.md"),
            "---\nname: dup\ndescription: from user\n---\nu\n",
        )
        .unwrap();
        std::fs::write(
            proj.join("dup.md"),
            "---\nname: dup\ndescription: from project\n---\np\n",
        )
        .unwrap();
        let r = load_skills(LoadSkillsOptions {
            user_dir: Some(user.clone()),
            project_dir: Some(proj.clone()),
            ..Default::default()
        });
        assert_eq!(r.skills.len(), 1);
        assert_eq!(r.skills[0].description, "from user");
        assert!(r
            .diagnostics
            .iter()
            .any(|d| d.level == DiagnosticLevel::Collision));
        let _ = std::fs::remove_dir_all(&user);
        let _ = std::fs::remove_dir_all(&proj);
    }

    #[test]
    fn no_discovery_still_loads_cli() {
        let user = tmp();
        std::fs::write(user.join("x.md"), "---\nname: x\ndescription: d\n---\nb\n").unwrap();
        let extra = tmp();
        let extra_file = extra.join("y.md");
        std::fs::write(&extra_file, "---\nname: y\ndescription: d\n---\nb\n").unwrap();
        let r = load_skills(LoadSkillsOptions {
            user_dir: Some(user.clone()),
            cli_paths: vec![extra_file.clone()],
            no_discovery: true,
            ..Default::default()
        });
        let names: Vec<_> = r.skills.iter().map(|s| s.name.clone()).collect();
        assert_eq!(names, vec!["y".to_string()]);
        let _ = std::fs::remove_dir_all(&user);
        let _ = std::fs::remove_dir_all(&extra);
    }

    #[test]
    fn nonexistent_cli_path_produces_diagnostic() {
        let r = load_skills(LoadSkillsOptions {
            cli_paths: vec![PathBuf::from("/nonexistent/skills/nope.md")],
            ..Default::default()
        });
        assert!(r.skills.is_empty());
        assert!(r
            .diagnostics
            .iter()
            .any(|d| d.message.contains("does not exist")));
    }

    #[test]
    fn format_skills_empty_returns_empty_string() {
        assert_eq!(format_skills_for_prompt(&[]), "");
    }

    #[test]
    fn expand_skill_command_produces_block() {
        let dir = tmp();
        let sub = dir.join("greet");
        write_skill(&sub, "greet", "greet", "Say hi to the user.");
        let r = load_skills_from_dir(&dir, SkillSource::User);
        let e = expand_skill_command("/skill:greet please", &r.skills).expect("expanded");
        assert_eq!(e.name, "greet");
        assert_eq!(e.user_args.as_deref(), Some("please"));
        assert!(e.expanded_text.contains("<skill name=\"greet\""));
        assert!(e.expanded_text.contains("Say hi to the user."));
        assert!(e.expanded_text.ends_with("please"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expand_skill_command_without_args() {
        let dir = tmp();
        write_skill(&dir.join("s"), "s", "d", "body only");
        let r = load_skills_from_dir(&dir, SkillSource::User);
        let e = expand_skill_command("/skill:s", &r.skills).expect("expanded");
        assert!(e.user_args.is_none());
        assert!(e.expanded_text.ends_with("</skill>"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expand_unknown_skill_returns_none() {
        assert!(expand_skill_command("/skill:missing hi", &[]).is_none());
    }

    #[test]
    fn expand_non_skill_returns_none() {
        assert!(expand_skill_command("hello world", &[]).is_none());
        assert!(expand_skill_command("/other", &[]).is_none());
    }

    #[test]
    fn splits_frontmatter_roundtrip() {
        let s = "---\nname: x\ndescription: y\n---\nbody";
        let (fm, body) = split_frontmatter(s);
        assert!(fm.contains("name: x"));
        assert_eq!(body, "body");
    }

    // -- flat frontmatter parser (v0.9.2 PI-parity leniency) --

    #[test]
    fn description_with_unquoted_colon_loads() {
        let dir = tmp();
        write_skill(
            &dir.join("s"),
            "s",
            "Office skillhub: publish, query, download",
            "body",
        );
        let r = load_skills_from_dir(&dir, SkillSource::User);
        assert_eq!(r.skills.len(), 1);
        assert_eq!(
            r.skills[0].description,
            "Office skillhub: publish, query, download"
        );
        assert!(
            r.diagnostics.is_empty(),
            "unexpected diagnostics: {:?}",
            r.diagnostics
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quoted_description_is_dequoted() {
        let dir = tmp();
        std::fs::write(
            dir.join("s.md"),
            "---\nname: s\ndescription: \"Foo: bar\"\n---\nbody\n",
        )
        .unwrap();
        let r = load_skills_from_dir(&dir, SkillSource::User);
        assert_eq!(r.skills.len(), 1);
        assert_eq!(r.skills[0].description, "Foo: bar");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn single_quoted_description_is_dequoted() {
        let dir = tmp();
        std::fs::write(
            dir.join("s.md"),
            "---\nname: s\ndescription: 'has # in it'\n---\nbody\n",
        )
        .unwrap();
        let r = load_skills_from_dir(&dir, SkillSource::User);
        assert_eq!(r.skills[0].description, "has # in it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_frontmatter_keys_ignored() {
        let dir = tmp();
        std::fs::write(
            dir.join("s.md"),
            "---\nname: s\ndescription: d\ncustom: whatever\nversion: 1.2.0\n---\nbody\n",
        )
        .unwrap();
        let r = load_skills_from_dir(&dir, SkillSource::User);
        assert_eq!(r.skills.len(), 1);
        assert!(r.diagnostics.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn comments_and_blank_lines_skipped() {
        let dir = tmp();
        std::fs::write(
            dir.join("s.md"),
            "---\n# top comment\nname: s\n\n# mid comment\ndescription: d\n---\nbody\n",
        )
        .unwrap();
        let r = load_skills_from_dir(&dir, SkillSource::User);
        assert_eq!(r.skills.len(), 1);
        assert_eq!(r.skills[0].name, "s");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disable_model_invocation_bool_from_string() {
        let dir = tmp();
        std::fs::write(
            dir.join("s.md"),
            "---\nname: s\ndescription: d\ndisable-model-invocation: true\n---\nbody\n",
        )
        .unwrap();
        let r = load_skills_from_dir(&dir, SkillSource::User);
        assert_eq!(r.skills.len(), 1);
        assert!(r.skills[0].disable_model_invocation);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
