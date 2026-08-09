# Resolution And Sync Plan

## Status

This is a working design document. It records the data that must flow through
resolution, locking, status, and sync. The pseudocode and type names below are
not a commitment to a particular Rust data model. The final types should follow
from the operations and invariants rather than from an illustrative collection
of vectors, maps, or IDs.

This work is a prerequisite for direct Git package sources. It applies equally
to registry packages and the active workspace package.

## Core Model

The rich in-memory result of dependency resolution is the runtime source of
truth. The lockfile is a durable projection of that result, not the runtime
model itself.

```text
Manifest + effective repositories + existing-lock preferences + R runtime
    -> Resolution
    -> Lockfile projection
    -> Sync plan
    -> Execution
```

The in-memory resolution must convey:

- The effective ordered repositories.
- The selected packages and parsed versions.
- The source that provides each selected package.
- The repository associated with each package source.
- The dependency relations between selected packages.
- The active workspace package and its hard dependencies.
- Source provenance needed for locking, installation, and cache identity.

Repository-to-package ownership and the dependency graph belong in this rich
runtime model. They must not be recovered during sync through URL-prefix
matching or repeated DESCRIPTION transformations.

## Extract Once

Each command should take one command-scoped snapshot of its inputs:

1. Resolve the project root once.
2. Read and parse `DESCRIPTION` once.
3. Read and parse `rpx.lock` once, when present or required.
4. Resolve the R executable and inspect its runtime once.
5. Read installed R package state once when status or sync requires it.
6. Read host and installed system-package state once when system dependency
   planning requires it.

Functions that compute status, lock validity, or a sync plan should receive
these snapshots explicitly. They should not hide additional project, runtime,
or installed-state reads behind a `Project` argument.

## Manifest Inputs

The manifest projection used for resolution and lock matching includes:

- Project `Package` and `Version`.
- Exact `Depends`, `Imports`, `LinkingTo`, and `Suggests` declarations.
- The field, operator, and version of each relation.
- R version requirements declared through `Depends: R`.
- Ordered additional repository declarations.
- Future direct-source declarations from `Remotes`, including normalized URL,
  requested ref, and package subdirectory.

Root `Suggests` are requested development packages and belong in the resolved
project environment. They are not hard dependencies of the workspace package.
The workspace installation dependency edges therefore include `Depends`,
`Imports`, and `LinkingTo`, but do not make installation wait for `Suggests`.

## Effective Repositories

Resolution uses an explicit ordered repository set derived from:

- Manifest repository declarations.
- Whether the built-in default repository is enabled.
- The effective default repository URL.
- A command-line default-repository override, when supplied.

The lock must retain both the resulting repository metadata and whether the
default repository policy was enabled. Default-repository membership must not
be inferred later from URL equality.

`RPX_REGISTRY_BASE_URL` affects a new resolution by selecting the effective
default repository URL. It does not affect `sync`; sync uses the exact
repositories recorded in a valid lock.

Credentials, network reachability, proxy settings, and response timing affect
whether an operation can execute. They do not define repository identity and
must not be persisted as resolution inputs.

## Existing Lock Preferences

An old lock is a source of conservative resolution preferences, not the
runtime model for a new resolution.

A preference must retain package source provenance in addition to package name
and version. It is eligible only when its source remains valid in the new
effective source universe.

- Removing a repository removes preferences provided by that repository.
- Changing a direct Git URL, ref, or subdirectory invalidates its preference.
- An unchanged Git declaration retains its locked commit during ordinary
  locking.
- Explicitly refreshing a Git source suppresses the old commit preference.
- A package with the same name and version from another source must not
  silently satisfy a source-specific preference.

## Resolution

Resolution consumes the manifest projection, effective repositories, runtime
target, and eligible preferences. It produces selected packages with source
provenance and dependency relations attached.

Package metadata should be loaded and transformed once per source/package/
version identity for the command. Lockfile generation must not fetch
DESCRIPTION again or independently reconstruct dependencies and source
ownership after resolution.

Repository adapters remain responsible for protocol-specific I/O and metadata
parsing. They are providers to resolution, not the persistent identity of a
selected package.

## Durable Lock Data

The lockfile does not need to persist runtime graph containers, indexes, or
maps that can be reconstructed mechanically. It must persist the underlying
resolution facts needed to reconstruct the locked runtime model without
re-resolution or repository metadata requests:

