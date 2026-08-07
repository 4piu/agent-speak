# Security model

Agent Speak treats its startup profile as trusted user policy. An
`utterpipe-*` backend additionally authorizes execution of the exact discovered
native provider. The framed protocol isolates parsing and lifecycle; it is not
a sandbox or privilege boundary.

## Provider trust boundary

Agent Speak resolves only `utterpipe-<configured-slug>` beside its own resolved
executable and then in absolute, nonempty `PATH` directories. Side-by-side
placement has precedence. The final regular executable is canonicalized and
spawned directly without a shell. A changed executable or `PATH` may select
different trusted code on the next command, so users and package managers must
protect those locations against untrusted writes.

The child environment is cleared. Agent Speak copies only platform process/TLS
basics plus names explicitly listed in `provider_environment`; missing requested
values fail startup. Provider options may contain credentials; those values are
stored in the trusted profile and sent through the provider's private
initialization pipe, but are not placed in command arguments, stdout, or MCP
results. Provider stderr is drained continuously and retained only up to an
internal bound. Providers must not log spoken text or secrets.

Local engine providers receive spoken text over a private inherited pipe.
Network-backed providers may disclose it to the endpoint fixed by startup
configuration. Agent Speak cannot verify a provider's internal retention,
proxy, TLS, model, or license behavior; inspect its provider-specific contract.

## Assets and consent

Providers own shared per-user data/cache roots and must implement interprocess
locking, immutable active versions, leases, integrity verification, and atomic
activation. Agent Speak neither constructs model paths nor deletes provider
files directly. Preparation/removal use same-session plan/apply operations with
human confirmation. Required licenses need explicit IDs even with `--yes`.

Generic asset import requires a provider-declared kind, an absolute regular
file, an explicit requested asset ID, and `--consent-confirmed`. For voice
references, import a recording only when every affected speaker has authorized
creation and intended use of the derived voice.

Provider-defined per-utterance controls are unavailable to an agent unless the
trusted profile names them in `agent_utterance_options`. Agent Speak verifies
the provider's bounded schema and digest at startup, projects only granted
properties into MCP, and validates values locally before relay. A provider
restart must return the identical schema and audio selection for that live
server.

## Untrusted output and cleanup

Control frames, JSON nesting/duplicate keys, transported audio lengths, PCM
alignment, sample rates, channel counts, and cumulative audio are independently
bounded and validated. Complete WAV output receives decoder preflight. MP3 and
Ogg Opus are treated as untrusted encoded input and receive one downstream
container/codec validation and decoding pass. Incremental compressed playback
uses separate four-item encoded and decoded handoff queues, a 200 ms prebuffer
measured from decoded samples, a bounded two-second playback queue, and a 256
MiB absolute decoded-byte ceiling. Partial output is never retried
automatically.

On Unix, each provider is placed in a dedicated process group and timeout or
cancellation kills that group. On Windows, Agent Speak creates a kill-on-close
Job Object before spawn, assigns the provider immediately after `spawn`
returns, and retains it; timeout, cancellation, or host exit terminates the job
and its descendants. Agent Speak refuses provider startup if Job Object setup
or assignment fails. Rust's standard Windows process API starts the child
before returning its handle, so a hostile provider could race to create an
unassigned descendant before assignment; this is another reason providers are
trusted native executables, not sandboxed extensions.

No provider download, data conversion, prompt, license acceptance, or data
mutation occurs during MCP calls. `validate`, local catalog queries, and runtime
initialization do not mutate provider data; initialization may only materialize
a provider-declared bounded, reconstructible engine cache. `prepare`, generic
asset import, and exact removal are separate explicit commands.
