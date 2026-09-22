//! Typed async operations joined by value-carrying dependency edges.
//!
//! Kahn scheduling and resource admission are internal to the task runner.
//! On failure, execution stops admitting new work and drains running operations
//! before returning. Dropping the execution future
//! instead aborts its Tokio tasks, and cannot stop already-running blocking work.

mod scheduler;

use scheduler::Scheduler;
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, OnceLock},
};
use thiserror::Error;
use tokio::task::JoinSet;

/// An opaque task identity used in execution events and diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(usize);

/// A resource pool created by [`GraphBuilder::resource`]. IDs are graph-local.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ResourceId(usize);

/// A reference to one producer's result, reusable by multiple consumers.
pub struct TaskRef<T> {
    graph: Arc<()>,
    node: NodeId,
    output: Arc<OnceLock<Arc<T>>>,
}

impl<T> Clone for TaskRef<T> {
    fn clone(&self) -> Self {
        Self {
            graph: self.graph.clone(),
            node: self.node,
            output: self.output.clone(),
        }
    }
}

impl<T> TaskRef<T> {
    pub fn id(&self) -> NodeId {
        self.node
    }

    /// Read a completed output, for example after awaiting graph execution.
    pub fn output(&self) -> Option<&Arc<T>> {
        self.output.get()
    }
}

mod sealed {
    pub trait Sealed {}
}

/// Supported task inputs. Dependency discovery and value retrieval cannot be
/// implemented independently by callers: this trait is sealed.
pub trait Inputs: sealed::Sealed + Send + 'static {
    type Value: Send;
    #[doc(hidden)]
    fn dependencies(&self, graph: &Arc<()>) -> Result<Vec<NodeId>, BuildError>;
    #[doc(hidden)]
    fn resolve(&self) -> Option<Self::Value>;
}

impl sealed::Sealed for () {}
impl Inputs for () {
    type Value = ();
    fn dependencies(&self, _: &Arc<()>) -> Result<Vec<NodeId>, BuildError> {
        Ok(vec![])
    }
    fn resolve(&self) -> Option<()> {
        Some(())
    }
}

impl<T> sealed::Sealed for TaskRef<T> {}
impl<T: Send + Sync + 'static> Inputs for TaskRef<T> {
    type Value = Arc<T>;
    fn dependencies(&self, graph: &Arc<()>) -> Result<Vec<NodeId>, BuildError> {
        if !Arc::ptr_eq(&self.graph, graph) {
            return Err(BuildError::ForeignTask);
        }
        Ok(vec![self.node])
    }
    fn resolve(&self) -> Option<Arc<T>> {
        self.output.get().cloned()
    }
}

impl<A: Inputs, B: Inputs> sealed::Sealed for (A, B) {}
impl<A: Inputs, B: Inputs> Inputs for (A, B) {
    type Value = (A::Value, B::Value);
    fn dependencies(&self, graph: &Arc<()>) -> Result<Vec<NodeId>, BuildError> {
        let mut nodes = self.0.dependencies(graph)?;
        nodes.extend(self.1.dependencies(graph)?);
        Ok(nodes)
    }
    fn resolve(&self) -> Option<Self::Value> {
        Some((self.0.resolve()?, self.1.resolve()?))
    }
}

impl<T> sealed::Sealed for Vec<TaskRef<T>> {}
impl<T: Send + Sync + 'static> Inputs for Vec<TaskRef<T>> {
    type Value = Vec<Arc<T>>;
    fn dependencies(&self, graph: &Arc<()>) -> Result<Vec<NodeId>, BuildError> {
        self.iter().try_fold(Vec::new(), |mut nodes, task| {
            nodes.extend(task.dependencies(graph)?);
            Ok(nodes)
        })
    }
    fn resolve(&self) -> Option<Self::Value> {
        self.iter().map(Inputs::resolve).collect()
    }
}

