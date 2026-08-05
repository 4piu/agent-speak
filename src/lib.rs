//! Core library for Agent Speak.

pub mod cli;
pub mod config;
mod history;
#[cfg(target_os = "macos")]
pub mod macos_runtime;
pub mod mcp;
pub mod playback;
