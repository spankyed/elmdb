//! Graph algorithms over a projection: the entities of a plan's set as nodes, numbered in set order, and the edges of
//! the kinds a step names between two of them, as compressed sparse rows (`docs/roadmap/graph-algorithms.md`). Pure:
//! the plan builds the projection from its read, and these answer from it alone.
//!
//! Everything here is deterministic: nodes are visited in their numbering, each node's edges in the order the plan
//! added them, and nothing runs in parallel, so one projection answers the same, bit for bit, on every platform.

/// A directed graph of `nodes` nodes as compressed sparse rows: node `v`'s edges lead to
/// `targets[offsets[v]..offsets[v + 1]]`, and in a weighted graph weigh `weights` over the same range
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Projection {
    offsets: Vec<u32>,
    targets: Vec<u32>,
    weights: Option<Vec<f64>>,
}

impl Projection {
    /// An empty graph, to which nodes are added in order with `node`
    pub fn new() -> Self {
        Self { offsets: vec![0], targets: Vec::new(), weights: None }
    }

    /// An empty weighted graph, to which nodes are added in order with `weighted_node`
    pub fn weighted() -> Self {
        Self { offsets: vec![0], targets: Vec::new(), weights: Some(Vec::new()) }
    }

    /// Adds the next node, with edges to the nodes `targets` names (each below the number of nodes the graph will hold)
    pub fn node(&mut self, targets: impl IntoIterator<Item = u32>) {
        debug_assert!(self.weights.is_none(), "a weighted graph's nodes come with their weights");
        self.targets.extend(targets);
        self.offsets.push(u32::try_from(self.targets.len()).expect("fewer than 2^32 edges"));
    }

    /// Adds the next node of a weighted graph, with an edge to each node named and the edge's weight
    pub fn weighted_node(&mut self, edges: impl IntoIterator<Item = (u32, f64)>) {
        let weights = self.weights.as_mut().expect("a weighted graph");
        for (target, weight) in edges {
            self.targets.push(target);
            weights.push(weight);
        }
        self.offsets.push(u32::try_from(self.targets.len()).expect("fewer than 2^32 edges"));
    }

    /// Every edge's weight added up, for a weighted graph
    pub fn total_weight(&self) -> Option<f64> {
        self.weights.as_ref().map(|weights| weights.iter().sum())
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

    /// Node `v`'s edges' weights, in the order of `out(v)`, for a weighted graph
    fn out_weights(&self, v: usize) -> Option<&[f64]> {
        self.weights.as_ref().map(|weights| &weights[self.offsets[v] as usize..self.offsets[v + 1] as usize])
    }
}

/// Groups of nodes, held flat: group `i` is `members[offsets[i]..offsets[i + 1]]`. Every answer in groups orders them the
/// same way, largest first, ties by lowest member, and each group's members in their numbering
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Groups {
    members: Vec<u32>,
    offsets: Vec<u32>,
}

impl Groups {
    /// The groups `labels` names, one label per node, each label below the number of nodes: a group is the nodes that
    /// share a label, so a label's own value orders nothing
    pub fn from_labels(labels: &[u32]) -> Self {
        let n = labels.len();
        let mut size = vec![0u32; n];
        let mut lowest = vec![u32::MAX; n];
        for (v, &label) in labels.iter().enumerate() {
            size[label as usize] += 1;
            lowest[label as usize] = lowest[label as usize].min(v as u32);
        }
        let mut order: Vec<u32> = (0..n as u32).filter(|&label| size[label as usize] > 0).collect();
        order.sort_unstable_by_key(|&label| (std::cmp::Reverse(size[label as usize]), lowest[label as usize]));
        // Each label's next free place in `members`, in the groups' order
        let mut next = vec![0u32; n];
        let mut offsets = Vec::with_capacity(order.len() + 1);
        let mut at = 0u32;
        for &label in &order {
            offsets.push(at);
            next[label as usize] = at;
            at += size[label as usize];
        }
        offsets.push(at);
        // Nodes in their numbering, so each group's members come out in it
        let mut members = vec![0u32; n];
        for (v, &label) in labels.iter().enumerate() {
            members[next[label as usize] as usize] = v as u32;
            next[label as usize] += 1;
        }
        Groups { members, offsets }
    }

    pub fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    pub fn get(&self, group: usize) -> &[u32] {
        &self.members[self.offsets[group] as usize..self.offsets[group + 1] as usize]
    }

    pub fn iter(&self) -> impl Iterator<Item = &[u32]> {
        (0..self.len()).map(|group| self.get(group))
    }

    /// Each group's size, without reading its members
    pub fn sizes(&self) -> impl Iterator<Item = u32> + '_ {
        self.offsets.windows(2).map(|pair| pair[1] - pair[0])
    }