#[derive(Debug, Error)]
pub enum BuildError {
    #[error("task belongs to a different graph")]
    ForeignTask,
    #[error("task {0:?} is already defined")]
    AlreadyDefined(NodeId),
    #[error("task {0:?} was reserved but never defined")]
    Undefined(NodeId),
    #[error("task {node:?} has an invalid request for resource {resource:?}")]
    InvalidResource { node: NodeId, resource: ResourceId },
    #[error("tasks are blocked by a dependency cycle: {blocked:?}")]
    Cycle { blocked: Vec<NodeId> },
}

#[derive(Debug, Error)]
pub enum ExecutionError<E> {
    #[error("operation {node:?} failed: {source}")]
    Operation { node: NodeId, source: E },
    #[error("operation {node:?} could not be joined: {source}")]
    Join {
        node: NodeId,
        source: tokio::task::JoinError,
    },
    #[error("task engine invariant failed: {0}")]
    Invariant(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionEvent {
    Started(NodeId),
    Succeeded(NodeId),
    Failed(NodeId),
}

type Operation<E> = Pin<Box<dyn Future<Output = Result<(), ExecutionError<E>>> + Send>>;
struct Node<E> {
    dependencies: Vec<NodeId>,
    resources: Vec<(ResourceId, usize)>,
    operation: Operation<E>,
}

/// Graph construction is separate from execution. Reserving a typed handle
/// permits forward references; finalization rejects undefined tasks and cycles.
pub struct GraphBuilder<E> {
    identity: Arc<()>,
    nodes: Vec<Option<Node<E>>>,
    capacities: Vec<usize>,
}

impl<E: Send + 'static> Default for GraphBuilder<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E: Send + 'static> GraphBuilder<E> {
    pub fn new() -> Self {
        Self {
            identity: Arc::new(()),
            nodes: vec![],
            capacities: vec![],
        }
    }

    /// Resource IDs are local to this builder. Quantities are reserved atomically
    /// by the scheduler, before polling an operation.
    pub fn resource(&mut self, capacity: usize) -> ResourceId {
        let id = ResourceId(self.capacities.len());
        self.capacities.push(capacity);
        id
    }

    pub fn reserve<T: Send + Sync + 'static>(&mut self) -> TaskRef<T> {
        let node = NodeId(self.nodes.len());
        self.nodes.push(None);
        TaskRef {
            graph: self.identity.clone(),
            node,
            output: Arc::new(OnceLock::new()),
        }
    }

    pub fn define<I, F, Fut, T>(
        &mut self,
        task: &TaskRef<T>,
        inputs: I,
        resources: Vec<(ResourceId, usize)>,
        operation: F,
    ) -> Result<(), BuildError>
    where
        I: Inputs,
        F: FnOnce(I::Value) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, E>> + Send + 'static,
        T: Send + Sync + 'static,
    {
        task.dependencies(&self.identity)?;
        let dependencies = inputs.dependencies(&self.identity)?;
        if self.nodes[task.node.0].is_some() {
            return Err(BuildError::AlreadyDefined(task.node));
        }
        let node = task.node;
        let output = task.output.clone();
        let operation = Box::pin(async move {
            let values = inputs
                .resolve()
                .ok_or(ExecutionError::Invariant("ready task has missing inputs"))?;
            let value = operation(values)
                .await
                .map_err(|source| ExecutionError::Operation { node, source })?;
            output
                .set(Arc::new(value))
                .map_err(|_| ExecutionError::Invariant("output published twice"))?;
            Ok(())
        });
        self.nodes[node.0] = Some(Node {
            dependencies,
            resources,
            operation,
        });
        Ok(())
    }

    pub fn task<I, F, Fut, T>(
        &mut self,
        inputs: I,
        resources: Vec<(ResourceId, usize)>,
        operation: F,
    ) -> Result<TaskRef<T>, BuildError>
    where
        I: Inputs,
        F: FnOnce(I::Value) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, E>> + Send + 'static,
        T: Send + Sync + 'static,
    {
        // Validate inputs before reserving so errors don't leave undefined nodes.
        inputs.dependencies(&self.identity)?;
        let task = self.reserve();
        self.define(&task, inputs, resources, operation)?;
        Ok(task)
    }

    pub fn finish(self) -> Result<ExecutableGraph<E>, BuildError> {
        let nodes = self
            .nodes
            .into_iter()
            .enumerate()
            .map(|(index, node)| node.ok_or(BuildError::Undefined(NodeId(index))))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ExecutableGraph {
            scheduler: Scheduler::new(nodes, self.capacities)?,
        })
    }
}

