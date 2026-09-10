# rpx

**Modern package management for R.**

rpx resolves compatible R package versions before installation, records them in `rpx.lock`, and maintains an isolated library for each project. Dependencies stay in the standard R `DESCRIPTION` format, so your project remains an ordinary R package.

- Automatic dependency bounds, from the selected version to below the next major release.
- A locked package set shared by developers and CI.
- Public, private, and Git package sources.

**[Documentation](https://rrepo.org/documentation/overview)**

## Install

Install R and make sure `R` and `Rscript` are available on `PATH`.

### macOS and Linux

```bash
curl -LsSf https://rrepo.org/rpx/latest/rpx-installer.sh | sh
```

### Windows

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://rrepo.org/rpx/latest/rpx-installer.ps1 | iex"
```

For source installation, Docker, and platform prerequisites, see the [installation guide](https://rrepo.org/documentation/install-rpx).

## Quick start

Create a project with the interactive initializer:

```bash
rpx init
```

From the project directory:

```bash
rpx add jsonlite
rpx add --dev testthat
rpx run R
```

Adding dependencies updates `DESCRIPTION`, resolves the lockfile, and synchronizes the project library. Commit `DESCRIPTION` and `rpx.lock` so the environment can be recreated with `rpx sync`.

For an existing project with a `DESCRIPTION` file, resolve its dependencies and install the environment:

```bash
rpx lock
rpx sync
```

## Package sources

rpx uses the rrepo repository API to discover package versions and dependency metadata. We host a CRAN mirror as one of rrepo's repositories, giving rpx access to CRAN packages through that interface. You can also configure other repositories, private packages, and Git sources.

## Documentation

- [User guide](https://rrepo.org/documentation/overview) — overview and getting started.
- [Managing dependencies](https://rrepo.org/documentation/manage-dependencies) — version bounds and dependency types.
- [Repositories](https://rrepo.org/documentation/repositories) — public, private, and Git sources.
- [Changelog](CHANGELOG.md) — release notes and breaking changes.

## Development

Run the test suite with:

```bash
cargo test
```

Integration tests require Docker and use `testcontainers` with the official `r-base` image to exercise package-management workflows without modifying your local R library.

Release-maintenance instructions are in [CONTRIBUTING.md](CONTRIBUTING.md).
