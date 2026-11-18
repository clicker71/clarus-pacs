// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! PersonName n-gram index (`idx/ngrams/` and `idx/physician/`).
//!
//! Fuzzymatch on `PatientName` / `ReferringPhysicianName` would otherwise need
//! a full `manifests/` scan. This index narrows the candidate study set: each
//! query name is broken into n-grams, the per-gram daily `.idx` files are read,
//! and their study-UID lists are intersected - any study sharing every n-gram
//! of the (short) query is a candidate.
//!
//! # Layout
//!
//! ```text
//! idx/
//! +-- ngrams/{hex_ngram}/YYYYMMDD.idx      <- patient names
//! +-- physician/{hex_ngram}/YYYYMMDD.idx   <- referring physicians
//! ```
//!
//! Each `.idx` row is `study_uid\n`, appended with a single `O_APPEND` write -
//! a mirror of the modality index contract (`idx::modality`). The day string
//! is `YYYYMMDD`; a query reads all days it is interested in and intersects.
//!
//! # Correctness model
//!
//! Like every `.idx`, this is an accelerator, not a source of truth. A stale or
//! missing n-gram file only widens the candidate set back to a full scan; the
//! manifest still decides the final match via `QueryFilter`.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// Which PersonName attribute an n-gram belongs to.
///
/// Each kind indexes under its own root so patient and physician names never
/// collide (a shared study can appear under both without ambiguity).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameKind {
    /// `PatientName` (0010,0010) - `idx/ngrams/`.
    Patient,
    /// `ReferringPhysicianName` (0008,0090) - `idx/physician/`.
    Physician,
}

/// The root directory name for a [`NameKind`].
pub(crate) fn kind_root(kind: NameKind) -> &'static str {
    match kind {
        NameKind::Patient => "ngrams",
        NameKind::Physician => "physician",
    }
}

/// Split a DICOM PersonName into hex-encoded UTF-8 n-grams.
///
/// Adaptive length: 1 char -> unigram, 2 chars -> bigram, >=3 -> trigram. DICOM PN
/// separators (`^`, `=`) are removed before extraction so the name reads as a
/// flat run of characters. The result is lowercased (multi-char lowercasing,
/// e.g. terminal sigma, is folded like a single replacement) before n-gram
/// extraction, and each n-gram's UTF-8 bytes are hex-encoded (two ASCII chars
/// per byte) so any name has a filesystem-safe key independent of charset.
///
/// Returns the hex keys in window order. An empty name yields an empty set.
pub fn name_ngrams(name: &str) -> Vec<String> {
    let cleaned: String = name
        .chars()
        .filter(|&c| c != '^' && c != '=') // DICOM PN separators
        .flat_map(|c| c.to_lowercase())
        .collect();
    let chars: Vec<char> = cleaned.chars().collect();
    let n = match chars.len() {
        0 => return Vec::new(),
        1 => 1,
        2 => 2,
        _ => 3,
    };
    chars
        .windows(n)
        .map(|w| {
            let s: String = w.iter().collect();
            s.as_bytes()
                .iter()
                .fold(String::with_capacity(s.len() * 2), |mut acc, b| {
                    use std::fmt::Write;
                    write!(&mut acc, "{b:02x}").unwrap();
                    acc
                })
        })
        .collect()
}

/// Normalize a DICOM PersonName for **fuzzy matching** by replacing the PN
/// separator characters with a space.
///
/// Per PS3.5 Section 6.2.1 a Person Name groups are separated by `^` and group
/// components by `=`. A fuzzymatch query typed by hand uses spaces where a
/// stored name has `^`/`=`; without normalization the bitap threshold (exactly
/// one edit) would count `^`<->space as a first edit and reject a name that is
/// otherwise a 1-edit match. Replacing both separators with a space lets both
/// sides line up, so e.g. query `Солгалов Даниил` matches stored
/// `СОЛГАЛОВ^ДАННИЛ^ЮРЬЕВИЧ` (only the `И`<->`Н` in `Даниил`/`Даннил` counts).
///
/// Applied ONLY in the fuzzy path (n-gram indexing, fuzzy query grams, and the
/// bitap predicate, before lowercasing). Exact search and wildcards use the raw
/// PN. Stored manifest data is not modified; this is a transient in-memory
/// normalization whose lowercase n-gram keys (with spaces) are what the index
/// stores (indices are rebuilt, so no migration is needed).
///
/// Returns an owned normalized string; empty input stays empty.
pub fn fuzzy_normalize(name: &str) -> String {
    name.chars()
        .map(|c| if c == '^' || c == '=' { ' ' } else { c })
        .collect()
}

