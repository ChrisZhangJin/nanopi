//! Slash commands supplied by something other than the built-in list.
//!
//! Today that means WASM plugins, but nothing here knows about WASM —
//! and that is the point. `mode::tui` carries zero
//! `#[cfg(feature = "wasm")]`, and it has to stay that way: the slash
//! dispatcher is an exhaustive `match` with no wildcard arm, so a
//! conditionally-compiled variant would make it only conditionally
//! exhaustive and quietly cost the compile-time check that catches an
//! unhandled command. So the vocabulary lives here, unconditionally
//! compiled, and the plugin layer reaches the TUI as
//! `Arc<dyn CommandHandler>` — the same trick plugin *tools* already
//! use with `Arc<dyn Tool>`.
//!
//! Modeled on PI's extension commands
//! (`packages/coding-agent/src/core/extensions/loader.ts:258`
//! `registerCommand`, dispatched in `core/agent-session.ts:1270-1294`),
//! with two deliberate divergences forced by the sandbox — see
//! [`CommandAction`] and [`resolve_commands`].

use std::collections::HashMap;
use std::sync::Arc;

pub use crate::resources::DiagnosticLevel;

/// A command's identity, as advertised by whatever supplied it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub name: String,
    pub description: String,
}

/// What a command asks nanopi to do. This is the entire surface: no
/// host callbacks, no capabilities, three outcomes.
///
/// PI's handlers instead call back into the host (`pi.sendMessage`,
/// `ctx.ui.select`, `ctx.fork`, …) and their return value is ignored.
/// A sandboxed plugin cannot be handed those without new host imports,
/// so here the intent travels back as data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandAction {
    /// Write to the user's scrollback. Never reaches the model, and
    /// never enters the session transcript.
    Print(String),
    /// Start a turn with this text, as if the user had typed it. Always
    /// echoed verbatim first — nothing is said on the user's behalf
    /// invisibly.
    SendUserMessage(String),
    /// A user-level failure. Shown to the user, never to the model,
    /// matching PI, where a throwing handler is swallowed rather than
    /// becoming a prompt.
    Error(String),
}

/// Runs one command.
///
/// Synchronous, and potentially slow: an implementation may block for
/// as long as the plugin's epoch budget allows, and may block on a lock
/// before that budget even starts counting. Callers MUST move it off
/// the async runtime — see the command task in `mode::tui`.
pub trait CommandHandler: Send + Sync {
    fn run(&self, name: &str, args: &str) -> Result<CommandAction, String>;
}

/// A registered command plus who supplied it.
///
/// `Clone` so the TUI can snapshot the list rather than reach into the
/// Agent behind its async lock — the same reason `App::skills_cache`
/// exists.
#[derive(Clone)]
pub struct PluginCommand {
    pub spec: CommandSpec,
    /// Attribution for collision warnings and the palette row.
    pub plugin_name: Arc<str>,
    pub handler: Arc<dyn CommandHandler>,
}

impl std::fmt::Debug for PluginCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginCommand")
            .field("spec", &self.spec)
            .field("plugin_name", &self.plugin_name)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub struct CommandDiagnostic {
    pub level: DiagnosticLevel,
    pub message: String,
    /// Plugins involved — two or more for a plugin-vs-plugin collision.
    pub plugins: Vec<String>,
}

#[derive(Default)]
pub struct CommandLoadResult {
    pub commands: Vec<PluginCommand>,
    pub diagnostics: Vec<CommandDiagnostic>,
}

/// Names owned by built-in slash commands, which a plugin may not take.
///
/// Lives here rather than in `mode::tui` because registration happens
/// inside `Agent::build_fresh`, which knows nothing about the TUI and
/// also runs in print mode. `mode::tui` owns a test asserting this
/// stays in sync with `slash_items()` — that guard is what makes a
/// hand-maintained list safe.
pub const RESERVED_COMMAND_NAMES: &[&str] = &[
    "model",
    "new",
    "resume",
    "fork",
    "session",
    "name",
    "copy",
    "export",
    "import",
    "compact",
    "hotkeys",
    "skills",
    "tools",
    "reload",
    "settings",
    "keybindings",
    "quit",
    "exit",
];

const MAX_COMMAND_NAME_LENGTH: usize = 64;

