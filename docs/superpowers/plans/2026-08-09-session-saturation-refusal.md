# Session Saturation Refusal Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A request can no longer remove a session entry that another in-flight request is holding — the TTL sweep spares held entries, victim selection prefers the least costly one, and a store whose every entry is in flight refuses the request instead of evicting a live session.

**Architecture:** `SessionStore` gains a three-tier classification of its entries — free-and-empty, free-with-values, held — computed with one `try_lock_owned` per entry inside the store's existing critical section. The TTL sweep keeps held entries whatever their age; victim selection picks the lowest tier and breaks ties by `last_seen`; a winner in the held tier means every entry is in flight, and `acquire` returns `SessionError::Saturated` rather than taking one.

**Tech Stack:** Rust, `tokio::sync::Mutex` (owned guards), `std::sync::Mutex` for the store map, `thiserror`, `axum` status codes. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-08-09-session-saturation-refusal-design.md`

## Global Constraints

- No new dependencies, no new configuration keys. The behaviour is not a policy the operator chooses.
- `gateway/src/session.rs` and `gateway/src/proxy.rs` are the only files that change.
- Never hold a session's `tokio` mutex guard while acquiring the store's `std` mutex, and never `await` inside the store's critical section. `tier()` takes a guard and drops it immediately, inside a synchronous function; keep it that way.
- Comments explain *why*, in the voice of the surrounding file. The existing `session.rs` comments are the reference.
- Every commit runs `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and `cargo test` from `gateway/`, and all three must be clean.
- Conventional commit subjects, lowercase after the type, as in `fix(gateway): claim a session's lock synchronously, prefer reclaiming empties`.
- No error message, log line or test name carries submitted text.

## File Structure

- `gateway/src/session.rs` (638 lines) — the store. Gains `Tier` and `tier()`, loses `reclaimable()`, rewrites the sweep and the victim scan in `acquire_at`, and gains `SessionError::Saturated`. This is the whole behavioural change.
- `gateway/src/proxy.rs` (1553 lines) — the caller. Propagates the new `Result` at one production call site and maps the new variant to `503` instead of the `400` its siblings carry.

Both files already own these responsibilities; nothing moves between them and no file is created.

---

### Task 1: Classify entries into three tiers; the sweep spares held ones

`acquire_at` currently makes two lock-state decisions with one helper and forgets it in a fallback: `reclaimable` filters for free-and-empty, then `.or_else` jumps straight to the globally-oldest entry, held or not. And `map.retain` never consults lock state at all. This task replaces the helper with a total order and teaches the sweep to spare held entries. It does **not** yet refuse anything: when every entry is held, the scan still evicts one, exactly as today. Task 2 closes that.

**Files:**
- Modify: `gateway/src/session.rs:174-184` (replace `reclaimable`), `gateway/src/session.rs:219-243` (sweep and victim scan)
- Test: `gateway/src/session.rs` test module — one new test, one existing test corrected

**Interfaces:**
- Consumes: `Entry { session: Arc<Session>, last_seen: Instant }`, `Session { mapping: Arc<tokio::sync::Mutex<Mapping>> }`, `Mapping::is_empty()`, `Limits { idle, max_sessions, max_values }` — all unchanged.
- Produces: `enum Tier { FreeAndEmpty, FreeWithValues, Held }` (private to the module, `Debug + Clone + Copy + PartialEq + Eq + PartialOrd + Ord`) and `fn tier(entry: &Entry) -> Tier`. Task 2 matches on `Tier::Held`.

- [ ] **Step 1: Write the failing test**

Add this test to the test module in `gateway/src/session.rs`, after `a_session_idle_past_the_ttl_is_swept`:

```rust
    #[tokio::test]
    async fn a_held_entry_survives_the_ttl_sweep() {
        let store = SessionStore::new(limits(4));
        let start = Instant::now();

        let mut held = store.acquire_at(&key("a", "Bearer k"), start);
        let guard = held
            .guard
            .take()
            .expect("a fresh session is always claimable");

        // Far past `idle`, and a request is still holding it. Sweeping it
        // would strand that request's commit and let the next request for
        // "a" open a second table for one conversation.
        store.acquire_at(&key("b", "Bearer k"), start + Duration::from_secs(61));
        assert_eq!(store.live(), 2, "a held entry was swept");

        // Released, it is ordinary again: still stale, so the next sweep
        // takes it and "a" comes back as a fresh table.
        drop(guard);
        let a_again = store.acquire_at(&key("a", "Bearer k"), start + Duration::from_secs(62));
        assert!(
            !Arc::ptr_eq(&held.session, &a_again.session),
            "a stale table outlived its holder"
        );
    }
```

