# Agent Speak

[![CI](https://github.com/4piu/agent-speak/actions/workflows/ci.yml/badge.svg)](https://github.com/4piu/agent-speak/actions/workflows/ci.yml)

Agent Speak is a local, Windows-first [Model Context Protocol (MCP)](https://modelcontextprotocol.io/) server that lets an agent use system text-to-speech and play audio. You decide which voices, sounds, files, and output devices are available.

Agent Speak is pre-release software.

## Requirements

- Windows with an audio output device
- Windows speech services for text-to-speech
- An MCP host that can run a local stdio server
- A prebuilt release, or Rust 1.88+ to build from source

## Install

Download the Windows ZIP and `.sha256` file from [GitHub Releases](https://github.com/4piu/agent-speak/releases). Verify and extract it in PowerShell:

```powershell
Get-FileHash .\agent-speak-vX.Y.Z-x86_64-pc-windows-msvc.zip -Algorithm SHA256
Expand-Archive .\agent-speak-vX.Y.Z-x86_64-pc-windows-msvc.zip -DestinationPath C:\Tools
```

To build from source instead:

```powershell
git clone https://github.com/4piu/agent-speak.git
cd agent-speak
cargo build --release --locked
```

## Quick start

Register `agent-speak.exe serve` with your MCP host. A generic host entry is:

```json
{
  "mcpServers": {
    "agent-speak": {
      "command": "C:\\Tools\\agent-speak-vX.Y.Z-x86_64-pc-windows-msvc\\agent-speak.exe",
      "args": ["serve"]
    }
  }
}
```

Use the executable's absolute path. Restart or reload the host, then ask the agent to call `get_audio_capabilities` followed by `speak_text`.

The quick profile enables arbitrary text-to-speech through the current default voice and output. Presets, arbitrary audio files, and history are disabled. Playback gain defaults to `0.4` within an allowed `0.0..0.7` range.

Common quick-profile options include:

```powershell
agent-speak.exe serve --voice-id <ID>
agent-speak.exe serve --maximum-gain 0.9 --default-gain 0.5
agent-speak.exe serve --maximum-text-characters 500 --log-level info
```

Run `agent-speak.exe serve --help` for the complete option list.

## Choose an output device

List active outputs without playing anything:

```powershell
agent-speak.exe devices
agent-speak.exe devices --format toml
```

The table shows friendly Windows names, stable device IDs, and the current default. Fixed-device profiles use the stable ID; display names are informational.

Generate a starter profile for the current machine:

```powershell
agent-speak.exe init
agent-speak.exe init --output .\my-agent-speak.toml
```

The generated profile includes every active output plus commented text and audio preset examples. The audio example points to a file you provide; Agent Speak does not bundle media. `init` never overwrites an existing file.

## Use a configuration profile

A TOML profile is required for presets, arbitrary local audio, history, or fixed output routing:

```powershell
agent-speak.exe validate --config .\my-agent-speak.toml
agent-speak.exe serve --config .\my-agent-speak.toml
```

Start with `agent-speak.exe init` or [examples/text-profile.toml](examples/text-profile.toml). Profiles are strict: unknown fields and invalid combinations are rejected. Relative preset and history paths are resolved from the profile's directory.

Important settings:

- `[permissions]` enables arbitrary text or arbitrary local audio.
- `[playback]` controls gain, queueing, concurrency, and optional duration limits.
- `[outputs]` defines friendly output aliases and whether each accepts audio, speech, or both.
- `[tts]` selects a voice and text-length limit.
- `[logging]` controls diagnostics and optional playback history.
- `[[presets]]` defines user-approved text or audio entries.

`maximum_audio_seconds` may be omitted or set to `0` for unlimited playback. A positive value limits decoded duration and runtime playback. Audio file size is not capped.

## Playback behavior

The exposed MCP tools follow the startup profile:

| Tool | Purpose |
| --- | --- |
| `get_audio_capabilities` | Show the effective profile and available tools |
| `list_audio_presets` | List configured preset IDs and descriptions |
| `play_audio_preset` | Play a configured text or audio preset |
| `speak_text` | Speak arbitrary text when enabled |
| `play_audio_source` | Play an arbitrary local audio file when enabled |

Calls are fire-and-forget: acceptance means the item entered the playback queue, not that it finished or was audible. `enqueue` adds an item to the FIFO queue. `interrupt` stops the active item, starts the replacement, and keeps already queued items.

WAV, MP3, FLAC, and Ogg Vorbis files are supported. Fixed output targets never silently reroute to another device when unavailable. The `system_default` target follows the current Windows default for each new item.

## Safety and privacy

- Enabling `arbitrary_local_audio` lets the agent try any absolute local regular file readable by the Agent Speak process. Leave it disabled if that access is not appropriate.
- Audio decoding runs inside Agent Speak. Treat presets and arbitrary files as untrusted media; use a restricted Windows account or OS sandbox if you require stronger isolation.
- File size and playback duration are uncapped by default. Set `maximum_audio_seconds` when prolonged playback is undesirable.
- Playback history is disabled by default. If enabled, protect the history file and opt into spoken-text retention only when needed.
- MCP host approvals are separate from Agent Speak. Agent Speak does not display per-call permission prompts.

## Troubleshooting

- Run `agent-speak.exe validate --config <PATH>` before registering a profile.
- Run `agent-speak.exe devices` again if a fixed endpoint is unavailable or its ID changed.
- Confirm Windows speech services and a voice are installed if TTS initialization fails.
- Diagnostics are written to stderr; stdout is reserved for MCP messages while serving.

## License

Licensed under the [Apache License 2.0](LICENSE).
