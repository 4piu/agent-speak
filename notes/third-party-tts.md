# Third-party TTS decision record

Status: accepted design; Pocket feasibility gate passed
Updated: 2026-08-07

This note records why Agent Speak will support external TTS providers and the
product decisions already made. It is intentionally not the wire protocol or an
implementation plan. The host-neutral protocol specification and each
provider's implementation specification live separately and are authoritative
for technical details.

## Goal

Give users access to natural local or remote voices without putting an inference
engine, model, Python environment, engine-specific API, or incompatible license
inside Agent Speak.

Agent Speak remains a small policy and playback application. Providers generate
audio; Agent Speak validates and plays it through its existing Rodio path.

## Settled decisions

### One host-neutral provider boundary

- Agent Speak supports built-in system TTS plus one versioned external-provider
  protocol. It does not add engine-specific integrations to the core.
- The reusable external unit is called a **provider process**, not an Agent
  Speak plugin. Agent Speak is the first host, not part of provider identity.
- **UtterPipe** is the working project and executable namespace until final name
  clearance. Example executables are `utterpipe-pocket-tts` and
  `utterpipe-openai-http`.
- The wire contract is language-neutral and permissively licensed. Passing the
  conformance suite, rather than linking a particular SDK, defines compatibility.

### Provider form

- A provider is one directly runnable, self-describing executable wherever
  practical. It may embed its adapter and inference runtime, but model and voice
  assets normally remain separate.
- A provider may internally use native inference, bridge an existing HTTP
  service, or manage a private runtime. Those choices belong in that provider's
  own specification and repository.
- Providers are released independently. One provider's runtime dependencies,
  license, or update schedule do not affect Agent Speak or another provider.
- Dynamic libraries are not the public extension mechanism: Rust has no stable
  plugin ABI, an in-process library can crash the host, and dependency/license
  isolation becomes weaker.
- Arbitrary shell-command templates are not supported. Spoken text never appears
  in command arguments or shell interpolation.

### Config-directed discovery, without registration

- There is no provider registry and no registration command.
- Agent Speak resolves only the portable executable name selected in
  `config.toml`. For `provider = "utterpipe-pocket-tts"`, it searches for
  exactly `utterpipe-pocket-tts` (`.exe` on Windows).
- Lookup checks the directory containing the resolved Agent Speak executable,
  then safe absolute entries in `PATH`, in order.
- Empty or relative `PATH` entries, the current working directory, shell
  expansion, and Windows script extensions are ignored. The selected target is
  canonicalized, checked as a regular executable, and spawned without a shell.
- A side-effect-free handshake must report the configured slug and a compatible
  protocol version. `agent-speak validate` shows the resolved path, provider
  identity, and version.
- Selecting a provider in a trusted config authorizes native code execution.
  Process isolation protects core reliability; it is not an account sandbox.

### One configuration file

- Agent Speak's `config.toml` remains the only configuration source.
- It contains the provider executable selector, model ID, voice ID, Agent Speak
  policy, environment-name allowlist, and a provider-options table validated by
  the provider.
- Provider options are the startup-fixed extension point for engine-specific
  controls such as speed, pitch, sampling parameters, inference steps, threads,
  or an engine-supported output sample rate. Agent Speak transports them but
  does not assign meaning or expose them to MCP calls.
- Providers do not maintain a second mutable settings file or named connection
  profiles. Credentials are referenced through environment variables or an OS
  credential facility and are never printed.
- Provider-owned model indexes, license records, downloaded models, imported
  voice presets, embeddings, and caches are operational data, not configuration.

Agent Speak schema-1 configuration:

```toml
schema_version = 1

[tts]
enabled = true
provider = "utterpipe-pocket-tts"
maximum_characters = 500

[tts.provider_options]
model_id = "pocket-tts-int8-2026-01-26"
voice_id = "my-voice"
num_threads = 2
```

### Explicit preparation

Installation, configuration, validation, preparation, and serving remain
separate phases:

1. The user or package manager installs the provider executable.
2. The user selects it and its model/voice in `config.toml`.
3. `agent-speak validate` performs read-only discovery and readiness checks.
4. `agent-speak prepare --config ...` asks the provider for a structured plan,
   presents downloads and licenses, obtains any required human confirmation,
   and delegates installation.
5. `agent-speak serve` validates and starts only already-prepared state.

Plain `serve` never downloads, upgrades, accepts a license, or prompts—even on
first use. MCP hosts commonly launch it unattended with stdin/stdout reserved
for MCP. A missing model or voice produces a fast, actionable error pointing to
`prepare`.

