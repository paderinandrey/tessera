# Session-Scoped Mapping Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give a conversation one value-to-placeholder table, so a value keeps the same placeholder across the turns of a conversation instead of only within one request.

**Architecture:** A `SessionStore` in `AppState` maps `(credential fingerprint, client id)` to a `Session` holding a `Mapping`. The session lock is held for the detect-and-mask phase and released before the upstream call; restoration — buffered or streamed — works from a snapshot clone of that mapping. New pairs are committed back to the session with `Mapping::absorb`, which runs after the last fallible step, so a refused request leaves the session untouched.

**Tech Stack:** Rust 2021, axum 0.7, tokio 1, `std::sync::Mutex` for the store map, `tokio::sync::Mutex` for a session's mapping, `sha2` for the credential fingerprint, `getrandom` for the per-process salt. Tests are `cargo test` with wiremock, as the existing gateway tests are.

## Global Constraints

- The spec is `docs/superpowers/specs/2026-08-07-session-mapping-design.md`. Where this plan and the spec disagree, the spec wins — stop and ask.
- Work on branch `feat/session-mapping`, which already exists and already carries the spec commit.
- Detection still runs over every text in every request. The session table never substitutes for detection.
- No original value and no raw session id may reach a log line at any level.
- Every new refusal happens **before** the upstream call and returns 400.
- Values are never evicted from within a live session. Whole sessions are evicted; parts of them are not.
- The store's `std::sync::Mutex` is never held across an `await`.
- `cargo fmt --check`, `cargo clippy -- -D warnings` and `cargo test` must all pass before each commit. Run them from `gateway/`.

## File Structure

| File | Responsibility |
|---|---|
| `gateway/src/session.rs` | **New.** Session identity (header validation, credential fingerprint, `SessionKey`), the store, its bounds and eviction policy, `SessionError`. |
| `gateway/src/mapping.rs` | **Modify.** Gains `Clone`, allocation order, `absorb`, `len`/`is_empty`. |
| `gateway/src/config.rs` | **Modify.** Three session keys and their validation. |
| `gateway/src/provider.rs` | **Modify.** One trait method: which header authenticates this provider. |
| `gateway/src/proxy.rs` | **Modify.** `AppState.sessions`, `ProxyError::Session`, `mask_all`, the session branch in `handle`. |
| `gateway/src/main.rs` | **Modify.** Declare the new module. |
| `gateway/Cargo.toml` | **Modify.** `sha2`, `getrandom`. |
| `gateway/tessera.example.toml` | **Modify.** The three keys, documented. |
| `README.md` | **Modify.** A Sessions subsection under Gateway. |

Six tasks. Tasks 1 and 2 are pure and independent of each other; 3 and 4 build `session.rs` bottom-up; 5 wires it into the proxy; 6 covers streaming and the documentation.

---

### Task 1: `Mapping` learns allocation order and `absorb`

**Files:**
- Modify: `gateway/src/mapping.rs:38-43` (the struct), `gateway/src/mapping.rs:106-137` (`placeholder_for`)
- Test: `gateway/src/mapping.rs`, the existing `mod tests` at the bottom

**Interfaces:**
- Consumes: nothing.
- Produces: `Mapping: Clone`; `Mapping::absorb(&mut self, other: &Mapping, cap: usize)`; `Mapping::len(&self) -> usize`; `Mapping::is_empty(&self) -> bool`. Task 4 stores a `Mapping` in a session, Task 5 calls `absorb`, Task 5 asserts on `is_empty`.

Background the implementer needs: `Mapping` holds `by_value` (original → placeholder) and `by_placeholder` (placeholder → original). `reserve_literals` inserts placeholder-shaped tokens found in the caller's own text into `by_placeholder` mapped **to themselves**, so an echo restores unchanged. Those entries must never be committed to a session — they name nobody. Tracking allocation order in a separate `Vec` that only `placeholder_for` pushes to gives `absorb` exactly the right set for free.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `gateway/src/mapping.rs`:

```rust
#[test]
fn absorb_commits_new_pairs_in_the_order_they_were_issued() {
    let mut session = Mapping::new();
    let mut work = session.clone();
    work.mask(
        "Weber und Meier",
        &[span("PERSON", 0, 5), span("PERSON", 10, 15)],
    )
    .unwrap();
    session.absorb(&work, 10);
    assert_eq!(
        session.restore("[PERSON_1] und [PERSON_2]").unwrap(),
        "Weber und Meier"
    );
}

#[test]
fn absorb_stops_at_the_cap_and_keeps_the_earliest() {
    let mut session = Mapping::new();
    let mut work = session.clone();
    work.mask(
        "Weber und Meier",
        &[span("PERSON", 0, 5), span("PERSON", 10, 15)],
    )
    .unwrap();
    session.absorb(&work, 1);
    assert_eq!(session.len(), 1);
    assert_eq!(session.restore("[PERSON_1]").unwrap(), "Weber");
    assert!(session.restore("[PERSON_2]").is_err());
}

#[test]
fn absorb_carries_the_counter_past_what_it_declined() {
    let mut session = Mapping::new();
    let mut work = session.clone();
    work.mask(
        "Weber und Meier",
        &[span("PERSON", 0, 5), span("PERSON", 10, 15)],
    )
    .unwrap();
    // [PERSON_2] went to Meier and was declined by the cap. It must not be
    // reissued: the number already named somebody in a request that shipped.
    session.absorb(&work, 1);
    let mut next = session.clone();
    next.mask("Schmidt", &[span("PERSON", 0, 7)]).unwrap();
    assert_eq!(next.restore("[PERSON_3]").unwrap(), "Schmidt");
}

#[test]
fn a_clone_does_not_write_back_to_its_source() {
    let session = Mapping::new();
    let mut work = session.clone();
    work.mask("Weber", &[span("PERSON", 0, 5)]).unwrap();
    // The request failed before absorb; the session never heard of Weber.
    assert!(session.restore("[PERSON_1]").is_err());
}

#[test]
fn absorb_ignores_placeholders_reserved_from_the_callers_own_text() {
    let mut session = Mapping::new();
    let mut work = session.clone();
    // "[PERSON_9]" is 10 characters, so "Weber" starts at 16.
    work.mask("[PERSON_9] traf Weber", &[span("PERSON", 16, 21)])
        .unwrap();
    session.absorb(&work, 10);
    assert_eq!(session.len(), 1);
    assert_eq!(session.restore("[PERSON_1]").unwrap(), "Weber");
    // The literal was reserved so an echo survives, not committed: it names
    // nobody, and a session that remembered it would restore it to itself
    // for every later caller of this conversation.
    assert!(session.restore("[PERSON_9]").is_err());
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd gateway && cargo test mapping:: 2>&1 | tail -30`
Expected: compilation fails — `Mapping` has no method `clone`, `absorb` or `len`.

