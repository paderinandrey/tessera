# The admission bounds charged for work the cache had already done — design

The cheapest half of #28, which the whole-branch review of #29 identified and
nobody had taken: *"Charging only cache-missing text recovers about 46% of the
budget every turn after the first, without moving a default. That looks like the
cheapest large win available here."*

> Decisions were taken without the user in the room, on a standing instruction.
> The one worth their attention is **what a caller is promised**, in the third
> section: this changes the meaning of a bound, not its number.

## The defect

`proxy::handle` counts every tool character and every tool document against
`max_tool_chars` and `max_tool_calls`, with no reference to `DetectionCache`.

Both bounds are on **what the caller waits for**. `max_tool_chars`' own
documentation says so and goes out of its way to deny it is a timeout budget:
*"`detector_timeout_secs` bounds each detector call on its own and never their
sum; this caps how long a caller waits for the whole request."*

A text the cache answers costs no wait. Charging it spends the budget on work
that never happens.

**Measured, on the ten pinned Claude Code tool definitions**: 9 193 of 20 000
characters and 20 of 40 calls, on *every* turn rather than the first, because
the definitions are byte-identical afterwards and the cache serves them. In a
live session the budget left for a new tool result was permanently about 10 800
characters — and that, rather than a low ceiling, is the real mechanism behind
the README's "the bounds admit roughly twenty tools".

## The change

Charge a text only when `DetectionCache` will not answer it.

**`DetectionCache::contains`**, which repeats `get`'s derivation — capacity,
credential, version, key — rather than calling it. `get` bumps `clock` and
stamps `entry.used`, so asking through it would let a request that is merely
being *priced* promote an entry over one being served. A caller could then steer
another tenant's evictions by sending texts it never intends to have detected.
The lock is taken immutably, so the compiler enforces what the comment claims.

**`document_detection`**, one function returning a document's leaves and the
single text the detector will be shown for them. `handle` prices what `mask_all`
will ask, which means hashing the exact string `detect` will be handed — and a
second copy of the join would let the two drift into charging for one text and
detecting another. That drift has no symptom: the bound would simply stop
meaning what it says, in whichever direction the copies diverged.

A side effect worth naming: **`Joined::separator_chars` is now needed only by a
test.** The counting loop used to charge separators through it — a second copy
of the join's arithmetic sitting beside the join. It charges `joined.text()`
directly now, so separators are counted by *being in the string the detector
reads*. One fewer pair of places to keep in step.

## What a caller is promised, which is what changed

The bound was on the caller's text. It is on the caller's text **this gateway
has not seen**.

So the same request can be admitted after a turn that warmed the cache and
refused after an eviction, where before it was refused either way.

**The failure direction is the safe one.** A refusal is what the request already
got, so nothing that works at the shipped defaults stops working. But a client
who comes to rely on the wider budget will meet the narrow one eventually, and
the narrow number is what they are owed. `config.rs` and the README both say so
where a caller reads them.

**A caller with no credential is charged for everything**, because the cache
declines to answer without one — deliberately, so credential-less callers do not
share a bucket in which a hit is a timing oracle. The bounds inherit that by
asking the same cache the same question, and a test holds it.

**A `true` from `contains` is not a promise.** An eviction between pricing and
detecting turns a free text into a paid one; the request is served anyway and
waits longer than the bound predicted, which is what every request did before.

## What this does not do

**It does not lift the ceiling on a large first-seen text**, which is what #28
is actually about. A 50 KB tool result is still roughly 92 seconds of wall clock
and still refused. This recovers the budget that was being spent on repeats; the
throughput problem is untouched, both quantised-weight candidates are measured
and rejected, and chunking across replicas remains the risky one.

## Testing

Nothing in the existing suite noticed the change — 551 tests passed before and
after — which is the same gap the change itself is about: a bound nothing
exercises is a number, not a rule.

- **a tool text the cache already holds is not charged again**: one description
  fits a bound of 30; two totalling 40 fit on the next turn because 20 of them
  are cached; and **the control** — two *different* descriptions of the same
  total size, still refused. Without the control the first assertion is
  satisfied by a bound that simply got wider;
- **the same for the call bound**, at a bound of one call;
- **a caller with no credential is charged for everything**, so repeating a text
  buys them nothing;
- **asking whether a text is cached does not make it recently used**: twenty
  asks about the oldest entry, then an insert, and the oldest is still the one
  evicted;
- **`contains` answers exactly what `get` would**, across a hit, a miss, another
  credential, no credential and a disabled cache.

Mutations:

- **charge every text again** → the char-bound test fails;
- **let `contains` promote the entry** → the LRU test fails, with its own
  message. Reaching it needed the lock rebound as `mut`: the first attempt
  did not compile, which is the property being held by the type system rather
  than by the test;
- **let `contains` fold a missing credential to an empty one** → **survived**,
  and the reason is worth recording rather than papering over. `insert` refuses
  a credential-less detection too, so nothing is ever stored under an empty
  credential and the lookup misses either way. The guard is defence in depth,
  exactly like `get`'s capacity check, which the module already documents as
  such. A surviving mutation is a missing test *or* a guard that is only
  defence in depth; claiming the test pins it would have been the false half.
