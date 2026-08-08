# Session Eviction Preference Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When `SessionStore` must evict to make room for a new session, prefer an already-empty entry over the globally-oldest one, so a request that fails during masking can no longer destroy another conversation's live session.

**Architecture:** One function changes: `SessionStore::acquire_at` in `gateway/src/session.rs`. The atomic check-or-insert under the store's `std::sync::Mutex` is untouched — only the victim-selection step inside the existing capacity check changes, using a new non-blocking `probably_empty` helper. No new files, no new config, no change to `proxy.rs`'s request-handling logic — only a new test proving the fix closes the actual finding.

**Tech Stack:** Rust 2021, existing `gateway` crate. Tests are `cargo test`, no new dependencies.

## Global Constraints

- The spec is `docs/superpowers/specs/2026-08-08-session-eviction-preference-design.md`. Where this plan and the spec disagree, the spec wins — stop and ask.
- Work on branch `fix/session-eviction-preference`, which already exists and already carries the spec commit (PR #15, draft).
- The store's `std::sync::Mutex` critical section in `acquire_at` must still never contain an `.await`. `try_lock()` on the session's `tokio::sync::Mutex` is non-blocking and does not violate this.
- A session's `tokio::sync::Mutex` is held by `handle()` continuously from before masking starts through `absorb()`, so a concurrent `try_lock()` on it fails for that entire window — this is the property the fix's correctness rests on, not new machinery to build.
- `last_seen` refresh on an existing key stays unconditional, win or lose — not touched by this fix.
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test` must all pass before each commit. Run them from `gateway/`.

## File Structure

| File | Responsibility |
|---|---|
| `gateway/src/session.rs` | **Modify.** `probably_empty` helper; victim selection inside `acquire_at`'s capacity check; three tests. |
| `gateway/src/mapping.rs` | **Modify.** Remove `#[allow(dead_code)]` from `Mapping::is_empty()` — this fix gives it its first live caller. |
| `gateway/src/proxy.rs` | **Modify.** One integration test proving the finding is closed end to end. |

Two tasks. Task 1 is the fix and its unit-level proof; Task 2 is the integration-level proof that the actual bug — not just its unit abstraction — is closed.

---

### Task 1: Prefer an empty victim over the globally-oldest one

**Files:**
- Modify: `gateway/src/session.rs:187-229` (`acquire_at`, and the new helper above it)
- Modify: `gateway/src/mapping.rs:56-58` (drop `#[allow(dead_code)]` from `is_empty`)
- Test: `gateway/src/session.rs`, the existing `mod tests`

**Interfaces:**
- Consumes: `Mapping::is_empty(&self) -> bool` (already exists, `mapping.rs:62`); `Session { pub mapping: tokio::sync::Mutex<Mapping> }` (already exists, `session.rs:148`).
- Produces: no new public interface — `probably_empty` is a private free function in `session.rs`. Task 2 does not call anything new; it only observes the changed behavior through `SessionStore::acquire`.

Background the implementer needs: `acquire_at` (`session.rs:188-229`) already sweeps stale entries, returns early on a cache hit, and otherwise evicts-if-full then inserts. The eviction block, today, is:

```rust
if map.len() >= self.limits.max_sessions {
    let oldest = map
        .iter()
        .min_by_key(|(_, entry)| entry.last_seen)
        .map(|(key, _)| key.clone());
    if let Some(oldest) = oldest {
        map.remove(&oldest);
    }
}
```

This picks the globally-oldest entry regardless of whether it holds real values. The fix changes only which entry `min_by_key` runs over: entries whose `Mapping` is currently empty, falling back to all entries only if none is empty.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `gateway/src/session.rs`, after `a_full_store_evicts_the_least_recently_used` (which stays exactly as it is — it is an implicit regression guard for this change, since its entries are never written to and it must keep passing unmodified):

```rust
    #[tokio::test]
    async fn an_empty_entry_is_evicted_before_a_live_one() {
        let store = SessionStore::new(limits(2));
        let start = Instant::now();

        // "a" becomes a live session: give it a real value.
        let a = store.acquire_at(&key("a", "Bearer k"), start);
        a.mapping
            .lock()
            .await
            .mask(
                "Weber",
                &[crate::mapping::Span {
                    entity_type: "PERSON".into(),
                    start: 0,
                    end: 5,
                }],
            )
            .unwrap();

        // "b" is created but never used — it stays empty. It is also newer
        // than "a" by last_seen, so plain oldest-first would pick "a" here,
        // not "b".
        store.acquire_at(&key("b", "Bearer k"), start + Duration::from_secs(1));

        // A third, new key needs a slot in a store already at its cap of 2.
        store.acquire_at(&key("c", "Bearer k"), start + Duration::from_secs(2));

        assert_eq!(store.live(), 2);
        let a_again = store.acquire_at(&key("a", "Bearer k"), start + Duration::from_secs(3));
        assert!(
            Arc::ptr_eq(&a, &a_again),
            "the live session was evicted ahead of the empty one"
        );
    }

    #[tokio::test]
    async fn a_locked_session_is_never_treated_as_the_empty_candidate() {
        let store = SessionStore::new(limits(2));
        let start = Instant::now();

        let a = store.acquire_at(&key("a", "Bearer k"), start);
        store.acquire_at(&key("b", "Bearer k"), start + Duration::from_secs(1));
        // Both "a" and "b" are empty. Hold "a"'s lock, simulating a request
        // mid-masking — its own emptiness must not make it a preferred
        // victim while a concurrent caller is using it, even though it is
        // also the older of the two by last_seen.
        let guard = a.mapping.lock().await;

        store.acquire_at(&key("c", "Bearer k"), start + Duration::from_secs(2));
        assert_eq!(store.live(), 2);

        drop(guard);
        let a_again = store.acquire_at(&key("a", "Bearer k"), start + Duration::from_secs(3));
        assert!(
            Arc::ptr_eq(&a, &a_again),
            "a locked-but-empty session was evicted while in use"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd gateway && cargo test session:: 2>&1 | tail -30`
Expected: `an_empty_entry_is_evicted_before_a_live_one` FAILs its `Arc::ptr_eq` assertion — today's code evicts "a" (the oldest) to make room for "c", so `a_again` is a freshly created session, not the original. `a_locked_session_is_never_treated_as_the_empty_candidate` FAILs the same way, for the same underlying reason (today's code has no concept of "locked" or "empty" at all, it only looks at age).

