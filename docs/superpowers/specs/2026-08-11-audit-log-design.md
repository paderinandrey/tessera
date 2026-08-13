# Audit log: evidence that a request was pseudonymized before it left the perimeter

## The problem

Tessera's claim to a DPO is that personal data was replaced with placeholders
before anything reached the model provider. Nothing in the gateway records that
this happened. `main.rs:35` emits one line at startup and the process is silent
afterwards, so a deployment that has masked ten million requests and a
deployment that has been fail-open the whole time produce byte-identical
evidence: none.

The README already promises the artifact — "append-only JSONL audit" among the
design principles, "audit logs never contain original values" beside it, and
"sessions and audit are the next slices" in the architecture. Sessions closed
with PR #17. SECURITY.md names "original values leaking into logs, audit
records, error messages, or traces" as a category of particular interest, which
presumes audit records exist.

This slice is the technical measure becoming *demonstrable*. Detection quality
is what the product does; the audit log is how anyone else can tell.

## What the log is for

The reader is a DPO or an external auditor, and the question the log answers is
"was this traffic pseudonymized before it left". That choice has a consequence
the rest of this design follows from: **the record is part of the control, not
part of the observability**. A record that cannot be written is a request that
cannot be served.

Operational debugging is not this log's job. `tracing` remains what it is —
best-effort, and free to say more.

## The decision

1. **Two records per request, correlated by `request`.** The `masked` record is
   written and fsynced *before* the upstream call and may refuse the request.
   The `outcome` record is written when the request ends, without fsync, and
   may not refuse anything.

2. **The gateway does not start without an audit log.** `audit_path` is a
   required config key; a file that cannot be opened for appending is a startup
   failure.

3. **The record counts, it does not sample.** Per-type counts of distinct values
   and a total of occurrences. No values, no hashes of values, no offsets, no
   placeholder names.

4. **Attribution is by salted digest**, of the caller's credential and of the
   session, with a salt that survives restart.

5. **The per-request record is an RAII guard with explicit outcome signals.**
   Each way a request can end tells the record what happened; `Drop` writes
   whatever it was told, and writes `aborted` when it was told nothing. The
   guard guarantees that a line is always written — it does not guess what the
   line says.

### Why write before the call rather than after

A record written after the response proves nothing about the request that
crashed the process. The gap between "bytes went to OpenAI" and "the journal
knows about it" is exactly the interval an auditor is entitled to ask about, and
writing afterwards makes that interval unbounded. Writing before makes it
bounded by fsync.

It costs an fsync per request. Against a detector round-trip of roughly a second
per 1 200 characters (see the latency table in the README), a few milliseconds
of disk is noise. It is not noise in principle — it is the price of the claim.

### Why one record cannot do both jobs

At the moment the evidence must be durable, the outcome is unknown. On a
streamed request it stays unknown for minutes. A single record would have to
choose between being written early and being complete; two records need not
choose.

## The record

Two shapes. A streamed request with a session:

```json
{"ts":"2026-08-11T09:14:22.418Z","event":"masked","request":"7f3a9c1e04b25d68","provider":"anthropic","route":"/v1/messages","tenant":"a41f9c02","session":"3bd7e105","stream":true,"texts":4,"spans":9,"types":{"PERSON":2,"IBAN":1,"HEALTH":1}}
{"ts":"2026-08-11T09:14:37.902Z","event":"outcome","request":"7f3a9c1e04b25d68","tenant":"a41f9c02","session":"3bd7e105","upstream":true,"status":200,"result":"completed","error":null,"ms":15484}
```

A request refused before the upstream call is one self-contained line:

```json
{"ts":"2026-08-11T09:14:22.418Z","event":"outcome","request":"91c4a70b6de83f12","tenant":"a41f9c02","session":null,"upstream":false,"status":502,"result":"refused","error":"detector_timeout","ms":30012}
```

`tenant` and `session` are repeated on the outcome line rather than left to a
join on `request`: a request refused before `masked` has no `masked` line to
join to, and a refusal attributable to nobody is exactly the line a DPO reading
a run of refusals cannot use.

**`types` counts distinct values, `spans` counts occurrences.** The difference
between them is the coreference the product sells: nine mentions, four people.

Both are computed from the spans the detector returned for *this request's*
texts, not from the session's table. A session-backed mapping is seeded from
earlier turns (`proxy.rs:153` clones the guard), so counting the table would
report the conversation's accumulated total on every turn and the record would
stop describing the request. `mask_all` returns the spans it counts, rather than
dropping them as it did before this slice.

