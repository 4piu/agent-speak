//! Command-line parsing and startup policy selection.

use std::{
    collections::HashSet,
    fs::OpenOptions,
    io::{self, Write},
    path::PathBuf,
};

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;
use thiserror::Error;

use crate::config::{
    ConfigError, LogLevel, OutputCategory, OutputTargetConfig, OutputTargetKind, OutputsConfig,
    QuickProfileOverrides, ValidatedConfig, load_config, quick_profile,
};
use crate::playback::{
    OutputDevice, PlaybackError, SystemVoice, list_output_devices, list_system_voices,
};
use crate::provider::{
    ModelScope, PrepareOptions, ProviderError, import_voice, inspect_provider, list_models,
    list_voices, prepare_provider, remove_provider,
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
    /// Validate a profile; configured providers are executed read-only.
    Validate(ValidateArgs),
    /// List output devices without opening an audio stream.
    Devices(DevicesArgs),
    /// List native system voices, or voices from a configured provider.
    Voices(VoicesArgs),
    /// Inspect or manage the configured UtterPipe provider.
    Provider(ProviderArgs),
    /// Plan and explicitly install the configured provider assets.
    Prepare(PrepareArgs),
    /// Create a complete starter profile for the current system.
    Init(InitArgs),
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
            "log_level"
        ]
    )]
    pub config: Option<PathBuf>,

    /// Select the TTS voice in the built-in quick profile.
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

