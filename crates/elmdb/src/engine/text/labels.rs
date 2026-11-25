//! Fuzzy labels (`docs/architecture/search.md`, Methods): a quick-open across types, matching a query's characters in order
//! anywhere in a label, as editors' file pickers do. A match scores more where its characters start words — after a
//! space or punctuation, or at a lower-to-upper case change — and run together, and less for the characters it skips
//! and for a longer label.

/// What each matched character scores, and the bonuses and costs around it
const MATCH: f64 = 16.0;
const WORD_START: f64 = 8.0;
const CONSECUTIVE: f64 = 6.0;
const GAP: f64 = 1.0;
const LEADING_GAP: f64 = 0.5;
const LENGTH: f64 = 0.1;

/// A label's match: its score, and the matched characters' places in UTF-16 units
#[derive(Debug, Clone, PartialEq)]
pub struct Matched {
    pub score: f64,
    pub positions: Vec<u32>,
}

fn starts_word(chars: &[char], at: usize) -> bool {
    at == 0 || {
        let before = chars[at - 1];
        !before.is_alphanumeric() || (before.is_lowercase() && chars[at].is_uppercase())
    }
}

fn folded(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// How `query` matches `label`, or `None` when its characters aren't all in it in order. Each start of the query's
/// first character is tried, each one going on greedily, and the best kept
pub fn score(query: &str, label: &str) -> Option<Matched> {
    use std::collections::HashMap;
    let wanted: Vec<char> = query.chars().filter(|c| !c.is_whitespace()).map(folded).collect();
    if wanted.is_empty() {
        return None;
    }
    let chars: Vec<char> = label.chars().collect();
    let lower: Vec<char> = chars.iter().copied().map(folded).collect();
    // Backward subsequence scan: fail fast in O(L) if `wanted` isn't in `lower`, and find the latest start position
    // that can still complete the query
    let mut need = wanted.len();
    let mut last_start = None;
    for i in (0..lower.len()).rev() {
        if lower[i] == wanted[need - 1] {
            need -= 1;
            if need == 0 {
                last_start = Some(i);
                break;
            }
        }
    }
    let last_start = last_start?;
    let word_start: Vec<bool> = (0..chars.len()).map(|i| starts_word(&chars, i)).collect();
    // Precompute sorted positions (word starts and all occurrences) for each distinct query character after the first,
    // turning every greedy step from O(L) rescanning into O(1) consecutive check + O(log L) binary search
    let mut word_starts: HashMap<char, Vec<usize>> = HashMap::new();
    let mut all_pos: HashMap<char, Vec<usize>> = HashMap::new();
    for &c in &wanted[1..] {
        word_starts.entry(c).or_default();
        all_pos.entry(c).or_default();
    }
    for (i, &c) in lower.iter().enumerate() {
        if let Some(positions) = all_pos.get_mut(&c) {
            positions.push(i);
            if word_start[i] {
                word_starts.get_mut(&c).expect("inserted with all_pos").push(i);
            }
        }
    }
    let mut best: Option<(f64, Vec<usize>)> = None;
    for start in (0..=last_start).filter(|&i| lower[i] == wanted[0]) {
        let mut at = start;
        let mut matched = Vec::with_capacity(wanted.len());
        matched.push(start);
        for want in &wanted[1..] {
            // Prefer the consecutive character, then the next word start holding it, else its next occurrence
            let next = if lower.get(at + 1) == Some(want) {
                Some(at + 1)
            } else {
                let ws = &word_starts[want];
                let p = ws.partition_point(|&i| i <= at);
                ws.get(p).copied().or_else(|| {
                    let all = &all_pos[want];
                    let p = all.partition_point(|&i| i <= at);
                    all.get(p).copied()
                })
            };
            match next {
                Some(i) => {
                    matched.push(i);
                    at = i;
                }
                None => {
                    matched.clear();
                    break;
                }
            }
        }
        if matched.is_empty() {
            continue;
        }
        let mut s = 0.0;
        for (k, &i) in matched.iter().enumerate() {
            s += MATCH;
            if word_start[i] {
                s += WORD_START;
            }
            if k > 0 {
                let gap = i - matched[k - 1] - 1;
                if gap == 0 {
                    s += CONSECUTIVE;
                } else {
                    s -= GAP * gap as f64;
                }
            }
        }
        s -= LEADING_GAP * matched[0] as f64;
        s -= LENGTH * chars.len() as f64;
        if best.as_ref().is_none_or(|(b, _)| s > *b) {
            best = Some((s, matched));
        }
    }
    let (score, matched) = best?;
    // Characters to UTF-16 units: a character outside the basic plane is two
    let mut units = Vec::with_capacity(chars.len() + 1);
    let mut n = 0u32;
    for c in &chars {
        units.push(n);
        n += c.len_utf16() as u32;
    }
    Some(Matched { score, positions: matched.into_iter().map(|i| units[i]).collect() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_characters_in_order_and_nothing_else() {
        assert!(score("thr", "Thread list").is_some());
        assert!(score("rht", "Thread").is_none());
        assert!(score("", "Thread").is_none());
        assert_eq!(score("tl", "Thread list").unwrap().positions, [0, 7]);
    }

    #[test]
    fn word_starts_and_runs_beat_scattered_characters() {
        let initials = score("ol", "Open Library").unwrap().score;
        let scattered = score("ol", "Tools").unwrap().score;
        assert!(initials > scattered, "{initials} > {scattered}");
        let run = score("lib", "Library").unwrap().score;
        let spread = score("lib", "Label in blue").unwrap().score;
        assert!(run > spread, "{run} > {spread}");
        assert!(score("note", "Notes").unwrap().score > score("note", "Notes about everything else").unwrap().score);
    }

    #[test]
    fn a_case_change_starts_a_word_and_positions_are_utf16_units() {
        assert_eq!(score("db", "openDatabase").unwrap().positions, [4, 8]);
        assert_eq!(score("b", "😀b").unwrap().positions, [2]);
    }

    #[test]
    fn matches_naive_reference_and_avoids_quadratic_blowup_on_repeated_characters() {
        // Naive O(Q * L^2) reference matching the original algorithm bit for bit
        fn reference(query: &str, label: &str) -> Option<Matched> {
            let wanted: Vec<char> = query.chars().filter(|c| !c.is_whitespace()).map(folded).collect();
            if wanted.is_empty() {
                return None;
            }
            let chars: Vec<char> = label.chars().collect();
            let lower: Vec<char> = chars.iter().copied().map(folded).collect();
            let mut best: Option<(f64, Vec<usize>)> = None;
            for start in (0..lower.len()).filter(|&i| lower[i] == wanted[0]) {
                let mut at = start;
                let mut matched = vec![start];
                for want in &wanted[1..] {
                    let rest = (at + 1)..lower.len();
                    let next = rest
                        .clone()
                        .find(|&i| lower[i] == *want && starts_word(&chars, i) && i == at + 1)
                        .or_else(|| rest.clone().find(|&i| lower[i] == *want && i == at + 1))
                        .or_else(|| rest.clone().find(|&i| lower[i] == *want && starts_word(&chars, i)))
                        .or_else(|| rest.clone().find(|&i| lower[i] == *want));
                    match next {
                        Some(i) => {
                            matched.push(i);
                            at = i;
                        }
                        None => {
                            matched.clear();
                            break;
                        }
                    }
                }
                if matched.is_empty() {
                    continue;
                }
                let mut s = 0.0;
                for (k, &i) in matched.iter().enumerate() {
                    s += MATCH;
                    if starts_word(&chars, i) {
                        s += WORD_START;
                    }
                    if k > 0 {
                        let gap = i - matched[k - 1] - 1;
                        if gap == 0 {
                            s += CONSECUTIVE;
                        } else {
                            s -= GAP * gap as f64;
                        }
                    }
                }
                s -= LEADING_GAP * matched[0] as f64;
                s -= LENGTH * chars.len() as f64;
                if best.as_ref().is_none_or(|(b, _)| s > *b) {
                    best = Some((s, matched));
                }
            }
            let (score, matched) = best?;
            let mut units = Vec::with_capacity(chars.len() + 1);
            let mut n = 0u32;
            for c in &chars {
                units.push(n);
                n += c.len_utf16() as u32;
            }
            Some(Matched { score, positions: matched.into_iter().map(|i| units[i]).collect() })
        }

        let labels = [
            "Open Library",
            "openDatabaseConnectionPool",
            "aaa_aaa_Aaa_b",
            "axxxxxab",
            "ab a_b aB ab",
            "foo/bar/baz_qux-Bar",
            "a",
            "😀a😀b😀A😀B",
            "",
        ];
        let queries = ["", " ", "a", "ab", "ol", "dbp", "aaab", "b", "bar", "z", "a b", "oDC"];
        for label in labels {
            for query in queries {
                assert_eq!(score(query, label), reference(query, label), "query={query:?} label={label:?}");
            }
        }

        // Long label with repeated characters: both miss and hit complete immediately without quadratic rescanning
        let repeated = "a".repeat(20_000);
        assert!(score("aaaaab", &repeated).is_none());
        let with_tail = format!("{repeated}b");
        let hit = score("aaab", &with_tail).unwrap();
        assert_eq!(hit.positions, [19_997, 19_998, 19_999, 20_000]);
    }
}