    #[cfg(test)]
    pub fn to_vecs(&self) -> Vec<Vec<u32>> {
        self.iter().map(<[u32]>::to_vec).collect()
    }
}

/// The weak components, the groups the edges join whichever way they point
pub fn weak_components(graph: &Projection) -> Groups {
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
    let labels: Vec<u32> = (0..n as u32).map(|v| root(&mut parent, v)).collect();
    Groups::from_labels(&labels)
}

/// The strong components, the groups whose nodes each reach every other following the edges' direction. A node on no
/// cycle is a group of its own. Reversing
/// every edge leaves them as they are. Tarjan's algorithm, walking with a stack of its own rather than recursing, so a
/// long chain can't overflow the thread's
pub fn strong_components(graph: &Projection) -> Groups {
    const UNSEEN: u32 = u32::MAX;
    let n = graph.nodes();
    // Each node's place in the walk, and the earliest place it reaches among the nodes still on `open`
    let mut place = vec![UNSEEN; n];
    let mut low = vec![0u32; n];
    let mut on_open = vec![false; n];
    // The nodes seen whose group isn't settled, and the walk: each node being visited with the next edge to follow
    let mut open: Vec<u32> = Vec::new();
    let mut walk: Vec<(u32, usize)> = Vec::new();
    let mut next = 0u32;
    // Each node's group, numbered as the walk settles them
    let mut labels = vec![0u32; n];
    let mut settled = 0u32;
    let mut visit = |v: u32, place: &mut [u32], low: &mut [u32], on_open: &mut [bool], open: &mut Vec<u32>| {
        // Each node is placed once, so the walk and `open` never outgrow the graph. Checked in test builds, where a
        // walk that revisited nodes would otherwise grow without bound until the machine ran out of memory
        debug_assert_eq!(place[v as usize], UNSEEN, "a node is placed once");
        place[v as usize] = next;
        low[v as usize] = next;
        next += 1;
        on_open[v as usize] = true;
        open.push(v);
    };
    for start in 0..n as u32 {
        if place[start as usize] != UNSEEN {
            continue;
        }
        visit(start, &mut place, &mut low, &mut on_open, &mut open);
        walk.push((start, 0));
        while let Some(&(v, at)) = walk.last() {
            let out = graph.out(v as usize);
            if let Some(&w) = out.get(at) {
                walk.last_mut().expect("the walk holds v").1 += 1;
                if place[w as usize] == UNSEEN {
                    visit(w, &mut place, &mut low, &mut on_open, &mut open);
                    walk.push((w, 0));
                } else if on_open[w as usize] {
                    low[v as usize] = low[v as usize].min(place[w as usize]);
                }
                continue;
            }
            walk.pop();
            if let Some(&(parent, _)) = walk.last() {
                low[parent as usize] = low[parent as usize].min(low[v as usize]);
            }
            if low[v as usize] == place[v as usize] {
                loop {
                    let w = open.pop().expect("v is still open");
                    on_open[w as usize] = false;
                    labels[w as usize] = settled;
                    if w == v {
                        break;
                    }
                }
                settled += 1;
            }
        }
    }
    Groups::from_labels(&labels)
}

/// What `topological_order` found: every node once, and how many cycles kept their members together
#[derive(Debug, Clone, PartialEq)]
pub struct Ordered {
    pub order: Vec<u32>,
    pub cycles: u32,
}

/// The nodes in an order where every edge leads from a node to a later one, as far as the edges allow: the members of a
/// strong component of more than one node, or of one with an edge to itself, can't be ordered so, and stay together in
/// their numbering where the component falls. What the edges leave free goes lowest first, so the answer is the order
/// the edges allow that is nearest the numbering. Kahn's algorithm over the strong components, taking each time the
/// ready component whose first member is lowest
pub fn topological_order(graph: &Projection) -> Ordered {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;
    let n = graph.nodes();
    let groups = strong_components(graph);
    let mut group_of = vec![0u32; n];
    for (at, group) in groups.iter().enumerate() {
        for &v in group {
            group_of[v as usize] = at as u32;
        }
    }
    let mut cycles = 0u32;
    let mut waiting = vec![0u32; groups.len()];
    for (at, group) in groups.iter().enumerate() {
        // A component of more than one node holds an edge between two of them, so an edge inside is what makes a cycle
        let mut looped = false;
        for &v in group {
            for &w in graph.out(v as usize) {
                let to = group_of[w as usize];
                if to as usize == at {
                    looped = true;
                } else {
                    waiting[to as usize] += 1;
                }
            }
        }
        cycles += u32::from(looped);
    }
    // A group's members are in their numbering, so its first is its lowest
    let mut ready: BinaryHeap<Reverse<(u32, u32)>> =
        (0..groups.len()).filter(|&at| waiting[at] == 0).map(|at| Reverse((groups.get(at)[0], at as u32))).collect();
    let mut order = Vec::with_capacity(n);
    while let Some(Reverse((_, at))) = ready.pop() {
        let group = groups.get(at as usize);
        order.extend_from_slice(group);
        for &v in group {
            for &w in graph.out(v as usize) {
                let to = group_of[w as usize];
                if to == at {
                    continue;
                }
                waiting[to as usize] -= 1;
                if waiting[to as usize] == 0 {
                    ready.push(Reverse((groups.get(to as usize)[0], to)));
                }
            }
        }
    }
    Ordered { order, cycles }
}

