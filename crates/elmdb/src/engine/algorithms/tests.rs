//! The algorithms against plain references over random graphs, the invariants any correct implementation keeps, and
//! one known answer: Zachary's karate club, against networkx 3.4.2's `pagerank` (alpha 0.85, unweighted, run to a
//! tolerance of 1e-15), the club's 78 friendships followed both ways as networkx follows an undirected graph's.

use proptest::prelude::*;

use super::{Projection, communities, rank, weak_components};

const KARATE_EDGES: [(u32, u32); 78] = [
    (0, 1),
    (0, 2),
    (0, 3),
    (0, 4),
    (0, 5),
    (0, 6),
    (0, 7),
    (0, 8),
    (0, 10),
    (0, 11),
    (0, 12),
    (0, 13),
    (0, 17),
    (0, 19),
    (0, 21),
    (0, 31),
    (1, 2),
    (1, 3),
    (1, 7),
    (1, 13),
    (1, 17),
    (1, 19),
    (1, 21),
    (1, 30),
    (2, 3),
    (2, 7),
    (2, 8),
    (2, 9),
    (2, 13),
    (2, 27),
    (2, 28),
    (2, 32),
    (3, 7),
    (3, 12),
    (3, 13),
    (4, 6),
    (4, 10),
    (5, 6),
    (5, 10),
    (5, 16),
    (6, 16),
    (8, 30),
    (8, 32),
    (8, 33),
    (9, 33),
    (13, 33),
    (14, 32),
    (14, 33),
    (15, 32),
    (15, 33),
    (18, 32),
    (18, 33),
    (19, 33),
    (20, 32),
    (20, 33),
    (22, 32),
    (22, 33),
    (23, 25),
    (23, 27),
    (23, 29),
    (23, 32),
    (23, 33),
    (24, 25),
    (24, 27),
    (24, 31),
    (25, 31),
    (26, 29),
    (26, 33),
    (27, 33),
    (28, 31),
    (28, 33),
    (29, 32),
    (29, 33),
    (30, 32),
    (30, 33),
    (31, 32),
    (31, 33),
    (32, 33),
];
const KARATE_PAGERANK: [f64; 34] = [
    0.09699728538830416,
    0.05287692406114843,
    0.05707850948846181,
    0.03585985778641362,
    0.0219779523645932,
    0.02911115467837563,
    0.02911115467837563,
    0.02449049703528381,
    0.029766056081016026,
    0.014309397129032293,
    0.0219779523645932,
    0.009564745492136189,
    0.01464489201187823,
    0.029536456151914504,
    0.014535993997919994,
    0.014535993997919994,
    0.016784005444192826,
    0.01455867720902252,
    0.014535993997919994,
    0.019604636325653214,
    0.014535993997919994,
    0.01455867720902252,
    0.014535993997919994,
    0.03152251477667457,
    0.021076033559220962,
    0.021006197394491014,
    0.015044038082724129,
    0.025639767482845847,
    0.0195734594638274,
    0.026288537695111996,
    0.024590155248578993,
    0.03715808706914282,
    0.07169322600574761,
    0.10091918233261699,
];
const KARATE_FROM_0: [f64; 34] = [
    0.2663736031484216,
    0.06488790798684252,
    0.05494775351279116,
    0.04623141631952724,
    0.030943355919900276,
    0.03776458388393527,
    0.03776458388393527,
    0.03149940978209271,
    0.0270616426834219,
    0.007230558508746669,
    0.030943355919900276,
    0.014151097667259721,
    0.02070054831252597,
    0.03405940924225212,
    0.004915562772105624,
    0.004915562772105624,
    0.016049948150671678,
    0.020279400088239204,
    0.004915562772105624,
    0.022839399548398618,
    0.004915562772105624,
    0.020279400088239204,
    0.004915562772105624,
    0.011586470513438848,
    0.008630131700717046,
    0.008236655761026581,
    0.004422527292647352,
    0.011645462477901491,
    0.011052310300618429,
    0.00876483685876577,
    0.015644344449266885,
    0.026977071472035083,
    0.03325501146276764,
    0.051199989203185206,
];

