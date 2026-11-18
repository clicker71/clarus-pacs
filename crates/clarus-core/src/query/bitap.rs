// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Bitap (Shift-Or) approximate string match for QIDO fuzzymatch.
//!
//! The QIDO `fuzzymatch=true` search runs `PatientName` / `ReferringPhysicianName`
//! with a Levenshtein threshold of exactly one edit (one substitution,
//! insertion, or deletion). Bitap evaluates that in a single pass over the
//! text using two `u64` bit-vectors (0-error and 1-error states).
//!
//! # Threshold and encoding
//!
//! - The mask is built per **Unicode scalar** (not per byte), so the "one
//!   edit" threshold is a one-character edit for Cyrillic and CJK names too.
//!   A pattern longer than 63 scalars cannot fit a `u64` mask (one bit per
//!   scalar); those fall back to a plain case-insensitive EXACT substring
//!   search. That is NARROWER than a 1-edit match: a long query with one
//!   typo does not match.
//! - Matching is case-insensitive for ASCII only (both register variants are
//!   folded into the same mask bit). The query layer lowercases both sides
//!   before calling in, so non-ASCII case never needs Unicode casing tables
//!   here.
//! - The mask is kept in small stack arrays (`[u32; 64]` / `[u64; 64]`), so the
//!   hotpath never allocates - a linear probe over at most 63 entries stands in
//!   for a dense char->bits table.
//!
//! The 1-error recurrence SEEDS its `r1` term with `| 1` (W2, 02.09): a
//! fresh empty prefix may carry one error, so the FIRST pattern scalar is
//! allowed to differ (first-letter edits). This makes a query like
//! "ononymo" reach a stored name starting "...Anonymo" - an edit that a
//! no-seed run treated as unmatchable because it anchors on the exact first
//! letter. The seed is bounded to exactly one edit by the other terms; a
//! two-edit (double-deletion) alignment still cannot pass, which the guard
//! tests pin down.

/// Capacity of the stack mask (one slot per pattern scalar, max 63 used).
const MASK_CAP: usize = 63;

/// Set the bit for `(ch, mask)` in the stack arrays, deduplicating on `ch`.
fn add_mask(
    keys: &mut [u32; MASK_CAP],
    vals: &mut [u64; MASK_CAP],
    n: &mut usize,
    ch: u32,
    bit: u64,
) {
    for k in 0..*n {
        if keys[k] == ch {
            vals[k] |= bit; // scalar already keyed -- fold the bit in
            return;
        }
    }
    keys[*n] = ch;
    vals[*n] = bit;
    *n += 1;
}

/// Look up the mask bits for a scalar, or 0 if the scalar occurs nowhere.
fn lookup(keys: &[u32; MASK_CAP], vals: &[u64; MASK_CAP], n: usize, ch: u32) -> u64 {
    for k in 0..n {
        if keys[k] == ch {
            return vals[k];
        }
    }
    0
}

/// The canonical spelling of a scalar for the mask table.
///
/// ASCII letters fold to lowercase so one pattern scalar occupies one table
/// entry and matches either case in the text. Non-ASCII scalars are left as
/// they are: Unicode case folding is not one-to-one, so folding it here could
/// widen or narrow a match in ways the query's semantics do not state.
fn fold_case(ch: char) -> char {
    if ch.is_ascii() {
        ch.to_ascii_lowercase()
    } else {
        ch
    }
}