- Exact selected package versions.
- Exact package source identity and kind.
- Explicit package-source-to-repository association.
- Dependency declarations for every selected package.
- Effective repository URL, kind, order, and required repository metadata.
- Default-repository policy.
- R runtime version and required base packages.
- Pinned system-requirements data.
- Future Git URL, requested ref, commit, tree, and subdirectory.

The current `LockedPackage.dependencies` records already retain package
dependency edges. Their min/max representation is lossy for some R operators;
the runtime model must not copy that limitation, and a future lockfile format
should preserve exact constraint semantics.

The provider map is derived from persisted package source and repository
associations. The dependency graph is derived from persisted package
dependency records. Neither needs to be duplicated as another serialized map.

## Lock Validity

`lockfile_matches` determines whether a lock is an exact projection of the
current project inputs. Sync and status refuse to repair declaration/lock
drift.

A lock is drifted when any of these change:

- Project package name or version.
- Any resolution-relevant manifest relation, including its field or
  constraint.
- R version requirement.
- Additional repository declarations or their order.
- Default-repository policy recorded by the lock.
- Future Git URL, requested ref, or subdirectory.
- Exact current R version.
- Lockfile format or semantic resolution revision.

The semantic revision must cover changes to resolution policy such as base
package classification, relation interpretation, source normalization, or
provider selection.

The current environment's default repository URL does not invalidate an
otherwise valid lock during sync because the effective URL is already locked.

## Lock Projection

Lock generation is a pure projection of the manifest and completed resolution:

```text
Manifest + Resolution + R runtime + pinned sysreq result -> Lockfile
```

It must not:

- Query package repositories again.
- Parse package DESCRIPTION metadata again.
- Recover repository ownership from source URL prefixes.
- Recompute package dependencies through a separate transformation path.

## Locked Reconstruction

`resolve_locked` reconstructs the useful runtime resolution from a valid lock
and the already-loaded workspace manifest. Despite its name, it performs no
dependency solving and no repository metadata requests.

The manifest is required because the workspace package itself is not an
external locked package. Its identity and hard dependency edges come from the
current manifest after lock validity has been established.

## Sync Planning

Sync uses:

```text
Locked resolution + installed state + runtime + host state + execution policy
    -> Sync plan
```

The plan contains all decisions required by execution:

- Packages to remove.
- Packages and workspace source to install.
- Exact source and repository for each package.
- Dependency ordering or dependency-ready groups.
- Artifact selection inputs.
- Compiled and artifact cache identities.
- System dependency actions.

Execution applies the plan. It must not perform declaration parsing,
repository association, dependency graph construction, installed-package
discovery, or source selection.

## Runtime And Host Inputs

The R runtime snapshot should contain enough information to keep all R calls in
one command consistent:

- Resolved executable identity.
- Exact R version.
- R platform and package type.
- Available base packages.

The exact R version participates in lock validity. Platform, package type, OS,
architecture, and R minor version also influence artifact selection and cache
identity.

Host distribution, installed system packages, pinned sysreq rules, and system
installation flags influence the sync plan. TTY state, sudo availability, and
package-manager availability are execution conditions rather than lock
identity.

## Installed Source Identity

Installed package name and version are not sufficient once direct Git sources
exist. Different commits may declare the same package version.

Sync therefore needs an rpx-maintained installed source fingerprint, likely in
project-library state separate from the installed package DESCRIPTION. The
fingerprint must distinguish at least:

- Registry source identity.
- Git package tree identity.
- Workspace source state when workspace caching is introduced.

The same immutable source fingerprint participates in artifact and compiled
cache keys. Without it, a Git commit change that retains the R package version
would be incorrectly treated as already installed.

## Status

Status first validates the manifest, runtime, and lock relationship. It then
compares the reconstructed locked resolution with explicit installed, runtime,
and host snapshots.

Status computation must not hide additional filesystem, R, repository, or host
queries. Repository availability and credentials do not affect whether an
already-locked project is in sync.

## Modification Pipeline

Add/remove package and add/remove repository commands follow one pipeline:

1. Read the current manifest and optional lock once.
2. Capture the R runtime once.
3. Mutate the manifest in memory.
4. Derive effective repositories and eligible old-lock preferences.
5. Resolve.
6. Project the resolution into a new lock.
7. Read installed state once.
8. Build a complete sync plan.
9. Persist the new manifest and lock atomically as individual files.
10. Apply the sync plan.

If package installation fails after successful resolution, the new
`DESCRIPTION` and `rpx.lock` remain in place. They describe the desired state,
and a later `rpx sync` resumes convergence. The command must not claim a full
transactional rollback that cannot include system package changes, concurrent
R installs, or other external side effects.

Project and cache locking, temporary-file writes, and atomic file replacement
are still required to prevent concurrent commands or interrupted writes from
corrupting durable state.

## Sync Pipeline

Sync follows a deliberately different path:

1. Read the manifest and required lock once.
2. Capture the R runtime once.
3. Reject an incompatible or drifted lock.
4. Reconstruct the locked runtime resolution without solving.
5. Read installed and host state once.
6. Build a complete sync plan.
7. Apply that plan without changing the manifest or lock.

Sync never resolves around drift and never advances a moving Git reference.

## Command Migration Roadmap

The command-first migration uses the `modify_and_sync` pseudocode discussed
during implementation as inspiration, not as a literal Rust API. In
particular, an unconstrained `add` must perform asynchronous repository lookup
before it can finish mutating the manifest. Commands therefore prepare their
specific mutation and then enter shared resolution, persistence, and sync
layers.

### Completed Checkpoints

Phase 1 moved command-scoped project file access onto `Project`:

- Add, remove, repository, lock, sync, and status commands discover one
  `Project`.
- `DESCRIPTION` and `rpx.lock` reads are cached by that project.
- Optional lock reads and root-relative manifest and lock writes belong to the
  project.
- Superseded global manifest and lock readers and writers were removed.
- Optional-lock consumers validate a present lock before deciding whether to
  reuse it, and status validates its required lock. The consumer, not the error
  type or `Project`, decides whether a particular validation failure makes the
  lock unusable. Sync still has its older validation path pending Phase 11.

Phase 2 made the R version an explicit command input without introducing a
runtime snapshot structure:

- `r_version_async()` remains asynchronous and is called at most once by each
  command that needs the R version.
- The resulting `String` is passed to lock validation, lock generation, sync
  validation, R minor-version derivation, and compiled cache-key generation.
- `RuntimeInfo` and its deprecated platform/package-type probe were removed.
- Binary artifact selection continues to use `std::env::consts::OS` and
  `std::env::consts::ARCH`; the selected artifact supplies the exact R install
  type (`win.binary`, `mac.binary.*`, or `source`).
- Base-package inspection remains a separate cached R query.

### Phase 3: Effective Repositories

Derive the repository runtime set from the mutated manifest and the effective
default-repository decision. Do not reconstruct the runtime repository set
from the old lock.

The old lock supplies default-policy inheritance and resolution preferences.
The effective base repository is the built-in repository unless overridden by
`RPX_REGISTRY_BASE_URL`. Its presence or absence in the locked effective
repository list represents whether the base repository was enabled; no
additional policy field is planned.

Required behavior:

- No old lock defaults to the base repository being enabled.
- `--default-repo` enables the current effective base repository.
- `--no-default-repo` disables it, including an inherited base repository.
- With no CLI override, a usable old lock supplies the previous decision.
- A modifying resolution uses the current environment override.
- Sync uses exactly the repositories in its valid lock and ignores the current
  environment override.

### Phase 4: Lock Matching And Reuse

Make lock matching a side-effect-free comparison over explicit inputs:

```text
Lockfile + mutated manifest + effective repositories + R version
    -> matching or drifted
```

Use the strongest facts supported by the current schema: roots and
constraints, repository metadata and order, exact R version, schema
compatibility, and current workspace compatibility.

Consumer policy remains local:

- Status and sync reject drift.
- Lock and modifying commands repair drift by resolving a replacement lock.
- Older readable schemas may be replaced.
- Newer schemas and operational read/runtime failures are errors.

When the requested mutation leaves the resolution inputs unchanged, retain the
existing lock instead of resolving or rewriting it. A no-op modification still
synchronizes the project library against that unchanged lock.

### Phase 5: Source-Aware Preferences

Separate an old lock as a possible preference source from a lock that is valid
as the current runtime model.

