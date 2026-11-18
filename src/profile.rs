//! Named config profiles.
//!
//! A profile is a complete, independent global config file. The layout
//! under the nanopi home (`~/.nanopi`, or `$NANOPI_HOME`):
//!
//! ```text
//! config.toml                  the "default" profile (the pre-profiles file)
//! api_key                      default's key file, as `nanopi init` writes it
//! profiles/work/config.toml    profile "work"
//! profiles/work/api_key        work's key file (written by the profile wizard)
//! profiles/local/config.toml   profile "local"
//! active_profile               name of the profile to use at startup (optional)
//! ```
//!
//! Each profile is a folder so everything belonging to it (key file,
//! wizard draft) lives next to its config. A folder without a
//! `config.toml` (a wizard that was cancelled) is not a profile yet.
//!
//! The active profile only decides which file plays the role of the
//! GLOBAL config. Everything else is unchanged: the project-local
//! `<cwd>/.nanopi/config.toml` is still merged on top, CLI flags and
//! `OPENAI_*` env vars still win at startup, and `settings.toml`,
//! sessions, skills, trust and MCP stay shared across profiles.
//!
//! With no `profiles/` directory and no `active_profile` file, nothing
//! changes at all: the active profile is `default`, which is exactly
//! `config.toml`.
//!
//! Which profile a process uses, highest priority first:
//!   1. `--profile NAME` (this run only, not persisted)
//!   2. `NANOPI_PROFILE` (how agent children inherit their parent's)
//!   3. the `active_profile` file (written by `/profile use`)
//!   4. `default`

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use crate::config::Config;

/// The profile backed by the legacy `config.toml`.
pub const DEFAULT: &str = "default";

/// Env var carrying the profile into child processes.
pub const ENV: &str = "NANOPI_PROFILE";

/// Profile names become file names, so keep them boring: ASCII letters,
/// digits, `-` and `_`, at most 64 characters.
pub fn validate_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "invalid profile name `{name}`: use 1-64 characters from A-Z a-z 0-9 - _"
        ))
    }
}

pub fn profiles_dir(home: &Path) -> PathBuf {
    home.join("profiles")
}

/// The folder holding `name`'s files. `default` is the nanopi home
/// itself, which is where its `config.toml` and `api_key` always lived.
pub fn dir_in(home: &Path, name: &str) -> PathBuf {
    if name == DEFAULT {
        home.to_path_buf()
    } else {
        profiles_dir(home).join(name)
    }
}

/// The config file backing `name`. `default` is the legacy file.
pub fn config_path_in(home: &Path, name: &str) -> PathBuf {
    dir_in(home, name).join("config.toml")
}

/// `default` always exists (a missing `config.toml` just means built-in
/// defaults, as it always has); any other profile exists when its file
/// does.
pub fn exists_in(home: &Path, name: &str) -> bool {
    name == DEFAULT || (validate_name(name).is_ok() && config_path_in(home, name).is_file())
}

/// Every profile: `default` first, then the rest sorted. Only folders
/// in `profiles/` with a valid name and a `config.toml` count.
pub fn list_in(home: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(profiles_dir(home))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            (name != DEFAULT && exists_in(home, &name)).then_some(name)
        })
        .collect();
    names.sort();
    names.insert(0, DEFAULT.to_string());
    names
}

/// Template written for a new profile when there is nothing to copy.
const STUB: &str = "# nanopi profile. Same keys as ~/.nanopi/config.toml;\n\
                    # see config.toml.example for the full list.\n\
                    #\n\
                    # model = \"gpt-4o-mini\"\n\
                    # base_url = \"https://api.openai.com/v1\"\n\
                    # api_key_file = \"~/.nanopi/api_key\"\n";

/// Make the folder for a NEW profile `name` and return it. Refuses
/// `default`, invalid names and profiles that already exist. A folder
/// left behind by a cancelled wizard (no `config.toml`) is reused.
/// The folder is owner-only: it will hold an API key.
pub fn prepare_dir_in(home: &Path, name: &str) -> Result<PathBuf, String> {
    validate_name(name)?;
    if name == DEFAULT {
        return Err("`default` always exists (it is ~/.nanopi/config.toml)".to_string());
    }
    let path = config_path_in(home, name);
    if path.exists() {
        return Err(format!("profile `{name}` already exists ({})", path.display()));
    }
    let dir = dir_in(home, name);
    create_private_dir(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    Ok(dir)
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir)
}

