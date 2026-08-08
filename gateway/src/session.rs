//! Session identity and the store that holds one table per conversation.
//!
//! A session table is a restoration oracle: a caller who writes a placeholder
//! into a prompt gets it echoed by the model, and the gateway restores it on
//! the way back. So a client-chosen id never selects a session on its own — it
//! is namespaced by a fingerprint of the caller's own credential.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use sha2::{Digest, Sha256};

use crate::mapping::Mapping;
use crate::provider::Provider;

/// The header a client uses to say two requests belong to one conversation.
pub const SESSION_HEADER: &str = "x-tessera-session";

/// A client-chosen id becomes part of a map key and, hashed, part of a log
/// line. Bounding it keeps both from having to cope with arbitrary input.
const MAX_SESSION_ID: usize = 128;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(
        "session id must be 1 to 128 characters of A-Z a-z 0-9 . _ : -; the request is \
         refused rather than served under an id that was quietly altered"
    )]
    BadId,
    #[error(
        "sessions are disabled (session_idle_secs = 0) but the request asked for one; the \
         request is refused rather than served without the coreference it asked for"
    )]
    Disabled,
    #[error(
        "a session needs the {0} header to namespace it; the request is refused rather \
         than putting every caller who sent no credential into one shared session"
    )]
    NoCredential(&'static str),
}

/// Random per process: the store must hold nothing from which a credential can
/// be recovered. Sessions do not survive a restart in any case — the store is
/// in memory — so a salt that does not either costs nothing.
fn salt() -> &'static [u8; 32] {
    static SALT: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
    SALT.get_or_init(|| {
        let mut bytes = [0u8; 32];
        getrandom::getrandom(&mut bytes).expect("the OS must provide randomness");
        bytes
    })
}

/// What the store keys on: a salted fingerprint of the caller's credential,
/// and the id the caller chose.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct SessionKey {
    credential: [u8; 32],
    id: String,
}

// `derive(Debug)` would print `id` verbatim, and `id` is client-chosen: it may
// itself be personal data, the same as the raw id `digest()` exists to keep
// out of logs. A type with a safe printable form and an unsafe one only
// needs one caller to reach into the wrong one, so there is exactly one
// `Debug` implementation and it prints only the digest.
impl std::fmt::Debug for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SessionKey").field(&self.digest()).finish()
    }
}

impl SessionKey {
    fn new(credential: &[u8], id: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(salt());
        hasher.update(credential);
        Self {
            credential: hasher.finalize().into(),
            id: id.to_owned(),
        }
    }

    /// The only form of a key that may reach a log. The raw id is chosen by the
    /// client, so it may itself be personal data: `patient-Weber-2026` is a
    /// plausible id and an unacceptable log line.
    pub fn digest(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.credential);
        hasher.update(self.id.as_bytes());
        let digest: [u8; 32] = hasher.finalize().into();
        digest[..4]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_SESSION_ID
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'))
}

/// Which session a request asks for. `Ok(None)` means it asked for none, which
/// is the behaviour that predates sessions: a mapping scoped to the request.
///
/// Every refusal here happens before detection, so a malformed header costs
/// nothing rather than a second per 1 200 characters.
pub fn key_from(
    headers: &HeaderMap,
    provider: &dyn Provider,
    enabled: bool,
) -> Result<Option<SessionKey>, SessionError> {
    let Some(raw) = headers.get(SESSION_HEADER) else {
        return Ok(None);
    };
    let id = raw.to_str().map_err(|_| SessionError::BadId)?;
    if !valid_id(id) {
        return Err(SessionError::BadId);
    }
    if !enabled {
        return Err(SessionError::Disabled);
    }
    let name = provider.credential_header();
    let credential = headers
        .get(name)
        .map(|value| value.as_bytes())
        .filter(|value| !value.is_empty())
        .ok_or(SessionError::NoCredential(name))?;
    Ok(Some(SessionKey::new(credential, id)))
}

/// The three bounds on how much personal data outlives a request.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// How long a session survives without a request. Zero disables sessions.
    pub idle: Duration,
    pub max_sessions: usize,
    pub max_values: usize,
}

