# rpx

**Modern package management for R.**

`rpx` resolves the dependencies declared in an R package's `DESCRIPTION`, records the exact result in `rpx.lock`, and installs it into an isolated project library.

## Install

`rpx` requires R to be installed and available on `PATH`. Confirm that `Rscript` works before installing `rpx`.

- [Install R on Windows](https://cran.r-project.org/bin/windows/base/)
- [Install R on macOS](https://cran.r-project.org/bin/macosx/)
- [Choose a CRAN mirror](https://cran.r-project.org/mirrors.html)

Install the latest release on macOS or Linux:

```bash
curl -LsSf https://rrepo.org/rpx/latest/rpx-installer.sh | sh
```

Install the latest release on Windows:

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://rrepo.org/rpx/latest/rpx-installer.ps1 | iex"
```

Windows Defender or SmartScreen may warn while binary signing is being finalized.

You can also install `rpx` from Git with Cargo:

```bash
cargo install --git https://github.com/scalerail-solutions/rpx.git
```

See the [installation guide](docs/02.install-rpx.md) for source-build requirements and Docker usage.

## Quick Start

Create a package project, add a dependency, and run R in its project library:

```bash
rpx init
cd <project>
rpx add digest
rpx run R
```

For an existing installable R package:

```bash
rpx lock
rpx sync
rpx status
```

By default, `rpx add`, `rpx remove`, and `rpx sync` also install the current project package. Pass `--no-install-project` to any of those commands to synchronize only its dependencies and remove an installed copy of the project package.

Commit both `DESCRIPTION` and `rpx.lock`.

## Documentation

- [Overview](docs/01.overview.md)
- [Install rpx](docs/02.install-rpx.md)
- [Start a project](docs/03.start-a-project.md)
- [Use an existing project](docs/04.use-an-existing-project.md)
- [Manage dependencies](docs/05.manage-dependencies.md)
- [Run commands](docs/06.run-r.md)
- [Configure repositories](docs/07.repositories.md)

The [rrepo documentation](https://rrepo.org/documentation/overview) covers hosted repositories, dashboard workflows, publishing, and API-key permissions.

## Local Development

Run the test suite with:

```bash
cargo test
```

The integration tests depend on Docker and use `testcontainers` to run package-management workflows against the official `r-base` image.
