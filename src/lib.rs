//! nanopi v0.5 — library crate.
//!
//! `main.rs` is a thin shim that dispatches to `mode::*`. All real logic lives here.

pub mod command;
pub mod config;
pub mod event;
pub mod keys;
pub mod paths;
pub mod models;
/// Unconditionally compiled, like `subscriber` below and for the same
/// reason: turn assembly in `agent::loop_` reads it, and that path must
/// stay free of `#[cfg(feature = "wasm")]`. Without the feature the
/// registry is simply always empty.
pub mod plugin_context;
pub mod resources;
pub mod session;
pub mod settings;
pub mod settings_toml;
pub mod subscriber;
pub mod trust;
pub mod wizard;

pub mod agent;
pub mod mode;
pub mod provider;
pub mod render;
pub mod tool;
pub mod util;
pub mod vendor;

#[cfg(feature = "wasm")]
pub mod wasm;

/// Process-wide test mutex. Tests that mutate `$NANOPI_HOME` (or any
/// other global env var) MUST acquire this lock before changing it,
/// so parallel test execution can't poison each other's environment.
#[cfg(test)]
pub static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
