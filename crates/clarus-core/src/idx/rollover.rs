// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Midnight rollover - immutable-ification of all daily `.idx` files.
//!
//! Every `.idx` file is named by its day (`YYYYMMDD.idx`). The rollover makes
//! the immutability decision *explicit*: once the process notices the clock
//! has crossed into a new day, it closes every mutable appender for the old
//! day, registers that day as immutable, and opens fresh appenders for the
//! new day. Because the file name *is* the day, no rename is required - the
//! grace that makes a "true" rename unnecessary on this scheme. What the
//! rollover guarantees is that no writer ever touches a past day again, which
//! is exactly the immutability the mmap read-path relies on.
//!
//! # Clock injection
//!
//! Tests rewind the clock by supplying a [`DaySource`] closure. The rollover
//! reads the day from the source on every `rotate_if_needed()` call, so a
//! mocked source that jumps from `20240115` to `20240116` exercises the same
//! path a wall clock crossing midnight would.

use std::collections::HashSet;
use std::path::PathBuf;

use super::date::{self, DateIndex};
use super::modality::{self, ModalityIndex};
use super::sop_owner::{self, SopOwnerIndex};

/// Longest BLAKE3-hex row (dates) - 65 bytes.
const DATE_ROW_LEN: usize = date::ROW_LEN;
/// Roomy staging for study-UID modality rows (UIDs cap at 64 chars + 1 NL).
const MOD_ROW_LEN: usize = 65;
/// Byte length of a `YYYYMMDD` day key.
const DAY_LEN: usize = 8;

/// Shared registry of the current day's mutable `.idx` appenders.
///
/// Handlers borrow writers by key; a rollover swaps the whole set in one
/// atomic step (behind a single caller-held lock) so readers never observe a
/// half-moved day.
pub struct DailyIndexSet {
    root: PathBuf,
    day: date::DayKey,
    date: Option<DateIndex>,
    modality: Vec<(String, ModalityIndex)>,
    sop_owner: Option<SopOwnerIndex>,
    /// Days already rolled over - immutable, never written again.
    immutable: HashSet<date::DayKey>,
}

/// Failure opening the three mutable appenders at once ([`DailyIndexSet::writers_mut`]).
#[derive(Debug)]
pub enum WritersError {
    /// The date appender could not be opened.
    Date(date::DateIndexError),
    /// The modality appender could not be opened.
    Modality(modality::ModalityIndexError),
    /// The `sop_owner` appender could not be opened.
    SopOwner(sop_owner::SopOwnerError),
}

impl std::fmt::Display for WritersError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WritersError::Date(e) => write!(f, "date index: {e}"),
            WritersError::Modality(e) => write!(f, "modality index: {e}"),
            WritersError::SopOwner(e) => write!(f, "sop_owner index: {e}"),
        }
    }
}

impl std::error::Error for WritersError {}

impl From<date::DateIndexError> for WritersError {
    fn from(e: date::DateIndexError) -> Self {
        WritersError::Date(e)
    }
}

impl From<modality::ModalityIndexError> for WritersError {
    fn from(e: modality::ModalityIndexError) -> Self {
        WritersError::Modality(e)
    }
}

impl From<sop_owner::SopOwnerError> for WritersError {
    fn from(e: sop_owner::SopOwnerError) -> Self {
        WritersError::SopOwner(e)
    }
}

impl DailyIndexSet {
    /// Create an empty registry rooted at `idx/` for a given day.
    ///
    /// `now` is the day source used for the initial day key and future
    /// rollover checks. No files are created until a writer is opened.
    pub fn new(root: PathBuf, day: date::DayKey) -> Self {
        Self {
            root,
            day,
            date: None,
            modality: Vec::new(),
            sop_owner: None,
            immutable: HashSet::new(),
        }
    }

    /// The day the registry currently writes to.
    pub fn current_day(&self) -> &date::DayKey {
        &self.day
    }

    /// Days that have been rolled over (immutable).
    pub fn immutable_days(&self) -> &HashSet<date::DayKey> {
        &self.immutable
    }

    /// Open (or reuse) the mutable date appender for the current day.
    pub fn date_writer(&mut self) -> Result<&mut DateIndex, date::DateIndexError> {
        if self.date.is_none() {
            self.date = Some(date::open_appender(&self.root, &self.day)?);
        }
        Ok(self.date.as_mut().expect("just initialised"))
    }

