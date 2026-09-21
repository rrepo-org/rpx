//! Resource-aware Kahn scheduling, independent of executors and task payloads.
//!
//! Node IDs are indices into the node specifications passed to [`Scheduler::new`].
//! A successful completion releases outgoing edges; a failed completion only
//! releases resources. Callers decide whether to continue admitting unrelated work.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use thiserror::Error;

/// An index into a scheduler's node specifications.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub usize);

/// An index into a scheduler's resource capacities.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ResourceId(pub usize);

/// Structural inputs to a node, with no executable payload.
#[derive(Clone, Debug, Default)]
pub struct NodeSpec {
    pub dependencies: Vec<NodeId>,
    pub resources: Vec<(ResourceId, usize)>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum GraphError {
    #[error("node {node:?} references unknown predecessor {dependency:?}")]
    UnknownDependency { node: NodeId, dependency: NodeId },
    #[error("node {node:?} has an invalid request for resource {resource:?}")]
    InvalidResource { node: NodeId, resource: ResourceId },
    #[error("nodes are blocked by a dependency cycle: {blocked:?}")]
    Cycle { blocked: Vec<NodeId> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Completion {
    Succeeded,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// There is running work, or work that can be admitted.
    Active,
    /// Every node completed successfully.
    Complete,
    /// No work can run; at least one node failed.
    Failed,
}

#[derive(Debug, Error, PartialEq, Eq)]
#[error("node {0:?} is not running")]
pub struct TransitionError(pub NodeId);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Pending,
    Running,
    Succeeded,
    Failed,
}

/// Deterministic Kahn scheduling with atomic multi-resource admission.
pub struct Scheduler {
    nodes: Vec<NodeSpec>,
    successors: Vec<Vec<NodeId>>,
    remaining: Vec<usize>,
    ready: BTreeSet<NodeId>,
    states: Vec<State>,
    available: Vec<usize>,
    running: usize,
}

impl Scheduler {
    /// Validate all edges, normalize duplicate requirements, and reject cycles
    /// before any node can be admitted.
    pub fn new(mut nodes: Vec<NodeSpec>, capacities: Vec<usize>) -> Result<Self, GraphError> {
        let count = nodes.len();
        let mut successors = vec![Vec::new(); count];
        let mut remaining = vec![0; count];
        for (index, spec) in nodes.iter_mut().enumerate() {
            let node = NodeId(index);
            spec.dependencies.sort_unstable();
            spec.dependencies.dedup();
            for &dependency in &spec.dependencies {
                if dependency.0 >= count {
                    return Err(GraphError::UnknownDependency { node, dependency });
                }
                successors[dependency.0].push(node);
            }
            remaining[index] = spec.dependencies.len();
            let mut resources = BTreeMap::<ResourceId, usize>::new();
            for &(resource, quantity) in &spec.resources {
                let invalid = || GraphError::InvalidResource { node, resource };
                let capacity = capacities.get(resource.0).ok_or_else(invalid)?;
                let total = resources.entry(resource).or_default();
                *total = total.checked_add(quantity).ok_or_else(invalid)?;
                if *total > *capacity {
                    return Err(invalid());
                }
            }
            spec.resources = resources.into_iter().filter(|(_, n)| *n != 0).collect();
        }
        let ready: BTreeSet<_> = remaining
            .iter()
            .enumerate()
            .filter_map(|(i, &n)| (n == 0).then_some(NodeId(i)))
            .collect();
        let mut validation_remaining = remaining.clone();
        let mut queue: VecDeque<_> = ready.iter().copied().collect();
        while let Some(node) = queue.pop_front() {
            for &next in &successors[node.0] {
                validation_remaining[next.0] -= 1;
                if validation_remaining[next.0] == 0 {
                    queue.push_back(next);
                }
            }
        }
        let blocked: Vec<_> = validation_remaining
            .iter()
            .enumerate()
            .filter_map(|(i, &n)| (n != 0).then_some(NodeId(i)))
            .collect();
        if !blocked.is_empty() {
            return Err(GraphError::Cycle { blocked });
        }
        Ok(Self {
            nodes,
            successors,
            remaining,
            ready,
            states: vec![State::Pending; count],
            available: capacities,
            running: 0,
        })
    }

    /// Admit the lowest-ID ready node whose complete resource request fits.
    /// Resource-blocked nodes stay ready and do not prevent other admissions.
    pub fn admit_next(&mut self) -> Option<NodeId> {
        let node = self.ready.iter().copied().find(|node| {
            self.nodes[node.0]
                .resources
                .iter()
                .all(|(r, n)| self.available[r.0] >= *n)
        })?;
        self.ready.remove(&node);
        for &(resource, quantity) in &self.nodes[node.0].resources {
            self.available[resource.0] -= quantity;
        }
        self.states[node.0] = State::Running;
        self.running += 1;
        Some(node)
    }

    /// Finish a running node. Only success releases its successor edges.
    pub fn complete(&mut self, node: NodeId, outcome: Completion) -> Result<(), TransitionError> {
        if self.states.get(node.0) != Some(&State::Running) {
            return Err(TransitionError(node));
        }
        self.states[node.0] = match outcome {
            Completion::Succeeded => State::Succeeded,
            Completion::Failed => State::Failed,
        };
        self.running -= 1;
        for &(resource, quantity) in &self.nodes[node.0].resources {
            self.available[resource.0] += quantity;
        }
        if outcome == Completion::Succeeded {
            for &next in &self.successors[node.0] {
                self.remaining[next.0] -= 1;
                if self.remaining[next.0] == 0 {
                    self.ready.insert(next);
                }
            }
        }
        Ok(())
    }

    pub fn status(&self) -> Status {
        if self.running != 0 || !self.ready.is_empty() {
            Status::Active
        } else if self.states.contains(&State::Failed) {
            Status::Failed
        } else {
            Status::Complete
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(dependencies: &[usize], resources: &[(usize, usize)]) -> NodeSpec {
        NodeSpec {
            dependencies: dependencies.iter().copied().map(NodeId).collect(),
            resources: resources.iter().map(|&(r, n)| (ResourceId(r), n)).collect(),
        }
    }

    #[test]
    fn diamond_releases_only_after_successful_completion() {
        let mut s = Scheduler::new(
            vec![
                node(&[], &[]),
                node(&[0, 0], &[]),
                node(&[0], &[]),
                node(&[1, 2], &[]),
            ],
            vec![],
        )
        .unwrap();
        assert_eq!(s.admit_next(), Some(NodeId(0)));
        assert_eq!(s.admit_next(), None);
        s.complete(NodeId(0), Completion::Succeeded).unwrap();
        assert_eq!(s.admit_next(), Some(NodeId(1)));
        assert_eq!(s.admit_next(), Some(NodeId(2)));
        s.complete(NodeId(2), Completion::Succeeded).unwrap();
        assert_eq!(s.admit_next(), None);
        s.complete(NodeId(1), Completion::Succeeded).unwrap();
        assert_eq!(s.admit_next(), Some(NodeId(3)));
        s.complete(NodeId(3), Completion::Succeeded).unwrap();
        assert_eq!(s.status(), Status::Complete);
    }

    #[test]
    fn resources_are_atomic_and_blocked_ready_nodes_are_skipped() {
        let mut s = Scheduler::new(
            vec![
                node(&[], &[(0, 1), (1, 1)]),
                node(&[], &[(0, 1), (1, 1)]),
                node(&[], &[(0, 1)]),
            ],
            vec![2, 1],
        )
        .unwrap();
        assert_eq!(s.admit_next(), Some(NodeId(0)));
        assert_eq!(s.admit_next(), Some(NodeId(2)));
        assert_eq!(s.admit_next(), None);
        s.complete(NodeId(0), Completion::Succeeded).unwrap();
        assert_eq!(s.admit_next(), Some(NodeId(1)));
    }

    #[test]
    fn failure_releases_capacity_but_not_dependents() {
        let mut s = Scheduler::new(
            vec![node(&[], &[(0, 1)]), node(&[0], &[]), node(&[], &[(0, 1)])],
            vec![1],
        )
        .unwrap();
        assert_eq!(s.admit_next(), Some(NodeId(0)));
        s.complete(NodeId(0), Completion::Failed).unwrap();
        assert_eq!(s.admit_next(), Some(NodeId(2)));
        assert_eq!(
            s.complete(NodeId(0), Completion::Succeeded),
            Err(TransitionError(NodeId(0)))
        );
        assert_eq!(
            s.complete(NodeId(99), Completion::Succeeded),
            Err(TransitionError(NodeId(99)))
        );
        s.complete(NodeId(2), Completion::Succeeded).unwrap();
        assert_eq!(s.admit_next(), None);
        assert_eq!(s.status(), Status::Failed);
    }

    #[test]
    fn validates_edges_resources_and_cycles_including_blocked_descendants() {
        assert!(matches!(
            Scheduler::new(vec![node(&[1], &[])], vec![]),
            Err(GraphError::UnknownDependency { .. })
        ));
        for resources in [
            vec![(0, 2)],
            vec![(0, 1), (0, 1)],
            vec![(1, 0)],
            vec![(0, usize::MAX), (0, 1)],
        ] {
            assert!(matches!(
                Scheduler::new(vec![node(&[], &resources)], vec![1]),
                Err(GraphError::InvalidResource { .. })
            ));
        }
        assert!(
            matches!(Scheduler::new(vec![node(&[1], &[]), node(&[0], &[]),
            node(&[1], &[]), node(&[], &[])], vec![]),
            Err(GraphError::Cycle { blocked }) if blocked == vec![NodeId(0), NodeId(1), NodeId(2)])
        );
        assert_eq!(
            Scheduler::new(vec![], vec![]).unwrap().status(),
            Status::Complete
        );
    }
}