/// Create profile `name` WITHOUT the wizard. Its config starts as a
/// copy of `copy_from` when that file exists (so the new profile works
/// straight away and only the differences need editing), otherwise as
/// a commented stub. Refuses `default` and names that already exist.
pub fn create_in(home: &Path, name: &str, copy_from: Option<&Path>) -> Result<PathBuf, String> {
    let body = match copy_from.filter(|p| p.is_file()) {
        Some(src) => std::fs::read_to_string(src)
            .map_err(|e| format!("cannot read {}: {e}", src.display()))?,
        None => STUB.to_string(),
    };
    let dir = prepare_dir_in(home, name)?;
    let path = dir.join("config.toml");
    write_private(&path, &body).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}

/// Move profiles written by the first version of this feature, one flat
/// file per profile (`profiles/<name>.toml`), into their own folder
/// (`profiles/<name>/config.toml`). Returns one line per file moved.
/// A file whose folder already has a config is left alone and reported.
pub fn migrate_flat_in(home: &Path) -> Vec<String> {
    let mut notes = Vec::new();
    let Ok(rd) = std::fs::read_dir(profiles_dir(home)) else {
        return notes;
    };
    let mut flat: Vec<(String, PathBuf)> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().and_then(|x| x.to_str()) == Some("toml"))
        .filter_map(|p| {
            let stem = p.file_stem()?.to_str()?.to_string();
            (stem != DEFAULT && validate_name(&stem).is_ok()).then_some((stem, p))
        })
        .collect();
    flat.sort();
    for (name, old) in flat {
        let new = config_path_in(home, &name);
        if new.exists() {
            notes.push(format!(
                "profile `{name}`: {} not moved, {} already exists",
                old.display(),
                new.display()
            ));
            continue;
        }
        let moved = create_private_dir(&dir_in(home, &name)).and_then(|()| std::fs::rename(&old, &new));
        notes.push(match moved {
            Ok(()) => format!("profile `{name}`: moved {} to {}", old.display(), new.display()),
            Err(e) => format!("profile `{name}`: could not move {}: {e}", old.display()),
        });
    }
    notes
}

/// A profile may hold an inline `api_key`, so its file is owner-only.
fn write_private(path: &Path, body: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(body.as_bytes())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, body)
    }
}

fn active_file(home: &Path) -> PathBuf {
    home.join("active_profile")
}

/// The persisted active profile, if the file names a valid one.
pub fn read_persisted_in(home: &Path) -> Option<String> {
    let s = std::fs::read_to_string(active_file(home)).ok()?;
    let name = s.trim().to_string();
    validate_name(&name).ok().map(|_| name)
}

/// Persist `name` as the active profile. Choosing `default` removes the
/// file, leaving the home exactly as it was before profiles existed.
pub fn persist_in(home: &Path, name: &str) -> std::io::Result<()> {
    let f = active_file(home);
    if name == DEFAULT {
        match std::fs::remove_file(&f) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    } else {
        std::fs::create_dir_all(home)?;
        std::fs::write(f, format!("{name}\n"))
    }
}

/// Which profile to start with, from the `--profile` flag, the env var
/// and the persisted file (in that order). Returns the name plus an
/// optional warning. An explicitly requested profile that does not exist
/// is an error; a stale persisted one only warns and falls back to
/// `default`, so a deleted profile file can never lock the user out.
pub fn resolve_startup(
    home: &Path,
    flag: Option<&str>,
    env: Option<&str>,
) -> Result<(String, Option<String>), String> {
    for (source, value) in [("--profile", flag), (ENV, env)] {
        let Some(name) = value.map(str::trim).filter(|v| !v.is_empty()) else {
            continue;
        };
        validate_name(name).map_err(|e| format!("{source}: {e}"))?;
        if !exists_in(home, name) {
            return Err(format!(
                "{source}: no profile `{name}` (expected {}). Profiles: {}",
                config_path_in(home, name).display(),
                list_in(home).join(", ")
            ));
        }
        return Ok((name.to_string(), None));
    }
    match read_persisted_in(home) {
        Some(name) if exists_in(home, &name) => Ok((name, None)),
        Some(name) => Ok((
            DEFAULT.to_string(),
            Some(format!(
                "active profile `{name}` no longer exists ({}); using `default`",
                config_path_in(home, &name).display()
            )),
        )),
        None => Ok((DEFAULT.to_string(), None)),
    }
}