- [ ] **Step 3: Add the field, the derive, and the accessors**

In `gateway/src/mapping.rs`, replace the struct definition:

```rust
#[derive(Debug, Default, Clone)]
pub struct Mapping {
    by_value: HashMap<String, String>,
    by_placeholder: HashMap<String, String>,
    /// Placeholders in the order they were issued. A session commits in this
    /// order, so a cap keeps the earliest values rather than an arbitrary set.
    /// Literals reserved from the caller's own text are deliberately absent:
    /// they map to themselves and name nobody.
    order: Vec<String>,
    next: usize,
}
```

In `placeholder_for`, immediately after the two existing `insert` calls and before `Ok(placeholder)`:

```rust
        self.order.push(placeholder.clone());
```

Add to `impl Mapping`, next to `new`:

```rust
    /// How many values this mapping has issued placeholders for.
    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
```

- [ ] **Step 4: Implement `absorb`**

Add to `impl Mapping`, after `mask`:

```rust
    /// Commit `other`'s new pairs, in the order they were issued, until `cap`
    /// is reached. `other` is the copy a request masked with; `self` is the
    /// session's table.
    ///
    /// The counter moves past pairs that were declined. A number that already
    /// named somebody in a request that reached the provider must never be
    /// issued to a second value, or a later response restores the wrong name.
    pub fn absorb(&mut self, other: &Mapping, cap: usize) {
        for placeholder in &other.order {
            if self.by_placeholder.contains_key(placeholder) {
                continue;
            }
            if self.order.len() >= cap {
                break;
            }
            let Some(value) = other.by_placeholder.get(placeholder) else {
                continue;
            };
            self.by_value.insert(value.clone(), placeholder.clone());
            self.by_placeholder
                .insert(placeholder.clone(), value.clone());
            self.order.push(placeholder.clone());
        }
        self.next = self.next.max(other.next);
    }
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cd gateway && cargo test mapping:: 2>&1 | tail -20`
Expected: PASS, including the pre-existing mapping tests.

- [ ] **Step 6: Check formatting and lints**

Run: `cd gateway && cargo fmt && cargo clippy --all-targets -- -D warnings`
Expected: no warnings. Clippy requires `is_empty` wherever `len` exists, which Step 3 already added.

- [ ] **Step 7: Commit**

```bash
git add gateway/src/mapping.rs
git commit -m "feat(gateway): a mapping can be cloned and absorbed under a cap

A session commits a request's new pairs in the order they were issued, so
a cap keeps the earliest values. The counter moves past what it declined:
a number that already named somebody upstream is never issued again."
```

---

### Task 2: Configuration for the three bounds

**Files:**
- Modify: `gateway/src/config.rs`
- Test: `gateway/src/config.rs`, the existing `mod tests`

**Interfaces:**
- Consumes: nothing.
- Produces: `Config::session_idle_secs: u64`, `Config::max_sessions: usize`, `Config::max_session_values: usize`. Task 5 reads all three in `AppState::from_config`.

`Config` already uses `#[serde(deny_unknown_fields)]`, so a typo in one of these keys fails loudly without any extra work. What needs adding is the cross-field rule: with sessions on, a zero limit would create sessions that can hold nothing — a configuration that looks enabled and does nothing.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `gateway/src/config.rs`:

```rust
#[test]
fn session_defaults_are_bounded() {
    let config = Config::from_toml("").expect("empty config is valid");
    assert_eq!(config.session_idle_secs, 1800);
    assert_eq!(config.max_sessions, 1000);
    assert_eq!(config.max_session_values, 1000);
}

#[test]
fn session_values_override_the_defaults() {
    let config = Config::from_toml(
        r#"
        session_idle_secs = 60
        max_sessions = 4
        max_session_values = 8
        "#,
    )
    .expect("valid config");
    assert_eq!(config.session_idle_secs, 60);
    assert_eq!(config.max_sessions, 4);
    assert_eq!(config.max_session_values, 8);
}

#[test]
fn zero_idle_disables_sessions_and_permits_zero_limits() {
    let config = Config::from_toml(
        r#"
        session_idle_secs = 0
        max_sessions = 0
        max_session_values = 0
        "#,
    )
    .expect("a deployment that wants no personal data in memory between requests");
    assert_eq!(config.session_idle_secs, 0);
}

#[test]
fn a_zero_limit_with_sessions_enabled_is_rejected() {
    // Enabled and unable to hold anything is not a configuration, it is a typo.
    assert!(Config::from_toml("max_sessions = 0").is_err());
    assert!(Config::from_toml("max_session_values = 0").is_err());
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd gateway && cargo test config:: 2>&1 | tail -30`
Expected: compilation fails — `Config` has no field `session_idle_secs`.

