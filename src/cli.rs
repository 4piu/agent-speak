//! Command-line parsing and startup policy selection.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::config::{
    ConfigError, LogLevel, QuickProfileOverrides, ValidatedConfig, load_config, quick_profile,
};

/// Agent-controlled, user-policy-constrained local audio playback.
#[derive(Debug, Parser)]
#[command(name = "agent-speak", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the MCP server over standard input and output.
    Serve(ServeArgs),
    /// Statically validate a configuration profile without starting MCP or audio.
    Validate(ValidateArgs),
}

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Load the complete startup policy from this TOML file.
    #[arg(
        long,
        value_name = "PATH",
        conflicts_with_all = [
            "voice_id",
            "minimum_gain",
            "maximum_gain",
            "default_gain",
            "maximum_text_characters",
            "maximum_plays_per_minute",
            "log_level"
        ]
    )]
    pub config: Option<PathBuf>,

    /// Select a system TTS voice in the built-in quick profile.
    #[arg(long, value_name = "ID")]
    pub voice_id: Option<String>,

    /// Set the lowest permitted normalized gain in the quick profile.
    #[arg(long, value_name = "0.0..1.0")]
    pub minimum_gain: Option<f64>,

    /// Set the highest permitted normalized gain in the quick profile.
    #[arg(long, value_name = "0.0..1.0")]
    pub maximum_gain: Option<f64>,

    /// Set the gain used when a call omits one in the quick profile.
    #[arg(long, value_name = "0.0..1.0")]
    pub default_gain: Option<f64>,

    /// Limit arbitrary TTS input in the quick profile.
    #[arg(long, value_name = "COUNT")]
    pub maximum_text_characters: Option<usize>,

    /// Limit accepted playback calls per minute in the quick profile.
    #[arg(long, value_name = "COUNT")]
    pub maximum_plays_per_minute: Option<u32>,

    /// Set stderr diagnostic verbosity in the quick profile.
    #[arg(long, value_enum, value_name = "LEVEL")]
    pub log_level: Option<LogLevel>,
}

impl ServeArgs {
    /// Load the selected file profile, or construct the built-in quick profile.
    pub fn startup_config(&self) -> Result<ValidatedConfig, ConfigError> {
        match &self.config {
            Some(path) => load_config(path),
            None => quick_profile(self.quick_overrides()),
        }
    }

    pub fn quick_overrides(&self) -> QuickProfileOverrides {
        QuickProfileOverrides {
            voice_id: self.voice_id.clone(),
            minimum_gain: self.minimum_gain,
            maximum_gain: self.maximum_gain,
            default_gain: self.default_gain,
            maximum_text_characters: self.maximum_text_characters,
            maximum_plays_per_minute: self.maximum_plays_per_minute,
            log_level: self.log_level,
        }
    }
}

#[derive(Debug, Args)]
pub struct ValidateArgs {
    /// TOML profile to validate.
    #[arg(long, required = true, value_name = "PATH")]
    pub config: PathBuf,
}

impl ValidateArgs {
    pub fn validated_config(&self) -> Result<ValidatedConfig, ConfigError> {
        load_config(&self.config)
    }
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::*;

    #[test]
    fn clap_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn serve_without_options_selects_quick_profile() {
        let cli = Cli::try_parse_from(["agent-speak", "serve"]).unwrap();
        let Command::Serve(args) = cli.command else {
            panic!("serve command expected");
        };

        let config = args.startup_config().unwrap();
        assert_eq!(config.profile().profile_name, "quickstart");
        assert!(config.capabilities().permissions.arbitrary_text);
    }

    #[test]
    fn quick_overrides_are_applied() {
        let cli = Cli::try_parse_from([
            "agent-speak",
            "serve",
            "--voice-id",
            "test-voice",
            "--minimum-gain",
            "0.1",
            "--maximum-gain",
            "0.9",
            "--default-gain",
            "0.6",
            "--maximum-text-characters",
            "42",
            "--maximum-plays-per-minute",
            "7",
            "--log-level",
            "debug",
        ])
        .unwrap();
        let Command::Serve(args) = cli.command else {
            panic!("serve command expected");
        };

        let config = args.startup_config().unwrap();
        let profile = config.profile();
        assert_eq!(profile.tts.voice_id, "test-voice");
        assert_eq!(profile.playback.minimum_gain, 0.1);
        assert_eq!(profile.playback.maximum_gain, 0.9);
        assert_eq!(profile.playback.default_gain, 0.6);
        assert_eq!(profile.tts.maximum_characters, 42);
        assert_eq!(profile.playback.maximum_plays_per_minute, 7);
        assert_eq!(profile.logging.level, LogLevel::Debug);
    }

    #[test]
    fn config_conflicts_with_every_quick_option() {
        let cases: &[&[&str]] = &[
            &["--voice-id", "voice"],
            &["--minimum-gain", "0.1"],
            &["--maximum-gain", "0.9"],
            &["--default-gain", "0.5"],
            &["--maximum-text-characters", "10"],
            &["--maximum-plays-per-minute", "10"],
            &["--log-level", "info"],
        ];

        for extra in cases {
            let mut args = vec!["agent-speak", "serve", "--config", "profile.toml"];
            args.extend_from_slice(extra);
            assert!(Cli::try_parse_from(args).is_err(), "accepted {extra:?}");
        }
    }

    #[test]
    fn validate_requires_config() {
        assert!(Cli::try_parse_from(["agent-speak", "validate"]).is_err());
        assert!(
            Cli::try_parse_from(["agent-speak", "validate", "--config", "profile.toml"]).is_ok()
        );
    }

    #[test]
    fn unsupported_log_level_is_rejected() {
        assert!(Cli::try_parse_from(["agent-speak", "serve", "--log-level", "warn"]).is_err());
    }
}