```text
old_lock: possible source of eligible preferences
current_lock: validated lock that may be reused directly
```

Preferences must retain source ownership. Remove the fallback that associates
an unmatched locked package with the first remaining repository. Removing a
repository invalidates preferences supplied by that repository. A same-name,
same-version package from another repository must not silently satisfy a
source-specific preference.

### Phase 6: One Installed Snapshot And Package Sync Planning

Read installed R package state once after resolution or locked reconstruction.
Build explicit package actions from the installed and locked maps before
executing them:

- Packages to remove because they are extra or have the wrong version.
- Correct installed packages to retain.
- Locked packages that require installation.
- Workspace package reinstallation.
- Dependency ordering inputs.

Pass the retained installed names into installation execution. Remove the
second installed-package query currently hidden inside
`install_locked_packages`.

Artifact selection may remain in execution during this milestone. The eventual
complete sync plan will also own artifact and cache decisions.

### Phase 7: Shared Modification Orchestration

Introduce shared orchestration that consumes command-prepared values rather
than forcing every mutation into a synchronous closure:

```text
Project
+ original optional lock
+ mutated manifest
+ effective repositories
+ roots
+ preference exclusions
+ R version
    -> reused or regenerated lock
    -> installed snapshot
    -> system and package actions
    -> persisted desired state
    -> applied actions
```

The shared layer should:

1. Reuse or regenerate the lock.
2. Read installed state once.
3. Build package and system actions.
4. Write the manifest only when changed.
5. Write the lock only when changed.
6. Apply system actions.
7. Apply package actions.
8. Return reporting information to the command.

Desired manifest and lock state is persisted before external installation. If
execution fails, the desired state remains for a later sync.

### Phase 8: Package Add And Remove

Migrate add and remove onto the shared orchestration.

Add must resolve unconstrained package relations before finalizing the
manifest mutation, then derive roots from the mutated manifest. Remove mutates
the manifest first and derives roots from that result. Neither command should
perform an early installed-state read solely for reporting.

Duplicate add and missing remove requests reuse an unchanged lock when
possible, but still run sync so they can repair an out-of-sync library.

### Phase 9: Repository Add And Remove

Migrate repository mutations onto the same resolve, persist, and sync path.
Successful repository changes must synchronize immediately rather than only
rewriting project files.

Repository removal filters source-specific preferences before resolution.
Credential removal remains an explicit command-specific side effect with a
documented ordering relative to persistence.

Duplicate add and missing remove requests may retain their existing reporting,
but still synchronize against an unchanged valid lock.

### Phase 10: Lock Command

Reuse the project extraction, effective-repository, matching, preference, and
resolution layers without entering modification sync orchestration.

The lock command:

1. Reads the manifest and optional old lock once.
2. Uses the already-captured R version.
3. Derives effective repositories.
4. Retains an exactly matching lock.
5. Otherwise resolves and projects a replacement lock.
6. Writes only when changed.
7. Never reads installed package state or synchronizes.

### Phase 11: Sync Command

Sync uses a required valid lock and never resolves or persists project
declarations:

1. Read manifest and lock once.
2. Use the already-captured R version.
3. Reject schema, repository, requirement, workspace, and R-version drift.
4. Read installed package state once.
5. Build system and package actions.
6. Respect `--install-system` and `--install-only-system`.
7. Apply the actions without rewriting manifest or lock.

This intentionally replaces current behavior that warns on R-version drift or
allows repository declaration drift.

### Phase 12: Persistence And Cleanup

After command orchestration is unified:

- Remove superseded readers, matchers, validators, and command-specific
  orchestration helpers.
- Make individual manifest and lock writes use temporary files followed by
  atomic replacement.
- Write only files whose serialized state changed.
- Do not claim a transaction spanning R installation, system package changes,
  credentials, and other external side effects.
- Add command-level regression tests for no-op lock reuse, repair behavior,
  default-repository overrides, source-aware preferences, one installed-state
  read, and persistence-before-installation failure behavior.

### Settled Decisions

- Do not introduce a runtime snapshot struct merely to carry the R version,
  platform, or package type.
- Keep R version acquisition asynchronous.
- Use Rust OS and architecture constants for current binary selection and
  cache platform identity.
- Keep lock-acceptance policy in each consumer; do not add a generic
  `is_repairable` classification to validation errors.