    /// Open (or reuse) the mutable modality appender for the current day.
    pub fn modality_writer(
        &mut self,
        modality: &str,
    ) -> Result<&mut ModalityIndex, modality::ModalityIndexError> {
        if let Some(pos) = self.modality.iter().position(|(m, _)| m == modality) {
            return Ok(&mut self.modality[pos].1);
        }
        let idx = modality::open_appender(&self.root, modality, &self.day)?;
        self.modality.push((modality.to_string(), idx));
        Ok(&mut self.modality.last_mut().expect("just pushed").1)
    }

    /// Open (or reuse) the mutable `sop_owner` appender for the current day.
    pub fn sop_owner_writer(&mut self) -> Result<&mut SopOwnerIndex, sop_owner::SopOwnerError> {
        if self.sop_owner.is_none() {
            self.sop_owner = Some(sop_owner::open_appender(&self.root, &self.day)?);
        }
        Ok(self.sop_owner.as_mut().expect("just initialised"))
    }

    /// Open (or reuse) the three mutable appenders at once for the current day.
    ///
    /// The three target DISJOINT fields (`date`, `modality`, `sop_owner`), so
    /// the borrow checker accepts the simultaneous `&mut` returns - which the
    /// single-writer accessors above cannot offer (each borrows all of
    /// `self`). The warm append path needs all three at once.
    ///
    /// Opening is COLD (allocates a modality bucket entry, opens files on
    /// first use of the day); it is the caller's responsibility to treat this
    /// as the cold half of the warm/cold split.
    #[allow(clippy::type_complexity)]
    pub fn writers_mut(
        &mut self,
        modality: &str,
    ) -> Result<(&mut DateIndex, &mut ModalityIndex, &mut SopOwnerIndex), WritersError> {
        // Open `date` first (own field), then `sop_owner` (own field), then
        // resolve `modality` - each touches only its own field, so no two
        // `&mut` overlap the same data.
        if self.date.is_none() {
            self.date = Some(date::open_appender(&self.root, &self.day)?);
        }
        if self.sop_owner.is_none() {
            self.sop_owner = Some(sop_owner::open_appender(&self.root, &self.day)?);
        }
        let pos = match self.modality.iter().position(|(m, _)| m == modality) {
            Some(pos) => pos,
            None => {
                let idx = modality::open_appender(&self.root, modality, &self.day)?;
                self.modality.push((modality.to_string(), idx));
                self.modality.len() - 1
            }
        };
        let (date, sop_owner) = (
            self.date.as_mut().expect("just initialised"),
            self.sop_owner.as_mut().expect("just initialised"),
        );
        let modality_writer = &mut self.modality[pos].1;
        Ok((date, modality_writer, sop_owner))
    }

    /// Roll over to `new_day` if it differs from the current day.
    ///
    /// Closes every mutable appender, records the old day as immutable, and
    /// clears the writers so the next access reopens against the new day.
    /// Returns `true` if a rollover happened.
    pub fn rollover_if_needed(&mut self, new_day: date::DayKey) -> bool {
        if new_day == self.day {
            return false;
        }
        // Drop all open handles for the old day - on Windows a rename (or the
        // mmap read path) cannot proceed while a writer handle is open.
        self.date = None;
        self.modality.clear();
        self.sop_owner = None;
        self.immutable.insert(self.day);
        self.day = new_day;
        true
    }

    /// The root directory of the index tree.
    pub fn root(&self) -> &PathBuf {
        &self.root
    }
}

/// A zero-allocation hotpath append through the registry.
///
/// Reads the day key from the wall clock, rolls over if needed, then appends
/// the BLAKE3 hash to the date index using a fixed staging buffer. This is the
/// `TestAllocator` target: it must not allocate.
pub fn append_date_row<F>(
    set: &mut DailyIndexSet,
    now: F,
    blake3: &[u8; 32],
) -> Result<(), date::DateIndexError>
where
    F: Fn() -> date::DayKey,
{
    let day = now();
    set.rollover_if_needed(day);
    let mut staging = [0u8; DATE_ROW_LEN];
    set.date_writer()?.append(blake3, &mut staging)
}

/// Zero-allocation hotpath append of a study UID to the modality index.
pub fn append_modality_row<F>(
    set: &mut DailyIndexSet,
    now: F,
    modality: &str,
    study_uid: &str,
) -> Result<(), modality::ModalityIndexError>
where
    F: Fn() -> date::DayKey,
{
    let day = now();
    set.rollover_if_needed(day);
    let mut row = [0u8; MOD_ROW_LEN];
    let filled = modality::format_row(study_uid, &mut row);
    set.modality_writer(modality)?.append(filled)
}

