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
    #[error(
        "every session in the store is in flight, so there is none to reclaim; the request \
         is refused rather than evicting a live session, which would leave one conversation \
         with two unsynchronized tables"
    )]
    Saturated,
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

/// How costly an entry is to take from the map, cheapest first. `Ord` is
/// derived from the declaration order, so the scan can simply take the
/// minimum.
///
/// Claiming the lock here and dropping it immediately only ever succeeds
/// when nobody else currently needs it — the same guarantee that makes the
/// return-time claim in `acquire_at` safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Tier {
    /// Nobody holds it and it remembers nothing: taking it costs nothing at all.
    FreeAndEmpty,
    /// Nobody holds it, but a conversation loses its coreference. The client
    /// re-sends the history, so the next request rebuilds the table.
    FreeWithValues,
    /// A request is inside it right now. Taking this one strands that request
    /// and lets the next request for the same id open a second, unsynchronized
    /// table for one conversation — where two concurrent requests can allocate
    /// one placeholder to two different values, and a response restores the
    /// wrong person's name.
    Held,
}

fn tier(entry: &Entry) -> Tier {
    // A strong count above one — the map's own reference — means a
    // `Claimed` clone is alive outside it: some request either holds the
    // session's mutex right now, or was handed `guard: None` and has not
    // reached its own `lock_owned().await` yet. That gap is exactly where
    // the mutex reads as free while a request is still headed for it, and
    // treating it as free there would let an unrelated eviction take the
    // table out from under that request — the same wrong-name failure this
    // file exists to prevent, reached through the "someone is inside it"
    // door instead of the "the guard is locked" one. A new clone is only
    // ever minted inside `acquire_at`'s own critical section, which is
    // exactly the section this scan runs in, so the count can be
    // stale-high (a request dropped its `Claimed` a moment ago) but never
    // stale-low — and stale-high only costs an eviction candidate.
    if Arc::strong_count(&entry.session) > 1 {
        return Tier::Held;
    }
    match Arc::clone(&entry.session.mapping).try_lock_owned() {
        Ok(guard) if guard.is_empty() => Tier::FreeAndEmpty,
        Ok(_) => Tier::FreeWithValues,
        Err(_) => Tier::Held,
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

    pub fn acquire(&self, key: &SessionKey) -> Result<Claimed, SessionError> {
        self.acquire_at(key, Instant::now())
    }

    /// Time is a parameter so eviction is tested without sleeping.
    fn acquire_at(&self, key: &SessionKey, now: Instant) -> Result<Claimed, SessionError> {
        // Poisoning is recovered rather than propagated. Nothing in this
        // critical section can panic today, but if something ever does, one
        // failure must not turn every later request into a 500.
        let mut map = self.inner.lock().unwrap_or_else(|error| error.into_inner());

        // Sweeping here rather than in a background task: at a thousand
        // sessions this costs less than a syscall, and there is no separate
        // task whose health is a new thing to reason about. A held entry is
        // spared whatever its age — removing one is the same mistake as
        // evicting it under saturation. `||` short-circuits, so the extra
        // `try_lock` is paid only by entries that are already stale.
        map.retain(|_, entry| {
            now.duration_since(entry.last_seen) < self.limits.idle || tier(entry) == Tier::Held
        });

        let session = if let Some(entry) = map.get_mut(key) {
            entry.last_seen = now;
            Arc::clone(&entry.session)
        } else {
            if map.len() >= self.limits.max_sessions {
                // One pass and one `try_lock` per entry: the cheapest tier
                // wins and `last_seen` breaks ties within it. A winner in
                // `Tier::Held` means every entry is in flight, and there is
                // nothing left to take that would not cost somebody their
                // conversation — so the newcomer is refused instead. That
                // trades a silent identity swap for a loud 503, which is the
                // trade this gateway makes everywhere else.
                let victim = map
                    .iter()
                    .map(|(key, entry)| (tier(entry), entry.last_seen, key))
                    .min_by_key(|(cost, last_seen, _)| (*cost, *last_seen))
                    .map(|(cost, _, key)| (cost, key.clone()));
                match victim {
                    // `None` is an empty map that is nonetheless full, which
                    // means `max_sessions == 0` — rejected by `config.rs`
                    // unless sessions are disabled entirely, in which case
                    // `key_from` refuses before this is ever reached.
                    Some((Tier::Held, _)) | None => return Err(SessionError::Saturated),
                    Some((_, victim)) => {
                        map.remove(&victim);
                    }
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
        Ok(Claimed { session, guard })
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
        let one = store.acquire_at(&key("conv1", "Bearer k"), now).unwrap();
        let two = store.acquire_at(&key("conv1", "Bearer k"), now).unwrap();
        assert!(Arc::ptr_eq(&one.session, &two.session));
    }

    #[test]
    fn a_different_credential_is_a_different_session() {
        let store = SessionStore::new(limits(4));
        let now = Instant::now();
        let one = store.acquire_at(&key("shared", "Bearer k1"), now).unwrap();
        let two = store.acquire_at(&key("shared", "Bearer k2"), now).unwrap();
        assert!(!Arc::ptr_eq(&one.session, &two.session));
        assert_eq!(store.live(), 2);
    }

    #[test]
    fn a_session_idle_past_the_ttl_is_swept() {
        let store = SessionStore::new(limits(4));
        let start = Instant::now();
        let one = store.acquire_at(&key("conv1", "Bearer k"), start).unwrap();
        // A `Weak` proves identity without itself counting as an
        // outstanding claimant. The claim goes with the request that took
        // it, guard and all — dropping `one` in full, not just its guard,
        // is what makes this the "nobody holding it" case;
        // `a_held_entry_survives_the_ttl_sweep` covers the other one.
        let one_session = Arc::downgrade(&one.session);
        drop(one);
        store
            .acquire_at(&key("conv1", "Bearer k"), start + Duration::from_secs(61))
            .unwrap();
        assert!(
            one_session.upgrade().is_none(),
            "a stale table was handed back"
        );
        assert_eq!(store.live(), 1, "the swept entry was left behind");
    }

    #[tokio::test]
    async fn a_held_entry_survives_the_ttl_sweep() {
        let store = SessionStore::new(limits(4));
        let start = Instant::now();

        let held = store.acquire_at(&key("a", "Bearer k"), start).unwrap();
        assert!(held.guard.is_some(), "a fresh session is always claimable");
        let held_session = Arc::downgrade(&held.session);

        // Far past `idle`, and a request is still holding it. Sweeping it
        // would strand that request's commit and let the next request for
        // "a" open a second table for one conversation.
        store
            .acquire_at(&key("b", "Bearer k"), start + Duration::from_secs(61))
            .unwrap();
        assert_eq!(store.live(), 2, "a held entry was swept");

        // Released — guard and claim together, exactly as `handle()` drops
        // `claimed` once masking finishes — it is ordinary again: still
        // stale, so the next sweep takes it and "a" comes back as a fresh
        // table.
        drop(held);
        store
            .acquire_at(&key("a", "Bearer k"), start + Duration::from_secs(62))
            .unwrap();
        assert!(
            held_session.upgrade().is_none(),
            "a stale table outlived its holder"
        );
    }

    #[tokio::test]
    async fn a_full_store_evicts_the_least_recently_used() {
        // Both "a" and "b" get real committed values, through their own
        // claimed guards, then are dropped in full — guard and claim
        // together, exactly as `handle()` drops `claimed` once masking
        // finishes — so neither reads as an outstanding claimant and both
        // are judged on tier and `last_seen` alone. This isolates the plain
        // oldest-by-last_seen fallback from the "prefer reclaimable"
        // behavior, which `a_reclaimable_entry_is_evicted_before_a_live_one`
        // below covers on its own.
        let store = SessionStore::new(limits(2));
        let start = Instant::now();

        let mut a = store.acquire_at(&key("a", "Bearer k"), start).unwrap();
        a.guard
            .as_mut()
            .expect("a fresh session is always claimable")
            .mask("Weber", &[span("PERSON", 0, 5)])
            .unwrap();
        let a_session = Arc::downgrade(&a.session);
        drop(a);

        let mut b = store
            .acquire_at(&key("b", "Bearer k"), start + Duration::from_secs(1))
            .unwrap();
        b.guard
            .as_mut()
            .expect("a fresh session is always claimable")
            .mask("Meier", &[span("PERSON", 0, 5)])
            .unwrap();
        drop(b);

        // Touching "a" makes "b" the oldest.
        store
            .acquire_at(&key("a", "Bearer k"), start + Duration::from_secs(2))
            .unwrap();
        store
            .acquire_at(&key("c", "Bearer k"), start + Duration::from_secs(3))
            .unwrap();

        assert_eq!(store.live(), 2);
        let a_again = store
            .acquire_at(&key("a", "Bearer k"), start + Duration::from_secs(4))
            .unwrap();
        let a_survived = a_session
            .upgrade()
            .expect("the touched session was evicted");
        assert!(
            Arc::ptr_eq(&a_survived, &a_again.session),
            "the touched session was evicted"
        );
    }

    #[tokio::test]
    async fn a_held_session_is_refused_room_rather_than_evicted() {
        let store = SessionStore::new(limits(1));
        let start = Instant::now();
        let mut held = store.acquire_at(&key("a", "Bearer k"), start).unwrap();
        let mut guard = held
            .guard
            .take()
            .expect("a fresh session is always claimable");

        // Another conversation wants the only slot, and the entry holding it
        // is in flight. It is refused rather than served at "a"'s expense.
        let refused = store.acquire_at(&key("b", "Bearer k"), start + Duration::from_secs(1));
        assert!(matches!(refused, Err(SessionError::Saturated)));

        // The request holding "a" finishes normally, and its commit still
        // lands where the next request for "a" will look for it.
        guard.absorb(&Mapping::new(), 10);
        drop(guard);
        let a_again = store
            .acquire_at(&key("a", "Bearer k"), start + Duration::from_secs(2))
            .unwrap();
        assert!(
            Arc::ptr_eq(&held.session, &a_again.session),
            "the held session was lost anyway"
        );
    }

    #[tokio::test]
    async fn a_store_whose_every_entry_is_held_refuses_a_new_session() {
        let store = SessionStore::new(limits(2));
        let start = Instant::now();

        let mut a = store.acquire_at(&key("a", "Bearer k"), start).unwrap();
        let _a_guard = a.guard.take().expect("a fresh session is always claimable");
        let mut b = store
            .acquire_at(&key("b", "Bearer k"), start + Duration::from_secs(1))
            .unwrap();
        let _b_guard = b.guard.take().expect("a fresh session is always claimable");

        // Both entries are in flight — the state a concurrent burst produces,
        // where every request holds its guard across its detector round-trip.
        let refused = store.acquire_at(&key("c", "Bearer k"), start + Duration::from_secs(2));
        assert!(
            matches!(refused, Err(SessionError::Saturated)),
            "a live session was taken to make room"
        );
        assert_eq!(store.live(), 2, "the store lost an entry anyway");
    }

    #[tokio::test]
    async fn a_saturated_store_still_serves_a_session_it_already_holds() {
        // Saturation refuses newcomers, never a conversation already in the
        // store: that path finds its entry by key and never reaches the scan.
        let store = SessionStore::new(limits(1));
        let start = Instant::now();

        let held = store.acquire_at(&key("a", "Bearer k"), start).unwrap();
        let again = store
            .acquire_at(&key("a", "Bearer k"), start + Duration::from_secs(1))
            .unwrap();

        assert!(Arc::ptr_eq(&held.session, &again.session));
        assert!(
            again.guard.is_none(),
            "a contended session was claimed twice"
        );
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
        // own claimed guard, then released in full — guard and claim
        // together, exactly as `handle()` drops `claimed` once masking
        // finishes — so it reads as merely reclaimable (`FreeWithValues`)
        // rather than as an outstanding claimant (`Held`). Without this,
        // "a" would classify `Held` at the "c" acquisition below and this
        // test would only prove `FreeAndEmpty < Held`, not the
        // `FreeAndEmpty < FreeWithValues` boundary it is about.
        let mut a = store.acquire_at(&key("a", "Bearer k"), start).unwrap();
        a.guard
            .as_mut()
            .expect("a fresh session is always claimable")
            .mask("Weber", &[span("PERSON", 0, 5)])
            .unwrap();
        let a_session = Arc::downgrade(&a.session);
        drop(a);

        // "b" is created but never used, and its own claim is dropped
        // immediately below by never binding it — it stays reclaimable. It
        // is also newer than "a" by last_seen, so plain oldest-first would
        // pick "a" here, not "b".
        store
            .acquire_at(&key("b", "Bearer k"), start + Duration::from_secs(1))
            .unwrap();

        // A third, new key needs a slot in a store already at its cap of 2.
        store
            .acquire_at(&key("c", "Bearer k"), start + Duration::from_secs(2))
            .unwrap();

        assert_eq!(store.live(), 2);
        let a_again = store
            .acquire_at(&key("a", "Bearer k"), start + Duration::from_secs(3))
            .unwrap();
        let survived = a_session
            .upgrade()
            .expect("the live session was evicted ahead of the reclaimable one");
        assert!(
            Arc::ptr_eq(&survived, &a_again.session),
            "the live session was evicted ahead of the reclaimable one"
        );
    }

    #[tokio::test]
    async fn a_free_entry_with_values_is_evicted_before_a_held_one() {
        let store = SessionStore::new(limits(2));
        let start = Instant::now();

        // "a" is held — its guard is taken and kept — and its mapping is
        // empty, but it is the OLDER of the two by `last_seen`. Plain
        // oldest-first would pick "a" here, same as "b" below is newer: this
        // test only passes if `Tier::Held` outranking `Tier::FreeWithValues`
        // in the scan's ordering dominates that tiebreak, which is the half
        // of the tier order this branch's whole guarantee rests on.
        let mut a = store.acquire_at(&key("a", "Bearer k"), start).unwrap();
        let a_guard = a.guard.take().expect("a fresh session is always claimable");

        // "b" is free but holds a committed value, and newer by `last_seen`.
        // Dropped in full — guard and claim together, exactly as `handle()`
        // drops `claimed` once masking finishes — so it reads as merely
        // reclaimable rather than as another outstanding claimant.
        let mut b = store
            .acquire_at(&key("b", "Bearer k"), start + Duration::from_secs(1))
            .unwrap();
        b.guard
            .as_mut()
            .expect("a fresh session is always claimable")
            .mask("Weber", &[span("PERSON", 0, 5)])
            .unwrap();
        drop(b);

        // A third, new key needs a slot in a store already at its cap of 2.
        store
            .acquire_at(&key("c", "Bearer k"), start + Duration::from_secs(2))
            .unwrap();
        assert_eq!(store.live(), 2, "the store grew instead of evicting");

        // "a" is still reachable once its guard drops: it was not the one
        // taken, even though it is older and was in flight the whole time.
        drop(a_guard);
        let a_again = store
            .acquire_at(&key("a", "Bearer k"), start + Duration::from_secs(3))
            .unwrap();
        assert!(
            Arc::ptr_eq(&a.session, &a_again.session),
            "the held entry was evicted ahead of the free one"
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

        let mut claimed_a = store.acquire_at(&key("a", "Bearer k"), start).unwrap();
        let guard = claimed_a
            .guard
            .take()
            .expect("a fresh session is always claimable");

        store
            .acquire_at(&key("b", "Bearer k"), start + Duration::from_secs(1))
            .unwrap();
        store
            .acquire_at(&key("c", "Bearer k"), start + Duration::from_secs(2))
            .unwrap();

        assert_eq!(store.live(), 2);
        drop(guard);
        let a_again = store
            .acquire_at(&key("a", "Bearer k"), start + Duration::from_secs(3))
            .unwrap();
        assert!(
            Arc::ptr_eq(&a_again.session, &claimed_a.session),
            "a's session was evicted while its claim was still held"
        );
    }

    #[tokio::test]
    async fn a_session_held_by_another_request_is_never_treated_as_the_reclaimable_candidate() {
        let store = SessionStore::new(limits(2));
        let start = Instant::now();

        let mut first = store.acquire_at(&key("a", "Bearer k"), start).unwrap();
        let _guard = first
            .guard
            .take()
            .expect("a fresh session is always claimable");
        // A second, concurrent request for the SAME key finds it contended
        // and gets no pre-claimed guard — it would wait with
        // `lock_owned().await` in real code. Here it is enough to confirm
        // the claim was not handed out twice.
        let second = store
            .acquire_at(&key("a", "Bearer k"), start + Duration::from_secs(1))
            .unwrap();
        assert!(
            second.guard.is_none(),
            "a contended session was claimed twice"
        );
        assert!(Arc::ptr_eq(&first.session, &second.session));
    }

    #[tokio::test]
    async fn a_pending_contended_claim_is_not_evicted_between_release_and_relock() {
        // The gap `tier()`'s strong-count check exists for: a contended
        // second request holds a `Claimed` with `guard: None` and only
        // registers on the mutex at its own `lock_owned().await`, which
        // this test never reaches — so between the first holder's release
        // and that future lock, `try_lock_owned` alone would read the
        // entry as free.
        let store = SessionStore::new(limits(1));
        let start = Instant::now();

        let mut first = store.acquire_at(&key("a", "Bearer k"), start).unwrap();
        let a_session = Arc::downgrade(&first.session);
        let guard = first
            .guard
            .take()
            .expect("a fresh session is always claimable");

        let second = store
            .acquire_at(&key("a", "Bearer k"), start + Duration::from_secs(1))
            .unwrap();
        assert!(
            second.guard.is_none(),
            "a contended session was claimed twice"
        );

        // The first request finishes and releases the mutex. The second
        // has not locked it yet — the entry's mutex reads as free right
        // now, but the second request is still headed for it.
        drop(guard);

        // The store's only slot belongs to "a"; an unrelated key wants it.
        let refused = store.acquire_at(&key("b", "Bearer k"), start + Duration::from_secs(2));
        assert!(
            matches!(refused, Err(SessionError::Saturated)),
            "the pending session was evicted instead of refusing"
        );
        assert_eq!(store.live(), 1, "the pending entry was evicted anyway");

        // Once both claims are gone, "a" is still the same table — not one
        // an unrelated eviction quietly replaced underneath the pending
        // request.
        drop(second);
        drop(first);
        let a_again = store
            .acquire_at(&key("a", "Bearer k"), start + Duration::from_secs(3))
            .unwrap();
        let survived = a_session
            .upgrade()
            .expect("the pending session was evicted");
        assert!(
            Arc::ptr_eq(&survived, &a_again.session),
            "a stale table replaced the pending one"
        );
    }

    #[tokio::test]
    async fn a_locked_entry_with_no_pending_claimant_is_still_held() {
        // The strong-count check short-circuits `tier()` before its
        // `try_lock_owned` arm ever runs whenever a live `Claimed` is
        // around — which every other test that produces a held entry
        // happens to keep. This one drops the `Claimed` in full, so the
        // only thing left signaling "held" is the lock itself: proof the
        // `Err(_) => Tier::Held` arm still does its job on its own.
        let store = SessionStore::new(limits(1));
        let start = Instant::now();

        let mut a = store.acquire_at(&key("a", "Bearer k"), start).unwrap();
        let guard = a.guard.take().expect("a fresh session is always claimable");
        drop(a);

        let refused = store.acquire_at(&key("b", "Bearer k"), start + Duration::from_secs(1));
        assert!(
            matches!(refused, Err(SessionError::Saturated)),
            "a locked entry with no pending claimant was evicted"
        );
        assert_eq!(store.live(), 1, "the locked entry was evicted anyway");

        drop(guard);
    }
}
