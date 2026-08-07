# Agent Speak

[![CI](https://github.com/4piu/agent-speak/actions/workflows/ci.yml/badge.svg)](https://github.com/4piu/agent-speak/actions/workflows/ci.yml)

Agent Speak is a local Windows, macOS, and Linux [Model Context Protocol
(MCP)](https://modelcontextprotocol.io/) server that lets an agent speak and
play audio. You decide which voices, sounds, files, and output devices are
available.

```text
MCP host → Agent Speak → optional UtterPipe provider → TTS engine or service
```

Agent Speak is pre-release software.

## Contents

- [Requirements](#requirements)
- [Install](#install)
- [Quick start](#quick-start)
- [Configuration](#configuration)
- [Playback and safety](#playback-and-safety)
- [Troubleshooting](#troubleshooting)
- [More documentation](#more-documentation)

## Requirements

- Windows with speech services, macOS 15 or later, or Linux with
  ALSA-compatible audio
- On Linux, an independently installed UtterPipe TTS provider;
  [`utterpipe-espeak-ng`](https://github.com/4piu/utterpipe-espeak-ng) is the
  quick-profile default
- An audio output device and an MCP host that can run a local stdio server
- A prebuilt release, or Rust 1.89+ to build from source

## Install

The installers download the latest release, verify its checksum, and install it
for the current user. The same one-line command handles initial installation,
reinstallation, and updates: run it again to replace the executable with the
current latest release. Profiles and provider assets are left untouched. Stop
running Agent Speak instances first, especially on Windows where an active
executable may be locked.

### Windows

```powershell
irm https://raw.githubusercontent.com/4piu/agent-speak/master/install.ps1 | iex
```

The default destination is
`%LOCALAPPDATA%\Programs\UtterPipe\bin\agent-speak.exe`; the installer adds that
directory to the user `PATH`. Open a new terminal, then verify it:

```powershell
(Get-Command agent-speak).Source
```

#### Uninstall on Windows

This removes the executable while preserving profiles and provider assets:

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/4piu/agent-speak/master/install.ps1))) -Uninstall
```

### macOS

```sh
curl -fsSL https://raw.githubusercontent.com/4piu/agent-speak/master/install.sh | sh && command -v agent-speak
```

The default destination is `${XDG_BIN_HOME:-$HOME/.local/bin}/agent-speak`. Add
that directory to `PATH` if the installer asks you to. Pre-release archives are
not yet signed or notarized; after verifying the download, macOS may require
explicit approval in System Settings > Privacy & Security.

#### Uninstall on macOS

This removes the executable while preserving profiles and provider assets:

```sh
curl -fsSL https://raw.githubusercontent.com/4piu/agent-speak/master/install.sh | sh -s -- --uninstall
```

### Linux

```sh
curl -fsSL https://raw.githubusercontent.com/4piu/agent-speak/master/install.sh | sh && command -v agent-speak
```

The default destination is `${XDG_BIN_HOME:-$HOME/.local/bin}/agent-speak`.
Agent Speak uses ALSA for playback. On Arch/Manjaro and Debian-based systems
(including Ubuntu), the installer verifies the ALSA runtime and, when PipeWire
is active, `pipewire-alsa`. If anything is missing, an interactive install
offers to run the appropriate `pacman` or `apt-get` command; it never changes
packages without confirmation. A non-interactive run prints the command
instead. Other distributions receive a warning when active PipeWire lacks a
detectable ALSA bridge.

Package managers and intentionally custom audio setups can bypass the check:

```sh
curl -fsSL https://raw.githubusercontent.com/4piu/agent-speak/master/install.sh | sh -s -- --skip-audio-check
```

Text to speech also needs a provider. The self-contained
`utterpipe-espeak-ng` executable needs no system eSpeak installation; put it
beside `agent-speak` or on `PATH`.

#### Uninstall on Linux

```sh
curl -fsSL https://raw.githubusercontent.com/4piu/agent-speak/master/install.sh | sh -s -- --uninstall
```

### Other installation methods

Prebuilt archives and matching `.sha256` files are available from
[GitHub Releases](https://github.com/4piu/agent-speak/releases). To build from
source, clone the repository and run `cargo build --release --locked`; Linux
also needs its distribution's ALSA development package.

## Quick start

With no config file, Agent Speak uses a safe quick profile: arbitrary speech is
enabled, while audio files, audio cues, and history are disabled. Windows and
macOS use their public system TTS APIs; Linux discovers `utterpipe-espeak-ng`
beside Agent Speak or on `PATH`.

| Command | Purpose | Example |
| --- | --- | --- |
| `devices` | List active output devices and stable IDs | `agent-speak devices` |
| `voices` | List native Windows/macOS voices | `agent-speak voices` |
| `serve` | Run the MCP stdio server with the quick profile | `agent-speak serve` |

Quick-profile settings are command-line options, not TOML fields:

| Goal | Example |
| --- | --- |
| Choose a voice | `agent-speak serve --voice-id <ID>` |
| Change the gain policy | `agent-speak serve --maximum-gain 0.9 --default-gain 0.5` |
| Change the text limit and diagnostics | `agent-speak serve --maximum-text-characters 500 --log-level info` |

Run `agent-speak serve --help` for the complete quick-profile option list.

## Configuration

Configuration covers MCP host registration and Agent Speak TOML profiles. A
profile is required for audio cues, arbitrary local audio, history, fixed output
routing, or a non-default TTS provider.

### Register with an MCP host

Register the executable's absolute path and the `serve` argument:

```json
{
  "mcpServers": {
    "agent-speak": {
      "command": "/absolute/path/to/agent-speak",
      "args": ["serve"]
    }
  }
}
```

Typical installer paths are:

| Platform | Command path |
| --- | --- |
| Windows | `%LOCALAPPDATA%\Programs\UtterPipe\bin\agent-speak.exe` |
| macOS | `/Users/you/.local/bin/agent-speak` |
| Linux | `/home/you/.local/bin/agent-speak` |

Resolve environment-variable and home-directory shorthand before copying the
path into a host that requires a literal absolute path. Restart or reload the
host before trying it.

#### Try it

Talk to the agent naturally; Agent Speak's MCP metadata teaches it the tool
workflow. For the quick profile, try:

- `Say “Agent Speak is ready” out loud.`
- `When you finish reviewing this file, speak a one-sentence summary.`

With configured audio cues, try `What audio cues are available?` or `Use the
completion audio cue when this task is done.` Installing the MCP makes audible
tools available, but the agent should use them only when requested or when an
approved audio cue description clearly applies.

To register a profile, add its absolute path to the arguments:

```json
"args": ["serve", "--config", "/absolute/path/to/agent-speak.toml"]
```

### Configuration commands

| Command | Purpose | Example |
| --- | --- | --- |
| `init` | Generate a complete profile for the current machine | `agent-speak init --output ./agent-speak.toml` |
| `validate` | Check profile and provider readiness without preparing assets | `agent-speak validate --config ./agent-speak.toml` |
| `serve --config` | Run the MCP server with the validated profile | `agent-speak serve --config ./agent-speak.toml` |
| `devices --format toml` | Print copyable output-target entries | `agent-speak devices --format toml` |
| `voices --config` | List voices for the configured provider | `agent-speak voices --config ./agent-speak.toml` |

`init` never overwrites an existing file. Profile parsing is strict: unknown
fields and invalid combinations are rejected.

### Profile file

Start from one of these complete examples:

- [native Windows/macOS TTS](examples/text-profile.toml)
- [cross-platform eSpeak NG](examples/espeak-provider.toml)
- [OpenAI-compatible HTTP TTS](examples/openai-http-provider.toml)

| Section | Purpose |
| --- | --- |
| `[permissions]` | Allow arbitrary text or arbitrary local audio |
| `[playback]` | Set gain, queueing, concurrency, and duration limits |
| `[outputs]` | Name allowed default or fixed output devices |
| `[tts]` | Select the backend, model, voice, and text limit |
| `[logging]` | Configure diagnostics and optional history |
| `[[audio_cues]]` | Define approved speech or audio-file actions |

Relative audio-file cue sources and history paths are resolved from the profile's directory.
Audio-file cues do not require `arbitrary_local_audio = true`; that permission
controls the separate `play_audio_source` MCP tool. The examples demonstrate
both speech and audio-file cues.

### UtterPipe provider configuration

A provider is an independently installed executable. Select its portable
command name without Windows' `.exe`; there is no registry or separate
`provider` field:

```toml
schema_version = 1

[tts]
enabled = true
backend = "utterpipe-espeak-ng"
model_id = "espeak-ng"
voice_id = "default"
maximum_characters = 300
```

Agent Speak checks its own executable directory and then each absolute `PATH`
directory for that exact backend. It starts one reusable provider process for
`serve`, owns its lifecycle, and keeps playback and decoding in Agent Speak.

| Provider | Use | Options and setup |
| --- | --- | --- |
| [`utterpipe-espeak-ng`](https://github.com/4piu/utterpipe-espeak-ng) | Embedded, offline eSpeak NG | [rate, pitch, amplitude](https://github.com/4piu/utterpipe-espeak-ng#agent-speak-configuration) |
| [`utterpipe-pocket-tts`](https://github.com/4piu/utterpipe-pocket-tts) | Local neural TTS | [threads, speed, seed, voice cache](https://github.com/4piu/utterpipe-pocket-tts#provider-options) |
| [`utterpipe-openai-http`](https://github.com/4piu/utterpipe-openai-http) | Local or remote OpenAI-compatible service | [endpoint, credentials, voices, audio format](https://github.com/4piu/utterpipe-openai-http#provider-options) |

`provider_options` is a provider-defined TOML table passed as JSON and fixed for
the process. It may contain a credential such as the OpenAI-compatible
provider's optional `api_key`; protect that plaintext profile and do not commit
it. `provider_environment` remains available for providers that explicitly
require allowlisted environment variables. See the
[provider configuration reference](docs/provider-configuration.md) for exact
environment, discovery, storage, and lifecycle behavior.

### Provider management commands

Provider models and voices change only through explicit human CLI commands,
never during MCP startup or a tool call.

| Command | Purpose |
| --- | --- |
| `agent-speak provider info --config ./agent-speak.toml` | Show the resolved executable and provider capabilities |
| `agent-speak provider models --config ./agent-speak.toml` | List installed or available models |
| `agent-speak voices --config ./agent-speak.toml` | List configured-provider voices |
| `agent-speak prepare --config ./agent-speak.toml` | Plan, confirm, and install required assets |
| `agent-speak provider voices import --config ./agent-speak.toml --source /absolute/reference.wav --id my-voice --consent-confirmed` | Import an approved reference voice |
| `agent-speak provider remove --config ./agent-speak.toml --artifact voice:my-voice` | Plan and remove an exact provider asset |

Agent Speak negotiates PCM16 WAV, raw PCM16, MP3, or Ogg Opus. A provider may
support more formats for other hosts; Agent Speak does not negotiate a format
it cannot decode.

## Playback and safety

| MCP tool | Purpose |
| --- | --- |
| `get_audio_capabilities` | Show the effective profile and available tools |
| `list_audio_cues` | List configured audio cue IDs and descriptions |
| `play_audio_cue` | Play a configured speech or audio-file cue |
| `speak_text` | Speak arbitrary text when enabled |
| `play_audio_source` | Play an arbitrary local audio file when enabled |

Calls are fire-and-forget: acceptance means the item entered the playback
queue, not that it finished or was audible. `enqueue` adds to the FIFO queue;
`interrupt` stops the active item, starts the replacement, and retains queued
items. WAV, MP3, FLAC, and Ogg Vorbis files are supported.

Enabling `arbitrary_local_audio` lets the agent try any absolute local regular
file readable by the Agent Speak process. Playback history is disabled by
default. Selecting a provider authorizes that native executable to run with
your user privileges; process separation is not a sandbox, and a remote
provider may receive every spoken text. Review [SECURITY.md](SECURITY.md).

## Troubleshooting

- Run `agent-speak validate --config <PATH>` before registering a profile.
- Run `agent-speak devices` again if a fixed endpoint is unavailable.
- On Linux, confirm the selected `utterpipe-*` executable is beside Agent Speak
  or in an absolute `PATH` directory, and confirm `aplay -l` works. PipeWire
  normally needs `pipewire-alsa`; PulseAudio needs its ALSA plugin.
- Diagnostics go to stderr; stdout is reserved for MCP messages while serving.

### Why not Windows Narrator / Siri

Apple and Microsoft, in their infinite wisdom, keep their best built-in voices
behind Narrator and Siri instead of exposing them through supported application
TTS APIs. Agent Speak lists only voices the public APIs actually allow, avoiding
brittle private-API hacks.

## More documentation

- [Provider configuration](docs/provider-configuration.md) — discovery,
  environment, options, storage, and lifecycle
- [Linux containers](docs/linux-containers.md) — edge-case ALSA/udev setup
- [UtterPipe integration contract](docs/utterpipe-integration.md) — host and
  provider protocol details for developers
- [Security](SECURITY.md) — trust boundaries and deployment guidance

## License

Licensed under the [Apache License 2.0](LICENSE).
