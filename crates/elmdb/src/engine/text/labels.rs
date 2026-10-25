//! Fuzzy labels (`docs/search.md`, Methods): a quick-open across types, matching a query's characters in order
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
            // Prefer the next word start holding the character, else its next occurrence
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
}