#[derive(Debug, Args)]
pub struct DevicesArgs {
    /// Choose a readable inventory or a copyable output-policy TOML section.
    #[arg(long, value_enum, default_value_t = DeviceListFormat::Table)]
    pub format: DeviceListFormat,
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
pub enum DeviceListFormat {
    #[default]
    Table,
    Toml,
}

#[derive(Debug, Error)]
pub enum DevicesError {
    #[error(transparent)]
    Playback(#[from] PlaybackError),
    #[error("output policy could not be serialized: {0}")]
    Serialize(#[from] toml::ser::Error),
}

impl DevicesArgs {
    pub fn render(&self) -> Result<String, DevicesError> {
        let devices = list_output_devices()?;
        match self.format {
            DeviceListFormat::Table => Ok(render_device_table(&devices)),
            DeviceListFormat::Toml => render_device_toml(&devices).map_err(Into::into),
        }
    }
}

#[derive(Debug, Args)]
pub struct VoicesArgs {
    /// Use the provider configured by this profile instead of system TTS.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,
    /// Explicitly permit a provider voice-catalog network refresh.
    #[arg(long, requires = "config")]
    pub refresh: bool,
}

impl VoicesArgs {
    pub fn render(&self) -> Result<String, CliProviderError> {
        match &self.config {
            None => list_system_voices()
                .map(|voices| render_voice_table(&voices))
                .map_err(Into::into),
            Some(path) => {
                let config = load_config(path)?;
                let result = list_voices(&config, ModelScope::All, self.refresh)?;
                Ok(crate::provider::render_json(&result) + "\n")
            }
        }
    }
}

#[derive(Debug, Args)]
pub struct ProviderArgs {
    #[command(subcommand)]
    pub command: ProviderCommand,
}

#[derive(Debug, Subcommand)]
pub enum ProviderCommand {
    /// Show provider identity, executable, protocol, and capabilities.
    Info(ProviderInfoArgs),
    /// List provider models.
    Models(ProviderModelsArgs),
    /// Perform voice-specific provider operations.
    Voices(ProviderVoicesArgs),
    /// Plan and remove exact provider-owned artifacts.
    Remove(ProviderRemoveArgs),
}

#[derive(Debug, Args)]
pub struct ProviderRemoveArgs {
    #[arg(long, required = true, value_name = "PATH")]
    pub config: PathBuf,
    /// Exact provider artifact ID to remove; repeat for multiple artifacts.
    #[arg(long, value_name = "ID")]
    pub artifact: Vec<String>,
    /// Plan removal of all provider-owned data.
    #[arg(long)]
    pub purge: bool,
    /// Apply without ordinary action confirmation.
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, Args)]
pub struct ProviderInfoArgs {
    #[arg(long, required = true, value_name = "PATH")]
    pub config: PathBuf,
}

#[derive(Debug, Args)]
pub struct ProviderModelsArgs {
    #[arg(long, required = true, value_name = "PATH")]
    pub config: PathBuf,
    #[arg(long, value_enum, default_value_t = ModelScope::All)]
    pub scope: ModelScope,
    /// Explicitly permit a provider catalog network refresh.
    #[arg(long)]
    pub refresh: bool,
}

#[derive(Debug, Args)]
pub struct ProviderVoicesArgs {
    #[command(subcommand)]
    pub command: ProviderVoicesCommand,
}

#[derive(Debug, Subcommand)]
pub enum ProviderVoicesCommand {
    /// Import a user-approved reference recording into provider-owned storage.
    Import(ProviderVoiceImportArgs),
}

#[derive(Debug, Args)]
pub struct ProviderVoiceImportArgs {
    #[arg(long, required = true, value_name = "PATH")]
    pub config: PathBuf,
    #[arg(long, required = true, value_name = "WAV")]
    pub source: PathBuf,
    #[arg(long = "id", required = true, value_name = "VOICE_ID")]
    pub requested_id: String,
    /// Confirm permission and consent to derive a TTS voice from this recording.
    #[arg(long)]
    pub consent_confirmed: bool,
}

#[derive(Debug, Args)]
pub struct PrepareArgs {
    #[arg(long, required = true, value_name = "PATH")]
    pub config: PathBuf,
    /// Apply without ordinary action confirmation (licenses remain explicit).
    #[arg(long)]
    pub yes: bool,
    /// Explicitly accept a provider-reported license ID.
    #[arg(long = "accept-license", value_name = "ID")]
    pub accepted_licenses: Vec<String>,
}

#[derive(Debug, Error)]
pub enum CliProviderError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Playback(#[from] PlaybackError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
}

impl ProviderArgs {
    pub fn render(&self) -> Result<String, CliProviderError> {
        match &self.command {
            ProviderCommand::Info(args) => {
                let config = load_config(&args.config)?;
                let info = inspect_provider(&config)?;
                let executable = single_line(&info.executable.to_string_lossy());
                let name = single_line(&info.provider.name);
                let vendor = single_line(&info.provider.vendor);
                let audio_formats = info
                    .audio_formats
                    .iter()
                    .map(|format| single_line(format))
                    .collect::<Vec<_>>()
                    .join(", ");
                Ok(format!(
                    "executable: {}\nprovider: {} ({})\nvendor: {}\nversion: {}\nprotocol: utterpipe.tts/{}\ncapabilities: synthesis={}, cancellation={}, model_catalog={}, voice_catalog={}, prepare={}, remove={}, voice_import={}\ndelivery_modes: {:?}\naudio_formats: {}\n",
                    executable,
                    name,
                    info.provider.slug,
                    vendor,
                    info.provider.version,
                    info.protocol_version,
                    info.capabilities.synthesis,
                    info.capabilities.cancellation,
                    info.capabilities.model_catalog,
                    info.capabilities.voice_catalog,
                    info.capabilities.prepare,
                    info.capabilities.remove,
                    info.capabilities.voice_import,
                    info.delivery_modes,
                    audio_formats
                ))
            }
            ProviderCommand::Models(args) => {
                let config = load_config(&args.config)?;
                let result = list_models(&config, args.scope, args.refresh)?;
                Ok(crate::provider::render_json(&result) + "\n")
            }
            ProviderCommand::Voices(args) => match &args.command {
                ProviderVoicesCommand::Import(args) => {
                    let config = load_config(&args.config)?;
                    let result = import_voice(
                        &config,
                        &args.source,
                        &args.requested_id,
                        args.consent_confirmed,
                    )?;
                    Ok(crate::provider::render_json(&result) + "\n")
                }
            },
            ProviderCommand::Remove(args) => {
                let config = load_config(&args.config)?;
                let result = remove_provider(&config, &args.artifact, args.purge, args.yes)?;
                Ok(crate::provider::render_json(&result) + "\n")
            }
        }
    }
}

impl PrepareArgs {
    pub fn render(&self) -> Result<String, CliProviderError> {
        let config = load_config(&self.config)?;
        let result = prepare_provider(
            &config,
            &PrepareOptions {
                yes: self.yes,
                accepted_licenses: self.accepted_licenses.clone(),
            },
        )?;
        Ok(crate::provider::render_json(&result) + "\n")
    }
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Create the profile at this path; existing files are never overwritten.
    #[arg(long, value_name = "PATH", default_value = "agent-speak.toml")]
    pub output: PathBuf,
}

#[derive(Debug, Error)]
pub enum InitError {
    #[error(transparent)]
    Playback(#[from] PlaybackError),
    #[error("generated profile could not be serialized: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("could not create configuration file '{}': {source}", path.display())]
    Create {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not write configuration file '{}': {source}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl InitArgs {
    pub fn generate(&self) -> Result<PathBuf, InitError> {
        let devices = list_output_devices()?;
        let source = render_complete_config(&devices)?;
        self.write_source(&source)
    }

    fn write_source(&self, source: &str) -> Result<PathBuf, InitError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.output)
            .map_err(|source| InitError::Create {
                path: self.output.clone(),
                source,
            })?;
        file.write_all(source.as_bytes())
            .map_err(|source| InitError::Write {
                path: self.output.clone(),
                source,
            })?;
        Ok(self.output.clone())
    }
}

fn render_device_table(devices: &[OutputDevice]) -> String {
    if devices.is_empty() {
        return "No output devices are currently available.\n".to_owned();
    }
    let mut output = String::new();
    for device in devices {
        let marker = if device.is_default { " [default]" } else { "" };
        let name = device.name.replace(['\r', '\n'], " ");
        output.push_str(&format!("{name}{marker}\n  {}\n", device.device_id));
    }
    output
}

fn render_voice_table(voices: &[SystemVoice]) -> String {
    if voices.is_empty() {
        return "No system TTS voices are available.\n".to_owned();
    }
    let mut output = String::new();
    for voice in voices {
        let marker = if voice.is_default {
            " [Agent Speak default]"
        } else {
            ""
        };
        let name = single_line(&voice.display_name);
        let language = single_line(&voice.language);
        let gender = single_line(&voice.gender);
        let description = single_line(&voice.description);
        let id = single_line(&voice.id);
        let config_value = toml::Value::String(id.clone()).to_string();
        output.push_str(&format!(
            "{name} ({language}, {gender}){marker}\n  id: {id}\n  config: voice_id = {config_value}\n  {description}\n"
        ));
    }
    output.push_str(
        "\n[Agent Speak default] is used when voice_id is empty; other operating-system speech features may use a different voice inventory or default.\n",
    );
    output
}

fn single_line(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn render_device_toml(devices: &[OutputDevice]) -> Result<String, toml::ser::Error> {
    #[derive(Serialize)]
    struct Document {
        outputs: OutputsConfig,
    }

    toml::to_string_pretty(&Document {
        outputs: outputs_for_devices(devices),
    })
}

fn render_complete_config(devices: &[OutputDevice]) -> Result<String, toml::ser::Error> {
    let mut profile = quick_profile(QuickProfileOverrides::default())
        .expect("the built-in quick profile must remain valid")
        .into_profile();
    profile.profile_name = "local".to_owned();
    profile.outputs = outputs_for_devices(devices);
    let mut source = toml::to_string_pretty(&profile)?;
    let example_audio_path = if cfg!(windows) {
        "C:/path/to/your/sound.wav"
    } else {
        "/path/to/your/sound.wav"
    };
    source.push_str(&format!(
        r#"
# Example text preset (remove `# ` from each line to enable):
# [[presets]]
# id = "needs-attention"
# kind = "text"
# text = "Your agent needs your attention."
# description = "Use when work cannot continue without user input."
# default_gain = 0.4

# Example audio preset (provide the referenced file yourself):
# [[presets]]
# id = "finished-chime"
# kind = "audio_file"
# source = "{example_audio_path}"
# description = "Use when a long-running task is complete."
# default_gain = 0.4
"#,
    ));
    Ok(source)
}

fn outputs_for_devices(devices: &[OutputDevice]) -> OutputsConfig {
    let mut outputs = OutputsConfig::default();
    let mut used_ids = HashSet::from(["system".to_owned()]);
    outputs.targets.extend(
        devices
            .iter()
            .enumerate()
            .map(|(index, device)| OutputTargetConfig {
                id: unique_device_alias(&device.name, index + 1, &mut used_ids),
                description: device.name.clone(),
                kind: OutputTargetKind::Device,
                device_id: Some(device.device_id.clone()),
                allow: vec![OutputCategory::Audio, OutputCategory::Speech],
            }),
    );
    outputs
}

fn unique_device_alias(name: &str, fallback_number: usize, used: &mut HashSet<String>) -> String {
    let mut base = String::new();
    let mut separator_pending = false;
    for character in name.chars() {
        if character.is_ascii_alphanumeric() {
            if separator_pending && !base.is_empty() && base.len() < 64 {
                base.push('-');
            }
            separator_pending = false;
            if base.len() < 64 {
                base.push(character.to_ascii_lowercase());
            }
        } else {
            separator_pending = true;
        }
    }
    while base.ends_with('-') {
        base.pop();
    }
    if base.is_empty() {
        base = format!("device-{fallback_number}");
    }

    let mut alias = base.clone();
    let mut suffix_number = 2;
    while used.contains(&alias) {
        let suffix = format!("-{suffix_number}");
        let prefix_length = 64 - suffix.len();
        alias = format!("{}{}", &base[..base.len().min(prefix_length)], suffix);
        suffix_number += 1;
    }
    used.insert(alias.clone());
    alias
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
            "--log-level",
            "debug",
        ])
        .unwrap();
        let Command::Serve(args) = cli.command else {
            panic!("serve command expected");
        };

