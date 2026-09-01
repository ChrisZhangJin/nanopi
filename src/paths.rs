//! Shared filesystem-path helpers for `~/.nanopi`.
//!
//! Consolidates the `NANOPI_HOME` / `~/.nanopi` resolution that was
//! previously reimplemented in `config`, `settings`, `trust`,
//! `agent::permission`, and `mode::interactive`. The rule everywhere:
//! `NANOPI_HOME` (if set) is the config root — treat it as
//! equivalent-to `~/.nanopi`, NOT as a home dir. Otherwise fall back
//! to `$HOME/.nanopi`.
//!
//! Skill discovery adds two more locations, mirroring PI's model:
//! - user   `<nanopi_home>/skills`
//! - project `<cwd>/.nanopi/skills`
//!
//! `expand_home` is the single place that turns a user-written `~/…`
//! into a real path. It lives here so writers (the wizard) and readers
//! (`main`, `agent::hook`) can't disagree about where `~` points — they
//! did, and on Windows that broke every first run: `dirs::home_dir()`
//! resolved the profile folder while the reader looked only at `$HOME`,
//! which cmd and PowerShell never set.

use std::path::{Path, PathBuf};

/// The nanopi config root. `NANOPI_HOME` overrides for test isolation.
/// Returns `None` only when both `NANOPI_HOME` is unset and no home
/// dir can be resolved.
pub fn nanopi_home() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("NANOPI_HOME") {
        return Some(PathBuf::from(p));
    }
    Some(dirs::home_dir()?.join(".nanopi"))
}

/// The user's home directory. `HOME` wins when set and non-empty (it's
/// what shells and `~` mean to the user); otherwise `dirs::home_dir()`,
/// which is the only thing that works on Windows — cmd and PowerShell
/// set `USERPROFILE`, never `HOME`.
pub fn home_dir() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("HOME") {
        if !h.is_empty() {
            return Some(PathBuf::from(h));
        }
    }
    dirs::home_dir()
}

/// Expand a leading `~`, `$HOME`, or `${HOME}` in a config-supplied
/// path string. Both `/` and `\` separators are accepted after the
/// prefix so hand-written Windows configs work.
///
/// `~/.nanopi/...` resolves through [`nanopi_home`], so a redirected
/// `NANOPI_HOME` stays consistent with everything else that writes
/// there. Anything else resolves through [`home_dir`].
///
/// Best-effort: an unrecognized prefix, or a home dir that can't be
/// resolved at all, yields the input unchanged.
pub fn expand_home(s: &str) -> PathBuf {
    expand_against(s, nanopi_home(), home_dir())
}

/// The body of [`expand_home`], with both roots injected. Tests drive
/// this directly rather than mutating `NANOPI_HOME`, which would race
/// the other env-mutating tests in this crate.
fn expand_against(s: &str, nanopi_root: Option<PathBuf>, home: Option<PathBuf>) -> PathBuf {
    let Some(rest) = strip_home_prefix(s) else {
        return PathBuf::from(s);
    };
    // `~/.nanopi/x` must land wherever the rest of nanopi writes, which
    // is NANOPI_HOME when set — not $HOME/.nanopi.
    if let Some(inner) = strip_any(rest, &[".nanopi/", ".nanopi\\"]) {
        if let Some(root) = nanopi_root {
            return root.join(sep_to_native(inner));
        }
    }
    match home {
        Some(h) => h.join(sep_to_native(rest)),
        None => PathBuf::from(s),
    }
}

/// Strip a home-denoting prefix, returning the remainder. Bare `~` and
/// `$HOME` with no trailing separator mean the home dir itself.
fn strip_home_prefix(s: &str) -> Option<&str> {
    if s == "~" || s == "$HOME" || s == "${HOME}" {
        return Some("");
    }
    strip_any(
        s,
        &["~/", "~\\", "${HOME}/", "${HOME}\\", "$HOME/", "$HOME\\"],
    )
}

fn strip_any<'a>(s: &'a str, prefixes: &[&str]) -> Option<&'a str> {
    prefixes.iter().find_map(|p| s.strip_prefix(p))
}

/// Rewrite `\` to `/` on Unix so a Windows-style tail still joins into
/// one path per component instead of a single weird filename. On
/// Windows both separators are already understood, so leave it alone.
fn sep_to_native(s: &str) -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(s)
    }
    #[cfg(not(windows))]
    {
        PathBuf::from(s.replace('\\', "/"))
    }
}

/// The global `config.toml`.
pub fn global_config_path() -> Option<PathBuf> {
    nanopi_home().map(|h| h.join("config.toml"))
}

