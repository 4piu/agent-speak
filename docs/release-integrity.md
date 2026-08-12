# Release integrity and signing

The v0.1 and v0.2 release lines deliberately use checksum verification without
platform code signing. This is the exact status unless a later release note
explicitly says otherwise.

| Platform | v0.1–v0.2 status |
| --- | --- |
| Windows | The PE executable and ZIP are not Authenticode-signed. |
| macOS | The Mach-O executable is not Developer ID-signed, hardened-runtime signed, notarized, or stapled. |
| Linux | The ELF executable and archive have no GPG, minisign, or Sigstore signature. |

The release tags are lightweight and unsigned. GitHub Actions builds each
archive and publishes a separate SHA-256 file; the installers download both,
reject a mismatch, and install by atomic replacement. Because the checksum and
archive share the same unsigned release channel, this detects corruption but is
not independent proof of publisher identity.

This is accepted for the experimental v0.1–v0.2 lines so the project does not depend
on private signing credentials. Platform signing, signed annotated tags, and
an independently signed release index are required before a wrapper or package
manager silently installs binaries without showing this status.