/// Number of striped locks. 64 is a compromise: enough that unrelated
/// n-gram/day files rarely collide, few enough that the table is 64 `Arc`s.
const NGRAM_LOCK_STRIPES: usize = 64;

/// Process-wide striped locks, one per `hash(kind, ngram, day) % 64`.
///
/// A fixed table (not a per-file map) because the job is to serialise the
/// repair-then-append of ONE file, and a fixed table does that without
/// growing unboundedly with the number of keys ever seen. Same shape as the
/// UPS store's `uid_lock`. The table is preallocated on first use; looking up
/// a stripe and taking the mutex allocates nothing.
pub fn ngram_lock(kind: NameKind, ngram: &str, day: &str) -> Arc<Mutex<()>> {
    static STRIPES: OnceLock<Vec<Arc<Mutex<()>>>> = OnceLock::new();
    let stripes = STRIPES.get_or_init(|| {
        (0..NGRAM_LOCK_STRIPES)
            .map(|_| Arc::new(Mutex::new(())))
            .collect()
    });
    // FNV-1a over the (kind, ngram, day) identity; the same cheap mix the UPS
    // store uses. The distribution only has to beat "every key in one stripe".
    let mut h: u32 = 0x811c_9dc5;
    h ^= kind as u32;
    h = h.wrapping_mul(0x0100_0193);
    for &b in ngram.as_bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    for &b in day.as_bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    Arc::clone(&stripes[(h as usize) % NGRAM_LOCK_STRIPES])
}

/// Open (or create) the append-only handle for one n-gram / day `.idx`.
///
/// `ngram` is a hex key from [`name_ngrams`]; it is a pure hex string (2 chars
/// per byte) so it never contains a path separator. The empty n-gram is
/// rejected because it would collide with the root bucket.
///
/// The torn tail is repaired BEFORE the handle is handed out. Rows here are
/// `study_uid\n`, so a crash mid-append - or an ENOSPC that wrote a prefix -
/// leaves an unterminated suffix; appending onto it would fuse the old
/// fragment and the new UID into one row that names no study. This writer
/// already opens a fresh handle per append, so the repair needs no extra
/// coordination with a held handle: nothing else in this process holds one.
fn open_appender(dir: &Path, kind: NameKind, ngram: &str, day: &str) -> io::Result<File> {
    if ngram.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ngram index: empty ngram",
        ));
    }
    let path = file_path(dir, kind, ngram, day);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Repair runs before the handle exists, so the truncate cannot race a
    // write this process is still making on this path.
    super::sop_owner::repair_torn_newline_tail(&path)?;
    OpenOptions::new().create(true).append(true).open(&path)
}

/// Full path of an n-gram/day `.idx` file.
pub fn file_path(dir: &Path, kind: NameKind, ngram: &str, day: &str) -> PathBuf {
    dir.join(kind_root(kind))
        .join(ngram)
        .join(format!("{day}.idx"))
}

/// Append one `study_uid\n` row to an n-gram/day index.
///
/// Best-effort append-only write; failures are the caller's to decide (a STOW
/// that cannot index still commits its manifest - the index is an accelerator).
pub fn append(
    dir: &Path,
    kind: NameKind,
    ngram: &str,
    study_uid: &str,
    day: &str,
) -> io::Result<()> {
    // Hold the file's stripe across repair + append: two concurrent appends
    // for the same n-gram/day could both see a torn tail, compute the same
    // truncation point, and have the second repair cut off the first's freshly
    // appended row. One stripe serialises the whole read-modify-append.
    let stripe = ngram_lock(kind, ngram, day);
    let _guard = stripe
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut file = open_appender(dir, kind, ngram, day)?;
    // ONE write. Writing the uid and the newline separately opened the window
    // this file's whole format depends on being closed: two appenders could
    // interleave as `uidAuidB\n\n` - which reads back as a single bogus row
    // plus an empty one, silently losing a study from the fuzzy index. A DICOM
    // UID is short, so a stack buffer covers it: 1 KiB is far above the 64
    // characters PS3.5 allows, and a longer value still works through the
    // fallback (it is not a valid UID, but the append must not fail on it).
    let bytes = study_uid.as_bytes();
    // `len < ROW_BUF` reads as "the uid and its newline both fit".
    if bytes.len() < ROW_BUF {
        let mut row = [0u8; ROW_BUF];
        row[..bytes.len()].copy_from_slice(bytes);
        row[bytes.len()] = b'\n';
        file.write_all(&row[..bytes.len() + 1])?;
    } else {
        let mut row = Vec::with_capacity(bytes.len() + 1);
        row.extend_from_slice(bytes);
        row.push(b'\n');
        file.write_all(&row)?;
    }
    Ok(())
}

