# Repository lifecycle and metadata authority

`PackageRepository` is an enum of shared CRAN, rrepo, Git, and local handles.
Concrete implementations expose native operations; resolution chooses how to
combine indexes, version endpoints, or single-package sources. They do not
construct solver candidates or implement a uniform repository trait.

## Protocol SDKs and client injection

`cran-sdk` and `rrepo-sdk` own endpoint construction, native response models,
parsing, and protocol errors. They borrow a `reqwest_middleware::ClientWithMiddleware`
per call. A plain reqwest client can be converted once; middleware clients retain
their complete chain and request initializers. SDK descriptors own only their URL.

The dependency direction is `rpx -> SDKs -> reqwest/middleware + metadata parsers`.
Neither SDK imports rpx or the other SDK. rpx's HTTP module configures the shared
client, authentication, HTTP tracing/progress, and URL display. The existing binary
target mapping remains shared there. SDK binary endpoints accept explicit native
platform/R-series values supplied by that mapping.

The rpx repository adapters retain Moka caches, source identity, and all existing
archive-support and fallback policy. Generic SDK errors preserve HTTP status and
parse findings; the adapters provide rpx-specific positioned diagnostics.
The CRAN SDK also exposes the latest web DESCRIPTION endpoint even though rpx's
resolver uses index or version-pinned source metadata instead.

SDK operation spans inherit caller tracing context and cover metadata parsing.
Injected middleware owns HTTP spans; rpx owns terminal UI and streamed-download
progress. SDKs configure no subscriber, runtime, cache, or authentication state.

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

## CRAN candidate discovery

Current PACKAGES membership is a fast path, not a prerequisite for archive
candidates. The resolver uses native directory listings when available. The
legacy `archive_support` field describes listing availability only: it does not
assert that known archive files are absent.

When directory listing is unavailable (including a denied per-package listing),
an eligible preferred version is probed at its current source URL and then its
archive URL. A successful source DESCRIPTION must identify the requested package
and version. The parsed description is shared through Moka with subsequent
dependency queries. Missing probes are also memoized for the current CLI run.

Only HTTP 404/410 from an artifact endpoint means absence. Authentication/access
failures, server failures, bad archives, and mismatched metadata propagate as
errors rather than silently selecting a newer version. An unavailable preferred
version falls back to the normal best eligible candidate; a preference excluded
by the version range is not probed. This discovers known preferred versions, not
an arbitrary history when the server offers no listing.

Source requests for metadata do not populate a new persistent metadata store.
Sync retains its existing artifact cache and binary-first download behavior.

## Compatibility

The lockfile wire format and artifact/installer cache versions are unchanged.
Repository dispatch no longer uses a second registry enum. `RegistryCacheKey` is
an opaque hash-only representation with explicit legacy kind tags, tested against
the previous cache-key encoding. The runtime repository enum is not hashed into
artifact cache keys.

Moka remains per-invocation memoization. Persistent HTTP caching, TTL handling,
and disk-cache garbage collection are deferred.
