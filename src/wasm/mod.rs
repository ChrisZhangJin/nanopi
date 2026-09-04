//! WASM plugin host. Phase-1 skeleton (v0.11.0).
//!
//! See `.planning/quick/260828-wasm-plugin-system/PLAN.md` for the full
//! design. This module is only compiled when the `wasm` feature is
//! enabled (`cargo build --features wasm`), keeping the default
//! binary at its ~4 MB size.
//!
//! What this skeleton does today:
//!   - Defines the `PluginLoadSpec` data flow used by the agent build
//!     path (loads every `.wasm` declared in `Config::extensions`).
//!   - Wraps wasmtime so subsequent phases (register-tool, execute-tool
//!     host calls, network/fs gate) land in one place.
//!
//! What it does not yet do:
//!   - WIT interface (phase 2)
//!   - Tool dispatch / command routing (phase 3)

use std::path::Path;

use crate::config::ExtensionConfig;

/// Outcome of a `PluginHost::load_all(...)` call.
pub struct PluginLoadSummary {
    /// Tools ready to register into `ToolRegistry`. Already wrapped
    /// so the agent loop can call them like any built-in tool.
    pub tools: Vec<std::sync::Arc<dyn crate::tool::Tool>>,
    /// Slash commands the plugins advertised, still *candidates* —
    /// `command::resolve_commands` decides which may actually
    /// register, since a collision needs to see every claimant at
    /// once and cannot be judged one plugin at a time.
    pub commands: Vec<crate::command::PluginCommand>,
    /// Lifecycle-event subscribers each successfully-loaded plugin
    /// registered, ready to fold into `crate::subscriber::EventSubscribers`.
    /// Empty for a plugin whose `list-events` ∩ config `events` is empty
    /// (including plugins that export no `list-events` at all).
    pub subscribers: Vec<crate::subscriber::Subscriber>,
    /// How many `.wasm` files instantiated cleanly.
    pub loaded: usize,
    /// Per-file failures — path plus the reason. Non-fatal: a broken
    /// plugin is reported and skipped, it does not stop startup.
    pub errors: Vec<(std::path::PathBuf, String)>,
    /// Everything worth telling the user, COLLECTED rather than
    /// printed as it happens. Loading interleaves warnings with
    /// progress, so printing inline produced a flat wall in which a
    /// network-exfiltration warning looked exactly like `registered
    /// extension tool`. The caller renders these as one grouped block
    /// (`render::notice`).
    pub notices: Vec<crate::render::notice::Notice>,
}

/// Loads `.wasm` components declared in `[[extensions]]` and turns the
/// tools they export into registry-ready `Tool` impls.
pub struct PluginHost;

impl PluginHost {
    pub fn new() -> Self {
        Self
    }

