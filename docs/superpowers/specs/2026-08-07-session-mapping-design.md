# Session-scoped mapping — design

**Slice D of the gateway.** A mapping currently lives and dies with one request, so a
value that keeps its name inside a request loses it between requests. This slice gives a
conversation one mapping, so `[PERSON_3]` stays the same person across its turns.

**Traceability:** the gateway-skeleton design, which deferred the question — "Coreference
across requests belongs to the session slice" — and the SSE streaming design, which named
this slice D. The fail-closed rule and the ban on originals in logs apply unchanged.

## What this slice is for, and what it is not for

The driver is **coreference, not cost**. The session holds a value-to-placeholder table
and nothing else. Detection still runs over every text in every request.

The tempting second feature — detect only the new turn and find known values by looking
them up in the session table — is a different slice and may never be one. It would mean a
value is masked only when it matches text the session has already seen, byte for byte,
and the whole point of the detection layer is that personal data does not arrive in the
same form twice. Making the session a substitute for detection would quietly convert a
detection miss into raw egress. Here it can only ever add stability to a placeholder that
detection already produced.

This bounds the blast radius of everything below. A session that is empty, stale, evicted
or wrong costs coreference. It cannot cost protection.

## Decisions taken during brainstorming

- **A session is identified by a client header, namespaced by the caller's credential.**
  `X-Tessera-Session` supplies the id; the store keys on `(credential fingerprint, id)`.
- **No header means today's behaviour** — a mapping scoped to the request.
- **Three bounds, all configurable:** an idle TTL, a cap on live sessions, and a cap on
  values per session.
- **At the value cap the gateway masks without remembering.** The value gets a placeholder
  that lives in this request and is restored in its response; only the commit to the
  session is skipped.

## The session id is a capability, not a hint

A session table is a restoration oracle. Given a session, a caller who writes
`[PERSON_1]` into a prompt gets it echoed by the model, and the gateway would restore it
to a real name on the way back. If the id alone selected the session, guessing another
caller's id would be enough to read their mapping out one placeholder at a time.

So the id alone does not select anything. The store key is a pair: a SHA-256 fingerprint
of the value of whichever header authenticates this provider (`authorization` for OpenAI,
`x-api-key` for Anthropic), salted with a per-process random value, together with the
client's id. A different credential is a different namespace, and a guessed id lands in
an empty one. A credential header that is present but empty counts as absent — an empty
string is not a namespace, it is every caller who sent nothing.

The salt is per process because the store must not hold anything from which a credential
can be recovered. Sessions do not survive a restart in any case — the store is in memory.

`Mapping::reserve_literals` already maps a placeholder-shaped token in the caller's own
text to itself, so an echo restores to exactly what was sent. That defence predates this
slice; the fingerprint is what keeps this slice from widening it into a hole.

## How the session survives the request

Masking calls the detector, so it runs across `await`. Restoration of a stream can run
for minutes after that. The two need different treatment, and picking the wrong split is
the one way this slice can hand a client the wrong person's name.

**The session lock is held for the masking phase and released before the upstream call.**
Restoration — buffered or streamed — works from a snapshot of the mapping taken at the end
of masking. Restoration is read-only by nature: it looks `[TYPE_N]` up and never adds. It
does not need the shared table, it needs the table that produced the text it is restoring.

Two consequences fall out. A stream, however long it runs or however badly it ends, holds
no lock. And two concurrent requests in one session serialize over the masking phase,
which is what keeps their numbering from interleaving.

`restore_stream` already takes `Mapping` by value; the snapshot fits its signature
unchanged.

### The alternative that was rejected

Seed a request-local copy from the session, mask without holding a lock, merge the new
pairs back at the end. It never holds a lock across `await`, which is why it is tempting.

It is wrong. Two concurrent requests both allocate `[PERSON_3]`, for two different values,
and both send it upstream before either merges. Whichever loses the merge is left with a
table in which a placeholder names somebody else, and restoration then puts the wrong
person's name into a response. The failure is silent, and it is precisely the failure the
project exists to prevent.

Holding the lock for the whole request — including the upstream call — is safe but makes
one stalled stream block its session indefinitely.

## Components

New module `gateway/src/session.rs`. `proxy.rs` is 1064 lines; a store with an eviction
policy does not belong in it.

```rust
pub struct SessionStore { inner: std::sync::Mutex<HashMap<SessionKey, Entry>>, limits: Limits }
struct Entry { session: Arc<Session>, last_seen: Instant }
pub struct Session { mapping: tokio::sync::Mutex<Mapping> }
```

Two lock types, deliberately. The store's is a `std::sync::Mutex` held only for map
operations and **never across `await`**. A session's is a `tokio::sync::Mutex`, because
that one is held across the detector calls.

The acquisition order is fixed: take the store lock, clone the `Arc<Session>`, release the
store lock, then await the session lock. Deadlock is excluded by construction rather than
by convention.

A poisoned store lock is recovered through `into_inner()` rather than propagated. Nothing
in that critical section can panic today, but if something ever does, one failure must not
turn every later request into a 500.

`AppState` gains the store. `Mapping` gains `#[derive(Clone)]`, a record of allocation
order, and one method:

```rust
/// Commit `other`'s new pairs in allocation order, stopping at `cap`.
pub fn absorb(&mut self, other: &Mapping, cap: usize);
```

`absorb` carries the `next` counter past pairs it declined to commit, so a number issued
to one value is never issued to another.

## Lifetime and bounds

**The header.** `X-Tessera-Session`, 1 to 128 characters from `A-Za-z0-9._:-`. Anything
else is refused rather than coerced into something acceptable.

