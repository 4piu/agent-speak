use std::{error::Error, io};

use agent_speak::{
    cli::{Cli, Command},
    config::{LogLevel, ValidatedConfig},
    mcp::{AgentSpeakServer, preflight_config_media},
};
use clap::Parser;
use rmcp::{ServiceExt, transport::stdio};
use tracing::level_filters::LevelFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    match Cli::parse().command {
        Command::Validate(args) => {
            let config = args.validated_config()?;
            preflight_config_media(config.profile())?;
            println!(
                "valid Agent Speak profile: {} ({} tools)",
                config.profile().profile_name,
                config.capabilities().tools.len()
            );
            Ok(())
        }
        Command::Serve(args) => serve(args.startup_config()?).await,
    }
}

async fn serve(config: ValidatedConfig) -> Result<(), Box<dyn Error>> {
    initialize_diagnostics(config.profile().logging.level)?;
    let server = AgentSpeakServer::new(config)?;
    tracing::info!(tools = ?server.registered_tool_names(), "Agent Speak MCP server starting");

    let running = server.clone().serve(stdio()).await?;
    let service_result = running.waiting().await;
    let shutdown_result = server.shutdown().await;

    service_result?;
    shutdown_result?;
    Ok(())
}

fn initialize_diagnostics(level: LogLevel) -> Result<(), io::Error> {
    let maximum_level = match level {
        LogLevel::Error => LevelFilter::ERROR,
        LogLevel::Warning => LevelFilter::WARN,
        LogLevel::Info => LevelFilter::INFO,
        LogLevel::Debug => LevelFilter::DEBUG,
        LogLevel::Trace => LevelFilter::TRACE,
    };
    tracing_subscriber::fmt()
        .with_max_level(maximum_level)
        .with_writer(io::stderr)
        .with_ansi(false)
        .try_init()
        .map_err(io::Error::other)
}