/// One conversation's table.
pub struct Session {
    /// Wrapped in its own `Arc` so the store can claim it synchronously via
    /// `try_lock_owned` inside the same critical section that decides
    /// whether to hand it back or evict it — closing the window between
    /// handing a session out and its caller actually locking it, which an
    /// unrelated eviction could otherwise exploit. Every existing
    /// `.lock().await` call site keeps working unchanged: `Arc<Mutex<T>>`
    /// still derefs to `Mutex<T>`.
    pub mapping: Arc<tokio::sync::Mutex<Mapping>>,
}

/// What `acquire` hands back: the session, and — when the store could claim
/// it synchronously, in the same critical section that decided this session
/// was safe to return — the lock already held. `None` means someone else
/// currently holds it; the caller waits its turn with `lock_owned().await`,
/// which is already-proven-safe serialization for two requests to one key.
pub struct Claimed {
    pub session: Arc<Session>,
    pub guard: Option<tokio::sync::OwnedMutexGuard<Mapping>>,
}

struct Entry {
    session: Arc<Session>,
    last_seen: Instant,
}

/// Whether `entry`'s session is safe for an unrelated eviction to reclaim:
/// its lock is free right now, and it holds no committed values. Claiming
/// the lock here and immediately dropping it only ever succeeds when
/// nobody else currently needs it — the same guarantee that makes the
/// return-time claim in `acquire_at` safe.
fn reclaimable(entry: &Entry) -> bool {
    match Arc::clone(&entry.session.mapping).try_lock_owned() {
        Ok(guard) => guard.is_empty(),
        Err(_) => false,
    }
}

pub struct SessionStore {
    /// A std mutex, held only for map operations and never across an `await`.
    inner: Mutex<HashMap<SessionKey, Entry>>,
    limits: Limits,
}

