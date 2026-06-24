//! Tokenization for the index and the query, kept in lock-step so indexed tokens and goal
//! keywords align. Rules mirror `ContextFinder::keywords`: split on non-alphanumeric, lowercase,
//! keep tokens of length >= 3. The query side additionally drops stopwords and de-duplicates.
//!
//! Note on parity with the lexical ContextFinder: query terms and indexed tokens use identical
//! tokenization rules so they align with each other. PATH scoring uses the same 5× weighting as
//! the lexical path. However, CONTENT scoring differs: the index matches by whole-token equality
//! (an inverted-index lookup), whereas the lexical fallback counts substring occurrences. As a
//! deliberate tradeoff (whole-token equality is what makes the index possible), a goal term that
//! only appears as a substring of a larger token (e.g. "auth" inside "authenticate") scores in
//! the lexical content path but not in the index.

use std::collections::HashMap;
use std::collections::HashSet;

/// Per-file content scanned for indexing (chars). Mirrors the ContextFinder content-scan cap.
pub const CONTENT_SCAN_CHARS: usize = 65_536;

const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "add", "fix", "make", "use", "into", "from", "you",
];

/// Index tokens: token -> occurrence count over the first `CONTENT_SCAN_CHARS` chars. No
/// stopword filtering (the query never asks for stopwords, so they cost nothing in the index).
pub fn index_tokens(content: &str) -> HashMap<String, i64> {
    let mut counts: HashMap<String, i64> = HashMap::new();
    for tok in content
        .chars()
        .take(CONTENT_SCAN_CHARS)
        .collect::<String>()
        .split(|c: char| !c.is_alphanumeric())
    {
        let t = tok.to_lowercase();
        if t.len() >= 3 {
            *counts.entry(t).or_insert(0) += 1;
        }
    }
    counts
}

/// Query terms: lowercased alphanumeric tokens of length >= 3, minus stopwords, de-duplicated.
/// Matches `ContextFinder::keywords` exactly so query terms hit the same indexed tokens.
pub fn query_terms(goal: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for tok in goal.split(|c: char| !c.is_alphanumeric()) {
        let t = tok.to_lowercase();
        if t.len() >= 3 && !STOPWORDS.contains(&t.as_str()) && seen.insert(t.clone()) {
            out.push(t);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_tokens_counts_and_filters_short() {
        let m = index_tokens("login Login io fn login_handler");
        assert_eq!(m.get("login"), Some(&3)); // 'login' three times: standalone, Login, and from login_handler (split drops '_')
        assert_eq!(m.get("handler"), Some(&1));
        assert!(!m.contains_key("io")); // length < 3 dropped
        assert!(!m.contains_key("fn")); // length < 3 dropped
    }

    #[test]
    fn query_terms_drops_stopwords_and_dedupes() {
        let t = query_terms("Fix the login Login flow at io");
        assert!(t.contains(&"login".to_string()));
        assert!(t.contains(&"flow".to_string()));
        assert!(!t.contains(&"fix".to_string())); // stopword
        assert!(!t.contains(&"the".to_string())); // stopword
        assert!(!t.contains(&"io".to_string())); // length < 3
        assert_eq!(t.iter().filter(|k| *k == "login").count(), 1); // deduped
    }
}
