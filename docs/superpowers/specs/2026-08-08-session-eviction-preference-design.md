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
state. This is real, but its blast radius is smaller than it first reads: the
victim it evicted is immediately replaced by the doomed request's own (now
empty) entry, so the *next* doomed request from the same source finds an empty
entry to reclaim first, not another live one. A burst of cheap failing requests
costs at most one incidental live eviction, not one per request, and the design
already treats evicting a whole session as safe (see slice D: "costs coreference,
never protection"). Closing this completely would mean never evicting a live
session under any circumstance, which just moves the resource-exhaustion problem
elsewhere (an unbounded store, or a rejected request when capacity is genuinely
exhausted by real traffic). Left out of scope; see "Out of scope" below for the
principled fix this would need if it is ever worth closing.

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
case that other request's hold is itself what keeps the entry safe from a
concurrent eviction scan, so there is nothing to close there.

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

What makes this correct now, where the first draft was not: every session this
store ever hands out is locked continuously from the moment it is handed out
(either because this call claimed it synchronously, or because someone else
already held it and keeps holding it) until whoever holds it releases it. There
is no longer a window in which a session is "handed out but not yet in use" and
simultaneously "unlocked, therefore evictable." `reclaimable` can only ever see
a session that finished its last use — either it never held a value, or its
holder released it after `absorb` — which is exactly the population this fix
means to reclaim from.

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
  `last_seen`. This is the direct proof finding 2 is closed: the first draft had
  no such guard to hold, and this is exactly the scenario it would have gotten
  wrong.
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

Fully closing finding 1 (a store genuinely saturated with live sessions can still
lose one to a doomed request) would need per-credential quotas — structurally
excluding cross-tenant eviction rather than just reducing its likelihood, a
stronger and broader property than this fix provides. A candidate for its own
future slice, not folded in here, and unaffected by the fix in this document.
Bounding a single value's byte size within a session (a different, already-
tracked follow-up). Anthropic session-path test coverage (already tracked
separately). Any change to `last_seen` refresh-on-failure, addressed above as a
deliberate non-change.