- Repository add and remove eventually resolve and sync.
- No-op modifications eventually reuse the lock and still sync.
- Represent base-repository enablement through its presence in the locked
  effective repository list rather than a new Boolean lock field.
- Treat `modify_and_sync` as data-flow guidance rather than requiring one
  literal generic closure.
- Persist desired state before applying external package changes.
- Do not introduce a misleading cross-system `Project::transaction` API.

## Context Sources

The following sources materially influenced the roadmap above and should be
revisited after context compaction.

- The `modify_and_sync` pseudocode supplied during implementation established
  the target read, mutate, match-or-resolve, plan, persist, and apply ordering.
- `RESOLUTION_AND_SYNC_PLAN.md`, especially **Extract Once**, **Effective
  Repositories**, **Existing Lock Preferences**, **Lock Validity**,
  **Modification Pipeline**, and **Sync Pipeline**, supplies the intended
  invariants.
- `GIT_PACKAGE_SOURCES.md` supplies downstream provenance, source identity,
  and cache requirements that the registry-only migration must not block.
- `src/project.rs` supplies `Project`, cached manifest/lock reads,
  root-relative writes, locked package reconstruction, and strict lock
  validation.
- `src/lib.rs` command functions (`cmd_add`, `cmd_remove`, `cmd_repo_add`,
  `cmd_repo_remove`, `cmd_lock`, `cmd_sync`, and `cmd_status`) exposed the
  duplicated command pipelines and consumer-specific lock policies.
- `src/lib.rs` repository helpers and `DefaultRepositoryPreference` exposed
  the inherited-default and `--no-default-repo` behavior that Phase 3 must
  correct.
- `src/lib.rs` resolution and locking helpers (`lockfile_from_roots` and
  `lockfile_from_selected_versions`) exposed hidden metadata and runtime reads.
- `src/lib.rs` sync helpers (`validate_project_for_sync`,
  `sync_locked_project`, `install_locked_packages`, and artifact preparation)
  exposed repeated installed/runtime reads and interleaved planning and
  execution.
- `src/r.rs` supplies asynchronous R version and base-package queries, typed R
  subprocess errors, installed package extraction, and package installation.
- `src/lockfile.rs` defines the durable repository, root, package, dependency,
  R, and system-requirement data currently available to matching and sync.
- `src/resolver.rs` supplies `PackageVersion` and its version-based equality,
  which enabled typed installed/locked comparisons while highlighting missing
  source identity.
- `src/repository.rs` and its adapters define repository identity, metadata
  access, and source URL construction used by resolution and preference
  filtering.
- `src/cache.rs` shows that compiled cache identity already uses R version and
  Rust host platform constants, while still lacking immutable source identity.
- `src/sysreqs.rs` supplies the existing host discovery and
  `SystemDependencyPlan`, and shows where system input capture and execution
  remain coupled.
- `tests/deps.rs` covers add, remove, lock, constraints, base packages, and
  current-library exclusion from locking.
- `tests/status.rs` covers strict validation and aggregate installed-state
  mismatches; status is the closest current example of the intended command
  shape.
- `tests/sync.rs` covers lock immutability, exact-version restoration,
  extra-package removal, schema handling, and current behaviors that conflict
  with the target design, notably repository-drift acceptance.
- `tests/cli.rs` covers project discovery, initialization, command execution,
  and cleanup behavior across containerized command paths.

## Cache Invalidation

Cache identity is separate from lock validity. At minimum it includes:

- Package name and version.
- Immutable package source fingerprint.
- Artifact kind and identity.
- R runtime/ABI identity required by the cache format.
- OS, architecture, and target platform.
- A cache format revision.

Credentials and cache presence are not cache identity. Cache writes should use
temporary files or directories followed by atomic replacement so interrupted
downloads are never accepted merely because a destination path exists.

## Deferred Type Design

This plan intentionally does not settle:

- Whether the runtime model is one type or state-specific declared, resolved,
  and locked types.
- Whether relationships use typed IDs, indexes, references, maps, or another
  representation.
- Whether repositories and direct package sources share one enum or compose
  through narrower interfaces.
- The exact lockfile version that introduces structured source provenance.
- The storage format for installed source fingerprints.

Those choices should be made while implementing the smallest end-to-end
operations, with tests enforcing the data-flow and invalidation rules above.