/// The global `settings.toml`.
pub fn global_settings_path() -> Option<PathBuf> {
    nanopi_home().map(|h| h.join("settings.toml"))
}

/// Rustyline history file.
pub fn history_path() -> Option<PathBuf> {
    nanopi_home().map(|h| h.join("history.txt"))
}

/// Directory holding `<cwd-key>=trusted|denied` markers.
pub fn trust_dir() -> Option<PathBuf> {
    nanopi_home().map(|h| h.join("trust"))
}

/// User-scope skills root.
pub fn user_skills_dir() -> Option<PathBuf> {
    nanopi_home().map(|h| h.join("skills"))
}

/// Project-scope skills root inside a given cwd.
pub fn project_skills_dir(cwd: &Path) -> PathBuf {
    cwd.join(".nanopi").join("skills")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nanopi_home_honors_env() {
        let _g = crate::TEST_LOCK.lock().unwrap();
        let prev = std::env::var_os("NANOPI_HOME");
        std::env::set_var("NANOPI_HOME", "/tmp/nanopi-test-home");

        assert_eq!(nanopi_home(), Some(PathBuf::from("/tmp/nanopi-test-home")));
        assert_eq!(
            global_config_path(),
            Some(PathBuf::from("/tmp/nanopi-test-home/config.toml"))
        );
        assert_eq!(
            user_skills_dir(),
            Some(PathBuf::from("/tmp/nanopi-test-home/skills"))
        );

        if let Some(p) = prev {
            std::env::set_var("NANOPI_HOME", p);
        } else {
            std::env::remove_var("NANOPI_HOME");
        }
    }

    #[test]
    fn project_skills_is_dot_nanopi_skills() {
        let d = project_skills_dir(Path::new("/tmp/proj"));
        assert_eq!(d, PathBuf::from("/tmp/proj/.nanopi/skills"));
    }

    /// `expand_against` with fixed roots — no env mutation, so these
    /// stay deterministic alongside the crate's env-mutating tests.
    fn expand(s: &str) -> PathBuf {
        expand_against(
            s,
            Some(PathBuf::from("/np-root")),
            Some(PathBuf::from("/home/u")),
        )
    }

    #[test]
    fn expand_home_resolves_tilde_and_home_vars() {
        for input in ["~/foo/bar.sh", "$HOME/foo/bar.sh", "${HOME}/foo/bar.sh"] {
            assert_eq!(
                expand(input),
                PathBuf::from("/home/u/foo/bar.sh"),
                "{input}"
            );
        }
        for input in ["~", "$HOME", "${HOME}"] {
            assert_eq!(expand(input), PathBuf::from("/home/u"), "{input}");
        }
    }

    #[test]
    fn expand_home_accepts_backslash_separators() {
        // Hand-written Windows configs. The tail must split into
        // components, not become one filename with backslashes in it.
        assert_eq!(expand(r"~\foo\bar.sh"), PathBuf::from("/home/u/foo/bar.sh"));
    }

    #[test]
    fn expand_home_routes_dot_nanopi_through_nanopi_home() {
        // Regression: the wizard wrote the key under NANOPI_HOME but
        // the reader expanded `~/.nanopi/api_key` to $HOME/.nanopi,
        // so the two disagreed whenever NANOPI_HOME was set.
        assert_eq!(
            expand("~/.nanopi/api_key"),
            PathBuf::from("/np-root/api_key")
        );
        assert_eq!(
            expand(r"~\.nanopi\api_key"),
            PathBuf::from("/np-root/api_key")
        );
    }

    #[test]
    fn expand_home_passes_through_absolute_and_relative() {
        assert_eq!(expand("/etc/keys/x"), PathBuf::from("/etc/keys/x"));
        assert_eq!(expand("keys/x"), PathBuf::from("keys/x"));
        // A `~` that isn't a home prefix is an ordinary filename.
        assert_eq!(expand("~backup/x"), PathBuf::from("~backup/x"));
    }

    #[test]
    fn expand_home_unresolvable_roots_pass_through() {
        assert_eq!(
            expand_against("~/.nanopi/api_key", None, None),
            PathBuf::from("~/.nanopi/api_key")
        );
        // No NANOPI_HOME, but a usable $HOME: fall back to $HOME/.nanopi.
        assert_eq!(
            expand_against("~/.nanopi/api_key", None, Some(PathBuf::from("/home/u"))),
            PathBuf::from("/home/u/.nanopi/api_key")
        );
    }
}