/// Stack buffer for one `uid\n` row. `PS3.5` caps a UID at 64 characters.
const ROW_BUF: usize = 1024;

/// Read every stored study UID from an n-gram/day `.idx` file.
///
/// A missing file yields an empty list (no candidates from that gram). A torn
/// trailing row (crash during append) is dropped, matching the modality
/// reader's semantics.
pub fn read_study_uids(
    dir: &Path,
    kind: NameKind,
    ngram: &str,
    day: &str,
) -> io::Result<Vec<String>> {
    let path = file_path(dir, kind, ngram, day);
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    // A row is `uid\n`; the final segment without a terminating newline is a
    // torn append (a crash between `write_all` and the newline), not a study
    // UID. `split` returns it as if it were whole, so it is excluded below.
    let mut segments: Vec<&[u8]> = data.split(|&b| b == b'\n').collect();
    if data.last() != Some(&b'\n') {
        segments.pop();
    }
    for line in segments {
        if line.is_empty() {
            continue;
        }
        out.push(String::from_utf8_lossy(line).into_owned());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigram_ivanov() {
        let keys = name_ngrams("Иванов");
        assert_eq!(keys.len(), 4);
        // Lowercased "иванов": first trigram "ива" -> d0b8 d0b2 d0b0.
        assert_eq!(keys[0], "d0b8d0b2d0b0");
    }

    #[test]
    fn bigram_cjk() {
        // "张伟" (2 chars) -> single bigram key.
        let keys = name_ngrams("张伟");
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0], "e5bca0e4bc9f");
    }

    #[test]
    fn unigram_single_char() {
        // "О" (1 char) -> lowercased single unigram "о" -> d0be.
        let keys = name_ngrams("О");
        assert_eq!(keys, vec!["d0be"]);
    }

    #[test]
    fn bigram_ascii() {
        // "Li" -> lowercase "li" -> 6c69.
        let keys = name_ngrams("Li");
        assert_eq!(keys, vec!["6c69"]);
    }

    #[test]
    fn separator_stripped() {
        // "Иванов^Иван" -> "ивановиван" (10 chars -> 8 trigrams).
        let keys = name_ngrams("Иванов^Иван");
        assert_eq!(keys.len(), 8);
        assert!(!keys.iter().any(|k| k.contains("5e"))); // no '^' (0x5e)
    }

    #[test]
    fn empty_name_yields_no_grams() {
        assert_eq!(name_ngrams(""), Vec::<String>::new());
    }

    #[test]
    fn fuzzy_normalize_separators_to_spaces() {
        // PN group separator `^` and component separator `=` both become a
        // space (PS3.5 Section 6.2.1); everything else is left as-is (case untouched -
        // lowercasing happens later in the path).
        assert_eq!(fuzzy_normalize("Иванов^Иван"), "Иванов Иван");
        assert_eq!(fuzzy_normalize("Иванов=Иван"), "Иванов Иван");
        assert_eq!(
            fuzzy_normalize("СОЛГАЛОВ^ДАННИЛ^ЮРЬЕВИЧ"),
            "СОЛГАЛОВ ДАННИЛ ЮРЬЕВИЧ"
        );
        // No separators -> unchanged; empty stays empty.
        assert_eq!(fuzzy_normalize("Иванов"), "Иванов");
        assert_eq!(fuzzy_normalize(""), "");
    }

    #[test]
    fn append_and_read_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let day = "20260811";
        append(dir.path(), NameKind::Patient, "abc", "1.2.3", day).expect("append");
        // A different kind uses a different root.
        append(dir.path(), NameKind::Physician, "abc", "9.9.9", day).expect("append physician");

        let patient = read_study_uids(dir.path(), NameKind::Patient, "abc", day).expect("read");
        assert_eq!(patient, vec!["1.2.3"]);

        let physician =
            read_study_uids(dir.path(), NameKind::Physician, "abc", day).expect("read phys");
        assert_eq!(physician, vec!["9.9.9"]);
    }

    #[test]
    fn missing_file_reads_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = read_study_uids(dir.path(), NameKind::Patient, "abc", "20260811").expect("read");
        assert!(out.is_empty());
    }

    /// A torn suffix must be repaired before the next UID is appended.
    ///
    /// A crash (or an ENOSPC that wrote a prefix) leaves an unterminated
    /// fragment. Appending onto it fuses the fragment and the new UID into one
    /// row, and the reader returns that concatenation as a study UID that
    /// exists nowhere.
    #[test]
    fn a_torn_suffix_is_repaired_before_the_next_append() {
        let dir = tempfile::tempdir().expect("tempdir");
        let day = "20260811";
        append(dir.path(), NameKind::Patient, "abcd", "1.2.3", day).expect("seed");
        let path = file_path(dir.path(), NameKind::Patient, "abcd", day);
        // An unterminated suffix is on disk before the next append runs.
        {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open");
            f.write_all(b"1.2.").expect("torn write");
        }

        append(dir.path(), NameKind::Patient, "abcd", "9.8.7", day).expect("append");

        let uids = read_study_uids(dir.path(), NameKind::Patient, "abcd", day).expect("read");
        assert_eq!(
            uids,
            vec!["1.2.3".to_string(), "9.8.7".to_string()],
            "the fragment must not be fused onto the new UID"
        );
    }

    #[test]
    fn empty_ngram_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(append(dir.path(), NameKind::Patient, "", "1.2.3", "20260811").is_err());
    }

    /// Concurrent appends for the SAME n-gram/day must serialize repair+append:
    /// the final file holds exactly the seed row plus one row per writer, every
    /// row newline-terminated - no lost row, no fused row.
    ///
    /// The fixture is a torn file with a LARGE unterminated tail: both writers
    /// read the whole tail during repair, which widens the window in which one
    /// writer's truncate can land after the other's append. Without the stripe
    /// lock that interleaving drops a row (see the falsification note).
    #[test]
    fn concurrent_appends_never_lose_or_fuse_rows() {
        use std::sync::Barrier;

        let dir = tempfile::tempdir().expect("tempdir");
        let kind = NameKind::Patient;
        let gram = "d0b8d0b2d0b0";
        let day = "20240115";
        let path = file_path(dir.path(), kind, gram, day);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");

        // One complete seed row + a large unterminated tail (no trailing `\n`).
        let torn_tail = vec![b'x'; 8192];
        let mut seed = b"seed\n".to_vec();
        seed.extend_from_slice(&torn_tail);

        const WRITERS: usize = 8;
        const ITERATIONS: usize = 40;

        for _ in 0..ITERATIONS {
            std::fs::write(&path, &seed).expect("seed torn");
            let barrier = Arc::new(Barrier::new(WRITERS));
            let mut handles = Vec::new();
            for i in 0..WRITERS {
                let dir = dir.path().to_path_buf();
                let barrier = Arc::clone(&barrier);
                handles.push(std::thread::spawn(move || {
                    barrier.wait();
                    append(&dir, kind, gram, &format!("2.25.{i}"), day).expect("append");
                }));
            }
            for h in handles {
                h.join().expect("join");
            }

            // A serialized run keeps the seed row, drops the torn tail, and
            // appends exactly one row per writer, all newline-terminated.
            let data = std::fs::read(&path).expect("read");
            assert_eq!(
                data.last(),
                Some(&b'\n'),
                "the file must end with a newline-terminated row"
            );
            let mut rows: Vec<String> = String::from_utf8(data)
                .expect("utf8")
                .split('\n')
                .filter(|r| !r.is_empty())
                .map(str::to_string)
                .collect();
            rows.sort();
            let mut expected: Vec<String> = (0..WRITERS)
                .map(|i| format!("2.25.{i}"))
                .chain(std::iter::once("seed".to_string()))
                .collect();
            expected.sort();
            assert_eq!(
                rows, expected,
                "every appended row must survive, none fused or duplicated"
            );
        }
    }
}
