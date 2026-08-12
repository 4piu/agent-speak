Agent Speak 0.2.0 adds observable, cancellable playback and opt-in audio
mixing while preserving the local, policy-controlled MCP boundary.

Highlights:

- report queued, active, completed, cancelled, and failed playback without
  exposing spoken text or local paths;
- cancel one playback or stop the complete queue through a private local
  control channel;
- opt into bounded `mix` playback with one shared output stream and
  proportional gain headroom;
- discover layered system, user, and working-directory profiles when
  `--config` is absent, while an explicit `--config` remains a complete,
  unmerged profile;
- expose structured device and system-voice inventories for local UIs; and
- update the Pocket TTS example for the verified XN Q8 provider profile.

Existing v0.1 profiles remain valid. Mixing is disabled unless `mix` is added
to `playback.allowed_concurrency`; an omitted `maximum_mix_streams` defaults to
2. Release binaries and tags remain unsigned, with SHA-256 files published
beside every archive.
