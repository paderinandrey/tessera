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

### Why a caller with no credential is not cached

The section above argues the credential into the key to stop one tenant learning,
from response time alone, that another submitted a byte-identical text. Where the
provider needs no credential — the demo stack, or a closed perimeter in front of
a self-hosted model — every caller presents none, they all digest alike, and that
timing oracle is exactly what the shared bucket restores.

So a request with no credential is served and never cached, the same way an
oversized detection is. The cost is real and worth naming: such a deployment gets
no cache at all until its callers carry distinguishable credentials.

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

### The rule this section keeps rediscovering

Six times in this slice the version described what the package *ships* instead
of what the process *holds*, and each was found the same way: the pinned
revision rather than the loaded weights; two named artifacts rather than the
directory the loader reads; the catalogs and weights but not this package's own
source; the inference tree but not the validators'; and finally the packaged
catalog while the detector held a caller's.

The rule is one sentence — **derive the version from the object, never from the
package** — and it is written into `detector_version`'s docstring rather than
only here, because every one of those six was added by someone who had the
design in front of them and still reached for the file on disk.

### Why the version is not just the model revision

Spans are determined by the weights *and* by `catalog/identifiers.yaml` and
`catalog/ner.yaml` — a threshold edit changes what is detected without touching
`HF_REVISION`. The version is therefore a digest over both catalogs, the weights
themselves, this package's own source, and the declared versions of the
dependencies both layers pull in — `schwifty` and `python-stdnum` decide which
IBANs and tax numbers validate, as surely as the weights decide which names are
found.

Not over `HF_REVISION`, which was this design's first answer and was wrong.
`TESSERA_NER_MODEL` is a supported override, so the pinned revision names the
weights a detector *would* load rather than the ones it did: two replicas, one
overridden, would report one version while returning different spans, and the
gateway would key them together. The digest is taken over the bytes of every
artifact in the resolved directory rather than a named list, because the first
attempt at this fix enumerated two files and missed the tokenizer — whose
`return_offsets_mapping` produces the very offsets being cached. Walking the
directory also makes the digest independent of the path the weights were found
at. It is computed once when the model is resolved, since the graph is 1.15 GB
and this value is read on every `/detect`.

The source digest closes the third version of the same hole: chunking, the token
window and conflict resolution all change spans with the weights and catalogs
untouched, and weights are fetched separately from the image, so a new build on
the same weights is a real deployment rather than a hypothetical one. Its own
boundary is stated where it is computed — a dependency upgrade changes spans too
and no digest here sees it. Pinning the packages that participate in inference is
the follow-up; hashing the whole lockfile is not, since it would invalidate every
entry in the fleet on a `pytest` bump. The dependency half is resolved from
`importlib.metadata`, walking what the inference root declares rather than a list
someone maintains — the fourth time in this slice that a hand-written list was
the wrong answer. Its boundary is the mirror image of the source digest's: it
covers what is declared, not what is merely imported.

There are two roots rather than one, and the distinction is structural rather
than a list creeping back. This package's own distribution metadata names the
deterministic layer's dependencies, which is where `schwifty` and
`python-stdnum` arrive. It cannot name the NER group's: a PEP 735 dependency
group is not part of installed distribution metadata the way `[project
.dependencies]` is, so the inference tree has to be reached from its own root.
A third root would be a list again; these two are the two places the metadata
actually is.

**All four halves fail loudly rather than digest less.** A weights file that
cannot be read, or a dependency whose version cannot be resolved, fails the
detector's startup. The alternative — a digest computed over what happened to be
available — reports a version that claims coverage it does not have, which is the
one outcome every part of this design is arranged to prevent.

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

Two keys. `detection_cache_entries`, defaulting to 10 000, bounds how many
detections are held; zero disables the cache, and unlike `max_sessions`, zero is
a meaningful setting rather than a validation error. `max_spans_per_entry`,
defaulting to 250, bounds how large any one of them may be; following
`max_session_values`, zero is a validation error while the cache is enabled.

**An entry count alone does not bound memory, because an entry has no fixed
size.** Measured with a counting allocator against the real types: 264 bytes
fixed plus 46 per span found in the text, and those are requested bytes, so they
exclude malloc rounding and hash-table slack and are a floor rather than an
estimate. Chat-shaped text with a name or two lands near 350 bytes and ten
thousand of those is a few megabytes. A 20 KB tool result with an entity every
25 characters is some 800 spans and about 37 KB in a single entry, and ten
thousand of those is 352 MB. Nothing in the request path bounds it there either:
axum's 2 MB body limit permits roughly 80 000 spans in one text, about 3.7 MB in
a single entry. An entry count multiplied by an unbounded term is not a memory
budget.

So the design ships a second key, `max_spans_per_entry`. A detection with more
spans than that is served normally and simply not stored — declining to cache is
never a refusal, which is invariant 4 below. This is the inner bound the session
store already has in `max_session_values` beside `max_sessions`, and the two
pairs exist for the same reason. The two defaults multiply out to
`10 000 × (264 + 250 × 46)` ≈ 118 MB, which is the figure an operator gets
without reading this document.

**Where the default comes from, and where the 25-character assumption came
from.** The density used above to argue the ceiling — an entity every 25
characters — was assumed, not measured, and it is about twenty times real. Run
against the detector over 6 000-character samples: a `git log --stat` gives 1.33
spans per 1 000 characters, README prose 2.50, and Rust source 1.00. The
evaluation corpus gives 28.67, but it is a detection benchmark of concatenated
dense PII sentences and is a proxy for nothing.

