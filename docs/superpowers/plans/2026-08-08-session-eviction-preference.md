# Session Eviction Preference Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When `SessionStore` must evict to make room for a new session, prefer an already-reclaimable entry over the globally-oldest one, and close a race in which an unrelated eviction can steal a session out from under the request it was just handed to.

**Architecture:** `Session.mapping` becomes `Arc<tokio::sync::Mutex<Mapping>>` so a session's lock can be claimed with an owned, non-blocking `try_lock_owned()`. `SessionStore::acquire`/`acquire_at` claim that lock synchronously, inside the same critical section that decides whether to hand a session back or evict to make room, and return a `Claimed { session, guard }` instead of a bare `Arc<Session>`. `proxy.rs`'s `handle()` uses the pre-claimed guard when it has one, waiting normally only when another request already holds it.

**Tech Stack:** Rust 2021, existing `gateway` crate. Tests are `cargo test`, no new dependencies.

## Global Constraints

- The spec is `docs/superpowers/specs/2026-08-08-session-eviction-preference-design.md`. Where this plan and the spec disagree, the spec wins — stop and ask.
- Work on branch `fix/session-eviction-preference`, which already exists (PR #15, draft) and already carries two spec commits: the original design, and a revision that closed a TOCTOU race Codex found in the first draft during review. Read the spec's "Revision history" section before starting — it explains why the mechanism below looks different from what a first read of "prefer an empty session" might suggest.
- **Never call `.lock()` or `.lock_owned()` on a session's mapping while a `Claimed.guard` for that same session is still alive.** `tokio::sync::Mutex` is not reentrant; doing so deadlocks the test (or the request) rather than erroring. Every step below that needs to both hold a claimed guard and read/write the mapping does so through the guard directly (`guard.mask(...)`, `guard.absorb(...)`, `guard.is_empty()`), never by calling `.lock()` again on the side.
- The store's `std::sync::Mutex` critical section in `acquire_at` must still never contain an `.await`. `try_lock_owned()` is non-blocking and does not violate this.
- `last_seen` refresh on an existing key stays unconditional, win or lose — not touched by this fix.
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test` must all pass before each commit. Run them from `gateway/`. Because `SessionStore::acquire`'s return type changes, `session.rs` and `proxy.rs` must be updated together — the crate does not compile with only one of them changed, so there is no way to split this into separately-committable pieces smaller than Task 1 below.

## File Structure

| File | Responsibility |
|---|---|
| `gateway/src/session.rs` | **Modify.** `Session.mapping` becomes `Arc<Mutex<Mapping>>`; new `Claimed` struct; `reclaimable` helper; `acquire`/`acquire_at` claim synchronously and return `Claimed`; existing tests adjusted for the new return type; new tests proving the race is closed and the eviction preference works. |
| `gateway/src/mapping.rs` | **Modify.** Remove `#[allow(dead_code)]` from `Mapping::is_empty()` — this fix gives it its first live caller. |
| `gateway/src/proxy.rs` | **Modify.** `handle()`'s masking phase destructures `Claimed` instead of calling `.mapping.lock().await` directly; two existing tests' session-inspection lines adjusted for the new return type; one new integration test. |

Two tasks. Task 1 is the mechanism itself — it must include every change needed for the crate to compile and its existing tests to keep passing, because `acquire`'s return type is a breaking change to both files at once. Task 2 adds the integration-level proof that a real failing request, through the full `handle()` path, cannot destroy a different conversation's committed value.

---

### Task 1: Claim a session's lock synchronously; prefer a reclaimable victim

**Files:**
- Modify: `gateway/src/session.rs` (throughout — the struct, the store, and its `mod tests`)
- Modify: `gateway/src/mapping.rs:56-58` (drop `#[allow(dead_code)]` from `is_empty`)
- Modify: `gateway/src/proxy.rs:139-153` (the masking phase's `Some(key)` arm), and two existing tests at `gateway/src/proxy.rs:1314` and `:1365` (adjusted only enough to keep compiling and passing — Task 2 adds the new integration test)

**Interfaces:**
- Consumes: `Mapping::is_empty(&self) -> bool`, `Mapping::mask`, `Mapping::absorb` (all already exist, unchanged).
- Produces:
  - `pub struct Claimed { pub session: Arc<Session>, pub guard: Option<tokio::sync::OwnedMutexGuard<Mapping>> }`
  - `SessionStore::acquire(&self, &SessionKey) -> Claimed` (was `-> Arc<Session>`)
  - `Session { pub mapping: Arc<tokio::sync::Mutex<Mapping>> }` (was `pub mapping: tokio::sync::Mutex<Mapping>`)

Task 2 consumes `Claimed`'s `.session` field, exactly as this task leaves it.

- [ ] **Step 1: Write the failing tests**

These are written against the target API and will not compile until Step 2 lands — that is the "RED" for this task; there is no meaningful intermediate compiling-but-failing state, because the API itself is changing.

Replace the whole `mod tests` block in `gateway/src/session.rs` from `fn limits(...)` (currently around line 402) through the end of the file with the following — every existing test below is adjusted for `Claimed`, and five new tests are added. Nothing before `fn limits` in the test module changes (the `headers`, `key_from`-focused tests above it are untouched).

```rust
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
```

- [ ] **Step 2: Run the tests to verify they fail to compile**

Run: `cd gateway && cargo test session:: 2>&1 | tail -40`
Expected: compilation fails — `SessionStore::acquire_at` returns `Arc<Session>`, not something with `.session`/`.guard` fields; `Claimed` does not exist yet.

- [ ] **Step 3: Change `Session` and add `Claimed`**

In `gateway/src/session.rs`, replace the `Session` struct:

```rust
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
```

Add, near it:

```rust
/// What `acquire` hands back: the session, and — when the store could claim
/// it synchronously, in the same critical section that decided this session
/// was safe to return — the lock already held. `None` means someone else
/// currently holds it; the caller waits its turn with `lock_owned().await`,
/// which is already-proven-safe serialization for two requests to one key.
pub struct Claimed {
    pub session: Arc<Session>,
    pub guard: Option<tokio::sync::OwnedMutexGuard<Mapping>>,
}
```

- [ ] **Step 4: Add the `reclaimable` helper and rewrite `acquire_at`**

Add above `impl SessionStore`:

```rust
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
```

Replace `acquire` and `acquire_at`:

```rust
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
```

The `live()` test helper below `acquire_at` is unchanged — it only counts map entries, unaffected by the return type of `acquire`.

- [ ] **Step 5: Update `proxy.rs`'s masking phase**

In `gateway/src/proxy.rs`, replace the `Some(key)` arm of the masking-phase `match` (currently lines 140-153):

```rust
        Some(key) => {
            let claimed = state.sessions.acquire(&key);
            let mut guard = match claimed.guard {
                Some(guard) => guard,
                None => Arc::clone(&claimed.session.mapping).lock_owned().await,
            };
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
```

No import changes are needed: `Arc` is already imported at `proxy.rs:1`, and neither `Claimed` nor `OwnedMutexGuard` needs to be named explicitly (both are used only through field access and `match`, which Rust resolves without the type being in scope).

- [ ] **Step 6: Update the two existing `proxy.rs` tests that inspect a session directly**

At `gateway/src/proxy.rs:1314` (inside `a_refused_request_leaves_the_session_untouched`):

```rust
        let session = state.sessions.acquire(&test_key("conv-1", "Bearer k1")).session;
        assert!(
            session.mapping.lock().await.is_empty(),
            "a refused request left values in the session"
        );
```

At `gateway/src/proxy.rs:1365` (inside `a_value_past_the_cap_is_still_masked_and_still_restored`):

```rust
        let session = state.sessions.acquire(&test_key("conv-1", "Bearer k1")).session;
        assert_eq!(session.mapping.lock().await.len(), 1);
```

Both are safe from the deadlock this task's Global Constraints warn about: the `Claimed` value these lines produce is a temporary — its `.guard` (if any) is dropped at the end of the `let` statement, before the next line locks the mapping fresh.

- [ ] **Step 7: Remove the stale `#[allow(dead_code)]` from `Mapping::is_empty`**

In `gateway/src/mapping.rs`, delete the `#[allow(dead_code)]` line directly above `pub fn is_empty(&self) -> bool {` (around line 61). `reclaimable` (Step 4) is now a live caller.

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cd gateway && cargo test session:: 2>&1 | tail -30`
Expected: PASS, all nine tests (six adjusted or pre-existing, three new — `a_reclaimable_entry_is_evicted_before_a_live_one`, `a_freshly_claimed_session_is_never_treated_as_the_reclaimable_candidate`, `a_session_held_by_another_request_is_never_treated_as_the_reclaimable_candidate`).

- [ ] **Step 9: Run the full suite, formatting and lints**

Run: `cd gateway && cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test 2>&1 | tail -20`
Expected: no warnings, everything passes — this exercises `proxy.rs`'s Step 5 and Step 6 changes too, since they're part of the same crate. If `Mapping::is_empty` is still flagged as unused, `reclaimable` (Step 4) was not wired correctly.

- [ ] **Step 10: Commit**

```bash
git add gateway/src/session.rs gateway/src/mapping.rs gateway/src/proxy.rs
git commit -m "fix(gateway): claim a session's lock synchronously, prefer reclaiming empties

SessionStore::acquire released the store's own lock before the caller
took the session's — a real, exploitable window on a multi-threaded
runtime, in which an unrelated eviction could see the session as
unlocked and steal it out from under the request it was just handed to.
A third request for the same id would then create a second, unsynced
session for one conversation: the exact placeholder-collision race the
session-mapping design excluded, reintroduced by an unrelated request's
eviction choice.

Session.mapping is now Arc<Mutex<Mapping>>, so acquire can claim it with
a non-blocking try_lock_owned inside the same critical section that
decides whether to hand a session back — a session is locked
continuously from the moment it is handed out until its holder releases
it, with no window in between.

Eviction, when needed, now also prefers a reclaimable (idle, empty)
victim over the globally-oldest one, so a request that fails during
masking can no longer destroy a live conversation to make room for
itself in the common case. A store genuinely saturated with live
sessions can still lose its oldest one to a doomed request — an
accepted, scoped limitation, not this fix's target."
```

---

### Task 2: Prove a failing request cannot evict a live third-party session

**Files:**
- Test: `gateway/src/proxy.rs`, the existing `mod tests`

**Interfaces:**
- Consumes: `SessionStore`, `Limits` (already imported in `proxy.rs`'s test module); `state_with`, `test_limits`, `test_key`, `session_headers`, `call_with_headers`, `person_span` (all existing test helpers, unchanged by Task 1).
- Produces: nothing new — this is the last task in the plan.

Task 1 proves the eviction *policy* and the *claim* mechanism correct in isolation. This task proves the thing Codex's original finding on PR #14 actually described — that a failing real HTTP request, through the full `handle()` path, no longer destroys another conversation's committed value — using the store the same way a live gateway does, not through `acquire_at` directly.

- [ ] **Step 1: Write the test**

Add to `mod tests` in `gateway/src/proxy.rs`, near `a_refused_request_leaves_the_session_untouched` (they test adjacent properties of the same mechanism, and this one reuses its detector-mock shape: a 200 with `person_span()` for text containing `SECRET`, a 503 for anything else).

```rust
    #[tokio::test]
    async fn a_failing_request_does_not_evict_a_live_third_party_session() {
        // Session "a" commits a real value through an ordinary successful
        // request. Session "b" then takes the store's second slot with a
        // request that fails during masking, leaving its entry created but
        // empty — `acquire` runs, and creates the entry, before `mask_all`
        // can fail. A third session's request also fails during masking,
        // and needs a slot in a now-full store: it must evict "b"
        // (reclaimable), never "a" (live) — even though "a" is the older
        // of the two by last_seen.
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
        let state = state_with(
            &detector,
            &upstream,
            Limits {
                idle: Duration::from_secs(1800),
                max_sessions: 2,
                max_values: 8,
            },
        );

        // "a" gets a real, committed value.
        call_with_headers(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
            &session_headers("Bearer k1", "a"),
        )
        .await;

        // "b" has no "Weber" in it, so the detector 503s and the request
        // fails — but its session entry was already created, and stays
        // empty and reclaimable.
        let (status_b, _) = call_with_headers(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "und dann?"}]}),
            &session_headers("Bearer k1", "b"),
        )
        .await;
        assert_eq!(status_b, StatusCode::BAD_GATEWAY);

        // "c" also fails during masking, and needs a slot in a store
        // already holding {a, b} at its cap of 2.
        let (status_c, _) = call_with_headers(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "und dann?"}]}),
            &session_headers("Bearer k1", "c"),
        )
        .await;
        assert_eq!(status_c, StatusCode::BAD_GATEWAY);

        // Session "a" must still hold Weber's placeholder.
        let session_a = state.sessions.acquire(&test_key("a", "Bearer k1")).session;
        assert_eq!(
            session_a.mapping.lock().await.restore("[PERSON_1]").unwrap(),
            "Weber",
            "a failing request for a different session evicted a's live value"
        );
    }
