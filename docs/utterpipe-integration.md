# UtterPipe integration specification

Status: locally implemented; public name and pre-release compatibility provisional
Target Agent Speak profile schema: 1
Target UtterPipe protocol major: 1

This document defines how Agent Speak consumes the host-neutral UtterPipe TTS
Provider Protocol. The UtterPipe protocol repository owns framing and provider
behavior; provider repositories own engine-specific behavior.

## Goals

- Add externally supplied natural TTS without engine-specific core code.
- Preserve Windows/macOS system TTS while moving Linux eSpeak support into a
  reusable provider.
- Keep Agent Speak one small executable with no provider registry.
- Keep `config.toml` as the only configuration file.
- Preserve Agent Speak ownership of policy, output routing, gain, queueing,
  interruption, decoder validation, playback, history, and MCP capabilities.
- Keep provider synthesis away from the playback actor so a slow provider can be
  cancelled without freezing queue control.

## Non-goals

- Bundling or updating providers, runtimes, models, or voices.
- Discovering every executable on `PATH`.
- Selecting providers, endpoints, models, voices, or styles through MCP calls.
- Provider audio formats other than PCM16 WAV, raw PCM16, MP3, and Ogg Opus.
- Bundling a Linux TTS engine or provider into Agent Speak.
- A provider marketplace, shared daemon, or sandbox.

## Configuration schema 1

`agent-speak init` emits schema 1. Validation is strict and rejects unknown or
cross-backend fields.

### System TTS

```toml
schema_version = 1

[tts]
enabled = true
backend = "system"
voice_id = ""
maximum_characters = 500
```

An empty system `voice_id` retains the existing Agent Speak default behavior.
The system backend is supported on Windows and macOS only.

### UtterPipe TTS

```toml
schema_version = 1

[tts]
enabled = true
backend = "utterpipe-openai-http"
model_id = "local-model"
voice_id = "F1"
maximum_characters = 500

[tts.provider_options]
base_url = "http://127.0.0.1:7788/v1"
transport = "loopback_http"
api_key = "replace-with-service-api-key"
```

Rules:

- `backend` is exactly `system` or `utterpipe-<slug>`, where `<slug>` matches
  the UtterPipe slug grammar. The suffix names both the executable and the
  provider identity; no separate registration or `provider` field exists.
- External `model_id` and `voice_id` are nonempty, contain no CR/LF/NUL, and are
  at most 256 Unicode scalar values each.
- `provider_environment` has at most 32 unique environment-variable names,
  using `[A-Za-z_][A-Za-z0-9_]{0,127}`. Values are read from the launching
  environment and never serialized.
- `provider_options` contains only TOML strings, booleans, finite numbers,
  arrays, and tables. Date/time values are rejected. Integers must fit the
  protocol's exact JSON range.
- The `system` variant rejects `model_id`,
  `provider_environment`, and `provider_options`.
- An `utterpipe-*` variant requires all fields except
  `provider_environment`/`provider_options`; an empty options table is valid.
- The provider performs authoritative option validation during `validate`,
  `prepare`, and `serve` initialization.
- `provider_options` is deliberately engine-specific and may hold controls such
  as speed, pitch, sampling parameters, inference steps, thread count, or a
  provider-supported output sample rate. Agent Speak transports this table and
  does not interpret option names. Protocol v1 permits an advertised JSON
  Schema, but Agent Speak v0.1 directs users to provider documentation rather
  than rendering an options UI.
- Provider options are frozen at session initialization and are never MCP
  parameters. Changing one requires restarting `serve` with a changed trusted
  config.
- Agent Speak does not inject an opaque sample-rate option from the selected
  device. Providers report actual sample rate; Rodio/CPAL remains responsible
  for adapting it to the device. A user may configure a documented provider
  sample-rate option when that engine genuinely supports one.

The Rust model should represent the backend as a tagged enum rather than a set
of loosely related optional fields.

## Quick profile