At 250 spans the cap admits prose to roughly 100 KB, logs to 188 KB and source
to 250 KB, so every realistic single tool result is cached and the pathological
shapes that motivated the bound are not. The number is chosen from that range
rather than from the memory side alone, because the value of caching a text grows
with its length: a cap trims the most valuable entries first, which is the wrong
end to trim on an arbitrary number.

Four samples from one repository are not a survey of customer traffic. The
measurement that would settle it is spans per 1 000 characters over a day of real
gateway traffic, which the journal is one field away from being able to answer.

**A uniformly dense text crosses the cap at single-digit kilobytes, and that is
a limit rather than a number to keep raising.** The densities above are code,
logs and prose — the shapes a coding agent sends. A contact list or an intake
form is not shaped like either. The only dense-PII sample in this repository is
the evaluation corpus, which annotates about 20 spans per 1 000 characters across
the whole file and about 30 across its eighty non-negative rows; a real contact row, at
roughly five entities in eighty characters, exceeds both. It characterises
nothing about a buyer's traffic and is not offered as doing so — it is an
illustration of where the cap lands on text that is dense throughout.

It cannot be more than that, and the reason is worth stating rather than
implying: every row of that corpus is a single rendered sentence under 126
characters. `generate.py` calls them documents; nothing in this repository is
shaped like one. The density figure below is therefore an illustration awaiting
its measurement, and the measurement is the one named above — spans per 1 000
characters over real gateway traffic.

At that density a single text crosses 250 spans somewhere between 8 and 16 KB, so
a uniformly dense document arriving as one message is declined and rescanned on
every turn. Two things bound how often that bites: the cache keys per text rather
than per document, so a conversation *about* a dense file is many short turns
that all cache normally; and a long document is rarely uniform — a contract with
personal data in its header and signature block is prose in between. Ordinary
correspondence is prose-shaped at nearer 2.5 spans per 1 000 and is not affected.

Raising the cap for a deployment whose texts really are dense throughout is a
deliberate lever with a priced cost — the formula is in front of the operator —
but it cannot be the default, because the same number multiplies against ten
thousand entries.

**Why this is in the slice rather than after it.** An earlier draft of this
document scoped the inner bound to a follow-up, on the grounds that the ceiling
converges rather than leaks and the scaling law could be documented in the
meantime. That reasoning missed the coupling that decides it: `SessionStore`
lives in the same process, cache entries expire only by count and never by time,
so the ceiling is reached rather than approached — and an OOM kill takes the
session mappings with it. This cache is built on the principle that losing an
entry costs time and not correctness. It must not be the component that destroys
the one where correctness lives.

It is on by default. The cache changes how fast the gateway answers and not what
it sends, so an operator who never reads the documentation should get the fast
behaviour — which is precisely why the product of the two defaults has to be a
memory figure that operator would accept unread. The reason to reach for either
key is a memory budget, not a policy.

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
8. A detection above `max_spans_per_entry` is served in full and not stored.
9. A hit returns the offsets that were computed for that text, not another's.

Store mechanics are unit tests in `detection_cache.rs`. Client behaviour goes
through `wiremock`, already used in `detector.rs`: a miss reaches the server, a
hit does not, and a partial run reaches it twice for one text. On the detector
side, a test that the version is stable across restarts and changes when either
catalog is edited.

Invariant 7 is asserted rather than assumed. The evidence layer must not get
weaker because an answer arrived from memory.

Invariant 9 is the one this design argues hardest for and the easiest to leave
untested, because every cheaper assertion passes without it: a hit that applies
another text's offsets returns the right number of spans of the right types, and
only the bytes forwarded upstream reveal it. It is pinned by comparing the body
sent to the provider across a miss and a hit.

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

The window is also wider than "one request" makes it sound. `mask_all` walks a
conversation's texts in order, so on the first request after an upgrade the
history hits the cache under the old version *before* the new turn misses and
reveals the new one — a single request can straddle the change rather than
merely the sequence of them. What the miss buys is that the request after it is
clean.

Polling `/health` would close this and stays out of scope, for the reason given
under Out of scope rather than because the argument is weak. The condition for
revisiting is unchanged and unmet: someone upgrades a detector on a live stand
and observes stale spans.

That reasoning assumed versions only move forward, which nothing guarantees: a
digest carries no order, and a slow response from a not-yet-upgraded replica can
arrive after a newer one. So a version change now sweeps every entry that does
not match it, in either direction, under the same lock that records it. The
invariant is that every entry present matches the known version, rather than the
weaker one it replaced — that stale entries merely fail to match and age out.

The honest cost: a fleet left permanently mixed sweeps on every insert and its
hit rate goes to nothing. That is this cache being correct and slow rather than
fast and wrong, which is the same choice it makes on saturation and on a poisoned
lock.

More precisely, the window closes on the first *cacheable* miss. A detection
declined for exceeding `max_spans_per_entry` returns before the new version is
recorded, so a run of oversized responses holds the window open. This is the
price of deciding the cap before taking the lock, and it is worth paying: the
alternative puts work in front of every decline to shorten a window that only a
mid-process detector upgrade can open at all.

Condition for revisiting: someone upgrades a detector on a live stand and
observes stale spans. Then `/health` polling is added. This is cheaper to decide
on evidence than on paper.