        let config = args.startup_config().unwrap();
        let profile = config.profile();
        assert_eq!(profile.tts.voice_id(), "test-voice");
        assert_eq!(profile.playback.minimum_gain, 0.1);
        assert_eq!(profile.playback.maximum_gain, 0.9);
        assert_eq!(profile.playback.default_gain, 0.6);
        assert_eq!(profile.tts.maximum_characters, 42);
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
    fn devices_command_accepts_both_formats() {
        assert!(Cli::try_parse_from(["agent-speak", "devices"]).is_ok());
        assert!(Cli::try_parse_from(["agent-speak", "devices", "--format", "toml"]).is_ok());
        assert!(Cli::try_parse_from(["agent-speak", "devices", "--format", "json"]).is_err());
    }

    #[test]
    fn voices_and_provider_voice_import_have_explicit_configuration() {
        assert!(Cli::try_parse_from(["agent-speak", "voices"]).is_ok());
        assert!(
            Cli::try_parse_from([
                "agent-speak",
                "voices",
                "--config",
                "profile.toml",
                "--refresh"
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["agent-speak", "voices", "--format", "toml"]).is_err());
        assert!(
            Cli::try_parse_from([
                "agent-speak",
                "provider",
                "voices",
                "import",
                "--config",
                "profile.toml",
                "--source",
                "/tmp/reference.wav",
                "--id",
                "my-voice",
                "--consent-confirmed"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "agent-speak",
                "provider",
                "voices",
                "import",
                "--config",
                "profile.toml",
                "--source",
                "/tmp/reference.wav"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "agent-speak",
                "provider",
                "remove",
                "--config",
                "profile.toml",
                "--artifact",
                "voice:my-voice",
                "--yes"
            ])
            .is_ok()
        );
    }

    #[test]
    fn init_command_defaults_to_a_local_config_file() {
        let cli = Cli::try_parse_from(["agent-speak", "init"]).unwrap();
        let Command::Init(args) = cli.command else {
            panic!("init command expected");
        };
        assert_eq!(args.output, PathBuf::from("agent-speak.toml"));

        let cli = Cli::try_parse_from(["agent-speak", "init", "--output", "private-profile.toml"])
            .unwrap();
        let Command::Init(args) = cli.command else {
            panic!("init command expected");
        };
        assert_eq!(args.output, PathBuf::from("private-profile.toml"));
    }

    #[test]
    fn device_rendering_is_copyable_and_escapes_display_names() {
        let devices = vec![OutputDevice {
            device_id: "wasapi:stable-id".to_owned(),
            name: "Desk\nSpeakers".to_owned(),
            is_default: true,
        }];
        let table = render_device_table(&devices);
        assert_eq!(table, "Desk Speakers [default]\n  wasapi:stable-id\n");

        let source = render_device_toml(&devices).unwrap();
        let parsed: toml::Value = toml::from_str(&source).unwrap();
        let targets = parsed["outputs"]["targets"].as_array().unwrap();
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[1]["id"].as_str(), Some("desk-speakers"));
        assert_eq!(targets[1]["device_id"].as_str(), Some("wasapi:stable-id"));
    }

