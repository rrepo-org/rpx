# rpx-task

Typed async workflows over `rpx-scheduler`:

```text
application operations → rpx-task → rpx-scheduler
                             ↓
                           Tokio
```

The scheduler decides when a node can run. This crate executes its async function
and delivers the function's successful return value to its consumers. Declaring a
task result as an input establishes both a scheduling edge and a typed data edge;
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

Run tests independently with `cargo test -p rpx-task --locked`.
