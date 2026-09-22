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

- `sync.rs`: project setup, execution context, running the graph, and progress.
- `sync/plan.rs`: installation/removal policy, graph edges, and resource requests.
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

`PlanError` separately reports dependency metadata errors, package-labelled cycles,
and task-graph construction failures. `SyncError` handles setup and forwards plan
or operation diagnostics at the command boundary. Executor panics/join failures
are attributed to the package and operation kind and preserve the `JoinError`;
internal invariant messages remain a distinct failure. Errors are not flattened
into a generic message string.

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
