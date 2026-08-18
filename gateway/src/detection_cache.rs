//! Detection results, remembered by digest.
//!
//! Detection runs over every text on every request, and a client resends the
//! whole conversation each turn, so the cost of a conversation grows with the
//! square of its length. Nothing about the rescan is useful: history does not
//! change, and the spans were already computed.
//!
//! Two properties make this safe to add to a gateway whose argument is that
//! personal data lives in exactly one place. No key here holds submitted text
//! — keys are digests — and a value holds only a type name and two offsets,
//! with the same caveat `audit.rs` records about type names: `Span.entity_type`
//! is an unrestricted string on the wire, and a detector that echoed found
//! text into it would put that text here too. And a miss is never a refusal:
//! every failure path below degrades to "call the detector", because losing
//! an entry costs time, not correctness. That is the opposite of
//! `SessionStore`, where losing an entry is a confidentiality problem and
//! saturation therefore refuses.

use std::collections::HashMap;
use std::sync::Mutex;

use sha2::{Digest, Sha256};

use crate::mapping::Span;

/// A 32-byte digest. Not truncated: a collision on the text digest applies one
/// text's offsets to another, and at equal length `Mapping::mask` accepts them
/// — the wrong ranges are masked and a real value leaves the process. Sixteen
/// more bytes per key is not a trade worth making against that.
// Unread outside tests until the cache (a later task) wires `DetectionCache`
// into `DetectorClient`.
#[allow(dead_code)]
type Digest32 = [u8; 32];

#[derive(PartialEq, Eq, Hash)]
#[allow(dead_code)]
struct Key {
    version: Digest32,
    tenant: Digest32,
    text: Digest32,
}

#[allow(dead_code)]
struct Entry {
    spans: Vec<Span>,
    /// Monotonic, from the store's own counter rather than a clock: eviction
    /// needs an order, not a time, and a counter cannot go backwards.
    used: u64,
}

#[allow(dead_code)]
struct Inner {
    entries: HashMap<Key, Entry>,
    clock: u64,
    /// The last version a detector reported. `get` has no other way to know
    /// which version to look under, because the version only ever arrives with
    /// a response.
    known_version: Option<Digest32>,
}

#[allow(dead_code)]
pub struct DetectionCache {
    capacity: usize,
    /// Minted per process and never persisted. The cache must not survive a
    /// restart, so its keys never need to be comparable across runs — and a
    /// per-process salt keeps them from becoming a second stable identifier
    /// for a tenant beside the journal's. Deliberately not the audit salt,
    /// which is on disk precisely so its digests do persist.
    salt: [u8; 32],
    inner: Mutex<Inner>,
}

#[allow(dead_code)]
impl DetectionCache {
    pub fn new(capacity: usize) -> Self {
        let mut salt = [0u8; 32];
        getrandom::getrandom(&mut salt).expect("the OS provides randomness");
        Self {
            capacity,
            salt,
            inner: Mutex::new(Inner {
                entries: HashMap::new(),
                clock: 0,
                known_version: None,
            }),
        }
    }

    fn digest(&self, bytes: &[u8]) -> Digest32 {
        let mut hasher = Sha256::new();
        hasher.update(self.salt);
        hasher.update(bytes);
        hasher.finalize().into()
    }

    #[cfg(test)]
    fn key_for(&self, version: &str, credential: Option<&[u8]>, text: &str) -> Digest32 {
        let key = self.key(self.digest(version.as_bytes()), credential, text);
        self.digest(&[key.version, key.tenant, key.text].concat())
    }

    fn key(&self, version: Digest32, credential: Option<&[u8]>, text: &str) -> Key {
        Key {
            version,
            // A request with no credential is its own bucket rather than
            // everyone's: an empty digest is a tenant like any other. That
            // makes `Some(b"")` and `None` collide here — unreachable today
            // only because `session::credential_of` filters an empty header
            // value before either ever reaches this file, a dependency this
            // module otherwise does not record.
            tenant: self.digest(credential.unwrap_or(b"")),
            text: self.digest(text.as_bytes()),
        }
    }

