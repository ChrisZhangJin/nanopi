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
    /// What each successfully-loaded plugin was granted, rendered at
    /// load for `/tools`'s grant section. One row per loaded plugin,
    /// including plugins granted nothing — see
    /// `crate::plugin_grants::PluginGrants`.
    pub grants: Vec<crate::plugin_grants::PluginGrants>,
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

/// Render one plugin's config into the short grant tokens `/tools`
/// shows. Built HERE, at load, rather than in the TUI: the TUI can
/// only see the config on disk, which is not necessarily the config
/// the running plugin was loaded under, and `ExtensionConfig` is
/// behind the feature flag anyway.
///
/// A grant that is off contributes nothing — the row lists what a
/// plugin HAS, and an operator scanning for `bash` should not have to
/// read past four `no`s per plugin to find it. Order is fixed
/// (filesystem, network, store, context, tools, events) so two rows
/// are comparable at a glance.
fn grant_tokens(cfg: &ExtensionConfig) -> Vec<String> {
    let mut t = Vec::new();
    if cfg.allow_fs {
        t.push("allow_fs".to_string());
    }
    if cfg.allow_network {
        // The allowlist is what actually bounds the reach, so it goes
        // in the token: `allow_network` alone reaches nothing, and
        // `allow_network(*)` reaches everything. Those must not look
        // the same in a row an operator is scanning for risk.
        t.push(if cfg.url_allowlist.is_empty() {
            "allow_network(no allowlist, reaches nothing)".to_string()
        } else {
            format!("allow_network({})", cfg.url_allowlist.join(", "))
        });
    }
    if cfg.allow_store {
        t.push("allow_store".to_string());
    }
    if cfg.allow_context {
        t.push("allow_context".to_string());
    }
    if !cfg.allow_tools.is_empty() {
        t.push(format!("allow_tools({})", cfg.allow_tools.join(", ")));
    }
    // The one grant that spends money, so it is spelled out rather than
    // left as a bare flag among six others an operator is scanning
    // past. `/tools` is where a user checks what they have installed;
    // "this plugin can bill you" should not read the same as "this
    // plugin can read a file".
    if cfg.allow_send_message {
        t.push("allow_send_message(can start billed turns)".to_string());
    }
    if !cfg.events.is_empty() {
        t.push(format!("events({})", cfg.events.join(", ")));
    }
    t
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
        let mut grants: Vec<crate::plugin_grants::PluginGrants> = Vec::new();
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
                    grants: Vec::new(),
                    loaded: 0,
                    errors: vec![(anchor, e)],
                    notices: Vec::new(),
                };
            }
        };

        // Store identity is the `.wasm` file stem, so two plugins with
        // the same stem would share one `store.json` — one plugin
        // silently reading and overwriting another's keys, which is
        // exactly the cross-plugin disclosure the per-stem directory
        // exists to prevent. Decided as a HARD LOAD ERROR for BOTH
        // claimants, not "the later one loses": that is the precedent
        // the command-name collision already set (`NEITHER registers`,
        // see `wit/nanopi-extension.wit`), and it is the safe
        // direction — with "later loses", which plugin gets the store
        // depends on config order, so a user reordering their config
        // would silently hand one plugin's memory to another.
        //
        // Computed across ALL configs before the per-path loop,
        // because a collision is not visible while looking at one
        // plugin: it needs every claimant at once, same as commands.
        // Stems only collide destructively when at least one claimant
        // actually has `allow_store` — two stem-sharing plugins with
        // the grant off touch no store at all and load normally.
        let colliding_stems = colliding_store_stems(self, configs);

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
            // And again for `allow_store` + `allow_network`
            // (`docs/plugin-capabilities.md` §3): a durable profile of
            // the user that can leave the machine. Same placement,
            // before the paths expand, for the same reason.
            if cfg.allow_store && cfg.allow_network {
                notices.push(crate::render::notice::Notice::warn(
                    cfg.path.display().to_string(),
                    "has both `allow_store` and `allow_network = true` — this \
                     plugin can build a durable profile across restarts AND \
                     reach the network, so anything it accumulates can leave \
                     the machine. Grant both only if you trust the plugin.",
                ));
            }
            // And `allow_context` + `allow_network` (§3), which is the
            // sharpest of the three: a plugin that can fetch text AND
            // put it in the model's system prompt can shape the
            // agent's behaviour from a remote source it controls,
            // without ever touching a tool. Same placement, before the
            // paths expand, so a directory entry warns once rather
            // than once per file.
            if cfg.allow_context && cfg.allow_network {
                notices.push(crate::render::notice::Notice::warn(
                    cfg.path.display().to_string(),
                    "has both `allow_context` and `allow_network = true` — this \
                     plugin can fetch text from the network AND place it in the \
                     model's system prompt, so a remote source can shape how \
                     the agent behaves. Grant both only if you trust the plugin \
                     and the hosts in its url_allowlist.",
                ));
            }
            // `allow_tools` containing `bash` is §3's escalated
            // combination and the sharpest grant in the whole file:
            // arbitrary execution walks past `allow_fs`'s cwd
            // confinement and past `url_allowlist`'s per-host
            // approval, which makes every other grant on this plugin
            // decorative. Warned ALONE, unlike the `allow_network`
            // pairs above, because nothing needs to be paired with it.
            //
            // Following the `allow_context`-alone precedent in the
            // other direction: a plugin with `allow_tools = ["read"]`
            // and no `bash` does NOT warn, or the warning becomes noise
            // the user learns to skip past.
            if cfg.allow_tools.iter().any(|t| t == "bash") {
                notices.push(crate::render::notice::Notice::warn(
                    cfg.path.display().to_string(),
                    "has `bash` in allow_tools — this plugin can execute                      arbitrary commands, which walks past allow_fs's cwd                      confinement and url_allowlist's per-host approval and                      makes every other grant on it decorative. Grant it only                      if you trust the plugin completely.",
                ));
            }
            // Warned ALONE, like `bash` and unlike the `allow_network`
            // pairs, and for the same kind of reason: it does not need
            // a partner to be dangerous. Every other grant lets a
            // plugin learn something, change what the agent believes,
            // or run a tool the user could have run themselves. This
            // one causes BILLED TURNS. The `allow_context`-alone
            // precedent does not apply — that grant is only sharp in
            // combination, so warning on it alone would be the noise
            // users learn to skip; this one is sharp by itself.
            if cfg.allow_send_message {
                notices.push(crate::render::notice::Notice::warn(
                    cfg.path.display().to_string(),
                    "has allow_send_message = true — this plugin can start \
                     turns on its own, which SPENDS MONEY against your \
                     provider. Turns it causes are always echoed to you and \
                     capped per session, but the cap is a backstop, not a \
                     budget. Grant it only to plugins you want driving the \
                     agent.",
                ));
            }
            // A name in `allow_tools` that is not a built-in is a LOAD
            // ERROR for this entry, not a silent no-op — the same rule
            // a retired hook key follows. Checked against
            // `ToolRegistry::standard()`, the CLOSED built-in set,
            // which is also why a plugin-supplied name is unknowable
            // here and has to be refused at call time instead.
            let builtins = crate::tool::ToolRegistry::standard().names();
            let unknown: Vec<String> = cfg
                .allow_tools
                .iter()
                .filter(|t| !builtins.iter().any(|b| b == *t))
                .cloned()
                .collect();
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
                let plugin_name: std::sync::Arc<str> = plugin_stem(&path);
                if !unknown.is_empty() {
                    errors.push((
                        path.clone(),
                        format!(
                            "allow_tools names {} — host-call-tool reaches                              built-in tools only, and the built-ins are: {}.                              A misspelled grant is refused rather than                              silently granting nothing.",
                            unknown
                                .iter()
                                .map(|t| format!("{t:?}"))
                                .collect::<Vec<_>>()
                                .join(", "),
                            builtins.join(", "),
                        ),
                    ));
                    continue;
                }
                if let Some(others) = colliding_stems.get(&*plugin_name) {
                    errors.push((
                        path.clone(),
                        format!(
                            "file stem {:?} is claimed by more than one extension \
                             ({}), and at least one of them sets \
                             `allow_store` — they would share the single store \
                             at {}. Refusing ALL of them rather than letting one \
                             read and overwrite another's keys; rename one of \
                             the `.wasm` files.",
                            &*plugin_name,
                            others.join(", "),
                            store::PluginStore::new(
                                store::PluginStore::default_root(),
                                &plugin_name
                            )
                            .file()
                            .display(),
                        ),
                    ));
                    continue;
                }
                match engine.load(
                    &path,
                    cfg.url_allowlist.clone(),
                    cwd.to_path_buf(),
                    cfg.allow_fs,
                    cfg.allow_network,
                    cfg.allow_store,
                    cfg.allow_context,
                    cfg.allow_tools.clone(),
                    cfg.allow_send_message,
                    store::PluginStore::default_root(),
                    plugin_name.clone(),
                    events_granted.clone(),
                ) {
                    Ok((bridge, specs)) => {
                        let plugin_path: std::sync::Arc<str> =
                            path.display().to_string().into();
                        grants.push(crate::plugin_grants::PluginGrants {
                            plugin_name: plugin_name.to_string(),
                            path: path.display().to_string(),
                            grants: grant_tokens(cfg),
                        });
                        // `/tools` now carries the full grant row
                        // (`plugin-capabilities.md` §3), so this notice
                        // is no longer the only place a grant is
                        // visible. It stays because it names the store
                        // FILE, which a one-line `/tools` row does not,
                        // and because it lands at startup rather than
                        // waiting for the user to think to ask.
                        if cfg.allow_store {
                            notices.push(crate::render::notice::Notice::info(
                                path.display().to_string(),
                                format!(
                                    "allow_store — keyed store at {}",
                                    store::PluginStore::new(
                                        store::PluginStore::default_root(),
                                        &plugin_name
                                    )
                                    .file()
                                    .display()
                                ),
                            ));
                        }
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
            grants,
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

/// The plugin's identity: its `.wasm` file stem. One helper so the
/// store directory name, the `host-notify` attribution, and the
/// collision check cannot drift apart.
fn plugin_stem(path: &Path) -> std::sync::Arc<str> {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("wasm-plugin")
        .into()
}

/// Stems claimed by more than one resolved `.wasm` path where at least
/// one claimant has `allow_store`, mapped to every path claiming them.
///
/// Returns an empty map in the ordinary case, so the per-path loop pays
/// one hash lookup for a check that almost never fires.
fn colliding_store_stems(
    host: &PluginHost,
    configs: &[ExtensionConfig],
) -> std::collections::HashMap<String, Vec<String>> {
    use std::collections::HashMap;
    // stem -> (paths claiming it, whether any claimant grants the store)
    let mut claims: HashMap<String, (Vec<String>, bool)> = HashMap::new();
    for cfg in configs {
        for path in host.resolve_paths(std::slice::from_ref(cfg)) {
            let stem = plugin_stem(&path).to_string();
            let entry = claims.entry(stem).or_insert_with(|| (Vec::new(), false));
            entry.0.push(path.display().to_string());
            entry.1 |= cfg.allow_store;
        }
    }
    claims
        .into_iter()
        .filter(|(_, (paths, granted))| paths.len() > 1 && *granted)
        .map(|(stem, (paths, _))| (stem, paths))
        .collect()
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
pub mod notify;
pub mod store;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

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
    fn a_plugin_granted_nothing_produces_no_tokens() {
        let cfg = ExtensionConfig::default();
        assert!(
            grant_tokens(&cfg).is_empty(),
            "an ungranted plugin contributes no tokens — the ROW still \
             renders, reading `no grants`, but that is the display \
             layer's job, not this function's: {:?}",
            grant_tokens(&cfg)
        );
    }

    #[test]
    fn every_grant_renders_once_in_a_fixed_order() {
        let cfg = ExtensionConfig {
            allow_fs: true,
            allow_network: true,
            url_allowlist: vec!["github.com".to_string()],
            allow_store: true,
            allow_context: true,
            allow_tools: vec!["find".to_string(), "read".to_string()],
            events: vec!["turn_start".to_string()],
            ..Default::default()
        };
        assert_eq!(
            grant_tokens(&cfg),
            vec![
                "allow_fs".to_string(),
                "allow_network(github.com)".to_string(),
                "allow_store".to_string(),
                "allow_context".to_string(),
                "allow_tools(find, read)".to_string(),
                "events(turn_start)".to_string(),
            ]
        );
    }

    #[test]
    fn allow_network_without_an_allowlist_says_it_reaches_nothing() {
        // `allow_network = true` with an empty allowlist reaches no
        // host at all. A bare `allow_network` token would read as the
        // opposite to anyone scanning the row for exfiltration risk.
        let cfg = ExtensionConfig {
            allow_network: true,
            ..Default::default()
        };
        assert_eq!(
            grant_tokens(&cfg),
            vec!["allow_network(no allowlist, reaches nothing)".to_string()]
        );
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
            allow_store: false,
            allow_context: false,
            allow_tools: Vec::new(),
            allow_send_message: false,
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

    /// Two `[[extensions]]` entries in different directories whose
    /// `.wasm` files share a file stem. The bytes are not real
    /// components — that is deliberate: the collision must be refused
    /// BEFORE anything is compiled, so a load error naming the
    /// collision (not a compile error) is the assertion.
    fn same_stem_configs(allow_store_a: bool, allow_store_b: bool) -> (PathBuf, Vec<ExtensionConfig>) {
        let root = tmp();
        let mut cfgs = Vec::new();
        for (sub, grant) in [("a", allow_store_a), ("b", allow_store_b)] {
            let dir = root.join(sub);
            std::fs::create_dir_all(&dir).unwrap();
            let mut f = std::fs::File::create(dir.join("mem.wasm")).unwrap();
            writeln!(f, "not a component").unwrap();
            cfgs.push(ExtensionConfig {
                path: dir.join("mem.wasm"),
                allow_store: grant,
                ..Default::default()
            });
        }
        (root, cfgs)
    }

    #[test]
    fn same_stem_with_allow_store_is_a_load_error_for_both() {
        let (root, cfgs) = same_stem_configs(true, false);
        let summary = PluginHost::new().load_all(&cfgs, &root);
        assert_eq!(
            summary.errors.len(),
            2,
            "the command-name precedent is NEITHER registers, so both are \
             refused — not just the later one: {:?}",
            summary.errors
        );
        for (path, err) in &summary.errors {
            assert!(
                err.contains("file stem") && err.contains("allow_store"),
                "the error must name the collision and the grant, not read as \
                 a compile failure — {}: {err}",
                path.display()
            );
            assert!(
                err.contains("store.json"),
                "and must point at the store they would have shared: {err}"
            );
        }
        assert_eq!(summary.loaded, 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn same_stem_without_allow_store_is_not_a_collision() {
        let (root, cfgs) = same_stem_configs(false, false);
        let summary = PluginHost::new().load_all(&cfgs, &root);
        // Both still fail — they are not real components — but for
        // COMPILING, not for colliding. Two stem-sharing plugins that
        // touch no store have nothing to collide over.
        for (_, err) in &summary.errors {
            assert!(
                !err.contains("file stem"),
                "no store, no collision: {err}"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn allow_store_plus_allow_network_warns() {
        let root = tmp();
        let cfg = ExtensionConfig {
            path: root.join("nope.wasm"),
            allow_store: true,
            allow_network: true,
            ..Default::default()
        };
        let summary = PluginHost::new().load_all(&[cfg], &root);
        let warned = summary.notices.iter().any(|n| {
            n.level == crate::render::notice::Level::Warn
                && n.message.contains("allow_store")
                && n.message.contains("allow_network")
        });
        assert!(
            warned,
            "the escalated warning must name BOTH grants: {:?}",
            summary.notices.iter().map(|n| &n.message).collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// §3's third escalated combination: fetch text from the network
    /// and put it in the model's system prompt, and a remote source
    /// shapes the agent's behaviour.
    #[test]
    fn allow_context_plus_allow_network_warns() {
        let root = tmp();
        let cfg = ExtensionConfig {
            path: root.join("nope.wasm"),
            allow_context: true,
            allow_network: true,
            ..Default::default()
        };
        let summary = PluginHost::new().load_all(&[cfg], &root);
        let warned = summary.notices.iter().any(|n| {
            n.level == crate::render::notice::Level::Warn
                && n.message.contains("allow_context")
                && n.message.contains("allow_network")
        });
        assert!(
            warned,
            "the escalated warning must name BOTH grants: {:?}",
            summary.notices.iter().map(|n| &n.message).collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The grant on its own is a normal opt-in, not a warning — a
    /// warning on every `allow_context` would be noise the user learns
    /// to skip past, which is what makes the paired warning worth
    /// reading.
    #[test]
    fn allow_context_alone_does_not_warn() {
        let root = tmp();
        let cfg = ExtensionConfig {
            path: root.join("nope.wasm"),
            allow_context: true,
            ..Default::default()
        };
        let summary = PluginHost::new().load_all(&[cfg], &root);
        assert!(
            !summary.notices.iter().any(|n| {
                n.level == crate::render::notice::Level::Warn
                    && n.message.contains("allow_context")
            }),
            "{:?}",
            summary.notices.iter().map(|n| &n.message).collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A misspelled or plugin-supplied name in `allow_tools` is a LOAD
    /// ERROR, not a silent no-op. The same rule a retired hook key
    /// follows, and for the same reason: a grant that fails quietly
    /// leaves the user believing they granted a capability they did
    /// not.
    #[test]
    fn an_unknown_name_in_allow_tools_is_a_load_error_naming_the_builtins() {
        let root = tmp();
        let path = root.join("nope.wasm");
        std::fs::write(&path, "not a component").unwrap();
        let cfg = ExtensionConfig {
            path,
            allow_tools: vec!["nope".to_string()],
            ..Default::default()
        };
        let summary = PluginHost::new().load_all(&[cfg], &root);
        assert_eq!(summary.loaded, 0);
        let err = summary
            .errors
            .first()
            .map(|(_, e)| e.clone())
            .unwrap_or_default();
        assert!(
            err.contains("allow_tools") && err.contains("nope"),
            "the load error must name the grant and the bad name, and must \
             not read as a compile failure: {err}"
        );
        for builtin in ["bash", "edit", "find", "grep", "ls", "read", "write"] {
            assert!(
                err.contains(builtin),
                "and must list the valid built-ins ({builtin} missing): {err}"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `bash` in `allow_tools` is arbitrary execution, so it warns on
    /// its own — no pairing needed.
    #[test]
    fn bash_in_allow_tools_warns() {
        let root = tmp();
        let cfg = ExtensionConfig {
            path: root.join("nope.wasm"),
            allow_tools: vec!["bash".to_string()],
            ..Default::default()
        };
        let summary = PluginHost::new().load_all(&[cfg], &root);
        assert!(
            summary.notices.iter().any(|n| {
                n.level == crate::render::notice::Level::Warn
                    && n.message.contains("allow_tools")
                    && n.message.contains("bash")
            }),
            "{:?}",
            summary.notices.iter().map(|n| &n.message).collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// And a read-only grant does NOT warn, following the
    /// `allow_context`-alone precedent: a warning on every
    /// `allow_tools` becomes noise the user learns to skip past, which
    /// is what makes the `bash` warning worth reading.
    #[test]
    fn a_read_only_allow_tools_does_not_warn() {
        let root = tmp();
        let cfg = ExtensionConfig {
            path: root.join("nope.wasm"),
            allow_tools: vec!["find".to_string(), "read".to_string()],
            ..Default::default()
        };
        let summary = PluginHost::new().load_all(&[cfg], &root);
        assert!(
            !summary
                .notices
                .iter()
                .any(|n| n.level == crate::render::notice::Level::Warn),
            "{:?}",
            summary.notices.iter().map(|n| &n.message).collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn allow_store_alone_does_not_warn() {
        let root = tmp();
        let cfg = ExtensionConfig {
            path: root.join("nope.wasm"),
            allow_store: true,
            ..Default::default()
        };
        let summary = PluginHost::new().load_all(&[cfg], &root);
        assert!(
            !summary
                .notices
                .iter()
                .any(|n| n.level == crate::render::notice::Level::Warn),
            "a durable store that cannot reach the network is not the \
             escalated combination: {:?}",
            summary.notices.iter().map(|n| &n.message).collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&root);
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
