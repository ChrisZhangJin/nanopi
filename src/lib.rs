//! nanopi v0.5 — library crate.
//!
//! `main.rs` is a thin shim that dispatches to `mode::*`. All real logic lives here.

pub mod config;
pub mod event;
pub mod keys;
pub mod paths;
pub mod pricing;
pub mod resources;
pub mod session;
pub mod settings;
pub mod trust;

pub mod agent;
pub mod mode;
pub mod provider;
pub mod render;
pub mod tool;
pub mod util;
pub mod vendor;

/// Process-wide test mutex. Tests that mutate `$NANOPI_HOME` (or any
/// other global env var) MUST acquire this lock before changing it,
/// so parallel test execution can't poison each other's environment.
#[cfg(test)]
pub static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
