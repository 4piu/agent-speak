# Provider configuration

This is the user-facing reference for selecting an independently installed
[UtterPipe](https://github.com/4piu/utterpipe) TTS provider in Agent Speak. For
the wire contract and host implementation details, see
[the integration specification](utterpipe-integration.md).

## Select and discover a provider

Put the provider's portable executable name, without Windows' `.exe`, in
`backend`. There is no registry and no separate provider field:

```toml
# Selects the provider and host-side synthesis policy.
[tts]
enabled = true
backend = "utterpipe-openai-http"
maximum_characters = 500
agent_utterance_options = ["instructions", "speed"]
audio_deliveries = [
  { mode = "incremental", format = "audio/ogg;codecs=opus" },
  { mode = "complete", format = "audio/wav;codec=pcm_s16le" },
]

# Fixes endpoint and model settings for the provider process.
[tts.provider_options]
base_url = "https://api.openai.com/v1"
transport = "remote_https"
api_key = "replace-with-service-api-key"
model = "gpt-4o-mini-tts"

# Sets per-request defaults that authorized agent values may override.
[tts.utterance_options]
voice = "coral"
speed = 1.0
instructions = "Speak clearly and naturally."
```

For `backend = "utterpipe-<slug>"`, Agent Speak checks:

1. the directory containing the running `agent-speak` executable;
2. each absolute, nonempty `PATH` directory, in order.

The first exact executable match is canonicalized and used. Discovery never
scans, registers, or persists provider paths. Agent Speak launches
`utterpipe-<slug> protocol --stdio` directly and requires the returned provider
slug and protocol identity to match.

Selecting a provider authorizes that native executable to run with your user
privileges. Slug matching catches mistakes; it is not binary authentication.

## Fixed provider options

`provider_options` is the engine-specific TOML table fixed at provider
initialization. Settings that build or warm expensive state, plus endpoint,
credential, thread, and cache choices, belong here—not directly under `[tts]`.

Agent Speak converts TOML strings, booleans, finite numbers, arrays, and tables
to JSON without assigning meaning to their keys. Date/time values and integers
outside UtterPipe's exact JSON range are rejected. The provider performs
authoritative validation during initialization, and the object remains fixed
for that provider process.

Options may contain credentials. Agent Speak sends them through the private
provider pipe, but the profile stores them in plaintext. Protect a
credential-bearing profile with operating-system file permissions and never
commit it to source control. Agent Speak does not print provider option values.

The exact available fixed options belong to each provider:

- [eSpeak NG options](https://github.com/4piu/utterpipe-espeak-ng#agent-speak-configuration)
- [Pocket TTS options](https://github.com/4piu/utterpipe-pocket-tts#provider-options)
- [OpenAI-compatible HTTP options](https://github.com/4piu/utterpipe-openai-http#configuration)

## Configured utterance defaults

`utterance_options` is a second provider-defined TOML table, but Agent Speak
sends it on every synthesis request rather than initialization. It is suitable
for inexpensive choices such as voice, speed, pitch, style, or seed when the
provider declares those controls per-request. The provider's resolved schema
is authoritative, and Agent Speak rejects invalid configured defaults during
`serve` startup.

A provider option name and utterance option name may not overlap. This keeps
one lifecycle authority for each engine setting. Catalog results likewise use
separate fixed and utterance patches.

## Agent-controlled utterance options

`agent_utterance_options` grants the agent permission to set selected provider
controls on an individual `speak_text` call:

```toml
# Selects the provider and host-side synthesis policy.
[tts]
enabled = true
backend = "utterpipe-espeak-ng"
maximum_characters = 300
agent_utterance_options = ["rate_wpm", "pitch"]

# Sets per-request defaults that authorized agent values may override.
[tts.utterance_options]
voice = "default"
rate_wpm = 175
pitch = 50
```

At `serve` startup, Agent Speak initializes the provider and verifies its
resolved utterance-options schema and digest. Every allowlisted name must exist.
Agent Speak copies only those property schemas into the optional
`speak_text.utterance_options` MCP field, including provider-supplied titles,
descriptions, bounds, enums, defaults, units, and behavioral guidance.

The allowlist grants the full currently advertised domain for each name. Agent
Speak validates submitted values against that startup schema before queueing
speech, overlays them onto configured `utterance_options`, then relays the
result unchanged. A supplied top-level value replaces that configured default
for one request only. It cannot
persist settings, install assets, change credentials/endpoints, or alter the
negotiated audio format.

An empty or omitted allowlist exposes no provider controls to the agent.
Updating a trusted provider may change its schema on the next `serve` run. A
provider restarted within one live run must return the exact startup schema and
audio delivery set or Agent Speak fails closed.

## Inspect catalogs and manage assets

Provider resources are generic. A provider may call a catalog `voices` or
`models`, but Agent Speak assigns no special meaning to those IDs.

```text
agent-speak provider info
agent-speak provider catalog --catalog voices
agent-speak provider catalog --catalog voices --scope available --refresh
agent-speak prepare
agent-speak provider import --kind voice --source /absolute/reference.wav --id my-voice --consent-confirmed
agent-speak provider remove --artifact voice:my-voice
```

These examples use the normal system, user, and working-directory discovery
layers. Add `--config ./agent-speak.toml` to any command to select one complete,
unmerged profile instead.

`provider info` shows the provider-declared catalog IDs and import kinds.
Catalog reads are local unless the explicit human command includes `--refresh`.
Catalog results may propose separately constrained `provider_options` and
`utterance_options` patches, but Agent Speak does not silently apply or persist
either one.

Preparation and removal use a displayed same-process plan followed by explicit
confirmation. `--yes` skips ordinary confirmation but never accepts a license;
each required license needs its own `--accept-license <ID>`. Imports require an
absolute regular file, a declared import kind, and `--consent-confirmed`.
Management is never triggered by `serve` startup or an MCP tool call.

## Provider environment

`provider_environment` is available only for a provider that explicitly
documents an environment-variable requirement. It contains names, not values:

```toml
[tts]
provider_environment = ["LOCAL_ENGINE_TOKEN"]
```

Agent Speak clears the child environment, then copies these baseline variables
when present:

```text
HOME USERPROFILE LOCALAPPDATA TMPDIR TEMP TMP LANG LC_ALL
SSL_CERT_FILE SSL_CERT_DIR SYSTEMROOT WINDIR
```

Finally, it copies each configured name from the process that launched Agent
Speak. A missing configured variable is an error. This reduces accidental
exposure but is not a sandbox. Agent Speak does not load `.env` files. The
OpenAI-compatible HTTP provider's optional `api_key` is a normal provider option
and does not require this mechanism.

## Storage, lifecycle, and audio

Agent Speak creates distinct provider-specific data and cache roots:

| Platform | Data | Cache |
| --- | --- | --- |
| Windows | `%LOCALAPPDATA%\UtterPipe\providers\<slug>\data` | `%LOCALAPPDATA%\UtterPipe\providers\<slug>\cache` |
| macOS | `~/Library/Application Support/UtterPipe/providers/<slug>/data` | `~/Library/Caches/UtterPipe/providers/<slug>` |
| Linux | `${XDG_DATA_HOME:-~/.local/share}/utterpipe/providers/<slug>` | `${XDG_CACHE_HOME:-~/.cache}/utterpipe/providers/<slug>` |

`serve` starts one runtime process and reuses it for sequential speech. Agent
Speak shuts it down with the server and permits one clean restart after an
unexpected crash. Inspect and management commands use separate short-lived
processes. Providers must therefore support multiple processes sharing the
same assets while owning their locks, leases, and atomic updates.

Agent Speak offers exact complete or incremental delivery pairs it can consume:
PCM16 WAV, raw PCM16, MP3, and Ogg Opus. Optional `tts.audio_deliveries` narrows
and orders those pairs; omission uses Agent Speak's automatic preference order.
Initialization returns the usable subset, and Agent Speak selects one exact
member on every synthesis request. Providers may advertise AAC, FLAC, or future
registered formats for other UtterPipe hosts; Agent Speak simply does not offer
formats it cannot decode.