/// Build a `YYYYMMDD` day key from a `SystemTime`.
///
/// Computed in UTC so a midnight boundary is unambiguous regardless of the
/// host's local time zone. A rollover triggered by this source is exact; the
/// caller may choose a local-time source and document it.
pub fn day_key_from_system_time(t: std::time::SystemTime) -> date::DayKey {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = secs / 86_400;
    // Civil date via the common civil-from-days algorithm (Howard Hinnant).
    // `days` is post-epoch (u64), so z is always non-negative - the signed
    // branch of the original algorithm is unreachable and elided.
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y }; // civil year

    let mut day = [b'0'; DAY_LEN];
    day[0] = b'0' + (y / 1000 % 10) as u8;
    day[1] = b'0' + (y / 100 % 10) as u8;
    day[2] = b'0' + (y / 10 % 10) as u8;
    day[3] = b'0' + (y % 10) as u8;
    day[4] = b'0' + (m / 10) as u8;
    day[5] = b'0' + (m % 10) as u8;
    day[6] = b'0' + (d / 10) as u8;
    day[7] = b'0' + (d % 10) as u8;
    day
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn day_key_matches_known_epoch() {
        // 2024-01-15T00:00:00Z = 1705276800
        let t = UNIX_EPOCH + Duration::from_secs(1_705_276_800);
        assert_eq!(day_key_from_system_time(t), *b"20240115");
    }

    #[test]
    fn day_key_rolls_at_epoch_boundary() {
        // 2024-01-15 is day 19737 since epoch; its last second is
        // 19737*86400-1 = 1705363199, and day 19738 (2024-01-16) starts at
        // 1705363200.
        let before = day_key_from_system_time(UNIX_EPOCH + Duration::from_secs(1_705_363_199));
        let after = day_key_from_system_time(UNIX_EPOCH + Duration::from_secs(1_705_363_200));
        assert_eq!(before, *b"20240115");
        assert_eq!(after, *b"20240116");
    }

    #[test]
    fn rollover_marks_previous_day_immutable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut set = DailyIndexSet::new(dir.path().to_path_buf(), *b"20240115");

        // Write into day 15 across date + modality.
        let blob = [0xABu8; 32];
        append_date_row(&mut set, || *b"20240115", &blob).expect("date");
        let mut row = [0u8; MOD_ROW_LEN];
        let filled = modality::format_row("uid1", &mut row);
        set.modality_writer("CT")
            .expect("mod")
            .append(filled)
            .expect("mod a");

        // Advance the clock to day 16 -> rollover.
        assert!(set.rollover_if_needed(*b"20240116"));
        assert_eq!(set.current_day(), b"20240116");
        assert!(set.immutable_days().contains(b"20240115"));
        assert!(!set.immutable_days().contains(b"20240116"));

        // Same-day check does nothing.
        assert!(!set.rollover_if_needed(*b"20240116"));
    }

    #[test]
    fn post_rollover_writes_go_to_new_day() {
        let dir = tempfile::tempdir().expect("tempdir");
        let day15 = *b"20240115";
        let day16 = *b"20240116";
        let mut set = DailyIndexSet::new(dir.path().to_path_buf(), day15);

        let blob = [1u8; 32];
        append_date_row(&mut set, || day15, &blob).expect("d15");

        // New day writes go to a NEW file.
        let blob2 = [2u8; 32];
        append_date_row(&mut set, || day16, &blob2).expect("d16");

        let d15_path = date::file_path(dir.path(), &day15);
        let d16_path = date::file_path(dir.path(), &day16);
        assert!(d15_path.exists());
        assert!(d16_path.exists());
        // Day 16 file holds only the day-16 hash.
        let d16 = std::fs::read(&d16_path).expect("read d16");
        assert_eq!(d16.len(), DATE_ROW_LEN);
        assert_eq!(&d16[..2], b"02", "second hash must land in the new day");
    }

    #[test]
    fn clock_rewind_drives_append_path() {
        // The append_*_row helpers take a `now` closure - a rewindable clock.
        // Here we assert that a source which never leaves day 15 keeps writing
        // into the same file (no spurious rollover).
        let dir = tempfile::tempdir().expect("tempdir");
        let mut set = DailyIndexSet::new(dir.path().to_path_buf(), *b"20240115");
        let mut blob = [7u8; 32];
        appendix(&mut set, &mut blob, || *b"20240115", 3);
        let data = std::fs::read(date::file_path(dir.path(), b"20240115")).expect("read");
        assert_eq!(data.len(), 3 * DATE_ROW_LEN);
    }

    /// Append `n` hashes while holding the clock at a fixed day.
    fn appendix<F>(set: &mut DailyIndexSet, blob: &mut [u8; 32], now: F, n: usize)
    where
        F: Fn() -> date::DayKey,
    {
        for i in 0..n {
            blob[0] = i as u8;
            append_date_row(set, &now, blob).expect("append");
        }
    }
}
