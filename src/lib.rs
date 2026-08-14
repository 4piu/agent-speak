//! Core library for Agent Speak.

pub mod cli;
pub mod config;
pub mod control;
mod history;
pub mod http;
#[cfg(target_os = "macos")]
pub mod macos_runtime;
pub mod mcp;
pub mod playback;
mod private_file;
pub mod provider;
