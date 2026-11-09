// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Shadow-tier writer identity registry (B-008a).
//!
//! Clarus does **not** authenticate - that is the reverse proxy responsible
//! (zone nginx). This registry only maps a validated `X-Authenticated-User`
//! header value to a compact 32-bit token used in CBOR journal entries, so a
//! 64-byte journal token never has to carry a variable-length username.
//!
//! # Zero-allocation hotpath
//!
//! [`UserRegistry::resolve`] is allocation-free: it hashes the username into
//! a 1024-slot fixed table and follows the collision chain. No `String`, no
//! `Vec`, no heap in the lookup path. Inserts ("promotion" of an unseen
//! username) allocate inside the cell table once per new user - that is cold
//! path (each distinct user registers exactly once).

/// Number of slots in the fixed RAM table. 2^10.
pub const REGISTRY_SLOTS: usize = 1024;

/// Token reported for a writer the registry could not attribute.
///
/// Journal tokens are 32-bit, so no real user can be excluded from the token
/// space; this value is reserved instead and [`UserRegistry::resolve`] skips
/// it when minting a token. It is what `by_token` answers for on a full table,
/// where the name genuinely could not be registered.
pub const RESERVED_UNATTRIBUTED: u32 = 0;

/// A registered username plus its 32-bit token.
#[derive(Debug, Clone)]
pub struct UserMapEntry {
    /// The original username (e.g. `dr.smith`), resolved to text in plaintext
    /// logs.
    pub name: String,
    /// Stable 32-bit token stored inside CBOR journal entries.
    pub token: u32,
}

/// A single slot in the fixed table.
///
/// `None` = empty; `Some(entry)` = a registered user. Collisions are handled
/// by linear probing within the fixed array ([`UserRegistry`] never grows the
/// table).
#[derive(Debug, Default)]
struct Slot(Option<UserMapEntry>);

/// Fixed-size username -> token registry.
pub struct UserRegistry {
    slots: Box<[Slot]>,
}

impl Default for UserRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl UserRegistry {
    /// Create a fresh 1024-slot registry.
    ///
    /// The backing array is boxed once at construction; the hotpath lookup
    /// (linear probe) never allocates after this.
    pub fn new() -> Self {
        let mut slots = Vec::with_capacity(REGISTRY_SLOTS);
        slots.resize_with(REGISTRY_SLOTS, Slot::default);
        Self {
            slots: slots.into_boxed_slice(),
        }
    }

    /// Starting slot for `name` - FNV-1a 32 over the bytes, mod slots.
    ///
    /// FNV-1a is cheap and well-distributed for short identifier strings,
    /// which is exactly the username shape we expect.
    fn start_slot(name: &str) -> usize {
        let mut h: u32 = 0x811c_9dc5;
        for &b in name.as_bytes() {
            h ^= b as u32;
            h = h.wrapping_mul(0x0100_0193);
        }
        (h as usize) & (REGISTRY_SLOTS - 1)
    }

    /// Resolve `name` to its token, registering it on first sight.
    ///
    /// Returns the 32-bit token. Allocates only when a brand-new user is
    /// registered (cold path). The hotpath (a known user) is allocation-free.
    ///
    /// The token of a NEW user is chosen so that [`Self::by_token`] can resolve
    /// it:
    /// - distinct from every token already in the table, because two names
    ///   sharing an FNV-1a value would otherwise resolve to whichever entry the
    ///   linear scan finds first - one writer's journal lines would be
    ///   attributed to another;
    /// - distinct from `RESERVED_UNATTRIBUTED`, the value `by_token` answers for
    ///   a token it cannot attribute.
    ///
    /// When the table is FULL the name cannot be registered, so the returned
    /// token is `RESERVED_UNATTRIBUTED` rather than a hash: a token that was
    /// never written to the table must not claim to name a user, and that is
    /// exactly what a bare FNV-1a value did (it resolved to whichever occupied
    /// entry happened to share the hash, or nothing at all).
    pub fn resolve(&mut self, name: &str) -> u32 {
        let start = Self::start_slot(name);
        let mut i = start;
        loop {
            match &self.slots[i].0 {
                Some(entry) if entry.name == name => return entry.token,
                Some(_) => {
                    // Occupied by another name -> probe next.
                    i = (i + 1) & (REGISTRY_SLOTS - 1);
                    if i == start {
                        // Table full - a pathological exhaustion. The user
                        // cannot be registered, so the journal records them as
                        // unattributed rather than borrowing another name.
                        return RESERVED_UNATTRIBUTED;
                    }
                }
                None => {
                    let token = self.free_token(name);
                    self.slots[i].0 = Some(UserMapEntry {
                        name: name.to_string(),
                        token,
                    });
                    return token;
                }
            }
        }
    }

    /// A token for `name` that no registered entry already answers to.
    ///
    /// Starts from the FNV-1a value and probes forward, so the common case
    /// (no collision) keeps the exact token earlier runs wrote to the journal.
    fn free_token(&self, name: &str) -> u32 {
        let mut token = fnv1a(name);
        // 1024 slots bound the table, so at most REGISTRY_SLOTS candidates can
        // be taken; the loop cannot reach its end without finding a free one.
        for _ in 0..=REGISTRY_SLOTS {
            if token == RESERVED_UNATTRIBUTED {
                token = token.wrapping_add(1);
                continue;
            }
            if self.slots.iter().all(|slot| match &slot.0 {
                Some(entry) => entry.token != token,
                None => true,
            }) {
                return token;
            }
            token = token.wrapping_add(1);
        }
        RESERVED_UNATTRIBUTED
    }

