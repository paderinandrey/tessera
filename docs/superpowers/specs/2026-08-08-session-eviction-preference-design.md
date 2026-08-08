# Session eviction preference — design

**A narrow follow-up to slice D (session-scoped mapping).** `SessionStore::acquire`
evicts unconditionally, before the caller's request is known to succeed. A request
that fails during masking can still have destroyed another conversation's live
session to make room for itself. This slice changes which entry gets evicted when
the store is full; nothing else.

**Traceability:** flagged by Codex on PR #14
(github.com/paderinandrey/tessera/pull/14), confirmed real against
`gateway/src/session.rs:188-224` and `gateway/src/proxy.rs:141`, and recorded as a
tracked follow-up in project memory (`session-store-eviction-followup`). The
session-mapping design's rejected alternative — seed a request-local copy, merge
back after success — is the failure mode any fix here must not reintroduce.

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
one `Arc<Session>` and serialize on its `tokio::sync::Mutex` during masking.
Deferring the insertion until success is known would remove that guarantee: two
concurrent first-touch requests for one new id would each mask independently
against a private `Mapping::new()`, with nothing serializing them, and both would
try to commit afterward — reintroducing the exact race slice D's design excluded
(two concurrent requests allocate a placeholder to two different values; the
loser's response restores the wrong name). A fix that reopens that race is worse
than the bug it closes.

## The fix: prefer an already-empty victim

The atomic check-or-insert is untouched. Only the victim selection, inside the
existing `if map.len() >= self.limits.max_sessions` branch, changes: prefer an
entry whose `Mapping` is empty over the globally-oldest entry, falling back to
today's oldest-regardless-of-content selection only when no empty entry exists.

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

```rust
if map.len() >= self.limits.max_sessions {
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

`try_lock` is non-blocking and needs no `.await`; it runs inside the same
synchronous critical section as today, under the same `std::sync::Mutex`, which
final review already verified is never held across an `await` — that property is
unaffected, since nothing here awaits.

This makes `Mapping::is_empty()` reachable from the live bin path for the first
time (it has carried `#[allow(dead_code)]` since slice D's Task 1). That
attribute is removed as part of this change.

## Correctness: a session mid-masking can never be misjudged as empty

This is the property the fix depends on, and it is not new machinery — it falls
out of locking discipline slice D already established. `handle()` takes the
session's lock before masking starts and holds it continuously through
`mask_all` and `absorb`, releasing only at the end of the match arm — that is,
for the entire window during which the session's `Mapping` could look empty from
outside. While that lock is held, any concurrent `try_lock()` on it fails, so
`probably_empty` returns `false` — not because of a special case, but as a direct
consequence of the lock already being held for exactly that window.

A session becomes a candidate for preferential eviction only once its current
request has fully finished: either it never held any values (a legitimately
empty session), or its last request failed. Evicting it in that state is safe
under Task 4's already-proven property — a store-level eviction never disturbs a
request still holding its own `Arc<Session>`, whether that request finished
successfully (its committed values live in the `Arc`, independent of the store's
map) or is still, briefly, in flight for some *other* reason.

One case worth naming explicitly: a session whose last request succeeded but
masked zero values (ordinary text with nothing to redact) is indistinguishable,
by this check, from one whose last request failed. Both are `is_empty()` and
both are equally fair game. This is intentional, not a gap — an empty-by-success
session has nothing to lose either, so treating it the same as empty-by-failure
costs nothing and keeps the check simple.

## What stays as it is

`last_seen` on an existing key is still refreshed unconditionally, win or lose.
This extends only the *requester's own* session's idle window; it never touches
another conversation. Not the class of problem this slice addresses.

## Testing

**`session.rs`**, extending the existing `acquire_at`-with-explicit-time tests:

- A store at capacity holding one entry with a real committed value and one
  untouched entry: a new key evicts the *untouched* one, even when it is more
  recently created than the value-holding one — the direct negation of plain
  oldest-first selection.
- A store at capacity holding only entries with real committed values falls back
  to evicting the globally oldest — the existing
  `a_full_store_evicts_the_least_recently_used` test already covers this (its
  entries are never written to, so it is an implicit regression guard) and must
  keep passing unmodified.
- A session whose lock is held (a simulated in-flight request) is never chosen
  as a preferred-empty victim, and the store falls through to the other
  candidate without panicking.

**`proxy.rs`**, proving the actual finding is closed, not just its unit-level
proxy: session A commits a real value through an ordinary successful request.
The store is filled to capacity with empty sessions. A request under a new
session id, guaranteed to fail during detection (as in the existing
`a_refused_request_leaves_the_session_untouched`), is sent and fails. Session
A's table still restores its original value afterward.

## Out of scope

Per-credential quotas (would structurally exclude cross-tenant eviction
entirely, a stronger and broader property than this fix provides — a candidate
for its own future slice, not folded in here). Bounding a single value's byte
size within a session (a different, already-tracked follow-up). Anthropic
session-path test coverage (already tracked separately). Any change to
`last_seen` refresh-on-failure, addressed above as a deliberate non-change.
