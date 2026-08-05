# Agent Speak

[![CI](https://github.com/4piu/agent-speak/actions/workflows/ci.yml/badge.svg)](https://github.com/4piu/agent-speak/actions/workflows/ci.yml)

Agent Speak is a local, Windows-first [Model Context Protocol (MCP)](https://modelcontextprotocol.io/) server that gives an agent user-controlled access to system text-to-speech and audio playback. It does not prescribe event types: you choose what sounds or phrases mean in your workflow and tell the agent when to use them.

The default setup permits arbitrary text-to-speech through the default system voice and audio device. A TOML policy can instead—or additionally—allow a fixed pool of text/audio presets, arbitrary audio files from any local path, and a named allowlist of output devices. The complete policy is loaded at startup, so Agent Speak never interrupts a tool call with a permission prompt.

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
| History | Disabled |

For simple adjustments, keep the quick profile and add flags after `serve`:

| Flag | Purpose |
| --- | --- |
| `--voice-id <ID>` | Select a system TTS voice; an empty value uses the default |
| `--minimum-gain <0.0..1.0>` | Lowest gain an MCP call may request |
| `--maximum-gain <0.0..1.0>` | Highest gain an MCP call may request |
| `--default-gain <0.0..1.0>` | Gain used when a call omits it |
| `--maximum-text-characters <COUNT>` | Maximum arbitrary TTS input length |
| `--log-level <LEVEL>` | Stderr verbosity: `error`, `warning`, `info`, `debug`, or `trace` |

For example:

```powershell
agent-speak.exe serve --maximum-gain 0.9 --default-gain 0.5 --log-level info
```

The quick profile already provides the simplest requested behavior: arbitrary text is allowed and omitted output selection follows the current system default. No configuration file or extra flag is needed.

### Discover output devices

List active output endpoints without opening an audio stream:

```powershell
agent-speak.exe devices
agent-speak.exe devices --format toml
```

The normal view shows each Windows-friendly endpoint name, its stable CPAL device ID, and which endpoint is currently the default. The TOML view emits an editable `[outputs]` section with readable aliases such as `headphones-bt-5181pro`. Remove any generated device targets you do not want an agent to use and copy the section into a file profile. Display names are informational only; fixed routing still uses the generated stable ID.

### Generate a starter configuration

Create a complete profile containing the currently active output devices:

```powershell
agent-speak.exe init
agent-speak.exe init --output .\my-agent-speak.toml
```

The default destination is `agent-speak.toml`. The generated profile uses the conservative quick-profile permissions, keeps `system` as the default target, adds every currently active endpoint under a readable fixed-device alias, and ends with commented text/audio preset examples. Review the file and remove outputs or permissions you do not want before registering it with an MCP host. `init` never overwrites an existing file.

## File-based configuration

Use a complete TOML profile for presets, arbitrary local audio, history, or settings outside the quick flags:

```powershell
agent-speak.exe validate --config .\examples\text-profile.toml
agent-speak.exe serve --config .\examples\text-profile.toml
```

See [examples/text-profile.toml](examples/text-profile.toml) for a complete text-only profile. A file profile does not merge with the quick profile, and `--config` cannot be combined with quick-profile flags. Unknown fields and invalid combinations are rejected before the server starts.

Relative paths in `history_path` and audio preset `source` fields are resolved from the configuration file's directory. Configured preset files must already exist. UNC shares and Windows device paths are rejected.

### Root fields

The policy sections shown below are strict: unknown fields are rejected and `outputs` is required. `presets` may be omitted and defaults to an empty list.

| Field | Meaning |
| --- | --- |
| `schema_version` | Must be `1` |
| `profile_name` | Model-visible name, 1–80 Unicode characters |
| `[permissions]` | Which arbitrary inputs are authorized |
| `[playback]` | Gain, concurrency, and queue behavior |
| `[outputs]` | Friendly, startup-approved output aliases and their permissions |
| `[tts]` | Speech backend and text limits |
| `[logging]` | Diagnostic and optional history settings |
| `[[presets]]` | Zero or more startup-approved text/audio entries |

### Permissions

| Field | Type | Meaning |
| --- | --- | --- |
| `arbitrary_text` | boolean | Expose `speak_text` when `tts.enabled` is also true |
| `arbitrary_local_audio` | boolean | Expose `play_audio_source` for any absolute local regular file the server account can read |

`arbitrary_local_audio` is a broad capability, not a directory sandbox. Enabling it lets the agent attempt to open any local path visible to the server account. Preset paths are fixed by configuration, but their file contents are still treated as untrusted media.

Profiles generated by an earlier pre-release build may contain `approved_directories`. Remove that field; strict parsing rejects it so it cannot be mistaken for an enforced security boundary.

### Playback

| Field | Valid values | Meaning |
| --- | --- | --- |
| `minimum_gain` | `0.0..1.0` | Lowest per-call or preset gain |
| `maximum_gain` | `0.0..1.0` | Highest per-call or preset gain |
| `default_gain` | Within the configured range | Used when an arbitrary call omits gain |
| `default_concurrency` | `enqueue` or `interrupt` | Used when a call omits concurrency; must also be allowed |
| `allowed_concurrency` | Unique array of `enqueue`, `interrupt` | Modes an MCP call may request |
| `maximum_queue_items` | `1..1024` | Waiting items; the currently playing item is not counted |
| `maximum_audio_seconds` | Non-negative integer | Decoded-duration and runtime limit; omitted or `0` means unlimited |

Gain is normalized: `0.0` is silent and `1.0` is the backend's unamplified level. It is not a master system-volume control.

Audio file size is not capped. Duration is profile-controlled through `maximum_audio_seconds` and defaults to unlimited (`0`) in the quick and generated profiles. A positive value is advertised by `get_audio_capabilities` and enforced during decoder preflight and again during playback.

### Output targets

Agents select only friendly target aliases. Raw device IDs never appear in MCP capabilities or tool errors.

```toml
[outputs]
default_target = "system"

[[outputs.targets]]
id = "system"
description = "Whichever Windows output is current when playback begins"
kind = "system_default"
allow = ["audio", "speech"]

[[outputs.targets]]
id = "private-headset"
description = "Private headset; prefer for spoken or sensitive notifications"
kind = "device"
device_id = "wasapi:{0.0.0.00000000}.{replace-with-device-guid}"
allow = ["audio", "speech"]

[[outputs.targets]]
id = "desk-speakers"
description = "Desk speakers; use only for non-sensitive chimes"
kind = "device"
device_id = "wasapi:{0.0.0.00000000}.{replace-with-device-guid}"
allow = ["audio"]
```

| Field | Requirement |
| --- | --- |
| `default_target` | Must name one configured target; used when an MCP call omits `output_target` |
| `targets` | At least one target with a unique ID |
| `id` | Friendly alias matching `[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}` |
| `description` | Model-visible routing guidance, at most 1,000 Unicode characters |
| `kind` | `system_default` or `device` |
| `device_id` | Required only for `device`; copy it from `agent-speak devices` |
| `allow` | Unique subset of `audio` and `speech` |

`system_default` is resolved immediately before each item starts. Changing the Windows default does not migrate active playback, but the next queued item follows the new default. A `device` target opens exactly its configured endpoint. If it is missing, disconnected, or fails, the job reports `output_unavailable`; Agent Speak never silently reroutes it elsewhere.

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

History is opt-in and non-blocking. It may contain timestamps, playback IDs, preset IDs, gain, concurrency, the friendly output target alias, state, error codes, and—only when explicitly enabled—arbitrary spoken text. Protect the history file according to the sensitivity of your workflow.

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

Capability output and errors do not reveal preset source paths, preset speech text, history paths, or raw device IDs.

Every playback tool accepts optional `gain`, `concurrency`, and `output_target` fields. The output target must be a visible startup-approved alias whose `allow` list includes the requested source kind. Omission uses `outputs.default_target`; unknown or disallowed aliases are rejected before queue acceptance.

## Playback semantics

Calls are fire-and-forget: success means the bounded playback actor accepted the job, not that playback finished or was audible. One item plays at a time.

- `enqueue` appends behind the active item in FIFO order.
- `interrupt` stops the active item, starts the replacement, and preserves items already waiting in the queue.

Supported files are WAV, MP3, FLAC, and Ogg Vorbis. Every file—including a preset—is content-sniffed and decoder-preflighted instead of trusted by extension. Arbitrary paths must be absolute. Agent Speak opens a file once, verifies that the opened Windows object is local rather than a UNC share or device path, and retains that exact handle through preflight and playback. The object must be a regular file. When `maximum_audio_seconds` is nonzero, the configured duration limit is checked during preflight and enforced again at runtime. Replacing a path or link after it is opened cannot redirect later decoding to another object.

On Windows, speech is synthesized to a bounded in-memory WAV and sent through the same Rodio/CPAL renderer as file audio. This gives speech and files identical output selection, gain, completion, stop, and device-loss behavior. If a live stream fails, the active job fails once, its player is stopped so the queue cannot hang, and a later item attempts to reopen only its selected endpoint.

## Security model

Arbitrary TTS is high trust: an agent may speak any text within the configured limit. Arbitrary local audio is a separate, broad permission: it lets the agent ask Agent Speak to read any local regular file available to the server account. Errors can reveal whether a path exists or resembles supported media, and a readable audio file's contents can be played aloud. It does not accept URLs, UNC shares, device paths, directories, or pipes.

All media bytes are untrusted, including files named by presets. The server sniffs content, preflights the decoder, applies a configured duration cap when enabled, uses only the enabled Rust decoder implementations, and never invokes a shell, external media player, URL, or file association. File size and, by default, playback duration are uncapped, so a permitted caller can cause extended disk I/O or playback. Malformed-media and dependency-audit tests reduce exposure to parser bugs.

Those controls are hardening, not a sandbox or a proof that a decoder has no vulnerability. Decoding occurs in this process. Also, Windows may contact a remote filesystem while resolving a pre-existing reparse point before Agent Speak can inspect and reject the opened remote target. If your threat model requires containment of a novel decoder exploit or strict network isolation, do not enable file playback unless Agent Speak itself runs under a suitably restricted Windows account or OS sandbox. A directory allowlist would not provide decoder containment because an agent able to write there could place the same malicious bytes inside it.

In serve mode, diagnostics go only to stderr because stdout is reserved for MCP JSON-RPC. Host-level approval and trust behavior is controlled by the MCP host; Agent Speak itself performs no per-request prompting.

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
