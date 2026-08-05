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
        Command::Devices(args) => {
            print!("{}", args.render()?);
            Ok(())
        }
        Command::Voices(args) => {
            print!("{}", args.render()?);
            Ok(())
        }
        Command::Init(args) => {
            let path = args.generate()?;
            println!("created Agent Speak profile: {}", path.display());
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
    let agent_level = match level {
        LogLevel::Error => "error",
        LogLevel::Warning => "warn",
        LogLevel::Info => "info",
        LogLevel::Debug => "debug",
        LogLevel::Trace => "trace",
    };
    let filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(LevelFilter::ERROR.into())
        .parse_lossy(format!("agent_speak={agent_level}"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(io::stderr)
        .with_ansi(false)
        .try_init()
        .map_err(io::Error::other)
}
