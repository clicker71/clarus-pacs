// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Multi-way intersection of per-gram study-UID lists.
//!
//! A fuzzymatch query is split into n-grams; each gram names a `.idx` whose
//! rows are the studies containing that gram (in chronological append order,
//! i.e. lexicographically sorted). A study matches a query only if it appears
//! in *every* gram's list, so the candidate set is their intersection.
//!
//! This is a classic K-way merge: advance the smallest cursor, and when all K
//! cursors agree on a UID, emit it and advance every cursor.

/// Intersect `lists` of sorted study UIDs, writing the common UIDs into `out`.
///
/// Zero-heap by contract (AP101B-101): the merge emits **borrowed** `&str`
/// slices from the input lists and reuses the caller's `cursors`/`out`
/// buffers, so a steady-state caller (buffers pre-sized) allocates nothing.
/// The function itself allocates only on the first call, when `cursors`/`out`
/// have no capacity yet.
///
/// Behavior is byte-identical to the owning form:
/// - empty input -> `out` is cleared (empty result);
/// - a single list -> passthrough of its elements (`limit` takes the first `n`);
/// - K >= 2 -> classic K-way merge, smallest-cursor first, UIDs present in all
///   lists emitted in ascending order; duplicates within a list are skipped;
///   `limit` stops early once `n` matches have been collected.
///
/// Each list is assumed sorted ascending (the `.idx` append order).
pub fn intersect_study_uids<'a>(
    lists: &[&'a [String]],
    limit: Option<usize>,
    cursors: &mut Vec<usize>,
    out: &mut Vec<&'a str>,
) {
    out.clear();
    if lists.is_empty() {
        return;
    }
    if lists.len() == 1 {
        let single = lists[0];
        let n = limit.unwrap_or(single.len()).min(single.len());
        for uid in &single[..n] {
            out.push(uid.as_str());
        }
        return;
    }

    cursors.clear();
    cursors.resize(lists.len(), 0);

    loop {
        // Find the smallest UID among the current cursor positions.
        let mut min_uid: Option<&str> = None;
        for i in 0..lists.len() {
            if cursors[i] >= lists[i].len() {
                return;
            }
            let uid: &str = lists[i][cursors[i]].as_str();
            match min_uid {
                None => min_uid = Some(uid),
                Some(m) if uid < m => min_uid = Some(uid),
                _ => {}
            }
        }
        let target = min_uid.unwrap();

        // Advance every cursor up to `target`; did they all land on it?
        let mut all_match = true;
        for i in 0..lists.len() {
            while cursors[i] < lists[i].len() && lists[i][cursors[i]].as_str() < target {
                cursors[i] += 1;
            }
            if cursors[i] >= lists[i].len() {
                return;
            }
            if lists[i][cursors[i]].as_str() != target {
                all_match = false;
            }
        }

        if all_match {
            out.push(target);
            // Early stop: the caller only needs `limit` results.
            if let Some(n) = limit
                && out.len() >= n
            {
                return;
            }
            // Advance PAST every occurrence of `target`, not just one: a list
            // carrying the same UID twice would otherwise re-emit it on the
            // next round, and the intersection would list a study once per
            // duplicate instead of once.
            for i in 0..lists.len() {
                while cursors[i] < lists[i].len() && lists[i][cursors[i]].as_str() == target {
                    cursors[i] += 1;
                }
            }
        } else {
            // Consume the minimum elements so the merge makes progress.
            for i in 0..lists.len() {
                if cursors[i] < lists[i].len() && lists[i][cursors[i]].as_str() == target {
                    cursors[i] += 1;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-only adapter: run the merge on owned `Vec<String>` inputs and
    /// return the matched UIDs as `Vec<String>`. NOT part of the hotpath API -
    /// it allocates to build the borrowed-slice view, which the zero-heap gate
    /// (`ap101_fuzzy`) deliberately avoids by reusing buffers.
    fn intersect_owned(lists: &[Vec<String>], limit: Option<usize>) -> Vec<String> {
        let views: Vec<&[String]> = lists.iter().map(|v| v.as_slice()).collect();
        let mut cursors = Vec::new();
        let mut out = Vec::new();
        intersect_study_uids(&views, limit, &mut cursors, &mut out);
        out.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn empty_input() {
        assert_eq!(intersect_owned(&[], None), Vec::<String>::new());
    }

    #[test]
    fn single_list_passthrough() {
        let lists = vec![vec!["a".to_string(), "b".to_string()]];
        assert_eq!(intersect_owned(&lists, None), lists[0]);
    }

    #[test]
    fn single_list_passthrough_with_limit() {
        let lists = vec![vec![
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string(),
        ]];
        // First `n` elements when the list is longer than the limit.
        assert_eq!(
            intersect_owned(&lists, Some(2)),
            vec!["a".to_string(), "b".to_string()]
        );
        // Limit larger than the list yields the whole list.
        assert_eq!(intersect_owned(&lists, Some(10)), lists[0]);
    }

    #[test]
    fn common_uids_preserved() {
        let lists = vec![
            vec![
                "1.2.1".to_string(),
                "1.2.2".to_string(),
                "1.2.3".to_string(),
            ],
            vec![
                "1.2.2".to_string(),
                "1.2.3".to_string(),
                "1.2.4".to_string(),
            ],
            vec!["1.2.3".to_string(), "1.2.4".to_string()],
        ];
        let out = intersect_owned(&lists, None);
        assert_eq!(out, vec!["1.2.3".to_string()]);
    }

    #[test]
    fn no_common_uids_is_empty() {
        let lists = vec![
            vec!["1.2.1".to_string()],
            vec!["1.2.2".to_string()],
            vec!["1.2.3".to_string()],
        ];
        assert_eq!(intersect_owned(&lists, None), Vec::<String>::new());
    }

    #[test]
    fn duplicates_do_not_break_intersection() {
        let lists = vec![
            vec![
                "1.2.1".to_string(),
                "1.2.1".to_string(),
                "1.2.2".to_string(),
            ],
            vec!["1.2.1".to_string(), "1.2.2".to_string()],
        ];
        assert_eq!(
            intersect_owned(&lists, None),
            vec!["1.2.1".to_string(), "1.2.2".to_string()]
        );
    }

    #[test]
    fn early_stop_by_limit() {
        let lists = vec![
            vec![
                "1.2.1".to_string(),
                "1.2.2".to_string(),
                "1.2.3".to_string(),
                "1.2.4".to_string(),
            ],
            vec![
                "1.2.1".to_string(),
                "1.2.2".to_string(),
                "1.2.3".to_string(),
                "1.2.4".to_string(),
            ],
        ];
        // Only the first `limit` common UIDs are emitted, in sorted order.
        assert_eq!(
            intersect_owned(&lists, Some(2)),
            vec!["1.2.1".to_string(), "1.2.2".to_string()]
        );
    }
}
