//! nanopi v0.5 — library crate.
//!
//! `main.rs` is a thin shim that dispatches to `mode::*`. All real logic lives here.

pub mod config;
pub mod pricing;
pub mod session;
pub mod settings;
pub mod trust;
pub mod resources;
pub mod event;

pub mod agent;
pub mod provider;
pub mod tool;
pub mod render;
pub mod mode;
pub mod util;

/// Process-wide test mutex. Tests that mutate `$NANOPI_HOME` (or any
/// other global env var) MUST acquire this lock before changing it,
/// so parallel test execution can't poison each other's environment.
#[cfg(test)]
pub static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());