    #[test]
    fn voice_rendering_exposes_copyable_ids_and_sanitizes_lines() {
        let voices = vec![SystemVoice {
            id: "voice-id".to_owned(),
            display_name: "Microsoft\nAva".to_owned(),
            language: "en-US".to_owned(),
            description: "Natural\r\nvoice".to_owned(),
            gender: "female".to_owned(),
            is_default: true,
        }];

        assert_eq!(
            render_voice_table(&voices),
            "Microsoft Ava (en-US, female) [Agent Speak default]\n  id: voice-id\n  config: voice_id = \"voice-id\"\n  Natural  voice\n\n[Agent Speak default] is used when voice_id is empty; other operating-system speech features may use a different voice inventory or default.\n"
        );
    }

    #[test]
    fn generated_device_aliases_are_readable_unique_and_valid() {
        let devices = vec![
            OutputDevice {
                device_id: "wasapi:first".to_owned(),
                name: "Speakers (USB Audio)".to_owned(),
                is_default: true,
            },
            OutputDevice {
                device_id: "wasapi:second".to_owned(),
                name: "Speakers (USB Audio)".to_owned(),
                is_default: false,
            },
            OutputDevice {
                device_id: "wasapi:third".to_owned(),
                name: "系统扬声器".to_owned(),
                is_default: false,
            },
        ];
        let outputs = outputs_for_devices(&devices);
        let ids: Vec<_> = outputs
            .targets
            .iter()
            .map(|target| target.id.as_str())
            .collect();
        assert_eq!(
            ids,
            [
                "system",
                "speakers-usb-audio",
                "speakers-usb-audio-2",
                "device-3"
            ]
        );
    }

