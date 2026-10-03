//! Agent runtime — the heart of nanopi.
//! Filled in Tasks 14 (hook), 15 (permission), 16 (loop).

pub mod agents;
pub mod branch_summary;
pub mod build;
pub mod compact;
pub mod context;
pub mod context_files;
pub mod hook;
pub mod loop_;
pub mod permission;
pub mod prompt_override;
#[cfg(test)]
mod subagent_e2e_tests;
pub mod subagent_registry;
pub mod system_prompt;
pub mod thinking;
