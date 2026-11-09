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
//!   scalar); those fall back to a plain case-insensitive substring search,
//!   which is at least as broad as a 1-edit match, never narrower.
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
    let mut keys = [0u32; MASK_CAP];
    let mut vals = [0u64; MASK_CAP];
    let mut n = 0usize;
    for (i, ch) in pattern.chars().enumerate() {
        let bit = 1u64 << i;
        add_mask(&mut keys, &mut vals, &mut n, ch as u32, bit);
        // ASCII case variants fold into the same mask entry.
        if ch.is_ascii() {
            let lo = ch.to_ascii_lowercase();
            let up = ch.to_ascii_uppercase();
            if lo != ch {
                add_mask(&mut keys, &mut vals, &mut n, lo as u32, bit);
            }
            if up != ch {
                add_mask(&mut keys, &mut vals, &mut n, up as u32, bit);
            }
        }
    }

    let finish = 1u64 << (plen - 1);
    let mut r0: u64 = 0; // 0 errors
    let mut r1: u64 = 0; // 1 error

    for ch in text.chars() {
        let m = lookup(&keys, &vals, n, ch as u32);
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
        // The terms:
        //   (prev_r0 << 1) & m  substitution - this text scalar is wrong.
        //   (prev_r0 << 2) & m  deletion     - the alignment skips ONE pattern
        //                       scalar and then matches this text scalar
        //                       against the NEXT one, so the state advances
        //                       two bits; the scalar still has to match.
        //   (r1 << 1) & m       keep going with the error already spent.
        //   (prev_r0 << 1)      insertion    - the text gained a scalar the
        //                       pattern does not have: the pattern position
        //                       stands still and the error is spent on it.
        //
        // The insertion term used to stand in for the deletion one, which is
        // why "abcd" could not match "abd": dropping a pattern scalar is the
        // deletion case, and it is the one the fuzzy PatientName search hits
        // in practice ("Atroschenko" typed as "Atroshko").
        r1 = ((r1 << 1) & m)
            | 1 // fresh start may carry one error (first scalar may differ)
            | ((prev_r0 << 1) & m) // substitution
            | ((prev_r0 << 2) & m) // deletion (one pattern scalar skipped)
            | (prev_r0 << 1); // insertion (an extra scalar in the text)

        // Early exit: if no state can possibly reach the finish line,
        // further characters cannot create a match.
        if r0 == 0 && r1 == 0 {
            return false;
        }

        if (r0 & finish) != 0 || (r1 & finish) != 0 {
            return true;
        }
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

    /// One deletion in the TEXT must not be confused with the reverse: the
    /// pattern dropping a scalar is covered by the substring rule above.
    #[test]
    fn one_extra_scalar_in_the_text_matches() {
        assert!(bitap_match("abc", "abcd"));
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
}
