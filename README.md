# Agent Speak

Agent Speak is a local, Windows-first MCP server that gives an agent
user-controlled access to system text-to-speech and audio playback. The server
does not define event types: you decide what sounds or phrases mean in your
workflow and tell the agent when to use them.

This repository currently contains an early MVP implementation. Its automated
queue, policy, media-preflight, and MCP contract tests pass; audible behavior
still needs real-host and real-speaker acceptance testing before a release.

## Quick start

Build with the stable Rust toolchain on Windows:

```powershell
cargo build --release
```

Run the zero-configuration MCP server:

```powershell
target\release\agent-speak.exe serve
```

The quick profile exposes only:

- `get_audio_capabilities`
- `speak_text`

It uses the default system voice and output, allows `enqueue` and `interrupt`,
and applies conservative built-in gain, text-length, queue, and rate limits.
Use `agent-speak serve --help` to see the small quick-profile override surface.

An MCP host configuration will generally point its command to the absolute
binary path and pass `serve` as the argument. Exact registration and persistent
trust settings are host-specific. Agent Speak never asks for permission per
request: its complete policy is fixed at startup, while any additional approval
prompt belongs to the MCP host.

## File-based profiles

Use a complete TOML policy for presets, arbitrary local audio, history, or
settings outside the quick flags:

```powershell
target\release\agent-speak.exe validate --config examples\text-profile.toml
target\release\agent-speak.exe serve --config examples\text-profile.toml
```

See [examples/text-profile.toml](examples/text-profile.toml) for a safe
text-only example. `--config` cannot be combined with quick-profile flags, so
there is always one authoritative startup policy.

Depending on that policy, the server can expose:

- `list_audio_presets` and `play_audio_preset` for user-authored presets
- `speak_text` for arbitrary system TTS
- `play_audio_source` for local files within canonical approved directories

Disabled tools are absent from MCP discovery. Capability output and errors do
not reveal approved directories, preset source paths, preset speech text, or
history paths.

## Playback behavior

Calls are fire-and-forget: success means the bounded playback actor accepted
the job, not that it finished or was audible. One item plays at a time.
`enqueue` appends to the FIFO; `interrupt` stops the active item before starting
the replacement and preserves already queued items.

Supported file formats are WAV, MP3, FLAC, and Ogg Vorbis. Files are sniffed and
decoder-preflighted instead of trusted by extension. Arbitrary paths must be
absolute, resolve inside an approved directory, identify a regular file, and
fit the configured byte and decoded-duration limits.

## Security boundary

Arbitrary TTS is high trust: an agent may speak any text within the configured
limit. Arbitrary local audio is a separate permission and does not grant
network, shell, or general filesystem access. Agent-generated media remains
untrusted decoder input even inside an approved directory.

The server never invokes a shell, external media player, URL, or file
association. Serve-mode diagnostics go only to stderr; stdout is reserved for
MCP JSON-RPC.

## Development

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

Three hardware/service spikes are ignored by the normal suite. Run them on a
suitable Windows machine with a default audio output (and `ffmpeg` for decoder
fixtures):

```powershell
cargo test spike_ --lib -- --ignored --test-threads=1
```

The product notes, implementation specification, and implementation plan live
under `notes/` in this working tree. That directory is intentionally ignored by
Git. Out-of-MVP ideas are tracked separately in `notes/deferred.md`.