The provider owns catalogs, checksums, downloads, extraction, atomic activation,
offline imports, and data migrations. The core owns consistent presentation,
confirmation, process limits, and policy.

### Shared provider data

- Providers use neutral per-user data/cache roots such as
  `UtterPipe/providers/<provider-slug>`, rather than an Agent Speak directory.
  Multiple future hosts can reuse one installed model or voice pack.
- The host resolves and passes absolute data/cache paths during initialization.
  A provider's direct CLI derives the same documented defaults or accepts an
  explicit override. Persistent data never depends on the current directory.
- Shared state requires interprocess install locks, temporary version
  directories, checksum verification, atomic activation, immutable active
  versions, and explicit cleanup.
- Separate provider processes must support concurrent runtime use of the same
  immutable active assets. Mutations are single-writer; removal and incompatible
  migration refuse leased versions. This is multi-process safety, not a shared
  daemon or concurrent synthesis inside one provider process.
- A host may deliberately supply an isolated root. No configuration is stored in
  either the shared or isolated provider data root.

### One cross-platform process protocol

- A host owns one long-lived provider child per runtime session so models stay
  warm. Short-lived management sessions use the same executable and framing but
  expose administrative operations separately.
- Agent Speak owns process lifetime: `serve` starts, reuses, shuts down, and
  reaps its runtime child; each management command starts and reaps a separate
  child. Providers do not outlive their host as idle daemons.
- Communication uses inherited binary stdin/stdout pipes on Windows, macOS, and
  Linux. No listening port, daemon, Unix socket, Windows named pipe, or
  platform-specific public IPC contract is needed.
- Bounded framed control messages and binary audio travel over those pipes.
  Provider stdout contains protocol frames only; bounded, redacted diagnostics
  go to stderr.
- Version 1 requires **complete delivery** as the universal baseline: one
  complete, bounded PCM16 WAV after synthesis finishes. It optionally negotiates
  **incremental delivery**, where PCM16 samples arrive while synthesis is still
  running.
- These names describe when provider output becomes available. Either process
  may use bounded memory buffers, and Agent Speak may retain a small
  **prebuffer** before playing incremental audio to reduce underruns.
- Runtime sessions support initialization, capabilities, health, synthesis,
  optional cancellation, and shutdown. Management sessions support read-only
  inspection plus optional catalog, prepare, import, update, and removal
  operations.
- Only one synthesis is active per provider process in version 1. A provider
  must continue reading control input during synthesis or management work so
  shutdown remains possible. Disallowed concurrent methods return `busy`; they
  are not accepted in arbitrary states.
- The host validates framing, byte counts, audio signature/decoder output,
  duration, deadlines, stderr size, exit behavior, and child-process cleanup.

Synthesized speech is never exchanged through temporary files. Models and voice
packs necessarily are persistent files; the provider downloads and writes them
directly rather than relaying multi-gigabyte assets through the host. An explicit
offline model import or reference-voice import may use a user-approved path, but
arbitrary paths are never synthesis parameters.

### Playback and policy ownership

- Providers generate audio and never receive an output-device identifier or
  play audio themselves.
- Agent Speak owns output routing, gain, queueing, interruption, decoder
  preflight, playback completion, history policy, and model-visible capability
  projection.
- Agents cannot choose executable paths, provider endpoints, model paths,
  reference recordings, arbitrary voices, styles, API keys, or provider options
  per call. They may use only the policy selected at startup.
- Agent Speak decoder-preflights complete audio before Rodio playback. For
  incremental audio it validates and queues bounded PCM chunks, starts after a
  small prebuffer, and continues receiving while Rodio plays. Provider synthesis
  must run away from the playback actor so a slow inference call cannot prevent
  interruption.

### Management ownership

The provider executable has both a direct human CLI and a machine protocol. The
core is the preferred front end but never scrapes human-readable output.

```text
utterpipe-pocket-tts info
utterpipe-pocket-tts doctor
utterpipe-pocket-tts models list
utterpipe-pocket-tts models prepare
utterpipe-pocket-tts voices list
utterpipe-pocket-tts voices import reference.wav --id my-voice --consent-confirmed
utterpipe-pocket-tts protocol --stdio

agent-speak validate --config config.toml
agent-speak voices --config config.toml
agent-speak prepare --config config.toml
agent-speak serve --config config.toml
```

Provider executables are updated by the user or package manager. Models and
voices are changed only through explicit provider management. Removing a
provider from config disables it without deleting assets; code removal and data
purging are separate explicit operations.

