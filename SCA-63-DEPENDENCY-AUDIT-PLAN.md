# SCA-63: Dependency Discovery and Pruning

## Goal

Add an `rpx audit` command that scans an R package's library source, compares
statically discovered package usage with `DESCRIPTION`, and can add missing or
remove apparently unused dependencies.

The first version should be deliberately narrow. It should use `r-syntax-rs`
directly inside `rpx` rather than introduce a separate dependency-discovery
crate or a generalized project scanner.

## Scope

Scan `.R` files recursively under the package's `R/` directory and detect:

- `library(pkg)`
- `library("pkg")`
- `library(package = pkg)`
- `library(package = "pkg")`
- `pkg::function`
- `pkg:::function`

Also read `NAMESPACE` as a pruning safeguard. Packages referenced by these
directives are considered used even if calls in `R/` are unqualified:

- `import(pkg)`
- `importFrom(pkg, function)`
- `importClassesFrom(pkg, class)`
- `importMethodsFrom(pkg, method)`

The first version does not scan tests, vignettes, examples, roxygen comments,
R Markdown, Quarto, notebooks, development scripts, native source, or build
configuration.

## Parser

Use the crates from <https://github.com/rrepo-org/r-syntax-rs> through Git
dependencies following `main` during development:

- `r-parser`
- `r-syntax`

`Cargo.lock` will record the exact resolved revision. Move to released crate
versions or a tagged revision before releasing `rpx` if they are available.

Parse each file independently with `r_parser::parse_source`. Walk the Rowan CST
using `SyntaxKind` and the typed AST wrappers where useful:

- inspect `NAMESPACE_EXPR` nodes for `::` and `:::` usage;
- inspect `CALL_EXPR` nodes whose callee is the identifier `library`;
- accept only a static identifier or string as the package argument;
- retain the package name, file path, and source range as evidence.

Do not use a regex fallback when parsing fails. A valid R construct that the
parser mishandles should become a reduced regression case and be fixed in
`r-syntax-rs`.

## Audit Model

Aggregate discoveries by package name in deterministic order. Each discovered
package should retain at least one source location for user-facing output.

Compare the discovered set with the dependencies currently declared in
`Depends`, `Imports`, `LinkingTo`, and `Suggests`:

- a discovered but undeclared package is missing;
- a declared `Imports` package with no discovered or NAMESPACE evidence is an
  apparent pruning candidate;
- preserve an existing dependency's field and version constraints when it is
  still used;
- add newly discovered packages to `Imports`;
- exclude the project package itself and `base`;
- select pruning candidates from `Imports`, then use the existing dependency
  removal semantics to remove the selected package from all managed fields;
- do not manage `Enhances` in the first version.

Selecting pruning candidates only from `Imports` avoids making claims about
optional, attachment, native-linking, and unmanaged dependencies that this
narrow scanner cannot verify. Removing a selected package from all managed
fields is deliberate and matches `rpx remove`.

## CLI Behavior

Add these forms:

```text
rpx audit
rpx audit --add
rpx audit --prune
rpx audit --add --prune
```

Interactive behavior:

- print missing dependencies and apparent pruning candidates;
- ask whether to add the missing dependencies;
- ask separately whether to remove the pruning candidates;
- explicit flags skip their corresponding prompt;
- finding mismatches alone does not cause an interactive invocation to fail.

Non-interactive behavior:

- without modification flags, print mismatches and exit nonzero like
  `rpx status`;
- `--add` and `--prune` perform deterministic modifications without prompting;
- report and exit nonzero if unrequested mismatches remain;
- use both flags to fully reconcile the supported dependency set.

`rpx audit` must not install or synchronize packages.

## Safety

Mark the scan incomplete if any source file cannot be read or has an
`Incomplete` or `Invalid` parse status, or if parser diagnostics or resource
limits indicate that source may have been skipped.

`NAMESPACE` is required for an installable R package. If it is missing, fail
explicitly instead of treating it as an empty source of import evidence.

For an incomplete scan:

- report parser and file errors with locations;
- report package usages that were still found;
- allow safe additions based on positive evidence;
- disable all pruning;
- exit nonzero in non-interactive mode.

If the requested DESCRIPTION changes cannot be resolved, leave DESCRIPTION and
`rpx.lock` unchanged. After successful reconciliation, resolve dependencies and
write DESCRIPTION and `rpx.lock` through the existing project write path. Pin
new unconstrained dependencies using the same policy as `rpx add`.

## Implementation Steps

1. Add Git dependencies on `r-parser` and `r-syntax`.
2. Add `AuditArgs` and `Commands::Audit` in `src/cli.rs`.
3. Add `src/commands/audit.rs` and wire it through `src/commands/mod.rs` and
   `src/lib.rs`.
4. Discover `.R` files below `R/` in deterministic path order.
5. Parse each file and collect `library()` and namespace-qualified package
   evidence.
6. Parse `NAMESPACE` and collect supported import-directive package evidence.
7. Compare discoveries with the existing DESCRIPTION dependency fields.
8. Implement terminal reporting, interactive prompts, flags, and exit behavior.
9. Reuse DESCRIPTION mutation, dependency resolution, pinning, and project file
   writing facilities.
10. Document `rpx audit` and the existing-project workflow in `README.md`.

## Verification

Add focused parser and command tests for:

- quoted, unquoted, named, and multiline `library()` calls;
- `::` and `:::` expressions;
- comments and strings that resemble dependency usage;
- NAMESPACE imports protecting unqualified dependencies from pruning;
- missing dependency reporting and addition;
- confirmed and flag-driven pruning;
- malformed source disabling pruning;
- preservation of existing fields and version constraints;
- no-op audits leaving DESCRIPTION byte-for-byte unchanged;
- audit modifications updating `rpx.lock` without installing packages.

Run formatting, unit tests, Clippy, and the relevant Docker integration tests
before completion.
