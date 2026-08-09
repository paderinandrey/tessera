# Session eviction under saturation: refuse rather than evict a live session

## The problem

Three code paths remove an entry from the session store's map. The
reclaimable-preference slice (PR #15) made one of them lock-aware and left two
untouched:

- the `.or_else` saturation fallback (`session.rs:238`) takes the
  globally-oldest entry, held or not;
- the `map.retain` TTL sweep (`session.rs:222`) never consults lock state at
  all.

Both remove an entry that an in-flight request may be holding at that moment.
The holder's `absorb` then commits into a table nobody will look up again, and
the next request carrying the same `X-Tessera-Session` id finds nothing and
creates a **second, unsynchronized session for one conversation**.

That state is the failure the session-mapping design already rejected an entire
approach to avoid. Its "The alternative that was rejected" section
(`2026-08-07-session-mapping-design.md:81`) turns down masking against a
request-local copy merged back at the end, because two concurrent requests both
allocate `[PERSON_3]` for two different values and both send it upstream before
either merges; restoration then puts the wrong person's name into a response,
silently. Two live sessions for one conversation reconstruct exactly that race,
by a different route.

So this is **confidentiality-class, not availability-class**. An earlier draft
of the eviction work called the same reachable state "coreference loss, never
protection"; that was wrong, and the misclassification is what kept this
follow-up ranked as cleanup.

### Why the existing preference does not already cover it

`reclaimable` — lock free *and* mapping empty — is checked first, and under a
single doomed request it works. It does not survive concurrency. Each in-flight
request holds its own guard across `mask_all`, which spans a detector HTTP
round-trip of roughly a second per 1 200 characters. K concurrent requests
therefore see no reclaimable entry at all, every one of them falls through to
the globally-oldest fallback, and K live sessions are evicted. Detector latency
widens that window rather than narrowing it.

Serial interleaving defeats it too: a request that masks successfully but fails
*upstream* still calls `absorb`, so it leaves a non-empty entry behind and no
reclaimable residue accumulates.

## The decision

Three changes, of which only the third is a close.

1. **The TTL sweep spares held entries.** An entry whose lock is currently taken
   stays in the map regardless of its idle age. It is swept on a later
   `acquire` once it is free — `last_seen` is not refreshed by this, so nothing
   is kept alive longer than one in-flight request.

2. **Victim selection becomes a total order over three tiers**, evaluated in a
   single pass: free-and-empty, then free-with-values, then held; within a tier,
   oldest by `last_seen`. This replaces `filter(reclaimable).min_by_key(..)`
   followed by an `.or_else` that forgets everything the filter knew.

3. **When every candidate is held, the request is refused** rather than served
   by evicting a live session.

The three are not equal. Point 1 closes the TTL vector outright — after it, the
sweep cannot remove a held entry at all. Point 2 only narrows the saturation
vector, from "every entry holds a value" down to "every entry is in flight".
Point 3 is what closes that one: without it the concurrent burst described above
still evicts held sessions, because under that burst *every* entry is in tier
three and tier three is what the scan would settle for.

Refusing is the house rule rather than a new one. The gateway already refuses a
body it cannot parse, a span it cannot apply, a placeholder it cannot restore
and a session it cannot honour as asked. "Fail-closed by default" is the first
design principle in the README, and a silent identity swap in a response is
precisely the failure it exists to prevent.

### What the refusal costs, stated plainly

A caller holding `max_sessions` sessions in flight can make *another* caller's
new session fail. That is a denial of service that crosses a credential
boundary — availability-class, where the bug it replaces is
confidentiality-class. It is the trade the fail-closed principle asks for, and
it is not a complete answer: **per-credential quotas remain the structural
close**, because they stop eviction from crossing a tenant boundary at all.
This slice deliberately does not attempt them.

Two things bound the blast radius. The refusal is reachable only when the store
is at `max_sessions` *and* every entry is currently in flight; the default is
1000 sessions. And it only affects a request asking for a session the store does
not already hold — an existing session is found by key and never reaches the
eviction path at all.

### Status code

`503 Service Unavailable`, not the `400` the other `SessionError` variants
carry. `BadId`, `Disabled` and `NoCredential` are all the client's to fix;
saturation is the gateway's own capacity and the same request may well succeed a
moment later. The variant is added to `SessionError` so the store keeps one
error type, and `ProxyError`'s status match is split per variant rather than
mapping `Session(_)` wholesale to `400`.

No `Retry-After` is sent. The condition clears when some other request finishes
its detector round-trip, and the gateway has no honest number for that; a
guessed one would be worse than none.

## Alternatives rejected

**Tiering without the refusal.** Roughly two lines cheaper and leaves the
reachable confidentiality bug fully reachable — under a concurrent burst every
entry is held, which is the tier the fallback would still evict from. Narrowing
a silent identity swap is not closing it.

**Evict the held entry but poison it,** so the holder's `absorb` fails and its
request dies. Fail-closed in the same spirit, but it kills an in-flight request
that did nothing wrong, and kills it *after* the detector has already been paid
for. Refusing the newcomer is cheaper and fairer, and it prevents the second
session rather than punishing the first.

**Wait for a slot.** `acquire_at` runs inside the store's own
`std::sync::Mutex` critical section, which by construction never spans an
`await`. Blocking there would block every other session operation in the
process, and making the store async to avoid that is a far larger change than
the bug justifies.

**Per-credential quotas.** The real structural close, and out of scope here: it
needs its own design (what a quota is per, what happens when one credential's
quota is full, how quotas interact with `max_sessions`), and this bug should not
wait for it.

## Components

`gateway/src/session.rs` carries the change. `gateway/src/proxy.rs` changes only
where it maps the new error to a status.

```rust
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Tier { FreeAndEmpty, FreeWithValues, Held }

/// One `try_lock_owned` per entry, dropped immediately. Safe for the same
/// reason `reclaimable` was: succeeding only ever means nobody needs it now,
/// and the caller holds no session guard at this point in `acquire_at`.
fn tier(entry: &Entry) -> Tier;

pub fn acquire(&self, key: &SessionKey) -> Result<Claimed, SessionError>;
```

The sweep keeps its existing freshness test and gains a second arm:
`retain(|_, e| now.duration_since(e.last_seen) < self.limits.idle || tier(e) == Tier::Held)`.
`||` short-circuits, so the `try_lock` is paid only for entries that are already
stale — the rare case — rather than on every entry of every request.

Victim selection becomes `map.iter().min_by_key(|(_, e)| (tier(e), e.last_seen))`,
which calls `tier` once per entry and orders by the derived `Ord`. A winner in
`Tier::Held` means every entry is held, and that is the refusal.

`Limits` needs no new field, and no configuration key is added: the behaviour is
not a policy the operator chooses. `max_sessions = 0` cannot reach any of this —
`config.rs:78` already rejects it unless sessions are disabled entirely, in
which case `key_from` refuses before `acquire` is ever called.

## Testing

- A held entry survives the TTL sweep; a stale free entry is still swept.
- The three tiers are evicted in order: free-and-empty before free-with-values
  before held.
- With every entry held, a new session is refused rather than evicting one.
- The refusal happens before the detector runs and before the upstream call —
  it costs no tokens and no detector latency.
- The refusal carries `503`, while a malformed session id still carries `400`.
- A request for a session the store already holds succeeds while the store is
  saturated, because it never reaches the eviction path.
- Existing guarantees stay green, in particular that a failing request cannot
  evict another session's value.

## Out of scope

Per-credential quotas. Byte bounds on a session's table. Anything about the
`X-Tessera-Session` grammar or the credential fingerprint. Metrics for how often
the refusal fires — worth having, and it belongs with the audit slice rather
than here.