Then correct the existing `a_session_idle_past_the_ttl_is_swept` (currently at `gateway/src/session.rs:487-498`). It binds the `Claimed` it gets back, which keeps that session's claim alive for the rest of the test — so once the sweep becomes lock-aware the entry is *held*, is spared, and the test fails for the right reason. Dropping the claim first is what the test always meant. Replace the whole test with:

```rust
    #[test]
    fn a_session_idle_past_the_ttl_is_swept() {
        let store = SessionStore::new(limits(4));
        let start = Instant::now();
        let mut one = store.acquire_at(&key("conv1", "Bearer k"), start);
        // The claim goes with the request that took it. A session nobody is
        // holding is the case this test is about; `a_held_entry_survives_the_ttl_sweep`
        // covers the other one.
        one.guard.take().expect("a fresh session is always claimable");
        let two = store.acquire_at(&key("conv1", "Bearer k"), start + Duration::from_secs(61));
        assert!(
            !Arc::ptr_eq(&one.session, &two.session),
            "a stale table was handed back"
        );
        assert_eq!(store.live(), 1, "the swept entry was left behind");
    }
```

- [ ] **Step 2: Run the tests to verify the new one fails**

Run: `cd gateway && cargo test session::tests::a_held_entry_survives_the_ttl_sweep -- --exact`
Expected: FAIL — `a held entry was swept`, because `map.retain` drops the entry on age alone.

`a_session_idle_past_the_ttl_is_swept` passes both before and after its correction; it is a guard against the new behaviour going too far, not a RED test.

- [ ] **Step 3: Replace `reclaimable` with the tier classification**

In `gateway/src/session.rs`, replace the whole `reclaimable` function (lines 174-184, the doc comment included) with:

```rust
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
    match Arc::clone(&entry.session.mapping).try_lock_owned() {
        Ok(guard) if guard.is_empty() => Tier::FreeAndEmpty,
        Ok(_) => Tier::FreeWithValues,
        Err(_) => Tier::Held,
    }
}
```

- [ ] **Step 4: Teach the sweep to spare held entries**

In `acquire_at`, replace the `map.retain(..)` call (line 222) and extend the comment above it (lines 219-221) so the block reads:

```rust
        // Sweeping here rather than in a background task: at a thousand
        // sessions this costs less than a syscall, and there is no separate
        // task whose health is a new thing to reason about. A held entry is
        // spared whatever its age — removing one is the same mistake as
        // evicting it under saturation. `||` short-circuits, so the extra
        // `try_lock` is paid only by entries that are already stale.
        map.retain(|_, entry| {
            now.duration_since(entry.last_seen) < self.limits.idle || tier(entry) == Tier::Held
        });
```

- [ ] **Step 5: Rewrite victim selection as a single ordered pass**

Replace the victim block inside the `else` branch (lines 228-243) with:

```rust
            if map.len() >= self.limits.max_sessions {
                // One pass and one `try_lock` per entry: the cheapest tier
                // wins and `last_seen` breaks ties within it. The old
                // `.or_else` threw that knowledge away and fell back to the
                // globally-oldest entry, held or not.
                let victim = map
                    .iter()
                    .map(|(key, entry)| (tier(entry), entry.last_seen, key))
                    .min_by_key(|(tier, last_seen, _)| (*tier, *last_seen))
                    .map(|(_, _, key)| key.clone());
                if let Some(victim) = victim {
                    map.remove(&victim);
                }
            }
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cd gateway && cargo test session::`
Expected: PASS, all of them. In particular `a_reclaimable_entry_is_evicted_before_a_live_one`, `a_freshly_claimed_session_is_never_treated_as_the_reclaimable_candidate` and `a_full_store_evicts_the_least_recently_used` must stay green — the tier order reproduces every preference they assert.

- [ ] **Step 7: Run the full suite, formatting and lints**

```bash
cd gateway
cargo fmt
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```
Expected: 179 tests pass (178 before, plus the new one), no clippy warnings.

- [ ] **Step 8: Commit**

```bash
git add gateway/src/session.rs
git commit -m "fix(gateway): order eviction candidates by cost, spare held entries from the sweep"
```

---

### Task 2: A store whose every entry is held refuses the request

With Task 1 in place the scan still evicts a held entry when every entry is held — and that is the reachable case, because each in-flight request holds its guard across `mask_all` and its detector round-trip. This task makes that case a refusal.

**Files:**
- Modify: `gateway/src/session.rs:26-42` (`SessionError`), `gateway/src/session.rs:208-269` (`acquire`, `acquire_at`)
- Modify: `gateway/src/proxy.rs:15` (import), `gateway/src/proxy.rs:33-42` (status mapping), `gateway/src/proxy.rs:141` (call site)
- Test: both test modules