- [ ] **Step 3: Add the helper and change the eviction selection**

In `gateway/src/session.rs`, add above `impl SessionStore`:

```rust
/// Non-blocking: a session whose lock is currently held (mid-masking, by
/// definition not a candidate here) is conservatively treated as not empty,
/// never as a false "empty."
fn probably_empty(entry: &Entry) -> bool {
    entry
        .session
        .mapping
        .try_lock()
        .map(|guard| guard.is_empty())
        .unwrap_or(false)
}
```

Replace the eviction block inside `acquire_at` (`session.rs:204-212`):

```rust
        if map.len() >= self.limits.max_sessions {
            // Prefer a victim that is already empty — nobody loses a live
            // conversation to make room for a request that turns out not to
            // need the space. Fall back to the globally-oldest entry only
            // when every entry currently holds something.
            let victim = map
                .iter()
                .filter(|(_, entry)| probably_empty(entry))
                .min_by_key(|(_, entry)| entry.last_seen)
                .or_else(|| map.iter().min_by_key(|(_, entry)| entry.last_seen))
                .map(|(key, _)| key.clone());
            if let Some(victim) = victim {
                map.remove(&victim);
            }
        }
```

- [ ] **Step 4: Remove the stale `#[allow(dead_code)]` from `Mapping::is_empty`**

In `gateway/src/mapping.rs`, `is_empty` (around line 61-63) currently reads:

```rust
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
```

Delete the `#[allow(dead_code)]` line above it. `probably_empty` (Step 3) is now a live caller.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cd gateway && cargo test session:: 2>&1 | tail -20`
Expected: PASS, including `a_full_store_evicts_the_least_recently_used` unmodified — confirm this explicitly, since it is the regression guard proving the fallback path still matches today's behavior when nothing is empty.

- [ ] **Step 6: Check formatting and lints**

Run: `cd gateway && cargo fmt && cargo clippy --all-targets -- -D warnings`
Expected: no warnings. If clippy still flags `Mapping::is_empty` as unused, `probably_empty` was not wired correctly in Step 3 — check it is actually called from `acquire_at`, not just defined.

- [ ] **Step 7: Run the full suite once**

Run: `cd gateway && cargo test 2>&1 | tail -10`
Expected: all tests pass (this only touches `session.rs` and `mapping.rs`, so nothing in `proxy.rs` or `stream.rs` should be affected).

- [ ] **Step 8: Commit**

```bash
git add gateway/src/session.rs gateway/src/mapping.rs
git commit -m "fix(gateway): prefer an empty session over a live one when evicting

SessionStore::acquire evicted the globally-oldest entry whenever the
store was full, without regard to whether it held a real conversation's
values. A request that then failed during masking had already destroyed
someone else's session for nothing.

