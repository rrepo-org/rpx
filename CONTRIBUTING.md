# Contributing

## Running tests

Run the test suite with:

```bash
cargo test --workspace --locked
```

The integration tests run native R processes and require R on `PATH`, plus the
toolchain needed to compile R source packages. They isolate project libraries,
caches, and temporary files. Some tests access the live built-in repository.

For the same concurrency settings used by the native E2E suite, run:

```bash
cargo nextest run --workspace --locked
```

The task runner, including its internal scheduler, can be tested without R:

```bash
cargo test -p rpx-task --locked
```

The protocol SDKs also run independently of R:

```bash
cargo test -p cran-sdk -p rrepo-sdk --locked
```

See [sync execution](docs/sync-engine.md) for crate boundaries and test coverage.

## Preparing a release

Before releasing:

1. Update the version in `crates/rpx/Cargo.toml` and refresh the root `Cargo.lock`.
2. Move the relevant entries from `Unreleased` into a matching version section in [CHANGELOG.md](CHANGELOG.md). The heading must contain the exact package version so cargo-dist can use it for the GitHub release title and body.
3. Run the test suite.

Create a release by pushing a matching version tag such as `v2.0.0`. The cargo-dist workflow builds and signs release artifacts, publishing archives, checksums, installers, and release notes to GitHub Releases and uploading release assets to R2 for distribution through `rrepo.org`.

The Docker workflow publishes `ghcr.io/rrepo-org/rpx` images for `linux/amd64` and `linux/arm64`. Only stable `vMAJOR.MINOR.PATCH` tags update the Docker `latest` tag and the `latest` download on `rrepo.org`.
