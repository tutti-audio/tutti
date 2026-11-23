//! Compiler passes 2 and 3: reject unbroken cycles (Tarjan SCC), then
//! produce the one deterministic topological order.
//!
//! Both run over the **direct** dependencies only — audio `Edge::Direct(Node)`
//! and `EventEdge::Direct` — because a feedback edge reads its source `MaxBlock`
//! frames in the past and is not a predecessor this block.

use std::collections::HashMap;
use std::hash::Hash;

/// Strongly connected components of a graph over `0..n` with successor lists
/// `succ`. Iterative Tarjan, so a deep chain cannot overflow the stack.
///
/// Returns the component id of every vertex.
pub(super) fn scc(succ: &[Vec<usize>]) -> Vec<usize> {
    let n = succ.len();
    const UNSEEN: usize = usize::MAX;
    let mut index = vec![UNSEEN; n];
    let mut low = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut comp = vec![UNSEEN; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut next_index = 0usize;
    let mut next_comp = 0usize;
    // (vertex, next successor position)
    let mut call: Vec<(usize, usize)> = Vec::new();

    for root in 0..n {
        if index[root] != UNSEEN {
            continue;
        }
        call.push((root, 0));
        while let Some(&mut (v, ref mut pos)) = call.last_mut() {
            if *pos == 0 && index[v] == UNSEEN {
                index[v] = next_index;
                low[v] = next_index;
                next_index += 1;
                stack.push(v);
                on_stack[v] = true;
            }
            if let Some(&w) = succ[v].get(*pos) {
                *pos += 1;
                if index[w] == UNSEEN {
                    call.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
                continue;
            }
            // All successors done: close v.
            call.pop();
            if let Some(&(parent, _)) = call.last() {
                low[parent] = low[parent].min(low[v]);
            }
            if low[v] == index[v] {
                loop {
                    let w = stack.pop().expect("v is on the stack");
                    on_stack[w] = false;
                    comp[w] = next_comp;
                    if w == v {
                        break;
                    }
                }
                next_comp += 1;
            }
        }
    }
    comp
}

/// A topological sort's answer: the vertices in order, and the ones a cycle
/// left unplaceable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Sorted<T> {
    /// Every vertex that is not on or downstream of a cycle, dependencies first.
    pub order: Vec<T>,
    /// The vertices left over, in vertex order. Empty exactly when the graph is
    /// acyclic.
    pub cyclic: Vec<T>,
}

/// Kahn's algorithm over vertices `0..preds.len()`, where `preds[i]` lists
/// `i`'s predecessors **with multiplicity** (one entry per edge).
///
/// The ready set is a stack seeded smallest-index-on-top, and each vertex's
/// newly ready dependents are pushed so the smallest pops next: ties break
/// toward the smaller index, so the order is a function of `preds`.
pub(crate) fn kahn(preds: &[Vec<usize>]) -> Sorted<usize> {
    let n = preds.len();
    let mut in_degree: Vec<usize> = preds.iter().map(Vec::len).collect();
    // Sinks are visited in index order, so every `dependents` list is sorted
    // by construction, and the batch that becomes ready comes out sorted.
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (node, ps) in preds.iter().enumerate() {
        for &p in ps {
            dependents[p].push(node);
        }
    }
    let mut stack: Vec<usize> = (0..n).rev().filter(|&i| in_degree[i] == 0).collect();
    let mut order = Vec::with_capacity(n);
    while let Some(v) = stack.pop() {
        order.push(v);
        // A dependent is ready when its last edge is released, whatever order
        // the edges are visited in; visiting descending pushes the smallest last.
        stack.extend(dependents[v].iter().rev().copied().filter(|&d| {
            in_degree[d] -= 1;
            in_degree[d] == 0
        }));
    }
    // A vertex is placed exactly when its in-degree reached zero.
    let cyclic = (0..n).filter(|&i| in_degree[i] > 0).collect();
    Sorted { order, cyclic }
}

/// [`kahn`] over arbitrary vertices: `nodes` is the vertex order (ties break
/// toward the earlier one) and `preds` yields a node's predecessors, with
/// multiplicity. A predecessor that is not in `nodes` is ignored.
pub(crate) fn sort_nodes<N, I>(nodes: &[N], preds: impl Fn(N) -> I) -> Sorted<N>
where
    N: Copy + Eq + Hash,
    I: IntoIterator<Item = N>,
{
    let index: HashMap<N, usize> = nodes.iter().enumerate().map(|(i, &n)| (n, i)).collect();
    let preds: Vec<Vec<usize>> = nodes
        .iter()
        .map(|&n| {
            preds(n)
                .into_iter()
                .filter_map(|p| index.get(&p).copied())
                .collect()
        })
        .collect();
    let Sorted { order, cyclic } = kahn(&preds);
    let named = |v: Vec<usize>| v.into_iter().map(|i| nodes[i]).collect();
    Sorted {
        order: named(order),
        cyclic: named(cyclic),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A two-cycle and a self-loop are components; a chain is not.
    #[test]
    fn scc_finds_cycles() {
        // 0 <-> 1, 2 -> 2, 3 -> 0
        let succ = vec![vec![1], vec![0], vec![2], vec![0]];
        let c = scc(&succ);
        assert_eq!(c[0], c[1]);
        assert_ne!(c[0], c[2]);
        assert_ne!(c[0], c[3]);
        assert_ne!(c[2], c[3]);
    }

    /// Ready vertices come out smallest first, a vertex only after all its
    /// edges (counted with multiplicity), and a cycle's vertices, with what
    /// hangs off it, are the leftover.
    #[test]
    fn kahn_orders_by_index_and_leaves_the_cycle() {
        // 3 <- {0, 0, 2}; 1 free; 4 <-> 5; 6 <- 4.
        let preds = vec![
            vec![],
            vec![],
            vec![],
            vec![0, 0, 2],
            vec![5],
            vec![4],
            vec![4],
        ];
        let sorted = kahn(&preds);
        assert_eq!(sorted.order, vec![0, 1, 2, 3]);
        assert_eq!(sorted.cyclic, vec![4, 5, 6]);
    }

    /// A newly ready batch is taken before older ready vertices.
    #[test]
    fn kahn_pops_the_newest_ready_batch_first() {
        // 0 -> 2, 1 free: 0 pops, readies 2, which pops before 1.
        let sorted = kahn(&[vec![], vec![], vec![0]]);
        assert_eq!(sorted.order, vec![0, 2, 1]);
    }

    /// `sort_nodes` names vertices by `nodes`' order and drops unknown
    /// predecessors.
    #[test]
    fn sort_nodes_ignores_unknown_predecessors() {
        let sorted = sort_nodes(&[10u8, 20, 30], |n| match n {
            10 => vec![30, 99],
            _ => vec![],
        });
        assert_eq!(sorted.order, vec![20, 30, 10]);
        assert!(sorted.cyclic.is_empty());
    }
}
