# Agent Speak — MCP text-to-speech and audio playback

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
that directory to `PATH` if the installer asks you to. Releases are not yet
signed or notarized; after verifying the download, macOS may require explicit
approval in System Settings > Privacy & Security.

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

With no discovered config file, Agent Speak uses a safe quick profile:
arbitrary speech is enabled, while audio files, audio cues, and history are
disabled. Windows and macOS use their public system TTS APIs; Linux discovers
`utterpipe-espeak-ng` beside Agent Speak or on `PATH`.

| Command | Purpose | Example |
| --- | --- | --- |
| `devices` | List active output devices and stable IDs | `agent-speak devices` |
| `voices` | List native Windows/macOS voices | `agent-speak voices` |
| `serve` | Run MCP with discovered layers or the quick fallback | `agent-speak serve` |

Quick-profile settings are command-line options, not TOML fields. Supplying any
of these options explicitly selects the quick profile and bypasses config-file
discovery:

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

Agent Speak has no per-call approval prompt. Unattended alerts therefore need
an MCP host that can persist approval after you review the startup profile. For
Codex, register the command with `codex mcp add`, then keep approval scoped to
the reviewed tools in `~/.codex/config.toml`:

```toml
[mcp_servers.agent-speak]
command = "/absolute/path/to/agent-speak"
args = ["serve"]
enabled_tools = ["cancel_playback", "get_audio_capabilities", "get_playback_status", "speak_text"]
default_tools_approval_mode = "prompt"

[mcp_servers.agent-speak.tools.cancel_playback]
approval_mode = "prompt"

[mcp_servers.agent-speak.tools.get_audio_capabilities]
approval_mode = "approve"

[mcp_servers.agent-speak.tools.get_playback_status]
approval_mode = "approve"

[mcp_servers.agent-speak.tools.speak_text]
approval_mode = "approve"
```