    /// Resolve, compile, instantiate, and wrap every declared
    /// extension. Errors are collected per-file rather than
    /// short-circuiting: one malformed `.wasm` must not stop nanopi
    /// from starting with the rest.
    pub fn load_all(
        &self,
        configs: &[ExtensionConfig],
        cwd: &std::path::Path,
    ) -> PluginLoadSummary {
        let mut tools: Vec<std::sync::Arc<dyn crate::tool::Tool>> = Vec::new();
        let mut commands: Vec<crate::command::PluginCommand> = Vec::new();
        let mut subscribers: Vec<crate::subscriber::Subscriber> = Vec::new();
        let mut errors = Vec::new();
        let mut notices: Vec<crate::render::notice::Notice> = Vec::new();
        let mut loaded = 0usize;

        // One engine shared by every plugin — compiled code caches
        // inside it, and it's internally Arc-refcounted.
        let engine = match loader::PluginEngine::new() {
            Ok(e) => e,
            Err(e) => {
                // No engine means no plugins at all; report once
                // against the first configured path so the message
                // has somewhere to anchor.
                let anchor = configs
                    .first()
                    .map(|c| c.path.clone())
                    .unwrap_or_default();
                return PluginLoadSummary {
                    tools,
                    commands,
                    subscribers,
                    loaded: 0,
                    errors: vec![(anchor, e)],
                    notices: Vec::new(),
                };
            }
        };

        for cfg in configs {
            // Say it once per entry, before the paths expand — a
            // directory entry would otherwise repeat the warning per
            // `.wasm` and bury the rest of startup.
            if cfg.allow_network && loader::allowlist_allows_any_host(&cfg.url_allowlist) {
                notices.push(crate::render::notice::Notice::warn(
                    cfg.path.display().to_string(),
                    "has url_allowlist = [\"*\"] — this plugin may fetch ANY \
                     http/https host, including link-local metadata endpoints. \
                     Narrow it to `*.example.com` or `example.com` if you can.",
                ));
            }
            // Same idea for `events` + `allow_network`: a plugin that
            // both observes lifecycle events and can reach the network
            // can exfiltrate whatever those events carry — worth a
            // startup warning even though both capabilities are
            // individually opt-in (`docs/v0.12-events.md` §5.1).
            if !cfg.events.is_empty() && cfg.allow_network {
                notices.push(crate::render::notice::Notice::warn(
                    cfg.path.display().to_string(),
                    "has both `events` and `allow_network = true` — this plugin \
                     can observe lifecycle events AND reach the network, and \
                     could exfiltrate event payloads. Grant both only if you \
                     trust the plugin.",
                ));
            }
            let (events_granted, refusal_reports) = crate::agent::hook::parse_event_grants(&cfg.events);
            for report in &refusal_reports {
                notices.push(crate::render::notice::Notice::warn(
                    cfg.path.display().to_string(),
                    report.to_string(),
                ));
            }
            let events_granted: Vec<String> =
                events_granted.into_iter().map(|s| s.to_string()).collect();
            for path in self.resolve_paths(std::slice::from_ref(cfg)) {
                match engine.load(
                    &path,
                    cfg.url_allowlist.clone(),
                    cwd.to_path_buf(),
                    cfg.allow_fs,
                    cfg.allow_network,
                    events_granted.clone(),
                ) {
                    Ok((bridge, specs)) => {
                        let plugin_name: std::sync::Arc<str> = path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("wasm-plugin")
                            .into();
                        let plugin_path: std::sync::Arc<str> =
                            path.display().to_string().into();
                        for spec in specs {
                            tools.push(std::sync::Arc::new(host::WasmTool::new(
                                spec,
                                plugin_name.clone(),
                                plugin_path.clone(),
                                bridge.clone(),
                            )));
                        }
                        // One handler per plugin, shared by its
                        // commands — they all dispatch through the same
                        // bridge, and the name is checked there.
                        if !bridge.command_specs().is_empty() {
                            let handler: std::sync::Arc<dyn crate::command::CommandHandler> =
                                std::sync::Arc::new(host::WasmCommandHandler::new(bridge.clone()));
                            for spec in bridge.command_specs() {
                                commands.push(crate::command::PluginCommand {
                                    spec,
                                    plugin_name: plugin_name.clone(),
                                    handler: handler.clone(),
                                });
                            }
                        }
                        for unsatisfied in bridge.unsatisfied_event_requests() {
                            notices.push(crate::render::notice::Notice::warn(
                                path.display().to_string(),
                                format!(
                                    "requested event {unsatisfied:?} but the config's \
                                     `events` did not grant it — not delivered."
                                ),
                            ));
                        }
                        let subscribed = bridge.event_subscriptions();
                        if !subscribed.is_empty() {
                            let events: Vec<&'static str> = subscribed
                                .iter()
                                .filter_map(|e| {
                                    crate::agent::hook::EVENT_NAMES
                                        .iter()
                                        .find(|&&n| n == e.as_str())
                                        .copied()
                                })
                                .collect();
                            notices.push(crate::render::notice::Notice::info(
                                path.display().to_string(),
                                format!("registered for events: {}", events.join(", ")),
                            ));
                            let handler: std::sync::Arc<dyn crate::subscriber::EventHandler> =
                                std::sync::Arc::new(host::WasmEventHandler::new(bridge.clone()));
                            subscribers.push(crate::subscriber::Subscriber {
                                plugin_name: plugin_name.clone(),
                                events,
                                handler,
                            });
                        }
                        loaded += 1;
                    }
                    Err(e) => errors.push((path, e)),
                }
            }
        }

        PluginLoadSummary {
            notices,
            tools,
            commands,
            subscribers,
            loaded,
            errors,
        }
    }