    /// Look up a registered entry by token (plaintext-log resolution).
    ///
    /// Returns a reference to the stored entry. Linear scan is O(n) over 1024
    /// slots - cold path only (log formatting), never in the QIDO/WADO/STOW
    /// hotpath.
    pub fn by_token(&self, token: u32) -> Option<&UserMapEntry> {
        self.slots.iter().find_map(|slot| match &slot.0 {
            Some(entry) if entry.token == token => Some(entry),
            _ => None,
        })
    }

    /// Number of registered users.
    pub fn len(&self) -> usize {
        self.slots.iter().filter(|s| s.0.is_some()).count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// FNV-1a 32-bit hash of `name` - both the slot function and the token.
fn fnv1a(name: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for &b in name.as_bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two usernames with the same FNV-1a 32 value.
    ///
    /// Searched rather than hard-coded: the pair depends on the hash, and a
    /// literal would silently stop being a collision if the function ever
    /// changed. The search space is small because 32-bit FNV-1a collides
    /// readily in a few thousand short strings.
    fn collision_pair() -> (&'static str, &'static str) {
        use std::collections::HashMap;
        let mut seen: HashMap<u32, String> = HashMap::new();
        // Plain decimal suffixes are rejected by the FNV-1a avalanche one
        // digit at a time, so the alphabet is mixed: a collision needs two
        // unrelated byte sequences, not two neighbours.
        const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789._-";
        for i in 0..4_000_000u32 {
            let mut n = i;
            let mut name = String::with_capacity(8);
            loop {
                name.push(ALPHABET[(n % ALPHABET.len() as u32) as usize] as char);
                n /= ALPHABET.len() as u32;
                if n == 0 {
                    break;
                }
            }
            let h = fnv1a(&name);
            if let Some(other) = seen.get(&h) {
                // Leaked deliberately: the test process is short-lived and the
                // pair has to outlive the map.
                return (
                    Box::leak(other.clone().into_boxed_str()),
                    Box::leak(name.into_boxed_str()),
                );
            }
            seen.insert(h, name);
        }
        panic!("no FNV-1a collision among 4M short names");
    }

    #[test]
    fn resolve_is_stable_for_same_name() {
        let mut reg = UserRegistry::new();
        let t1 = reg.resolve("dr.smith");
        let t2 = reg.resolve("dr.smith");
        assert_eq!(t1, t2);
    }

    #[test]
    fn distinct_names_get_distinct_tokens() {
        let mut reg = UserRegistry::new();
        let a = reg.resolve("dr.smith");
        let b = reg.resolve("ml.chen");
        assert_ne!(a, b);
    }

    #[test]
    fn resolving_many_users_fills_registry() {
        let mut reg = UserRegistry::new();
        for i in 0..500 {
            let name = format!("user{i}");
            reg.resolve(&name);
        }
        assert_eq!(reg.len(), 500);
    }

    #[test]
    fn by_token_roundtrips() {
        let mut reg = UserRegistry::new();
        let token = reg.resolve("dr.smith");
        let entry = reg.by_token(token).expect("entry");
        assert_eq!(entry.name, "dr.smith");
        assert_eq!(entry.token, token);
    }

    /// A token minted for a new user must resolve back to that user, even when
    /// another name hashes to the same FNV-1a value.
    ///
    /// Two names sharing a hash used to be given the SAME token, so
    /// `by_token` answered with whichever entry the linear scan reached first
    /// and one writer's journal lines were attributed to another.
    #[test]
    fn colliding_names_get_distinct_tokens_that_resolve_to_themselves() {
        // Found by brute force over suffixes; FNV-1a over short strings has
        // plenty of pairs.
        let (a, b) = collision_pair();
        assert_eq!(fnv1a(a), fnv1a(b), "the fixture must be a real collision");

        let mut reg = UserRegistry::new();
        let ta = reg.resolve(a);
        let tb = reg.resolve(b);
        assert_ne!(ta, tb, "colliding names must not share a token");
        assert_eq!(reg.by_token(ta).map(|e| e.name.as_str()), Some(a));
        assert_eq!(reg.by_token(tb).map(|e| e.name.as_str()), Some(b));
    }

    /// A full table cannot register a new user: the token must say "not
    /// attributed" rather than claim a user the table does not hold.
    #[test]
    fn a_full_table_reports_an_unattributed_token() {
        let mut reg = UserRegistry::new();
        for i in 0..REGISTRY_SLOTS {
            reg.resolve(&format!("user{i}"));
        }
        assert_eq!(reg.len(), REGISTRY_SLOTS, "the table is full");

        let token = reg.resolve("one.user.too.many");
        assert_eq!(
            token, RESERVED_UNATTRIBUTED,
            "an unregisterable user is not attributed to anyone"
        );
        assert!(
            reg.by_token(token).is_none(),
            "and the reserved token names no user"
        );
    }

    /// The reserved token is never handed to a real user.
    #[test]
    fn no_registered_user_gets_the_reserved_token() {
        let mut reg = UserRegistry::new();
        for i in 0..64 {
            let token = reg.resolve(&format!("user{i}"));
            assert_ne!(token, RESERVED_UNATTRIBUTED);
        }
    }
}