`agent-speak serve` without `--config` uses built-in system TTS on Windows and
macOS. On Linux it uses provider `espeak-ng`, model `espeak-ng`, and voice
`default`; discovery still requires the independently installed
`utterpipe-espeak-ng` executable beside Agent Speak or on `PATH`. Existing
quick-profile flags and policy defaults remain unchanged, and `--voice-id`
selects the active backend's voice. No provider executable, URL, model,
credential, or engine option is added to `serve` flags.

## CLI behavior

### Existing commands

- `agent-speak voices` lists the built-in system API on Windows/macOS. On Linux
  it gives an actionable error because there is no native inventory.
- `agent-speak voices --config <PATH>` resolves the configured UtterPipe
  provider and lists its local/configured voices. It is read-only and
  network-free unless `--refresh` is explicitly supplied.
- `agent-speak validate --config <PATH>` first validates Agent Speak policy,
  then performs provider discovery, hello, management initialization, and
  `provider.validate`. It performs no network access, synthesis, download,
  data conversion, license acceptance, or audio initialization.

The help text must stop calling external-provider validation purely “static,”
because it intentionally executes the configured provider.

### New commands

```text
agent-speak provider info --config <PATH>
agent-speak provider models --config <PATH> \
  [--scope installed|available|all] [--refresh]
agent-speak prepare --config <PATH> \
  [--yes] [--accept-license <ID> ...]
agent-speak provider remove --config <PATH> \
  [--artifact <ID> ...] [--purge] [--yes]
agent-speak provider voices import --config <PATH> \
  --source <ABSOLUTE_WAV> --id <VOICE_ID> --consent-confirmed
```

- `provider info` performs inspect hello and prints canonical executable path,
  provider identity/version, protocol version, and capability flags.
- `provider models` uses a management session. `--refresh` is the explicit
  permission for a catalog network refresh; without it the operation is local.
- `prepare` requests a plan for the config-selected model/voice, presents exact
  actions/sizes/sources/licenses, asks on the controlling terminal, and applies
  only that plan.
- `--yes` suppresses the ordinary action confirmation but does not accept a
  special license. Every required license ID must appear in
  `--accept-license`; the provider verifies the list again.
- If stdin/stdout are not attached to an interactive terminal and confirmation
  is required, `prepare` fails unless the necessary explicit flags were given.
- System TTS reports that no preparation/provider operation is required.
- `provider remove` requests an exact same-session plan and applies it only
  after interactive confirmation or `--yes`. At least one artifact or `--purge`
  is required; the provider retains responsibility for leases and safe removal.
- `provider voices import` performs the explicitly consented `voice.import`
  operation and copies/derives the reference into provider-owned storage.

## Discovery

For provider slug `<slug>`, derive only:

```text
utterpipe-<slug>
utterpipe-<slug>.exe     # Windows
```

Resolution algorithm:

1. Obtain the resolved absolute path of the running Agent Speak executable.
2. Check its containing directory for the exact provider filename.
3. Check each absolute, nonempty `PATH` directory in order.
4. Ignore the current working directory, empty/relative entries, and Windows
   `PATHEXT` expansion. Never accept `.cmd`, `.bat`, `.com`, PowerShell, or shell
   scripts on Windows.
5. Permit a package-manager symlink, canonicalize the final target, and verify
   a regular executable file. On Unix require an executable mode bit. Apply
   useful ownership/signature diagnostics without claiming they establish
   trust.
6. Select the first valid exact match. Do not scan, execute, or inventory other
   provider names.
7. Spawn the canonical path directly with arguments `protocol --stdio`.
8. Require the hello slug to match config exactly. Any mismatch is fatal.

Resolution occurs once per Agent Speak command/process and is never persisted.
Diagnostics show the path selected. A changed `PATH` can select a different
binary on the next run; this is an intentional consequence of registry-free
discovery and must be documented as a trust boundary.

## Provider environment

Agent Speak constructs a deliberate child environment:

- provider variables named by `provider_environment` are copied from the host;
- missing named variables are startup/validation errors;
- the baseline variables `HOME`, `USERPROFILE`, `LOCALAPPDATA`, `TMPDIR`,
  `TEMP`, `TMP`, `LANG`, `LC_ALL`, `SSL_CERT_FILE`, `SSL_CERT_DIR`,
  `SYSTEMROOT`, and `WINDIR` are copied when present;