/// Whether `pattern` occurs in `text` with at most one Levenshtein edit.
///
/// Returns `true` immediately on an exact-match or 1-edit window; the caller
/// decides the semantics of the empty pattern (this function treats it as a
/// match, matching `str::contains("")`).
pub fn bitap_match(pattern: &str, text: &str) -> bool {
    let plen = pattern.chars().count();
    if plen == 0 {
        return true;
    }
    if plen > MASK_CAP {
        return text.to_lowercase().contains(&pattern.to_lowercase());
    }

    // Build the per-scalar mask as stack arrays (0 heap allocation).
    //
    // `keys` has `MASK_CAP` entries but a scalar's ASCII case variants used to
    // be inserted as TWO EXTRA keys on top of the scalar itself, so a pattern of
    // `MASK_CAP` ASCII letters needed up to 3x that many slots - and the
    // `add_mask` overflow guard then wrote `keys[63]` on a 38-scalar pattern,
    // panicking the server on a query. The table is sized per PATTERN SCALAR,
    // so the fold has to be per scalar too: the mask bits of both cases are
    // recorded on ONE key (the lowercase spelling), and the lookup folds the
    // text the same way.
    let mut keys = [0u32; MASK_CAP];
    let mut vals = [0u64; MASK_CAP];
    let mut n = 0usize;
    for (i, ch) in pattern.chars().enumerate() {
        let bit = 1u64 << i;
        add_mask(&mut keys, &mut vals, &mut n, fold_case(ch) as u32, bit);
    }

    let finish = 1u64 << (plen - 1);
    let mut r0: u64 = 0; // 0 errors
    // 1 error. Pre-seeded with the `| 1` bit BEFORE the first text scalar is
    // read: the in-loop seed is applied after `(r1 << 1) & m` consumes `r1`,
    // so it can only express a WRONG first scalar (substitution). A leading
    // DELETION has the first pattern scalar ABSENT, which needs the single
    // error already spent when text[0] arrives - hence the pre-seeded bit.
    let mut r1: u64 = 1;

    for ch in text.chars() {
        let m = lookup(&keys, &vals, n, fold_case(ch) as u32);
        // The PRE-shift 0-error state: it is what the 1-error terms below are
        // derived from, and it has to be read before `r0` is advanced.
        let prev_r0 = r0;

        // 0 errors: shift, allow a fresh start, demand a matching scalar.
        r0 = ((r0 << 1) | 1) & m;

        // 1 error: continue an existing 1-error match, or derive one from the
        // 0-error state by substitution / insertion / deletion. The `| 1`
        // seeds the 1-error state on a FRESH empty prefix, so the first
        // pattern scalar may be wrong (W2: first-letter edits) - e.g. the
        // query "ononymo" can reach a stored name starting "...Anonymo".
        //
        // The terms (Wu-Manber k=1 with the W2 seed):
        //   prev_r0            insertion    - the TEXT gained one scalar the
        //                      pattern does not have, so the pattern position
        //                      stands still (no shift) and the error is spent
        //                      on the extra text scalar. Unshifted is the
        //                      point: a shifted term would consume a pattern
        //                      position that was never matched.
        //   (prev_r0 << 1)     substitution - this text scalar is wrong; the
        //                      state advances one bit on the pattern.
        //   ((prev_r0 << 2) & m) deletion   - skip ONE pattern scalar, then
        //                      this text scalar must match the NEXT one, so
        //                      the state advances two bits and is masked.
        //   ((r1 << 1) & m)    keep going with the error already spent.
        //
        // The masked single-shift term that previously stood here duplicated
        // `r0` (a subset of the no-error state) and is dropped; it is NOT the
        // substitution, and the unshifted insertion term is the member that
        // was missing - which is why "abcd" could not match "abxcd" (text
        // gained a scalar in the MIDDLE).
        r1 = ((r1 << 1) & m)
            | 1 // fresh start may carry one error (first scalar may differ)
            | prev_r0 // insertion (an extra scalar in the text, pattern stands)
            | (prev_r0 << 1) // substitution (this text scalar is wrong)
            | ((prev_r0 << 2) & m); // deletion (one pattern scalar skipped)

        // Early exit: if no state can possibly reach the finish line,
        // further characters cannot create a match.
        if r0 == 0 && r1 == 0 {
            return false;
        }

        if (r0 & finish) != 0 || (r1 & finish) != 0 {
            return true;
        }
    }

    // Trailing-deletion completion: the masked deletion term `((prev_r0 << 2)
    // & m)` cannot fire when the deleted scalar is the LAST pattern scalar -
    // there is no following text scalar to satisfy the mask. If the 0-error
    // state is exactly one scalar short of `finish`, the final pattern scalar
    // is simply absent from the text. `r0` (not `r1`) is the source so a
    // substitution plus a deletion (two edits) cannot pass through here.
    if ((r0 << 1) & finish) != 0 {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match() {
        assert!(bitap_match("иван", "иван"));
        assert!(bitap_match("иванов", "иванов"));
    }

    #[test]
    fn single_substitution() {
        assert!(bitap_match("ивонов", "иванов")); // о -> а
    }

    #[test]
    fn prefix_zero_errors() {
        assert!(bitap_match("иван", "иванов")); // insertion of "о"
        assert!(bitap_match("ив", "иванов")); // prefix, 0 errors
    }

    #[test]
    fn shorter_pattern_matches_as_approximate_substring() {
        // "ивн" occurs inside "иванов" with one edit ("иван" differs by the
        // dropped "о"), so at threshold 1 it matches.
        assert!(bitap_match("ивн", "иванов"));
    }

    /// One DELETION from the text side: the text drops a scalar the pattern
    /// has. This is the member the issue reports missing, and it cannot be
    /// covered by an exact substring - "abd" is not inside "abcd".
    #[test]
    fn one_deletion_in_the_text_matches() {
        // Substring start at index 0: the text is one scalar SHORTER than the
        // pattern, so the alignment has to spend the single error on skipping
        // the pattern's scalar that the text does not have.
        assert!(bitap_match("abcd", "abd"), "\"abd\" is \"abcd\" minus one");
    }

    /// A one-edit deletion at the TRAILING edge: the text drops the last
    /// pattern scalar. The masked deletion term cannot fire there (no following
    /// text scalar to satisfy the mask), so the post-loop completion decides.
    #[test]
    fn trailing_deletion_matches() {
        assert!(
            bitap_match("abcd", "abc"),
            "\"abc\" is \"abcd\" minus the trailing \"d\""
        );
    }

    /// A one-edit deletion at the LEADING edge: the text drops the first
    /// pattern scalar. The in-loop `| 1` seed is one iteration late to express
    /// an ABSENT first scalar, so the seed is pre-applied to `r1`.
    #[test]
    fn leading_deletion_matches() {
        assert!(
            bitap_match("abcd", "bcd"),
            "\"bcd\" is \"abcd\" minus the leading \"a\""
        );
    }

    /// A substitution plus a trailing deletion is TWO edits: the post-loop
    /// completion reads `r0` (the 0-error state) only, so a substitution that
    /// derails the exact match cannot sneak through.
    #[test]
    fn substitution_plus_trailing_deletion_rejected() {
        assert!(
            !bitap_match("abc", "ax"),
            "\"ax\" needs a substitution and a deletion"
        );
    }

    /// One deletion in the TEXT must not be confused with the reverse: the
    /// pattern dropping a scalar is covered by the substring rule above.
    #[test]
    fn one_extra_scalar_in_the_text_matches() {
        assert!(bitap_match("abc", "abcd"));
    }

    /// The reported M12 case: the text gained a scalar in the MIDDLE, not at
    /// the tail. The alignment must spend the single error on the insertion
    /// (pattern stands still at "c" while the text passes "x") and then resume
    /// matching "cd". A shifted insertion term consumed a pattern position
    /// that was never matched, so this read as dist=2 and failed.
    #[test]
    fn one_extra_scalar_in_the_middle_matches() {
        assert!(
            bitap_match("abcd", "abxcd"),
            "\"abxcd\" is \"abcd\" plus one middle scalar"
        );
        assert!(
            bitap_match("abc", "abxcd"),
            "\"abc\" occurs in \"abxcd\" within one edit"
        );
    }

    #[test]
    fn pattern_longer_than_text_is_rejected() {
        // A pattern cannot match text shorter than itself: "иванов" (4 scalars
        // past "ив") has no room to align within one edit of "ивн".
        assert!(!bitap_match("иванов", "ивн"));
    }

    #[test]
    fn three_substitutions_rejected() {
        // "ивн" vs "сидоров" shares no aligned run within one edit.
        assert!(!bitap_match("ивн", "сидоров"));
    }

    #[test]
    fn disjoint_rejected() {
        assert!(!bitap_match("иван", "петров"));
    }

    #[test]
    fn empty_pattern_matches() {
        assert!(bitap_match("", "anything"));
    }

    #[test]
    fn ascii_case_insensitive() {
        assert!(bitap_match("Ivan", "IVANOV"));
        assert!(bitap_match("ivan", "Ivan"));
    }

    #[test]
    fn long_pattern_falls_back_to_contains() {
        let long = "x".repeat(70);
        let target = format!("zz{}{}", "x".repeat(70), "zz");
        assert!(bitap_match(&long, &target));
        assert!(!bitap_match(&long, "nothing-here"));
    }

    // first-letter edits. The query layer lowercases both sides
    // before calling in, so a capitalised stored name is folded away; the
    // matcher must still accept a 1-edit alignment whose FIRST scalar differs
    // (the no-seed run anchored on the exact first letter and missed it).
    #[test]
    fn w2_first_letter_substitution_matches() {
        assert!(bitap_match("ononymo", "anonymo"));
    }

    #[test]
    fn w2_first_letter_substitution_case_insensitive() {
        // The stored name is capitalised; ASCII case is folded in the mask.
        assert!(bitap_match("ononymo", "Anonymo"));
    }

    #[test]
    fn w2_first_letter_prefix_matches() {
        // Dropping/leading only the first scalar leaves "nonymo" as a prefix.
        assert!(bitap_match("nonymo", "anonymo"));
        assert!(bitap_match("onymo", "anonymo"));
    }

    #[test]
    fn w2_second_letter_still_requires_a_real_edit() {
        // Two scalar differences cannot slide past as one seeded edit.
        assert!(!bitap_match("otonymo", "anonymo"));
    }

    #[test]
    fn w2_double_deletion_does_not_masquerade_as_one() {
        // Explicit guard: `| 1` must NOT let a two-edit
        // DELETION (here: both `bc` dropped from the pattern) read as a single
        // edit. `abcd` vs `ad` needs two deletions -> rejected.
        assert!(!bitap_match("abcd", "ad"));
    }

    #[test]
    fn w2_pattern_longer_than_text_still_rejected() {
        // A fresh seeded start cannot conjure edits the text cannot hold.
        assert!(!bitap_match("aeiou", "aiu"));
    }

    /// A pattern of `MASK_CAP` DISTINCT ASCII letters must not overflow.
    ///
    /// Each pattern scalar used to be stored as up to THREE keys (the scalar
    /// plus its two ASCII case variants), so 38 distinct letters already
    /// needed more slots than the table has. `add_mask` has no bounds check -
    /// it writes `keys[*n]` directly - so the 64th key was an out-of-bounds
    /// write, and a remote client could take the server down with one QIDO
    /// query. `MASK_CAP` bounds SCALARS, so the table must hold at most one
    /// entry per scalar.
    #[test]
    fn a_pattern_of_mask_cap_distinct_letters_does_not_overflow_the_table() {
        // 38 distinct mixed-case letters: the exact shape the review reported.
        let upper: String = ('A'..='Z').collect();
        let lower: String = ('a'..='l').collect();
        let pattern = format!("{upper}{lower}");
        assert_eq!(
            pattern.chars().count(),
            38,
            "the fixture must be the reported shape: 38 distinct scalars"
        );
        // Must not panic; that is the whole point.
        let matched = bitap_match(&pattern, &pattern.to_lowercase());
        assert!(
            matched,
            "a pattern must match its own lowercase spelling, case-insensitively"
        );
        assert!(!bitap_match(&pattern, "nothing like it"));
    }

    /// The table tolerates the longest pattern it can hold without a panic.
    ///
    /// `MASK_CAP` distinct scalars is the documented ceiling; the case fold
    /// must not push it over. Distinct printable ASCII keeps every scalar its
    /// own key, which is the worst case for the table.
    #[test]
    fn a_pattern_of_mask_cap_letters_does_not_overflow_the_table() {
        let scalars: Vec<char> = ('!'..='~').collect();
        assert!(
            scalars.len() >= MASK_CAP,
            "the fixture needs at least MASK_CAP distinct scalars"
        );
        let pattern: String = scalars.into_iter().take(MASK_CAP).collect();
        assert_eq!(pattern.chars().count(), MASK_CAP);
        // Must not panic; that is the whole point.
        assert!(bitap_match(&pattern, &pattern));
        assert!(!bitap_match(&pattern, "nothing like it"));
    }

    /// Case-insensitive matching still works after the fold.
    #[test]
    fn ascii_case_folding_still_matches_either_case() {
        assert!(bitap_match("Anonymo", "anonymo"));
        assert!(bitap_match("anonymo", "Anonymo"));
        assert!(bitap_match("Anonymo", "ANONYMO"));
    }
}