- [ ] **Step 3: Add the fields and their defaults**

In `gateway/src/config.rs`, add to `struct Config` after `anthropic_base`:

```rust
    /// How long a session survives without a request. Zero disables sessions
    /// entirely: personal data then never outlives the request that carried it,
    /// which is the right setting for a deployment that wants that guarantee.
    #[serde(default = "default_session_idle_secs")]
    pub session_idle_secs: u64,
    /// How many conversations may hold a table at once. The oldest is dropped
    /// when a new one arrives at the limit.
    #[serde(default = "default_max_sessions")]
    pub max_sessions: usize,
    /// How many values one conversation may remember. Past it, values are still
    /// masked and still restored — they are simply not remembered.
    #[serde(default = "default_max_session_values")]
    pub max_session_values: usize,
```

And the default functions beside the existing ones:

```rust
fn default_session_idle_secs() -> u64 {
    1800
}
fn default_max_sessions() -> usize {
    1000
}
fn default_max_session_values() -> usize {
    1000
}
```

- [ ] **Step 4: Add the validation**

Add two variants to `ConfigError`:

```rust
    #[error("max_sessions must be greater than zero unless session_idle_secs is zero")]
    ZeroSessionLimit,
    #[error("max_session_values must be greater than zero unless session_idle_secs is zero")]
    ZeroSessionValues,
```

And in `Config::from_toml`, after the existing timeout check:

```rust
        if config.session_idle_secs > 0 {
            if config.max_sessions == 0 {
                return Err(ConfigError::ZeroSessionLimit);
            }
            if config.max_session_values == 0 {
                return Err(ConfigError::ZeroSessionValues);
            }
        }
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cd gateway && cargo test config:: 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 6: Check formatting and lints**

Run: `cd gateway && cargo fmt && cargo clippy --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 7: Commit**

```bash
git add gateway/src/config.rs
git commit -m "feat(gateway): configure the three session bounds

An idle TTL, a cap on live sessions and a cap on values per session. Zero
idle disables sessions outright; a zero limit while they are enabled is a
typo rather than a configuration, and is rejected."
```

---

### Task 3: Session identity — the header and the credential fingerprint

**Files:**
- Create: `gateway/src/session.rs`
- Modify: `gateway/Cargo.toml`, `gateway/src/main.rs:9-14`, `gateway/src/provider.rs:17-30` (the trait) and both `impl Provider` blocks
- Test: `gateway/src/session.rs`, a new `mod tests`

**Interfaces:**
- Consumes: `Mapping` from Task 1 (only as an import path for Task 4; not used yet in this task).
- Produces:
  - `pub const SESSION_HEADER: &str = "x-tessera-session"`
  - `pub enum SessionError { BadId, Disabled, NoCredential(&'static str) }`, implementing `std::error::Error` via `thiserror`
  - `pub struct SessionKey` — `Clone + PartialEq + Eq + Hash + Debug`
  - `pub fn key_from(headers: &HeaderMap, provider: &dyn Provider, enabled: bool) -> Result<Option<SessionKey>, SessionError>`
  - `SessionKey::digest(&self) -> String` — the only form of a key that may be logged
  - `Provider::credential_header(&self) -> &'static str`

Task 4 adds the store to this same file. Task 5 calls `key_from` and maps `SessionError` to 400.

Why the fingerprint exists at all, so the implementer does not simplify it away: a session table is a restoration oracle. A caller who writes `[PERSON_1]` into a prompt gets it echoed by the model, and the gateway restores it on the way back. If the client's id alone selected a session, guessing another caller's id would read their table out one placeholder at a time. Keying on the credential too means a guessed id lands in an empty namespace.

- [ ] **Step 1: Add the two dependencies**

In `gateway/Cargo.toml`, under `[dependencies]`:

```toml
sha2 = "0.10"
getrandom = "0.2"
```

Run: `cd gateway && cargo build 2>&1 | tail -5`
Expected: builds; `getrandom` 0.2 is already in `Cargo.lock` transitively.

- [ ] **Step 2: Add the trait method**

In `gateway/src/provider.rs`, add to `pub trait Provider` after `upstream_path`:

```rust
    /// The header this provider authenticates with. A session is namespaced by
    /// its value: the mapping table is a restoration oracle, and an id alone
    /// would let a guessed id read another caller's values out of it one
    /// placeholder at a time.
    fn credential_header(&self) -> &'static str;
```

In the `impl Provider for OpenAi` block:

```rust
    fn credential_header(&self) -> &'static str {
        "authorization"
    }
```

In the `impl Provider for Anthropic` block:

```rust
    fn credential_header(&self) -> &'static str {
        "x-api-key"
    }
```

- [ ] **Step 3: Declare the module**

In `gateway/src/main.rs`, add to the module list so it stays alphabetical:

```rust
mod session;
```

(The list becomes `config, detector, mapping, provider, proxy, session, stream`.)

- [ ] **Step 4: Write the failing tests**

Create `gateway/src/session.rs` containing only this test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::OpenAi;
    use axum::http::HeaderMap;

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
}
```

- [ ] **Step 5: Run the tests to verify they fail**

Run: `cd gateway && cargo test session:: 2>&1 | tail -30`
Expected: compilation fails — `key_from`, `SESSION_HEADER` and `SessionError` do not exist.

- [ ] **Step 6: Write the implementation**

Prepend to `gateway/src/session.rs`, above the test module:

```rust
//! Session identity and the store that holds one table per conversation.
//!
//! A session table is a restoration oracle: a caller who writes a placeholder
//! into a prompt gets it echoed by the model, and the gateway restores it on
//! the way back. So a client-chosen id never selects a session on its own — it
//! is namespaced by a fingerprint of the caller's own credential.