## Repository boundaries

Use separate local and remote repositories:

1. **Protocol repository**: normative wire specification, schemas, security
   rules, conformance runner, deterministic fake provider, and tiny reference
   host. It contains no real inference engine.
2. **One repository per published provider**: its implementation specification,
   code, upstream/version policy, license notices, packaging, tests, and release
   artifacts.
3. **Agent Speak repository**: only the host integration, config schema, process
   supervision, synthesis/playback seam, and end-to-end tests.
4. **Optional provider index**: documentation and package links only. It is not
   contacted during discovery or runtime.

The protocol has its own major version. Agent Speak and every provider have
independent product versions. Provider executable names do not contain a version.

## Provider shortlist

| Provider | Intended role | Current judgment |
| --- | --- | --- |
| OpenAI-compatible HTTP | First bridge/conformance provider | Small reusable executable; proves remote/loopback transport, credentials, bounded audio, and cancellation, but voice discovery needs a configured manifest |
| Pocket TTS | Selected first engine-specific provider | The pinned sherpa-onnx Rust/static path passed the Apple Silicon packaging, true-incremental, cancellation, and audio smoke gates; the first converted artifact is English-only and its separate terms are handled conservatively |
| Piper | Traditional reference provider | Excellent model/voice lifecycle and HTTP contract; efficient and mature, but the maintained engine is GPL-3.0 and voice licenses vary |
| sherpa-onnx | Possible implementation runtime or its own multi-model provider | Broad cross-platform native runtime with Rust/C APIs and many TTS families; useful but substantially broadens one provider's scope |
| Supertonic 3 | Compatibility experiment, no longer preferred first provider | Attractive 99M ONNX model and 44.1 kHz output, but upstream announced repository archival and no further official support on 2026-07-23 |
| KittenTTS | Later tiny provider | Very small ONNX models and simple voices, but still a developer preview |
| Chatterbox, Qwen3-TTS, CosyVoice | Later high-quality sidecars | Larger Python/GPU-oriented stacks and cloning semantics; useful only after the basic lifecycle is proven |

A shortlist entry is research, not a commitment to publish it. A provider gets
its own repository and accepted implementation specification only when selected
for implementation; there is no generic specification that silently chooses a
technology stack for several engines.

Relevant upstream sources:

- [Pocket TTS](https://github.com/kyutai-labs/pocket-tts)
- [Piper](https://github.com/OHF-Voice/piper1-gpl)
- [sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx)
- [Supertonic archival notice](https://github.com/supertone-inc/supertonic)
- [KittenTTS](https://github.com/KittenML/KittenTTS)

Model quality, latency, resource use, license suitability, and packaging must be
tested from exact pinned artifacts. A framework license never substitutes for
the model or voice license.

## Implementation order after specifications are accepted

1. Publish Protocol v1 and its conformance fixtures locally.
2. Refactor Agent Speak around a provider-neutral synthesis result while keeping
   all existing system backends working.
3. Implement discovery, supervision, validation, preparation, config schema v2,
   and the fake-provider end-to-end path.
4. Implement the OpenAI-compatible HTTP provider.
5. Implement one engine-specific provider with complete model/voice lifecycle.
6. Test Windows, macOS Intel/Apple Silicon, and Linux behavior, including crash,
   malformed frame, oversize output, timeout, cancellation, missing assets,
   interrupted installs, and concurrent shared-store access.
7. Only then add further providers.

## Accepted defaults and feasibility result

On 2026-08-06 the project accepted:

1. **UtterPipe** as the provisional local namespace pending public name
   clearance;
2. Apache-2.0 for the protocol and permissive first-party providers;
3. mandatory complete PCM16 WAV delivery plus optional incremental PCM16
   delivery in Protocol v1;
4. neutral shared per-user provider data by default;
5. Pocket TTS as the first engine-specific provider;
6. a pinned sherpa-onnx-backed native executable as the preferred Pocket
   implementation direction.

The bounded Pocket spike passed on macOS arm64 with sherpa-onnx `v1.13.4`: a
single native executable produced genuine incremental 24 kHz PCM, cooperatively
cancelled from the engine callback, ran faster than real time on the test Mac,
and played successfully through the selected system output. The provider
specification pins the tested native/model hashes.

The converted model archive combines CC-BY-4.0 material and upstream acceptable
use terms with an explicit non-commercial notice. The provider therefore keeps
the model separate, requires all disclosures to be accepted during explicit
preparation, and does not install the archive's sample WAVs as voices. Version
0.1 imports a user-approved reference WAV with explicit consent instead.
