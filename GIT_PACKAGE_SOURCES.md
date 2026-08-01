# Git Package Source Plan

## Goal

Add direct Git package sources to rpx without requiring a `git` executable.
A direct source binds one R package to one Git repository and requested ref.
Locking resolves that request to immutable Git objects, and syncing installs
those exact objects.

The first implementation is not a tag-based package repository. The design
should leave room for a future mode that enumerates tags as package versions,
but no tag-resolution flag or user interface will be added now.

## Settled Decisions

- Implement direct package sources, not a global Git package universe.
- Use the `gix` crate instead of invoking a Git binary.
- Keep the active workspace package separate from external Git packages.
- Record user intent in `DESCRIPTION` and immutable resolution in `rpx.lock`.
- Resolve moving refs only while locking. Sync never resolves a branch or tag.
- A configured Git source overrides registry candidates for that package.
- A Git commit or tree must participate in artifact and compiled-cache keys.
- Do not block this feature on a general repository-identity redesign.
- When a locked Git package must be associated with declared Git repository
  metadata, select the longest valid normalized URL-prefix match.

## User Model

Use the conventional R `Remotes` field for direct sources:

```text
Imports:
    mypackage
Remotes:
    git::https://github.com/example/mypackage.git@main
```

The initial CLI should accept the same source form:

```sh
rpx add git::https://github.com/example/mypackage.git@main
```

The command resolves the requested ref, reads the package's `DESCRIPTION`,
discovers `Package` and `Version`, adds the package to `Imports`, and records
the source in `Remotes`.

The source model should contain:

```rust
struct GitSourceSpec {
    url: String,
    requested_ref: Option<String>,
    subdirectory: Option<String>,
}
```

`subdirectory` keeps monorepo support possible. Whether it is exposed in the
first CLI can be decided during implementation.

The `r-description-scalerail` dependency currently has no typed lossless
accessors for `Remotes`. Prefer adding `remotes()` and `set_remotes()` there
rather than editing serialized `DESCRIPTION` text in rpx.

## Parallel With LocalRepository

`LocalRepository` is the useful implementation precedent:

- It represents one package and one version.
- It parses and retains a `DESCRIPTION` snapshot for one command.
- It implements `PackageRepository` so PubGrub can consume package metadata.

A direct Git adapter should follow that shape after resolving a ref:

```rust
struct GitRepository {
    source: GitSourceSpec,
    commit: ObjectId,
    tree: ObjectId,
    package: String,
    version: Version,
    description: Arc<RDescription>,
}
```

Unlike the workspace `LocalRepository`, a Git package is external and must be
user-visible and locked. It must not be filtered from the resolution result or
installed from the current workspace.

Keep ref resolution separate from the one-snapshot adapter. A future
tag-backed implementation can reuse fetching, tree inspection, DESCRIPTION
parsing, and source materialization while exposing several snapshots through
`packages()` and `versions()`.

## Git Object Cache

Use `gix` to maintain a shared bare object cache keyed by normalized remote
identity. The cache should not be tied to a temporary checkout path.

Locking should:

1. Open or initialize the cached repository.
2. Fetch the requested ref or default branch.
3. Peel annotated tags to a commit.
4. Resolve abbreviated revisions to a full commit object ID.
5. Read `DESCRIPTION` from the selected tree and optional subdirectory.
6. Validate `Package` and `Version`.
7. Retain the parsed DESCRIPTION for the duration of resolution.

Sync should first use cached objects. If the locked commit is absent, it may
fetch that exact commit from the remote. If neither cache nor remote can
provide it, sync must fail without falling back to the current branch head.

## Resolution

Direct Git sources need a package-specific source override map:

```rust
BTreeMap<String, Arc<dyn PackageRepository>>
```

For an overridden package, version selection queries only its Git adapter. A
registry package with the same name and version must not satisfy that package.
Dependencies declared by the Git package continue resolving from normal
repositories unless they have their own direct source override.

PubGrub ranges continue comparing R package versions. Git source identity is
not part of version ordering. The selected package must nevertheless retain
its exact commit/tree as source provenance through lockfile generation.

The current `PackageVersion` equality intentionally ignores its repository.
That does not prevent direct sources as long as source overrides select one
deterministic provider for a package before candidates are compared.

## Existing Lock Reuse

Conservative lock behavior should apply to Git sources:

- If the declared URL/ref/subdirectory is unchanged, ordinary `rpx lock`
  retains the existing locked commit.
- Initial addition resolves the requested ref.
- Re-adding the same direct Git source explicitly refreshes the moving ref.
- Changing the declared Git source makes the lock stale.
- Deleting the lock causes all moving refs to resolve again.

