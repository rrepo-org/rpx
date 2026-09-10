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

## How rpx compares

| | rpx | rv | uvr | renv | pak | rig |
|---|---|---|---|---|---|---|
| **Primary focus** | Constraint-driven project environments | Declarative project environments | Package and R-version management | Project snapshot and restore | Package resolution and installation | R-version management |
| **Dependency declaration** | `DESCRIPTION` | `rproject.toml` | `uvr.toml` | Project discovery or `DESCRIPTION` | Package requests or `DESCRIPTION` | — |
| **Version selection** | Explicit ranges with automatic bounds | Repository snapshots and package sources | Version requirements | Recorded versions and explicit installs | Package requests and constraints | — |
| **Lockfile** | `rpx.lock` | `rv.lock` | `uvr.lock` | `renv.lock` | CI-oriented lockfiles | — |
| **Remove packages outside the locked set** | Default during sync | Supported during sync | Supported during sync | Optional during restore | Not its primary workflow | — |
| **R-version management** | Planned; currently uses installed R | Selects installed R | Installs and selects R | Records R version; installation is external | Uses installed R | Installs and selects R |

rpx focuses on resolving compatibility requirements expressed in standard R metadata and maintaining the resulting project environment. R-version management is on the roadmap; today, rpx uses the R installation available on `PATH` and validates its version against the lockfile.

Sources: [rv](https://a2-ai.github.io/rv-docs/), [rv version selection](https://a2-ai.github.io/rv-docs/cookbook/pkg_version/), [uvr](https://github.com/nbafrank/uvr), [renv](https://rstudio.github.io/renv/), [renv restore](https://rstudio.github.io/renv/reference/restore.html), [pak](https://pak.r-lib.org/), [rig](https://github.com/r-lib/rig). Comparison reviewed September 2026.

## Package sources

Resolving across package versions requires access to their dependency metadata. The rrepo repository API exposes that information directly, including historical versions, so rpx can compare candidates before downloading package archives. We host a CRAN mirror as one of rrepo's repositories to make CRAN packages available through this interface. rpx also supports other repositories, private packages, and Git sources.

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
