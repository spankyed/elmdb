//! Graph algorithms over a projection: the entities of a plan's set as nodes, numbered in set order, and the edges of
//! the kinds a step names between two of them, as compressed sparse rows (`docs/roadmap/graph-algorithms.md`). Pure:
//! the plan builds the projection from its read, and these answer from it alone.
//!
//! Everything here is deterministic: nodes are visited in their numbering, each node's edges in the order the plan
//! added them, and nothing runs in parallel, so one projection answers the same, bit for bit, on every platform.

/// A directed graph of `nodes` nodes as compressed sparse rows: node `v`'s edges lead to
/// `targets[offsets[v]..offsets[v + 1]]`
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Projection {
    offsets: Vec<u32>,
    targets: Vec<u32>,
}

impl Projection {
    /// An empty graph, to which nodes are added in order with `node`
    pub fn new() -> Self {
        Self { offsets: vec![0], targets: Vec::new() }
    }

    /// Adds the next node, with edges to the nodes `targets` names (each below the number of nodes the graph will hold)
    pub fn node(&mut self, targets: impl IntoIterator<Item = u32>) {
        self.targets.extend(targets);
        self.offsets.push(u32::try_from(self.targets.len()).expect("fewer than 2^32 edges"));
    }

    pub fn nodes(&self) -> usize {
        self.offsets.len() - 1
    }

    pub fn edges(&self) -> usize {
        self.targets.len()
    }

    fn out(&self, v: usize) -> &[u32] {
        &self.targets[self.offsets[v] as usize..self.offsets[v + 1] as usize]
    }
}

/// The weak components, the groups the edges join whichever way they point: each a list of nodes in their numbering,
/// largest first, ties by first member
pub fn weak_components(graph: &Projection) -> Vec<Vec<u32>> {
    let n = graph.nodes();
    // Union-find whose root is always the lowest node of its set, so the answer doesn't depend on the order of unions
    let mut parent: Vec<u32> = (0..n as u32).collect();
    fn root(parent: &mut [u32], mut v: u32) -> u32 {
        while parent[v as usize] != v {
            let up = parent[parent[v as usize] as usize];
            parent[v as usize] = up;
            v = up;
        }
        v
    }
    for v in 0..n {
        for &w in graph.out(v) {
            let (a, b) = (root(&mut parent, v as u32), root(&mut parent, w));
            if a != b {
                parent[a.max(b) as usize] = a.min(b);
            }
        }
    }
    let mut group_of: Vec<Option<usize>> = vec![None; n];
    let mut groups: Vec<Vec<u32>> = Vec::new();
    for v in 0..n as u32 {
        let r = root(&mut parent, v) as usize;
        let at = *group_of[r].get_or_insert_with(|| {
            groups.push(Vec::new());
            groups.len() - 1
        });
        groups[at].push(v);
    }
    // Groups are in order of first member already, and the sort is stable
    groups.sort_by_key(|group| std::cmp::Reverse(group.len()));
    groups
}

/// What `rank` found: each node's score, summing to 1, and whether the scores settled within the rounds allowed
#[derive(Debug, Clone, PartialEq)]
pub struct Ranked {
    pub scores: Vec<f64>,
    pub converged: bool,
    pub iterations: u32,
}

/// Personalized PageRank by power iteration: a walker follows one of a node's edges with probability `damping`, and
/// otherwise jumps back to a seed, chosen evenly; a node with no edges sends the walker back to the seeds. Every node
/// as a seed is PageRank. Stops when the scores change by less than `tolerance` (the L1 distance between two rounds),
/// or after `iterations` rounds. `None` when no seed is a node of the graph
pub fn rank(graph: &Projection, seeds: &[u32], damping: f64, iterations: u32, tolerance: f64) -> Option<Ranked> {
    let n = graph.nodes();
    let mut teleport = vec![0.0; n];
    let mut seeded = 0usize;
    for &s in seeds {
        if let Some(slot) = teleport.get_mut(s as usize).filter(|slot| **slot == 0.0) {
            *slot = 1.0;
            seeded += 1;
        }
    }
    if seeded == 0 {
        return None;
    }
    let share = 1.0 / seeded as f64;
    for slot in &mut teleport {
        *slot *= share;
    }
    let mut scores = teleport.clone();
    let mut next = vec![0.0; n];
    for round in 1..=iterations {
        next.iter_mut().for_each(|x| *x = 0.0);
        let mut dangling = 0.0;
        for (v, &score) in scores.iter().enumerate() {
            let out = graph.out(v);
            if out.is_empty() {
                dangling += score;
            } else {
                let each = score / out.len() as f64;
                for &w in out {
                    next[w as usize] += each;
                }
            }
        }
        let back = damping * dangling + (1.0 - damping);
        let mut change = 0.0;
        for v in 0..n {
            next[v] = damping * next[v] + back * teleport[v];
            change += (next[v] - scores[v]).abs();
        }
        std::mem::swap(&mut scores, &mut next);
        if change < tolerance {
            return Some(Ranked { scores, converged: true, iterations: round });
        }
    }
    Some(Ranked { scores, converged: false, iterations })
}

/// Leiden communities (Traag, Waltman and van Eck, 2019) by modularity at `resolution`, through leiden-rs: each edge
/// joins its ends whichever way it points, and counts once. Each community is connected, which Leiden guarantees and
/// Louvain doesn't. Each a list of nodes in their numbering, largest first, ties by first member. Leiden's random
/// order comes from `seed`, so a seed and a graph answer the same partition on every platform; it runs on one thread,
/// leiden-rs's parallel moves being compiled out, and stops after `iterations` rounds or when a round no longer
/// improves the partition
pub fn communities(graph: &Projection, resolution: f64, seed: u64, iterations: u32) -> Vec<Vec<u32>> {
    let n = graph.nodes();
    if n == 0 {
        return Vec::new();
    }
    let mut data = leiden_rs::GraphDataBuilder::new(n);
    for v in 0..n {
        for &w in graph.out(v) {
            data.add_edge(v, w as usize, 1.0).expect("a node of the graph, and a finite weight");
        }
    }
    let data = data.build().expect("a graph of its own nodes");
    let config = leiden_rs::LeidenConfig {
        max_iterations: iterations as usize,
        resolution,
        seed: Some(seed),
        ..leiden_rs::LeidenConfig::default()
    };
    let found = leiden_rs::Leiden::new(config).run(&data).expect("leiden-rs runs over a built graph");
    let mut group_of: Vec<Option<usize>> = vec![None; n];
    let mut groups: Vec<Vec<u32>> = Vec::new();
    for v in 0..n {
        let community = found.partition.community_of(v);
        if community >= group_of.len() {
            group_of.resize(community + 1, None);
        }
        let at = *group_of[community].get_or_insert_with(|| {
            groups.push(Vec::new());
            groups.len() - 1
        });
        groups[at].push(v as u32);
    }
    groups.sort_by_key(|group| std::cmp::Reverse(group.len()));
    groups
}

#[cfg(test)]
mod tests;
