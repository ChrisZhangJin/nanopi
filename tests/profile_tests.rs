//! End-to-end tests of the public profile API (`nanopi::profile`) as an
//! outside crate sees it: create two profiles with different settings,
//! switch back and forth, check isolation, persistence across a reload,
//! and that the legacy `config.toml` still works as the default profile.

use std::path::Path;

use nanopi::config::{load_config, load_config_from, Config};
use nanopi::profile;

/// A home holding the legacy `config.toml` plus profiles `work` and
/// `local`, which differ in model, endpoint, key and an `[agent]` limit.
fn home_with_profiles() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    std::fs::write(
        h.join("config.toml"),
        "model = \"legacy-model\"\nbase_url = \"https://legacy.example/v1\"\napi_key = \"k-legacy\"\n",
    )
    .unwrap();
    let work = profile::create_in(h, "work", None).unwrap();
    std::fs::write(
        work,
        "model = \"work-model\"\nbase_url = \"https://work.example/v1\"\napi_key = \"k-work\"\n\
         [agent]\nmax_turns = 7\n",
    )
    .unwrap();
    let local = profile::create_in(h, "local", None).unwrap();
    std::fs::write(
        local,
        "model = \"local-model\"\nbase_url = \"http://localhost:11434/v1\"\napi_key = \"k-local\"\n",
    )
    .unwrap();
    home
}

/// What a fresh process would load: resolve the active profile from disk
/// alone (no in-process state), then load it under an empty project.
fn load_after_restart(home: &Path, cwd: &Path) -> Config {
    let (name, _warning) = profile::resolve_startup(home, None, None).unwrap();
    load_config_from(Some(&profile::config_path_in(home, &name)), cwd).unwrap()
}

/// The explicit-home API: no env vars touched, so it cannot race.
#[test]
fn profiles_switch_stay_isolated_and_persist() {
    let home = home_with_profiles();
    let h = home.path();
    let cwd = tempfile::tempdir().unwrap();
    assert_eq!(profile::list_in(h), ["default", "local", "work"]);

    // Legacy config.toml is the default profile until something is chosen.
    let c = load_after_restart(h, cwd.path());
    assert_eq!(c.model.as_deref(), Some("legacy-model"));

    // Switch back and forth; every reload sees only that profile.
    for _ in 0..2 {
        profile::persist_in(h, "work").unwrap();
        let c = load_after_restart(h, cwd.path());
        let ep = profile::resolve_endpoint(&c, |_| None).unwrap();
        assert_eq!((ep.model.as_str(), ep.api_key.as_str()), ("work-model", "k-work"));
        assert_eq!(ep.base_url, "https://work.example/v1");
        assert_eq!(c.agent_config().max_turns, 7);

        profile::persist_in(h, "local").unwrap();
        let c = load_after_restart(h, cwd.path());
        let ep = profile::resolve_endpoint(&c, |_| None).unwrap();
        assert_eq!((ep.model.as_str(), ep.api_key.as_str()), ("local-model", "k-local"));
        assert_eq!(ep.base_url, "http://localhost:11434/v1");
        assert_ne!(c.agent_config().max_turns, 7, "work's [agent] setting must not leak into local");
    }

    // Persisted: a "restart" still lands on local; --profile overrides it
    // for one run without changing what is saved.
    assert_eq!(profile::resolve_startup(h, None, None).unwrap().0, "local");
    assert_eq!(profile::resolve_startup(h, Some("work"), None).unwrap().0, "work");
    assert_eq!(profile::read_persisted_in(h).as_deref(), Some("local"));
    assert!(profile::resolve_startup(h, Some("missing"), None).is_err());

    // Back to default: the marker file goes away and legacy config.toml
    // works exactly as before.
    profile::persist_in(h, profile::DEFAULT).unwrap();
    assert!(!h.join("active_profile").exists());
    let c = load_after_restart(h, cwd.path());
    assert_eq!(c.model.as_deref(), Some("legacy-model"));
    assert_eq!(c.api_key.as_deref(), Some("k-legacy"));
}

/// The process-wide path every loader uses (`load_config`), driven by
/// `NANOPI_HOME` and `profile::switch`. The only test here touching the
/// env var.
#[test]
fn switch_redirects_load_config() {
    let home = home_with_profiles();
    let cwd = tempfile::tempdir().unwrap();
    std::env::set_var("NANOPI_HOME", home.path());

    let model = || load_config(cwd.path()).unwrap().model;
    assert_eq!(profile::current(), profile::DEFAULT);
    assert_eq!(model().as_deref(), Some("legacy-model"));

    profile::switch("work").unwrap();
    assert_eq!(model().as_deref(), Some("work-model"));
    profile::switch("local").unwrap();
    assert_eq!(model().as_deref(), Some("local-model"));
    profile::switch("work").unwrap();
    assert_eq!(model().as_deref(), Some("work-model"));
    assert!(profile::switch("missing").is_err(), "unknown profile is refused");
    assert_eq!(profile::current(), "work", "a failed switch changes nothing");
    assert_eq!(profile::read_persisted_in(home.path()).as_deref(), Some("work"));

    profile::switch(profile::DEFAULT).unwrap();
    assert_eq!(model().as_deref(), Some("legacy-model"));
    std::env::remove_var("NANOPI_HOME");
}