```

- [ ] **Step 2: Run the test to verify it passes**

This is a regression guard on behavior Task 1 already implemented and unit-tested — like the streaming-lock test in the original session-mapping plan, it is expected to PASS on its first run, not fail first; the API it exercises did not exist before Task 1, so there is no meaningful pre-fix version of this exact test to run.

Run: `cd gateway && cargo test a_failing_request_does_not_evict_a_live_third_party_session -- --nocapture 2>&1 | tail -20`
Expected: PASS. If it fails, Task 1's fix does not close the finding end to end even though its own unit tests passed — stop and report BLOCKED with the failure output rather than adjusting this test to make it pass.

- [ ] **Step 3: Run the full suite, formatting and lints**

Run: `cd gateway && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test 2>&1 | tail -10`
Expected: no warnings, everything passes.

- [ ] **Step 4: Commit**

```bash
git add gateway/src/proxy.rs
git commit -m "test(gateway): a failing request cannot evict another session's value

Task 1 fixed the eviction policy and the claim mechanism in isolation,
with their own unit tests. This is the integration-level proof: a real
request that fails during masking, through the full handle() path, no
longer destroys a different conversation's committed value. A
regression guard on already-delivered behavior, so it passes on its
first run rather than needing a RED phase."
```

- [ ] **Step 5: Push and update the PR**

```bash
git push
```

PR #15 already exists (draft) against `main`, and already carries both spec commits. Pushing adds Task 1's and Task 2's commits to it. Do not mark it ready for review or merge — report back to the controller with the commit range for review, and note that both of Codex's PR #15 findings (the reclaimable-preference gap and the TOCTOU race) are addressed by name in the two design-doc commits and in this implementation, so the controller can point Codex back at the updated PR.

---

## Self-Review

**Spec coverage.** The spec's structural fix (`Arc<Mutex<Mapping>>`, synchronous `try_lock_owned` claim inside `acquire_at`'s critical section, `Claimed` return type) maps to Task 1 Steps 3-5. The `reclaimable` victim-selection logic (finding 1's fix) maps to Task 1 Step 4 and its `a_reclaimable_entry_is_evicted_before_a_live_one` test. The TOCTOU closure (finding 2's fix) maps to Task 1's `a_freshly_claimed_session_is_never_treated_as_the_reclaimable_candidate` and `a_session_held_by_another_request_is_never_treated_as_the_reclaimable_candidate` tests, which the spec's Testing section calls the direct proof. The explicit non-change (`last_seen` refresh stays unconditional) is called out in Global Constraints so no task accidentally touches it. `Mapping::is_empty()` losing its `#[allow(dead_code)]` is Task 1 Step 7. The integration-level proof the spec's Testing section calls for is Task 2. Out-of-scope items (per-credential quotas, byte bounds, Anthropic session-path coverage) have no task, deliberately.