    pub fn get(&self, credential: Option<&[u8]>, text: &str) -> Option<Vec<Span>> {
        if self.capacity == 0 {
            // Defence in depth, not the only thing keeping a disabled cache
            // from answering: `insert`'s own guard below already keeps
            // `known_version` at `None` forever when capacity is zero, so
            // `entries` stays empty and a lookup would miss even without
            // this check.
            return None;
        }
        // A poisoned lock means some other request panicked mid-update. That is
        // worth neither failing this request nor propagating: answer "miss".
        //
        // The two digests below run outside the lock, unlike the rest of this
        // method: `insert` can hash before it ever locks, but `get` needs
        // `known_version` first, and holding the lock across a hash whose
        // cost is proportional to text length would serialize every lookup
        // in the process behind whichever request is hashing the longest
        // text — in the one feature whose entire purpose is throughput. A
        // version bump between the two acquisitions below does not make the
        // key built from the first *wrong* — a `Key` matches only on all
        // three digests, so a stale key can at worst produce a stale but
        // exact hit for this same text, never someone else's.
        //
        // Scoped so the guard is dropped before the hashing below. A
        // `let`-bound guard here would live to the end of the method, and
        // the second acquisition would deadlock against it — `Mutex` is not
        // reentrant.
        let version = {
            let inner = self.inner.lock().ok()?;
            inner.known_version?
        };
        let key = self.key(version, credential, text);
        // A second poisoning, in the window between the two acquisitions
        // above, is defence in depth rather than a proven path: no test
        // reaches it, since a single thread cannot poison its own lock
        // between two of its own acquisitions, and reaching it in
        // production needs another thread to panic inside that same
        // window.
        let mut inner = self.inner.lock().ok()?;
        inner.clock += 1;
        let clock = inner.clock;
        let entry = inner.entries.get_mut(&key)?;
        entry.used = clock;
        Some(entry.spans.clone())
    }

    pub fn insert(&self, version: &str, credential: Option<&[u8]>, text: &str, spans: &[Span]) {
        if self.capacity == 0 {
            return;
        }
        let version = self.digest(version.as_bytes());
        let key = self.key(version, credential, text);
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        // A version the store has not seen before makes every entry under the
        // old one unreachable. They are not swept: they simply stop matching
        // and age out through the ceiling, which costs nothing on the path a
        // request is waiting on.
        inner.known_version = Some(version);
        inner.clock += 1;
        let clock = inner.clock;
        if inner.entries.len() >= self.capacity && !inner.entries.contains_key(&key) {
            // One ordered pass, the same shape the session store's eviction
            // scan uses, though ten times the length: this store's default
            // ceiling is 10,000 entries against the session store's 1,000.
            // It runs only when full, and at that size a linear scan still
            // costs tens of microseconds, not the single microsecond a
            // smaller structure might suggest.
            if let Some(oldest) = inner
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| Key {
                    version: key.version,
                    tenant: key.tenant,
                    text: key.text,
                })
            {
                inner.entries.remove(&oldest);
            }
        }
        inner.entries.insert(
            key,
            Entry {
                spans: spans.to_vec(),
                used: clock,
            },
        );
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner
            .lock()
            .map(|inner| inner.entries.len())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(entity_type: &str, start: usize, end: usize) -> Span {
        Span {
            entity_type: entity_type.to_owned(),
            start,
            end,
        }
    }

    const A: Option<&[u8]> = Some(b"Bearer a");
    const B: Option<&[u8]> = Some(b"Bearer b");

