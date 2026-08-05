//! Skills + prompt templates loader.
//!
//! Skills live as markdown files with YAML frontmatter under
//! `~/.nanopi/skills/` and `<cwd>/.nanopi/skills/`. Each skill's
//! `description` is injected into the system prompt; the body is loaded
//! when the LLM invokes the skill via `<skill name="..."/>`.
//!
//! v0.5 ships the loader; full skill invocation protocol is wired in
//! the agent loop (v0.5.1 follow-up).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub location: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptTemplate {
    pub name: String,
    pub description: String,
    pub args: Vec<String>,
    pub body: String,
    pub location: PathBuf,
}

/// Load skills from a directory. Walks `*.md` files, parses frontmatter,
/// returns Skill structs. Missing dir → Ok(empty).
pub fn load_skills_from_dir(dir: &Path) -> Result<Vec<Skill>> {
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        if let Some(skill) = parse_skill_file(&path)? {
            out.push(skill);
        }
    }
    Ok(out)
}

/// Load prompt templates from a directory.
pub fn load_prompts_from_dir(dir: &Path) -> Result<Vec<PromptTemplate>> {
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        if let Some(p) = parse_prompt_file(&path)? {
            out.push(p);
        }
    }
    Ok(out)
}

fn parse_skill_file(path: &Path) -> Result<Option<Skill>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?;
    let (fm, _) = split_frontmatter(&content);
    let fm: SkillFrontmatter = match serde_yaml::from_str(&fm) {
        Ok(f) => f,
        Err(_) => return Ok(None), // skip unparseable
    };
    Ok(Some(Skill {
        name: fm.name,
        description: fm.description,
        location: path.to_path_buf(),
    }))
}

fn parse_prompt_file(path: &Path) -> Result<Option<PromptTemplate>> {
    let content = std::fs::read_to_string(path)?;
    let (fm, body) = split_frontmatter(&content);
    let fm: PromptFrontmatter = match serde_yaml::from_str(&fm) {
        Ok(f) => f,
        Err(_) => return Ok(None),
    };
    let args = fm
        .args
        .map(|s| s.split_whitespace().map(String::from).collect())
        .unwrap_or_default();
    Ok(Some(PromptTemplate {
        name: fm.name,
        description: fm.description,
        args,
        body,
        location: path.to_path_buf(),
    }))
}

#[derive(Debug, Deserialize)]
struct SkillFrontmatter {
    name: String,
    description: String,
}

#[derive(Debug, Deserialize)]
struct PromptFrontmatter {
    name: String,
    description: String,
    #[serde(default)]
    args: Option<String>,
}

/// Split YAML frontmatter from body. Frontmatter is delimited by `---` lines.
fn split_frontmatter(content: &str) -> (String, String) {
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

/// Format all loaded skills as an `<available_skills>` block to inject
/// into the system prompt.
pub fn format_skills_for_prompt(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut out = String::from("<available_skills>\n");
    for s in skills {
        out.push_str(&format!(
            "<skill>\n<name>{}</name>\n<description>{}</description>\n<location>{}</location>\n</skill>\n",
            s.name, s.description, s.location.display()
        ));
    }
    out.push_str("</available_skills>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("nanopi-res-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn missing_dir_returns_empty() {
        let p = std::path::PathBuf::from("/nonexistent/xyz");
        let v = load_skills_from_dir(&p).unwrap();
        assert!(v.is_empty());
    }

    #[test]
    fn loads_skills_from_dir() {
        let dir = tmp();
        std::fs::write(
            dir.join("greet.md"),
            "---\nname: greet\ndescription: greet the user\n---\nBody here.\n",
        )
        .unwrap();
        let v = load_skills_from_dir(&dir).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "greet");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn splits_frontmatter() {
        let s = "---\nname: x\ndescription: y\n---\nbody";
        let (fm, body) = split_frontmatter(s);
        assert!(fm.contains("name: x"));
        assert_eq!(body, "body");
    }

    #[test]
    fn no_frontmatter_returns_full_body() {
        let s = "just some text\n";
        let (fm, body) = split_frontmatter(s);
        assert!(fm.is_empty());
        assert!(body.contains("just some text"));
    }

    #[test]
    fn format_skills_for_prompt_contains_all() {
        let skills = vec![
            Skill {
                name: "a".into(),
                description: "alpha".into(),
                location: std::path::PathBuf::from("/a"),
            },
            Skill {
                name: "b".into(),
                description: "beta".into(),
                location: std::path::PathBuf::from("/b"),
            },
        ];
        let s = format_skills_for_prompt(&skills);
        assert!(s.contains("<name>a</name>"));
        assert!(s.contains("<name>b</name>"));
        assert!(s.contains("</available_skills>"));
    }

    #[test]
    fn loads_prompts_with_args() {
        let dir = tmp();
        std::fs::write(
            dir.join("explain.md"),
            "---\nname: explain\ndescription: explain code\nargs: code\n---\nExplain {code}\n",
        )
        .unwrap();
        let v = load_prompts_from_dir(&dir).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].args, vec!["code"]);
        assert!(v[0].body.contains("Explain"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}