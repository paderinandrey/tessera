# Session eviction preference — design

**A narrow follow-up to slice D (session-scoped mapping).** `SessionStore::acquire`
evicts before the caller's request is known to succeed. A request that fails
during masking can still have destroyed another conversation's live session to
make room for itself. This slice changes which entry gets evicted when the store
is full, and closes a second, more serious gap Codex found in this design's own
first draft while it was still in review.

**Traceability:** flagged by Codex on PR #14
(github.com/paderinandrey/tessera/pull/14), confirmed real against
`gateway/src/session.rs:188-224` and `gateway/src/proxy.rs:141`, and recorded as a
tracked follow-up in project memory (`session-store-eviction-followup`). This
design's own first draft was reviewed by Codex on PR #15 before implementation
began, which caught a race the first draft would have introduced — see
"Revision history" below.

## The bug, precisely

`acquire_at` (`session.rs`) runs before `mask_all` can fail with `?`
(`proxy.rs:141` precedes `proxy.rs:144`). For a **new** session key, if the store
is at `max_sessions`, it evicts the globally-oldest entry and inserts a new empty
one — unconditionally. For an **existing** key, it refreshes `last_seen`
unconditionally. If the request then fails during detection or masking, the
eviction has already happened and cannot be undone: another conversation's real
value-to-placeholder table is gone, forever, for a request that never used the
space it took.

No value ever crosses a session boundary and no wrong name is ever restored —
restoration always works from the request's own snapshot (`proxy.rs`'s `work`),
established by slice D and reverified independently by that slice's final review.
This is a coreference-availability bug, not a confidentiality one. But it is real
and it is triggerable by any caller holding a single valid credential, by sending
cheap, guaranteed-to-fail requests with fresh session ids.

## Why the obvious fix is excluded

Codex's own suggested fix — make the store mutation provisional, roll it back
when masking fails — is unsafe for this code. `acquire_at`'s check-or-insert runs
in one critical section under the store's `std::sync::Mutex`, which is what
guarantees that two concurrent requests for the same brand-new session id share
one `Arc<Session>` and serialize on its lock during masking. Deferring the
insertion until success is known would remove that guarantee: two concurrent
first-touch requests for one new id would each mask independently against a
private `Mapping::new()`, with nothing serializing them, and both would try to
commit afterward — reintroducing the exact race slice D's design excluded (two
concurrent requests allocate a placeholder to two different values; the loser's
response restores the wrong name). A fix that reopens that race is worse than the
bug it closes.

## Revision history: what Codex's review of the first draft found

This design's first draft proposed changing only which entry gets evicted —
preferring an entry whose `Mapping` was empty, checked with a non-blocking
`try_lock()` from inside `acquire_at`'s existing critical section — without
changing anything about how a session is handed back to its caller. Codex
reviewed that draft on PR #15 and raised two findings.

**Finding 1 (real, scoped, left as an accepted limitation).** When the store is
completely full of live, valued sessions — no empty entry anywhere — the fallback
still evicts the globally-oldest one regardless of content, exactly as before this
slice. A single doomed request can still destroy a live session in that specific
state. This is not merely a coreference-availability cost: an entry evicted while
it is held is exactly the double-session precursor finding 2 below describes — a
later request for the same id finds nothing in the store, creates a second,
unsynchronized session, and a response can restore the wrong person's name. That
makes this confidentiality-class, not availability-class, and it is not
hypothetical: `an_evicted_session_does_not_interrupt_a_request_holding_it` in the
implemented test suite runs at `limits(1)`, where the held session is the only
candidate, and the fallback evicts it while its guard is alive.