    /// Resolve every `[[extensions]]` entry into a list of `.wasm`
    /// files. A directory entry expands to up to `max_files`
    /// `.wasm` files (1-level scan, not recursive — defense in
    /// depth against a misconfigured glob pulling half the FS).
    ///
    /// Returns the count actually loaded vs skipped. This is the
    /// only behavior needed before phase-2 wires wasmtime; later
    /// phases replace this with a full `wasmtime::Engine` /
    /// `Component` instantiate.
    pub fn resolve_paths(
        &self,
        configs: &[ExtensionConfig],
    ) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        for cfg in configs {
            let expanded = expand_path(&cfg.path);
            if expanded.is_dir() {
                match std::fs::read_dir(&expanded) {
                    Ok(rd) => {
                        let mut count = 0;
                        for entry in rd.flatten() {
                            if count >= cfg.max_files {
                                break;
                            }
                            let p = entry.path();
                            if p.extension().and_then(|e| e.to_str()) == Some("wasm") {
                                out.push(p);
                                count += 1;
                            }
                        }
                    }
                    Err(e) => {
                        crate::note!(
                            "nanopi: skipping extension dir {}: {e}",
                            expanded.display()
                        );
                    }
                }
            } else if expanded.is_file() {
                if expanded.extension().and_then(|e| e.to_str()) == Some("wasm") {
                    out.push(expanded);
                } else {
                    crate::note!(
                        "nanopi: skipping extension (not .wasm): {}",
                        expanded.display()
                    );
                }
            } else {
                crate::note!(
                    "nanopi: skipping extension (not found): {}",
                    expanded.display()
                );
            }
        }
        out
    }
}

impl Default for PluginHost {
    fn default() -> Self {
        Self::new()
    }
}

/// Expand `~/` and `$HOME/` prefixes in a path. Same convention as
/// `agent::hook::expand_command` — duplicate the few lines rather
/// than couple two subsystems.
fn expand_path(p: &Path) -> std::path::PathBuf {
    let s = p.to_string_lossy();
    let expanded = if let Some(stripped) = s.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            format!("{}/{}", home.to_string_lossy(), stripped)
        } else {
            s.into_owned()
        }
    } else if s.starts_with("${HOME}/") || s.starts_with("$HOME/") {
        if let Some(home) = std::env::var_os("HOME") {
            let stripped = s
                .trim_start_matches("${HOME}/")
                .trim_start_matches("$HOME/");
            format!("{}/{}", home.to_string_lossy(), stripped)
        } else {
            s.into_owned()
        }
    } else {
        s.into_owned()
    };
    std::path::PathBuf::from(expanded)
}

pub mod host;
pub mod loader;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "nanopi-wasm-test-{}-{}",
            std::process::id(),
            crate::util::uuid::v7()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn resolve_nonexistent_logs_and_skips() {
        let host = PluginHost::new();
        let cfg = ExtensionConfig {
            path: std::path::PathBuf::from("/does/not/exist.wasm"),
            ..Default::default()
        };
        let got = host.resolve_paths(&[cfg]);
        assert!(got.is_empty());
    }

    #[test]
    fn resolve_directory_picks_up_wasm_files() {
        let dir = tmp();
        // Create two .wasm files + one non-.wasm file.
        for name in ["a.wasm", "b.wasm", "skip.txt"] {
            let mut f = std::fs::File::create(dir.join(name)).unwrap();
            writeln!(f, "fake").unwrap();
        }
        let cfg = ExtensionConfig {
            path: dir.clone(),
            max_files: 64,
            allow_network: false,
            allow_fs: false,
            url_allowlist: Vec::new(),
            events: Vec::new(),
        };
        let got = PluginHost::new().resolve_paths(&[cfg]);
        assert_eq!(got.len(), 2);
        // Both should be .wasm files inside `dir`.
        for p in &got {
            assert_eq!(p.extension().and_then(|e| e.to_str()), Some("wasm"));
            assert_eq!(p.parent(), Some(dir.as_path()));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_directory_respects_max_files_cap() {
        let dir = tmp();
        for n in 0..5 {
            let mut f = std::fs::File::create(dir.join(format!("x{n}.wasm"))).unwrap();
            writeln!(f).unwrap();
        }
        let cfg = ExtensionConfig {
            path: dir.clone(),
            max_files: 2,
            ..Default::default()
        };
        let got = PluginHost::new().resolve_paths(&[cfg]);
        assert_eq!(got.len(), 2, "max_files should cap the count");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
