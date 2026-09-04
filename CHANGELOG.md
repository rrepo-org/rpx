# Changelog

All notable changes to rpx are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and rpx adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

[Unreleased]: https://github.com/rrepo-org/rpx/compare/v2.0.0...HEAD
[2.0.0]: https://github.com/rrepo-org/rpx/compare/v1.7.0...v2.0.0

## [Unreleased]

## [2.0.0]

### Breaking changes

- Migrated `rpx.lock` to schema version 5. Existing lockfiles must be regenerated with `rpx lock` before they can be synchronized.
- Removed `--default-repo` and `--no-default-repo` from `add`, `remove`, and `lock`. Projects now always have one persistent base repository, configured with `rpx repo base set` and restored with `rpx repo base reset`.
- Reworked project initialization. `rpx init` now initializes only a new or completely empty target directory and generates an installable package by default.
- Changed the operating-system project directory organization from Scalerail to rrepo. Existing caches and project libraries are not migrated and can be recreated with `rpx sync`.

### Added

- Added Git package repositories from GitHub, GitLab, Bitbucket, and generic Git URLs. Branches and tags are resolved to immutable commits in the lockfile.
- Added persistent base, additional, and Git repository management through `rpx repo`.
- Added `--depends`, `--imports`, `--linking-to`, `--suggests`, and `--dev` dependency field selection to `rpx add`.
- Added version constraints to `rpx add` using forms such as `'digest@>=0.6.37'`.
- Added automatic lower and next-major upper bounds when an unconstrained repository package is added.
- Added `--no-install-project` to `add`, `remove`, and `sync` for synchronizing only package dependencies.
- Added dependency-only projects through `rpx init --type project`.
- Added interactive project type, base repository, development dependency, and Git repository selection to `rpx init`.
- Added cross-platform end-to-end coverage for Linux, macOS, and Windows.

### Changed

- The built-in rrepo-backed CRAN universe is now an explicit base repository that can be replaced per project.
- `rpx init` generates package metadata, namespace, build exclusions, license files, a lockfile, and an isolated project library in one flow.
- Git repository operations use the installed `git` executable and the user's configured credential helpers or SSH agent.
- Package metadata parsing and DESCRIPTION mutations use the shared `r-metadata` implementation.
- Package installation uses library transactions so failed installs do not publish partial package directories.
- Synchronization schedules package operations from the complete hard-dependency graph and reports dependency cycles and missing packages directly.
- `rpx run` validates the locked environment, executes commands without an implicit shell, and reliably propagates command failures.

### Fixed

- Preserved R package installation diagnostics, including failures from packages such as `RcppParallel` on Windows.
- Isolated source package builds from project and repository checkout directories.
- Restored case-insensitive package sorting in `DESCRIPTION`.
- Improved diagnostics for invalid CRAN package indexes and malformed package metadata.
- Removed stale packages and interrupted installation state more reliably during synchronization.
