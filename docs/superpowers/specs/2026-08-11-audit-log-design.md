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

5. **The per-request record is an RAII guard.** Its `Drop` writes the `outcome`
   line, so every early return and every way a stream can end is covered without
   a single call site.

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
{"ts":"2026-08-11T09:14:37.902Z","event":"outcome","request":"7f3a9c1e04b25d68","upstream":true,"status":200,"result":"completed","error":null,"ms":15484}
```

A request refused before the upstream call is one self-contained line:

```json
{"ts":"2026-08-11T09:14:22.418Z","event":"outcome","request":"91c4a70b6de83f12","upstream":false,"status":502,"result":"refused","error":"detector_timeout","ms":30012}
```

**`types` counts distinct values, `spans` counts occurrences.** The difference
between them is the coreference the product sells: nine mentions, four people.

Both are computed from the spans the detector returned for *this request's*
texts, not from the session's table. A session-backed mapping is seeded from
earlier turns (`proxy.rs:153` clones the guard), so counting the table would
report the conversation's accumulated total on every turn and the record would
stop describing the request. `mask_all` (`proxy.rs:116`) currently drops the
spans it receives; it will return them.

**`result` has three values.** `completed` — the client received the whole
response. `refused` — the request ended with an error, which may be before the
upstream call (`upstream: false`) or after it, since a response carrying a
placeholder no mapping knows is refused too. `stream_failed` — bytes had already
been sent, so the stream ended mid-flight instead; `status` is then the 200 the
client already received, and `error` names why it stopped.

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

**`tenant` and `session` are digests.** `SessionKey::digest()` (`session.rs:95`)
already exists and already exists for this reason — the raw session id is
client-chosen and `patient-Weber-2026` is a plausible one. Two changes are
needed:

- the credential fingerprint is currently computed only inside
  `SessionKey::new` (`session.rs:82`), so a request with no session header has
  none. `tenant` needs a fingerprint of the credential independent of whether a
  session was asked for.
- `salt()` (`session.rs:53`) is random per process. That is right for sessions,
  whose data dies with the process, and wrong for a journal read months later:
  the same tenant would appear as a different one after every restart. The
  audit writer takes its own salt from `<audit_path>.salt` — 32 bytes, mode
  0600, created on first run and reused afterwards. Sessions keep their
  ephemeral salt; the two want opposite things and should not share.

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

`Record` is the per-request guard. It is constructed as the first statement of
`handle` (`proxy.rs:131`), before `provider.request_pointers`, so that a body
whose shape the gateway refuses is still journaled. It accumulates fields as
they become known: provider immediately, `tenant` and `session` after `key_from`
(`proxy.rs:142`), `types`/`spans`/`texts` after `mask_all`. It exposes exactly
two things:

- `masked(&mut self) -> Result<(), AuditError>`, called once, immediately before
  `state.upstream.post(...)` (`proxy.rs:172`). It serializes, writes and fsyncs
  inside `spawn_blocking`, so a slow disk does not occupy a runtime worker.
- `Drop`, which writes the `outcome` line: one `write_all` under the same mutex,
  no fsync.

The buffered path drops the `Record` when `handle` returns. The streamed path
moves it into `restore_stream` (`proxy.rs:215`) alongside `mapping`, and its
`Drop` fires when the stream ends — normally, by a broken connection, or by an
unrestorable token. No new code in `stream.rs` beyond taking ownership: the
three ways a stream can end are already implemented there, and none of them need
to know an audit log exists.

`Drop` being neither async nor fallible is not a limitation here. It matches a
decision already made: the outcome line does not fsync and may not refuse.

## Failure

**Startup.** `audit_path` has no `serde` default, so its absence is a config
error from the existing `deny_unknown_fields` parser (`config.rs:16`) rather
than a silent empty string. `main.rs` opens the file with `O_APPEND | O_CREAT`
before `bind`, and fsyncs the containing directory once — on a first run the
directory entry itself is what a machine failure would lose. A file that cannot
be opened stops the process. A `.salt` file that exists but is shorter than 32
bytes also stops it: regenerating a salt silently would split the journal in the
middle, and a journal that quietly renumbers its tenants is worse than one that
refuses to start.

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
- A stream that ends by a broken connection still closes its record; the
  streamed `Drop` is covered separately from the buffered one.
- No line of the journal contains any substring of the submitted text —
  exercised over a corpus of values, including the case where a value is echoed
  back inside a provider error body.
- The salt file is created once and reused: two successive runs against the same
  path produce the same `tenant` for the same credential, and a truncated salt
  file refuses startup.

## Out of scope

Rotation and retention — the file is one path, rotated externally around a
restart. Signing or hash-chaining records against tampering. A DSAR export. A
management or query endpoint. Metrics, including how often the saturation
refusal fires, which the session-saturation design
(`2026-08-09-session-saturation-refusal-design.md:179`) deferred to "the audit
slice" — it is observability rather than evidence, and it belongs with the
tracing work, not here. Per-credential quotas, still the open item from PR #17.
