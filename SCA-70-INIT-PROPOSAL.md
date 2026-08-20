# Proposal: Production-Ready `rpx init`

Status: Draft for discussion

Related issue: SCA-70, "fix: make init generated packages installable"

Date: 2026-08-19

## Summary

`rpx sync` now installs the project root as a local R package. This makes the
output of `rpx init` part of the installation contract: a newly initialized
project must be a valid, installable R source package, not only a DESCRIPTION
file that is sufficient for dependency resolution.

The immediate production fix for this requirement partly exists already.
`rpx init` currently generates a DESCRIPTION with the metadata needed by R and
creates a NAMESPACE file. However, this behavior has not yet been designed as a
complete initialization experience or documented as an explicit product
contract.

This document explores a broader `rpx init` design. It is a proposal, not a
record of settled decisions. In particular, the execution model, interactive
questions, generated files, license support, and `rpx run` behavior all remain
open to revision.

## Goals

- Generate an R package that can be installed immediately with `R CMD INSTALL`.
- Generate a useful starting point rather than a collection of inconsistent
  placeholders.
- Make the package-first nature of rpx explicit.
- Preserve deterministic, scriptable initialization through CLI flags.
- Offer guided initialization when attached to an interactive terminal.
- Determine whether a newly generated package should demonstrate executable
  behavior that can be tested through rpx.
- Keep generated development files out of package tarballs.
- Avoid package load-time and installation-time side effects.
- Leave room for breaking changes before rpx 2.0.

## Non-Goals

These are tentative non-goals for this proposal, not permanent exclusions.

- Replacing DESCRIPTION with a second general-purpose project manifest.
- Supporting non-package projects in the same implementation.
- Generating a complete CRAN submission workflow.
- Generating testthat, roxygen2, pkgdown, or CI configuration by default.
- Managing or installing R itself.
- Defining a general cross-package command registration standard for the R
  ecosystem.

## Background

### Current rpx model

rpx uses DESCRIPTION as the project's dependency declaration and compatibility
contract. It resolves the root package together with its dependencies, excludes
the root from `rpx.lock`, reconstructs the root as a local repository during
sync, and installs it into the project library.

This differs from most R environment managers. Tools such as renv and rv create
isolated dependency environments without requiring the project itself to be an
installable package. rpx now treats the root as part of the installed package
graph.

The package-first model has useful properties:

- DESCRIPTION remains the single dependency declaration.
- The root can participate in dependency resolution.
- Reverse dependencies can constrain the root package version.
- Package dependencies are installed before the root.
- Analysis projects can follow the "package within" methodology.

It also imposes a requirement that all initialized projects satisfy R's package
structure and metadata rules.

### Current implementation

At the time of writing:

- `src/commands/init.rs::run` initializes a missing or empty target directory.
- `rpx init [PATH]` defaults to the current directory and derives the package
  name from the target unless `--name` is supplied.
- Init accepts direct title, description, and license overrides.
- `src/description.rs::initial_description` creates package metadata.
- `rpx init` writes DESCRIPTION, NAMESPACE, a production `.Rbuildignore`, and
  an initial `rpx.lock` resolved for the active R version.
- Init synchronizes the project library from that resolution before completing.
- Interactive init can initialize a Git repository and write GitHub's R
  `.gitignore` template.
- Init rejects any existing target that contains a file or directory, including
  hidden entries.
- `rpx sync` inserts the root as a local required package.
- Local packages are always considered reinstallable because their contents can
  change without a version change.
- Root installation is delegated to `R CMD INSTALL`.

