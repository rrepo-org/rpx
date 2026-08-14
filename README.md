# rpx

**Modern package management for R.**

`rpx` brings modern package-management semantics to R projects. Packages declare compatible dependency ranges in [`DESCRIPTION`](https://cran.r-project.org/doc/manuals/r-release/R-exts.html#The-DESCRIPTION-file), `rpx` resolves those constraints before installation, and `rpx.lock` records the exact result.

`rpx` works with `rrepo`, the registry infrastructure that provides the package metadata needed for reliable resolution across public, private, and historical R packages.

The short version:

- `rpx` is the developer workflow.
- `rrepo` provides registry APIs for CRAN mirrors and private repositories.
- `DESCRIPTION` declares compatibility.
- `rpx.lock` records the exact solution.

## Why rpx Exists

R projects should not depend on whatever happens to be installed in a user's global library. `rpx` gives each project its own locked package set and runs R with that project library active.

Use `rpx` when you want:

1. A committed `rpx.lock` for the exact package versions used by the project.
2. A local project library that can be recreated from that lockfile.
3. Dependency declarations in `DESCRIPTION` that give the resolver useful version bounds.
4. CI and developer machines using the same package set without sharing a global library.

The important difference from a snapshot-only workflow is that `DESCRIPTION` is not only a list of package names. It is the input to the resolver.

## Dependency Bounds

Most R packages either leave dependency versions unbounded or only set a lower bound. Compatibility is often handled outside the dependency declaration, especially through [CRAN reverse dependency checks](https://r-pkgs.org/release.html#sec-release-revdep-checks).

`rpx` moves more of that compatibility information into `DESCRIPTION`, where the resolver can use it directly. When you add a package, `rpx` records the version it selected as the lower bound and the next major version as the upper bound.

For example, adding a package resolved at `1.4.2` writes:

```text
Imports:
    examplepkg (>= 1.4.2),
    examplepkg (< 2.0.0)
```

The lower bound prevents the solver from choosing a version older than the one you added. The upper bound prevents an automatic jump to the next major version unless you change the constraint.

This uses semver because the major version is the common place to signal breaking changes. R packages do not universally follow semver, so this is not a guarantee that every `1.x` release is compatible or every `2.x` release is incompatible. It is a default constraint that is safer than leaving the dependency open-ended.

For `0.x` packages, `rpx` records the selected version as the lower bound and `< 1.0.0` as the upper bound.

To set a constraint yourself, use `PACKAGE@OPERATORVERSION`. For example, this writes
`digest (>= 0.6.37)` to `Imports`:

```bash
rpx add 'digest@>=0.6.37'
```

Supported operators are `<`, `<=`, `==`, `!=`, `>=`, and `>`. A constrained add replaces every
existing relation for that package in `DESCRIPTION` before adding the requested relation to
`Imports`.

## Install

`rpx` requires R to be installed and available on `PATH`. Before using `rpx`, confirm that `Rscript` works in your shell.

Install R:

- Windows: https://cran.r-project.org/bin/windows/base/
- macOS: https://cran.r-project.org/bin/macosx/
- CRAN mirrors: https://cran.r-project.org/mirrors.html

Install the latest release on macOS or Linux:

```bash
curl -LsSf https://rrepo.org/rpx/latest/rpx-installer.sh | sh
```

Install the latest release on Windows:

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://rrepo.org/rpx/latest/rpx-installer.ps1 | iex"
```

Windows binary signing is still being worked on. The PowerShell installer is available, but Windows Defender or SmartScreen may warn until the signing flow is finalized.

You can also install `rpx` from Git with Cargo:

```bash
cargo install --git https://github.com/scalerail-solutions/rpx.git
```

If you do not already have Rust and Cargo installed, install them with `rustup`:

- Rust: https://rustup.rs/
- Windows Rust/MSVC prerequisites: https://rust-lang.github.io/rustup/installation/windows-msvc.html

`rpx` prefers binary R package artifacts on Windows and macOS when available, but some packages may still need to be built from source. On Windows, install Rtools if you hit source-build requirements:

- Rtools: https://cran.r-project.org/bin/windows/Rtools/

You can also run the Docker image directly:

```bash
docker run --rm ghcr.io/scalerail-solutions/rpx:latest --help
```

The Docker image contains the `rpx` binary but does not include R. For project workflows, copy `rpx` into an image that provides R:

```dockerfile
FROM r-base:latest
COPY --link --from=ghcr.io/scalerail-solutions/rpx:latest /rpx /usr/local/bin/rpx
```

## Start a New Project

Create a `DESCRIPTION` file, add a dependency, and start R through `rpx`:

```bash
rpx init
rpx add digest
rpx run R
```

`rpx add` updates `DESCRIPTION`, resolves a compatible package set, writes `rpx.lock`, and syncs the project library.

## Use an Existing Project

For an R package project that already has a `DESCRIPTION` file, create the lockfile and install the locked package set:

```bash
rpx lock
rpx sync
rpx run R
```

Commit both `DESCRIPTION` and `rpx.lock`. Do not commit the project library or local cache; `rpx sync` recreates local state from the lockfile.

## Daily Workflow

Add or remove direct dependencies with `rpx` so the manifest, lockfile, and project library stay together:

```bash
rpx add jsonlite
rpx remove digest
```

Check the project before committing or in CI:

```bash
rpx status
```

Run R commands through `rpx run` so R sees the project library:

```bash
rpx run R
rpx run Rscript scripts/check.R
```

If local package state becomes confusing, remove the project library and caches:

```bash
rpx clean
```

## Repository Management

The package universe is the set of package versions and metadata available to the resolver.

By default, `rpx` uses the public rrepo-backed CRAN universe at `https://upstream.rrepo.dev/cran`. This gives the resolver CRAN package metadata through rrepo APIs, including historical package metadata instead of only today's latest package state.

Projects can add additional repositories. `rpx` supports rrepo API repositories, CRAN repositories, and CRAN-like repositories:

```bash
rpx repo add https://cloud.r-project.org
rpx repo add https://packagemanager.posit.co/cran/latest
rpx repo add https://<org-slug>.rrepo.dev/<repo-slug>
rpx repo list
rpx repo remove https://packagemanager.posit.co/cran/latest
```

The default rrepo-backed CRAN universe remains enabled unless you explicitly disable it. A useful setup is to keep the default universe enabled and add another CRAN or CRAN-like repository as a fallback for binary artifacts:

```bash
rpx repo add https://packagemanager.posit.co/cran/latest
```

During locking, `rpx` merges versions across enabled repositories. Existing locked versions are preferred when they are still available from enabled repositories. If the same version is available from multiple repositories, the earlier repository wins for the locked source URL.

When a repository requires authentication, `rpx` prompts for an API key and stores it in the operating system keyring for that repository URL.

You can override the default public registry root with `RPX_REGISTRY_BASE_URL`:

```bash
RPX_REGISTRY_BASE_URL=https://example.rrepo.dev/cran rpx lock
```

To lock without the default public registry, use `rpx lock --no-default-repo`. The default-repo choice is recorded in `rpx.lock`; use `--default-repo` to enable it again when regenerating the lockfile.

For private package universes, add an rrepo repository for your organization:

```bash
rpx repo add https://<org-slug>.rrepo.dev/<repo-slug>
```

## rrepo

`rpx` can use CRAN and CRAN-like repositories, but its default package universe is the rrepo-backed CRAN mirror at `https://upstream.rrepo.dev/cran`.

A plain CRAN-style mirror is mostly a package distribution endpoint. It is enough for installing available packages, but it is not a registry API built around dependency solving, package history, artifact selection, authentication, and private packages.

`rrepo.org` mirrors CRAN and exposes that package universe through rrepo APIs. That gives `rpx` a default source for CRAN package versions, dependency metadata, historical package metadata, and platform artifacts.

For teams, rrepo provides the same registry model for private R packages: publishing, access control, and private package metadata that can be resolved together with CRAN packages.

## Documentation

The full user guide lives at [rrepo.org](https://rrepo.org/documentation/overview):

- [Install rpx](https://rrepo.org/documentation/install-rpx)
- [Start a project](https://rrepo.org/documentation/start-a-project)
- [Use an existing project](https://rrepo.org/documentation/use-an-existing-project)
- [Run R](https://rrepo.org/documentation/run-r)
- [Private packages](https://rrepo.org/documentation/private-packages)

## How It Works

- `DESCRIPTION` is the dependency manifest and compatibility contract.
- `rpx.lock` records the resolved package set, package sources, and R runtime metadata.
- `rpx lock` resolves dependencies from enabled repositories and writes `rpx.lock`; it does not install packages.
- `rpx` prefers existing locked versions when they are still available from enabled repositories.
- `rpx sync` installs exactly what `rpx.lock` records into the project library.
- `rpx sync` tries Windows and macOS binary artifacts from enabled repositories when available, then falls back to the locked source artifact.
- `rpx run` sets the R library path for the command it runs.

## Local Development

Run the test suite with:

```bash
cargo test
```

The test suite depends on Docker and uses `testcontainers`.

Integration tests run against the official `r-base` image and execute realistic package-management workflows inside containers. This keeps tests close to real usage while avoiding changes to your local R installation or package library.

Releases are created by pushing a version tag such as `v1.1.0`. The release workflow builds precompiled binaries for Linux, macOS, and Windows, then uploads archives, checksums, and installers to GitHub Releases. The Docker workflow publishes `ghcr.io/scalerail-solutions/rpx` images for `linux/amd64` and `linux/arm64`.
