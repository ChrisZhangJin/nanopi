//! End-to-end coverage for the v0.9 skills pipeline.
//!
//! These sit above the unit tests in `src/resources.rs` and drive the
//! Agent factory (`Agent::build_fresh`) with real filesystem layouts,
//! then assert the resulting `agent.skills` and system prompt.
//!
//! Anything that needs a running Provider is NOT tested here; those
//! shapes are covered by the loop_ integration tests in
//! `src/agent/loop_.rs`. This file tests the wiring around them.

use std::path::PathBuf;

use nanopi::agent::build::{AgentBuildInputs, SkillLoadPolicy};
use nanopi::agent::loop_::Agent;
use nanopi::agent::hook::HookConfig as _HookConfig;
use nanopi::agent::loop_::HooksConfig;
use nanopi::agent::permission::PermissionGate;
use nanopi::provider::openai::OpenAiProvider;
use nanopi::resources::{
    DiagnosticLevel, LoadSkillsOptions, SkillSource, expand_skill_command,
    format_skills_for_prompt, load_skills, load_skills_from_dir,
};
use nanopi::tool::ToolRegistry;
use nanopi::util::uuid;

fn tmp_dir(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("nanopi-it-{tag}-{}", uuid::v7()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn write_skill(dir: &std::path::Path, name: &str, desc: &str, body: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {desc}\n---\n{body}\n"),
    )
    .unwrap();
}

fn stub_agent_inputs(
    cwd: PathBuf,
    session_path: PathBuf,
    skill_load: SkillLoadPolicy,
) -> AgentBuildInputs {
    AgentBuildInputs {
        cwd,
        registry: ToolRegistry::standard(),
        provider: Box::new(OpenAiProvider::new("", "", "")),
        session_path,
        session_id: uuid::v7(),
        permission: PermissionGate::from_cli(true, None),
        hooks: HooksConfig::default(),
        model: "test-model".into(),
        base_url: String::new(),
        api_key: String::new(),
        skill_load,
    }
}

#[test]
fn agent_loads_user_and_project_skills_into_prompt() {
    let user = tmp_dir("user");
    let proj_root = tmp_dir("proj");
    let proj_skills = proj_root.join(".nanopi").join("skills");
    write_skill(&user.join("greet"), "greet", "greet the user warmly", "hi");
    write_skill(&proj_skills.join("format"), "format", "format code cleanly", "run rustfmt");

    let policy = SkillLoadPolicy {
        user_dir: Some(user.clone()),
        project_dir: Some(proj_skills.clone()),
        cli_paths: Vec::new(),
        no_discovery: false,
        disabled: Vec::new(),
    };
    let sess_dir = tmp_dir("sess");
    let (agent, diagnostics) =
        Agent::build_fresh(stub_agent_inputs(proj_root.clone(), sess_dir.join("s.jsonl"), policy));

    assert_eq!(agent.skills.len(), 2, "both skills should load");
    assert!(diagnostics.is_empty(), "no diagnostics expected: {diagnostics:?}");

    let names: Vec<_> = agent.skills.iter().map(|s| s.name.clone()).collect();
    assert!(names.contains(&"greet".to_string()));
    assert!(names.contains(&"format".to_string()));

    let prompt = agent.context.system.as_ref().expect("system prompt set");
    assert!(prompt.contains("<available_skills>"), "prompt has skills block");
    assert!(prompt.contains("<name>greet</name>"), "greet listed");
    assert!(prompt.contains("<name>format</name>"), "format listed");

    let _ = std::fs::remove_dir_all(&user);
    let _ = std::fs::remove_dir_all(&proj_root);
    let _ = std::fs::remove_dir_all(&sess_dir);
}

#[test]
fn distrusted_project_gates_off_project_skills() {
    let root = tmp_dir("distrust");
    let proj_skills = root.join(".nanopi").join("skills");
    write_skill(&proj_skills.join("leak"), "leak", "would leak env", "cat env");

    // from_cli with project_trusted=false clears the project_dir arm.
    let policy =
        SkillLoadPolicy::from_cli(&root, Vec::new(), true /*no_skills*/, false, Vec::new());
    assert!(policy.project_dir.is_none(), "distrust must drop project dir");
    assert!(policy.no_discovery);

    // Even with the raw options that include the dir, --no-skills drops
    // user + project discovery.
    let policy_no_skills = SkillLoadPolicy {
        user_dir: None,
        project_dir: Some(proj_skills.clone()),
        cli_paths: Vec::new(),
        no_discovery: true,
        disabled: Vec::new(),
    };
    let sess = tmp_dir("sess-distrust");
    let (agent, _) = Agent::build_fresh(stub_agent_inputs(
        root.clone(),
        sess.join("s.jsonl"),
        policy_no_skills,
    ));
    assert!(agent.skills.is_empty());
    let prompt = agent.context.system.as_ref().unwrap();
    assert!(!prompt.contains("<available_skills>"));

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&sess);
}