**Interfaces:**
- Consumes: `Tier` and `tier()` from Task 1.
- Produces: `SessionError::Saturated`; `SessionStore::acquire(&self, key: &SessionKey) -> Result<Claimed, SessionError>` and the private `acquire_at(&self, key: &SessionKey, now: Instant) -> Result<Claimed, SessionError>`.

- [ ] **Step 1: Write the failing tests**

Add to the test module in `gateway/src/session.rs`:

```rust
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
```

Then replace `an_evicted_session_does_not_interrupt_a_request_holding_it` (currently at `gateway/src/session.rs:533-547`) — it asserts the behaviour this task removes. The situation it set up is now a refusal:

```rust
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
```

And add to the test module in `gateway/src/proxy.rs`, after `a_malformed_session_id_is_refused`:

```rust
    #[tokio::test]
    async fn a_saturated_store_refuses_before_the_detector_runs() {
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
                idle: Duration::from_secs(1800),
                max_sessions: 1,
                max_values: 8,
            },
        );

        // The only slot belongs to a session another request is inside right
        // now, exactly as it would be mid-`mask_all`.
        let mut held = state
            .sessions
            .acquire(&test_key("conv-1", "Bearer k1"))
            .unwrap();
        let _guard = held
            .guard
            .take()
            .expect("a fresh session is always claimable");

        let (status, body) = call_with_headers(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
            &session_headers("Bearer k1", "conv-2"),
        )
        .await;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains("in flight"), "{body}");
        assert_eq!(
            detector.received_requests().await.unwrap().len(),
            0,
            "a refused request cost a detection pass"
        );
        assert_eq!(
            upstream.received_requests().await.unwrap().len(),
            0,
            "a refused request still reached the provider"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail to compile**

Run: `cd gateway && cargo test session::`
Expected: FAIL to compile — `no variant named Saturated`, and `.unwrap()` on `Claimed`, which is not a `Result`.

- [ ] **Step 3: Add the `Saturated` variant**

In `gateway/src/session.rs`, add as the last variant of `SessionError` (after `NoCredential`, line 41):

```rust
    #[error(
        "every session in the store is in flight, so there is none to reclaim; the request \
         is refused rather than evicting a live session, which would leave one conversation \
         with two unsynchronized tables"
    )]
    Saturated,
```

- [ ] **Step 4: Return `Result` from `acquire` and `acquire_at`, and refuse**

Change `acquire` (line 208) to:

```rust
    pub fn acquire(&self, key: &SessionKey) -> Result<Claimed, SessionError> {
        self.acquire_at(key, Instant::now())
    }
