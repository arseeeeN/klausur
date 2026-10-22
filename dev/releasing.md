# Releasing

Releases are built and published by `.github/workflows/release.yml` when a tag `vX.Y.Z` is
pushed. Each release carries a binary per target (`klausur-<target>[.exe]`), `SHA256SUMS`, and
the install scripts from `install/`, which check the download against `SHA256SUMS`.

Targets: Linux x86_64 and aarch64 (static musl builds), macOS arm64 and x86_64, Windows x86_64.

## Cutting a release

1. Bump `version` in `Cargo.toml` (and in `editors/tree-sitter-klausur/tree-sitter.json` and
   `package.json` if the grammar changed), run `cargo test`, commit, and push to `main`.
2. Wait for CI to pass, then tag and push: `git tag v0.2.0 && git push origin v0.2.0`.
3. The workflow checks that the tag matches `Cargo.toml`, builds every target, and publishes
   the release with notes generated from the merged pull requests and commits.

The repository must be public: the install scripts download from GitHub without logging in.
