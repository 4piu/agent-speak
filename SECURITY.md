# Security model

Agent Speak treats its startup profile as trusted user policy. An
`utterpipe-*` provider additionally authorizes execution of the exact discovered
native provider. The framed protocol isolates parsing and lifecycle; it is not
a sandbox or privilege boundary.

## Configuration trust boundary

An explicit `--config` selects one complete, unmerged profile. Without it,
Agent Speak starts from built-in defaults and merges existing system, user, and
working-directory layers in that order. Later layers can enable
permission-shaped MCP tools. These files are trusted user-controlled policy,
but a working-directory file may come from a checked-out project; review
`agent-speak validate` and its reported source paths before granting persistent
MCP-host approval.

Any present unreadable, malformed, or invalid layer aborts startup. Known
relative history and cue paths resolve against their declaring layer. Opaque
provider-option strings are passed literally. Switching `tts.provider` removes
the previous provider's voice, environment allowlist, options, and credentials
before the new layer is merged, so secrets are not silently reused across
provider identities. Configuration paths are written only to human CLI output
or info-level stderr diagnostics, never MCP capability results.

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

Playback status and cancellation are scoped to IDs accepted by the current
server process. They report only lifecycle state plus a sanitized failure code:
never spoken text, cue text, local paths, output-device identities, provider
diagnostics, or history metadata. Unknown and expired IDs use the same
list-free error. There is no playback-ID listing or cancel-all operation.
Cancellation is an audible side effect: any client sharing one server process
and knowing an ID can stop that item. Retention is bounded and does not provide
human acknowledgement.

An integration can explicitly pass `serve --control-file ABSOLUTE_PATH` to
create a separate human-facing control channel. Agent Speak listens only on an
ephemeral `127.0.0.1` port, creates rather than replaces the descriptor, and
writes a random bearer token; POSIX descriptors must have mode `0600`. The
descriptor is a same-user secret, not a sandbox against another process running
as that user. It must stay in an integration-owned private directory and must
never be sent to an MCP client or webview.

The control channel can list the current server process's bounded lifecycle
snapshot, cancel one retained ID, or emergency-stop all active and queued work.
Responses contain only IDs, stable states, terminal flags, one sanitized
failure code, and an affected-item count. They contain no playback content,
paths, devices, provider data, or backend diagnostics. Emergency stop first
discards queued work. If the backend cannot confirm stopping the active item,
that item retains its last observed state, the backend becomes unhealthy, and
the request returns only `playback_unavailable`; Agent Speak never starts
replacement audio to disguise a failed stop. The channel is optional and does
not alter the MCP tool surface.

`serve-http` is a separate MCP transport for trusted desktop integrations. It
binds only to an ephemeral IPv4 loopback port, rejects every request without
the exact generated bearer credential, retains the SDK's loopback `Host`
validation against DNS rebinding, and limits request bodies to 128 KiB. Its
private descriptor contains the loopback URL and bearer token, is created with
the same no-overwrite/POSIX `0600` rules, and is removed on orderly shutdown.
The token is never accepted through process arguments and is not written to
logs or errors.

The initial HTTP transport uses one random credential for one server lifetime.
Every authorized MCP session shares the same frozen profile, playback queue,
status retention, and cancellation namespace. It is therefore a same-user
broker, not a client isolation boundary; restart the process to revoke and
rotate the credential. Per-client credentials and revocation belong to the
extension-owned broker layer. No public bind address or unauthenticated HTTP
mode is available. Remote SSH use must forward loopback through an authenticated
encrypted tunnel rather than expose the desktop listener directly.

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