Its blast radius is bounded only for a **serial** burst of doomed requests: the
first one's victim is immediately replaced by that same request's own (now
empty) entry, so the next doomed request from the same source finds an empty
entry to reclaim first, not another live one — at most one incidental live
eviction, steady state. Concurrency defeats this bound directly: each in-flight
request holds its own entry's lock across `mask_all`, which spans one or more
round-trips to the detector service, so none of the entries left behind by
requests 1..i-1 are reclaimable when request *i* runs its own eviction scan —
every one of them takes the fallback in turn instead. K concurrent doomed
requests cost min(K, max_sessions) live evictions, not one; the attacker needs no
special timing beyond not waiting for responses, and the detector's documented
latency (roughly a second per 1 200 characters) widens this window rather than
narrowing it. A second, serial way to defeat the bound: a request that masks
successfully but then fails upstream still calls `absorb`, leaving a non-empty
entry behind; alternating such requests with fresh session ids costs one live
eviction each, indefinitely, because no reclaimable residue ever accumulates.

Still accepted for this slice — it is strictly better than the pre-fix behavior,
where *every* eviction was unconditional rather than only this saturated-and-
concurrent or non-empty-residue case — but it is not the availability-only "costs
coreference, never protection" case slice D accepted; that comparison stops
holding once the evicted entry can be one a live request currently holds.
Closing this completely would mean never evicting a live session under any
circumstance, which just moves the resource-exhaustion problem elsewhere (an
unbounded store, or a rejected request when capacity is genuinely exhausted by
real traffic). Left out of scope; see "Out of scope" below for the principled fix
this would need, and for why it belongs ahead of availability-only follow-ups.

**Finding 2 (real, serious, the reason this draft was rewritten).** The proposed
`try_lock()` check has a gap the first draft did not account for:
`SessionStore::acquire` fully completes and releases the store's own
`std::sync::Mutex` *before* the caller in `proxy.rs` reaches
`session.mapping.lock().await` (`proxy.rs:141-142`). Between those two lines, on a
multi-threaded runtime, a session that was just handed out — new or an existing
one that happened to be idle — is genuinely unlocked and empty from another
thread's point of view. A concurrent `acquire_at` call for an unrelated key,
running its eviction scan in exactly that window, would see it as a legitimate
"empty" victim under the first draft's check and remove it from the store's map.

