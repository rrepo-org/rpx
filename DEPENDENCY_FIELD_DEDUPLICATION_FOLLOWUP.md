# Dependency Field Deduplication Follow-Up

Refactor all DESCRIPTION dependency-field modifiers to normalize relations through `BTreeSet<Relation>` before writing them back.

This should apply consistently to every modifier of:

- `Depends`
- `Imports`
- `LinkingTo`
- `Suggests`

The goal is to prevent duplicate relations from surviving or being introduced when adding, removing, moving, or replacing dependencies. Each modifier should parse the complete affected fields first, preserve the existing all-or-nothing error behavior, collect the resulting relations into `BTreeSet`, perform its mutation, and pass the deduplicated ordered set to the corresponding DESCRIPTION setter.

Keep `Enhances` outside this refactor unless its command semantics explicitly change.

Review at least these paths when implementing the follow-up:

- `description::add_dependencies`
- `description::remove_dependencies`
- Deferred post-resolution pinning described in `ADD_PINNING_FOLLOWUP.md`
- Any future command or helper that rewrites dependency fields

Deduplication must remain semantic: relations with different version requirements are distinct and may intentionally coexist for the same package.
