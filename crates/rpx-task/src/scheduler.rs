//! Internal resource-aware Kahn scheduling. An admitted node carries its operation
//! with it; only successful completion releases successor edges.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::{BuildError, Node, NodeId, Operation, ResourceId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Pending,
    Running,
    Succeeded,
    Failed,
}

struct ScheduledNode<E> {
    remaining: usize,
    successors: Vec<NodeId>,
    resources: Vec<(ResourceId, usize)>,
    operation: Option<Operation<E>>,
    state: State,
}

pub(super) struct Scheduler<E> {
    nodes: Vec<ScheduledNode<E>>,
    ready: BTreeSet<NodeId>,
    available: Vec<usize>,
}

impl<E> Scheduler<E> {
    /// Validate resource requests and cycles before any operation can run.
    /// GraphBuilder has already checked that all input handles belong to it.
    pub(super) fn new(nodes: Vec<Node<E>>, capacities: Vec<usize>) -> Result<Self, BuildError> {
        let mut successors = vec![Vec::new(); nodes.len()];
        let mut scheduled = Vec::with_capacity(nodes.len());
        for (index, mut node) in nodes.into_iter().enumerate() {
            let id = NodeId(index);
            node.dependencies.sort_unstable();
            node.dependencies.dedup();
            for dependency in &node.dependencies {
                successors[dependency.0].push(id);
            }
            let mut resources = BTreeMap::<ResourceId, usize>::new();
            for (resource, quantity) in node.resources {
                let invalid = || BuildError::InvalidResource { node: id, resource };
                let capacity = capacities.get(resource.0).ok_or_else(invalid)?;
                let total = resources.entry(resource).or_default();
                *total = total.checked_add(quantity).ok_or_else(invalid)?;
                if *total > *capacity {
                    return Err(invalid());
                }
            }
            scheduled.push(ScheduledNode {
                remaining: node.dependencies.len(),
                successors: Vec::new(),
                resources: resources.into_iter().filter(|(_, n)| *n != 0).collect(),
                operation: Some(node.operation),
                state: State::Pending,
            });
        }
        for (node, successors) in scheduled.iter_mut().zip(successors) {
            node.successors = successors;
        }
        let ready: BTreeSet<_> = scheduled
            .iter()
            .enumerate()
            .filter_map(|(i, node)| (node.remaining == 0).then_some(NodeId(i)))
            .collect();
        let mut validation_remaining: Vec<_> =
            scheduled.iter().map(|node| node.remaining).collect();
        let mut queue: VecDeque<_> = ready.iter().copied().collect();
        while let Some(node) = queue.pop_front() {
            for &next in &scheduled[node.0].successors {
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
            return Err(BuildError::Cycle { blocked });
        }
        Ok(Self {
            nodes: scheduled,
            ready,
            available: capacities,
        })
    }

    /// Reserve all resources and return the lowest-ID eligible operation.
    /// A ready node waiting for capacity does not block other ready nodes.
    pub(super) fn admit_next(&mut self) -> Option<(NodeId, Operation<E>)> {
        let id = self.ready.iter().copied().find(|id| {
            self.nodes[id.0]
                .resources
                .iter()
                .all(|(r, n)| self.available[r.0] >= *n)
        })?;
        self.ready.remove(&id);
        let node = &mut self.nodes[id.0];
        for &(resource, quantity) in &node.resources {
            self.available[resource.0] -= quantity;
        }
        node.state = State::Running;
        Some((
            id,
            node.operation.take().expect("admitted task executes once"),
        ))
    }

    /// Called once per joined task, after successful output publication or failure.
    pub(super) fn complete(&mut self, id: NodeId, succeeded: bool) {
        let node = &mut self.nodes[id.0];
        assert_eq!(
            node.state,
            State::Running,
            "only running tasks can complete"
        );
        node.state = if succeeded {
            State::Succeeded
        } else {
            State::Failed
        };
        for &(resource, quantity) in &node.resources {
            self.available[resource.0] += quantity;
        }
        if succeeded {
            for next in std::mem::take(&mut node.successors) {
                self.nodes[next.0].remaining -= 1;
                if self.nodes[next.0].remaining == 0 {
                    self.ready.insert(next);
                }
            }
        }
    }

    pub(super) fn is_complete(&self) -> bool {
        self.nodes.iter().all(|node| node.state == State::Succeeded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(dependencies: &[usize], resources: &[(usize, usize)]) -> Node<()> {
        Node {
            dependencies: dependencies.iter().copied().map(NodeId).collect(),
            resources: resources.iter().map(|&(r, n)| (ResourceId(r), n)).collect(),
            operation: Box::pin(async { Ok(()) }),
        }
    }

    fn admit(scheduler: &mut Scheduler<()>) -> Option<NodeId> {
        scheduler.admit_next().map(|(id, _operation)| id)
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
        assert_eq!(admit(&mut s), Some(NodeId(0)));
        assert_eq!(admit(&mut s), None);
        s.complete(NodeId(0), true);
        assert_eq!(admit(&mut s), Some(NodeId(1)));
        assert_eq!(admit(&mut s), Some(NodeId(2)));
        s.complete(NodeId(2), true);
        assert_eq!(admit(&mut s), None);
        s.complete(NodeId(1), true);
        assert_eq!(admit(&mut s), Some(NodeId(3)));
        s.complete(NodeId(3), true);
        assert!(s.is_complete());
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
        assert_eq!(admit(&mut s), Some(NodeId(0)));
        assert_eq!(admit(&mut s), Some(NodeId(2)));
        assert_eq!(admit(&mut s), None);
        s.complete(NodeId(0), true);
        assert_eq!(admit(&mut s), Some(NodeId(1)));
    }

    #[test]
    fn failure_releases_capacity_but_not_dependents() {
        let mut s = Scheduler::new(
            vec![node(&[], &[(0, 1)]), node(&[0], &[]), node(&[], &[(0, 1)])],
            vec![1],
        )
        .unwrap();
        assert_eq!(admit(&mut s), Some(NodeId(0)));
        s.complete(NodeId(0), false);
        assert_eq!(admit(&mut s), Some(NodeId(2)));
        s.complete(NodeId(2), true);
        assert_eq!(admit(&mut s), None);
        assert!(!s.is_complete());
    }

    #[test]
    #[should_panic(expected = "only running tasks can complete")]
    fn completing_twice_is_an_internal_invariant_violation() {
        let mut s = Scheduler::new(vec![node(&[], &[])], vec![]).unwrap();
        admit(&mut s);
        s.complete(NodeId(0), true);
        s.complete(NodeId(0), true);
    }

    #[test]
    fn validates_resources_and_cycles_including_blocked_descendants() {
        for resources in [
            vec![(0, 2)],
            vec![(0, 1), (0, 1)],
            vec![(1, 0)],
            vec![(0, usize::MAX), (0, 1)],
        ] {
            assert!(matches!(
                Scheduler::new(vec![node(&[], &resources)], vec![1]),
                Err(BuildError::InvalidResource { .. })
            ));
        }
        assert!(
            matches!(Scheduler::new(vec![node(&[1], &[]), node(&[0], &[]),
            node(&[1], &[]), node(&[], &[])], vec![]),
            Err(BuildError::Cycle { blocked }) if blocked == vec![NodeId(0), NodeId(1), NodeId(2)])
        );
        assert!(Scheduler::<()>::new(vec![], vec![]).unwrap().is_complete());
    }
}