- unrelated environment variables are cleared;
- the provider executable directory is not added to `PATH`;
- Agent Speak does not interpret provider options to discover hidden environment
  requirements.

Provider specifications list every non-baseline variable they consume. Agent
Speak does not load `.env` files. A provider may instead accept a credential
directly in `provider_options`; in that case it is stored in the trusted profile
and serialized only into the provider initialization frame.

## Shared data and cache paths

Agent Speak passes the neutral per-user defaults:

| Platform | Data | Cache |
| --- | --- | --- |
| Windows | `%LOCALAPPDATA%\UtterPipe\providers\<slug>\data` | `%LOCALAPPDATA%\UtterPipe\providers\<slug>\cache` |
| macOS | `~/Library/Application Support/UtterPipe/providers/<slug>/data` | `~/Library/Caches/UtterPipe/providers/<slug>` |
| Linux | `${XDG_DATA_HOME:-~/.local/share}/utterpipe/providers/<slug>` | `${XDG_CACHE_HOME:-~/.cache}/utterpipe/providers/<slug>` |

Paths are created only for a command/session that needs them. Inspect does not
create directories. Validate and runtime initialization do not mutate
`data_dir`; either may create `cache_dir` only when the provider specification
declares a necessary bounded, reconstructible engine cache. Prepare creates or
mutates operational data under provider management.

Agent Speak performs no model-path construction inside the provider root and
does not delete it implicitly.

Several Agent Speak or other UtterPipe host processes may pass the same roots to
separate provider processes. Agent Speak does not take an application-level
exclusive lock: providers own runtime leases, mutation locks, atomic activation,
and cache publication. It surfaces `resource_busy` with the competing operation
and remediation, and never works around it by changing or deleting provider
files.

## Provider process lifecycle

`agent-speak serve --config ...` discovers and starts exactly one runtime
provider process for the selected external TTS backend, performs hello and
initialization once, and reuses that warm process for sequential speech jobs.
The child has no idle timeout and normally lives until `serve` shuts down.

Agent Speak owns the child completely:

- normal server shutdown sends `session.shutdown`, waits for the bounded grace,
  closes stdin, and reaps the child;
- an unexpected crash fails the active job and permits the one documented clean
  restart before the next job;
- a stuck child and its provider-created process tree are terminated after
  cancellation/shutdown grace;
- TTS-disabled or system-TTS profiles never start an UtterPipe runtime child.

Each inspect, validate, catalog, or prepare CLI invocation starts its own
short-lived inspect/management provider process and reaps it before returning.
It never attaches to a running `serve` child and no provider remains as a daemon.

The provider's protocol reader remains responsive for the process lifetime, but
methods are not legal in every state. Agent Speak sends one ordinary operation
at a time; during active work it sends only cancellation or shutdown. A
provider returns `busy` for any other concurrent request rather than queueing
it.

## Process client

Add one engine-neutral client implementation responsible for:

- exact child spawn and environment construction;
- asynchronous stdin/stdout framing;
- the UtterPipe session state machine and request correlation;
- control-frame and audio-frame bounds before allocation;
- provider identity/capability/delivery-mode/audio-format negotiation;
- deadlines and cancellation grace;
- concurrent bounded stderr draining/redaction;
- process exit monitoring and complete child-tree termination;
- mapping protocol failures to concise Agent Speak errors.

Suggested module boundary, not a public API promise:

```text
src/provider/
  discovery.rs
  frame.rs
  message.rs
  client.rs
  management.rs
  runtime.rs
  worker.rs
```

The protocol implementation must be usable by CLI management and runtime TTS
without duplicating parsers or state machines.

## Playback integration

### Current constraint

The playback actor owns a non-mixing queue. Current system TTS synthesizes and
then plays through Rodio inside `TtsAdapter::speak_to`. A slow external provider
must not block that actor because `interrupt` must remain responsive.