    #[test]
    fn complete_generated_profile_uses_safe_quick_defaults() {
        let devices = vec![OutputDevice {
            device_id: "wasapi:stable-id".to_owned(),
            name: "Desk Speakers".to_owned(),
            is_default: true,
        }];
        let source = render_complete_config(&devices).unwrap();
        let profile: crate::config::ProfileConfig = toml::from_str(&source).unwrap();
        assert_eq!(profile.profile_name, "local");
        assert!(profile.permissions.arbitrary_text);
        assert!(!profile.permissions.arbitrary_local_audio);
        assert_eq!(profile.outputs.targets[1].id, "desk-speakers");
        assert_eq!(
            profile.outputs.targets[1].device_id.as_deref(),
            Some("wasapi:stable-id")
        );
        assert!(source.contains("# [[presets]]"));
        assert!(source.contains("# kind = \"text\""));
        assert!(source.contains("# kind = \"audio_file\""));
        assert!(!source.contains("presets = []"));
        assert!(!source.contains("maximum_file_bytes"));
        assert!(source.contains("maximum_audio_seconds = 0"));
        assert!(!source.contains("maximum_plays_per_minute"));
        if cfg!(windows) {
            assert!(source.contains("# source = \"C:/path/to/your/sound.wav\""));
        } else {
            assert!(source.contains("# source = \"/path/to/your/sound.wav\""));
        }

        let enabled_text_preset = format!(
            r#"{source}
[[presets]]
id = "needs-attention"
kind = "text"
text = "Your agent needs your attention."
description = "Use when work cannot continue without user input."
default_gain = 0.4
"#
        );
        let profile: crate::config::ProfileConfig = toml::from_str(&enabled_text_preset).unwrap();
        assert_eq!(profile.presets.len(), 1);
    }

    #[test]
    fn init_never_overwrites_an_existing_file() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("agent-speak.toml");
        std::fs::write(&output, "user-owned").unwrap();
        let args = InitArgs {
            output: output.clone(),
        };

        assert!(matches!(
            args.write_source("replacement"),
            Err(InitError::Create {
                source,
                ..
            }) if source.kind() == io::ErrorKind::AlreadyExists
        ));
        assert_eq!(std::fs::read_to_string(output).unwrap(), "user-owned");
    }

    #[test]
    fn unsupported_log_level_is_rejected() {
        assert!(Cli::try_parse_from(["agent-speak", "serve", "--log-level", "warn"]).is_err());
    }

    #[test]
    fn removed_rate_limit_flag_is_rejected_without_a_fallback() {
        assert!(
            Cli::try_parse_from(["agent-speak", "serve", "--maximum-plays-per-minute", "10"])
                .is_err()
        );
    }
}
