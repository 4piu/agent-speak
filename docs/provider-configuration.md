# Provider configuration

This is the user-facing reference for selecting an independently installed
[UtterPipe](https://github.com/4piu/utterpipe) TTS provider in Agent Speak. For
the wire contract and host implementation details, see
[the integration specification](utterpipe-integration.md).

## Select and discover a provider

Use profile schema 1 and put the provider's portable command name (without
Windows' `.exe`) in `backend`:

```toml
schema_version = 1

[tts]
enabled = true
backend = "utterpipe-openai-http"
model_id = "gpt-4o-mini-tts"
voice_id = "coral"
maximum_characters = 500
```

For `backend = "utterpipe-<slug>"`, Agent Speak checks:

1. the directory containing the running `agent-speak` executable;
2. each absolute, nonempty `PATH` directory, in order.

The first exact executable match is used. Discovery does not scan for or
register providers, and the selected path is not persisted. Agent Speak invokes
the provider as `utterpipe-<slug> protocol --stdio` and requires its reported
slug to match.

## Model, voice, and options

`model_id` and `voice_id` select provider-owned resources. Use these commands to
inspect the configured provider:

```text
agent-speak provider info --config ./agent-speak.toml
agent-speak provider models --config ./agent-speak.toml
agent-speak voices --config ./agent-speak.toml
```

`provider_options` is an engine-specific TOML table. Agent Speak converts TOML
strings, booleans, finite numbers, arrays, and tables to JSON without assigning
engine meaning. The provider validates the result during initialization, and
the values remain fixed for that provider process.

```toml
[tts.provider_options]
speed = 1.0
```

Available options are documented by each provider:

- [eSpeak NG options](https://github.com/4piu/utterpipe-espeak-ng#agent-speak-configuration)
- [Pocket TTS options](https://github.com/4piu/utterpipe-pocket-tts#provider-options)
- [OpenAI-compatible HTTP options](https://github.com/4piu/utterpipe-openai-http#provider-options)

## Provider environment

`provider_environment` contains environment-variable names, not values:

```toml
provider_environment = ["OPENAI_TTS_API_KEY"]

[tts.provider_options]
api_key_env = "OPENAI_TTS_API_KEY"
```

Agent Speak clears the child environment before starting a provider. It then
copies these baseline variables when they exist:

```text
HOME USERPROFILE LOCALAPPDATA TMPDIR TEMP TMP LANG LC_ALL
SSL_CERT_FILE SSL_CERT_DIR SYSTEMROOT WINDIR
```

Finally, it copies every name listed in `provider_environment` from the process
that launched Agent Speak. A listed variable that is absent is an error. This
allowlist reduces accidental credential exposure, but it is not a sandbox: a
provider is a native process running as your user.

Agent Speak does not read `.env` files. Set variables in the MCP host's process
environment (usually in that host's server configuration) or launch the host
from an environment where they are already present. When a provider option such
as `api_key_env` names a credential, the same name must also appear in
`provider_environment`.

## Storage and management

Agent Speak passes one provider-specific data root and cache root:

| Platform | Data | Cache |
| --- | --- | --- |
| Windows | `%LOCALAPPDATA%\UtterPipe\providers\<slug>\data` | `%LOCALAPPDATA%\UtterPipe\providers\<slug>\cache` |
| macOS | `~/Library/Application Support/UtterPipe/providers/<slug>/data` | `~/Library/Caches/UtterPipe/providers/<slug>` |
| Linux | `${XDG_DATA_HOME:-~/.local/share}/utterpipe/providers/<slug>` | `${XDG_CACHE_HOME:-~/.cache}/utterpipe/providers/<slug>` |

Use Agent Speak's management commands when the provider will be used by Agent
Speak; this guarantees the same roots are supplied during preparation and
serving:

```text
agent-speak prepare --config ./agent-speak.toml
agent-speak provider voices import --config ./agent-speak.toml --source /absolute/reference.wav --id my-voice --consent-confirmed
agent-speak provider remove --config ./agent-speak.toml --artifact voice:my-voice
```

Preparation, import, and removal require an explicit human CLI operation. They
are never triggered by `serve` startup or an MCP call.

## Process and audio behavior

`serve` starts one runtime provider, initializes it once, and reuses it for
sequential speech jobs. Agent Speak shuts it down with the server and may make
one clean restart after an unexpected crash. Other Agent Speak commands use
their own short-lived provider process, so providers must safely support
multiple processes sharing the same assets.

Agent Speak offers complete and incremental delivery and negotiates only the
formats it can decode: PCM16 WAV, raw PCM16, MP3, and Ogg Opus. Providers may
advertise additional UtterPipe formats for other host applications; Agent Speak
will not select them.
