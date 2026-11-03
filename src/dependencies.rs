//! Card dependency graph helpers.

use std::collections::{HashMap, HashSet};

/// Would adding `new_dependencies` to `task_id` create a cycle?
///
/// `edges` is the existing `(task_id, blocked_by)` set **without** the outgoing
/// edges of `task_id`, because a replace operation removes them first. A cycle
/// exists when a proposed dependency can already reach the task through the
/// remaining edges (or is the task itself).
pub fn creates_cycle(
    edges: &[(String, String)],
    task_id: &str,
    new_dependencies: &[String],
) -> bool {
    let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();
    for (task, blocked_by) in edges {
        adjacency
            .entry(task.as_str())
            .or_default()
            .push(blocked_by.as_str());
    }

    for dependency in new_dependencies {
        if dependency == task_id {
            return true;
        }
        let mut stack = vec![dependency.as_str()];
        let mut seen = HashSet::new();
        while let Some(node) = stack.pop() {
            if node == task_id {
                return true;
            }
            if !seen.insert(node) {
                continue;
            }
            if let Some(next) = adjacency.get(node) {
                stack.extend(next.iter().copied());
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(task: &str, blocked_by: &str) -> (String, String) {
        (task.to_owned(), blocked_by.to_owned())
    }

    #[test]
    fn detects_self_and_transitive_cycles() {
        // A -> B -> C (A depends on B, B depends on C).
        let edges = vec![edge("A", "B"), edge("B", "C")];
        assert!(creates_cycle(&edges, "A", &["A".to_owned()]));
        // C -> A would close C -> A -> B -> C.
        assert!(creates_cycle(&edges, "C", &["A".to_owned()]));
        // C -> B also closes C -> B -> C.
        assert!(creates_cycle(&edges, "C", &["B".to_owned()]));
        // D -> A is safe.
        assert!(!creates_cycle(&edges, "D", &["A".to_owned()]));
    }

    #[test]
    fn ignores_edges_removed_by_the_replace() {
        // The caller removes task A's outgoing edges, but B -> A remains, so
        // adding A -> B would still close a cycle.
        let edges = vec![edge("B", "A")];
        assert!(creates_cycle(&edges, "A", &["B".to_owned()]));
        // B's own outgoing edges are the ones removed in this call.
        assert!(!creates_cycle(&edges, "B", &["A".to_owned()]));
    }

    #[test]
    fn a_diamond_still_closes_a_cycle() {
        // A -> B, A -> C, B -> D, C -> D.
        let edges = vec![
            edge("A", "B"),
            edge("A", "C"),
            edge("B", "D"),
            edge("C", "D"),
        ];
        assert!(!creates_cycle(&edges, "E", &["A".to_owned()]));
        assert!(creates_cycle(&edges, "D", &["A".to_owned()]));
    }
}