The caller that was handed that session is unaffected in itself — it still holds
its own `Arc<Session>`, and slice D already proved that a store-level eviction
does not disturb a request already holding one. The actual damage happens if a
*third*, unrelated request for the *same* session id arrives shortly after: it
finds nothing in the store (the entry was just evicted out from under its
rightful owner) and creates a brand-new, separate session for that id. Two
requests are now both "first touches" of the same conversation, with two
unsynchronized `Mapping`s — precisely the race slice D's design excluded,
reintroduced here by an entirely unrelated third request's eviction choice. This
converts an availability bug (lost coreference) into a potential confidentiality
one (a response could restore the wrong person's name), which is a worse defect
than the one this slice sets out to fix. It had to be closed before any
implementation began.

## The fix

Two changes to `session.rs`, landing together because Rust requires the whole
crate to compile as one unit and the second cannot exist without the first
requiring a small, coupled change to `proxy.rs`'s `handle()`.

**1. `Session.mapping` is wrapped in its own `Arc`:**

```rust
pub struct Session {
    mapping: Arc<tokio::sync::Mutex<Mapping>>,
}
```

This is the only structural change needed to make `try_lock_owned()` available —
it requires its receiver to be `Arc<Mutex<T>>` specifically, not a `Mutex<T>`
field nested inside another `Arc`-wrapped struct. Every existing `.lock().await`
call site (`proxy.rs`, and `session.rs`'s own tests) keeps working unchanged:
`Arc<Mutex<T>>` still derefs to `Mutex<T>`, so `.lock()` resolves exactly as it
did before. Only code that needs the *owned*, non-blocking form changes.

**2. `acquire`/`acquire_at` claim the lock synchronously, inside the same
critical section that decides whether to hand a session back, and return that
claim instead of a bare `Arc<Session>`:**

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

Immediately after `acquire_at` decides which `Arc<Session>` to return — whether a
cache hit or a freshly created entry — and still holding the store's
`std::sync::Mutex`, it attempts `Arc::clone(&session.mapping).try_lock_owned()`.
For a freshly created entry this always succeeds: the mutex was just constructed
inside this same critical section, so nothing else could possibly hold a
reference to it yet. For a cache hit it succeeds whenever the session is
currently idle, and fails only when another request already holds it — in which
case `Claimed.guard` comes back `None`, and the caller in `proxy.rs` waits its
turn with `lock_owned().await` (`proxy.rs:142-144`).

That wait is not gap-free. A `lock_owned()` future does not register itself as a
waiter on the mutex until it is first polled, and the caller cannot poll it
until control returns from this whole function and the store's own
`std::sync::Mutex` has already been released. If the request that was holding
the lock finishes — releasing it — in the interval between its release and this
new caller's future being polled for the first time, the entry is, briefly,
genuinely unlocked. If that finishing request failed during masking and never
reached `absorb`, the entry is also empty. A concurrent `acquire_at` call's
`reclaimable` check, running its own eviction scan in exactly that interval, sees
a free, empty entry and reclaims it — so when the waiting caller's turn finally
comes, its session is gone. A later request for the same id then finds nothing
in the store and starts a second, unsynchronized session: finding 2's mechanism,
reappearing here on the contended path rather than the fresh-claim path finding 2
was written against. This window is real but narrow — it requires the previous
holder to finish inside the gap between two specific instructions, not at some
arbitrary later point — and it does not undermine the fresh-claim and
idle-cache-hit cases, which this call closes outright. See "What makes this
correct now" below for exactly which paths remain exposed and which do not.

The victim-selection logic for a *different* key's insertion is otherwise
unchanged from the first draft — prefer an entry whose lock can be claimed and
whose `Mapping` is empty, falling back to the globally-oldest entry when no such
candidate exists:

```rust
fn reclaimable(entry: &Entry) -> bool {
    match Arc::clone(&entry.session.mapping).try_lock_owned() {
        Ok(guard) => guard.is_empty(),
        Err(_) => false,
    }
}
```

```rust
if map.len() >= self.limits.max_sessions {
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
```

What makes this correct now, where the first draft was not — and only for the
`reclaimable` path specifically, which is what this fix actually changes. A
session claimed synchronously by this call, or found already idle and
re-claimed by it, is locked continuously from the moment it is handed out until
its holder releases it: there is no window in which such a session is "handed
out but not yet in use" and simultaneously "unlocked, therefore evictable" to
`reclaimable`'s `try_lock_owned` check. `reclaimable` can only ever see a
session that finished its last use — either it never held a value, or its
holder released it after `absorb` — which is exactly the population this fix
means to reclaim from. The narrower exception is the contended path described
above: a caller who was told `guard: None` and has not yet queued on
`lock_owned()` is not yet protected by its own hold, only by the previous
holder's, and that hold can end before the new caller queues.

That guarantee does not extend to the rest of `acquire_at`. Two other paths
remove an entry from the store's map without consulting lock state at all, and
neither is what this slice changed:

- The `.or_else` fallback a few lines below — reached whenever nothing is
  currently reclaimable — evicts the globally-oldest entry regardless of
  whether it is locked. `an_evicted_session_does_not_interrupt_a_request_holding_it`
  in the test suite exercises exactly this: at `limits(1)`, the held session is
  the only candidate, and the fallback evicts it while its guard is alive.
- The `map.retain` TTL sweep at the top of `acquire_at` removes any entry whose
  `last_seen` has exceeded `idle`, again without asking whether it is locked.

Both are pre-existing behavior this slice did not touch and was not scoped to
fix; see "Revision history" and "Out of scope" for what closing them fully
would require.

**3. `proxy.rs`'s `handle()` destructures the claim instead of locking directly:**

```rust
let claimed = state.sessions.acquire(&key);
let mut guard = match claimed.guard {
    Some(guard) => guard,
    None => Arc::clone(&claimed.session.mapping).lock_owned().await,
};
let mut work = guard.clone();
let masked = mask_all(&state.detector, &body, &pointers, &mut work).await?;
guard.absorb(&work, state.sessions.max_values());
(masked, work)
```

Everything after obtaining `guard` — clone into `work`, mask, `absorb`, drop at
the end of the match arm before the upstream call — is unchanged from slice D.

`try_lock_owned` and `lock_owned` are non-blocking-to-call and blocking-to-await
respectively, same as the plain (non-owned) forms; neither changes the property
slice D's final review already verified — the store's `std::sync::Mutex` is never
held across an `await`, because nothing here awaits while holding it.

## What stays as it is

`last_seen` on an existing key is still refreshed unconditionally, win or lose.
This extends only the *requester's own* session's idle window; it never touches
another conversation. Not the class of problem this slice addresses.

## Testing

A race is not honestly testable by racing — the earlier session-mapping design
already established this ("A race is not honestly testable; the structural fact
is."). The claim mechanism is proven the same way: hold onto a `Claimed`'s guard
deliberately (simulating a caller mid-masking, exactly what `handle()` does) and
show a concurrent eviction scan cannot select it, rather than attempting to race
real concurrent requests through the HTTP stack.

**`session.rs`:**

- A freshly claimed session — its `Claimed.guard` held, not dropped — is never
  selected as a victim by a concurrent `acquire_at` call for a different key,
  even though its `Mapping` is empty and it is the older of the candidates by
  `last_seen`. This is the direct proof finding 2's own scenario is closed: the
  first draft had no such guard to hold, and this is exactly the scenario it
  would have gotten wrong. It does not cover the contended path's narrower
  residual window — see "The fix" above.
- A session whose lock is held by a genuine second request to the *same* key
  (contended, not merely unclaimed) is likewise never selected — the existing
  serialize-on-same-key behavior stays intact and is not confused with a
  reclaimable entry.
- An entry that is reclaimable (empty, unclaimed) is evicted in preference to a
  live one, even when it is newer by `last_seen` — the direct negation of plain
  oldest-first selection, and the finding-1 fix itself.
- The existing `a_full_store_evicts_the_least_recently_used` test must keep
  passing with only mechanical adjustment for the new `Claimed` return type
  (`.session` field access where the test checks identity) — it is the
  regression guard proving the fallback path still matches pre-slice behavior
  when nothing is reclaimable.
- Every other existing `session.rs` test that reads `acquire_at`'s return value
  needs the same mechanical `.session` adjustment; none needs new assertions.

**`proxy.rs`:** one integration test, unchanged in intent from the first draft —
session A commits a real value through an ordinary successful request; the store
fills to capacity with an empty session; a request under a new session id,
guaranteed to fail during detection, is sent and fails; session A's table still
restores its original value afterward. Existing tests that inspect a session via
`state.sessions.acquire(...)` for assertions need the same mechanical `.session`
adjustment.

## Out of scope

Fully closing finding 1 (a store genuinely saturated with live sessions can
still lose a held one to a doomed request — reliably under concurrency, and
serially whenever masking-succeeds-then-upstream-fails leaves non-empty residue;
see "Revision history" above) would need per-credential quotas — structurally
excluding cross-tenant eviction rather than just reducing its likelihood, a
stronger and broader property than this fix provides. This closes a
confidentiality-class hazard (the double-session state finding 2 identified),
not an availability nicety, and the next slice that touches this store should
rank it accordingly. A candidate for its own future slice, not folded in here,
and unaffected by the fix in this document.
Bounding a single value's byte size within a session (a different, already-
tracked follow-up). Anthropic session-path test coverage (already tracked
separately). Any change to `last_seen` refresh-on-failure, addressed above as a
deliberate non-change.