/// Jaccard's similarity of two neighbour sets of `a` and `b` members sharing `shared`: the shared over the union
pub fn jaccard(shared: usize, a: usize, b: usize) -> f64 {
    shared as f64 / (a + b - shared) as f64
}

/// What one shared neighbour of `degree` adds to Adamic and Adar's similarity: 1 / ln(degree). A shared neighbour is
/// linked with both entities, so its degree is at least 2. The logarithm is libm's, a port of musl's, rather than
/// std's, which calls the platform's maths library and may differ from it in the last bit
pub fn adamic_adar(degree: usize) -> f64 {
    1.0 / libm::log(degree as f64)
}

/// What `rank` found: each node's score, summing to 1, and whether the scores settled within the rounds allowed
#[derive(Debug, Clone, PartialEq)]
pub struct Ranked {
    pub scores: Vec<f64>,
    pub converged: bool,
    pub iterations: u32,
}

/// Personalized PageRank by power iteration: a walker follows one of a node's edges with probability `damping`, and
/// otherwise jumps back to a seed, each in proportion to its weight; a node with no edges sends the walker back to the
/// seeds. Every node as a seed of one weight is PageRank. In a weighted graph the walker follows an edge in proportion to
/// its weight among its node's, and a node whose edges weigh nothing sends it back to the seeds too. Stops when the
/// scores change by less than `tolerance` (the L1 distance between two rounds), or after `iterations` rounds. `None`
/// when no seed is a node of the graph
pub fn rank(graph: &Projection, seeds: &[(u32, f64)], damping: f64, iterations: u32, tolerance: f64) -> Option<Ranked> {
    let n = graph.nodes();
    let mut teleport = vec![0.0; n];
    let mut seeded = 0.0;
    for &(s, weight) in seeds {
        if let Some(slot) = teleport.get_mut(s as usize) {
            *slot += weight;
            seeded += weight;
        }
    }
    if seeded == 0.0 {
        return None;
    }
    for slot in &mut teleport {
        *slot /= seeded;
    }
    // Each node's edges' weights added up, once: what a walker's share of each edge is out of
    let out_weight: Option<Vec<f64>> =
        graph.weights.as_ref().map(|_| (0..n).map(|v| graph.out_weights(v).unwrap_or_default().iter().sum()).collect());
    let mut scores = teleport.clone();
    let mut next = vec![0.0; n];
    for round in 1..=iterations {
        next.iter_mut().for_each(|x| *x = 0.0);
        let mut dangling = 0.0;
        for (v, &score) in scores.iter().enumerate() {
            let out = graph.out(v);
            match (&out_weight, graph.out_weights(v)) {
                (Some(totals), Some(weights)) => {
                    if totals[v] == 0.0 {
                        dangling += score;
                    } else {
                        for (&w, &weight) in out.iter().zip(weights) {
                            next[w as usize] += score * weight / totals[v];
                        }
                    }
                }
                _ if out.is_empty() => dangling += score,
                _ => {
                    let each = score / out.len() as f64;
                    for &w in out {
                        next[w as usize] += each;
                    }
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
/// joins its ends whichever way it points, and counts once, by its weight in a weighted graph. Each community is
/// connected, which Leiden guarantees and Louvain doesn't. Leiden's random order comes from `seed`, so a seed and a graph
/// answer the same partition on every platform; it runs on one thread, leiden-rs's parallel moves being compiled out,
/// and stops after `iterations` rounds or when a round no longer improves the partition
pub fn communities(graph: &Projection, resolution: f64, seed: u64, iterations: u32) -> Groups {
    let n = graph.nodes();
    if n == 0 {
        return Groups::default();
    }
    let mut data = leiden_rs::GraphDataBuilder::new(n);
    for v in 0..n {
        let weights = graph.out_weights(v);
        for (at, &w) in graph.out(v).iter().enumerate() {
            let weight = weights.map_or(1.0, |weights| weights[at]);
            data.add_edge(v, w as usize, weight).expect("a node of the graph, and a finite weight of at least 0");
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
    // leiden-rs numbers communities from 0 and has no more of them than nodes
    let labels: Vec<u32> = (0..n).map(|v| found.partition.community_of(v) as u32).collect();
    Groups::from_labels(&labels)
}

#[cfg(test)]
pub(crate) mod tests;