### External TTS worker

Implement an `UtterPipeTts` handle satisfying `TtsAdapter`. It communicates with
a dedicated worker thread that owns:

- one initialized runtime provider child;
- bounded command/frame channels and a dedicated blocking protocol reader for
  bidirectional pipe I/O and cancellation;
- one Rodio output path used only for provider-generated speech, supporting
  complete prepared audio and incremental decoded-PCM sample queues;
- one bounded encoded-input/decoder worker and bounded decoded-output channel
  when MP3 or Ogg Opus is negotiated;
- the active synthesis/playback state.

`speak_to` sends a bounded worker command containing text, gain, selected output
target, and the existing `CompletionNotifier`, then returns after the worker
accepts it—not after synthesis. The playback actor marks speech active and
continues servicing interrupt/shutdown commands.

For runtime initialization Agent Speak sends
`accepted_delivery_modes = ["incremental", "complete"]`; management sessions
send `["complete"]`. Both send these accepted formats in preference order:
`audio/ogg;codecs=opus`, `audio/mpeg`, `audio/pcm;codec=pcm_s16le`, and
`audio/wav;codec=pcm_s16le`. Agent Speak uses the provider's negotiated pair for
the lifetime of that process; there is no user-facing delivery setting in the
profile schema. Providers may advertise other registered formats for other hosts.

Complete-delivery sequence:

1. send `synthesis.start` using initialized model/voice/options;
2. concurrently watch the actor command channel for stop/shutdown;
3. receive and validate the declared bounded audio frame;
4. for WAV, construct `PreparedAudio::from_memory`, preserving decoder
   preflight; for MP3/Ogg Opus, decode and validate the bounded complete stream
   once before playback;
5. call worker-owned `RodioAudio` with the original target/gain;
6. deliver the existing completion callback only after Rodio completes/fails;
7. retain no generated bytes after terminal cleanup.

Incremental-delivery sequence:

1. send `synthesis.start` and concurrently watch stop/shutdown;
2. validate `synthesis.audio_begin`; raw PCM16 requires sample rate/channel
   metadata, while self-describing MP3/Ogg Opus forbids it;
3. for PCM, validate frame alignment directly; for compressed streams,
   concatenate arbitrary transport chunks through a bounded decoder input,
   validate the negotiated container/codec, and feed its bounded decoded PCM
   output to Rodio in order;
4. start the output after a 200 ms prebuffer target or after successful
   synthesis termination for a shorter result, whichever happens first;
5. keep at most two seconds of decoded samples queued, with four bounded encoded
   and four bounded decoded handoff slots; normal pipe backpressure slows a
   faster provider without growing host memory;
6. treat the synthesis response as generation completion, but deliver the
   existing completion callback only when every accepted sample has played;
7. on a terminal synthesis error, stop and discard queued samples, report
   failure once, and never retry the partially heard text.

The incremental player must not use the existing `Player::empty()` watcher as
generation completion: the queue can become momentarily empty between chunks.
Completion requires both a successful terminal provider response and an empty
Rodio queue. Temporary underrun silence is observable in diagnostics and tests,
not treated as completion.

`stop` sends a synchronous worker command and waits for a bounded acknowledgement:

- during synthesis, request provider cancellation; after grace, kill and mark
  the runtime for restart;
- during complete or incremental Rodio playback, stop the sink and discard any
  queued/in-flight incremental frames;
- return only after the active output has been asked to stop;
- complete the interrupted job through the actor's existing lifecycle rules.

`finished` tells the worker to release per-item Rodio/provider state.
`shutdown` cancels active work, shuts down the provider protocol, kills it if
needed, stops Rodio, joins the worker, and leaves no provider child.

Windows/macOS system TTS remains the existing implementation. Linux uses the
same `UtterPipeTts` path as every other provider; no eSpeak process, catalog, or
audio code belongs in the host. Use an internal configured-TTS enum or
equivalent to select `SystemTts` versus `UtterPipeTts`; do not put provider
branches throughout MCP handlers.