This keeps tools exposed by a later, broader profile unavailable until you
explicitly review and add them.
See the [Codex MCP configuration reference](https://developers.openai.com/codex/mcp)
for the registration and approval options.

#### Try it

Talk to the agent naturally; Agent Speak's MCP metadata teaches it the tool
workflow. For the quick profile, try:

- `Say “Agent Speak is ready” out loud.`
- `When you finish reviewing this file, speak a one-sentence summary.`

With configured audio cues, try `What audio cues are available?` or `Use the
completion audio cue when this task is done.` Installing the MCP makes audible
tools available, but the agent should use them only when requested or when an
approved audio cue description clearly applies.

To register one complete, isolated profile, add its absolute path to the
arguments:

```json
"args": ["serve", "--config", "/absolute/path/to/agent-speak.toml"]
```

### Configuration commands

| Command | Purpose | Example |
| --- | --- | --- |
| `init` | Generate a complete profile for the current machine | `agent-speak init --output ./agent-speak.toml` |
| `validate [--config]` | Check the discovered or explicit profile and report its source | `agent-speak validate` |
| `serve [--config] [--control-file]` | Run MCP, optionally with a private local-UI control descriptor | `agent-speak serve` |
| `devices [--format table|toml|json]` | List outputs or print copyable/versioned data | `agent-speak devices --format json` |
| `voices [--format table|json]` | List native Windows/macOS voices for people or integrations | `agent-speak voices --format json` |

`init` never overwrites an existing file. Profile parsing is strict: unknown
fields and invalid combinations are rejected.

The JSON device and voice inventories are versioned with
`"schema_version": 1` and are intended for local UI integrations. Device JSON
contains stable IDs, display names, and default status; voice JSON contains the
same voice metadata shown by the human-readable table. These read-only commands
inspect the current host and do not load or merge a profile.

### Layered discovery

When a config-consuming command omits `--config`, Agent Speak starts with its
built-in quick-profile defaults and loads each existing file in this order:

| Layer | Windows | macOS/Linux |
| --- | --- | --- |
| System | `%ProgramData%\Agent Speak\agent-speak.toml` | `/etc/agent-speak.toml` |
| User | `%USERPROFILE%\.agent-speak.toml` | `$HOME/.agent-speak.toml` |
| Working directory | `.\.agent-speak.toml` | `./.agent-speak.toml` |

Later layers have higher priority. Tables merge recursively; later scalars and
arrays replace earlier values wholesale. A changed `tts.backend` also discards
fields belonging to the previous backend, so provider options or credentials
cannot accidentally carry into a different provider. This makes a complete
user profile plus a small project permission layer natural, and also permits
smaller layers that inherit built-in defaults:

```toml
# ~/.agent-speak.toml
[tts]
backend = "utterpipe-pocket-tts"

[tts.provider_options]
voice = "alba"
```

```toml
# project/.agent-speak.toml
[permissions]
arbitrary_text = true
arbitrary_local_audio = false
```

Known relative `logging.history_path` and audio-cue `source` paths resolve from
the layer that declared them. Provider options are opaque; relative strings
inside `provider_options` are passed literally, so use absolute paths when a
provider option represents a file. Any present but unreadable, malformed, or
invalid layer fails the command—Agent Speak never falls back to a partial
policy. `agent-speak validate` prints the loaded sources from lowest to highest
priority.

An explicit `--config PATH` always loads that one complete file with no
defaults, discovery, or merging. This is how the VS Code extension keeps its
managed instance isolated. Working-directory discovery uses the process working
directory chosen by the shell or MCP host; use an explicit path when that
directory is not stable.

### Profile file

Start from one of these complete examples:

- [native Windows/macOS TTS](examples/text-profile.toml)
- [cross-platform eSpeak NG](examples/espeak-provider.toml)
- [OpenAI-compatible HTTP TTS](examples/openai-http-provider.toml)
- [offline Pocket TTS](examples/pocket-provider.toml)

| Section | Purpose |
| --- | --- |
| `[permissions]` | Allow arbitrary text or arbitrary local audio |
| `[playback]` | Set gain, queueing, concurrency, and duration limits |
| `[outputs]` | Name allowed default or fixed output devices |
| `[tts]` | Select the backend, audio policy, provider permissions, and text limit |
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

# Selects the speech backend and host-side synthesis policy.
[tts]
enabled = true
backend = "utterpipe-espeak-ng"
maximum_characters = 300
agent_utterance_options = ["rate_wpm", "pitch"]

# Sets per-request provider defaults that authorized agent values may override.
[tts.utterance_options]
voice = "default"
rate_wpm = 175
pitch = 50
```

Agent Speak checks its own executable directory and then each absolute `PATH`
directory for that exact backend. It starts one reusable provider process for
`serve`, owns its lifecycle, and keeps playback and decoding in Agent Speak.

| Provider | Use | Options and setup |
| --- | --- | --- |
| [`utterpipe-espeak-ng`](https://github.com/4piu/utterpipe-espeak-ng) | Embedded, offline eSpeak NG | [voice, rate, pitch, and amplitude](https://github.com/4piu/utterpipe-espeak-ng#agent-speak-configuration) |
| [`utterpipe-pocket-tts`](https://github.com/4piu/utterpipe-pocket-tts) | Local neural TTS | [model/voice setup and request controls](https://github.com/4piu/utterpipe-pocket-tts#provider-options) |
| [`utterpipe-openai-http`](https://github.com/4piu/utterpipe-openai-http) | Local or remote OpenAI-compatible service | [endpoint/model setup and request controls](https://github.com/4piu/utterpipe-openai-http#configuration) |

`provider_options` is a provider-defined TOML table passed as JSON and fixed for
the process; expensive model loading and endpoint or credential settings belong
there. `utterance_options` contains configured defaults sent on every synthesis,
such as a cheap voice, speed, or tone choice. A provider assigns each key to
exactly one of those lifecycles.
Credentials such as `provider_options.api_key` remain plaintext in the profile;
protect the file and do not commit it.

`agent_utterance_options` is a simple permission allowlist. At startup, the
provider supplies the exact type, range, choices, and agent-facing explanation
for each available per-utterance control. Agent Speak exposes only the named
controls beneath `speak_text.utterance_options`, validates every value locally,
and overlays it onto configured `utterance_options` without assigning
engine-specific meaning. A request never changes later speech.

Optional `audio_deliveries` is the host's ordered format policy. When omitted,
Agent Speak automatically prefers low-latency compressed delivery, then its
other decodable pairs; every synthesis explicitly selects one initialized pair.

`provider_environment` remains available for providers that explicitly require
allowlisted environment variables. See the
[provider configuration reference](docs/provider-configuration.md) for exact
environment, discovery, storage, and lifecycle behavior.

### Provider management commands

Provider catalogs and assets change only through explicit human CLI commands,
never during MCP startup or a tool call. Catalog IDs and import kinds come from
`provider info`; names such as `models` or `voices` are provider conventions,
not hard-coded Agent Speak concepts.

| Command | Purpose |
| --- | --- |
| `agent-speak provider info` | Show the resolved executable and provider capabilities |
| `agent-speak provider catalog --catalog voices` | List one provider-declared catalog |
| `agent-speak prepare` | Plan, confirm, and install required assets |
| `agent-speak provider import --kind voice --source /absolute/reference.wav --id my-voice --consent-confirmed` | Import a file using a provider-declared kind |
| `agent-speak provider remove --artifact voice:my-voice` | Plan and remove an exact provider asset |

These commands use discovered layers by default and accept `--config PATH` for
one complete explicit profile.

Agent Speak negotiates PCM16 WAV, raw PCM16, MP3, or Ogg Opus. A provider may
support more formats for other hosts; Agent Speak does not negotiate a format
it cannot decode.

## Playback and safety

| MCP tool | Purpose |
| --- | --- |
| `cancel_playback` | Stop an active item or remove a queued item by playback ID |
| `get_audio_capabilities` | Show the effective profile and available tools |
| `get_playback_status` | Inspect the current or retained terminal state of an accepted playback ID |
| `list_audio_cues` | List configured audio cue IDs and descriptions |
| `play_audio_cue` | Play a configured speech or audio-file cue |
| `speak_text` | Speak arbitrary text when enabled |
| `play_audio_source` | Play an arbitrary local audio file when enabled |

Calls are fire-and-forget: acceptance means the item entered the playback
queue, not that it finished or was audible. `enqueue` adds to the FIFO queue;
`interrupt` stops every active item, starts the replacement, and retains queued
items. When startup policy allows `mix`, it starts beside active playback until
`maximum_mix_streams` is reached, then waits in the same FIFO. An earlier
enqueued item remains a barrier, so later mix requests cannot skip it. On each
physical output, requested gains are unchanged while their sum is at most 1.0;
above that, Agent Speak scales all active gains proportionally to preserve
relative levels and headroom. Use the returned `playback_id` with
`get_playback_status` when terminal confirmation matters. States are
`accepted`, `playing`, `completed`,
`interrupted`, and `failed`; `completed` is backend playback completion, not
human acknowledgement. `cancel_playback` stops an active item or removes a
queued item; repeating it for a terminal ID is a successful no-op with
`cancelled = false`. All in-flight states and the newest 256 terminal states are
retained for the life of that server process. Status and cancellation results
never include spoken text, cue text, or source paths. WAV, MP3, FLAC, and Ogg
Vorbis files are supported.

Local human-facing integrations can opt into a separate control channel with
`agent-speak serve --control-file /absolute/private/control.json`. Agent Speak
binds an ephemeral IPv4 loopback listener and creates that new descriptor with
a random session ID and bearer token; on POSIX the descriptor mode is `0600`.
The channel provides a sanitized newest-first lifecycle snapshot, targeted
cancellation, and an emergency stop that discards queued work and stops the
active item. It is never exposed as an MCP tool and returns no spoken text,
source paths, output identities, or provider diagnostics. Treat the descriptor
as a same-user secret and let the launching integration choose and protect its
path. Agent Speak removes it on orderly shutdown.

Enabling `arbitrary_local_audio` lets the agent try any absolute local regular
file readable by the Agent Speak process. Playback history is disabled by
default. Selecting a provider authorizes that native executable to run with
your user privileges; process separation is not a sandbox, and a remote
provider may receive every spoken text. Review [SECURITY.md](SECURITY.md).

## Troubleshooting

- Run `agent-speak validate` for discovered layers or
  `agent-speak validate --config <PATH>` for one explicit profile before
  registration.
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
- [Third-party notices](THIRD_PARTY_NOTICES.md) — distributable dependency
  licenses and source obligations
- [Release integrity](docs/release-integrity.md) — checksum, signing, and
  notarization status

## License

Licensed under the [Apache License 2.0](LICENSE).