**`result` has four values, and each is signalled rather than inferred.**

- `completed` — the gateway produced a whole response. Not "the client received
  it": for a buffered response the bytes are handed to axum, and whether the
  client's connection then survived is not something this gateway observes. The
  log records what the gateway did.
- `refused` — the request ended with an error, either before the upstream call
  (`upstream: false`) or after it, since a response carrying a placeholder no
  mapping knows is refused too.
- `stream_failed` — bytes had already gone out, so the stream ended mid-flight
  instead. `status` is the 200 the client already received and `error` names why
  it stopped.
- `aborted` — the record was dropped without any signal. In practice this is a
  client that disconnected mid-stream, which is the one ending the streaming
  code does not reach a `return` for.

The fourth value exists because the alternative is a lie. A guard that assumed
`completed` on an unsignalled drop would report success for every abandoned
stream.

**`texts` is how many text fields were masked** — the length of
`provider.request_pointers`. A conversation history is many fields, and a record
that says nine spans without saying across how much text invites the wrong
reading.

**`request` is eight random bytes in hex**, from `getrandom`, which the gateway
already depends on. It is not derived from anything in the request: an id that
were a hash of content would be a fingerprint of content.

**`error` is a fixed vocabulary, not `to_string()`.** One class per variant of
`ProxyError` and `StreamError` — `detector_timeout`, `session_saturated`,
`shape_unsupported`, `stream_unrestorable`, and so on. The error messages do not
carry submitted text today, but a vocabulary makes that structural rather than
observed: there is no expression in the audit writer that could interpolate a
message.

**`upstream` states plainly whether bytes left the perimeter.** It is derivable
from whether a `masked` line with the same `request` exists, but a refused
request has only one line, and that line should answer the central question
without a join.

It is set before the provider call rather than after it returns, and that
asymmetry is deliberate: a request that times out or is reset mid-flight *did*
send its bytes, so a flag set only on success would under-report — and for this
journal a false "nothing left" is far more dangerous than a false "something
left". The claim is therefore withdrawn in exactly one case, where the opposite
is definitively knowable: `reqwest::Error::is_connect`, a connection that was
never established (a refused port or a failed DNS lookup), and nothing else.

**`tenant` and `session` are digests the audit writer computes itself**, 16
bytes each, rendered as 32 hex characters. The example above shortens them for
readability; the real fields are long.

The reason for a digest at all is the one `SessionKey::digest()` (`session.rs:95`)
was written for: the raw session id is client-chosen, and `patient-Weber-2026`
is a plausible id and an unacceptable log line. But that method is not reused
here, for three reasons, each of which alone would be enough:

- **It keeps four bytes** (`session.rs:100`). Thirty-two bits are ample for a
  debug label on a store that holds a thousand entries and dies with the
  process. They are not ample for a journal: at 100 000 distinct sessions the
  probability that some pair collides is 0.69, and a collision merges two
  callers into one audit identity — corrupting precisely the field's purpose.
  Sixteen bytes make it unreachable. Attribution is worth more bytes than a
  debug label is.
- **Its salt is per-process** (`session.rs:53`), which is right for sessions,
  whose data dies with the process, and wrong for a journal read months later:
  the same tenant would appear as a different one after every restart. The
  audit writer takes its own salt from `<audit_path>.salt` — 32 bytes, mode
  0600, created on first run and reused afterwards. Sessions keep their
  ephemeral salt; the two want opposite things and should not share one.
- **It covers credential and id together**, so a request with no session header
  has no fingerprint at all — the credential is hashed only inside
  `SessionKey::new` (`session.rs:82`). `tenant` must exist whether or not a
  session was asked for, so it is a digest of the credential alone, and
  `session` is a digest of credential and id. A request without the header
  records `tenant` and a null `session`.

**`ts` is RFC 3339 UTC**, which adds the `time` crate. An audit line whose
timestamp needs a converter before a human can read it is a worse artifact than
one dependency is a cost.

## The module

`gateway/src/audit.rs`, two types.

`Audit` lives in `AppState` beside `sessions`: the append-only file descriptor
behind a `std::sync::Mutex`, plus the salt. The mutex is deliberate rather than
relying on `O_APPEND` atomicity — a record with a long `types` map can exceed
the size at which that atomicity holds, and interleaved lines in the evidence
base are worse than a lock held for microseconds.