Eviction now prefers an already-empty entry, found via a non-blocking
try_lock so a session mid-masking — which holds its lock throughout —
can never be misjudged as empty. Falls back to the globally-oldest entry
only when nothing empty exists, matching today's behavior exactly in
that case."
```

---

### Task 2: Prove the actual finding is closed, not just its unit-level shape

**Files:**
- Test: `gateway/src/proxy.rs`, the existing `mod tests`

**Interfaces:**
- Consumes: `SessionStore`, `Limits` (`gateway/src/session.rs`, both already imported in `proxy.rs`'s test module); `state_with`, `test_limits`, `test_key`, `session_headers`, `call_with_headers`, `detector_finding_weber`, `person_span` (all existing test helpers in `proxy.rs`, unchanged by Task 1).
- Produces: nothing new — this is the last task in the plan.

Task 1 proves the eviction *policy* is correct in isolation. This task proves the thing Codex actually found — that a failing real HTTP request, going through the full `handle()` path, no longer destroys another conversation's committed value — using the store through the same code path a live gateway uses, not through `acquire_at` directly.

- [ ] **Step 1: Write the test**

Add to `mod tests` in `gateway/src/proxy.rs`, near `a_refused_request_leaves_the_session_untouched` (they are testing adjacent properties of the same mechanism, and this one reuses its detector-mock shape: a 200 with `person_span()` for text containing `SECRET`, a 503 for anything else).

```rust
    #[tokio::test]
    async fn a_failing_request_does_not_evict_a_live_third_party_session() {
        // Session "a" commits a real value through an ordinary successful
        // request. Session "b" then takes the store's second slot with a
        // request that fails during masking, leaving its entry created but
        // empty — `acquire` runs, and creates the entry, before `mask_all`
        // can fail. A third session's request also fails during masking,
        // and needs a slot in a now-full store: it must evict "b" (empty),
        // never "a" (live) — even though "a" is the older of the two by
        // last_seen.
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
        // empty.
        let (status_b, _) = call_with_headers(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "und dann?"}]}),
            &session_headers("Bearer k1", "b"),
        )
        .await;
        assert_eq!(status_b, StatusCode::BAD_GATEWAY);

        // "c" also fails during masking, and needs a slot in a store already
        // holding {a, b} at its cap of 2.
        let (status_c, _) = call_with_headers(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "und dann?"}]}),
            &session_headers("Bearer k1", "c"),
        )
        .await;
        assert_eq!(status_c, StatusCode::BAD_GATEWAY);

        // Session "a" must still hold Weber's placeholder.
        let session_a = state.sessions.acquire(&test_key("a", "Bearer k1"));
        assert_eq!(
            session_a.mapping.lock().await.restore("[PERSON_1]").unwrap(),
            "Weber",
            "a failing request for a different session evicted a's live value"
        );
    }
```

- [ ] **Step 2: Run the test to verify it passes**

This is a regression guard on behavior Task 1 already implemented and unit-tested — like Task 6's `a_stream_holds_no_session_lock` test in the original session-mapping plan, it is expected to PASS on its first run, not fail first.

Run: `cd gateway && cargo test a_failing_request_does_not_evict_a_live_third_party_session -- --nocapture 2>&1 | tail -20`
Expected: PASS. If it fails, that means Task 1's fix does not actually close the finding end to end even though its own unit tests passed — stop and report BLOCKED with the failure output rather than adjusting this test to make it pass.

- [ ] **Step 3: Run the full suite, formatting and lints**

Run: `cd gateway && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test 2>&1 | tail -10`
Expected: no warnings, everything passes.

- [ ] **Step 4: Commit**

```bash
git add gateway/src/proxy.rs
git commit -m "test(gateway): a failing request cannot evict another session's value

Task 1 fixed the eviction policy in isolation, with its own unit tests.
This is the integration-level proof: a real request that fails during
masking, through the full handle() path, no longer destroys a different
conversation's committed value. A regression guard on already-delivered
behavior, so it passes on its first run rather than needing a RED phase."
```

- [ ] **Step 5: Push and update the PR**

```bash
git push
```

PR #15 already exists (draft, design-only) against `main`. Pushing adds these two commits to it. Do not mark it ready for review or merge — report back to the controller with the commit range for review.

---

## Self-Review

**Spec coverage.** The spec's fix (prefer an empty victim, fall back to oldest) maps to Task 1 Step 3. The correctness argument (a session mid-masking holds its lock throughout, so `try_lock` can never misjudge it) maps to Task 1's `a_locked_session_is_never_treated_as_the_empty_candidate` test. The explicit non-change (`last_seen` refresh stays unconditional) is called out in Global Constraints so no task accidentally touches it. The regression guard (`a_full_store_evicts_the_least_recently_used` must keep passing unmodified) is verified in Task 1 Step 5. The integration-level proof the spec's Testing section calls for is Task 2. `Mapping::is_empty()` losing its `#[allow(dead_code)]` is Task 1 Step 4, matching the spec's note that this fix gives it a live caller. Out-of-scope items (per-credential quotas, byte bounds, Anthropic session-path coverage, `last_seen` behavior) have no task, deliberately.

**Type consistency.** `probably_empty(entry: &Entry) -> bool` is defined once (Task 1 Step 3) and called once, from inside `acquire_at` in the same file — no cross-task signature to keep consistent. Task 2 does not call `probably_empty` or reference `Entry` at all; it only observes behavior through `SessionStore::acquire`, `Session.mapping`, and `Mapping::restore`, all pre-existing and unchanged.

**Correction from the first draft.** Task 2 originally used the `detector_finding_weber` helper for the failing sessions, whose fallback returns `spans: []` with a 200 — a successful empty detection, not a failure, which would not have exercised the eviction path this test exists to prove. It also originally proposed verifying the test against pre-fix code via `git stash`, which doesn't work once Task 1's fix is already committed (stashing removes only Task 2's uncommitted test, not Task 1's already-committed fix). Both are corrected above: the detector is built inline with the same 503-on-no-match shape `a_refused_request_leaves_the_session_untouched` already uses, and Step 2 is reframed as a pass-on-first-run regression guard, matching how the original session-mapping plan's Task 6 handled the same situation.
