# Sync execution

The dependency direction is `rpx -> rpx-task -> Tokio`. Package operations belong
to rpx; typed composition, scheduling, and execution belong to one task-runner
crate. Scheduling is a private implementation detail, not a separate library API.

## Internal scheduler

The private scheduler module validates the DAG before execution and uses Kahn's
algorithm. Nodes become ready when all distinct predecessors succeed. Admission atomically
reserves all requested resources, and completion releases those resources.
Edges are released on success, not admission. Failed nodes leave their consumers
blocked. The ready set is deterministic for a given sequence of completions.

Ready nodes that cannot obtain capacity are skipped so other eligible work can
run. Sync configures 50 shared workers, one checkout slot, and eight shared
build/install slots. Waiting consumers occupy no worker slots.

Each node keeps its operation with its resource and dependency bookkeeping.
Admission returns the ready operation directly to the executor. There is no
parallel executor operation table, public completion/status protocol, or separate
graph-error wrapper. Graph-construction errors are reported through `BuildError`.

## Tasks

`rpx-task` connects typed function inputs and outputs to scheduler nodes.
Referencing a task's result establishes both the dependency edge and its input
value. Shared producers execute once and deliver `Arc<T>` results. Tuple/vector
inputs support fan-in; reserved handles allow forward references.

The task runner publishes outputs before reporting successful completion. It
owns Tokio task IDs and attributes panics to graph nodes. On the first observed
failure it stops launching operations and drains already-running work. This is
important for installation, where aborting an async wrapper would not terminate
its `spawn_blocking` operation. Dropping the execution future is cancellation and
does not provide the drain guarantee.

The runner is in-memory: no persistence, retries, or exactly-once side-effect
guarantees are implied. Lifecycle events carry node IDs, while rpx owns display
metadata, progress spans, and diagnostics.

## Package operations

Sync has three application modules:

- `sync.rs`: adapt the resolved project and root-package policy, prepare the target,
  and render package progress. It does not interpret graph IDs or task kinds.
- `sync/plan.rs`: bind the library snapshot, reconcile packages, assemble and run
  the private graph, and translate its events and errors into package terms.
- `sync/operations.rs`: download, checkout, build, install, and remove functions,
  together with their inputs, outputs, errors, and artifact/cache helpers.

`sync/plan.rs` constructs the workflow using existing repository implementations:

```text
registry download -------------------- artifact ----> install
Git checkout ---- BuildInput ----> build -- artifact -> install
local BuildInput ---------------> build -- artifact -> install
dependency installs -------------------------------> install
```

Downloads return the successfully published or cached `PreparedArtifact`, with
its source/binary kind, path, and binary format. Git checkout returns the pinned
tree and commit-specific archive destination. Builds return their source archive.
Installation receives that exact result and never reconstructs candidate cache
paths or searches for another artifact.

Package version, R version, dependency fingerprint inputs, installer options,
binary-first fallback, and cache-key construction retain their existing policy.
Cycles are now rejected during graph finalization, before artifact operations
or removals start. Normal CLI entry points and repository dispatch remain in rpx.

## Plan invariants

`SyncTarget::inspect` binds one installed-version snapshot to the library that
execution will modify and to the R version supplied by resolution. It does not
use the scanner's placeholder repositories as installed-source provenance.
This is a snapshot, not a library-wide lock against concurrent processes.

`SyncPlan::prepare` performs two phases:

1. Pure `reconcile`: classify retained, installed, and removed packages using
   explicit dependency records from resolution or locked replay. Installation and
   removal sets are disjoint. Dependency versions come from the same resolution,
   even for retained packages; runtime-provided dependencies keep optional versions.
2. Fold the changes into a private `Assembly`: reserve every installation handle,
   register one artifact-producing pipeline per installation, wire dependency
   installations, register removals, and finalize the graph.

All operation definitions use one helper that assigns resources, attaches the
current tracing span, and records package metadata together. Metadata and graph
handles stay private. Local archive destinations are computed when their build
operation runs; dependency versions are formatted only by installer preparation.

`SyncPlan::run` owns execution, reports `SyncProgress` counts, and attributes task
panics internally. Its caller only needs `install_count()` and the progress
observer; it never indexes a task metadata map. Tracing instrumentation is bound
by constructing the plan under the parent span, not passed as execution data.

## Error boundaries

Operations return their own errors, not `SyncError`:

| Operation | Result | Error |
| --- | --- | --- |
| Download | `PreparedArtifact` | `DownloadPackageArtifactError` |
| Checkout | `BuildInput` | `CheckoutError` |
| Build | `PreparedArtifact` | `r::PackageBuildError` |
| Install | `()` | `InstallPackageError` |
| Remove | `()` | `RemovePackageError` |

Graph registration attaches package context and converts those failures to the
common `OperationError`. Existing download/build/install/removal diagnostic codes
are preserved. Checkout has its own diagnostic, with distinct commit-resolution
and checkout causes. Installer preparation and materialization (including their
blocking-task join failures) are distinguished. All retain typed source errors.

Native metadata is validated when constructing resolved package records, before
sync planning. `PlanError` reports package-labelled cycles and task-graph
construction failures. `RunError` forwards contextual operation errors
and attributes executor panics/join failures to a package and operation while
preserving the `JoinError`. Internal invariant messages remain distinct.
`SyncError` handles project setup and transparently forwards plan/run diagnostics
at the command boundary. Errors are not flattened into a generic message string.

## Tests

```sh
cargo test -p rpx-task --locked
cargo nextest run --workspace --locked
cargo test --workspace --doc --locked
```

Infrastructure tests cover scheduling, resource limits, typed fan-in/fan-out,
forward references, failures, and panic handling. Sync tests cover explicit
artifact results, publication cleanup, pinned checkout inputs, and cycle errors.
Native E2E tests exercise downloads, source fallback, binary/source cache reuse,
dependency installation ordering, and failure recovery through real R processes.
Existing E2E assertions remain in place.

The GitHub Tests workflow archives all workspace tests on Linux, macOS, and
Windows. It runs unit tests once per OS and E2E tests against current and previous
R releases. Rust doctests are run separately locally because nextest does not
include them.
