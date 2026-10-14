//! Which transcript rows contain a search query.
//!
//! The window knows what each row says. This only answers which rows match,
//! so the matching itself can be tested without GTK.

/// Row indexes whose text contains `query`, ignoring case.
/// A blank query matches nothing: the bar is open, but nothing is marked.
pub fn hits(rows: &[impl AsRef<str>], query: &str) -> Vec<i32> {
    let query = query.trim();
    if query.is_empty() {
        return Vec::new();
    }
    let query = query.to_lowercase();
    rows.iter()
        .enumerate()
        .filter(|(_, row)| row.as_ref().to_lowercase().contains(&query))
        .map(|(index, _)| index as i32)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_query_matches_nothing() {
        assert!(hits(&["hello"], "").is_empty());
        assert!(hits(&["hello"], "   ").is_empty());
    }

    #[test]
    fn matches_ignore_case_and_skip_other_rows() {
        let rows = ["No transcript yet", "Budget", "Maya We ship on Friday"];
        assert_eq!(hits(&rows, "friday"), vec![2]);
        assert_eq!(hits(&rows, "MAYA"), vec![2]);
        assert_eq!(hits(&rows, "budget"), vec![1]);
    }

    #[test]
    fn reports_every_matching_row_in_order() {
        let rows = ["one", "two", "one more", ""];
        assert_eq!(hits(&rows, "one"), vec![0, 2]);
        assert!(hits(&rows, "absent").is_empty());
    }
}