`Record` is a cheap clonable handle to per-request state — the accumulated
fields behind a mutex, plus a reference to `Audit`. The `outcome` line is
written when the **last** handle drops. That indirection is what lets the record
outlive `handle` on the streaming path while still being finalized by the
wrapper on the buffered one.

It is constructed in the `openai` and `anthropic` wrappers (`proxy.rs:253`,
`proxy.rs:264`), not inside `handle`, and this is the correction that matters.
`handle` returns `Result<Response, ProxyError>`, and a `ProxyError` does not
become a status until `into_response` (`proxy.rs:31`) runs in the wrapper. A
guard constructed inside `handle` and dropped on its return would have to invent
both `status` and `error`: a bare `?` unwinds past the guard without telling it
which failure occurred. The wrapper is the first place where the outcome is a
value one can read.

So the wrapper holds one handle, passes a clone into `handle`, and on return
signals `refused(&error)` or `completed(status)` before dropping its own. For a
buffered response its handle is the last one, and the line is written there.

`handle` fills the fields as they become known — provider immediately, `tenant`
and `session` after `key_from` (`proxy.rs:142`), `types`/`spans`/`texts` after
`mask_all` — and calls `masked()` once, immediately before
`state.upstream.post(...)` (`proxy.rs:172`). That call serializes, writes and
fsyncs inside `spawn_blocking`, so a slow disk does not occupy a runtime worker,
and it is the one audit call that returns a `Result` and can refuse the request.

The fsync is issued outside the sink's lock, through a second descriptor onto
the same file: `Sink` writes lines under the lock, `Flush` commits them without
it. Otherwise a slow disk would still reach the runtime, by a different route —
the outcome line is written from `Drop`, on whatever thread holds the last
handle, and it would queue behind some other request's fsync for the whole
round-trip. Ordering survives the split because an fsync commits everything
written to the file before it rather than one particular write.

The streamed path passes a clone into `restore_stream` (`proxy.rs:215`)
alongside `mapping`, so the wrapper's handle is no longer the last and the line
waits for the stream. `stream.rs` does change, contrary to what an earlier draft
of this design claimed: its generator has three distinct exits — the upstream
connection breaking (`stream.rs:643`), restoration failing mid-stream
(`stream.rs:657`), and restoration failing on the final flush
(`stream.rs:661`) — and the first two end in a bare `return` while the third
falls off the end of the generator. No `Drop` can tell the three apart, nor tell
any of them from a success. Each gains one call naming what happened. A client that disconnects reaches none of
them, the generator is simply dropped, and that is the ending `aborted` exists
to describe.

`Drop` being neither async nor fallible is not a limitation here. It matches a
decision already made: the outcome line does not fsync and may not refuse.

## Failure

**Startup.** `audit_path` has no `serde` default, so its absence is a config
error from the existing `deny_unknown_fields` parser (`config.rs:16`) rather
than a silent empty string. `main.rs` opens the file with `O_APPEND | O_CREAT`
before `bind`, and fsyncs the containing directory once — on a first run the
directory entry itself is what a machine failure would lose, for the salt as
much as for the journal, so that one fsync comes *after* the salt is minted and
covers both entries. Losing the salt's entry alone is the expensive half: by
the rule below, a journal that survives without its salt refuses every restart
until an operator intervenes. A file that cannot
be opened stops the process. A `.salt` file that exists but is not exactly 32
bytes also stops it: regenerating a salt silently would split the journal in the
middle, and a journal that quietly renumbers its tenants is worse than one that
refuses to start. A salt that is *absent* stops it for the same reason, unless
the journal has no bytes in it — emptiness by length rather than by existence,
since `open` creates the journal before the salt is read. That one exception is
what keeps a first run a first run and keeps external rotation working: a
journal moved aside beside a kept salt is an empty journal with a valid salt,
and the digests carry on unchanged.

**A journal that ends mid-record** is the cost of the outcome line not being
fsynced: a process or machine death can leave a partial JSON object with no
newline after it. Reopening in append mode would then concatenate the next
record onto that fragment, so a crash would corrupt not only the line it
interrupted — which is unavoidable — but also the first record written after the
restart, which is a `masked` line that was fsynced precisely because it is
evidence. So `open` appends one newline when the journal is non-empty and does
not end in one, and warns.

