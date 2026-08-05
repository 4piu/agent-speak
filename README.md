# Agent Speak

[![CI](https://github.com/4piu/agent-speak/actions/workflows/ci.yml/badge.svg)](https://github.com/4piu/agent-speak/actions/workflows/ci.yml)

Agent Speak is a local Windows and macOS [Model Context Protocol (MCP)](https://modelcontextprotocol.io/) server that lets an agent use system text-to-speech and play audio. You decide which voices, sounds, files, and output devices are available.

Agent Speak is pre-release software.

## Requirements

- Windows with speech services, or macOS 15 or later
- An audio output device for audible playback
- An MCP host that can run a local stdio server
- A prebuilt release, or Rust 1.88+ to build from source

## Install

Download the archive and `.sha256` file for your platform from [GitHub Releases](https://github.com/4piu/agent-speak/releases).

On Windows, verify and extract the x64 ZIP in PowerShell:

```powershell
Get-FileHash .\agent-speak-vX.Y.Z-x86_64-pc-windows-msvc.zip -Algorithm SHA256
Expand-Archive .\agent-speak-vX.Y.Z-x86_64-pc-windows-msvc.zip -DestinationPath C:\Tools
```

On macOS, choose the `aarch64-apple-darwin` tarball for Apple silicon or `x86_64-apple-darwin` for an Intel Mac:

```sh
shasum -a 256 -c agent-speak-vX.Y.Z-aarch64-apple-darwin.tar.gz.sha256
tar -xzf agent-speak-vX.Y.Z-aarch64-apple-darwin.tar.gz
```

The pre-release macOS archives are not yet signed or notarized. If Gatekeeper blocks a verified download, approve it explicitly in System Settings > Privacy & Security, or build from source:

```sh
git clone https://github.com/4piu/agent-speak.git
cd agent-speak
cargo build --release --locked
```

To build from source on Windows instead:

```powershell
git clone https://github.com/4piu/agent-speak.git
cd agent-speak
cargo build --release --locked
```

## Quick start

Register `agent-speak serve` (`agent-speak.exe serve` on Windows) with your MCP host. A Windows host entry is:

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

The equivalent macOS entry uses the extracted executable path:

```json
{
  "mcpServers": {
    "agent-speak": {
      "command": "/Users/you/Tools/agent-speak-vX.Y.Z-aarch64-apple-darwin/agent-speak",
      "args": ["serve"]
    }
  }
}
```

Use the executable's absolute path. Restart or reload the host, then ask the agent to call `get_audio_capabilities` followed by `speak_text`.

The quick profile enables arbitrary text-to-speech through Agent Speak's TTS-API default voice and the current default output. Presets, arbitrary audio files, and history are disabled. Playback gain defaults to `0.4` within an allowed `0.0..0.7` range.

Common quick-profile options include:

```text
agent-speak serve --voice-id <ID>
agent-speak serve --maximum-gain 0.9 --default-gain 0.5
agent-speak serve --maximum-text-characters 500 --log-level info
```

Run `agent-speak serve --help` for the complete option list. Add the `.exe` suffix on Windows when the executable is not already resolved by your shell.

## Choose an output device

List active outputs without playing anything:

```text
agent-speak devices
agent-speak devices --format toml
```

The table shows platform-friendly names, stable CPAL device IDs, and the current default. Fixed-device profiles use the stable ID; display names are informational.

List voices exposed to applications by the system TTS API:

```text
agent-speak voices
```

Use a listed raw ID with `--voice-id`, or copy its escaped `config: voice_id = ...` line under `[tts]`. `[Agent Speak default]` is the voice used when `voice_id` is empty, not necessarily the default shown by another operating-system feature. Agent Speak can select only voices exposed by the platform application TTS API. In particular, macOS Siri or Spoken Content selections may include voices that AVSpeech does not expose and Agent Speak cannot synthesize.

Generate a starter profile for the current machine:

```text
agent-speak init
agent-speak init --output ./my-agent-speak.toml
```

The generated profile includes every active output plus commented text and audio preset examples. The audio example points to a file you provide; Agent Speak does not bundle media. `init` never overwrites an existing file.

## Use a configuration profile

A TOML profile is required for presets, arbitrary local audio, history, or fixed output routing:

```text
agent-speak validate --config ./my-agent-speak.toml
agent-speak serve --config ./my-agent-speak.toml
```

Start with `agent-speak init` or [examples/text-profile.toml](examples/text-profile.toml). Profiles are strict: unknown fields and invalid combinations are rejected. Relative preset and history paths are resolved from the profile's directory.

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

WAV, MP3, FLAC, and Ogg Vorbis files are supported. Fixed output targets never silently reroute to another device when unavailable. The `system_default` target follows the current operating-system default for each new item.

## Safety and privacy

- Enabling `arbitrary_local_audio` lets the agent try any absolute local regular file readable by the Agent Speak process. Leave it disabled if that access is not appropriate.
- Audio decoding runs inside Agent Speak. Treat presets and arbitrary files as untrusted media; use a restricted user account or OS sandbox if you require stronger isolation.
- File size and playback duration are uncapped by default. Set `maximum_audio_seconds` when prolonged playback is undesirable.
- Playback history is disabled by default. If enabled, protect the history file and opt into spoken-text retention only when needed.
- MCP host approvals are separate from Agent Speak. Agent Speak does not display per-call permission prompts.

## Troubleshooting

- Run `agent-speak validate --config <PATH>` before registering a profile.
- Run `agent-speak devices` again if a fixed endpoint is unavailable or its ID changed.
- Confirm Windows speech services or macOS voices are available if TTS initialization fails.
- Diagnostics are written to stderr; stdout is reserved for MCP messages while serving.

## Why not Windows Narrator / Siri

Apple and Microsoft, in their infinite wisdom, keep their best built-in voices behind Narrator and Siri instead of exposing them through supported application TTS APIs. Agent Speak therefore lists only voices the public APIs actually allow, avoiding brittle private-API hacks.

## License

Licensed under the [Apache License 2.0](LICENSE).
