//! Agent runtime — the heart of nanopi.
//! Filled in Tasks 14 (hook), 15 (permission), 16 (loop).

pub mod context;
pub mod branch_summary;
pub mod build;
pub mod compact;
pub mod thinking;
pub mod hook;
pub mod permission;
pub mod loop_;
pub mod system_prompt;