use axum::http::HeaderMap;
use sha2::{Digest, Sha256};

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
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionKey {
    credential: [u8; 32],
    id: String,
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
        digest[..4].iter().map(|byte| format!("{byte:02x}")).collect()
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
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cd gateway && cargo test session:: 2>&1 | tail -20`
Expected: PASS, eight tests.

- [ ] **Step 8: Run the whole suite, formatting and lints**

Run: `cd gateway && cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test 2>&1 | tail -20`
Expected: no warnings, everything passes. The new trait method is implemented for both providers, so `provider.rs` still compiles.

- [ ] **Step 9: Commit**

```bash
git add gateway/Cargo.toml gateway/Cargo.lock gateway/src/session.rs gateway/src/main.rs gateway/src/provider.rs
git commit -m "feat(gateway): a session id is namespaced by the caller's credential

A session table is a restoration oracle: echo a placeholder and the
gateway restores it. An id alone would let a guessed id read another
caller's table out one placeholder at a time, so the store keys on a
salted fingerprint of the credential as well.

The raw id never reaches a log — a client may well name its session
after the person in it."
```

---

### Task 4: The store, its bounds, and eviction

**Files:**
- Modify: `gateway/src/session.rs`
- Test: `gateway/src/session.rs`, the `mod tests` from Task 3

**Interfaces:**
- Consumes: `SessionKey` and `key_from` from Task 3; `Mapping` from Task 1.
- Produces:
  - `pub struct Limits { pub idle: Duration, pub max_sessions: usize, pub max_values: usize }` — `Clone + Copy + Debug`
  - `pub struct Session { pub mapping: tokio::sync::Mutex<Mapping> }`
  - `pub struct SessionStore`, with `SessionStore::new(Limits)`, `acquire(&self, &SessionKey) -> Arc<Session>`, `enabled(&self) -> bool`, `max_values(&self) -> usize`, and `#[cfg(test)] pub(crate) fn live(&self) -> usize`

Two lock types, deliberately. The store's map is a `std::sync::Mutex` held only for map operations and **never across an `await`**. A session's mapping is a `tokio::sync::Mutex`, because that one *is* held across the detector calls in Task 5. The acquisition order is fixed — store lock, clone the `Arc`, release, then await the session lock — which excludes deadlock by construction.

Eviction runs on acquisition rather than in a background task: at a thousand sessions a full sweep costs less than a syscall, and there is no separate task whose health becomes a new thing to reason about. Time is a parameter of the private entry point so the tests drive TTL and LRU without sleeping.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `gateway/src/session.rs`:

```rust
    use std::time::{Duration, Instant};

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

    #[test]
    fn the_same_key_returns_the_same_session() {
        let store = SessionStore::new(limits(4));
        let now = Instant::now();
        let one = store.acquire_at(&key("conv1", "Bearer k"), now);
        let two = store.acquire_at(&key("conv1", "Bearer k"), now);
        assert!(Arc::ptr_eq(&one, &two));
    }

    #[test]
    fn a_different_credential_is_a_different_session() {
        let store = SessionStore::new(limits(4));
        let now = Instant::now();
        let one = store.acquire_at(&key("shared", "Bearer k1"), now);
        let two = store.acquire_at(&key("shared", "Bearer k2"), now);
        assert!(!Arc::ptr_eq(&one, &two));
        assert_eq!(store.live(), 2);
    }

    #[test]
    fn a_session_idle_past_the_ttl_is_swept() {
        let store = SessionStore::new(limits(4));
        let start = Instant::now();
        let one = store.acquire_at(&key("conv1", "Bearer k"), start);
        let two = store.acquire_at(&key("conv1", "Bearer k"), start + Duration::from_secs(61));
        assert!(!Arc::ptr_eq(&one, &two), "a stale table was handed back");
        assert_eq!(store.live(), 1, "the swept entry was left behind");
    }

    #[test]
    fn a_full_store_evicts_the_least_recently_used() {
        let store = SessionStore::new(limits(2));
        let start = Instant::now();
        let a = store.acquire_at(&key("a", "Bearer k"), start);
        store.acquire_at(&key("b", "Bearer k"), start + Duration::from_secs(1));
        // Touching "a" makes "b" the oldest.
        store.acquire_at(&key("a", "Bearer k"), start + Duration::from_secs(2));
        store.acquire_at(&key("c", "Bearer k"), start + Duration::from_secs(3));

        assert_eq!(store.live(), 2);
        let a_again = store.acquire_at(&key("a", "Bearer k"), start + Duration::from_secs(4));
        assert!(Arc::ptr_eq(&a, &a_again), "the touched session was evicted");
    }

    #[tokio::test]
    async fn an_evicted_session_does_not_interrupt_a_request_holding_it() {
        let store = SessionStore::new(limits(1));
        let start = Instant::now();
        let held = store.acquire_at(&key("a", "Bearer k"), start);
        // Another conversation pushes "a" out of a store that holds one.
        store.acquire_at(&key("b", "Bearer k"), start + Duration::from_secs(1));
        // The request already holding the Arc finishes normally; its commit is
        // simply lost to a table nobody will look up again.
        held.mapping.lock().await.absorb(&Mapping::new(), 10);
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd gateway && cargo test session:: 2>&1 | tail -30`
Expected: compilation fails — `SessionStore` and `Limits` do not exist.

- [ ] **Step 3: Write the implementation**

Add to the top of `gateway/src/session.rs`, extending the imports:

```rust
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::mapping::Mapping;
```

And append to the file, above the test module:

```rust
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
    /// A tokio mutex because it is held across the detector calls that mask a
    /// request. It is released before the upstream call: a stream may run for
    /// minutes, and a lock that outlived one would block its session for as
    /// long as the stream did — forever, if the stream hung.
    pub mapping: tokio::sync::Mutex<Mapping>,
}

struct Entry {
    session: Arc<Session>,
    last_seen: Instant,
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

    pub fn acquire(&self, key: &SessionKey) -> Arc<Session> {
        self.acquire_at(key, Instant::now())
    }

    /// Time is a parameter so eviction is tested without sleeping.
    fn acquire_at(&self, key: &SessionKey, now: Instant) -> Arc<Session> {
        // Poisoning is recovered rather than propagated. Nothing in this
        // critical section can panic today, but if something ever does, one
        // failure must not turn every later request into a 500.
        let mut map = self
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner());

        // Sweeping here rather than in a background task: at a thousand
        // sessions this costs less than a syscall, and there is no separate
        // task whose health is a new thing to reason about.
        map.retain(|_, entry| now.duration_since(entry.last_seen) < self.limits.idle);

        if let Some(entry) = map.get_mut(key) {
            entry.last_seen = now;
            return Arc::clone(&entry.session);
        }

        if map.len() >= self.limits.max_sessions {
            let oldest = map
                .iter()
                .min_by_key(|(_, entry)| entry.last_seen)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                map.remove(&oldest);
            }
        }

        // A whole session is evicted; values within one never are. A value
        // dropped from a live session can come back from the model on the next
        // turn, and that is not a lost coreference but a request that dies with
        // nothing to restore to.
        let session = Arc::new(Session {
            mapping: tokio::sync::Mutex::new(Mapping::new()),
        });
        map.insert(
            key.clone(),
            Entry {
                session: Arc::clone(&session),
                last_seen: now,
            },
        );
        session
    }

    #[cfg(test)]
    pub(crate) fn live(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .len()
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cd gateway && cargo test session:: 2>&1 | tail -20`
Expected: PASS.

If `a_session_idle_past_the_ttl_is_swept` fails on the `live()` assertion, note that `acquire_at` sweeps *before* inserting, so after the sweep and the insert exactly one entry remains — the assertion is correct and the implementation is at fault.

- [ ] **Step 5: Check formatting and lints**

Run: `cd gateway && cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test 2>&1 | tail -10`
Expected: no warnings, everything passes.

- [ ] **Step 6: Commit**

```bash
git add gateway/src/session.rs
git commit -m "feat(gateway): a bounded store of session tables

An idle TTL swept on acquisition, an LRU cap on live sessions, and a cap
on values carried to the masking phase. No background task: at a thousand
sessions a full sweep is cheaper than the task would be to reason about.

A whole session is evicted; values within one never are. Evicting a
session costs coreference, evicting a value costs a request."
```

---

### Task 5: Wire the session into the proxy

**Files:**
- Modify: `gateway/src/proxy.rs:16-26` (`ProxyError`), `:28-40` (status mapping), `:68-94` (`AppState`), `:96-120` (the masking phase of `handle`), `:274-281` (the test `state` helper)
- Test: `gateway/src/proxy.rs`, the existing `mod tests`

**Interfaces:**
- Consumes: `Limits`, `Session`, `SessionStore`, `SessionError`, `SessionKey`, `key_from`, `SESSION_HEADER` from Tasks 3 and 4; `Mapping::absorb`, `Mapping::is_empty` from Task 1; `Config`'s three fields from Task 2.
- Produces: `AppState.sessions: SessionStore`; `ProxyError::Session`; `mask_all`.

The one structural change: the detect-and-mask loop moves out of `handle` into `mask_all`, because both branches call it and inline it would exist twice and diverge at the first edit.

Note for the implementer: the existing test helper `call_with_headers` builds a fresh `router` per call but takes an `Arc<AppState>`, so the store persists across calls made with the same state. That is what makes a two-turn test possible.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `gateway/src/proxy.rs`. First the helpers, beside the existing ones:

```rust
    fn test_limits() -> Limits {
        Limits {
            idle: Duration::from_secs(1800),
            max_sessions: 8,
            max_values: 8,
        }
    }

    fn state_with(
        detector: &MockServer,
        upstream: &MockServer,
        limits: Limits,
    ) -> Arc<AppState> {
        Arc::new(AppState {
            detector: DetectorClient::new(detector.uri(), Duration::from_secs(5)),
            upstream: reqwest::Client::new(),
            openai_base: upstream.uri(),
            anthropic_base: upstream.uri(),
            sessions: SessionStore::new(limits),
        })
    }

    fn session_headers<'a>(credential: &'a str, id: &'a str) -> [(&'a str, &'a str); 2] {
        [("authorization", credential), (SESSION_HEADER, id)]
    }

    fn test_key(id: &str, credential: &str) -> SessionKey {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", credential.parse().unwrap());
        headers.insert(HeaderName::from_static(SESSION_HEADER), id.parse().unwrap());
        key_from(&headers, &OpenAi, true).unwrap().unwrap()
    }

    /// A detector that finds "Weber" and nothing else. wiremock takes the
    /// first mount that matches, so the specific rule is mounted first.
    async fn detector_finding_weber() -> MockServer {
        use wiremock::matchers::body_string_contains;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .and(body_string_contains(SECRET))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"spans": person_span(), "layers_run": ["deterministic"]}),
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"spans": [], "layers_run": ["deterministic"]})),
            )
            .mount(&server)
            .await;
        server
    }
```

Then update the existing `state` helper to delegate:

```rust
    fn state(detector: &MockServer, upstream: &MockServer) -> Arc<AppState> {
        state_with(detector, upstream, test_limits())
    }
```

Then the tests:

```rust
    #[tokio::test]
    async fn one_session_keeps_one_value_on_one_placeholder() {
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;
        let state = state(&detector, &upstream);
        let headers = session_headers("Bearer k1", "conv-1");

        for _ in 0..2 {
            call_with_headers(
                Arc::clone(&state),
                "/v1/chat/completions",
                json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
                &headers,
            )
            .await;
        }

        let received = upstream.received_requests().await.unwrap();
        let first = String::from_utf8(received[0].body.clone()).unwrap();
        let second = String::from_utf8(received[1].body.clone()).unwrap();
        assert!(first.contains("[PERSON_1]"));
        assert!(
            second.contains("[PERSON_1]"),
            "the second turn renamed the same person: {second}"
        );
    }

    #[tokio::test]
    async fn a_guessed_session_id_returns_no_other_callers_value() {
        let detector = detector_finding_weber().await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "[PERSON_1]"}}]}),
        )
        .await;
        let state = state(&detector, &upstream);

        // The first caller puts Weber into the session called "shared".
        call_with_headers(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
            &session_headers("Bearer k1", "shared"),
        )
        .await;

        // A second caller guesses the id but holds a different key, and asks
        // the model to echo the placeholder back.
        let (status, body) = call_with_headers(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "[PERSON_1] wer?"}]}),
            &session_headers("Bearer k2", "shared"),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(
            !body.contains(SECRET),
            "another caller's value came back: {body}"
        );
        assert!(body.contains("[PERSON_1]"));
    }

    #[tokio::test]
    async fn a_refused_request_leaves_the_session_untouched() {
        // The first message masks; the second has no "Weber" and the detector
        // refuses it, so the request dies after masking has already happened.
        let detector = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .and(wiremock::matchers::body_string_contains(SECRET))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"spans": person_span(), "layers_run": ["deterministic"]}),
            ))
            .mount(&detector)
            .await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&detector)
            .await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;
        let state = state(&detector, &upstream);

        let (status, _) = call_with_headers(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [
                {"role": "user", "content": "Weber schreibt"},
                {"role": "user", "content": "und dann?"}
            ]}),
            &session_headers("Bearer k1", "conv-1"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);

        let session = state.sessions.acquire(&test_key("conv-1", "Bearer k1"));
        assert!(
            session.mapping.lock().await.is_empty(),
            "a refused request left values in the session"
        );
    }

    #[tokio::test]
    async fn a_value_past_the_cap_is_still_masked_and_still_restored() {
        let two_spans = json!([
            {"entity_type": "PERSON", "start": 0, "end": 5, "confidence": 1.0,
             "recognizer": "ner:fake", "tier": 2, "boosted": false},
            {"entity_type": "PERSON", "start": 10, "end": 15, "confidence": 1.0,
             "recognizer": "ner:fake", "tier": 2, "boosted": false}
        ]);
        let detector = detector_returning(two_spans).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant",
                   "content": "[PERSON_1] und [PERSON_2]"}}]}),
        )
        .await;
        let state = state_with(
            &detector,
            &upstream,
            Limits {
                idle: Duration::from_secs(1800),
                max_sessions: 8,
                max_values: 1,
            },
        );

        let (status, body) = call_with_headers(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber und Meier"}]}),
            &session_headers("Bearer k1", "conv-1"),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let sent = String::from_utf8(
            upstream.received_requests().await.unwrap()[0].body.clone(),
        )
        .unwrap();
        assert!(!sent.contains(SECRET), "the value past the cap went up raw");
        assert!(!sent.contains("Meier"), "the value past the cap went up raw");
        assert!(body.contains(SECRET) && body.contains("Meier"));

        let session = state.sessions.acquire(&test_key("conv-1", "Bearer k1"));
        assert_eq!(session.mapping.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn no_header_creates_no_session() {
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;
        let state = state(&detector, &upstream);

        let (status, _) = call(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            state.sessions.live(),
            0,
            "a request that asked for no session got one anyway"
        );
    }

    #[tokio::test]
    async fn a_session_header_against_a_disabled_gateway_is_refused() {
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;
        let state = state_with(
            &detector,
            &upstream,
            Limits {
                idle: Duration::ZERO,
                max_sessions: 0,
                max_values: 0,
            },
        );

        let (status, body) = call_with_headers(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
            &session_headers("Bearer k1", "conv-1"),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("disabled"), "{body}");
        assert_eq!(
            upstream.received_requests().await.unwrap().len(),
            0,
            "a refused request still reached the provider"
        );
    }

    #[tokio::test]
    async fn a_malformed_session_id_is_refused() {
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;

        let (status, _) = call_with_headers(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
            &session_headers("Bearer k1", "conv 1"),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            detector.received_requests().await.unwrap().len(),
            0,
            "a malformed header cost a detection pass"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd gateway && cargo test proxy:: 2>&1 | tail -30`
Expected: compilation fails — `AppState` has no field `sessions`, and `Limits`/`SessionStore`/`SESSION_HEADER` are not in scope.

- [ ] **Step 3: Extend `AppState` and the error type**

In `gateway/src/proxy.rs`, add to the imports:

```rust
use crate::session::{key_from, Limits, SessionStore};
```

`SESSION_HEADER` is used only by the tests, so it goes into the test module's own imports
instead — an unused import in the non-test build would fail `clippy -D warnings`. Add to
the top of `mod tests`:

```rust
    use crate::session::{SessionKey, SESSION_HEADER};
```

Add the variant to `ProxyError`:

```rust
    #[error("{0}")]
    Session(#[from] crate::session::SessionError),
```

And add it to the 400 arm of `IntoResponse`:

```rust
        let status = match self {
            // A body we cannot read is the client's to fix, and it is refused
            // rather than forwarded unmasked. So is a session the gateway
            // cannot honour as asked.
            ProxyError::Shape(ShapeError::Request(_))
            | ProxyError::Shape(ShapeError::Unsupported(_, _))
            | ProxyError::Session(_) => StatusCode::BAD_REQUEST,
            _ => StatusCode::BAD_GATEWAY,
        };
```

Add the field to `AppState`:

```rust
pub struct AppState {
    pub detector: DetectorClient,
    pub upstream: reqwest::Client,
    pub openai_base: String,
    pub anthropic_base: String,
    pub sessions: SessionStore,
}
```

And build it in `from_config`, after `anthropic_base`:

```rust
            sessions: SessionStore::new(Limits {
                idle: Duration::from_secs(config.session_idle_secs),
                max_sessions: config.max_sessions,
                max_values: config.max_session_values,
            }),
```

- [ ] **Step 4: Extract `mask_all`**

Add above `handle` in `gateway/src/proxy.rs`:

```rust
/// Detect and mask every text the provider pointed at. Shared by both branches
/// of `handle`: inline it would exist twice and diverge at the first edit.
async fn mask_all(
    detector: &DetectorClient,
    body: &Value,
    pointers: &[String],
    mapping: &mut Mapping,
) -> Result<Value, ProxyError> {
    let mut masked = body.clone();
    for pointer in pointers {
        let text = read_pointer(body, pointer)?;
        let spans = detector.detect(&text).await?;
        write_pointer(&mut masked, pointer, &mapping.mask(&text, &spans)?)?;
    }
    Ok(masked)
}
```

- [ ] **Step 5: Rewrite the masking phase of `handle`**

Replace lines 102-112 of `gateway/src/proxy.rs` — from the `// Where is the text?` comment through the end of the masking loop — with:

```rust
    // Where is the text? A shape we do not recognize is refused, not forwarded.
    let pointers = provider.request_pointers(&body)?;

    // Resolved before detection: a malformed header must cost nothing, not a
    // second per 1 200 characters.
    let key = key_from(&headers, provider, state.sessions.enabled())?;

    // One mapping for the whole request so a value keeps one name; seeded from
    // the conversation's table so it keeps that name across turns too.
    let (masked, mapping) = match key {
        Some(key) => {
            let session = state.sessions.acquire(&key);
            let mut guard = session.mapping.lock().await;
            let mut work = guard.clone();
            let masked = mask_all(&state.detector, &body, &pointers, &mut work).await?;
            // After the last `?`, and on a copy until here: a refused request
            // leaves the session exactly as it was, so a client whose detector
            // blinked does not carry a hole in its numbering for the rest of
            // the conversation.
            guard.absorb(&work, state.sessions.max_values());
            (masked, work)
            // `guard` is dropped here — before the upstream call, so a stream
            // that runs for minutes holds no lock on its session.
        }
        None => {
            let mut work = Mapping::new();
            let masked = mask_all(&state.detector, &body, &pointers, &mut work).await?;
            (masked, work)
        }
    };
```

The rest of `handle` is unchanged: it already refers to `masked` for the upstream body and `mapping` for restoration.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cd gateway && cargo test 2>&1 | tail -30`
Expected: PASS, including every pre-existing proxy and stream test.

If `no_header_creates_no_session` fails to compile because `live()` is `#[cfg(test)]` and `pub(crate)`, confirm Task 4 Step 3 used exactly that attribute pair — the proxy tests are in the same crate and compile under `cfg(test)`, so it resolves.

- [ ] **Step 7: Check formatting and lints**

Run: `cd gateway && cargo fmt && cargo clippy --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 8: Commit**

```bash
git add gateway/src/proxy.rs
git commit -m "feat(gateway): a conversation gets one mapping

The session lock is held for the detect-and-mask phase and released
before the upstream call; restoration works from the snapshot that did
the masking. Merging a request-local copy back afterwards was rejected in
the design: two concurrent requests would allocate one placeholder to two
values and restore the wrong name.

absorb runs after the last fallible step and on a copy until then, so a
refused request leaves the session as it was."
```

---

### Task 6: A stream holds no session lock, and the documentation

**Files:**
- Test: `gateway/src/proxy.rs`, the existing `mod tests`
- Modify: `gateway/tessera.example.toml`, `README.md`

**Interfaces:**
- Consumes: everything from Tasks 1-5. Produces nothing new.

The claim that the session lock is released before the upstream call is the one that keeps a hung stream from blocking its conversation forever. Racing two requests to prove it would be a flaky test that proves nothing on a fast machine; the structural fact is testable directly. The existing helpers `upstream_streaming` (`proxy.rs:617`) and `STREAM_BODY` (`proxy.rs:630`) are what this needs.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `gateway/src/proxy.rs`:

```rust
    #[tokio::test]
    async fn a_stream_holds_no_session_lock() {
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_streaming(STREAM_BODY).await;
        let state = state(&detector, &upstream);

        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .header("authorization", "Bearer k1")
            .header(SESSION_HEADER, "conv-1")
            .body(Body::from(
                json!({"model": "gpt", "stream": true,
                       "messages": [{"role": "user", "content": "Weber schreibt"}]})
                .to_string(),
            ))
            .unwrap();
        let response = router(Arc::clone(&state)).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // The stream has not been read yet. If the masking guard were still
        // held, this would fail — and a stream that hung would block its
        // conversation for as long as it hung.
        let session = state.sessions.acquire(&test_key("conv-1", "Bearer k1"));
        assert!(
            session.mapping.try_lock().is_ok(),
            "the stream is holding its session lock"
        );

        // Draining afterwards proves the stream still restores from the
        // snapshot it was handed.
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let served = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(served.contains(SECRET), "the stream did not restore: {served}");
    }
```

- [ ] **Step 2: Run the test to verify it passes**

Run: `cd gateway && cargo test a_stream_holds_no_session_lock -- --nocapture 2>&1 | tail -20`
Expected: PASS. Unlike the earlier tasks this test is written against behaviour Task 5 already implements — it is a regression guard on the design's central claim. If it fails, the guard in `handle` is outliving its match arm; that is a bug in Task 5 Step 5, not in this test.

- [ ] **Step 3: Document the configuration**

Append to `gateway/tessera.example.toml`:

```toml
# A conversation keeps one value on one placeholder when the client sends
# X-Tessera-Session. The table holds real values in memory between requests, so
# it is bounded three ways: how long a conversation survives without a request,
# how many conversations may hold a table, and how many values one may hold.
# Set session_idle_secs = 0 to disable sessions outright — personal data then
# never outlives the request that carried it, and a request that asks for a
# session is refused rather than quietly served without one.
session_idle_secs = 1800
max_sessions = 1000
max_session_values = 1000
```

- [ ] **Step 4: Document the behaviour**

In `README.md`, add a `### Sessions` subsection under `## Gateway`, immediately before `### Streaming`:

```markdown
### Sessions

Within a request an identical value always gets the same placeholder. Across the turns of
a conversation it does too, if the client sends `X-Tessera-Session: <id>` — otherwise each
request gets its own table, which is the behaviour without the header.

The id does not select a session on its own. A session table is a restoration oracle: put
`[PERSON_1]` in a prompt, get it echoed by the model, and the gateway would restore it to
a real name on the way back. So the store keys on a salted fingerprint of the caller's own
credential as well as the id, and a guessed id lands in an empty namespace. The raw id
never reaches a log either — a client may well name its session after the person in it.

The table holds real values in memory between requests, which nothing else in the gateway
does, so it is bounded three ways: `session_idle_secs`, `max_sessions` and
`max_session_values`. Reaching a bound costs coreference, never protection. The client
holds restored text and sends the history again, so a session that was evicted is rebuilt
from scratch by the next request — `[PERSON_3]` becomes `[PERSON_1]` and nothing else
changes. Past `max_session_values` a value is still masked and still restored; it is
simply not remembered. Values are never evicted from within a live session: one that came
back from the model would end a request with nothing to restore to.

Detection still runs over every text in every request. The session stabilises a
placeholder that detection produced; it is never asked to find personal data on its own,
because personal data does not arrive in the same form twice.

A request refused for any reason leaves its session exactly as it was. Asking for a
session the gateway cannot honour — a malformed id, no credential to namespace it, or
`session_idle_secs = 0` — is refused before the detector runs rather than served without
the coreference it asked for.
```

- [ ] **Step 5: Run the whole suite one last time**

Run: `cd gateway && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test 2>&1 | tail -20`
Expected: everything passes with no warnings.

- [ ] **Step 6: Commit**

```bash
git add gateway/src/proxy.rs gateway/tessera.example.toml README.md
git commit -m "test(gateway): a stream holds no session lock, and document sessions

Racing two requests would prove nothing on a fast machine; that the guard
is released before the upstream call is testable directly."
```

- [ ] **Step 7: Open the pull request**

```bash
git push -u origin feat/session-mapping
gh pr create --title "Gateway: session-scoped mapping" --body "$(cat <<'EOF'
Slice D. A conversation gets one value-to-placeholder table, so a value keeps
its placeholder across turns rather than only within one request.

Design: `docs/superpowers/specs/2026-08-07-session-mapping-design.md`.

- `X-Tessera-Session` names the conversation; the store keys on a salted
  fingerprint of the caller's credential as well, because a session table is a
  restoration oracle and an id alone would let a guessed id read another
  caller's values out of it.
- The session lock is held for the detect-and-mask phase and released before
  the upstream call. Restoration works from the snapshot that did the masking,
  so a stream holds no lock however long it runs.
- Three bounds: idle TTL, live sessions, values per session. Reaching one costs
  coreference, never protection.
- A refused request leaves its session untouched.

Detection still runs over every text in every request; the session never
substitutes for it.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Then comment `@codex review` on the pull request.

---

## Self-Review

**Spec coverage.** Every section of the design maps to a task: the driver and the
non-goal to the README text in Task 6 and to the constraint at the top; session identity
and the oracle argument to Task 3; the lock split and the rejected alternative to Task 5
and its regression guard in Task 6; components to Tasks 3-5; lifetime, eviction, the value
cap and the no-partial-eviction rule to Tasks 2 and 4; data flow and "a refused request
leaves no trace" to Task 5; the three 400s to Tasks 3 and 5; configuration to Task 2;
every named test to Tasks 1, 3, 4, 5 and 6; out-of-scope items to no task, deliberately.

**Type consistency.** `Limits { idle, max_sessions, max_values }` is spelled the same in
Tasks 4, 5 and 6 — note that the config keys are `max_session_values` while the `Limits`
field is `max_values`, and Task 5 Step 3 is where the two meet. `SessionStore::acquire`
is public and `acquire_at` is private, so Task 4's tests (same module) use `acquire_at`
and Task 5's tests (another module) use `acquire`. `live()` is `#[cfg(test)] pub(crate)`,
which is what lets Task 5 call it from `proxy.rs`. `Mapping::len`/`is_empty` are used in
Task 5's cap and refusal tests exactly as Task 1 defines them.
