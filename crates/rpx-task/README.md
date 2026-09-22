# rpx-task

Typed async workflows with resource-aware execution:

```text
application operations → rpx-task → Tokio
```

This crate schedules and executes async functions and delivers their successful
return values to consumers. Declaring a task result as an input establishes both
a scheduling edge and a typed data edge;
applications do not maintain separate output lookup tables.

## Construction and execution

- `GraphBuilder<E>` builds a single-use graph of operations with application error
  type `E`.
- `task` creates a producer; its `TaskRef<T>` refers to that producer's result.
- Inputs can be `()`, a task reference, nested pairs, or vectors of task references.
- `reserve` and `define` support forward references. `finish` rejects undefined,
  foreign, or cyclic task dependencies and invalid resource requests.
- The same producer can feed many consumers. It executes once, and consumers
  receive shared `Arc<T>` values; `T` does not need to implement `Clone`.
- `execute` admits only ready, resource-eligible nodes. Outputs are published
  before successful completion releases successor edges.
- Resource IDs are local to their builder. Resource requests are reserved together
  before an operation runs; consumers waiting for predecessors use no slots.

## Failure and lifecycle

On an observed operation error or panic, execution stops admitting new work and
drains running operations. The first observed failure is returned; subsequent
failures still generate events. Failed producers publish no successful output.

Execution events identify nodes, leaving package names, progress rendering, and
diagnostics to the application. Observers must not block or panic.

Dropping the execution future aborts its Tokio tasks. That is cancellation, not
the normal drain-on-failure path, and it cannot stop already-running blocking
operations. Callers requiring orderly shutdown must continue awaiting execution.

This is an in-memory workflow runner, not a durable execution system. It does not
journal results, retry operations, or provide exactly-once external side effects.

## Internal scheduling

Kahn readiness and resource accounting live in the private `scheduler` module.
Each scheduled node owns its operation; admission reserves capacity and returns
the operation directly to the executor. There is no separate public scheduling
protocol or parallel operation table.

The ready set selects the lowest-ID eligible node, skipping ready nodes whose
resource requests do not fit. Completion releases resources; only success releases
dependency edges. Graph finalization normalizes duplicate edges, sums repeated
resource requests, and rejects cycles and impossible requests through `BuildError`.
Cycle errors identify all cycle-blocked nodes, including downstream consumers.

Scheduling decisions have synchronous unit tests alongside the async execution
tests, without requiring a separate crate.

Run tests independently with `cargo test -p rpx-task --locked`.