fn graph(n: usize, edges: &[(u32, u32)]) -> Projection {
    let mut graph = Projection::new();
    for v in 0..n as u32 {
        graph.node(edges.iter().filter(|(a, _)| *a == v).map(|(_, b)| *b));
    }
    graph
}

fn karate() -> Projection {
    let both: Vec<(u32, u32)> = KARATE_EDGES.iter().flat_map(|&(a, b)| [(a, b), (b, a)]).collect();
    graph(34, &both)
}

/// PageRank as its definition reads, over an adjacency matrix: what the walker holds after each round, until it stops
/// moving
fn dense(n: usize, edges: &[(u32, u32)], seeds: &[u32], damping: f64) -> Vec<f64> {
    let mut seeded = vec![false; n];
    seeds.iter().for_each(|&s| seeded[s as usize] = true);
    let count = seeded.iter().filter(|s| **s).count() as f64;
    let teleport: Vec<f64> = seeded.iter().map(|&s| if s { 1.0 / count } else { 0.0 }).collect();
    let mut matrix = vec![vec![0.0; n]; n];
    for &(a, b) in edges {
        matrix[a as usize][b as usize] += 1.0;
    }
    let mut p = teleport.clone();
    for _ in 0..100_000 {
        let mut q = vec![0.0; n];
        for a in 0..n {
            let degree: f64 = matrix[a].iter().sum();
            for b in 0..n {
                q[b] += if degree == 0.0 { p[a] * teleport[b] } else { p[a] * matrix[a][b] / degree };
            }
        }
        let q: Vec<f64> = (0..n).map(|b| damping * q[b] + (1.0 - damping) * teleport[b]).collect();
        let change: f64 = q.iter().zip(&p).map(|(x, y)| (x - y).abs()).sum();
        p = q;
        if change < 1e-15 {
            break;
        }
    }
    p
}

/// The weak components by breadth-first search over the edges taken both ways, each sorted, in order of first member
fn reached(n: usize, edges: &[(u32, u32)]) -> Vec<Vec<u32>> {
    let mut seen = vec![false; n];
    let mut groups = Vec::new();
    for start in 0..n {
        if seen[start] {
            continue;
        }
        seen[start] = true;
        let mut group = vec![start as u32];
        let mut at = 0;
        while at < group.len() {
            let v = group[at];
            for &(a, b) in edges {
                for (from, to) in [(a, b), (b, a)] {
                    if from == v && !seen[to as usize] {
                        seen[to as usize] = true;
                        group.push(to);
                    }
                }
            }
            at += 1;
        }
        group.sort_unstable();
        groups.push(group);
    }
    groups
}