pub struct ExecutableGraph<E> {
    scheduler: Scheduler<E>,
}

impl<E: Send + 'static> ExecutableGraph<E> {
    /// Execute once. The observer runs synchronously and should not block or panic.
    /// On an operation failure or panic, stop admission and await all running work.
    pub async fn execute(
        mut self,
        mut observe: impl FnMut(ExecutionEvent),
    ) -> Result<(), ExecutionError<E>> {
        let mut running = JoinSet::new();
        let mut ids = HashMap::new();
        let mut failure = None;
        loop {
            if failure.is_none() {
                while let Some((node, operation)) = self.scheduler.admit_next() {
                    let handle = running.spawn(operation);
                    ids.insert(handle.id(), node);
                    observe(ExecutionEvent::Started(node));
                }
            }
            let Some(completed) = running.join_next_with_id().await else {
                break;
            };
            let (node, result) = match completed {
                Ok((id, result)) => (ids.remove(&id).expect("running task has a node"), result),
                Err(source) => {
                    let node = ids.remove(&source.id()).expect("failed task has a node");
                    (node, Err(ExecutionError::Join { node, source }))
                }
            };
            self.scheduler.complete(node, result.is_ok());
            observe(if result.is_ok() {
                ExecutionEvent::Succeeded(node)
            } else {
                ExecutionEvent::Failed(node)
            });
            if let Err(error) = result {
                failure.get_or_insert(error);
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        if !self.scheduler.is_complete() {
            return Err(ExecutionError::Invariant(
                "unfinished graph has no runnable operations",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn typed_diamond_and_duplicate_inputs_execute_producer_once() {
        let mut graph = GraphBuilder::<&'static str>::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let producer = graph
            .task((), vec![], move |_| async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(21_u32)
            })
            .unwrap();
        let left = graph
            .task(producer.clone(), vec![], |n| async move { Ok(*n + 1) })
            .unwrap();
        let right = graph
            .task(
                (producer.clone(), producer.clone()),
                vec![],
                |(a, b)| async move {
                    assert!(Arc::ptr_eq(&a, &b));
                    Ok(*a + *b)
                },
            )
            .unwrap();
        let sum = graph
            .task(vec![left, right], vec![], |values| async move {
                Ok(values.iter().map(|n| **n).sum::<u32>())
            })
            .unwrap();
        graph.finish().unwrap().execute(|_| {}).await.unwrap();
        assert_eq!(**sum.output().unwrap(), 64);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn forward_references_and_resources_do_not_occupy_waiting_slots() {
        let mut graph = GraphBuilder::<&'static str>::new();
        let worker = graph.resource(1);
        let consumer = graph.reserve::<usize>();
        let producer = graph.reserve::<usize>();
        graph
            .define(
                &consumer,
                producer.clone(),
                vec![(worker, 1)],
                |n| async move { Ok(*n + 1) },
            )
            .unwrap();
        graph
            .define(&producer, (), vec![(worker, 1)], |_| async { Ok(1) })
            .unwrap();
        let mut starts = vec![];
        graph
            .finish()
            .unwrap()
            .execute(|event| {
                if let ExecutionEvent::Started(node) = event {
                    starts.push(node);
                }
            })
            .await
            .unwrap();
        assert_eq!(starts, [producer.id(), consumer.id()]);
        assert_eq!(**consumer.output().unwrap(), 2);
    }

    #[test]
    fn rejects_foreign_undefined_duplicate_and_cyclic_definitions() {
        let mut graph = GraphBuilder::<&'static str>::new();
        let foreign = GraphBuilder::<&'static str>::new().reserve::<()>();
        assert!(matches!(
            graph.task(foreign, vec![], |_| async { Ok(()) }),
            Err(BuildError::ForeignTask)
        ));
        let a = graph.reserve::<()>();
        assert!(matches!(graph.finish(), Err(BuildError::Undefined(_))));
        let mut graph = GraphBuilder::<&'static str>::new();
        assert!(matches!(
            graph.define(&a, (), vec![], |_| async { Ok(()) }),
            Err(BuildError::ForeignTask)
        ));
        let a = graph.reserve::<()>();
        let b = graph.reserve::<()>();
        graph
            .define(&a, b.clone(), vec![], |_| async { Ok(()) })
            .unwrap();
        assert!(matches!(
            graph.define(&a, (), vec![], |_| async { Ok(()) }),
            Err(BuildError::AlreadyDefined(_))
        ));
        graph.define(&b, a, vec![], |_| async { Ok(()) }).unwrap();
        assert!(matches!(graph.finish(), Err(BuildError::Cycle { .. })));
    }

    #[tokio::test]
    async fn failure_drains_running_work_and_stops_admission() {
        let mut graph = GraphBuilder::<&'static str>::new();
        let worker = graph.resource(2);
        let (release, wait) = tokio::sync::oneshot::channel();
        let failed = graph
            .task((), vec![(worker, 1)], |_| async { Err::<(), _>("failure") })
            .unwrap();
        let draining = graph
            .task((), vec![(worker, 1)], |_| async {
                wait.await.unwrap();
                Ok(42)
            })
            .unwrap();
        let never = graph
            .task((), vec![(worker, 1)], |_| async { Ok(()) })
            .unwrap();
        let dependent = graph.task(failed, vec![], |_| async { Ok(()) }).unwrap();
        let mut release = Some(release);
        let error = graph
            .finish()
            .unwrap()
            .execute(|event| {
                if matches!(event, ExecutionEvent::Failed(_)) {
                    release.take().unwrap().send(()).unwrap();
                }
            })
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ExecutionError::Operation {
                source: "failure",
                ..
            }
        ));
        assert_eq!(**draining.output().unwrap(), 42);
        assert!(never.output().is_none());
        assert!(dependent.output().is_none());
    }

    #[tokio::test]
    async fn panic_is_attributed_and_does_not_release_dependents() {
        let mut graph = GraphBuilder::<&'static str>::new();
        let task = graph
            .task((), vec![], |_| async {
                panic!("test panic");
                #[allow(unreachable_code)]
                Ok(())
            })
            .unwrap();
        let dependent = graph
            .task(task.clone(), vec![], |_| async { Ok(()) })
            .unwrap();
        let error = graph.finish().unwrap().execute(|_| {}).await.unwrap_err();
        assert!(matches!(error, ExecutionError::Join { node, .. } if node == task.id()));
        assert!(dependent.output().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_operations_obey_resource_limits_and_deliver_all_results() {
        let mut graph = GraphBuilder::<&'static str>::new();
        let worker = graph.resource(5);
        let limited = graph.resource(2);
        let active = Arc::new(AtomicUsize::new(0));
        let mut producers = Vec::new();
        for value in 0..40 {
            let active = active.clone();
            producers.push(
                graph
                    .task((), vec![(worker, 1), (limited, 1)], move |()| async move {
                        assert!(active.fetch_add(1, Ordering::SeqCst) < 2);
                        for _ in 0..5 {
                            tokio::task::yield_now().await;
                        }
                        active.fetch_sub(1, Ordering::SeqCst);
                        Ok(value)
                    })
                    .unwrap(),
            );
        }
        let result = graph
            .task(producers, vec![(worker, 1)], |values| async move {
                Ok(values.iter().map(|n| **n).sum::<usize>())
            })
            .unwrap();
        let mut running = 0;
        graph
            .finish()
            .unwrap()
            .execute(|event| {
                match event {
                    ExecutionEvent::Started(_) => running += 1,
                    ExecutionEvent::Succeeded(_) | ExecutionEvent::Failed(_) => running -= 1,
                }
                assert!(running <= 2);
            })
            .await
            .unwrap();
        assert_eq!(running, 0);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(**result.output().unwrap(), (0..40).sum::<usize>());
    }
}