/// The profile this process selected, tagged with the home it was
/// selected under. The tag keeps the selection from leaking into code
/// (tests) that points `NANOPI_HOME` somewhere else.
static CURRENT: RwLock<Option<(PathBuf, String)>> = RwLock::new(None);

fn home() -> Option<PathBuf> {
    crate::paths::nanopi_home()
}

/// Select `name` for the rest of this process (not persisted).
pub fn set_current(name: &str) {
    if let Some(h) = home() {
        *CURRENT.write().unwrap_or_else(|e| e.into_inner()) = Some((h, name.to_string()));
    }
}

/// The active profile: the process selection if one was made under the
/// current home, else the persisted one (if it still exists), else
/// `default`.
pub fn current() -> String {
    let Some(h) = home() else {
        return DEFAULT.to_string();
    };
    if let Some((sel_home, name)) = CURRENT.read().unwrap_or_else(|e| e.into_inner()).as_ref() {
        if *sel_home == h {
            return name.clone();
        }
    }
    read_persisted_in(&h)
        .filter(|n| exists_in(&h, n))
        .unwrap_or_else(|| DEFAULT.to_string())
}

/// The global config file of the active profile.
pub fn current_config_path() -> Option<PathBuf> {
    home().map(|h| config_path_in(&h, &current()))
}

/// Every profile under the current home.
pub fn list() -> Vec<String> {
    home().map(|h| list_in(&h)).unwrap_or_else(|| vec![DEFAULT.to_string()])
}

/// Create a profile under the current home, copying the active
/// profile's config file.
pub fn create(name: &str) -> Result<PathBuf, String> {
    let h = home().ok_or("cannot resolve ~/.nanopi")?;
    let from = config_path_in(&h, &current());
    create_in(&h, name, Some(&from))
}

/// Make `name` the active profile: persisted (survives restart) and
/// selected for this process. Fails, changing nothing, if it does not
/// exist.
pub fn switch(name: &str) -> Result<PathBuf, String> {
    let h = home().ok_or("cannot resolve ~/.nanopi")?;
    validate_name(name)?;
    if !exists_in(&h, name) {
        return Err(format!(
            "no profile `{name}`. Profiles: {} (create one with /profile create {name})",
            list_in(&h).join(", ")
        ));
    }
    persist_in(&h, name).map_err(|e| format!("cannot save active profile: {e}"))?;
    set_current(name);
    Ok(config_path_in(&h, name))
}

/// The connection settings a profile resolves to when switched to at
/// runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub model: String,
    pub base_url: String,
    pub api_key: String,
    pub api_kind: Option<String>,
    pub provider: Option<String>,
    pub inline_think_tags: Option<bool>,
}

