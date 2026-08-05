# Agent Speak

[![CI](https://github.com/4piu/agent-speak/actions/workflows/ci.yml/badge.svg)](https://github.com/4piu/agent-speak/actions/workflows/ci.yml)

Agent Speak is a local, Windows-first [Model Context Protocol (MCP)](https://modelcontextprotocol.io/) server that gives an agent user-controlled access to system text-to-speech and audio playback. It does not prescribe event types: you choose what sounds or phrases mean in your workflow and tell the agent when to use them.

The default setup permits arbitrary text-to-speech through the default system voice and audio device. A TOML policy can instead—or additionally—allow a fixed pool of text/audio presets or arbitrary audio files from specific directories. The complete policy is loaded at startup, so Agent Speak never interrupts a tool call with a permission prompt.

Agent Speak is pre-release software. The automated suite and audible Windows acceptance tests cover TTS, queueing, interruption, volume limits, and WAV, MP3, FLAC, and Ogg Vorbis playback. Additional MCP host integrations are still being tested.

## Requirements

- Windows with a default audio output device; TTS also needs a working Windows speech service
- An MCP host that can start a local stdio server
- Either a prebuilt release ZIP or Rust 1.88+ to build from source

## Quick start

### Prebuilt release

Download `agent-speak-vX.Y.Z-x86_64-pc-windows-msvc.zip` and its `.sha256` file from [GitHub Releases](https://github.com/4piu/agent-speak/releases). Verify and extract it in PowerShell:

```powershell
Get-FileHash .\agent-speak-vX.Y.Z-x86_64-pc-windows-msvc.zip -Algorithm SHA256
Expand-Archive .\agent-speak-vX.Y.Z-x86_64-pc-windows-msvc.zip -DestinationPath C:\Tools
```

Compare the displayed hash with the first value in the downloaded `.sha256` file.

### Build from source

```powershell
git clone https://github.com/4piu/agent-speak.git
cd agent-speak
cargo build --release --locked
```

The executable is `target\release\agent-speak.exe`.

### Register it with an MCP host

Run the zero-configuration server with `serve`. A generic MCP host entry looks like this:

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

Use the absolute path where you extracted or built the executable. The exact configuration file and persistent-trust setting depend on the host. Restart or reload the host, then ask the agent to call `get_audio_capabilities` followed by `speak_text`.

The quick profile exposes only `get_audio_capabilities` and `speak_text`. Its defaults are:

| Setting | Default |
| --- | --- |
| Arbitrary text | Allowed |
| Presets / arbitrary local audio | Disabled |
| Voice / output | System defaults |
| Minimum / maximum / default gain | `0.0` / `0.7` / `0.4` |
| Concurrency | `enqueue` and `interrupt`; default `enqueue` |
| Waiting queue | 16 items |
| Text length | 300 Unicode characters |
| Accepted calls | 10 per rolling minute |
| History | Disabled |

For simple adjustments, keep the quick profile and add flags after `serve`:

| Flag | Purpose |
| --- | --- |
| `--voice-id <ID>` | Select a system TTS voice; an empty value uses the default |
| `--minimum-gain <0.0..1.0>` | Lowest gain an MCP call may request |
| `--maximum-gain <0.0..1.0>` | Highest gain an MCP call may request |
| `--default-gain <0.0..1.0>` | Gain used when a call omits it |
| `--maximum-text-characters <COUNT>` | Maximum arbitrary TTS input length |
| `--maximum-plays-per-minute <COUNT>` | Per-server accepted-call rate limit |
| `--log-level <LEVEL>` | Stderr verbosity: `error`, `warning`, `info`, `debug`, or `trace` |

For example:

```powershell
agent-speak.exe serve --maximum-gain 0.9 --default-gain 0.5 --log-level info
```

## File-based configuration

Use a complete TOML profile for presets, arbitrary local audio, history, or settings outside the quick flags:

```powershell
agent-speak.exe validate --config .\examples\text-profile.toml
agent-speak.exe serve --config .\examples\text-profile.toml
```

See [examples/text-profile.toml](examples/text-profile.toml) for a complete text-only profile. A file profile does not merge with the quick profile, and `--config` cannot be combined with quick-profile flags. Unknown fields and invalid combinations are rejected before the server starts.

Relative paths in `approved_directories`, `history_path`, and audio preset `source` fields are resolved from the configuration file's directory. Configured files/directories must already exist where noted. UNC and Windows device paths are rejected.

### Root fields

All root sections are required. `presets` may be omitted and then defaults to an empty list.

| Field | Meaning |
| --- | --- |
| `schema_version` | Must be `1` |
| `profile_name` | Model-visible name, 1–80 Unicode characters |
| `[permissions]` | Which arbitrary inputs are authorized |
| `[playback]` | Gain, concurrency, size, duration, queue, and rate limits |
| `[tts]` | Speech backend and text limits |
| `[logging]` | Diagnostic and optional history settings |
| `[[presets]]` | Zero or more startup-approved text/audio entries |

### Permissions

| Field | Type | Meaning |
| --- | --- | --- |
| `arbitrary_text` | boolean | Expose `speak_text` when `tts.enabled` is also true |
| `arbitrary_local_audio` | boolean | Expose `play_audio_source` |
| `approved_directories` | path array | Roots available to arbitrary audio; at least one is required when arbitrary audio is enabled |

Preset audio files do not have to be under an approved directory: each preset source is independently approved by being named in the startup profile.

### Playback

| Field | Valid values | Meaning |
| --- | --- | --- |
| `minimum_gain` | `0.0..1.0` | Lowest per-call or preset gain |
| `maximum_gain` | `0.0..1.0` | Highest per-call or preset gain |
| `default_gain` | Within the configured range | Used when an arbitrary call omits gain |
| `default_concurrency` | `enqueue` or `interrupt` | Used when a call omits concurrency; must also be allowed |
| `allowed_concurrency` | Unique array of `enqueue`, `interrupt` | Modes an MCP call may request |
| `maximum_queue_items` | `1..1024` | Waiting items; the currently playing item is not counted |
| `maximum_file_bytes` | `1..1073741824` | Per-file limit (up to 1 GiB) |
| `maximum_audio_seconds` | `1..86400` | Decoded duration limit (up to 24 hours) |
| `maximum_plays_per_minute` | `1..10000` | Accepted calls in a rolling one-minute window |

Gain is normalized: `0.0` is silent and `1.0` is the backend's unamplified level. It is not a master system-volume control.

### Text-to-speech

| Field | Valid values | Meaning |
| --- | --- | --- |
| `enabled` | boolean | Initialize TTS and allow text presets; disabling it also hides arbitrary TTS |
| `voice_id` | string | System voice identifier; `""` selects the default voice |
| `maximum_characters` | `1..10000` | Unicode-character limit for arbitrary text and every text preset |

### Logging and history

| Field | Valid values | Meaning |
| --- | --- | --- |
| `level` | `error`, `warning`, `info`, `debug`, `trace` | Diagnostics written to stderr |
| `history_enabled` | boolean | Append playback lifecycle records as JSON Lines |
| `history_path` | file path | Required when history is enabled; its parent directory must exist |
| `history_include_spoken_text` | boolean | Include arbitrary spoken text in history records |

History is opt-in and non-blocking. It may contain timestamps, playback IDs, preset IDs, gain, concurrency, state, error codes, and—only when explicitly enabled—arbitrary spoken text. Protect the history file according to the sensitivity of your workflow.

### Presets

At most 256 presets may be configured. Every preset has:

| Field | Requirement |
| --- | --- |
| `id` | Unique; must match `[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}` |
| `kind` | `text` or `audio_file` |
| `description` | Optional model-visible guidance, at most 1,000 Unicode characters |
| `default_gain` | Within the configured playback gain range |
| `text` | Required only for `text`; non-empty and within `tts.maximum_characters` |
| `source` | Required only for `audio_file`; an existing regular audio file |

Example presets:

```toml
[[presets]]
id = "finished"
kind = "text"
text = "The agent has finished its task."
description = "Use when a long-running task is complete."
default_gain = 0.4

[[presets]]
id = "needs-attention"
kind = "audio_file"
source = "sounds/needs-attention.ogg"
description = "Use when work cannot continue without user input."
default_gain = 0.4
```

### Tool discovery

Agent Speak derives its MCP tool list from the startup policy. Disabled capabilities are absent from discovery.

| Tool | Exposed when |
| --- | --- |
| `get_audio_capabilities` | Always |
| `list_audio_presets` | At least one preset exists |
| `play_audio_preset` | At least one preset exists |
| `speak_text` | `permissions.arbitrary_text` and `tts.enabled` are both true |
| `play_audio_source` | `permissions.arbitrary_local_audio` is true |

Capability output and errors do not reveal approved directories, preset source paths, preset speech text, or history paths.

## Playback semantics

Calls are fire-and-forget: success means the bounded playback actor accepted the job, not that playback finished or was audible. One item plays at a time.

- `enqueue` appends behind the active item in FIFO order.
- `interrupt` stops the active item, starts the replacement, and preserves items already waiting in the queue.

Supported files are WAV, MP3, FLAC, and Ogg Vorbis. Files are content-sniffed and decoder-preflighted instead of trusted by extension. Arbitrary paths must be absolute, resolve inside a canonical approved directory, identify a regular file, and fit the configured byte and decoded-duration limits.

## Security model

Arbitrary TTS is high trust: an agent may speak any text within the configured limit. Arbitrary local audio is a separate permission and does not grant network, shell, or general filesystem access. Agent-generated media remains untrusted decoder input even inside an approved directory.

The server never invokes a shell, external media player, URL, or file association. In serve mode, diagnostics go only to stderr because stdout is reserved for MCP JSON-RPC. Host-level approval and trust behavior is controlled by the MCP host; Agent Speak itself performs no per-request prompting.

## Development

```powershell
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

Three hardware/service spikes are ignored by the normal suite. Run them on a suitable Windows machine with a default audio output and `ffmpeg` available for decoder fixtures:

```powershell
cargo test spike_ --lib -- --ignored --test-threads=1
```

## Releases

Releases are tag-driven. Set the version in `Cargo.toml`, commit it, and push an annotated tag with the exact matching `v` prefix:

```powershell
git tag -a v0.1.0 -m "Agent Speak v0.1.0"
git push origin v0.1.0
```

The [release workflow](.github/workflows/release.yml) rejects a mismatched tag, runs the release test suite, builds Windows x64, creates a ZIP and SHA-256 checksum, and publishes both files in a GitHub Release with generated notes.

## License

Licensed under the [Apache License 2.0](LICENSE).
