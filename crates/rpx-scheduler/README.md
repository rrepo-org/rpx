# rpx-scheduler

A synchronous, payload-free, resource-aware Kahn scheduler. It has no async
runtime or knowledge of packages, artifacts, task outputs, or application errors.

The caller supplies node dependencies and resource capacities. `Scheduler::new`
normalizes duplicate edges, sums repeated resource requests, and validates the
entire graph (including cycles) before execution.

`admit_next` selects the lowest-ID ready node whose resource requests all fit,
reserves those resources atomically, and marks it running. Ready nodes waiting
for capacity do not block admission of other ready nodes.

`complete` releases resources. Successful completion also releases successor
edges; failure leaves dependent nodes blocked. Admission is deterministic for a
given completion sequence, but concurrent completion order is not deterministic.

The caller owns execution and failure policy. `rpx-task` implements stop-admission
and drain-running behavior using this API. It is also possible for another caller
to continue unrelated work after failure.

Dependency validation and edge-release bookkeeping are O(V + E), excluding
ordered-set operations. Admission scans ready nodes for resource eligibility.
Cycle diagnostics report all cycle-blocked nodes, including descendants, rather
than claiming every remaining node belongs to a cycle.

Run tests independently with `cargo test -p rpx-scheduler --locked`.