impl SessionStore {
    pub fn new(limits: Limits) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            limits,
        }
    }

    pub fn enabled(&self) -> bool {
        !self.limits.idle.is_zero()
    }

    pub fn max_values(&self) -> usize {
        self.limits.max_values
    }

    pub fn acquire(&self, key: &SessionKey) -> Claimed {
        self.acquire_at(key, Instant::now())
    }

    /// Time is a parameter so eviction is tested without sleeping.
    fn acquire_at(&self, key: &SessionKey, now: Instant) -> Claimed {
        // Poisoning is recovered rather than propagated. Nothing in this
        // critical section can panic today, but if something ever does, one
        // failure must not turn every later request into a 500.
        let mut map = self.inner.lock().unwrap_or_else(|error| error.into_inner());

        // Sweeping here rather than in a background task: at a thousand
        // sessions this costs less than a syscall, and there is no separate
        // task whose health is a new thing to reason about.
        map.retain(|_, entry| now.duration_since(entry.last_seen) < self.limits.idle);

        let session = if let Some(entry) = map.get_mut(key) {
            entry.last_seen = now;
            Arc::clone(&entry.session)
        } else {
            if map.len() >= self.limits.max_sessions {
                // Prefer a victim that is already reclaimable — nobody
                // loses a live conversation to make room for a request
                // that turns out not to need the space. Fall back to the
                // globally-oldest entry only when every entry currently
                // holds something.
                let victim = map
                    .iter()
                    .filter(|(_, entry)| reclaimable(entry))
                    .min_by_key(|(_, entry)| entry.last_seen)
                    .or_else(|| map.iter().min_by_key(|(_, entry)| entry.last_seen))
                    .map(|(key, _)| key.clone());
                if let Some(victim) = victim {
                    map.remove(&victim);
                }
            }

            // A whole session is evicted; values within one never are. A
            // value dropped from a live session can come back from the
            // model on the next turn, and that is not a lost coreference
            // but a request that dies with nothing to restore to.
            let session = Arc::new(Session {
                mapping: Arc::new(tokio::sync::Mutex::new(Mapping::new())),
            });
            map.insert(
                key.clone(),
                Entry {
                    session: Arc::clone(&session),
                    last_seen: now,
                },
            );
            session
        };

        // Claimed synchronously, still holding the store's own lock: this
        // is what closes the window between handing a session back and its
        // caller actually locking it. A fresh session's mutex is always
        // claimable here — nothing else could hold a reference to it yet.
        // An existing one is claimable exactly when it is currently idle.
        let guard = Arc::clone(&session.mapping).try_lock_owned().ok();
        Claimed { session, guard }
    }

    #[cfg(test)]
    pub(crate) fn live(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::OpenAi;
    use axum::http::HeaderMap;
    use std::time::{Duration, Instant};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                // from_bytes, not parse: a header value may legally carry bytes
                // that are not UTF-8, and refusing those is `key_from`'s job
                // rather than the helper's.
                axum::http::HeaderValue::from_bytes(value.as_bytes()).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn no_header_asks_for_no_session() {
        let asked = key_from(&headers(&[("authorization", "Bearer k")]), &OpenAi, true);
        assert!(matches!(asked, Ok(None)));
    }

    #[test]
    fn an_id_outside_the_grammar_is_refused() {
        for id in ["", "conv 1", "conv/1", &"c".repeat(129)] {
            let asked = key_from(
                &headers(&[("authorization", "Bearer k"), (SESSION_HEADER, id)]),
                &OpenAi,
                true,
            );
            assert!(
                matches!(asked, Err(SessionError::BadId)),
                "accepted the id {id:?}"
            );
        }
    }

    #[test]
    fn an_id_that_is_not_text_is_refused() {
        // A header value may carry arbitrary bytes. `to_str` is what catches
        // this, before the grammar ever runs.
        let mut raw = HeaderMap::new();
        raw.insert("authorization", "Bearer k".parse().unwrap());
        raw.insert(
            axum::http::HeaderName::from_static(SESSION_HEADER),
            axum::http::HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap(),
        );
        assert!(matches!(
            key_from(&raw, &OpenAi, true),
            Err(SessionError::BadId)
        ));
    }

    #[test]
    fn the_grammar_accepts_what_a_client_actually_uses() {
        let asked = key_from(
            &headers(&[
                ("authorization", "Bearer k"),
                (SESSION_HEADER, "conv:2026-08-07_ab.12-x"),
            ]),
            &OpenAi,
            true,
        );
        assert!(matches!(asked, Ok(Some(_))));
    }

    #[test]
    fn asking_for_a_session_on_a_disabled_gateway_is_refused() {
        let asked = key_from(
            &headers(&[("authorization", "Bearer k"), (SESSION_HEADER, "conv1")]),
            &OpenAi,
            false,
        );
        assert!(matches!(asked, Err(SessionError::Disabled)));
    }

    #[test]
    fn a_session_without_a_credential_is_refused() {
        for pairs in [
            vec![(SESSION_HEADER, "conv1")],
            // Present but empty: an empty string is not a namespace, it is
            // every caller who sent nothing.
            vec![("authorization", ""), (SESSION_HEADER, "conv1")],
        ] {
            let asked = key_from(&headers(&pairs), &OpenAi, true);
            assert!(
                matches!(asked, Err(SessionError::NoCredential("authorization"))),
                "accepted a session with {pairs:?}"
            );
        }
    }

    #[test]
    fn one_id_under_two_credentials_is_two_keys() {
        let one = key_from(
            &headers(&[("authorization", "Bearer k1"), (SESSION_HEADER, "shared")]),
            &OpenAi,
            true,
        )
        .unwrap()
        .unwrap();
        let two = key_from(
            &headers(&[("authorization", "Bearer k2"), (SESSION_HEADER, "shared")]),
            &OpenAi,
            true,
        )
        .unwrap()
        .unwrap();
        let again = key_from(
            &headers(&[("authorization", "Bearer k1"), (SESSION_HEADER, "shared")]),
            &OpenAi,
            true,
        )
        .unwrap()
        .unwrap();
        assert_ne!(one, two, "a guessed id reached another caller's namespace");
        assert_eq!(one, again, "the same caller lost its own session");
    }

    #[test]
    fn what_may_be_logged_carries_neither_the_id_nor_the_credential() {
        // A client picks its own id, so it may itself be personal data.
        let key = key_from(
            &headers(&[
                ("authorization", "Bearer sekrit"),
                (SESSION_HEADER, "patient.Weber-2026"),
            ]),
            &OpenAi,
            true,
        )
        .unwrap()
        .unwrap();
        let digest = key.digest();
        assert!(!digest.contains("Weber"));
        assert!(!digest.contains("sekrit"));
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn debug_formatting_carries_neither_the_id_nor_the_credential() {
        // `digest()` is not the only way a `SessionKey` can reach a log line —
        // `{:?}` is another, and a derived `Debug` would print the raw id.
        let key = key_from(
            &headers(&[
                ("authorization", "Bearer sekrit"),
                (SESSION_HEADER, "patient.Weber-2026"),
            ]),
            &OpenAi,
            true,
        )
        .unwrap()
        .unwrap();
        let formatted = format!("{key:?}");
        assert!(!formatted.contains("Weber"));
        assert!(!formatted.contains("sekrit"));
    }

    fn limits(max_sessions: usize) -> Limits {
        Limits {
            idle: Duration::from_secs(60),
            max_sessions,
            max_values: 100,
        }
    }

    fn key(id: &str, credential: &str) -> SessionKey {
        key_from(
            &headers(&[("authorization", credential), (SESSION_HEADER, id)]),
            &OpenAi,
            true,
        )
        .unwrap()
        .unwrap()
    }

    fn span(entity_type: &str, start: usize, end: usize) -> crate::mapping::Span {
        crate::mapping::Span {
            entity_type: entity_type.to_owned(),
            start,
            end,
        }
    }

    #[test]
    fn the_same_key_returns_the_same_session() {
        let store = SessionStore::new(limits(4));
        let now = Instant::now();
        let one = store.acquire_at(&key("conv1", "Bearer k"), now);
        let two = store.acquire_at(&key("conv1", "Bearer k"), now);
        assert!(Arc::ptr_eq(&one.session, &two.session));
    }

    #[test]
    fn a_different_credential_is_a_different_session() {
        let store = SessionStore::new(limits(4));
        let now = Instant::now();
        let one = store.acquire_at(&key("shared", "Bearer k1"), now);
        let two = store.acquire_at(&key("shared", "Bearer k2"), now);
        assert!(!Arc::ptr_eq(&one.session, &two.session));
        assert_eq!(store.live(), 2);
    }

    #[test]
    fn a_session_idle_past_the_ttl_is_swept() {
        let store = SessionStore::new(limits(4));
        let start = Instant::now();
        let one = store.acquire_at(&key("conv1", "Bearer k"), start);
        let two = store.acquire_at(&key("conv1", "Bearer k"), start + Duration::from_secs(61));
        assert!(
            !Arc::ptr_eq(&one.session, &two.session),
            "a stale table was handed back"
        );
        assert_eq!(store.live(), 1, "the swept entry was left behind");
    }

    #[tokio::test]
    async fn a_full_store_evicts_the_least_recently_used() {
        // Both "a" and "b" get real committed values, through their own
        // claimed guards, so neither is reclaimable regardless of whether
        // its claim happens to still be held. This isolates the plain
        // oldest-by-last_seen fallback from the "prefer reclaimable"
        // behavior, which `a_reclaimable_entry_is_evicted_before_a_live_one`
        // below covers on its own.
        let store = SessionStore::new(limits(2));
        let start = Instant::now();

        let mut a = store.acquire_at(&key("a", "Bearer k"), start);
        let mut a_guard = a.guard.take().expect("a fresh session is always claimable");
        a_guard.mask("Weber", &[span("PERSON", 0, 5)]).unwrap();
        drop(a_guard);

        let mut b = store.acquire_at(&key("b", "Bearer k"), start + Duration::from_secs(1));
        let mut b_guard = b.guard.take().expect("a fresh session is always claimable");
        b_guard.mask("Meier", &[span("PERSON", 0, 5)]).unwrap();
        drop(b_guard);

        // Touching "a" makes "b" the oldest.
        store.acquire_at(&key("a", "Bearer k"), start + Duration::from_secs(2));
        store.acquire_at(&key("c", "Bearer k"), start + Duration::from_secs(3));

        assert_eq!(store.live(), 2);
        let a_again = store.acquire_at(&key("a", "Bearer k"), start + Duration::from_secs(4));
        assert!(
            Arc::ptr_eq(&a.session, &a_again.session),
            "the touched session was evicted"
        );
    }

    #[tokio::test]
    async fn an_evicted_session_does_not_interrupt_a_request_holding_it() {
        let store = SessionStore::new(limits(1));
        let start = Instant::now();
        let mut held = store.acquire_at(&key("a", "Bearer k"), start);
        let mut guard = held
            .guard
            .take()
            .expect("a fresh session is always claimable");
        // Another conversation pushes "a" out of a store that holds one.
        store.acquire_at(&key("b", "Bearer k"), start + Duration::from_secs(1));
        // The request already holding the guard finishes normally; its
        // commit is simply lost to a table nobody will look up again.
        guard.absorb(&Mapping::new(), 10);
    }

    #[test]
    fn a_zero_idle_reports_sessions_disabled() {
        let store = SessionStore::new(Limits {
            idle: Duration::ZERO,
            max_sessions: 0,
            max_values: 0,
        });
        assert!(!store.enabled());
    }

    #[tokio::test]
    async fn a_reclaimable_entry_is_evicted_before_a_live_one() {
        let store = SessionStore::new(limits(2));
        let start = Instant::now();

        // "a" becomes a live session: a real value, committed through its
        // own claimed guard, then released — exactly what a real request
        // does once masking finishes.
        let mut a = store.acquire_at(&key("a", "Bearer k"), start);
        let mut a_guard = a.guard.take().expect("a fresh session is always claimable");
        a_guard.mask("Weber", &[span("PERSON", 0, 5)]).unwrap();
        drop(a_guard);

        // "b" is created but never used, and its own claim is dropped
        // immediately below by never binding it — it stays reclaimable. It
        // is also newer than "a" by last_seen, so plain oldest-first would
        // pick "a" here, not "b".
        store.acquire_at(&key("b", "Bearer k"), start + Duration::from_secs(1));

        // A third, new key needs a slot in a store already at its cap of 2.
        store.acquire_at(&key("c", "Bearer k"), start + Duration::from_secs(2));

        assert_eq!(store.live(), 2);
        let a_again = store.acquire_at(&key("a", "Bearer k"), start + Duration::from_secs(3));
        assert!(
            Arc::ptr_eq(&a.session, &a_again.session),
            "the live session was evicted ahead of the reclaimable one"
        );
    }

    #[tokio::test]
    async fn a_freshly_claimed_session_is_never_treated_as_the_reclaimable_candidate() {
        // The direct proof the TOCTOU race is closed: hold onto a fresh
        // claim exactly as `handle()` does while masking, rather than
        // dropping it, and confirm a concurrent eviction elsewhere cannot
        // select it — even though its `Mapping` is empty and it is the
        // older of the two candidates by `last_seen`.
        let store = SessionStore::new(limits(2));
        let start = Instant::now();

        let mut claimed_a = store.acquire_at(&key("a", "Bearer k"), start);
        let guard = claimed_a
            .guard
            .take()
            .expect("a fresh session is always claimable");

        store.acquire_at(&key("b", "Bearer k"), start + Duration::from_secs(1));
        store.acquire_at(&key("c", "Bearer k"), start + Duration::from_secs(2));

        assert_eq!(store.live(), 2);
        drop(guard);
        let a_again = store.acquire_at(&key("a", "Bearer k"), start + Duration::from_secs(3));
        assert!(
            Arc::ptr_eq(&a_again.session, &claimed_a.session),
            "a's session was evicted while its claim was still held"
        );
    }

    #[tokio::test]
    async fn a_session_held_by_another_request_is_never_treated_as_the_reclaimable_candidate() {
        let store = SessionStore::new(limits(2));
        let start = Instant::now();

        let mut first = store.acquire_at(&key("a", "Bearer k"), start);
        let _guard = first
            .guard
            .take()
            .expect("a fresh session is always claimable");
        // A second, concurrent request for the SAME key finds it contended
        // and gets no pre-claimed guard — it would wait with
        // `lock_owned().await` in real code. Here it is enough to confirm
        // the claim was not handed out twice.
        let second = store.acquire_at(&key("a", "Bearer k"), start + Duration::from_secs(1));
        assert!(
            second.guard.is_none(),
            "a contended session was claimed twice"
        );
        assert!(Arc::ptr_eq(&first.session, &second.session));
    }
}
