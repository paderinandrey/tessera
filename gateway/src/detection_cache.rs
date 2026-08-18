//! Detection results, remembered by digest.
//!
//! Detection runs over every text on every request, and a client resends the
//! whole conversation each turn, so the cost of a conversation grows with the
//! square of its length. Nothing about the rescan is useful: history does not
//! change, and the spans were already computed.
//!
//! Two properties make this safe to add to a gateway whose argument is that
//! personal data lives in exactly one place. Nothing here holds submitted text
//! — keys are digests, values are spans, and a span is a type and two offsets.
//! And a miss is never a refusal: every failure path below degrades to "call
//! the detector", because losing an entry costs time, not correctness. That is
//! the opposite of `SessionStore`, where losing an entry is a confidentiality
//! problem and saturation therefore refuses.

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
            // everyone's: an empty digest is a tenant like any other.
            tenant: self.digest(credential.unwrap_or(b"")),
            text: self.digest(text.as_bytes()),
        }
    }

    pub fn get(&self, credential: Option<&[u8]>, text: &str) -> Option<Vec<Span>> {
        if self.capacity == 0 {
            return None;
        }
        // A poisoned lock means some other request panicked mid-update. That is
        // worth neither failing this request nor propagating: answer "miss".
        let mut inner = self.inner.lock().ok()?;
        let version = inner.known_version?;
        let key = self.key(version, credential, text);
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
            // scan uses. It runs only when full, and at the default ceiling it
            // is microseconds.
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
    fn nothing_is_known_before_the_first_insert() {
        // The version is only ever learned from a response, so a cold cache
        // has no version to look under and must miss.
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
}