fn graphs() -> impl Strategy<Value = (usize, Vec<(u32, u32)>, Vec<u32>)> {
    (1usize..=12).prop_flat_map(|n| {
        let node = 0..n as u32;
        (Just(n), prop::collection::vec((node.clone(), node.clone()), 0..=30), prop::collection::vec(node, 1..=3))
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn rank_agrees_with_the_dense_reference((n, edges, seeds) in graphs()) {
        let found = rank(&graph(n, &edges), &seeds, 0.85, 100_000, 1e-15).unwrap();
        let expected = dense(n, &edges, &seeds, 0.85);
        for (got, want) in found.scores.iter().zip(&expected) {
            prop_assert!((got - want).abs() < 1e-9, "{got} against {want}");
        }
        prop_assert!((found.scores.iter().sum::<f64>() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn components_agree_with_breadth_first_search((n, edges, _) in graphs()) {
        let mut found = weak_components(&graph(n, &edges));
        prop_assert!(found.windows(2).all(|pair| pair[0].len() >= pair[1].len()), "largest first");
        found.sort_by_key(|group| group[0]);
        prop_assert_eq!(found, reached(n, &edges));
    }

    #[test]
    fn relabelling_the_nodes_permutes_the_scores((n, edges, seeds) in graphs(), shift in 0u32..12) {
        let to = |v: u32| (v + shift) % n as u32;
        let moved: Vec<(u32, u32)> = edges.iter().map(|&(a, b)| (to(a), to(b))).collect();
        let before = rank(&graph(n, &edges), &seeds, 0.85, 100_000, 1e-15).unwrap().scores;
        let after = rank(&graph(n, &moved), &seeds.iter().map(|&s| to(s)).collect::<Vec<_>>(), 0.85, 100_000, 1e-15)
            .unwrap()
            .scores;
        for v in 0..n as u32 {
            prop_assert!((before[v as usize] - after[to(v) as usize]).abs() < 1e-9);
        }
    }
}

#[test]
fn the_karate_club_scores_as_networkx_does() {
    let all: Vec<u32> = (0..34).collect();
    for (seeds, expected) in [(&all[..], &KARATE_PAGERANK), (&[0][..], &KARATE_FROM_0)] {
        let found = rank(&karate(), seeds, 0.85, 100_000, 1e-15).unwrap();
        for (v, (got, want)) in found.scores.iter().zip(expected.iter()).enumerate() {
            assert!((got - want).abs() < 1e-9, "member {v}: {got} against networkx's {want}");
        }
    }
    assert_eq!(weak_components(&karate()), [all]);
}

#[test]
fn a_cycle_and_a_complete_graph_score_every_node_alike() {
    let cycle: Vec<(u32, u32)> = (0..5).map(|v| (v, (v + 1) % 5)).collect();
    let complete: Vec<(u32, u32)> = (0..5).flat_map(|a| (0..5).filter(move |&b| b != a).map(move |b| (a, b))).collect();
    let all: Vec<u32> = (0..5).collect();
    for edges in [cycle, complete] {
        let found = rank(&graph(5, &edges), &all, 0.85, 1000, 1e-15).unwrap();
        assert!(found.scores.iter().all(|s| (s - 0.2).abs() < 1e-12), "{:?}", found.scores);
    }
}

#[test]
fn a_node_without_edges_sends_its_walker_back_to_the_seeds() {
    // 0 → 1, and 1 leads nowhere: what reaches 1 returns to 0, the seed, so 2, unreached, holds nothing
    let found = rank(&graph(3, &[(0, 1)]), &[0], 0.85, 1000, 1e-15).unwrap();
    let total: f64 = found.scores.iter().sum();
    assert!((total - 1.0).abs() < 1e-12);
    assert_eq!(found.scores[2], 0.0);
    // The walker is at 0 a share 1 / (1 + 0.85) of the time, and at 1 the rest
    assert!((found.scores[0] - 1.0 / 1.85).abs() < 1e-12, "{:?}", found.scores);
}

#[test]
fn seeds_outside_the_graph_are_ignored_and_none_inside_answers_nothing() {
    let g = graph(3, &[(0, 1), (1, 2)]);
    assert_eq!(rank(&g, &[7, 9], 0.85, 100, 1e-6), None);
    assert_eq!(rank(&g, &[1, 9], 0.85, 100, 1e-6), rank(&g, &[1], 0.85, 100, 1e-6));
    assert_eq!(rank(&g, &[1, 1], 0.85, 100, 1e-6), rank(&g, &[1], 0.85, 100, 1e-6), "a seed named twice counts once");
    assert_eq!(rank(&Projection::new(), &[0], 0.85, 100, 1e-6), None);
    assert_eq!(weak_components(&Projection::new()), Vec::<Vec<u32>>::new());
}

#[test]
fn rounds_stop_at_the_tolerance_or_the_limit_and_say_which() {
    let g = karate();
    let all: Vec<u32> = (0..34).collect();
    let settled = rank(&g, &all, 0.85, 100, 1e-6).unwrap();
    assert!(settled.converged && settled.iterations < 100, "{settled:?}");
    let cut = rank(&g, &all, 0.85, 3, 1e-6).unwrap();
    assert_eq!((cut.converged, cut.iterations), (false, 3));
    // One round more or fewer than the one it stopped at would have answered differently
    let one_less = rank(&g, &all, 0.85, settled.iterations - 1, 1e-6).unwrap();
    assert!(!one_less.converged);
}

#[test]
fn components_put_the_largest_first_and_ties_by_their_first_member() {
    // {2, 4, 5}, then {0, 3}, then {1}: by size
    let found = weak_components(&graph(6, &[(3, 0), (5, 2), (4, 5)]));
    assert_eq!(found, [vec![2, 4, 5], vec![0, 3], vec![1]]);
    let ties = weak_components(&graph(4, &[(2, 3), (0, 1)]));
    assert_eq!(ties, [vec![0, 1], vec![2, 3]]);
}

/// The karate club's scores at the defaults, bit for bit, as this code answered them on an Apple M1 Pro on 2026-10-30.
/// CI runs this on Linux, macOS and Windows, so a platform that sums in another order or rounds otherwise fails here
const KARATE_BITS: [u64; 34] = [
    4591653827927720981,
    4587781197431042344,
    4588386699957798317,
    4585328779019220999,
    4581991961915377457,
    4584047981083442108,
    4584047981083442108,
    4582716137351249969,
    4584236697136671374,
    4579402419696124646,
    4581991961915377457,
    4576667332508614774,
    4579595843716552336,
    4584170532274511785,
    4579533035826004150,
    4579533035826004150,
    4580494904742140379,
    4579546143591559004,
    4579533035826004150,
    4581307877236938534,
    4579533035826004150,
    4579546143591559004,
    4579533035826004150,
    4584703682131442628,
    4581731960762017514,
    4581711830702992637,
    4579825899308887012,
    4583047366235770044,
    4581298879190048418,
    4583234356276197829,
    4582744843944934015,
    4585515857620024049,
    4589830448264899976,
    4591936396419369313,
];
const KARATE_FROM_0_BITS: [u64; 34] = [
    4598470184892720122,
    4589340092631323278,
    4588079625965018083,
    4586823479315235869,
    4584576066104776659,
    4585603293285389626,
    4585603293285389626,
    4584700370160356521,
    4583457201324074506,
    4574986268470135360,
    4584576066104776659,
    4579311185117536724,
    4581623757167048073,
    4585069302557322464,
    4572317239550293145,
    4572317239550293145,
    4580283329510308165,
    4581502368781984402,
    4572317239550293145,
    4582240233575916290,
    4572317239550293145,
    4581502368781984402,
    4572317239550293145,
    4577832711258749375,
    4576128523977789101,
    4575901698382678807,
    4571748802878355909,
    4577866739349087481,
    4577524827131097183,
    4576206159690481223,
    4580166387584866706,
    4583432812823906766,
    4584953345362285275,
    4587539481495648395,
];

#[test]
fn the_same_graph_ranks_to_the_same_bits_on_every_platform() {
    let found = rank(&karate(), &(0..34).collect::<Vec<_>>(), 0.85, 100, 1e-6).unwrap();
    assert_eq!((found.converged, found.iterations), (true, 30));
    assert_eq!(bits(&found.scores), KARATE_BITS);
    let from_0 = rank(&karate(), &[0], 0.85, 100, 1e-6).unwrap();
    assert_eq!((from_0.converged, from_0.iterations), (true, 39));
    assert_eq!(bits(&from_0.scores), KARATE_FROM_0_BITS);
}

fn bits(scores: &[f64]) -> Vec<u64> {
    scores.iter().map(|s| s.to_bits()).collect()
}

/// Newman's modularity of a partition of an undirected graph, each listed edge once, as its definition reads
fn modularity(n: usize, edges: &[(u32, u32)], groups: &[Vec<u32>], resolution: f64) -> f64 {
    let m = edges.len() as f64;
    if m == 0.0 {
        return 0.0;
    }
    let mut community = vec![0usize; n];
    for (c, group) in groups.iter().enumerate() {
        group.iter().for_each(|&v| community[v as usize] = c);
    }
    let mut degree = vec![0.0; n];
    let mut inside = vec![0.0; groups.len()];
    for &(a, b) in edges {
        degree[a as usize] += 1.0;
        degree[b as usize] += 1.0;
        if community[a as usize] == community[b as usize] {
            inside[community[a as usize]] += 1.0;
        }
    }
    let mut total = vec![0.0; groups.len()];
    (0..n).for_each(|v| total[community[v]] += degree[v]);
    (0..groups.len()).map(|c| inside[c] / m - resolution * (total[c] / (2.0 * m)).powi(2)).sum()
}

/// Whether a group's nodes are all reached from its first by edges between two of them, either way
fn connected(group: &[u32], edges: &[(u32, u32)]) -> bool {
    let mut reached = vec![group[0]];
    let mut at = 0;
    while at < reached.len() {
        let v = reached[at];
        for &(a, b) in edges {
            for (from, to) in [(a, b), (b, a)] {
                if from == v && group.contains(&to) && !reached.contains(&to) {
                    reached.push(to);
                }
            }
        }
        at += 1;
    }
    reached.len() == group.len()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn communities_cover_every_node_once_each_connected_and_no_worse_than_alone((n, edges, _) in graphs(), seed in 0u64..4) {
        let found = communities(&graph(n, &edges), 1.0, seed, 100);
        let mut all: Vec<u32> = found.iter().flatten().copied().collect();
        all.sort_unstable();
        prop_assert_eq!(all, (0..n as u32).collect::<Vec<_>>());
        prop_assert!(found.windows(2).all(|pair| pair[0].len() >= pair[1].len()), "largest first");
        for group in &found {
            prop_assert!(connected(group, &edges), "{group:?} isn't connected over {edges:?}");
        }
        let alone: Vec<Vec<u32>> = (0..n as u32).map(|v| vec![v]).collect();
        prop_assert!(modularity(n, &edges, &found, 1.0) >= modularity(n, &edges, &alone, 1.0) - 1e-12);
        prop_assert_eq!(&found, &communities(&graph(n, &edges), 1.0, seed, 100), "a seed answers the same");
    }
}

#[test]
fn the_karate_club_splits_into_its_known_communities() {
    let found = communities(&karate(), 1.0, 0, 100);
    // The club's best modularity is 0.4198 (Brandes et al., 2008), in four communities, and Leiden finds that partition
    let q = modularity(34, &KARATE_EDGES, &found, 1.0);
    assert!((q - 0.4197896).abs() < 1e-6, "modularity {q} of {found:?}");
    // Seed 0's partition as this code found it on an Apple M1 Pro on 2026-10-31, which CI's Linux, macOS and Windows
    // runs must each find too
    let recorded: [&[u32]; 4] = [
        &[8, 9, 14, 15, 18, 20, 22, 26, 29, 30, 32, 33],
        &[0, 1, 2, 3, 7, 11, 12, 13, 17, 19, 21],
        &[23, 24, 25, 27, 28, 31],
        &[4, 5, 6, 10, 16],
    ];
    assert_eq!(found, recorded);
    assert!(found.iter().all(|group| connected(group, &KARATE_EDGES)));
    assert_eq!(found, communities(&karate(), 1.0, 0, 100));
    // The two leaders, 0 and 33, around whom the club split, end up apart
    let of = |v: u32| found.iter().position(|group| group.contains(&v)).unwrap();
    assert_ne!(of(0), of(33));
}

#[test]
fn a_round_moves_refines_and_aggregates_once_and_more_rounds_merge_further() {
    // One round leaves the club in seven communities; the rounds after merge them into its best four
    let one = communities(&karate(), 1.0, 0, 1);
    let many = communities(&karate(), 1.0, 0, 100);
    assert_eq!((one.len(), many.len()), (7, 4));
    assert!(modularity(34, &KARATE_EDGES, &one, 1.0) < modularity(34, &KARATE_EDGES, &many, 1.0));
}

#[test]
fn a_lower_resolution_merges_and_none_leaves_each_component_whole() {
    let coarse = communities(&karate(), 0.25, 0, 100);
    let fine = communities(&karate(), 2.0, 0, 100);
    assert!(coarse.len() < fine.len(), "{} at 0.25 against {} at 2", coarse.len(), fine.len());
    // Two triangles apart: at resolution 0, every edge only adds, so each component is one community
    let apart = graph(6, &[(0, 1), (1, 2), (2, 0), (3, 4), (4, 5), (5, 3)]);
    assert_eq!(communities(&apart, 0.0, 0, 100), weak_components(&apart));
    assert_eq!(communities(&Projection::new(), 1.0, 0, 100), Vec::<Vec<u32>>::new());
}
