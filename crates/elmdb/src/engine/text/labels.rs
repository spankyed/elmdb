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

/// Labels up to this length use a zero-table linear scan; longer labels switch to the O(Q * L) backward DP so even
/// pathological repeated-pattern labels finish in milliseconds without allocating a `HashMap`.
const SHORT_LABEL: usize = 256;

/// The next greedy match for `want` strictly after `at`: the consecutive character if it matches, else the next word
/// start holding `want`, else the next occurrence of `want`, in one pass over `(at + 1)..lower.len()`.
fn greedy_next(lower: &[char], word_start: &[bool], at: usize, want: char) -> Option<usize> {
    if lower.get(at + 1) == Some(&want) {
        return Some(at + 1);
    }
    let mut first = None;
    for i in (at + 1)..lower.len() {
        if lower[i] == want {
            if word_start[i] {
                return Some(i);
            }
            if first.is_none() {
                first = Some(i);
            }
        }
    }
    first
}

/// Zero-table greedy scorer for short labels (`lower.len() <= SHORT_LABEL`), reusing one `matched` buffer across
/// every candidate start position.
fn score_short(lower: &[char], word_start: &[bool], wanted: &[char], last_start: usize) -> Option<(f64, Vec<usize>)> {
    let mut best: Option<(f64, Vec<usize>)> = None;
    let mut matched = Vec::with_capacity(wanted.len());
    for start in (0..=last_start).filter(|&i| lower[i] == wanted[0]) {
        matched.clear();
        matched.push(start);
        let mut at = start;
        let mut ok = true;
        for &want in &wanted[1..] {
            match greedy_next(lower, word_start, at, want) {
                Some(i) => {
                    matched.push(i);
                    at = i;
                }
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if !ok {
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
        s -= LENGTH * lower.len() as f64;
        if best.as_ref().is_none_or(|(b, _)| s > *b) {
            best = Some((s, matched.clone()));
        }
    }
    best
}

/// Exact O(Q * L) time, O(L) space greedy scorer for long labels. Each greedy transition `at -> next(at, wanted[k])`
/// and its step score depend only on `(at, k)`, and every intermediate term (`MATCH`, `WORD_START`, `CONSECUTIVE`,
/// `GAP`, `LEADING_GAP`) is an exact dyadic float, so evaluating tail scores backward by query character computes the
/// bit-for-bit identical score for every start position without any `HashMap`.
fn score_long(lower: &[char], word_start: &[bool], wanted: &[char], last_start: usize) -> Option<(f64, Vec<usize>)> {
    let n = lower.len();
    let mut cur = vec![0.0f64; n];
    let mut prev = vec![f64::NEG_INFINITY; n];
    for &want in wanted[1..].iter().rev() {
        let mut next_ws = n;
        let mut next_occ = n;
        for at in (0..n).rev() {
            let next = if at + 1 < n && lower[at + 1] == want {
                at + 1
            } else if next_ws < n {
                next_ws
            } else {
                next_occ
            };
            prev[at] = if next < n {
                let mut step = MATCH;
                if word_start[next] {
                    step += WORD_START;
                }
                let gap = next - at - 1;
                if gap == 0 {
                    step += CONSECUTIVE;
                } else {
                    step -= GAP * gap as f64;
                }
                step + cur[next]
            } else {
                f64::NEG_INFINITY
            };
            if lower[at] == want {
                next_occ = at;
                if word_start[at] {
                    next_ws = at;
                }
            }
        }
        std::mem::swap(&mut cur, &mut prev);
    }
    let mut best: Option<(f64, usize)> = None;
    for start in (0..=last_start).filter(|&i| lower[i] == wanted[0]) {
        if !cur[start].is_finite() {
            continue;
        }
        let mut s = MATCH;
        if word_start[start] {
            s += WORD_START;
        }
        s += cur[start];
        s -= LEADING_GAP * start as f64;
        s -= LENGTH * n as f64;
        if best.is_none_or(|(b, _)| s > b) {
            best = Some((s, start));
        }
    }
    let (score, best_start) = best?;
    let mut matched = Vec::with_capacity(wanted.len());
    let mut at = best_start;
    matched.push(at);
    for &want in &wanted[1..] {
        let next = greedy_next(lower, word_start, at, want)?;
        matched.push(next);
        at = next;
    }
    Some((score, matched))
}

/// How `query` matches `label`, or `None` when its characters aren't all in it in order. Each start of the query's
/// first character is tried, each one going on greedily, and the best kept
pub fn score(query: &str, label: &str) -> Option<Matched> {
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
    // mutant-skip: replace <= with (<|>|==) in score
    // `score_short` and `score_long` compute the exact same greedy score; `SHORT_LABEL` only selects between the
    // zero-table linear scan and the O(Q * L) backward DP
    let (score, matched) = if lower.len() <= SHORT_LABEL {
        score_short(&lower, &word_start, &wanted, last_start)?
    } else {
        score_long(&lower, &word_start, &wanted, last_start)?
    };
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

        let straddle_255 = format!("{}ab_B", "x".repeat(251));
        let straddle_256 = format!("{}ab_B", "x".repeat(252));
        let straddle_257 = format!("{}ab_B", "x".repeat(253));
        let labels = [
            "Open Library",
            "openDatabaseConnectionPool",
            "aaa_aaa_Aaa_b",
            "axxxxxab",
            "ab a_b aB ab",
            "ab_B",
            "xab_B",
            "axbb",
            "axb c _B",
            "a_a",
            // Exact score tie between index 1 (non-word-start: 16 - 0.5 = 15.5) and index 17 (word-start: 24 - 8.5 = 15.5)
            "xaxxxxxxxxxxxxxx_a",
            "foo/bar/baz_qux-Bar",
            "a",
            "😀a😀b😀A😀B",
            "",
            straddle_255.as_str(),
            straddle_256.as_str(),
            straddle_257.as_str(),
        ];
        let queries = ["", " ", "a", "aa", "ab", "abc", "ol", "dbp", "aaab", "b", "bar", "z", "a b", "oDC"];
        for label in labels {
            let chars: Vec<char> = label.chars().collect();
            let lower: Vec<char> = chars.iter().copied().map(folded).collect();
            let word_start: Vec<bool> = (0..chars.len()).map(|i| starts_word(&chars, i)).collect();
            for query in queries {
                let expected = reference(query, label);
                assert_eq!(score(query, label), expected, "query={query:?} label={label:?}");
                // Verify both `score_short` and `score_long` agree bit-for-bit on every input regardless of length
                let wanted: Vec<char> = query.chars().filter(|c| !c.is_whitespace()).map(folded).collect();
                if !wanted.is_empty() {
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
                    if let Some(last) = last_start {
                        let short = score_short(&lower, &word_start, &wanted, last);
                        let long = score_long(&lower, &word_start, &wanted, last);
                        assert_eq!(short, long, "short vs long query={query:?} label={label:?}");
                    }
                }
            }
        }

        // Long label with repeated characters: both miss and hit complete immediately without quadratic rescanning
        let repeated = "a".repeat(20_000);
        assert!(score("aaaaab", &repeated).is_none());
        let with_tail = format!("{repeated}b");
        let hit = score("aaab", &with_tail).unwrap();
        assert_eq!(hit.positions, [19_997, 19_998, 19_999, 20_000]);

        // Worst-case alternating pattern: O(Q * L) completes cleanly and matches exact greedy score
        let alternating = "ab".repeat(10_000);
        let alt_query = "ab".repeat(200);
        let alt_hit = score(&alt_query, &alternating).unwrap();
        let expected_positions: Vec<u32> = (0..400).collect();
        assert_eq!(alt_hit.positions, expected_positions);
        let expected_score = 400.0 * MATCH + WORD_START + 399.0 * CONSECUTIVE - LENGTH * (alternating.len() as f64);
        assert_eq!(alt_hit.score, expected_score);
    }
}
