//! Core library for Agent Speak.

pub mod cli;
pub mod config;
pub mod control;
mod history;
#[cfg(target_os = "macos")]
pub mod macos_runtime;
pub mod mcp;
pub mod playback;
pub mod provider;
