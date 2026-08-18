# A detection cache, so a conversation is not rescanned every turn

## The problem

Detection runs over every text on every request. A client resends the whole
conversation each turn, so turn ten pays for turns one through nine again, and
the cost of a conversation grows quadratically in its length.

Measured on the compose stack at `b3d5e4a`: **20.3 CPU-seconds per 1 200
characters**, about 59 characters per CPU-second. A ten-turn chat of 400- and
600-character turns scans ~49 000 characters in total and costs ~830 CPU-seconds
— roughly fourteen core-minutes for one conversation.

For a coding agent the same arithmetic stops being an expense and becomes a
wall. One tool result of 50 KB costs ~92 seconds, which is not a slow answer but
a refusal: `detector_timeout_secs` defaults to 30. A 200 KB agent context costs
~56 core-minutes **per request**, which no cluster size fixes.

Nothing here is wasted work in the interesting sense. The gateway is rescanning
text it has already scanned, whose spans it already computed, and which cannot
have changed — history is immutable, and the client resends it verbatim.

## What this slice is for

Turning that quadratic into a linear one, for the workload that needs it most,
without adding a second place where personal data lives.

It is a precondition for the tool-aware gateway (issue #22) rather than a
follow-up to it: supporting tool fields without an answer to latency does not
open Claude Code, it moves its death from "rejected shape" to "detector
timeout". Both are 503 to the person typing.

## The decisions

1. **The cache holds no submitted text.** Keys are digests; values are spans,
   which carry a type and offsets and never a value.

2. **A hit is never worse than a fresh call.** Only a complete detector run is
   stored, so a cached answer cannot be a degraded one.

3. **The detector's version is part of the key**, so upgrading the model or
   editing a catalog invalidates every entry without an operator ritual.

4. **A miss is not a refusal.** The cache may always be bypassed; nothing it
   does can fail a request.

5. **It lives inside `DetectorClient`**, so no call site can forget it.

### Why the cache cannot be scoped to a session

The obvious home for it is the session table: same conversation, same texts.
But a session exists only when the client sends `X-Tessera-Session`, and no
harness sends it — there is nowhere in Claude Code or Codex to configure such a
header, and `session::key_from` returns `Ok(None)` without it, giving a mapping
scoped to the single request.

So the clients that need the cache most would be exactly the ones a
session-scoped cache never serves. It is keyed by credential and content
instead, and works whether or not a session is attached.

### Why the key carries a digest of the credential

Keying by content alone would maximise hits and turn the cache into an oracle:
tenant B could learn from response time that tenant A had previously submitted a
byte-identical document. The spans themselves leak nothing — B already has the
text it sent — but the timing does, across a tenant boundary, which is the one
boundary this product sells.

### Why the digests are not truncated

A collision on the text digest applies one text's offsets to another. When the
two are the same length `mask` will not reject them: the spans are in range and
do not overlap, so the wrong ranges are masked and a real value leaves the
process unmasked. That is raw egress, the failure this gateway is built to make
impossible.

Full 32-byte SHA-256 removes the question rather than bounding it. At the
default ceiling the extra sixteen bytes per key cost 160 KB, which is not a
trade worth making against that outcome.

### Why the salt is minted per process and never persisted

The cache is in memory and must not survive a restart, so its keys never need to
be comparable across runs. A fresh random salt per process makes them
meaningless outside it, and keeps the cache from accidentally becoming a second
stable identifier for the same tenant alongside the journal's.

It is deliberately **not** the audit salt. That salt lives on disk beside the
journal precisely so digests stay comparable across restarts; borrowing it here
would give cache keys a persistence they must not have.

### Why the version travels in the detect response

The version could be polled from `/health`, which would keep it fresh
independently of traffic. It arrives in the `/detect` response instead, because
then an entry is always stamped with the version that produced it, and no timer,
no background task and no "before the first successful poll" state exist.

The cost is honest and stated in **Known limits** below.

### Why the version is not just the model revision

Spans are determined by the weights *and* by `catalog/identifiers.yaml` and
`catalog/ner.yaml` — a threshold edit changes what is detected without touching
`HF_REVISION`. The version is therefore a digest over the pinned revision and
the contents of both catalogs.

### Why a degraded run is dropped rather than keyed

Keying by `layers_run` would be strictly correct and would keep results of
different layer sets from mixing. It would also spend the ceiling on entries
nobody wants: a deterministic-only result is worth having once, while the NER
layer is down, and worth nothing afterwards.

Dropping it buys a stronger and simpler invariant — everything in the cache is a
complete run — at the cost of no hits during a degradation, which is a period
when the deployment has a larger problem.

## Shape

`gateway/src/detection_cache.rs`, one type, owned by `DetectorClient`.

```rust
pub async fn detect(&self, text: &str, credential: &[u8])
    -> Result<Vec<Span>, DetectorError>
```

The signature gains a parameter and nothing else changes for callers. `mask_all`
does not know a cache exists and cannot bypass it; the second detection call
site that arrives with tool support inherits it for free.

The caller passes the credential, not a digest of it. The salt then never leaves
the cache, and no other module needs to know that digesting happens at all —
`proxy` already holds the credential, via `session::credential_of`, and hands it
down unchanged.

Internally it is a `Mutex<HashMap<Key, Entry>>` where `Entry` carries the spans
and a monotonic last-use counter, and saturation evicts the least recently used
through one ordered scan. This deliberately repeats the shape of `SessionStore`
rather than introducing an LRU crate: the pattern is already in the codebase and
already tested, and a dependency is a supply-chain question in a product sold
into closed perimeters.

```
Key = (version_digest, tenant_digest, text_digest)   // three [u8; 32]
```

All three are 32-byte arrays. `tenant_digest` and `text_digest` are SHA-256 over
the process salt and the value; `version_digest` is SHA-256 over the version
string the detector reported, so the key is one fixed-size type throughout and
holds no `String` — submitted text cannot be put there even by mistake.

## Flow

**Miss.** `/detect` is called as today. The response is now parsed in full,
including `version` and `layers_run`. The version is remembered as the current
known one. The entry is stored only if `layers_run` covers every layer in a list
the gateway holds itself — `["deterministic", "ner"]` — and a partial run is
served and forgotten.

The list is the gateway's own copy for the same reason `ENTITY_TYPES` is: asking
the detector which layers count as complete would be worthless against a
detector that answers "the ones I ran". It is two entries and it changes when a
layer is added, which is a deliberate change in two places rather than a silent
divergence — `scripts/check_entity_types.py` is the precedent for guarding that
in CI, and this list gets the same treatment.

**Hit.** Spans are returned from memory and the detector is not called. Entries
whose version differs from the last known one are unreachable — they are not
swept, they simply stop matching and age out through the ceiling.

**Disabled.** `detection_cache_entries = 0` makes `detect` behave exactly as it
does today.

## Errors

The cache is an optimisation, and that governs every failure path: **it may not
fail a request.** A poisoned mutex degrades to "no cache" and the call proceeds
to the detector. Saturation evicts. Detector errors are never stored.

The asymmetry with `SessionStore` is deliberate and worth stating, because the
two look alike. Losing a session entry is a confidentiality problem, so
saturation there refuses the request. Losing a cache entry costs time, so
saturation here evicts.

## Configuration

One key, `detection_cache_entries`, defaulting to 10 000 — single-digit
megabytes at roughly 300 bytes an entry. Zero disables the cache; unlike
`max_sessions`, zero is a meaningful setting rather than a validation error.

It is on by default. The cache changes how fast the gateway answers and not what
it sends, so an operator who never reads the documentation should get the fast
behaviour; the reason to reach for the key is a memory budget, not a policy.

Added to `gateway/tessera.example.toml`, `deploy/tessera.container.toml` and
`deploy/tessera.demo.toml`.

## Testing

Invariants the tests must pin, each proved by breaking it and watching the test
fail:

1. A partial run is not cached — a hit is never worse than a fresh call.
2. A different detector version invalidates every entry.
3. The same text under a different credential misses.
4. A miss is never a refusal, under any cache state including a poisoned lock.
5. `detection_cache_entries = 0` reproduces today's behaviour exactly.
6. Saturation evicts the least recently used rather than refusing.
7. The journal's `types` and `spans` counts are identical for a hit and a miss.

Store mechanics are unit tests in `detection_cache.rs`. Client behaviour goes
through `wiremock`, already used in `detector.rs`: a miss reaches the server, a
hit does not, and a partial run reaches it twice for one text. On the detector
side, a test that the version is stable across restarts and changes when either
catalog is edited.

Invariant 7 is asserted rather than assumed. The evidence layer must not get
weaker because an answer arrived from memory.

## Out of scope

Polling `/health` for the version (see Known limits), persisting the cache across
restarts, sharing it between replicas, caching at any granularity other than the
text the detector is called with, and the prefilter and field-selection work that
sits behind the same latency goal.

## Known limits

**A hit does not refresh the known version.** If the detector is upgraded while
the gateway keeps running, entries from the previous version stay reachable until
the first miss reveals the new one. On agent traffic misses are continuous —
every turn carries new text — so the window is one request. The pathological case
is a deployment that, after a model upgrade, only ever replays byte-identical
text.

Condition for revisiting: someone upgrades a detector on a live stand and
observes stale spans. Then `/health` polling is added. This is cheaper to decide
on evidence than on paper.