**The raw id never reaches a log.** The client chooses it, so it may itself be personal
data — `patient-Weber-2026` is a plausible id and an unacceptable log line. A short hash
prefix goes to logs instead.

**Eviction runs on acquisition, with no background task and no timer.** Each acquisition
first sweeps entries idle for longer than `session_idle_secs`. At `max_sessions = 1000` a
full sweep costs less than a syscall, and in exchange there is no separate task whose
health is a new thing to reason about. It is also deterministic: the internal entry point
takes `now: Instant` and the public one supplies `Instant::now()`, so TTL and LRU are
tested without sleeping.

If the map is still full after the sweep and the key is new, the entry with the oldest
`last_seen` is dropped.

A request that is already holding an `Arc<Session>` when another request evicts it keeps
working against that `Arc` and completes normally; its commit is simply lost to a table
nobody will look up again. Eviction never interrupts a request in flight.

**Evicting a session is safe, and this is why the bounds can be aggressive.** The client
holds restored text — real names — and sends the history again on the next turn.
Detection runs over it, and the table is rebuilt from nothing. What changes is that
`[PERSON_3]` becomes `[PERSON_1]`. No value leaves unmasked, no placeholder reaches the
client unrestored. Eviction degrades coreference, never protection.

**The value cap.** At `max_session_values`, a new value still gets a placeholder in this
request's snapshot and is still restored in this request's response; only the commit to
the session is skipped. The snapshot is complete because it is the table that did the
masking — the cap bounds what outlives the request, nothing else.

**Values are never evicted from within a session.** Not under any memory pressure. A
placeholder dropped from a live session can come back from the model on the next turn,
and that is not a lost coreference but a request that dies with nothing to restore to. A
session grows to its cap and stops there; the whole session can be evicted, parts of it
cannot.

## Data flow

Only the masking phase of `handle()` changes. Everything after the upstream call is
untouched.

```rust
let pointers = provider.request_pointers(&body)?;

// Resolved before detection: a malformed header must cost nothing, not a
// second per 1 200 characters.
let session = state.sessions.resolve(provider, &headers)?;

let (masked, snapshot) = match &session {
    Some(session) => {
        let mut guard = session.mapping.lock().await;
        let mut work = guard.clone();
        let masked = mask_all(&state.detector, &body, &pointers, &mut work).await?;
        guard.absorb(&work, state.sessions.max_values());
        (masked, work)          // guard released here, before the upstream call
    }
    None => {
        let mut work = Mapping::new();
        let masked = mask_all(&state.detector, &body, &pointers, &mut work).await?;
        (masked, work)
    }
};
```

The detect-and-mask loop moves into `mask_all`, which both branches call. Left inline it
would exist twice and diverge at the first edit.

**A refused request leaves no trace in the session**, and this falls out of the shape
rather than needing its own mechanism: `work` is a copy, and `absorb` sits after the last
`?`. A detector timeout, an unusable span, an unrecognized body shape — the session is
left exactly as it was, and the next request numbers as though the failed one had not
happened. Otherwise a client whose detector blinked would carry a hole in its numbering
for the rest of the conversation.

## Fail-closed

A new `ProxyError::Session` maps to 400. Three cases, all decided before the upstream call:

- the header fails the length or alphabet rule;
- the header is present while sessions are disabled (`session_idle_secs = 0`);
- the header is present and the provider's credential header is absent, so there is
  nothing to build a fingerprint from.

The second and third refuse rather than quietly proceed without a session. The header is a
statement about required behaviour, and a client that asked for coreference and silently
did not get it is worse off than one that was told. The third would fail upstream with a
401 regardless; failing here names the actual problem.

Every existing refusal keeps its behaviour and its status.

## Configuration

Three keys, in a file that already rejects unknown ones:

```toml
session_idle_secs = 1800     # 0 disables sessions entirely
max_sessions = 1000
max_session_values = 1000
```

## Testing

**`session.rs`**, pure unit tests with the clock as a parameter — no sleeps:

- the same `(fingerprint, id)` returns the same session; the same id under a different
  API key returns a different one, and neither sees the other's values;
- an entry idle beyond the TTL is swept, and the request after it starts from an empty
  table;
- at `max_sessions` the oldest entry is the one evicted, and touching a session updates
  `last_seen` and saves it from eviction;
- header validation: length, alphabet, empty string.

**`mapping.rs`**: `absorb` commits in allocation order and stops at the cap; `next` steps
past what was declined, so no number is issued twice; a clone is independent of its
source.

**`proxy.rs`**, on wiremock as the existing tests are. The assertions are on what the fake
upstream *received*, not only on what the gateway returned:

- two requests in one session — one value, one placeholder, both times;
- **the exfiltration test, the one that matters.** A second caller, holding a different
  API key but sending the first caller's session id and a prompt containing `[PERSON_1]`,
  gets `[PERSON_1]` back verbatim — never the name from the other session;
- a refusal does not move numbering: one request fails at the detector, and the next
  receives the numbers the failed one would have taken;
- past the value cap, a value is still masked upstream and still restored in the response;
- with no header, behaviour is byte-identical to today's; with the header and sessions
  disabled, 400.

**Streaming.** Restoration runs from the snapshot. The claim that the lock is released
before the upstream call is tested directly rather than by racing: while a stream is open,
`try_lock` on its session must succeed. A race is not honestly testable; the structural
fact is.

## Out of scope

The audit log (the next slice), incremental detection (rejected above), surviving a
restart, sharing a session between processes, a management endpoint that drops a session,
and metrics. Each is its own slice.