```

Change `acquire_at`'s signature (line 213) to `fn acquire_at(&self, key: &SessionKey, now: Instant) -> Result<Claimed, SessionError> {`, replace the victim block written in Task 1 Step 5 with the version below, and change the final `Claimed { session, guard }` (line 268) to `Ok(Claimed { session, guard })`.

```rust
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
                    .min_by_key(|(tier, last_seen, _)| (*tier, *last_seen))
                    .map(|(tier, _, key)| (tier, key.clone()));
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
```

- [ ] **Step 5: Fix the call sites in `session.rs`'s tests**

Every `store.acquire_at(..)` in the test module now yields a `Result`. Add `.unwrap()` to each one **except** the two that assert a refusal (`a_store_whose_every_entry_is_held_refuses_a_new_session` and `a_held_session_is_refused_room_rather_than_evicted`, which bind the `Result` itself). Statement-position calls whose value is discarded — `store.acquire_at(&key("b", "Bearer k"), start + Duration::from_secs(1));` — also need `.unwrap()`, or clippy's `unused_must_use` will fail the build.

Run `cargo test session:: 2>&1 | head -40` and let the compiler enumerate them; there are 23 such calls before this task's additions.

- [ ] **Step 6: Propagate the error in `proxy.rs`**

Add `SessionError` to the import on line 15:

```rust
use crate::session::{key_from, Limits, SessionError, SessionStore};
```

Replace line 141 with:

```rust
            let claimed = state.sessions.acquire(&key)?;
```

Replace the status match in `IntoResponse for ProxyError` (lines 33-42) with:

```rust
        let status = match self {
            // A body we cannot read is the client's to fix, and it is refused
            // rather than forwarded unmasked. So is a session the gateway
            // cannot honour as asked.
            ProxyError::Shape(ShapeError::Request(_))
            | ProxyError::Shape(ShapeError::Unsupported(_, _))
            | ProxyError::Session(SessionError::BadId)
            | ProxyError::Session(SessionError::Disabled)
            | ProxyError::Session(SessionError::NoCredential(_)) => StatusCode::BAD_REQUEST,
            // Saturation is this gateway's own capacity rather than anything
            // the caller got wrong, and the same request may well succeed a
            // moment later. No `Retry-After`: the wait is another request's
            // detector round-trip, and the gateway has no honest number for it.
            ProxyError::Session(SessionError::Saturated) => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::BAD_GATEWAY,
        };
```

Then add `.unwrap()` to the four `state.sessions.acquire(..)` calls in `proxy.rs`'s test module (lines 868, 1324, 1406 and 1464 before this task's additions). Each of those stores has spare capacity and no held entry at that point, so none of them can refuse.

- [ ] **Step 7: Run the tests to verify they pass**

```bash
cd gateway
cargo test session::
cargo test proxy::
```
Expected: PASS. `a_failing_request_does_not_evict_a_live_third_party_session` must stay green — its store holds a free-and-empty entry when the third request arrives, so the scan still has something to take and never reaches the refusal.

- [ ] **Step 8: Run the full suite, formatting and lints**

```bash
cd gateway
cargo fmt
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```
Expected: 182 tests pass (179 after Task 1, plus three new; one existing test was replaced rather than added), no clippy warnings.

- [ ] **Step 9: Update the README's session bounds paragraph**

`README.md` currently tells the reader that "Reaching a bound costs coreference, never protection." That stays true of `max_session_values` and of the TTL, and is now incomplete for `max_sessions`. In the "Sessions" section, after the sentence ending "it is simply not remembered.", add:

```markdown
Reaching `max_sessions` is the one bound that can cost a request rather than a
coreference. A session table is only ever reclaimed from a conversation that has
no request inside it; when every table in a full store is in flight, a request
asking for a *new* session is refused with a 503 rather than served by evicting a
live one. Evicting one would leave that conversation with two unsynchronized
tables, and two concurrent requests can then give one placeholder to two
different values — which is a wrong name in a response, not a lost coreference.
A request for a session the store already holds is never refused.
```

- [ ] **Step 10: Commit**

```bash
git add gateway/src/session.rs gateway/src/proxy.rs README.md
git commit -m "fix(gateway): refuse a request rather than evict a session in flight"
```

- [ ] **Step 11: Push and open the PR**

```bash
git push -u origin fix/session-saturation-refusal
```

Open the PR against `main`, then comment `@codex review` on it. Report the commit range back rather than merging: the design document is already on the branch, and the whole-branch review is what catches a misclassified risk in the prose as well as in the code.

---

## Self-Review

**Spec coverage.** The spec's three changes map to Task 1 Step 4 (sweep spares held entries), Task 1 Step 5 (three-tier order) and Task 2 Step 4 (refusal). The `503`-not-`400` decision and the absent `Retry-After` map to Task 2 Step 6. The spec's `Tier` and `tier()` sketch is Task 1 Step 3; its note that `max_sessions = 0` is unreachable is carried into the `None` arm's comment in Task 2 Step 4. Every test the spec's Testing section names has a home: held entry survives the sweep and stale free entry still swept (Task 1 Step 1), tier order (covered by the three existing eviction tests, which Task 1 Step 6 requires to stay green), refusal when all held (Task 2 Step 1), refusal costs no detector pass and no upstream call and carries `503` while a malformed id still carries `400` (Task 2 Step 1's proxy test plus the untouched `a_malformed_session_id_is_refused`), an already-held session still served under saturation (Task 2 Step 1), and the existing third-party-eviction guarantee (Task 2 Step 7). The spec's out-of-scope list — per-credential quotas, byte bounds, metrics — has no task, deliberately.

**Placeholder scan.** No step says "handle errors" or "add tests" without the code. Every code step carries the literal text to write; the two steps that enumerate mechanical call-site edits (Task 2 Steps 5 and 6) give the counts, the line numbers and the exceptions, and name the compiler as the enumerator rather than leaving the list implied.

**Type consistency.** `Tier` is declared once (Task 1 Step 3) with `FreeAndEmpty`, `FreeWithValues`, `Held`, and every later use — the sweep's `tier(entry) == Tier::Held`, the scan's `min_by_key`, Task 2's `Some((Tier::Held, _))` — spells those names identically and relies on the derived `Ord` matching declaration order. `tier(entry: &Entry) -> Tier` is called only with `&Entry`, which is what `map.iter()` yields. `SessionError::Saturated` is a unit variant everywhere it appears: the `#[error]` attribute, the `matches!` assertions and the `ProxyError::Session(SessionError::Saturated)` arm. `acquire` and `acquire_at` change to `Result<Claimed, SessionError>` in one step, and every call site named afterwards accounts for it. `Claimed`'s fields stay `.session` and `.guard`, and every test that both holds a guard and touches a mapping does so through that guard rather than locking again — the deadlock the Global Constraints warn about appears in no step.
