# Third-party notices

Agent Speak is licensed under Apache-2.0. The exact Rust dependency inventory,
copyright notices, and license texts for all release targets are in
[`THIRD_PARTY_LICENSES.html`](THIRD_PARTY_LICENSES.html). `Cargo.lock` and each
release SBOM record the corresponding versions and checksums.

## Symphonia codecs

Agent Speak incorporates unmodified Symphonia 0.5.5 source files under
MPL-2.0 for WAV, MP3, FLAC, Ogg Vorbis, and PCM decoding. Recipients may obtain
the exact MPL-covered source from the `corresponding-source/mpl-crates`
directory in every release source archive, or from
<https://github.com/pdeljanov/Symphonia/tree/v0.5.5>. The MPL-2.0 text and
per-package notices are reproduced in `THIRD_PARTY_LICENSES.html`; no Agent
Speak source file is derived from a Symphonia source file.

Ogg Opus decoding uses the separately listed, permissively licensed
`symphonia-adapter-libopus` and `opusic-sys` packages.

Regenerate and compare the report with cargo-about 0.9.1:

```sh
cargo about generate --locked --offline --fail --all-features \
  about.hbs --output-file THIRD_PARTY_LICENSES.generated.html
tr -d '\r' < THIRD_PARTY_LICENSES.generated.html > THIRD_PARTY_LICENSES.normalized.html
cmp THIRD_PARTY_LICENSES.html THIRD_PARTY_LICENSES.normalized.html
```
