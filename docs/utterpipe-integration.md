# Agent Speak UtterPipe v1 integration

- Status: implemented host contract
- Agent Speak profile schema: `1`
- UtterPipe protocol: `utterpipe.tts/1` over `UTP1` framing
- Utterance schema profile: `utterpipe.utterance-options/1`

The [UtterPipe v1 specification](https://github.com/4piu/utterpipe/blob/master/docs/SPEC.md)
is normative for the wire protocol. This document records Agent Speak's host
policy and implementation boundary. Provider repositories separately specify
their engine-specific options, catalogs, assets, and upstream behavior.

## Responsibilities

Agent Speak owns:

- MCP caller permissions and tool schemas;
- text, queue, gain, output, duration, and decoded-audio policy;
- provider discovery, process lifecycle, deadlines, and cleanup;
- UtterPipe framing, negotiation, request correlation, and audio validation;
- decoding, bounded prebuffering, playback, interruption, and history;
- the allowlist deciding which per-utterance controls an agent may set.

The provider owns:

- all engine-specific option names and meanings;
- engine/service initialization and synthesis;
- provider catalogs, assets, licenses, preparation, import, and removal;
- data-store locking, runtime leases, and atomic activation;
- upstream network, retention, billing, and license disclosures.

Agent Speak never bundles, installs, updates, or registers a provider. It does
not define universal model, voice, language, style, emotion, or sample-rate
fields.

## Configuration

System TTS keeps its provider-specific voice field under provider options:

```toml
[tts]
enabled = true
provider = "system"
maximum_characters = 500

[tts.provider_options]
voice_id = ""
```

An external provider uses its executable name as `provider`. Engine-specific
settings are separated by lifetime:

```toml
[tts]
enabled = true
provider = "utterpipe-openai-http"
maximum_characters = 500
provider_environment = []
agent_utterance_options = ["instructions", "speed"]
audio_deliveries = [
  { mode = "incremental", format = "audio/ogg;codecs=opus" },
  { mode = "complete", format = "audio/wav;codec=pcm_s16le" },
]

[tts.provider_options]
base_url = "https://api.openai.com/v1"
transport = "remote_https"
api_key = "replace-with-service-api-key"
model = "gpt-4o-mini-tts"

[tts.utterance_options]
voice = "coral"
speed = 1.0
```

Rules:

- `provider` is `system` or `utterpipe-<slug>` using the UtterPipe slug grammar.
- Provider-specific model, voice, and selection fields belong in
  `provider_options` or `utterance_options`; Agent Speak defines no universal
  model or voice field.
- `provider_options` accepts TOML values representable as bounded JSON and
  rejects date/time values and unsafe integers. Agent Speak transports the
  object opaquely; the provider validates it authoritatively.
- `utterance_options` accepts the same JSON-compatible TOML values, must satisfy
  the resolved request schema at startup, and is sent on each synthesis.
- `audio_deliveries` is an optional ordered list of unique exact pairs Agent
  Speak can decode; omission enables automatic host preference.
- `provider_environment` contains at most 32 unique names matching
  `[A-Za-z_][A-Za-z0-9_]{0,127}`.
- `agent_utterance_options` contains at most 64 unique names matching
  `[a-z][a-z0-9_]{0,63}`. Names are checked against the exact resolved schema at
  startup.
- Fixed options never become MCP parameters. Agent-authorized values overlay
  configured utterance defaults for one request and do not mutate fixed or
  persistent state.

On Linux, the built-in default configuration selects `utterpipe-espeak-ng` and
puts the chosen `--voice-id` value in `utterance_options.voice`. Windows and
macOS built-in defaults continue to use system TTS.

## Discovery and child environment

For `<slug>`, Agent Speak derives exactly `utterpipe-<slug>` or
`utterpipe-<slug>.exe`. It checks the resolved Agent Speak executable directory,
then absolute nonempty `PATH` entries in order. It ignores the current working
directory, relative/empty entries, shell wrappers, and Windows extension
expansion. The selected regular executable is canonicalized and spawned
directly as:

```text
utterpipe-<slug> protocol --stdio
```

The child environment is cleared. Agent Speak copies present baseline variables
`HOME`, `USERPROFILE`, `LOCALAPPDATA`, `TMPDIR`, `TEMP`, `TMP`, `LANG`, `LC_ALL`,
`SSL_CERT_FILE`, `SSL_CERT_DIR`, `SYSTEMROOT`, and `WINDIR`, plus every required
name in `provider_environment`. A missing configured value fails startup.

The reported slug is checked, but this prevents accidental mismatch rather than
authenticating the binary. A selected provider is trusted native code running
with the user's privileges.

## Storage

Agent Speak creates private, canonical, distinct, non-nested data and cache
roots before runtime or management initialization. Inspect sessions create
nothing.

| Platform | Data | Cache |
| --- | --- | --- |
| Windows | `%LOCALAPPDATA%\UtterPipe\providers\<slug>\data` | `%LOCALAPPDATA%\UtterPipe\providers\<slug>\cache` |
| macOS | `~/Library/Application Support/UtterPipe/providers/<slug>/data` | `~/Library/Caches/UtterPipe/providers/<slug>` |
| Linux | `${XDG_DATA_HOME:-~/.local/share}/utterpipe/providers/<slug>` | `${XDG_CACHE_HOME:-~/.cache}/utterpipe/providers/<slug>` |

Agent Speak does not construct provider artifact paths or take a global store
lock. Multiple hosts may initialize separate provider processes against the
same roots; providers own safe sharing, mutation locks, leases, and cache
publication.

## Handshake and initialization

Every process begins with `protocol.hello`, offering only protocol major 1 and
`utterpipe.utterance-options/1`. Agent Speak verifies:

- protocol, framing, provider identity, and SemVer;
- capability promises and generic catalog/import descriptors;
- bounded closed fixed-configuration schemas;
- a registered complete delivery and all known exact audio pairs;
- forward-compatible unknown capabilities and well-formed future audio pairs.

Runtime initialization sends the opaque `provider_options`, paths, effective
limits, and an ordered intersection of pairs both sides advertised. A configured
`audio_deliveries` list supplies the host order; otherwise Agent Speak prefers
incremental Ogg Opus, MP3, then raw PCM16, followed by complete Ogg Opus, MP3,
and PCM16 WAV. It does not offer AAC or FLAC because its playback path does not
decode them. A provider returns a nonempty order-preserving usable subset.

The successful runtime result atomically returns that set plus a resolved
utterance-options schema and RFC 8785/SHA-256 digest. Agent Speak validates the
restricted schema profile, all annotations/defaults/examples, the digest, and
disjoint top-level fixed/request option names. It also validates configured
utterance defaults. The schema and audio set are immutable for the live MCP
server.

Management initialization sends paths and provider options only. It performs no
audio negotiation and accepts partial configuration according to the provider's
management schema. Inspect performs hello and shutdown without initialization.

## Agent option authority

At `serve` startup Agent Speak projects only configured
`agent_utterance_options` properties into the optional
`speak_text.utterance_options` MCP object. Provider-supplied descriptions,
bounds, enum choices, units, defaults, and behavioral notes tell the agent how
each granted control behaves. No catalog text is used as tool instruction.

Agent Speak independently checks every request against the frozen projected
schema before queue acceptance. It rejects unknown names, invalid types,
out-of-range values, null, excessive depth, or an object above 65,536 bytes.
The worker overlays accepted values onto configured `utterance_options` and
validates the effective object against the full resolved schema. The provider
validates it again before inference, network, billing, asset loading, or audio
output.

Speech cues use only configured utterance defaults. System TTS exposes no
provider option namespace.

## Runtime and recovery

`serve` starts one provider worker and keeps its process warm for sequential
synthesis. The playback actor sends text, frozen per-request options, gain,
output target, and completion notification to that worker without blocking
queue control.

Only one synthesis is active. During work, interruption or shutdown remains
responsive. Agent Speak never retries an active or partly audible utterance. If
the process crashes, the active job fails; before later speech, the worker may
start one clean replacement. The replacement must return the exact startup
audio delivery set, schema, digest, and configured defaults or speech fails closed. A second failure
leaves provider TTS unavailable.

Normal shutdown sends `session.shutdown`, closes stdin, waits three seconds,
then terminates the dedicated Unix process group or Windows kill-on-close Job
Object and reaps the child. Cancellation has a one-second grace; unsupported or
stuck cancellation terminates the process.

## Audio and backpressure

Agent Speak enforces a 256 MiB cumulative transport ceiling and sets the
reader's pre-allocation limit before each request: 1 MiB per incremental frame
or the full bounded complete-result ceiling.

- Complete PCM16 WAV is container-validated and decoder-preflighted before
  playback. Complete MP3/Ogg Opus is decoded through the same bounded path used
  for encoded streaming.
- Incremental raw PCM validates sample metadata, channel alignment, frame and
  cumulative counts, and configured duration before enqueueing samples.
- Incremental MP3/Ogg Opus treats UtterPipe chunks as arbitrary transport
  boundaries and validates the actual encoded stream while decoding.
- Playback starts at a 200 ms target prebuffer, or after successful generation
  for shorter audio. At most two seconds of decoded samples and four bounded
  encoded/decoded handoff items are queued; pipe backpressure slows the
  producer.
- Provider generation completion and audible playback completion remain
  distinct. Queue acceptance never claims the speech was heard.

For every synthesis Agent Speak selects an exact member of the initialized set,
includes it in `synthesis.start`, and validates response format and metadata
against that request. Output-device adaptation happens only after decoding; a
provider or utterance option cannot switch the wire format.

## Generic management

The host exposes these explicit human commands:

```text
agent-speak provider info [--config <PATH>]
agent-speak provider catalog [--config <PATH>] --catalog <ID> \
  [--scope installed|available|all] [--refresh]
agent-speak prepare [--config <PATH>] \
  [--yes] [--accept-license <ID> ...]
agent-speak provider import [--config <PATH>] --kind <KIND> \
  --source <ABSOLUTE_FILE> --id <ASSET_ID> --consent-confirmed
agent-speak provider remove [--config <PATH>] \
  [--artifact <ID> ...] [--purge] [--yes]
```

Omitting `--config` uses the ordinary layered discovery policy; supplying it
selects one complete, unmerged profile.

Catalog and import IDs are opaque provider values. Catalog patches are bounded,
contain no null deletion, and may touch only provider-declared non-secret
top-level options. Agent Speak prints proposals but never applies them to the
profile. Refresh is network-enabled only when explicitly requested.

Prepare and remove use same-process plan/apply. Ordinary confirmation and each
required license acceptance are separate. Import canonicalizes a user-approved
absolute regular file, checks the provider's declared size limit, and requires
explicit consent. None of these operations runs during MCP startup or calls.

## Verification gates

Automated tests cover strict config variants, process cleanup, framing and JSON
bounds, exact pair negotiation, schema/digest projection, MCP enforcement,
complete/incremental PCM and encoded decoding, cancellation, restart identity,
generic catalogs, patches, and stdio lifecycle.

Before a release, run the full Rust suite on Windows, macOS, and Linux; run the
UtterPipe conformance runner against each provider; and exercise real output on
the default and a Bluetooth device when practical. Remote providers additionally
need a mock for failures/timeouts plus an explicitly funded, scoped real API
smoke test.
