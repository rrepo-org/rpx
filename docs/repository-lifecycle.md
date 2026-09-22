# Repository lifecycle and metadata authority

`PackageRepository` is an enum of shared CRAN, rrepo, Git, and local handles.
Concrete implementations expose native operations; resolution chooses how to
combine indexes, version endpoints, or single-package sources. They do not
construct solver candidates or implement a uniform repository trait.

## Ownership

- Discovery loads the native index into the returned repository's Moka cache.
  Repeated HTTP repository URLs in one configuration load share discovery and
  handles while retaining their precedence positions.
- Cloning preserves the shared caches, Git commit cell, and local DESCRIPTION
  overrides. Configuration equality is not metadata snapshot identity.
- Locked replay reconstructs each repository record once and shares it among
  packages. The existing first-matching-URL lockfile semantics are preserved.
- Git configuration validation compares normalized fields without creating caches.
- Local overrides remain authoritative for metadata reads. The same path can have
  different staged descriptions; local sources are not interned by path.

## Fresh resolution versus locked replay

`rpx lock` resolves from current repository metadata. Previous locked versions are
preferences, not hard constraints, and previous locked dependencies never seed
the resolver's metadata state. Editing a version in the lock and running `lock`
therefore refreshes its dependency graph from the actual selected package.

The solver retains validated declared dependencies in strong per-resolution
state keyed by source slot, package name, and version. `PackageVersion` equality
continues to compare versions only for solver ranges; it is not a metadata key.
`ResolvedPackage` carries the selected source/version and dependency relations.
The result is assembled before the provider is dropped, without post-solve
hydration or a dependency on repository-cache retention.

`rpx sync` instead trusts the locked versions and relations after configuration
validation. It constructs package records directly, without synthesizing a
DESCRIPTION or requesting repository metadata. Other commands retain their
existing explicit choice between resolution and valid-lock replay.

Declared Imports/Depends/LinkingTo relations are retained for lockfile output.
The solver filters runtime dependencies when deriving constraints; sync filters
R when building task edges. Root resolution requirements retain their distinct
project policy, including Suggests. Git repository serialization still finalizes
configured commits, including unused configured Git sources as before.

## Compatibility

The lockfile wire format and artifact/installer cache versions are unchanged.
Repository dispatch no longer uses a second registry enum. `RegistryCacheKey` is
an opaque hash-only representation with explicit legacy kind tags, tested against
the previous cache-key encoding. The runtime repository enum is not hashed into
artifact cache keys.

Moka remains per-invocation memoization. Persistent HTTP caching, TTL handling,
and disk-cache garbage collection are deferred.