**Type consistency.** `Claimed { session: Arc<Session>, guard: Option<OwnedMutexGuard<Mapping>> }` is defined once (Task 1 Step 3) and consumed identically everywhere it appears afterward: Task 1's own tests, `proxy.rs`'s `handle()`, `proxy.rs`'s two adjusted existing tests, and Task 2's new test all access `.session` and `.guard` the same way, and every place that holds a `.guard` operates on the mapping through that guard directly rather than calling `.lock()` again — the deadlock this plan's Global Constraints warn about doesn't appear in any step's code. `reclaimable(entry: &Entry) -> bool` is defined once (Task 1 Step 4) and called once, from inside `acquire_at` in the same file.

**Corrections from the first draft of this plan.** The first draft assumed only `session.rs` would change and `proxy.rs` would gain only a new test; that was written before Codex's TOCTOU finding, which requires `acquire`'s return type to change, which in turn requires `proxy.rs`'s `handle()` to change in the same commit for the crate to compile. The first draft also proposed a `probably_empty` check using the plain (non-owned) `try_lock()` API and a simpler `an_empty_entry_is_evicted_before_a_live_one` test that manually re-locked a session's mapping after obtaining it from `acquire_at` — under the corrected `Claimed`-returning API, doing that while a claimed guard for the same session is still held would deadlock rather than merely be redundant, which is why every test above that both holds a guard and touches the mapping does so through the guard directly, and why the Global Constraints call this out explicitly rather than leaving it implicit.