#[test]
fn no_skills_still_honors_cli_paths() {
    let user = tmp_dir("no-skills-user");
    write_skill(&user.join("hidden"), "hidden", "should not load", "b");

    let cli = tmp_dir("cli-explicit");
    write_skill(&cli.join("explicit"), "explicit", "loads via --skill", "b");

    let policy = SkillLoadPolicy {
        user_dir: Some(user.clone()),
        project_dir: None,
        cli_paths: vec![cli.clone()],
        no_discovery: true,
        disabled: Vec::new(),
    };
    let root = tmp_dir("root");
    let sess = tmp_dir("sess");
    let (agent, _) =
        Agent::build_fresh(stub_agent_inputs(root.clone(), sess.join("s.jsonl"), policy));

    let names: Vec<_> = agent.skills.iter().map(|s| s.name.clone()).collect();
    assert_eq!(names, vec!["explicit".to_string()]);

    let _ = std::fs::remove_dir_all(&user);
    let _ = std::fs::remove_dir_all(&cli);
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&sess);
}

#[test]
fn expand_skill_command_matches_pi_shape() {
    let dir = tmp_dir("expand");
    let base = dir.join("brave-search");
    write_skill(&base, "brave-search", "web search", "Run ./search.js.");
    let r = load_skills_from_dir(&dir, SkillSource::User);
    assert_eq!(r.skills.len(), 1);

    let e = expand_skill_command("/skill:brave-search rust async", &r.skills)
        .expect("expansion");
    assert_eq!(e.name, "brave-search");
    assert_eq!(e.user_args.as_deref(), Some("rust async"));

    // Exact PI shape: `<skill name="X" location="Y">\nReferences are
    // relative to Z.\n\n<body>\n</skill>\n\n<args>`.
    let expected_header =
        format!("<skill name=\"brave-search\" location=\"{}\">", base.join("SKILL.md").display());
    assert!(e.expanded_text.starts_with(&expected_header));
    assert!(e.expanded_text.contains("References are relative to"));
    assert!(e.expanded_text.contains("Run ./search.js."));
    assert!(e.expanded_text.ends_with("rust async"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn collision_prefers_earlier_source() {
    let user = tmp_dir("collide-user");
    let proj = tmp_dir("collide-proj");
    write_skill(&user.join("dup"), "dup", "user version", "u");
    write_skill(&proj.join("dup"), "dup", "project version", "p");
    let r = load_skills(LoadSkillsOptions {
        user_dir: Some(user.clone()),
        project_dir: Some(proj.clone()),
        cli_paths: Vec::new(),
        no_discovery: false,
        disabled: Vec::new(),
    });
    assert_eq!(r.skills.len(), 1);
    assert_eq!(r.skills[0].description, "user version");
    assert!(r
        .diagnostics
        .iter()
        .any(|d| d.level == DiagnosticLevel::Collision));
    let _ = std::fs::remove_dir_all(&user);
    let _ = std::fs::remove_dir_all(&proj);
}

#[test]
fn disable_model_invocation_hidden_but_expandable() {
    let dir = tmp_dir("hidden");
    let sub = dir.join("silent");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(
        sub.join("SKILL.md"),
        "---\nname: silent\ndescription: hidden from prompt\ndisable-model-invocation: true\n---\nRun the thing.\n",
    )
    .unwrap();
    let r = load_skills_from_dir(&dir, SkillSource::User);
    assert_eq!(r.skills.len(), 1);
    assert!(r.skills[0].disable_model_invocation);

    let prompt = format_skills_for_prompt(&r.skills);
    assert_eq!(prompt, "", "hidden skill should not appear in prompt");

    let expansion = expand_skill_command("/skill:silent", &r.skills).expect("still expandable");
    assert!(expansion.expanded_text.contains("Run the thing."));

    let _ = std::fs::remove_dir_all(&dir);
}

#[allow(dead_code)]
fn _keep_hook_import(_h: _HookConfig) {}