    #[test]
    fn a_stored_result_comes_back() {
        let cache = DetectionCache::new(4);
        cache.insert("v1", A, "Weber", &[span("PERSON", 0, 5)]);
        let found = cache
            .get(A, "Weber")
            .expect("stored under the known version");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].entity_type, "PERSON");
    }

    #[test]
    fn a_cold_cache_misses_rather_than_guessing_a_version() {
        // This cannot fail through the version guard alone: `insert` is the
        // only writer of `entries` and sets `known_version` first, so a cold
        // store misses on the empty map whatever key `get` computes. What it
        // does pin is the coldest point of the miss-is-never-a-refusal rule —
        // a `get` before any response answers `None` rather than panicking
        // or inventing spans.
        let cache = DetectionCache::new(4);
        assert!(cache.get(A, "Weber").is_none());
    }

    #[test]
    fn another_credential_does_not_see_the_entry() {
        // Not because the spans would leak — B already has the text it sent —
        // but because the response time would say that A sent it first.
        let cache = DetectionCache::new(4);
        cache.insert("v1", A, "Weber", &[span("PERSON", 0, 5)]);
        assert!(cache.get(B, "Weber").is_none());
    }

    #[test]
    fn a_new_version_hides_everything_stored_under_the_old_one() {
        let cache = DetectionCache::new(4);
        cache.insert("v1", A, "Weber", &[span("PERSON", 0, 5)]);
        cache.insert("v2", A, "Schmidt", &[span("PERSON", 0, 7)]);
        assert!(cache.get(A, "Weber").is_none());
        assert!(cache.get(A, "Schmidt").is_some());
    }

    #[test]
    fn saturation_evicts_the_least_recently_used() {
        let cache = DetectionCache::new(2);
        cache.insert("v1", A, "first", &[span("PERSON", 0, 5)]);
        cache.insert("v1", A, "second", &[span("PERSON", 0, 6)]);
        // Touching "first" makes "second" the oldest.
        assert!(cache.get(A, "first").is_some());
        cache.insert("v1", A, "third", &[span("PERSON", 0, 5)]);
        assert_eq!(cache.len(), 2);
        assert!(cache.get(A, "first").is_some());
        assert!(cache.get(A, "second").is_none());
        assert!(cache.get(A, "third").is_some());
    }

    #[test]
    fn a_disabled_cache_stores_nothing_and_answers_nothing() {
        let cache = DetectionCache::new(0);
        cache.insert("v1", A, "Weber", &[span("PERSON", 0, 5)]);
        assert_eq!(cache.len(), 0);
        assert!(cache.get(A, "Weber").is_none());
    }

    #[test]
    fn two_gateways_do_not_agree_on_a_key() {
        // The salt is per process and never persisted, so a digest here names
        // nothing anywhere else — including in a second gateway's memory.
        let first = DetectionCache::new(4);
        let second = DetectionCache::new(4);
        first.insert("v1", A, "Weber", &[span("PERSON", 0, 5)]);
        second.insert("v1", A, "Weber", &[span("PERSON", 0, 5)]);
        assert_ne!(
            first.key_for("v1", A, "Weber"),
            second.key_for("v1", A, "Weber")
        );
    }

    #[test]
    fn a_request_without_a_credential_is_its_own_bucket() {
        let cache = DetectionCache::new(4);
        cache.insert("v1", None, "Weber", &[span("PERSON", 0, 5)]);
        assert!(cache.get(None, "Weber").is_some());
        assert!(cache.get(A, "Weber").is_none());
    }

    #[test]
    fn a_poisoned_lock_answers_a_miss_rather_than_a_panic() {
        let cache = DetectionCache::new(4);
        cache.insert("v1", A, "Weber", &[span("PERSON", 0, 5)]);
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = cache.inner.lock().expect("not yet poisoned");
            panic!("a request died mid-update");
        }));
        assert!(panicked.is_err(), "the panic is what poisons the lock");
        assert!(cache.get(A, "Weber").is_none(), "a poisoned lock must miss");
        cache.insert("v1", A, "Meier", &[span("PERSON", 0, 5)]); // must not panic
    }

    #[test]
    fn reinserting_a_key_the_store_already_holds_evicts_nothing() {
        // Capacity 3, not 2: at capacity 2 the least-recently-used entry
        // would be the one being re-inserted, and this mutation would stay
        // invisible — the eviction it would wrongly trigger and the
        // overwrite that follows would remove the same key either way.
        let cache = DetectionCache::new(3);
        cache.insert("v1", A, "first", &[span("PERSON", 0, 5)]);
        cache.insert("v1", A, "second", &[span("PERSON", 0, 6)]);
        cache.insert("v1", A, "third", &[span("PERSON", 0, 5)]);
        cache.insert("v1", A, "third", &[span("PERSON", 0, 5)]);
        assert_eq!(cache.len(), 3);
        assert!(
            cache.get(A, "first").is_some(),
            "an unrelated entry was taken"
        );
    }

    #[test]
    fn two_entries_stored_in_a_row_do_not_share_a_recency() {
        let cache = DetectionCache::new(4);
        cache.insert("v1", A, "first", &[span("PERSON", 0, 5)]);
        cache.insert("v1", A, "second", &[span("PERSON", 0, 6)]);
        let inner = cache.inner.lock().expect("not poisoned");
        let used: Vec<u64> = inner.entries.values().map(|entry| entry.used).collect();
        assert_ne!(used[0], used[1], "eviction would break the tie arbitrarily");
    }
}
