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
//!
//! Memory is bounded on two dimensions, not one: `capacity` caps how many
//! texts are remembered, and `max_spans_per_entry` caps how large one
//! remembered text's detection may be. Without the second, the first is not
//! actually a memory bound — a single text with enough spans can outweigh
//! thousands of ordinary ones. Declining an oversized detection is not a
//! third failure path with its own rule: it is the same "a miss is never a
//! refusal" property, since the caller already has the spans it asked for
//! and serves them exactly as it would if the cache did not exist.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use sha2::{Digest, Sha256};

use crate::mapping::Span;

/// A 32-byte digest. Not truncated: a collision on the text digest applies one
/// text's offsets to another, and at equal length `Mapping::mask` accepts them
/// — the wrong ranges are masked and a real value leaves the process. Sixteen
/// more bytes per key is not a trade worth making against that.
type Digest32 = [u8; 32];

#[derive(PartialEq, Eq, Hash)]
struct Key {
    version: Digest32,
    tenant: Digest32,
    text: Digest32,
}

struct Entry {
    spans: Vec<Span>,
    /// Monotonic, from the store's own counter rather than a clock: eviction
    /// needs an order, not a time, and a counter cannot go backwards.
    used: u64,
}

struct Inner {
    entries: HashMap<Key, Entry>,
    clock: u64,
    /// The last version a detector reported. `get` has no other way to know
    /// which version to look under, because the version only ever arrives with
    /// a response.
    known_version: Option<Digest32>,
}

pub struct DetectionCache {
    capacity: usize,
    /// The other dimension `capacity` does not bound: how many spans one
    /// remembered detection may carry. `capacity` bounds how many texts the
    /// cache remembers; this bounds how large one remembered text's
    /// detection may be, the same relationship `SessionStore` has between
    /// `max_sessions` and `max_session_values`. A detection over this cap is
    /// declined rather than stored — the caller already has its spans from
    /// this call and serves them exactly as it would anyway; only whether
    /// they are remembered for next time is affected.
    max_spans_per_entry: usize,
    /// Minted per process and never persisted. The cache must not survive a
    /// restart, so its keys never need to be comparable across runs — and a
    /// per-process salt keeps them from becoming a second stable identifier
    /// for a tenant beside the journal's. Deliberately not the audit salt,
    /// which is on disk precisely so its digests do persist.
    salt: [u8; 32],
    inner: Mutex<Inner>,
    /// Set the first time a poisoned lock is observed, so a run of requests
    /// against a permanently degraded cache logs the operator's signal once
    /// rather than once per request: without this, a poisoned cache would
    /// fail silently forever, and an operator would see only a throughput
    /// collapse with nothing in the logs to explain it.
    poisoned_warned: AtomicBool,
}

impl DetectionCache {
    pub fn new(capacity: usize, max_spans_per_entry: usize) -> Self {
        let mut salt = [0u8; 32];
        getrandom::getrandom(&mut salt).expect("the OS provides randomness");
        Self {
            capacity,
            max_spans_per_entry,
            salt,
            inner: Mutex::new(Inner {
                entries: HashMap::new(),
                clock: 0,
                known_version: None,
            }),
            poisoned_warned: AtomicBool::new(false),
        }
    }

    fn digest(&self, bytes: &[u8]) -> Digest32 {
        let mut hasher = Sha256::new();
        hasher.update(self.salt);
        hasher.update(bytes);
        hasher.finalize().into()
    }

