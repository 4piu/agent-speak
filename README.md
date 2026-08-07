# Agent Speak

[![CI](https://github.com/4piu/agent-speak/actions/workflows/ci.yml/badge.svg)](https://github.com/4piu/agent-speak/actions/workflows/ci.yml)

Agent Speak is a local Windows, macOS, and Linux [Model Context Protocol (MCP)](https://modelcontextprotocol.io/) server that lets an agent use text-to-speech and play audio. You decide which voices, sounds, files, and output devices are available.

Agent Speak is pre-release software.

## Requirements

- Windows with speech services, macOS 15 or later, or Linux with ALSA-compatible audio
- On Linux, an independently installed UtterPipe provider for text-to-speech;
  `utterpipe-espeak-ng` is the quick-profile default
- An audio output device for audible playback
- An MCP host that can run a local stdio server
- A prebuilt release, or Rust 1.89+ to build from source

## Install

Download the archive and `.sha256` file for your platform from [GitHub Releases](https://github.com/4piu/agent-speak/releases).

Or install the latest checksum-verified release for the current user:

```sh
curl -fsSL https://raw.githubusercontent.com/4piu/agent-speak/master/install.sh | sh
```

```powershell
irm https://raw.githubusercontent.com/4piu/agent-speak/master/install.ps1 | iex
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/4piu/agent-speak/master/install.ps1))) -Uninstall
```

Pass `--uninstall` to the shell script or use the second PowerShell command to
remove Agent Speak. User-selected profiles, history files, and independently
managed provider assets are preserved.

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

On Linux, install your distribution's ALSA runtime package. PipeWire and PulseAudio are supported through their ALSA compatibility plugins. For example, on Arch Linux:

```sh
sudo pacman -S alsa-lib pipewire-alsa
sha256sum -c agent-speak-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz.sha256
tar -xzf agent-speak-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz
```

For text-to-speech, install a provider independently. The self-contained
[`utterpipe-espeak-ng`](https://github.com/4piu/utterpipe-espeak-ng) executable
needs no system eSpeak installation; put it beside `agent-speak` or on `PATH`.
To build Agent Speak on Linux, install the distribution's ALSA development
package, then run `cargo build --release --locked`.

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

On Linux, use the executable from the Linux tarball:

```json
{
  "mcpServers": {
    "agent-speak": {
      "command": "/home/you/Tools/agent-speak-vX.Y.Z-x86_64-unknown-linux-gnu/agent-speak",
      "args": ["serve"]
    }
  }
}
```

Use the executable's absolute path. Restart or reload the host, then ask the agent to call `get_audio_capabilities` followed by `speak_text`.

The quick profile enables arbitrary text-to-speech and the current default
output. Windows and macOS use their public application TTS API; Linux discovers
the `espeak-ng` UtterPipe provider beside Agent Speak or on `PATH`. Presets,
arbitrary audio files, and history are disabled. Playback gain defaults to
`0.4` within an allowed `0.0..0.7` range.

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

List voices exposed by the Windows/macOS native TTS backend:

```text
agent-speak voices
```

On Linux, list the configured provider catalog instead:

```text
agent-speak init --output ./agent-speak.toml
agent-speak voices --config ./agent-speak.toml
```

Use a listed raw ID with `--voice-id`, or copy its escaped `voice_id` value under
`[tts]`. `[Agent Speak default]` is the voice used when a native `voice_id` is
empty, not necessarily the default shown by another operating-system feature.
In particular, macOS Siri or Spoken Content selections may include voices that
AVSpeech does not expose and Agent Speak cannot synthesize.

## Linux containers

Agent Speak needs the ALSA device nodes and udev's sound-card metadata. A tested unprivileged LXC setup is:

```ini
lxc.cgroup2.devices.allow = c 116:* rwm
lxc.mount.entry = /dev/snd dev/snd none bind,optional,create=dir
lxc.mount.entry = tmpfs run tmpfs rw,nosuid,nodev,mode=0755,size=20%,nr_inodes=800k,create=dir
lxc.mount.entry = /run/udev/data run/udev/data none bind,ro,create=dir
```

Mount only `/run/udev/data`, not all of `/run/udev`; the latter can expose the host udev control socket. The explicit `/run` entry must precede the udev-data bind so the guest's runtime tmpfs does not hide it.

For an unprivileged container, grant the mapped host UID access whenever sound nodes are created. If container UID `1000` maps to host UID `101000`, place this late rule at `/etc/udev/rules.d/99-z-lxc-audio.rules` on the host:

```udev
ACTION=="add", SUBSYSTEM=="sound", ENV{DEVNAME}!="", RUN{program}+="/usr/bin/setfacl -m u:101000:rw $env{DEVNAME}"
```

The late filename matters: the standard `uaccess` rule can otherwise recalculate and erase an earlier ACL. Reload and apply it with:

```sh
sudo udevadm control --reload-rules
sudo udevadm trigger --subsystem-match=sound --action=add
sudo udevadm settle
```

Adjust `101000` for the container's actual UID map. Inside the container, `aplay -l`, `agent-speak devices`, and `wpctl status` should then show the expected card and sink.

Generate a starter profile for the current machine:

```text
agent-speak init
agent-speak init --output ./my-agent-speak.toml
```

The generated profile includes every active output plus commented text and audio preset examples. The audio example points to a file you provide; Agent Speak does not bundle media. `init` never overwrites an existing file.

## Use a configuration profile

A TOML profile is required for presets, arbitrary local audio, history, fixed output routing, or an external UtterPipe TTS provider:

```text
agent-speak validate --config ./my-agent-speak.toml
agent-speak serve --config ./my-agent-speak.toml
```

Start with `agent-speak init`, [examples/text-profile.toml](examples/text-profile.toml)
for native Windows/macOS TTS, or
[examples/espeak-provider.toml](examples/espeak-provider.toml) for the
cross-platform eSpeak provider. Profiles are strict: unknown fields and invalid
combinations are rejected. Relative preset and history paths are resolved from
the profile's directory.

Important settings:

- `[permissions]` enables arbitrary text or arbitrary local audio.
- `[playback]` controls gain, queueing, concurrency, and optional duration limits.
- `[outputs]` defines friendly output aliases and whether each accepts audio, speech, or both.
- `[tts]` selects the `system` or `utterpipe` backend, voice, and text-length
  limit. Native `system` TTS is available on Windows and macOS. Existing
  schema-1 profiles still parse as system-TTS profiles, but Linux users must
  migrate them to an UtterPipe backend; `init` emits the correct schema-2
  backend for the current platform.
- `[logging]` controls diagnostics and optional playback history.
- `[[presets]]` defines user-approved text or audio entries.

`maximum_audio_seconds` may be omitted or set to `0` for unlimited playback. A positive value limits decoded duration and runtime playback. Audio file size is not capped.

An independently installed provider can supply a natural voice without being bundled into Agent Speak. The executable is discovered as `utterpipe-<provider>` beside Agent Speak and then in absolute `PATH` directories. For example:

```toml
schema_version = 2

[tts]
enabled = true
backend = "utterpipe"
provider = "pocket-tts"
model_id = "pocket-tts-int8-2026-01-26"
voice_id = "my-voice"
maximum_characters = 500
provider_environment = []

[tts.provider_options]
speed = 1.0
```

The Linux quick-profile equivalent is:

```toml
[tts]
enabled = true
backend = "utterpipe"
provider = "espeak-ng"
model_id = "espeak-ng"
voice_id = "default"
maximum_characters = 300
provider_environment = []

[tts.provider_options]
rate_wpm = 175
pitch = 50
amplitude = 100
```

Provider options are provider-defined, validated at startup, and fixed for the `serve` process. Inspect and prepare a configured provider explicitly:

```text
agent-speak provider info --config ./my-agent-speak.toml
agent-speak provider models --config ./my-agent-speak.toml
agent-speak voices --config ./my-agent-speak.toml
agent-speak prepare --config ./my-agent-speak.toml
agent-speak provider voices import --config ./my-agent-speak.toml --source /absolute/reference.wav --id my-voice --consent-confirmed
agent-speak provider remove --config ./my-agent-speak.toml --artifact voice:my-voice
```

Preparation, voice import, and removal are human CLI operations and are never performed by MCP startup or tool calls. See [the integration contract](docs/utterpipe-integration.md) for discovery, storage, and protocol details.

UtterPipe providers may return PCM16 WAV, incremental raw PCM16, MP3, or Ogg
Opus. Providers relay compressed streams unchanged; Agent Speak performs the
single container/codec validation and decoding pass. Incremental compressed
audio uses bounded encoded and decoded queues, and its 200 ms startup prebuffer
is measured from decoded samples.

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
- MP3 and Ogg Opus provider decoding is compiled into the single Agent Speak executable; no external decoder process or runtime library is required.
- File size and playback duration are uncapped by default. Set `maximum_audio_seconds` when prolonged playback is undesirable.
- Playback history is disabled by default. If enabled, protect the history file and opt into spoken-text retention only when needed.
- MCP host approvals are separate from Agent Speak. Agent Speak does not display per-call permission prompts.
- Selecting an UtterPipe provider authorizes that native executable to run with your user privileges; process separation is not a sandbox. A remote provider may receive every spoken text. Review [SECURITY.md](SECURITY.md) before enabling one.

## Troubleshooting

- Run `agent-speak validate --config <PATH>` before registering a profile.
- Run `agent-speak devices` again if a fixed endpoint is unavailable or its ID changed.
- Confirm Windows speech services or macOS voices are available for native TTS.
- On Linux, confirm `utterpipe-espeak-ng` (or the selected provider) is beside
  Agent Speak or in an absolute `PATH` directory; `agent-speak validate
  --config <PATH>` reports discovery and initialization errors.
- On Linux, confirm `aplay -l` works and that an ALSA default output is configured. PipeWire users normally need `pipewire-alsa`; PulseAudio users need its ALSA plugin.
- Diagnostics are written to stderr; stdout is reserved for MCP messages while serving.

## Why not Windows Narrator / Siri

Apple and Microsoft, in their infinite wisdom, keep their best built-in voices behind Narrator and Siri instead of exposing them through supported application TTS APIs. Agent Speak therefore lists only voices the public APIs actually allow, avoiding brittle private-API hacks.

## License

Licensed under the [Apache License 2.0](LICENSE).