/// Resolve a loaded config into the connection settings to switch to.
///
/// Unlike startup (flag > env > config), the PROFILE wins here and the
/// `OPENAI_*` env vars are only a fallback for keys the profile leaves
/// out: the user just asked for this profile by name, and an env var
/// set in the shell must not silently cancel the switch. CLI flags from
/// the original launch are not re-applied for the same reason.
pub fn resolve_endpoint(
    cfg: &Config,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Endpoint, String> {
    let model = cfg
        .model
        .clone()
        .or_else(|| env("OPENAI_MODEL"))
        .ok_or("the profile sets no `model` (and OPENAI_MODEL is unset)")?;
    let base_url = cfg
        .base_url
        .clone()
        .or_else(|| env("OPENAI_BASE_URL"))
        .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
    let api_key = match (&cfg.api_key, &cfg.api_key_file) {
        (Some(k), _) => k.clone(),
        (None, Some(p)) => {
            let path = crate::paths::expand_home(&p.to_string_lossy());
            std::fs::read_to_string(&path)
                .map(|s| s.trim().to_string())
                .map_err(|e| format!("cannot read api_key_file {}: {e}", path.display()))?
        }
        (None, None) => env("OPENAI_API_KEY")
            .ok_or("the profile sets no `api_key` / `api_key_file` (and OPENAI_API_KEY is unset)")?,
    };
    Ok(Endpoint {
        model,
        base_url,
        api_key,
        api_kind: cfg.api_kind.clone(),
        provider: cfg.provider.clone(),
        inline_think_tags: cfg.inline_think_tags,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load_config_from;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    /// A home with the legacy config.toml plus two profiles that differ
    /// in every connection setting and in an `[agent]` limit.
    fn home_with_two_profiles() -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        std::fs::write(
            h.join("config.toml"),
            "model = \"legacy-model\"\nbase_url = \"https://legacy.example/v1\"\napi_key = \"k-legacy\"\n",
        )
        .unwrap();
        let work = create_in(h, "work", None).unwrap();
        std::fs::write(
            &work,
            "model = \"work-model\"\nbase_url = \"https://work.example/v1\"\napi_key = \"k-work\"\n\
             [agent]\nmax_turns = 7\n",
        )
        .unwrap();
        let local = create_in(h, "local", None).unwrap();
        std::fs::write(
            &local,
            "model = \"local-model\"\nbase_url = \"http://localhost:11434/v1\"\napi_key = \"k-local\"\n\
             api_kind = \"openai\"\n",
        )
        .unwrap();
        home
    }

    /// Load the active profile's config the way `load_config` does,
    /// against an empty project directory.
    fn load_active(home: &Path, cwd: &Path) -> Config {
        let name = resolve_startup(home, None, None).unwrap().0;
        load_config_from(Some(&config_path_in(home, &name)), cwd).unwrap()
    }

    #[test]
    fn names_are_validated() {
        for ok in ["work", "a", "my-profile_2", &"x".repeat(64)] {
            assert!(validate_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", "../etc", "a/b", "a b", "a.toml", "émoji", &"x".repeat(65)] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn create_list_and_refusals() {
        let home = home_with_two_profiles();
        let h = home.path();
        assert_eq!(list_in(h), vec!["default", "local", "work"]);
        assert!(create_in(h, "work", None).unwrap_err().contains("already exists"));
        assert!(create_in(h, DEFAULT, None).is_err());
        assert!(create_in(h, "../x", None).is_err());
        // Stray files, invalid names and folders without a config.toml
        // (a cancelled wizard) are not profiles.
        std::fs::write(profiles_dir(h).join("notes.txt"), "").unwrap();
        std::fs::create_dir_all(profiles_dir(h).join("bad name")).unwrap();
        std::fs::write(profiles_dir(h).join("bad name/config.toml"), "").unwrap();
        let half = prepare_dir_in(h, "half").unwrap();
        assert_eq!(list_in(h), vec!["default", "local", "work"]);
        assert!(!exists_in(h, "half"));
        // ...and the wizard can reuse that folder.
        assert_eq!(prepare_dir_in(h, "half").unwrap(), half);
        assert!(prepare_dir_in(h, "work").unwrap_err().contains("already exists"));
    }

    #[test]
    fn each_profile_is_a_folder() {
        let home = home_with_two_profiles();
        let h = home.path();
        assert_eq!(dir_in(h, "work"), h.join("profiles/work"));
        assert_eq!(config_path_in(h, "work"), h.join("profiles/work/config.toml"));
        assert!(h.join("profiles/work/config.toml").is_file());
        assert_eq!(dir_in(h, DEFAULT), h);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir_in(h, "work")).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "the folder holds a key file");
        }
    }

    /// Flat files from the first version move into folders, keeping
    /// their contents; one whose folder already exists is left alone.
    #[test]
    fn flat_profile_files_migrate_into_folders() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        std::fs::create_dir_all(profiles_dir(h)).unwrap();
        std::fs::write(profiles_dir(h).join("office.toml"), "model = \"office-m\"\n").unwrap();
        std::fs::write(profiles_dir(h).join("test01.toml"), "model = \"t-m\"\n").unwrap();
        create_in(h, "both", None).unwrap();
        std::fs::write(profiles_dir(h).join("both.toml"), "model = \"old\"\n").unwrap();
        std::fs::write(profiles_dir(h).join("notes.txt"), "").unwrap();

        let notes = migrate_flat_in(h);
        assert_eq!(notes.len(), 3, "{notes:?}");
        assert!(notes.iter().any(|n| n.contains("`both`") && n.contains("not moved")));
        assert_eq!(list_in(h), vec!["default", "both", "office", "test01"]);
        let c = load_config_from(Some(&config_path_in(h, "office")), h).unwrap();
        assert_eq!(c.model.as_deref(), Some("office-m"));
        assert!(!profiles_dir(h).join("office.toml").exists());
        assert!(profiles_dir(h).join("both.toml").exists());
        assert!(migrate_flat_in(h).iter().all(|n| n.contains("`both`")), "idempotent");
    }

    #[test]
    fn create_copies_the_source_config() {
        let home = home_with_two_profiles();
        let h = home.path();
        let p = create_in(h, "copy", Some(&config_path_in(h, "work"))).unwrap();
        let cfg = load_config_from(Some(&p), home.path()).unwrap();
        assert_eq!(cfg.model.as_deref(), Some("work-model"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "a profile can hold an api key");
        }
    }

    /// Two profiles, switched back and forth: each switch loads exactly
    /// that profile's settings, and nothing carries over from the other
    /// (a key only `work` sets is gone after switching to `local`).
    #[test]
    fn switching_back_and_forth_keeps_profiles_isolated() {
        let home = home_with_two_profiles();
        let h = home.path();
        let cwd = tempfile::tempdir().unwrap();

        for _ in 0..2 {
            persist_in(h, "work").unwrap();
            let c = load_active(h, cwd.path());
            assert_eq!(c.model.as_deref(), Some("work-model"));
            assert_eq!(c.agent_config().max_turns, 7);
            assert_eq!(c.api_kind, None);
            let e = resolve_endpoint(&c, no_env).unwrap();
            assert_eq!(
                (e.base_url.as_str(), e.api_key.as_str()),
                ("https://work.example/v1", "k-work")
            );

            persist_in(h, "local").unwrap();
            let c = load_active(h, cwd.path());
            assert_eq!(c.model.as_deref(), Some("local-model"));
            assert_eq!(c.api_kind.as_deref(), Some("openai"));
            assert_eq!(
                c.agent_config().max_turns,
                crate::config::AgentConfig::default().max_turns,
                "work's [agent] max_turns must not leak into local"
            );
            let e = resolve_endpoint(&c, no_env).unwrap();
            assert_eq!(
                (e.base_url.as_str(), e.api_key.as_str()),
                ("http://localhost:11434/v1", "k-local")
            );
        }

        // Editing one profile leaves the other untouched.
        std::fs::write(config_path_in(h, "work"), "model = \"work-v2\"\n").unwrap();
        persist_in(h, "local").unwrap();
        assert_eq!(load_active(h, cwd.path()).model.as_deref(), Some("local-model"));
        persist_in(h, "work").unwrap();
        assert_eq!(load_active(h, cwd.path()).model.as_deref(), Some("work-v2"));
    }

    /// The persisted choice survives a "restart" (a fresh resolve from
    /// disk with no in-process state), `--profile` overrides it without
    /// changing it, and choosing `default` removes the file.
    #[test]
    fn active_profile_persists_across_reload() {
        let home = home_with_two_profiles();
        let h = home.path();
        assert_eq!(resolve_startup(h, None, None).unwrap().0, DEFAULT);

        persist_in(h, "work").unwrap();
        assert_eq!(resolve_startup(h, None, None).unwrap().0, "work");
        assert_eq!(resolve_startup(h, Some("local"), None).unwrap().0, "local");
        assert_eq!(resolve_startup(h, None, Some("local")).unwrap().0, "local");
        assert_eq!(resolve_startup(h, Some("work"), Some("local")).unwrap().0, "work");
        assert_eq!(read_persisted_in(h).as_deref(), Some("work"), "--profile is not persisted");

        persist_in(h, DEFAULT).unwrap();
        assert!(!h.join("active_profile").exists());
        assert_eq!(resolve_startup(h, None, None).unwrap().0, DEFAULT);
    }

    #[test]
    fn missing_profiles_error_or_fall_back() {
        let home = home_with_two_profiles();
        let h = home.path();
        let e = resolve_startup(h, Some("nope"), None).unwrap_err();
        assert!(e.contains("nope") && e.contains("work"), "{e}");
        assert!(resolve_startup(h, Some("../x"), None).is_err());
        // A persisted profile that was deleted warns and uses default.
        persist_in(h, "work").unwrap();
        std::fs::remove_file(config_path_in(h, "work")).unwrap();
        let (name, warn) = resolve_startup(h, None, None).unwrap();
        assert_eq!(name, DEFAULT);
        assert!(warn.unwrap().contains("work"));
    }

    /// With no profiles at all the legacy file is the global config,
    /// byte for byte the pre-profiles behaviour, and the project file is
    /// still merged over whichever profile is active.
    #[test]
    fn legacy_config_is_the_default_profile() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        std::fs::write(h.join("config.toml"), "model = \"legacy-model\"\n").unwrap();
        assert_eq!(list_in(h), vec![DEFAULT]);
        assert_eq!(config_path_in(h, DEFAULT), h.join("config.toml"));
        let cwd = tempfile::tempdir().unwrap();
        assert_eq!(load_active(h, cwd.path()).model.as_deref(), Some("legacy-model"));

        // Project-local config still layers on top of a profile.
        std::fs::create_dir_all(cwd.path().join(".nanopi")).unwrap();
        std::fs::write(cwd.path().join(".nanopi/config.toml"), "base_url = \"https://proj/v1\"\n")
            .unwrap();
        create_in(h, "work", Some(&h.join("config.toml"))).unwrap();
        persist_in(h, "work").unwrap();
        let c = load_active(h, cwd.path());
        assert_eq!(c.model.as_deref(), Some("legacy-model"));
        assert_eq!(c.base_url.as_deref(), Some("https://proj/v1"));
    }

    #[test]
    fn endpoint_prefers_the_profile_and_falls_back_to_env() {
        let env = |k: &str| match k {
            "OPENAI_MODEL" => Some("env-model".to_string()),
            "OPENAI_API_KEY" => Some("env-key".to_string()),
            _ => None,
        };
        let cfg: Config = toml::from_str("model = \"p-model\"\napi_key = \"p-key\"").unwrap();
        let e = resolve_endpoint(&cfg, env).unwrap();
        assert_eq!((e.model.as_str(), e.api_key.as_str()), ("p-model", "p-key"));
        assert_eq!(e.base_url, "https://api.openai.com/v1");

        let empty: Config = toml::from_str("").unwrap();
        let e = resolve_endpoint(&empty, env).unwrap();
        assert_eq!((e.model.as_str(), e.api_key.as_str()), ("env-model", "env-key"));
        assert!(resolve_endpoint(&empty, no_env).unwrap_err().contains("model"));

        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("key");
        std::fs::write(&key, "file-key\n").unwrap();
        let cfg: Config = toml::from_str(&format!(
            "model = \"m\"\napi_key_file = \"{}\"",
            key.display()
        ))
        .unwrap();
        assert_eq!(resolve_endpoint(&cfg, no_env).unwrap().api_key, "file-key");
    }

    /// The process-wide wiring: `switch` persists and selects, and
    /// `global_config_path` (which every loader uses) follows it.
    #[test]
    fn switch_moves_the_global_config_path() {
        let g = crate::TempNanopiHome::new();
        let h = g.path().to_path_buf();
        create_in(&h, "work", None).unwrap();
        assert_eq!(crate::paths::global_config_path(), Some(h.join("config.toml")));
        assert!(switch("nope").is_err());
        assert_eq!(current(), DEFAULT, "a failed switch changes nothing");

        switch("work").unwrap();
        assert_eq!(current(), "work");
        assert_eq!(read_persisted_in(&h).as_deref(), Some("work"));
        assert_eq!(crate::paths::global_config_path(), Some(config_path_in(&h, "work")));

        switch(DEFAULT).unwrap();
        assert_eq!(crate::paths::global_config_path(), Some(h.join("config.toml")));
    }
}