    /// The `warn!` a poisoned lock earns, logged at most once per process.
    /// `swap` rather than `load`-then-`store`: two requests racing to be the
    /// first to notice the poisoning would otherwise both see `false` and
    /// both log, which is exactly the spam this guards against.
    fn warn_poisoned(&self) {
        if !self.poisoned_warned.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                "the detection cache's lock is poisoned; caching is disabled for the rest of \
                 this process and every request now calls the detector directly"
            );
        }
    }

    #[cfg(test)]
    fn key_for(&self, version: &str, credential: Option<&[u8]>, text: &str) -> Digest32 {
        let key = self.key(self.digest(version.as_bytes()), credential, text);
        self.digest(&[key.version, key.tenant, key.text].concat())
    }

    // A separate method rather than a block inside `get`, because a function
    // body is a scope no later edit can accidentally widen: the guard this
    // acquires is dropped when this call returns, full stop, with no local
    // in the caller for a future edit to hold onto. `get` cannot lock
    // `self.inner` again while still holding a guard from this call —
    // `Mutex` is not reentrant, and doing so would deadlock the very next
    // acquisition it makes.
    fn known_version(&self) -> Option<Digest32> {
        match self.inner.lock() {
            Ok(inner) => inner.known_version,
            Err(_) => {
                self.warn_poisoned();
                None
            }
        }
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
        // text — in the one feature whose entire purpose is throughput.
        //
        // A version bump can land in the gap between the two acquisitions
        // below. It could never resurrect someone else's spans — a `Key`
        // matches on all three digests, so the key built from the stale
        // `version` still only matches this exact text under that stale
        // version — but serving it anyway would answer from a version the
        // store has already moved past. Closed below, at the cost of one
        // comparison the second lock was already going to pay for: recheck
        // `known_version` under that lock and treat a mismatch as a miss.
        //
        // `known_version()` returns rather than lending its guard, so there
        // is no guard here for this method to hold across the hashing below.
        let version = self.known_version()?;
        self.get_at_version(version, credential, text)
    }

    // Split out of `get` so a test can drive the race described above it
    // explicitly: capture a version, let a concurrent `insert` move
    // `known_version` past it, then call this directly with the
    // now-stale version. That reaches the exact interleaving a real race
    // would produce without racing actual threads and hoping the
    // scheduler cooperates.
    fn get_at_version(
        &self,
        version: Digest32,
        credential: Option<&[u8]>,
        text: &str,
    ) -> Option<Vec<Span>> {
        let key = self.key(version, credential, text);
        // A second poisoning, in the window between the two acquisitions
        // above, is defence in depth rather than a proven path: no test
        // reaches it, since a single thread cannot poison its own lock
        // between two of its own acquisitions, and reaching it in
        // production needs another thread to panic inside that same
        // window.
        let mut inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(_) => {
                self.warn_poisoned();
                return None;
            }
        };
        // The version this key was built from may already be stale — see
        // above. A concurrent insert for a newer version between the two
        // acquisitions would otherwise let this lookup serve a version the
        // store has already left behind.
        if inner.known_version != Some(version) {
            return None;
        }
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
        // Declined, not refused: the caller already has `spans` from this
        // call and serves them exactly as it would if the cache did not
        // exist. A single oversized detection is the failure mode
        // `capacity` alone cannot prevent — it bounds entry count, not the
        // size of one entry, and a 20 KB tool result can carry hundreds of
        // spans in a single text.
        if spans.len() > self.max_spans_per_entry {
            return;
        }
        let version = self.digest(version.as_bytes());
        let key = self.key(version, credential, text);
        let Ok(mut inner) = self.inner.lock() else {
            self.warn_poisoned();
            return;
        };
        // A version that differs from the one currently known is swept, not
        // just left to stop matching. Digests carry no order, so a
        // differing version cannot be assumed newer: a slow response for an
        // older version can land after a faster one for a newer version —
        // real with cacheable misses straddling a rolling upgrade, or with
        // mixed-version replicas behind one gateway, both real deployments
        // (see the version-honesty fix this cache already carries). Without
        // the sweep, that late arrival would reassign `known_version` back
        // to the older value, and every entry still sitting under it —
        // never removed, only waiting to stop matching — would become
        // reachable again through `get`'s own version recheck, which trusts
        // `known_version` to mean "current" and has no way to know it just
        // went backwards. Served that way, the answer is missing whatever
        // the newer version's run added: a masking gap, not merely stale
        // data.
        //
        // So every entry not stamped with the version about to become
        // current is dropped right here, before it is adopted — an entry
        // present in the map, at any instant, is now stamped with
        // `known_version` by construction, whichever direction the version
        // moved. In a fleet that is permanently mixed rather than mid-
        // rollover, this costs the hit rate: every insert sees a different
        // version than the last and sweeps the map on every call, thrashing
        // toward zero. That is the correct trade for this cache, the same
        // one this gateway makes everywhere else — slow and right over fast
        // and wrong — and it costs nothing extra on the common path, where
        // the version matches and the sweep never runs.
        if inner.known_version != Some(version) {
            inner.entries.retain(|stored, _| stored.version == version);
            inner.known_version = Some(version);
        }
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
    /// The span cap tests that are not about the cap itself pass this, so a
    /// handful of `PERSON` spans never accidentally brushes against it.
    const UNCAPPED: usize = usize::MAX;

    #[test]
    fn a_stored_result_comes_back() {
        let cache = DetectionCache::new(4, UNCAPPED);
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
        let cache = DetectionCache::new(4, UNCAPPED);
        assert!(cache.get(A, "Weber").is_none());
    }

    #[test]
    fn another_credential_does_not_see_the_entry() {
        // Not because the spans would leak — B already has the text it sent —
        // but because the response time would say that A sent it first.
        let cache = DetectionCache::new(4, UNCAPPED);
        cache.insert("v1", A, "Weber", &[span("PERSON", 0, 5)]);
        assert!(cache.get(B, "Weber").is_none());
    }

    #[test]
    fn a_new_version_hides_everything_stored_under_the_old_one() {
        let cache = DetectionCache::new(4, UNCAPPED);
        cache.insert("v1", A, "Weber", &[span("PERSON", 0, 5)]);
        cache.insert("v2", A, "Schmidt", &[span("PERSON", 0, 7)]);
        assert!(cache.get(A, "Weber").is_none());
        assert!(cache.get(A, "Schmidt").is_some());
    }

    #[test]
    fn a_version_rollback_does_not_revive_a_different_texts_stale_entry() {
        // Digests carry no order, so a version rollback is not hypothetical:
        // a slow response for an older version can land after a faster one
        // for a newer version, whether from a cacheable miss straddling a
        // rolling upgrade or from mixed-version replicas behind one
        // gateway. "TextA" here stands for a text `insert` never touches
        // again after the rollback — the entry the old, unswept design
        // would have quietly left sitting under v1, waiting for
        // `known_version` to come back around, never re-verified against
        // whatever v2's run added for it.
        let cache = DetectionCache::new(4, UNCAPPED);
        cache.insert("v1", A, "TextA", &[span("PERSON", 0, 5)]);
        cache.insert("v2", A, "TextB", &[span("PERSON", 0, 5)]);
        assert!(
            cache.get(A, "TextA").is_none(),
            "TextA's v1 entry was reachable right after the move to v2"
        );

        // The late v1 response: a text v1 never scanned before now,
        // arriving only after the fleet has already moved on to v2.
        cache.insert("v1", A, "TextC", &[span("PERSON", 0, 5)]);

        assert!(
            cache.get(A, "TextA").is_none(),
            "a version rollback revived TextA's stale v1 entry, never re-verified under v2"
        );
    }

    #[test]
    fn a_version_that_moved_between_gets_two_locks_is_a_miss() {
        // Drives the interleaving `get`'s doc comment describes explicitly,
        // rather than racing threads and hoping the scheduler cooperates:
        // capture the version exactly as `get` would before its second
        // lock, then let a concurrent insert move `known_version` past it
        // — simulated here by simply calling `insert` in between, since
        // this test *is* the thread that would otherwise be racing `get`.
        let cache = DetectionCache::new(4, UNCAPPED);
        cache.insert("v1", A, "Weber", &[span("PERSON", 0, 5)]);
        let stale_version = cache.known_version().expect("insert set it");

        cache.insert("v2", A, "Meier", &[span("PERSON", 0, 5)]);
        assert_ne!(
            cache.known_version(),
            Some(stale_version),
            "the second insert did not actually move the version"
        );

        // `get_at_version` is exactly the second half of `get` — the half
        // that runs after the version above would have been captured. A
        // real race lands here with `stale_version` in hand and the store
        // already past it.
        assert!(
            cache.get_at_version(stale_version, A, "Weber").is_none(),
            "a lookup built from a version the store had already moved past was served anyway"
        );
    }

    #[test]
    fn saturation_evicts_the_least_recently_used() {
        let cache = DetectionCache::new(2, UNCAPPED);
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
        let cache = DetectionCache::new(0, UNCAPPED);
        cache.insert("v1", A, "Weber", &[span("PERSON", 0, 5)]);
        assert_eq!(cache.len(), 0);
        assert!(cache.get(A, "Weber").is_none());
    }

    #[test]
    fn a_detection_over_the_span_cap_is_declined() {
        let cache = DetectionCache::new(4, 2);
        cache.insert(
            "v1",
            A,
            "Weber Meier Schmidt",
            &[
                span("PERSON", 0, 5),
                span("PERSON", 6, 11),
                span("PERSON", 12, 19),
            ],
        );
        assert!(
            cache.get(A, "Weber Meier Schmidt").is_none(),
            "a detection over the span cap was stored anyway"
        );
        assert_eq!(cache.len(), 0, "an oversized detection took a slot");
    }

    #[test]
    fn a_detection_at_the_span_cap_is_stored() {
        // The boundary: strictly over the cap is declined, so exactly at it
        // must still be an ordinary hit — the cap is not off by one in
        // either direction.
        let cache = DetectionCache::new(4, 2);
        cache.insert(
            "v1",
            A,
            "Weber Meier",
            &[span("PERSON", 0, 5), span("PERSON", 6, 11)],
        );
        assert!(cache.get(A, "Weber Meier").is_some());
    }

    #[test]
    fn two_gateways_do_not_agree_on_a_key() {
        // The salt is per process and never persisted, so a digest here names
        // nothing anywhere else — including in a second gateway's memory.
        let first = DetectionCache::new(4, UNCAPPED);
        let second = DetectionCache::new(4, UNCAPPED);
        first.insert("v1", A, "Weber", &[span("PERSON", 0, 5)]);
        second.insert("v1", A, "Weber", &[span("PERSON", 0, 5)]);
        assert_ne!(
            first.key_for("v1", A, "Weber"),
            second.key_for("v1", A, "Weber")
        );
    }

    #[test]
    fn a_request_without_a_credential_is_its_own_bucket() {
        let cache = DetectionCache::new(4, UNCAPPED);
        cache.insert("v1", None, "Weber", &[span("PERSON", 0, 5)]);
        assert!(cache.get(None, "Weber").is_some());
        assert!(cache.get(A, "Weber").is_none());
    }

    #[test]
    fn a_poisoned_lock_answers_a_miss_rather_than_a_panic() {
        let cache = DetectionCache::new(4, UNCAPPED);
        cache.insert("v1", A, "Weber", &[span("PERSON", 0, 5)]);
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = cache.inner.lock().expect("not yet poisoned");
            panic!("a request died mid-update");
        }));
        assert!(panicked.is_err(), "the panic is what poisons the lock");
        assert!(
            !cache.poisoned_warned.load(Ordering::Relaxed),
            "warned about a poisoning nothing has observed yet"
        );
        assert!(cache.get(A, "Weber").is_none(), "a poisoned lock must miss");
        assert!(
            cache.poisoned_warned.load(Ordering::Relaxed),
            "an operator gets no signal that the cache degraded"
        );
        cache.insert("v1", A, "Meier", &[span("PERSON", 0, 5)]); // must not panic
    }

    #[test]
    fn a_poisoning_is_warned_about_once_not_once_per_request() {
        // Not a race: `swap` returns the *old* value, and only the call that
        // observes `false` there ever logs — every later call observes
        // `true` and skips unconditionally, whatever order concurrent
        // requests actually run in. This is the guarantee the field exists
        // for, checked directly rather than through the `tracing` output the
        // real call site produces.
        let cache = DetectionCache::new(4, UNCAPPED);
        assert!(!cache.poisoned_warned.swap(true, Ordering::Relaxed));
        assert!(cache.poisoned_warned.swap(true, Ordering::Relaxed));
        assert!(cache.poisoned_warned.swap(true, Ordering::Relaxed));
    }

    #[test]
    fn reinserting_a_key_the_store_already_holds_evicts_nothing() {
        // Capacity 3, not 2: at capacity 2 the least-recently-used entry
        // would be the one being re-inserted, and this mutation would stay
        // invisible — the eviction it would wrongly trigger and the
        // overwrite that follows would remove the same key either way.
        let cache = DetectionCache::new(3, UNCAPPED);
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
        let cache = DetectionCache::new(4, UNCAPPED);
        cache.insert("v1", A, "first", &[span("PERSON", 0, 5)]);
        cache.insert("v1", A, "second", &[span("PERSON", 0, 6)]);
        let inner = cache.inner.lock().expect("not poisoned");
        let used: Vec<u64> = inner.entries.values().map(|entry| entry.used).collect();
        assert_ne!(used[0], used[1], "eviction would break the tie arbitrarily");
    }
}
