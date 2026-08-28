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
//!   - Network + filesystem host functions (phase 4)

use std::path::Path;

use crate::config::ExtensionConfig;

/// Outcome of a `PluginHost::load_all(...)` call.
pub struct PluginLoadSummary {
    pub loaded: usize,
    pub skipped: usize,
}

/// Placeholder loader — Phase 1 just validates that each declared
/// extension path resolves to an existing file. wasmtime instantiation
/// lands in phase 2 once the WIT interface ships.
pub struct PluginHost;

impl PluginHost {
    pub fn new() -> Self {
        Self
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
                        eprintln!(
                            "nanopi: skipping extension dir {}: {e}",
                            expanded.display()
                        );
                    }
                }
            } else if expanded.is_file() {
                if expanded.extension().and_then(|e| e.to_str()) == Some("wasm") {
                    out.push(expanded);
                } else {
                    eprintln!(
                        "nanopi: skipping extension (not .wasm): {}",
                        expanded.display()
                    );
                }
            } else {
                eprintln!(
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