/// Why a name is unusable, or `None` if it is fine.
///
/// Stricter than it looks, and each rule earns its place:
/// - whitespace would make the command unreachable, since both the
///   palette filter and the argument splitter cut at the first space;
/// - `:` would collide with the `/skill:<name>` namespace;
/// - `/` would break the palette's `strip_prefix('/')`.
///
/// PI validates none of this — a name with a space there is silently
/// registered and permanently unreachable. Refusing loudly is cheaper
/// than the bug report.
fn name_problem(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("a command name may not be empty".into());
    }
    let len = name.chars().count();
    if len > MAX_COMMAND_NAME_LENGTH {
        return Some(format!(
            "a command name may not exceed {MAX_COMMAND_NAME_LENGTH} characters (got {len})"
        ));
    }
    if name.chars().any(char::is_whitespace) {
        return Some("a command name may not contain whitespace".into());
    }
    if name.contains(':') {
        return Some("a command name may not contain ':' (reserved for /skill:<name>)".into());
    }
    if name.contains('/') {
        return Some("a command name may not contain '/'".into());
    }
    None
}

/// Decide which candidate commands may register.
///
/// Collisions are **refused, not renamed**. PI renames both claimants
/// to `name:1` / `name:2` (`extensions/runner.ts:605-636`), so the bare
/// name vanishes and — because the renamed pair escapes its
/// built-in-shadow filter — colliding twice ends up working better than
/// colliding once. nanopi fails loudly in-band everywhere else, so:
///
/// - a name owned by a built-in → that command does not register;
/// - a name claimed by two or more plugins → **none** of them register,
///   because silently picking a winner means `/deploy` runs whichever
///   plugin happened to load first.
///
/// A refusal is scoped to the one command. The plugin's other commands,
/// and all of its tools, are unaffected.
///
/// Pure: no I/O, no printing. Diagnostics come back as data so tests
/// can inspect them without noise, mirroring `LoadSkillsResult`.
pub fn resolve_commands(candidates: Vec<PluginCommand>) -> CommandLoadResult {
    let mut out = CommandLoadResult::default();

    // Pass 1: drop unusable names, then tally what is left. Tallying
    // has to happen after validation, or an invalid name could also
    // knock out a valid one it happens to match.
    let mut valid: Vec<PluginCommand> = Vec::new();
    let mut counts: HashMap<&str, Vec<String>> = HashMap::new();
    for cmd in candidates {
        if let Some(why) = name_problem(&cmd.spec.name) {
            out.diagnostics.push(CommandDiagnostic {
                level: DiagnosticLevel::Warning,
                message: format!(
                    "command {:?} from plugin {:?} is unusable — skipping it ({why})",
                    cmd.spec.name, cmd.plugin_name
                ),
                plugins: vec![cmd.plugin_name.to_string()],
            });
            continue;
        }
        valid.push(cmd);
    }
    for cmd in &valid {
        counts
            .entry(cmd.spec.name.as_str())
            .or_default()
            .push(cmd.plugin_name.to_string());
    }

    // Pass 2: admit or refuse. A duplicated name is diagnosed once, not
    // once per claimant, and the message names every one of them.
    let mut reported: Vec<&str> = Vec::new();
    for cmd in &valid {
        let name = cmd.spec.name.as_str();
        if RESERVED_COMMAND_NAMES.contains(&name) {
            out.diagnostics.push(CommandDiagnostic {
                level: DiagnosticLevel::Collision,
                message: format!(
                    "command \"/{name}\" from plugin {:?} collides with a built-in \
                     — skipping it (rename it in the plugin)",
                    cmd.plugin_name
                ),
                plugins: vec![cmd.plugin_name.to_string()],
            });
            continue;
        }
        let claimants = counts.get(name).map(Vec::as_slice).unwrap_or_default();
        if claimants.len() > 1 {
            if !reported.contains(&name) {
                reported.push(name);
                out.diagnostics.push(CommandDiagnostic {
                    level: DiagnosticLevel::Collision,
                    message: format!(
                        "command \"/{name}\" is claimed by plugins {} — none of them \
                         register it (rename it in all but one)",
                        claimants
                            .iter()
                            .map(|p| format!("{p:?}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    plugins: claimants.to_vec(),
                });
            }
            continue;
        }
        out.commands.push(cmd.clone());
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Enough of a handler to build a `PluginCommand` without any wasm.
    /// That these tests run in the DEFAULT build is itself the point:
    /// it proves the vocabulary carries no feature-gated types.
    struct FakeHandler(CommandAction);

    impl CommandHandler for FakeHandler {
        fn run(&self, _name: &str, _args: &str) -> Result<CommandAction, String> {
            Ok(self.0.clone())
        }
    }

    fn cmd(plugin: &str, name: &str) -> PluginCommand {
        PluginCommand {
            spec: CommandSpec {
                name: name.into(),
                description: format!("{name} from {plugin}"),
            },
            plugin_name: Arc::from(plugin),
            handler: Arc::new(FakeHandler(CommandAction::Print("ok".into()))),
        }
    }

    fn names(r: &CommandLoadResult) -> Vec<&str> {
        r.commands.iter().map(|c| c.spec.name.as_str()).collect()
    }

    #[test]
    fn a_unique_name_registers() {
        let r = resolve_commands(vec![cmd("a", "todo"), cmd("b", "deploy")]);
        assert_eq!(names(&r), vec!["todo", "deploy"]);
        assert!(r.diagnostics.is_empty(), "{:?}", r.diagnostics);
    }

    /// The rule that differs from PI: two claimants means NEITHER wins.
    /// Picking one would make `/todo` run whichever plugin happened to
    /// load first, which is not something a user can reason about.
    #[test]
    fn two_plugins_claiming_one_name_both_lose() {
        let r = resolve_commands(vec![
            cmd("alpha", "todo"),
            cmd("beta", "todo"),
            cmd("alpha", "other"),
        ]);
        assert_eq!(
            names(&r),
            vec!["other"],
            "the colliding name must vanish, the sibling must survive"
        );
        assert_eq!(
            r.diagnostics.len(),
            1,
            "one diagnostic, not one per claimant"
        );
        let d = &r.diagnostics[0];
        assert_eq!(d.level, DiagnosticLevel::Collision);
        assert!(
            d.message.contains("alpha") && d.message.contains("beta"),
            "{}",
            d.message
        );
        assert_eq!(d.plugins, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[test]
    fn three_way_collision_reports_once_and_names_all() {
        let r = resolve_commands(vec![cmd("a", "x"), cmd("b", "x"), cmd("c", "x")]);
        assert!(names(&r).is_empty());
        assert_eq!(r.diagnostics.len(), 1);
        for p in ["a", "b", "c"] {
            assert!(
                r.diagnostics[0].message.contains(p),
                "{}",
                r.diagnostics[0].message
            );
        }
    }

    /// A plugin must not be able to shadow `/compact` — that is how a
    /// plugin would quietly take over an existing workflow.
    #[test]
    fn a_builtin_name_is_refused() {
        let r = resolve_commands(vec![cmd("a", "compact"), cmd("a", "mine")]);
        assert_eq!(names(&r), vec!["mine"], "siblings are unaffected");
        assert_eq!(r.diagnostics.len(), 1);
        assert_eq!(r.diagnostics[0].level, DiagnosticLevel::Collision);
        assert!(r.diagnostics[0].message.contains("built-in"));
    }

    #[test]
    fn every_reserved_name_is_actually_reserved() {
        for n in RESERVED_COMMAND_NAMES {
            let r = resolve_commands(vec![cmd("a", n)]);
            assert!(names(&r).is_empty(), "{n} should have been refused");
        }
    }

    #[test]
    fn unusable_names_are_refused_with_a_reason() {
        let long = "x".repeat(65);
        for bad in ["", "my cmd", "my\tcmd", "skill:foo", "a/b", long.as_str()] {
            let r = resolve_commands(vec![cmd("a", bad)]);
            assert!(names(&r).is_empty(), "{bad:?} should have been refused");
            assert_eq!(r.diagnostics.len(), 1, "{bad:?}");
            assert_eq!(r.diagnostics[0].level, DiagnosticLevel::Warning);
        }
        // …and a name at the limit is fine.
        let ok = "x".repeat(64);
        assert_eq!(resolve_commands(vec![cmd("a", &ok)]).commands.len(), 1);
    }

    /// An invalid name must not be able to knock out a valid one it
    /// happens to match — validation has to run before the tally.
    #[test]
    fn an_invalid_duplicate_does_not_take_down_a_valid_name() {
        let r = resolve_commands(vec![cmd("a", "todo"), cmd("b", "todo x")]);
        assert_eq!(
            names(&r),
            vec!["todo"],
            "\"todo x\" is unusable, so \"todo\" is not actually contested"
        );
    }

    #[test]
    fn handlers_survive_resolution() {
        let mut c = cmd("a", "todo");
        c.handler = Arc::new(FakeHandler(CommandAction::SendUserMessage("hi".into())));
        let r = resolve_commands(vec![c]);
        assert_eq!(
            r.commands[0].handler.run("todo", "").unwrap(),
            CommandAction::SendUserMessage("hi".into())
        );
    }
}