This avoids silently advancing branches during an unrelated lock operation.
A broader update command can replace the re-add behavior later.

## Lockfile

A locked Git package needs, at minimum:

- Clone URL.
- Requested ref, if one was supplied.
- Full commit object ID.
- Tree object ID for the package source.
- Optional package subdirectory.
- DESCRIPTION package name and version through the existing package fields.

The preferred shape is a structured package source rather than encoding Git
coordinates into `source_url`:

```json
{
  "package": "mypackage",
  "version": "1.2.3",
  "source": {
    "kind": "git",
    "url": "https://github.com/example/mypackage.git",
    "requested_ref": "main",
    "commit": "0123456789abcdef0123456789abcdef01234567",
    "tree": "89abcdef0123456789abcdef0123456789abcdef"
  }
}
```

This likely requires a lockfile-version bump. Registry source migration can be
kept minimal; direct Git support does not require solving the general registry
repository-association problem first.

Credentials must never be written to `DESCRIPTION` or `rpx.lock`.

## Matching Declared Git Repositories

If the locked source stores the same normalized clone URL as the declaration,
use exact equality. If package source URLs extend a declared Git repository
URL, associate them as follows:

1. Normalize both URLs.
2. Require equal scheme, host, and port.
3. Require a path-segment-boundary prefix.
4. Select the declaration with the longest matching path.
5. Reject equal-length ambiguous matches.

This matching exists only to recover declared Git repository metadata. It is
not registry artifact routing and does not need to account for CDN URLs.

## Sync And Installation

The sync refactor separates project loading, system dependencies, and R package
installation. Git installation should integrate with `sync_locked_project`,
which receives the already-loaded DESCRIPTION and lockfile.

For a locked Git source, sync should:

1. Locate or fetch the exact commit.
2. Verify the commit and package tree object IDs.
3. Materialize the package tree into a temporary source directory.
4. Install it through the existing source-package installation path.
5. Never inspect the requested branch/tag to choose installation content.

Git packages have no registry binary lookup in the first implementation.

## Cache Identity

Current artifact and compiled caches are primarily keyed by package/version.
That is unsafe for Git because multiple commits can declare the same package
version.

Add an immutable source fingerprint to cache keys:

- Git packages use the package tree object ID.
- Registry packages can initially use their locked source identity.

Different Git commits with identical `Package` and `Version` values must not
share a compiled cache entry unless their package tree object IDs are equal.

## Validation

Reject a Git source when:

- The requested ref cannot be resolved.
- The resolved object is not a commit after peeling.
- The package subdirectory is absolute, escapes the tree, or is otherwise
  invalid.
- `DESCRIPTION` is missing or invalid.
- `Package` or `Version` is missing or invalid.
- Two direct sources claim the same package.
- The discovered package is not declared as a project dependency.
- The locked commit/tree cannot be reproduced during sync.

Initial policy should reject or explicitly not support submodules and Git LFS
rather than silently installing incomplete content.

## Implementation Sequence

1. Add typed lossless `Remotes` support to `r-description-scalerail`.
2. Add `gix` and implement remote/ref parsing plus the bare object cache.
3. Implement exact-commit DESCRIPTION loading and the single-package Git
   repository adapter.
4. Build direct-source overrides and feed them into resolution.
5. Persist requested and resolved Git source data in the lockfile.
6. Materialize and install locked Git trees during sync.
7. Include source fingerprints in artifact and compiled-cache keys.
8. Add CLI and documentation for direct Git additions and refresh behavior.

## Required Tests

- Parse and round-trip supported `Remotes` entries.
- Resolve default branch, named branch, lightweight tag, annotated tag, full
  commit, and abbreviated commit.
- Reject malformed refs and invalid DESCRIPTION metadata.
- Prove a direct source overrides a registry package with the same name and
  version.
- Prove ordinary locking retains the existing commit after a branch advances.
- Prove explicitly refreshing the source advances the locked commit.
- Prove sync installs the locked commit after the declared branch advances.
- Exercise unavailable commits with and without cached Git objects.
- Prove commits with the same package version do not collide in caches.
- Test optional subdirectory normalization and traversal rejection.
- Test longest valid repository URL matching and ambiguous matches.
- Run installation tests without a `git` executable available.

## Deferred Decisions And Non-Goals

- Tag enumeration as a package-version universe is deferred.
- No tag-resolution CLI flag is added in the first implementation.
- A general registry repository-identity redesign is not required.
- Private Git authentication details need a separate transport/credential
  decision. `gix` must remain the Git implementation, with no Git binary
  fallback.
- Signed commit/tag verification is not part of the first implementation.
- Submodule and Git LFS support are not part of the first implementation.
