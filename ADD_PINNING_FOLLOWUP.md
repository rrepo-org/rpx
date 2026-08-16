# Deferred Add Pinning

`rpx add PACKAGE` currently records an unconstrained relation before resolution. We want to prevent later downgrades and automatic major-version upgrades, but must not constrain the initial resolution based on a repository's latest version because that version may be incompatible with the rest of the project.

## Intended behavior

- Track packages supplied without an explicit `@...` constraint.
- Add those packages as unconstrained relations for the initial resolution.
- Resolve the complete project normally.
- For each unconstrained requested package, use its selected version to replace the unconstrained DESCRIPTION relation with both:
  - `>= selected-version`
  - `< next-major-version`
- Preserve user-supplied constraints exactly as entered.
- Leave unresolved base packages unconstrained unless a reliable selected version is available.
- Recompute project requirements after updating DESCRIPTION.
- Store those final requirements in `rpx.lock` so DESCRIPTION and the lock validate immediately.
- Do not restore the old pre-resolution repository-index scan. Package availability must be decided by the resolver.

## Implementation notes

The DESCRIPTION update must happen after every resolution/reuse branch has produced the selected package map and before DESCRIPTION and the lockfile are written. The selected packages already contain the versions needed to construct the bounds.

The old `pinned_package_relations` and package-suggestion path was removed during the command migration because it queried repository indexes and chose constraints before compatibility was known. Any future typo-suggestion behavior should be designed independently around resolver failures.