The production changes that create NAMESPACE and provide a creator email were
introduced as part of commit `010d722` ("feat: resolve and install project
package"). SCA-70 should decide whether those incidental changes are the full
contract or only the start of a more deliberate design.

## What Constitutes an R Package?

The R Core "Writing R Extensions" manual defines an R source package as a
directory containing DESCRIPTION and NAMESPACE, with optional standard
directories such as R, data, demo, exec, inst, man, src, tests, tools, and
vignettes. Optional standard directories may be absent and generally should not
be empty.

The effective mandatory DESCRIPTION metadata is:

- Package
- Version
- Title
- Description
- License
- Author
- Maintainer

Author and Maintainer may be generated from a valid Authors@R field. A creator
with role `cre` needs a valid email address.

Package-name requirements include:

- ASCII letters, digits, and dots only.
- At least two characters.
- Starts with a letter.
- Does not end with a dot.

The current rpx derivation validates the starting character but does not enforce
the two-character minimum.

Installation requirements can extend beyond package structure:

- Required package dependencies must be available at acceptable versions.
- Packages containing native code may require compilers and system libraries.
- Configure scripts and installation hooks must succeed.
- R code must parse and be suitable for lazy loading.
- The namespace must parse and its imports must be satisfiable.

`R CMD INSTALL` is the authoritative test for installability. `R CMD check` is a
broader quality and distribution check and can report problems that do not
prevent installation.

## Prior Art

### renv

`renv::init()` creates project infrastructure, a project library, activation
through `.Rprofile`, and a lockfile. It discovers and installs dependencies, but
does not turn a generic project into an R package.

For existing package projects, renv adds its own infrastructure to
`.Rbuildignore` so that environment-management files do not enter the package
tarball.

### rv

`rv init` creates an `rproject.toml` manifest, an isolated project library,
activation scripts, and ignore rules. It records an R version and repositories.
It does not generate or install a root R package.

### uvr

`uvr init` creates `uvr.toml`, a project library, activation scripts,
`.Rprofile`, ignore rules, and IDE configuration. If DESCRIPTION exists, uvr
imports package metadata and dependencies. It distinguishes actual R package
source trees from non-package projects when deciding whether to update
`.Rbuildignore`.

### pak

pak is primarily an installer and dependency solver, not a project initializer.
`pak::local_install()` installs a local package tree and its dependencies.
`pak::local_deps()` and related functions read dependency metadata from a local
package tree.

### rig

rig installs, removes, and selects R versions. It also sets up package libraries
and pak, but does not initialize project source trees.

### usethis

`usethis::create_package()` is the closest R-specific scaffolding precedent. It
creates:

- DESCRIPTION
- NAMESPACE
- An R directory
- Optional RStudio project files and related ignore entries

Its DESCRIPTION defaults are intentionally placeholders and may not represent a
finished package. It validates all documented R package-name rules.

### uv

Python's uv separates packaged applications, packaged libraries, non-package
applications, and bare projects. Packaged applications and libraries define a
build system and can be installed into the project environment. Applications
receive a generated command entrypoint.

The distinction is useful prior art if rpx later supports non-package projects.
For the current architecture, however, adding a non-package mode would require
a second manifest and conditional root-installation behavior across much of the
codebase.

## Project Model Options

### Option A: Package-only projects

Every rpx project is an R source package. `rpx init` creates a valid package and
`rpx sync` installs the root.

Advantages:

- Matches the current architecture.
- Keeps DESCRIPTION authoritative.
- Avoids duplicate dependency declarations.
- Supports root participation in resolution.
- Makes root installation unsurprising.

Disadvantages:

- Analysis and script users must accept package structure.
- The CLI must clearly say "package project", not only "R project".

This is the leading direction in this proposal, but it is not yet a settled
decision.

### Option B: Package and non-package projects

Initialization could offer package, application, and bare project modes. A
non-package mode would need a dedicated manifest such as `rpx.toml`, and sync
would not install the root.

Advantages:

- Better conceptual fit for scripts, analyses, Quarto projects, and
  applications that are not packages.
- Similar to uv's explicit project types.

Disadvantages:

- Requires two manifest formats or a migration away from DESCRIPTION.
- Requires conditional behavior in add, remove, lock, sync, status, repository
  management, project discovery, and validation.
- Weakens the current DESCRIPTION-first product model.

### Option C: Synthetic root package

rpx could generate a temporary package internally for non-package projects.

This is likely the least attractive option because it would obscure dependency
ownership, produce confusing installed metadata, complicate root constraints,
and make installation failures harder to explain.

## Possible Generated Package

The minimum useful scaffold and any executable example should be decided
separately. A package needs DESCRIPTION and NAMESPACE to establish its package
structure, but does not need an R directory, an entrypoint, or a script merely
to be installable.

A metadata-only scaffold could be:

```text
DESCRIPTION
NAMESPACE
.Rbuildignore
rpx.lock
LICENSE
LICENSE.md
```

If init should also demonstrate package code, an idiomatic example would use a
file named after the function or concept it contains:

```text
R/
└── hello.R
```

R packages have no default source filename analogous to `lib.R` or `main.R`.
All R files under R are loaded into one package namespace, in collation order.
`R/hello.R` is therefore more idiomatic for a `hello()` function than a
generic `R/lib.R`. A package-name file is also common for package-level
documentation, but it is not an execution entrypoint.

If init should demonstrate a runnable orchestration script as well, possible
locations include:

```text
scripts/main.R
script/main.R
exec/main.R
inst/scripts/main.R
```

These locations have different distribution semantics and no location has
been selected. A custom top-level scripts directory would normally need an
exact `.Rbuildignore` entry. The standard exec directory is installed and its
files are marked executable, but its contents are not automatically added to
PATH. Files under inst are copied to the top level of the installed package.

Possible DESCRIPTION:

```text
Package: my.package
Title: My Package
Version: 0.1.0
Authors@R: person("First", "Last", email = "you@example.com",
    role = c("aut", "cre"))
Description: Add a package description.
License: MIT + file LICENSE
```

Open questions include:

- Whether `0.1.0` or `0.0.0.9000` is the better initial version.
- Whether Encoding should be generated. R Core advises against specifying an
  encoding unless it is needed, while usethis defaults to UTF-8.
- Whether MIT should remain the non-interactive default.
- Whether LICENSE and LICENSE.md should always be created.
- Whether an example function, runnable script, package entrypoint, or no
  executable example should be generated by default.
- Whether the entrypoint metadata belongs in DESCRIPTION or should be a fixed
  convention.

### Authorship consistency

The selected source metadata uses Authors@R with one valid `aut`/`cre` person.
Init also emits matching Author and Maintainer fields because `R CMD check` does
not derive them when checking a source directory directly. All three fields are
rendered from the same resolved identity to avoid contradictory metadata. Init
resolves the creator name and email from explicit options, effective Git
identity, or stable placeholders, in that order. A complete Git display name is
preserved as the person's `given` value rather than being split according to
Western name conventions.

## Execution Model

### Motivation

Generating executable behavior could:

- Keep the R directory non-empty.
- Give a new project a concrete starting point.
- Make root installation exercise R code loading, not only metadata parsing.
- Provide a direct integration-test target for `rpx run`.
- Offer an application-oriented workflow while retaining valid package
  structure.

R does not define a package entrypoint equivalent to Python console scripts or
Rust binaries. It does separately define installed package namespaces,
Rscript files, and package exec files. Any convention that combines these into
an rpx project runner would be rpx-specific.

No execution approach in this section is selected. In particular, it remains
open whether init should generate executable behavior at all.

### Option: R file with a `__main__` guard

R exposes the current evaluation-frame depth through `sys.nframe()`. Direct
execution by Rscript evaluates top-level code at frame zero. `source()`, package
installation, and package-development loaders evaluate the file through other
frames.

A generated file could use:

```r
main <- function(args = commandArgs(trailingOnly = TRUE)) {
  cat("Hello from my.package!\n")
  invisible(0L)
}

if (sys.nframe() == 0L) {
  main()
}
```

Expected behavior:

| Invocation | Automatic execution |
| --- | --- |
| `Rscript R/main.R` | Yes |
| `source("R/main.R")` | No |
| `devtools::load_all()` | No |
| `R CMD INSTALL` | No |
| Loading the installed package | No |

This pattern is used in current R tooling, including the R driver scripts in
`r-lib/ir`.

### Limitation of direct source execution

Direct execution loads only `R/main.R`. It does not construct the package
namespace from every file under R. If `main()` later calls a helper defined in
`R/helpers.R`, direct execution can fail even though the installed package works.

For example:

```text
R/
├── helpers.R
└── main.R
```

`Rscript R/main.R` does not automatically evaluate `helpers.R`. Automatically
sourcing all R files would poorly reproduce package collation, namespace,
imports, lazy loading, and installation behavior.

Therefore, the guard may be a useful script fallback and familiar convention,
but it is not sufficient as a package-aware runner. R documents the meaning of
`sys.nframe()` but does not define this use as a formal package-entrypoint
protocol. Supporting both direct execution and installed-package execution
would create two execution models that can diverge as a project grows.

### Option: thin orchestration script over the installed package

The "package within" workflow keeps analyses and operational scripts while
moving reusable data and logic into an R package. A generated example could
make that boundary explicit:

```r
library(my.package)

main <- function(args) {
  cat(hello(), "\n")
}

main(commandArgs(trailingOnly = TRUE))
```

with package code in `R/hello.R`:

```r
hello <- function() {
  "Hello, world!"
}
```

Loading the root package loads its complete installed namespace, including all
R files, declared imports, lazy-loaded objects, and native code. The script can
then remain an orchestration layer for runtime inputs, outputs, and side
effects, while reusable implementation stays under R.

This example also creates package-API obligations. For the unqualified
`hello()` call to work after `library(my.package)`, `hello` must be exported in
NAMESPACE. A clean `R CMD check` then requires documentation, such as a
`man/hello.Rd` file. Calling an internal function with `:::` or
`getFromNamespace()` would avoid an export but would demonstrate a weaker
package convention.

Ideally, project scripts would attach only the root package and call its
exported API. Direct use of many third-party packages in scripts can recreate
ad hoc dependency and helper-loading patterns. However, R package metadata has
no dedicated field for dependencies used only by project-local scripts.
Listing such packages in Imports makes them hard installation dependencies but
can produce an unused-import check note if package code never references them;
Suggests does not guarantee installation. This is another reason to keep
dependency-using implementation in package functions, but whether rpx should
enforce or merely demonstrate this convention remains open.

Script location also remains open:

- A project-local scripts directory is clear for analyses and operations, but
  is not a standard package directory and should usually be excluded from the
  package build.
- exec is a standard package directory for executable scripts and is installed,
  but does not expose commands on PATH.
- inst can distribute arbitrary scripts, but source and installed paths differ.
- tools is intended for package configuration and developer maintenance tasks,
  not general application execution.

### Option: invoke a function from the installed namespace

rpx could launch Rscript with a small driver that explicitly selects the
project library, loads the installed root namespace, retrieves a named
function, and calls it:

```r
library_path <- Sys.getenv("RPX_PROJECT_LIBRARY")
.libPaths(c(library_path, .libPaths()))

namespace <- loadNamespace(
  Sys.getenv("RPX_PACKAGE"),
  lib.loc = library_path
)
entrypoint <- get(
  Sys.getenv("RPX_ENTRYPOINT"),
  envir = namespace,
  mode = "function",
  inherits = FALSE
)
entrypoint(commandArgs(trailingOnly = TRUE))
```

This uses supported base-R namespace operations, works for internal or
exported functions, and gives exact installed-package semantics. It does not
require attaching the package. Values should be passed through arguments or
environment variables rather than interpolated as R source.

The principal cost is source freshness: the installed root must be rebuilt
after source changes. This option also introduces an rpx-specific function and
return-value protocol that the thin-script option does not require.

### Option: load the source package for development

`pkgload::load_all()` can construct a namespace-like environment from a source
package without first installing it. This provides fast feedback for R code
changes and handles multiple R files and compiled code. However, it adds a
development dependency and intentionally differs from installed-package
behavior in areas such as imports, exports, file lookup, and namespace
reloading. Reimplementing equivalent behavior inside rpx would amount to
building a package loader and is not a small initialization feature.

### Possible command designs

#### Bare `rpx run`

The existing command could make its command argument optional:

```sh
# Invoke the selected default project behavior
rpx run

# Preserve the existing arbitrary-command behavior
rpx run Rscript analysis.R
rpx run R
rpx run quarto render
```

For bare `rpx run`, rpx could run either a conventional script or a package
entrypoint. If the package-entrypoint model is selected, rpx could:

1. Discover the root package.
2. Read `Config/rpx/entrypoint`.
3. Ensure the local root package is installed from current source.
4. Load the installed root namespace.
5. Retrieve the named function from that namespace.
6. Call it with trailing arguments.
7. Convert a scalar integer return value into the process exit status.

This would ensure that helpers from all R files and installed dependencies are
available.

Open concerns:

- Reinstalling before every run can be expensive for packages with native code.
- Not reinstalling can execute stale source because local package contents can
  change without a version change.
- A source hash could avoid unnecessary reinstallations, but would add state and
  complexity.
- Bare `rpx run` and `rpx run <command>` would have intentionally different
  synchronization behavior.
- A bare-run default script needs a fixed path or metadata selecting the script.

One possible compatibility-preserving command contract is:

```sh
# Invoke the default project behavior
rpx run

# Preserve arbitrary-command behavior
rpx run Rscript scripts/main.R argument
rpx run quarto render
rpx run bash deploy.sh
```

Under this contract, a completely empty command vector has special behavior
and every non-empty command vector is passed through unchanged. Bare run would
not have its own argument syntax. A user who needs to pass arguments to the
default script would write the complete Rscript command. This avoids splitting
arbitrary execution into a new `rpx exec` command, but bare and explicit
execution may still differ in root-installation or synchronization behavior.

#### Separate `rpx start`

A new command could invoke the package entrypoint while leaving `rpx run`
command-only.

This is less ambiguous but expands the command surface and differs from tools
where `run` means "run the project".

#### Separate `rpx exec`

`run` could be reserved for project behavior and arbitrary commands moved to
`exec`. This creates the clearest distinction and supports unambiguous
entrypoint arguments, but it is a breaking command split. It remains a
comparison point rather than a selected direction.

#### Installed `exec/` wrapper

R packages may include non-R scripts under exec. A generated Rscript wrapper
could load the package and invoke its entrypoint.

This is an established R package mechanism, but scripts under exec are not
automatically exposed on PATH. rpx would still need to locate and invoke the
installed wrapper. It also creates two generated files for one entrypoint.

#### Explicit script invocation only

Users could run:

```sh
rpx run Rscript R/main.R
```

This requires no rpx-specific run behavior, but it has the multi-file namespace
limitation described above when the file itself is under R. A separate script
that calls `library(root.package)` does not have that limitation, provided the
current root package has been installed.

### Root installation and source freshness

The current sync implementation treats local and Git packages as always
requiring installation, even when their version is unchanged. Every successful
sync therefore refreshes the root from current source. Current `rpx run`, in
contrast, only sets R_LIBS_USER and launches a command; it does not read the
lockfile, synchronize dependencies, or reinstall the root.

Possible policies for default project execution are:

- Run full sync first. This gives the strongest existing guarantee but requires
  a valid lockfile, prunes the library, and may reinstall Git packages as well
  as the root.
- Reinstall only the root. This is narrower and guarantees fresh package code,
  but assumes its locked dependencies are already present and compatible.
- Require an explicit prior sync. This keeps run fast and unsurprising but can
  execute stale same-version package code after source edits.
- Hash installation inputs and reinstall only when the hash changes. This is
  the best performance model but requires new state and careful treatment of R,
  src, inst, data, DESCRIPTION, NAMESPACE, configure scripts, and other build
  inputs.
- Load source with pkgload. This avoids root installation but uses development
  rather than exact installed semantics.

The missing-lockfile case is significant because current init does not create a
lockfile. A promise that `rpx init && rpx run` works immediately may require init
to lock or sync, bare run to bootstrap missing state, or a root-only special
case.

### R process environment

Current command execution sets R_LIBS_USER to the project library. R startup
files can subsequently change library paths, and site or system libraries
remain visible. A package-aware driver can prepend the project library with
`.libPaths()` and pass it explicitly as `lib.loc` to `loadNamespace()`.

Using `Rscript --vanilla` would further isolate execution, but it would also
ignore project and user `.Renviron` and `.Rprofile` files that applications may
intentionally use. No startup-file policy is selected.

### Function-entrypoint visibility

If the function-entrypoint model is selected, the generated entrypoint could be:

- Exported through NAMESPACE as public package API.
- Internal and retrieved by rpx directly from the namespace.
- Wrapped by a separate exported function.

An internal function avoids requiring generated public documentation and keeps
the mechanism application-oriented. An exported `main()` is easier for users
and other tools to invoke. This remains open.

### Function-entrypoint exit behavior

A possible convention for a function entrypoint is:

- An error produces a non-zero R process exit.
- A scalar integer return value becomes the process exit status.
- `NULL` or any other return value maps to zero.
- The generated function returns `invisible(0L)`.

Argument forwarding and signal behavior would need dedicated tests.

## `.Rbuildignore`

### R's built-in exclusions

R CMD build already excludes many development artifacts without requiring
entries in `.Rbuildignore`, including:

- `.Rbuildignore` itself.
- Version-control directories such as `.git`, `.svn`, `.hg`, and `.jj`.
- `.gitignore`, `.gitattributes`, and `.gitmodules`.
- `.Rprofile`, `.Renviron`, `.Rhistory`, and `.RData`.
- `.Rproj.user`.
- Editor backups and swap files.
- Common compilation leftovers.
- Existing package archives matching the package name and version.

The generated file should not blindly duplicate every built-in exclusion.
However, some duplication, such as `.Rproj.user`, is common in real packages and
can make intent clearer.

### Proposed modern baseline

The selected direction for further design is an opinionated modern package
baseline:

```text
^rpx\.lock$
^.*\.Rproj$
^\.Rproj\.user$
^README\.Rmd$
^LICENSE\.md$
^cran-comments\.md$
^CRAN-SUBMISSION$
^CRAN-RELEASE$
^revdep$
^data-raw$
^pkgdown$
^_pkgdown\.yml$
^docs$
^\.github$
^\.vscode$
^[.]?air[.]toml$
```

Rationale:

| Pattern | Purpose |
| --- | --- |
| `rpx.lock` | Reproducible development input, not package contents |
| `*.Rproj`, `.Rproj.user` | IDE state |
| `README.Rmd` | Source for the distributable README.md |
| `LICENSE.md` | Full repository license when DESCRIPTION points at LICENSE |
| `cran-comments.md` | CRAN submission notes |
| `CRAN-SUBMISSION`, `CRAN-RELEASE` | Release workflow state |
| `revdep` | Reverse-dependency workspace |
| `data-raw` | Scripts used to generate packaged data |
| `pkgdown`, `_pkgdown.yml`, `docs` | Website source and output |
| `.github` | Repository automation and templates |
| `.vscode` | Editor configuration |
| `air.toml`, `.air.toml` | Formatter configuration |

This baseline should still be reviewed for unintended exclusions. In
particular, generated patterns should not broadly exclude Makefile, Dockerfile,
arbitrary YAML, tools, scripts, or exec because those files may participate in
building or distributing a package.

Init embeds this exact baseline from `assets/Rbuildignore` and writes it into a
clean target. It does not merge or interpret existing `.Rbuildignore` content
because non-empty targets are rejected before any project files are written.

## Licensing

The initializer defaults to MIT. R's bundled license database marks MIT as a
template that needs package-specific copyright information through
`+ file LICENSE`, so init writes the complete R form:

```text
License: MIT + file LICENSE
```

with:

```text
YEAR: 2026
COPYRIGHT HOLDER: First Last
```

in LICENSE and the full MIT text in LICENSE.md. LICENSE.md is excluded from the
package tarball, following common usethis conventions.

### Selected CLI shape

`--license` is a constrained enum rather than an arbitrary DESCRIPTION
expression. The supported values follow the license families exposed by
usethis:

| CLI value | DESCRIPTION `License` value |
|---|---|
| `mit` | `MIT + file LICENSE` |
| `apache-2.0` | `Apache License (== 2.0)` |
| `gpl-2` | `GPL-2` |
| `gpl-3` | `GPL-3` |
| `agpl-3` | `AGPL-3` |
| `lgpl-2.1` | `LGPL-2.1` |
| `lgpl-3` | `LGPL-3` |
| `cc0` | `CC0` |
| `cc-by-4.0` | `CC BY 4.0` |
| `proprietary` | `file LICENSE` |

MIT remains the non-interactive default. Unknown values should fail during CLI
parsing, before the target directory is created. Arbitrary custom license
expressions are outside the initial enum contract.

The versioned enum names above map to exact license versions. usethis defaults
GPL-family and Apache helpers to the selected version or any later version, but
that grants permissions under licenses that do not yet exist. If rpx supports
that policy later, it should expose explicit values such as
`gpl-3-or-later` rather than silently assigning it to `gpl-3`.

### Generated files

Every open-source enum value generates a complete `LICENSE.md` suitable
for repository hosting. `.Rbuildignore` excludes that file so standard license
texts are not redundantly included in the built R package.

MIT additionally generates R's package-level LICENSE template data:

```text
YEAR: 2026

COPYRIGHT HOLDER: First Last
```

Proprietary licensing instead generates the actual package-level LICENSE:

```text
Copyright 2026 First Last. All rights reserved.
```

It does not need a separate LICENSE.md. The remaining standard licenses do not
generate top-level LICENSE because their canonical texts are already known to
R and the DESCRIPTION field does not refer to `file LICENSE`.

The license templates are checked into rpx as text assets sourced from usethis
3.2.1 and embedded in the binary. Generation does not fetch license content
from the network.

### License-holder identity

MIT and proprietary files require a real legal copyright holder. Init now
supports `--author-name` and `--author-email`, renders Authors@R from those
values, and uses effective Git identity as the interactive and non-interactive
fallback. When Git identity is unavailable or invalid, the stable placeholders
are `Package Author` and `author@example.com`. These keep generated metadata
valid and clearly editable but must not be treated as production-ready legal
identity. License generation should use the resolved author name as its default
copyright holder.

## Interactive Initialization

### Selected interactive increments

The interactive initializer uses Cliclack. It currently asks for the project
directory, package name, package title, package description, author name, and
author email, followed by a license selection, optional development packages,
and, when applicable, Git repository initialization.

- `rpx init PATH` uses the explicit path and skips only the directory question.
- The remaining unresolved fields are still prompted when both stdin and stderr
  are terminals.
- `rpx init` opens the full current form only when both stdin and stderr are
  terminals.
- Non-interactive `rpx init` retains the current directory as its target.
- Interactive `rpx init` suggests a missing child directory under the current
  directory.
- The suggestion is a two-word lowercase ASCII Petname joined with a hyphen
  and displayed as an explicit child path, such as `./quiet-otter`.
- A colliding suggestion is regenerated before the form is shown.
- The suggestion is generated once per invocation and remains the editable
  default throughout the prompt.
- The selected directory continues to determine the inferred package name and
  title through the existing derivation rules.
- Package-name sanitization and package-title derivation belong to the init
  command. DESCRIPTION generation receives fully resolved package and title
  values rather than applying its own fallbacks.
- The package-name question defaults to the sanitized selected directory name.
- The package-title question defaults to a title derived from the final package
  name, including an explicitly supplied or interactively edited name.
- The package-description question defaults to `Describe what this package
  does.`.
- Author name and email default independently to effective Git `user.name` and
  `user.email` values, then to `Package Author` and `author@example.com`.
- The license question is a constrained selection and defaults to MIT.
- The optional development-package multiselect offers testthat, roxygen2, and
  devtools with no initial selections.
- Selected development packages are added to Suggests without generating tool
  configuration or scaffolding.
- Init resolves selected packages once, then records the same `>= resolved` and
  `< next major` bounds used by unconstrained `rpx add`.
- When the target is not already inside a Git worktree, interactive init asks
  whether to initialize one and defaults to yes.
- Targets inside an existing worktree use that repository without prompting or
  creating a nested repository.
- Non-interactive init does not initialize a Git repository.
- A newly initialized repository receives GitHub's R `.gitignore` template,
  embedded in the rpx binary from `assets/R.gitignore`.
- Explicit `--name`, `--title`, `--description`, `--author-name`, and
  `--author-email` values skip their respective questions; `--license` skips
  the license selection.
- Non-interactive invocations use the same package, title, description, and
  author fallbacks, default to MIT, and select no development packages.
- Interactive completion suggests changing into the selected directory before
  running `rpx add`; init has already synchronized the project library.

Interactive and non-interactive override flags, partially supplied metadata
testing, and broader prompt testing are deferred.

### Proposed behavior

- Prompt when stdin and stderr are attached to terminals.
- Use stable inferred/default values in non-TTY contexts.
- Allow `--no-interactive` to suppress prompts.
- Allow `--interactive` to require prompts and fail if no usable terminal is
  available.
- Let explicit CLI values override inference and skip the corresponding prompt.

This preserves automation while improving first-run ergonomics.

### Possible questions

```text
Package name [my.package]:
Title [My Package]:
Description [Describe what this package does.]:
Author name [Package Author]:
Author email [author@example.com]:
License [MIT]:
Development packages [none]:
Initialize a Git repository? [Y/n]:
Generate executable example code? [Y/n]:
```

This list should remain short. Questions about test frameworks, documentation,
CI providers, pkgdown, repository remotes, or R versions could be added later or
handled by dedicated commands.

### Inference order

A possible precedence order is:

1. Explicit CLI option.
2. Git user configuration for author identity.
3. Directory-derived package name and title.
4. Stable built-in fallback.

### Possible CLI

```sh
rpx init [PATH] \
  --name my.package \
  --title "My Package" \
  --description "Processes example data." \
  --author-name "First Last" \
  --author-email "first.last@example.com" \
  --license mit \
  --example <KIND> \
  --no-interactive
```

Candidate options:

```text
rpx init [PATH]
--name <NAME>
--title <TITLE>
--description <TEXT>
--author-name <NAME>
--author-email <EMAIL>
--license <mit|apache-2.0|gpl-2|gpl-3|agpl-3|lgpl-2.1|lgpl-3|cc0|cc-by-4.0|proprietary>
--example <NONE|FUNCTION|SCRIPT|ENTRYPOINT>
--interactive
--no-interactive
```

The example option is illustrative only. It may be better to omit this choice
from init entirely until one execution convention is established.

Open UX questions:

- Whether `rpx init NAME` creates a new directory, as uv and uvr do, or treats
  NAME as package metadata for the current directory.
- Whether flags should support environment-variable defaults.
- Whether invalid inferred Git email should be ignored or shown for editing.
- Whether `--yes` should accept all inferred defaults.
- Whether interactive initialization should be restartable after partial file
  creation.

## Error Handling and File Safety

A production initializer should avoid leaving a partially initialized project
when validation fails.

The selected clean-target contract is:

- Gather and validate every value before creating a missing target.
- Create a missing target directory, including missing parents.
- Allow an existing empty directory.
- Reject an existing directory containing any entry, including hidden entries.
- Reject a target that is not a directory.
- Do not merge, preserve, replace, or interpret existing project files.

Atomic multi-file creation and cleanup after a later write failure remain
possible future improvements.

## Validation and Test Proposal

### Unit tests

- Accept every valid package-name form supported by R.
- Reject one-character names, trailing dots, invalid characters, and invalid
  starting characters.
- Derive deterministic package names and titles from directory names.
- Validate author email and license selections.
- Render consistent Authors@R metadata.
- Render every supported license's exact DESCRIPTION value and required file
  set.
- Verify the complete embedded license text for every open-source variant.
- Verify MIT and proprietary holder/year substitution.
- Reject arbitrary license values during CLI parsing.
- Render each selected example shape exactly, if init supports examples.
- Validate any conventional script path or entrypoint metadata.
- Render the exact production `.Rbuildignore` baseline.
- Reject non-empty targets before writing any files.
- Resolve CLI, inferred, and interactive values in the intended precedence.
- Keep prompt logic separate from file generation so it can be tested without a
  terminal.

### Integration tests

- Run non-interactive `rpx init` in an empty directory.
- Verify the generated file tree and metadata.
- Run `rpx sync` immediately after init without adding dependencies.
- Exercise the default MIT file set and at least one standard license that must
  not generate top-level LICENSE.
- Load the generated package from the isolated project library.
- Verify the installed version.
- If a guarded R file is selected, execute it directly and verify that package
  installation and loading do not execute it.
- If a thin script is selected, verify that it loads the installed root and can
  call an exported function from another R file.
- If a function entrypoint is selected, invoke it through the installed
  namespace and verify that it can call a helper from another R file.
- If bare run gains default behavior, verify bare and non-empty command vectors
  follow their distinct contracts.
- Verify source freshness according to the selected run policy.
- Forward arguments and propagate exit status where the selected model defines
  those behaviors.
- Verify non-TTY initialization never blocks for input.
- Exercise forced interactive behavior through a pseudo-terminal if supported
  by the test environment.

### Check expectations

Installability is the selected hard acceptance requirement. A clean `R CMD
check` is a stronger possible goal, but generated placeholder title,
description, and author values may intentionally require editing.

The proposal should decide whether `rpx init` promises:

- Immediate installation only.
- Installation plus a clean package build.
- A clean `R CMD check` with no errors.
- A clean `R CMD check` with no errors, warnings, or notes.

The broader the promise, the more metadata and documentation the initializer
must generate.

## Possible Implementation Shape

The following is one possible decomposition, not a prescribed implementation.

1. Expand the init CLI arguments in `src/cli.rs`.
2. Add an initialization-options type containing resolved metadata and feature
   choices.
3. Resolve options from flags, existing state, Git configuration, defaults, and
   prompts before writing files.
4. Strengthen package-name validation in `src/description.rs`.
5. Make Authors@R the single source of generated authorship metadata.
6. Add focused writers for NAMESPACE, `.Rbuildignore`, licenses, and any
   selected example files.
7. Keep every writer idempotent and preservation-aware where merging is valid.
8. Add entrypoint metadata parsing if DESCRIPTION stores the selected function.
9. Decide whether run remains command-only, gives an empty command vector
   default behavior, or splits project and arbitrary execution.
10. Add conventional-script or namespace-function invocation if selected.
11. Add source freshness handling if default execution uses the installed root.
12. Update README and CLI help to describe package-first project semantics.

## Open Decisions

- Is rpx definitively package-only for 2.0?
- Does init generate no executable example, an R function, a thin script, a
  package function entrypoint, or some combination?
- If a `hello()` example is generated, is it exported and documented?
- If a script is generated, is it project-local or distributed with the
  package, and what is its conventional path?
- Is `sys.nframe() == 0L` part of the supported generated-code contract?
- Is direct `Rscript R/main.R` supported beyond the initial scaffold?
- Does bare `rpx run` have default behavior while every non-empty command vector
  remains an arbitrary command?
- If bare run has default behavior, does it run a script or invoke a package
  function?
- Does default execution run full sync, reinstall only the root, require prior
  sync, use a source hash, or load source through a development loader?
- Is the entrypoint public or internal?
- Is `Config/rpx/entrypoint` the right metadata field?
- What return values control process exit status?
- Are user and project R startup files honored during default execution?
- Which interactive questions are essential?
- Which non-interactive defaults are acceptable for authorship and licensing?
- Does init generate README.md, tests, or package-level documentation?
- What exact `R CMD check` quality level does init promise?

## Data Sources

Sources were accessed on 2026-08-19 unless otherwise noted.

### Authoritative R documentation and implementation

- R Core Team, "Writing R Extensions", package structure, DESCRIPTION,
  licensing, non-R scripts, namespaces, installation, building, and checking:
  <https://cran.r-project.org/doc/manuals/r-release/R-exts.html>
- R Core Team, "R Installation and Administration", installing packages:
  <https://cran.r-project.org/doc/manuals/r-release/R-admin.html#Installing-packages>
- R base documentation, call-stack inspection including `sys.nframe()`:
  <https://stat.ethz.ch/R-manual/R-devel/library/base/html/sys.parent.html>
- R base documentation, loading installed namespaces with an explicit library:
  <https://stat.ethz.ch/R-manual/R-devel/library/base/html/ns-load.html>
- R base documentation, retrieving a named namespace binding:
  <https://stat.ethz.ch/R-manual/R-devel/library/base/html/get.html>
- R base documentation, command-line argument handling:
  <https://stat.ethz.ch/R-manual/R-devel/library/base/html/commandArgs.html>
- R base documentation, startup files and `--vanilla`:
  <https://stat.ethz.ch/R-manual/R-devel/library/base/html/Startup.html>
- R utils documentation, Rscript invocation and argument ordering:
  <https://stat.ethz.ch/R-manual/R-devel/library/utils/html/Rscript.html>
- R base documentation, process exit statuses:
  <https://stat.ethz.ch/R-manual/R-devel/library/base/html/quit.html>
- R Core source, `R CMD build` exclusions and `.Rbuildignore` processing:
  <https://github.com/wch/r-source/blob/trunk/src/library/tools/R/build.R>
- R Core source, built-in version-control and hidden-file exclusions:
  <https://github.com/wch/r-source/blob/trunk/src/library/tools/R/utils.R>
- R Core documentation, `package.skeleton()`:
  <https://stat.ethz.ch/R-manual/R-devel/library/utils/html/package.skeleton.html>

### R package-development guidance

- Wickham and Bryan, "R Packages (2e)", package structure and
  `.Rbuildignore`:
  <https://r-pkgs.org/structure.html#sec-rbuildignore>
- Wickham and Bryan, "R Packages (2e)", the package-within methodology and
  package execution semantics:
  <https://r-pkgs.org/package-within.html>
- Wickham and Bryan, "R Packages (2e)", R code execution timing and package
  side effects:
  <https://r-pkgs.org/code.html>
- Wickham and Bryan, "R Packages (2e)", exec, inst, and other package
  directories:
  <https://r-pkgs.org/misc.html>
- Wickham and Bryan, "R Packages (2e)", dependency use and unused Imports:
  <https://r-pkgs.org/dependencies-in-practice.html>
- pkgload, source-package loading behavior and differences from installed
  namespaces:
  <https://github.com/r-lib/pkgload/blob/main/R/load.R>
- usethis, `create_package()`:
  <https://usethis.r-lib.org/reference/create_package.html>
- usethis, `use_build_ignore()`:
  <https://usethis.r-lib.org/reference/use_build_ignore.html>
- usethis source, package-name validation and DESCRIPTION defaults:
  <https://github.com/r-lib/usethis/blob/main/R/description.R>

### Comparable R tools

- renv, `init()`:
  <https://rstudio.github.io/renv/reference/init.html>
- renv, `scaffold()` and generated project infrastructure:
  <https://rstudio.github.io/renv/reference/scaffold.html>
- rv project initialization:
  <https://a2-ai.github.io/rv-docs/commands/project-initialization/init/>
- rv source implementation of init:
  <https://github.com/A2-ai/rv/blob/main/src/cli/commands/init.rs>
- uvr project model and commands:
  <https://github.com/nbafrank/uvr>
- uvr source implementation of init:
  <https://github.com/nbafrank/uvr/blob/main/crates/uvr/src/commands/init.rs>
- pak local package installation:
  <https://pak.r-lib.org/reference/local_install.html>
- pak local dependency discovery:
  <https://pak.r-lib.org/reference/local_deps.html>
- pak getting-started and package-development workflows:
  <https://pak.r-lib.org/reference/get-started.html>
- rig, R version and installation management:
  <https://github.com/r-lib/rig>
- ir R driver using `sys.nframe() == 0L` for script-only execution:
  <https://github.com/r-lib/ir/blob/main/driver/resolve.R>

### Cross-language prior art

- uv project initialization, application/library/package/bare distinctions and
  generated entrypoints:
  <https://docs.astral.sh/uv/concepts/projects/init/>
- Cliclack, selected for the interactive init form:
  <https://docs.rs/cliclack/latest/cliclack/>
- Petname, selected for human-readable random project-directory suggestions:
  <https://docs.rs/petname/latest/petname/>
- GitHub's R `.gitignore` template, embedded for newly initialized Git
  repositories:
  <https://github.com/github/gitignore/blob/main/R.gitignore>

### Example modern `.Rbuildignore` files

- dplyr:
  <https://github.com/tidyverse/dplyr/blob/main/.Rbuildignore>
- usethis:
  <https://github.com/r-lib/usethis/blob/main/.Rbuildignore>
- pak:
  <https://github.com/r-lib/pak/blob/main/.Rbuildignore>
- cli:
  <https://github.com/r-lib/cli/blob/main/.Rbuildignore>

## Next Step

Before implementation, the open decisions should be narrowed into a small
versioned contract for:

1. The generated file tree and metadata.
2. Interactive and non-interactive initialization behavior.
3. Whether any execution convention is generated, and its source-freshness
   contract.
4. Failure cleanup or atomicity beyond the clean-target precondition.
5. The promised R build/check quality level.

Once those are agreed, SCA-70 can be implemented and tested as one coherent
initializer change rather than a sequence of unrelated scaffolding patches.
