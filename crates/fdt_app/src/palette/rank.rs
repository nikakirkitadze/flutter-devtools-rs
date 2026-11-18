//! Fuzzy scoring and ordering for the command palette.

/// Scores `query` against `haystack`; `None` when any term does not match.
pub fn fuzzy_score(query: &str, haystack: &str) -> Option<i32> {
    let terms: Vec<&str> = query.split_whitespace().collect();
    if terms.is_empty() {
        return Some(0);
    }
    let orig: Vec<char> = haystack.chars().collect();
    let h: Vec<char> = orig.iter().map(|c| lower(*c)).collect();
    let boundary: Vec<bool> = (0..h.len())
        .map(|j| {
            j == 0
                || matches!(orig[j - 1], ' ' | '_' | '-' | '/' | '.' | ':')
                || (orig[j - 1].is_lowercase() && orig[j].is_uppercase())
        })
        .collect();
    let mut prev = vec![NEG; h.len()];
    let mut cur = vec![NEG; h.len()];
    let mut total = 0i32;
    for term in terms {
        let t: Vec<char> = term.chars().map(lower).collect();
        total += term_score(&t, &h, &boundary, &mut prev, &mut cur)?;
    }
    Some(total)
}

const NEG: i32 = i32::MIN / 2;

fn lower(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Best score of `t` as a subsequence of `h`, O(t.len() * h.len()).
fn term_score(
    t: &[char],
    h: &[char],
    boundary: &[bool],
    prev: &mut Vec<i32>,
    cur: &mut Vec<i32>,
) -> Option<i32> {
    let m = h.len();
    if t.len() > m {
        return None;
    }
    let bonus = |j: usize| 1 + if boundary[j] { 8 } else { 0 };
    for j in 0..m {
        prev[j] = if t[0] == h[j] {
            bonus(j) - j.min(5) as i32
        } else {
            NEG
        };
    }
    for &tc in &t[1..] {
        let mut far = NEG; // max of prev[k] for k <= j - 4
        for j in 0..m {
            if j >= 4 {
                far = far.max(prev[j - 4]);
            }
            cur[j] = NEG;
            if tc != h[j] {
                continue;
            }
            let mut best = NEG;
            if j >= 1 && prev[j - 1] > NEG {
                best = prev[j - 1] + 4;
            }
            if j >= 2 && prev[j - 2] > NEG {
                best = best.max(prev[j - 2] - 1);
            }
            if j >= 3 && prev[j - 3] > NEG {
                best = best.max(prev[j - 3] - 2);
            }
            if far > NEG {
                best = best.max(far - 3);
            }
            if best > NEG {
                cur[j] = bonus(j) + best;
            }
        }
        std::mem::swap(prev, cur);
    }
    let best = prev[..m].iter().copied().max().unwrap_or(NEG);
    (best > NEG).then_some(best)
}

const RECENT_BONUS_BASE: i32 = 20;

/// Indices of `haystacks` in display order.
pub fn rank(query: &str, haystacks: &[String], recent: &[usize]) -> Vec<usize> {
    let mut seen = vec![false; haystacks.len()];
    let mut recents = Vec::new();
    let mut bonus = vec![0i32; haystacks.len()];
    for (pos, &i) in recent.iter().enumerate() {
        if i < haystacks.len() && !seen[i] {
            seen[i] = true;
            recents.push(i);
            bonus[i] = (RECENT_BONUS_BASE - pos as i32).max(0);
        }
    }
    if query.split_whitespace().next().is_none() {
        let rest = (0..haystacks.len()).filter(|&i| !seen[i]);
        return recents.into_iter().chain(rest).collect();
    }
    let mut scored: Vec<(i32, usize)> = haystacks
        .iter()
        .enumerate()
        .filter_map(|(i, h)| fuzzy_score(query, h).map(|s| (s + bonus[i], i)))
        .collect();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(haystacks[a.1].len().cmp(&haystacks[b.1].len()))
            .then(a.1.cmp(&b.1))
    });
    scored.into_iter().map(|(_, i)| i).collect()
}

const VERBS: [&str; 7] = ["Go to", "Show", "Open", "Copy", "Clear", "Export", "Toggle"];
const NOUNS: [&str; 13] = [
    "Memory",
    "Network",
    "Logs",
    "Frame",
    "Request",
    "Widget",
    "State",
    "Timeline",
    "Heap snapshot",
    "Settings",
    "Range",
    "Selection",
    "Theme",
];

/// Deterministic palette-sized label set shared by the budget test and the bench.
#[doc(hidden)]
pub fn synthetic_labels(n: usize) -> Vec<String> {
    (0..n)
        .map(|i| format!("{} {} {}", VERBS[i % 7], NOUNS[i % 13], i))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(q: &str, h: &str) -> Option<i32> {
        fuzzy_score(q, h)
    }

    #[test]
    fn matches_subsequence_case_insensitively() {
        assert!(s("MEM", "Go to Memory").is_some());
        assert!(s("xyz", "Go to Memory").is_none());
    }

    #[test]
    fn empty_query_scores_zero() {
        assert_eq!(s("", "anything"), Some(0));
        assert_eq!(s("   ", "anything"), Some(0));
    }

    #[test]
    fn word_start_beats_mid_word() {
        assert!(s("mem", "Go to Memory").unwrap() > s("mem", "Remembered items").unwrap());
    }

    #[test]
    fn consecutive_beats_scattered() {
        assert!(s("net", "Network").unwrap() > s("net", "Nested items").unwrap());
    }

    #[test]
    fn every_term_must_match() {
        assert!(s("net req", "Network: clear requests").is_some());
        let items = vec!["Network: clear requests".to_string()];
        assert!(rank("net xyz", &items, &[]).is_empty());
    }

    #[test]
    fn non_ascii_and_long_queries_do_not_panic() {
        assert!(s("ß", "Straße").is_some());
        assert!(s(&"ß".repeat(300), "Go to Memory").is_none());
        assert!(s("🙂", "Go to Memory").is_none());
    }

    #[test]
    fn empty_query_lists_recents_first() {
        let items: Vec<String> = ["A", "B", "C"].iter().map(|s| s.to_string()).collect();
        assert_eq!(rank("", &items, &[2, 0, 2, 9]), vec![2, 0, 1]);
    }

    #[test]
    fn recent_breaks_ties_for_matches() {
        let items: Vec<String> = ["Go to Logs", "Go to Memory"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(rank("go", &items, &[1]), vec![1, 0]);
    }

    #[test]
    #[cfg_attr(debug_assertions, ignore = "budget: run with --release")]
    fn budget_rank_5000_items_per_keystroke() {
        let items = synthetic_labels(5000);
        for q in [
            "s", "sh", "sho", "show", "show m", "show me", "net req", "frame 48", "zzz",
        ] {
            std::hint::black_box(rank(q, &items, &[]));
            let mut worst = std::time::Duration::ZERO;
            for _ in 0..20 {
                let t = std::time::Instant::now();
                std::hint::black_box(rank(q, &items, &[1, 2, 3]));
                worst = worst.max(t.elapsed());
            }
            eprintln!("budget query {q:?} worst {worst:?}");
            assert!(
                worst <= std::time::Duration::from_millis(8),
                "query {q:?} took {worst:?}"
            );
        }
    }
}
