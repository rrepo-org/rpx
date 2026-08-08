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

Status should consume one expected package/version projection instead of
special-casing the workspace package. Until locked resolution reconstruction
owns this operation, use a pure helper conceptually shaped as:

```text
expected_package_versions(manifest, lockfile) -> package/version map
```

The projection contains every non-base locked package plus the package and
version declared by the workspace manifest. The manifest entry replaces a
legacy lock entry with the same package name. Its keys drive missing and extra
package checks, and its values drive one generic installed-version mismatch
check for both workspace and external packages.

The path-only `Project` abstraction must not own this operation. It derives
from already-loaded documents and will eventually belong to the reconstructed
locked resolution.

As part of that migration, remove status's legacy
`lockfile_supports_project` check. Whether the workspace package was
incorrectly serialized among external packages is repaired by relocking and
does not require a dedicated status validation path. Sync will retain any
structural validation needed before applying a lock.

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