The actor already prevents audio-file and speech jobs from intentionally
overlapping. Because ordinary audio and external speech use separate Rodio
instances, the synchronous stop acknowledgement is required before the actor
starts a replacement job.

## Startup and recovery

`serve --config` performs provider discovery and runtime initialization before
starting the MCP stdio service. Missing executable/model/voice, bad identity,
invalid options, incompatible protocol, or unsupported mandatory complete
PCM16 WAV fails startup with an `agent-speak prepare`/configuration
remediation. Startup chooses a valid provider-advertised pair from the exact
preference lists above.

No startup code calls prepare or prompts.

If the provider crashes or is killed for stuck cancellation:

- the active playback job fails;
- no automatic synthesis retry occurs;
- before the next queued speech job, the worker performs one clean runtime
  restart from the same frozen config;
- repeated restart failure leaves external TTS unavailable and fails subsequent
  speech jobs without affecting ordinary audio playback;
- endpoint/model/voice/options are never changed during recovery.

## Limits

Agent Speak supplies:

- `max_text_code_points = tts.maximum_characters`;
- `max_audio_bytes = 256 MiB` initially, matching current synthesized-audio
  bounds;
- synthesis timeout computed from text length and capped at 120 seconds,
  matching the current platform policy unless a lower provider-specific bound
  applies;
- control frame maximum 1 MiB;
- retained provider stderr maximum 1 MiB per process while continuing to drain;
- cancellation grace initially 1 second;
- shutdown grace initially 3 seconds;
- incremental prebuffer target 200 ms;
- maximum queued incremental audio 2 seconds, independently bounded by the
  cumulative transported-byte limit;
- encoded and decoded handoff queues of four items each, plus a 256 MiB absolute
  decoded-byte ceiling even when the user duration limit is disabled.

All constants live in one provider policy module and are covered by boundary
tests. Provider-reported lengths, formats, duration, and readiness are untrusted.

## MCP and capability behavior

The MCP tool surface remains policy-shaped:

- external TTS does not add tools;
- `speak_text` and speech cues use the startup-selected backend;
- provider identifiers, paths, endpoint, options, credentials, model paths,
  voice reference paths, and management operations are never MCP parameters;
- `get_audio_capabilities` may report a sanitized selected voice display value,
  but does not expose executable path, service URL, environment names, or
  provider options;
- acceptance remains fire-and-forget queue acceptance, not a claim that
  synthesis or playback completed.

History retains the existing policy. Synthesized audio is never persisted.
Spoken text is retained only when the existing explicit history setting permits.

## Errors and diagnostics

Add provider-specific internal error categories without exposing raw provider
JSON or stderr to MCP clients. CLI/startup messages may include:

- configured provider slug;
- canonical executable path;
- provider product/protocol version;
- stable protocol error code;
- selected model/voice ID when needed for remediation;
- the explicit next human command.

They must not include spoken text, provider option values marked/resembling
secrets, resolved environment values, response bodies, or imported source paths.

Provider stderr is diagnostic only. Prefix retained lines with provider slug,
sanitize control characters, cap individual line length, and truncate retained
output while continuing to drain.

## Security documentation

Before release, update the README safety section and add a tracked security
model covering:

- config as authorization to execute the selected native provider;
- `PATH` hijacking and side-by-side precedence;
- process isolation not being a sandbox;
- child environment and secret exposure;
- local versus remote provider text disclosure;
- model/voice license and reference-voice consent;
- frame/audio/parser limits and child cleanup;
- shared data locking and supply-chain integrity;
- no downloads/prompts/licenses during MCP startup or calls.

## Test plan

### Unit

- every strict schema-1 backend combination;
- slug/ID/environment/options validation;
- side-by-side and `PATH` resolution including empty/relative entries, symlinks,
  permissions, duplicates, and Windows extension rules;
- frame partial I/O, every malformed header/envelope, bounds, and duplicate keys;
- session state, correlation, delivery negotiation, errors, and complete versus
  incremental audio sequencing;
