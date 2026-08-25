//! Agent runtime — the heart of nanopi.
//! Filled in Tasks 14 (hook), 15 (permission), 16 (loop).

pub mod branch_summary;
pub mod build;
pub mod compact;
pub mod context;
pub mod context_files;
pub mod hook;
pub mod loop_;
pub mod permission;
pub mod prompt_override;
pub mod system_prompt;
pub mod thinking;
