# Contributing

## Running tests

Run the test suite with:

```bash
cargo test
```

The integration tests require Docker and use `testcontainers` with the official `r-base` image. They exercise package-management workflows without changing your local R installation or package library.

## Preparing a release

Before releasing:

1. Update the version in `Cargo.toml` and `Cargo.lock`.
2. Move the relevant entries from `Unreleased` into a matching version section in [CHANGELOG.md](CHANGELOG.md). The heading must contain the exact package version so cargo-dist can use it for the GitHub release title and body.
3. Run the test suite.

Create a release by pushing a matching version tag such as `v2.0.0`. The cargo-dist workflow builds and signs release artifacts, publishing archives, checksums, installers, and release notes to GitHub Releases and uploading release assets to R2 for distribution through `rrepo.org`.

The Docker workflow publishes `ghcr.io/rrepo-org/rpx` images for `linux/amd64` and `linux/arm64`. Only stable `vMAJOR.MINOR.PATCH` tags update the `latest` download on `rrepo.org`.