- incremental PCM alignment plus MP3/Ogg Opus arbitrary transport boundaries,
  cumulative bounds, prebuffer threshold, terminal accounting, decoder failure,
  and partial failure before device playback;
- stderr sanitization/truncation and environment construction;
- worker start/stop/finished/shutdown/restart state transitions;
- provider options remain opaque/startup-fixed and actual sample-rate metadata
  drives playback adaptation.

### Fake-provider integration

- valid inspect/validate/runtime/prepare flows;
- wrong identity/version/capabilities;
- startup output contamination;
- slow synthesis with responsive interrupt;
- supported/unsupported/stuck cancellation;
- crash before/during/after audio metadata and after partially heard audio;
- missing/extra/truncated/oversized/malformed complete and incremental audio,
  including container/codec mismatch and decompression limits;
- incremental frame ordering, correlated terminal accounting, cancellation,
  and no retry after partial output;
- stderr flood without deadlock;
- clean and stuck shutdown with process-tree cleanup;
- prepare progress, license refusal, integrity failure, interruption, locking,
  stale plan, atomic activation, and no runtime mutation;
- two runtime clients using one fake shared store, runtime plus preparation,
  competing preparation, leased removal, stale lease recovery, cache races, and
  `resource_busy` diagnostics;
- sequential jobs reuse one child; inspect/management use separate children;
  idle child persistence, shutdown during work, stdin EOF, crash restart, and
  no orphan process;
- no provider execution when the effective profile has no speech path.

### Manual audio acceptance gates

Automated provider tests deliberately do not open a real output device. Before
release, exercise these behaviors with both the system speaker and a Bluetooth
output where available:

- complete and incremental audio is audible and uses the selected device;
- the incremental queue stays within two seconds of samples as measured against
  actual Rodio/player consumption, including a slow or temporarily stalled
  device, and generation applies backpressure instead of growing without bound;
- temporary underrun recovers, first-audio latency remains acceptable, and
  generation completion remains distinct from audible playback completion;
- stopping during incremental playback silences already queued samples.

### Existing regression

- current Windows/macOS system TTS tests plus Linux eSpeak-provider discovery,
  catalog, synthesis, cancellation, and playback tests;
- devices, profiles, audio cues, queue semantics, arbitrary audio, history, MCP
  stdio lifecycle, and quick-profile behavior;
- the manual speaker/Bluetooth acceptance gates above on each supported desktop
  OS when practical.

## Implementation sequence

1. Add strict schema-1 backend types with no runtime behavior change.
2. Add discovery, framing/messages, and fake-provider management client.
3. Add inspect/validate/provider CLI commands.
4. Add the external TTS worker and fake complete/incremental synthesis and
   playback.
5. Add prepare plan/apply presentation.
6. Integrate the real `openai-http` provider against a local mock and one
   manually configured compatible loopback service.
7. Update README/security documentation and complete cross-platform tests.

## Accepted project decisions

Local implementation proceeds with the common defaults recorded in the
UtterPipe specification: provisional local namespace, Apache-2.0, mandatory
complete PCM16 WAV plus optional incremental PCM16, and neutral shared provider
data roots.

The implemented child-environment baseline is `HOME`, `USERPROFILE`,
`LOCALAPPDATA`, `TMPDIR`, `TEMP`, `TMP`, `LANG`, `LC_ALL`, `SSL_CERT_FILE`,
`SSL_CERT_DIR`, `SYSTEMROOT`, and `WINDIR`, copied only when present. Configured
`provider_environment` names are required in addition to that baseline.

The Unix implementation assigns each child a dedicated process group and kills
that group on a stuck operation. The Windows implementation creates a
kill-on-close Job Object before spawn, assigns the provider immediately after
`spawn` returns, retains the job, and terminates it on a stuck operation.
Windows provider startup fails explicitly if Job Object setup or assignment is
unavailable. The standard process API starts the child before returning its
process handle, leaving a narrow pre-assignment race in which a hostile provider
could create an unassigned descendant. Providers are already trusted native
executables rather than sandboxed extensions; eliminating that residual race
would require a future Windows-specific suspended/at-creation launcher.