The three alternatives are worse. Refusing to start makes a failure the journal
inflicted on itself require an operator before the gateway serves again.
Truncating the fragment destroys a record in an append-only evidence file to
tidy it, and an interrupted record is itself something an auditor may want.
Doing nothing is the bug. The newline is not fsynced and nothing waits for it:
the write is idempotent, so a second crash before it reaches the disk is
repaired by the next start, and the next `masked` line fsyncs, which commits
everything written to the file before it. It runs *after* `load_salt`, so the
one startup that refuses leaves the journal byte-identical to how it found it,
and only on a non-empty journal, so it cannot turn a first run into the
non-empty journal the salt-loss rule refuses.

**A failed `masked` write** becomes `ProxyError::Audit` → **503**, by the same
reasoning as `SessionError::Saturated` (`proxy.rs:46`): it is this gateway's own
health rather than anything the caller got wrong, and the same request may
succeed a moment later. The client is told `audit unavailable`; the path and the
underlying error go to `tracing::error!` only. A filesystem path is of no use to
the client and of some use to an attacker.

**A failed outcome write** cannot refuse anything — the request already
happened. It emits `tracing::error!` and returns. The hole this leaves is
bounded by the previous rule: a full disk refuses the *next* request at
`masked()`, so the journal does not quietly shed records, it stops the gateway.

**The one record that cannot be guaranteed** is the outcome of a request that
was refused because the audit write failed: its `Drop` writes to the same broken
file. `audit_write_failed` stays in the vocabulary for the transient case, and
this boundary is stated here rather than discovered later. The operator learns
from `tracing` and from the gap.

**Ordering** is the property the whole slice exists for: the fsync of the
`masked` line completes before the upstream `send()` is awaited. It is tested
structurally, not by racing.

**A poisoned mutex** is recovered with `into_inner()` and a warning rather than
propagated. A line is written by a single `write_all` under the lock, so a panic
mid-record is not reachable in practice, and refusing to serve because another
thread panicked costs more than it protects.

**A body the `Json<Value>` extractor rejects** never reaches `handle` and is
therefore never journaled. Nothing left the perimeter, so there is no
evidentiary hole — but it is a boundary of the claim and belongs in writing.

## Testing

The tests that matter are invariants, not the presence of a line.

- A detector failure produces exactly **one** record, with `upstream: false`,
  and the wiremock upstream received nothing.
- A success produces two records sharing one `request`, and the `masked` line is
  on disk before the provider is called — asserted by having the mock respond
  only once the file already contains the line, rather than by timing.
- A write failure (a read-only directory) refuses with 503 and the provider is
  not called.
- `types` and `spans` describe the request, not the session: a second turn
  repeating the same name records `{"PERSON": 1}`, not two.
- The recorded `error` is the failure that actually occurred, not a generic one:
  a detector timeout records `detector_timeout` and a saturated store records
  `session_saturated`, each with the status its `IntoResponse` arm produces.
  This is the invariant a guard that inferred its outcome would violate
  silently, so it is exercised per variant rather than once.
- Each of the three stream exits records its own class, and they differ from
  each other.
- A client that disconnects mid-stream records `aborted` — not `completed`,
  which is what an unsignalled drop would otherwise be free to claim.
- A stream that ends by a broken connection still closes its record; the
  streamed path is covered separately from the buffered one.
- `tenant` and `session` are 32 hex characters, and a request with no session
  header still records a `tenant` with a null `session`.
- No line of the journal contains any substring of the submitted text —
  exercised over a corpus of values, including the case where a value is echoed
  back inside a provider error body.
- The salt file is created once and reused: two successive runs against the same
  path produce the same `tenant` for the same credential, and a truncated salt
  file refuses startup.
- A journal ending mid-record — the state a crash leaves — keeps its fragment
  and the record written after the restart parses on a line of its own. A
  journal already ending in a newline gains no blank line, an empty one is left
  empty and so is still a first run, and a startup that refuses on the salt
  rule leaves the journal byte-identical.

## Out of scope

Rotation and retention — the file is one path, rotated externally around a
restart. Signing or hash-chaining records against tampering. A DSAR export. A
management or query endpoint. Metrics, including how often the saturation
refusal fires, which the session-saturation design
(`2026-08-09-session-saturation-refusal-design.md:179`) deferred to "the audit
slice" — it is observability rather than evidence, and it belongs with the
tracing work, not here. Per-credential quotas, still the open item from PR #17.
