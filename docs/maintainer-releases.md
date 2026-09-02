# Maintainer release guide

Crate publication is deliberately manual. Configure the protected `crates-io`
GitHub environment with a `CARGO_REGISTRY_TOKEN` secret, then run the
**Publish crate** workflow from `main` for `wineforge-core` followed by
`wineforge-cli`. CLI publication succeeds only after crates.io has indexed the
matching core version.

To publish prebuilt binaries, push a `vVERSION` tag whose version exactly
matches `wineforge-cli` and whose commit is contained in `main`. The release
workflow creates an immutable GitHub release containing:

- ad-hoc-signed archives for Apple Silicon and Intel macOS;
- an x86-64 Linux archive;
- a per-target SPDX 2.3 JSON SBOM (`wineforge-cli-TARGET.spdx.json`); and
- SHA-256 sidecars for the archives and SBOMs.

The SBOM scans the packaged binaries and the release's committed `Cargo.lock`.
It includes resolved Cargo dependencies, including development and other-target
dependencies, not just crates linked into that target's executables. It is an
inventory aid, not proof of license compliance or absence of vulnerabilities.
Generation or basic document validation failure blocks release publication.
Existing immutable releases are not modified retroactively.

Archive names match the explicit cargo-binstall metadata. The workflow refuses
to replace an existing release.

Before publishing, run locally:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
