//! nanopi v0.5 — library crate.
//!
//! `main.rs` is a thin shim that dispatches to `mode::*`. All real logic lives here.

pub mod config;
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