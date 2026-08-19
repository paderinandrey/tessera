# Tessera

**Tessera** is a self-hosted privacy gateway for LLM traffic. It sits as a transparent
reverse proxy between your application and an LLM provider (OpenAI- or
Anthropic-compatible APIs): personal data is replaced with placeholders on the way to
the model and restored in the response. Your application changes one thing — the base URL.

> In ancient Rome, a *tessera* was a token that stood in for an identity.
> Tessera replaces identities with controlled tokens — and puts them back.

## Why

Companies in regulated environments (banks, law firms, fiduciaries, insurers, medical
networks) want to use LLMs but cannot send client data to a third party. Tessera is a
**technical measure of pseudonymization** (GDPR Art. 32(1)(a)): the mapping table never
leaves your perimeter, the provider only ever sees placeholders.

**What Tessera does not claim:** it does not take you out of GDPR scope, and it is not
anonymization. Pseudonymized data remains personal data for you as the controller.
Tessera reduces exposure and gives your DPO a measurable, evidence-backed argument —
nothing more, nothing less.

## Design principles

- **Detection quality first.** The moat is high-quality PII detection in **French and
  German** (with code-switching), Swiss and EU identifiers with checksum validation,
  GDPR Art. 9 special categories, and quasi-identifiers — measured on a reproducible
  benchmark.
- **Fail-closed by default.** An unparsed request body or a lost mapping aborts the
  request; it never silently forwards raw data.
- **Nothing leaves the perimeter.** Self-hosted, no telemetry, no license phone-home.
  Audit logs never contain original values.
- **Minimal infrastructure.** Two containers (Rust gateway + Python detector), config in
  TOML/YAML, in-memory session mapping, append-only JSONL audit. No database, no Redis.

## Architecture

```
tessera/
  detector/    Python detection service: deterministic recognizers with checksum
               validation, NER (GLiNER/ONNX), context boosting. Stable HTTP contract.
  gateway/     Rust reverse proxy: drop-in base URL for OpenAI- and Anthropic-shaped
               requests, placeholder substitution and restoration, buffered and
               streamed, with per-conversation sessions and an audit journal built in.
  evaluation/  Public synthetic corpus and metrics harness. The manually annotated
               corpus stays private and never enters this repository.
```

## Running it

Two containers, and nothing else to install:

```
docker compose run --rm weights           # once: 2 GB of NER weights
docker compose up -d --build              # gateway on 127.0.0.1:${TESSERA_PORT:-8080}
```

**Download the weights first.** The detector builds its pipeline once, at
startup, because loading the model takes seconds and paying that per request
would be absurd — so a detector that started without weights stays
deterministic-only until it is restarted, however much you download afterwards.
Adding them to a running stack therefore needs `docker compose restart detector`,
and skipping that restart is the one way to end up with a successful 2 GB
download, a gateway that looks installed, and names, places and health mentions
still reaching the provider unmasked.

The gateway does serve before the weights are there, deliberately. Without them
the detector runs its deterministic layer alone — checksum-validated
identifiers, the layer that scores 1.000 — and a partial install stays visible
rather than silent: the detector's own `GET /health` reports `ner: false` and
why, though it answers only on the compose network, never on the host. The
download is a separate command on purpose: a gateway that belongs inside your
perimeter should not reach the internet on its own, and a first start that
blocks for minutes is indistinguishable from one that has hung. It is the same
download `make model` does for a local, non-containerized detector; here it lands on a named volume,
so it is a one-time cost per host rather than a cost of every `up`.

Only the gateway is published, and on `${TESSERA_PORT:-8080}` rather than a
fixed `8080` — a variable, because a host that already has something bound to
8080 should not have to edit a file to try Tessera; set `TESSERA_PORT` before
`up` to use another port instead. The host address is a variable too, and
defaults to loopback: the gateway authenticates no caller, it forwards
whatever credential arrives, so reaching it from beyond this host is a
deliberate act — `TESSERA_BIND=0.0.0.0` — and never a side effect of the
default. It holds no key of its own, so what you publish is not your provider
credit but an unauthenticated relay out through your egress and into a journal
whose worth is that it records your traffic and not a stranger's: strangers can
fill the session table until legitimate callers are refused with a 503, and
anyone who already holds one of your callers' keys can guess a session id — they
are chosen by the client and need not be secret — and read that conversation's
real values back out of its table, which going to the provider directly would
never have given them. Put an authenticating proxy in front of it before you do.
The detector answers on the compose network and nowhere else: `POST /detect`
takes arbitrary text and authenticates nobody, so exposing it would be a way to
run text through the model outside the gateway, and therefore outside the audit
journal.

The journal and its salt share the `audit` volume and must stay together — a
journal with records whose salt has gone missing refuses to start rather than
silently renumbering every tenant beneath you. Back up that volume, not just
the file. `docker compose down` stops the stack and keeps the journal, its
salt and the downloaded weights; add `-v` only when you mean to discard them
— journal and salt together, since one without the other is what refuses to
start back up.

### Seeing it work without an API key

```
export TESSERA_PORT=${TESSERA_PORT:-8080}
docker compose -f docker-compose.yml -f deploy/docker-compose.demo.yml up -d --build
curl -X POST http://127.0.0.1:${TESSERA_PORT}/v1/chat/completions \
  -H 'content-type: application/json' -H 'authorization: Bearer sk-demo' \
  -d '{"messages":[{"role":"user","content":"Meine IBAN lautet CH9300762011623852957."}]}'
```

The overlay replaces the provider with a stand-in that records what reached it,
so one request shows both directions at once:

```
docker compose -f docker-compose.yml -f deploy/docker-compose.demo.yml \
  exec mock-provider cat /received/received.json
```

The answer you got back carries the real IBAN. What the provider received
carries a placeholder in its place, never the value itself. `make compose-smoke`
asserts exactly that, plus that the journal recorded the request without
quoting any of it.

## CLI

Point the detector at a folder (or file) of texts to see what would be redacted:

```
uv run --project detector tessera scan path/to/texts            # human-readable, values masked
uv run --project detector tessera scan path/to/texts --json     # machine-readable
```

Found values are masked by default (`FR76…89`) so a saved report is not itself
a PII leak; `--show-values` prints them verbatim.

The NER layer (PERSON, LOCATION, ORG) runs automatically when its weights are present
(`make model`, 2 GB, cached under `~/.cache/tessera/models`).
Without them the scan runs the deterministic layer alone and says so; `--ner` makes their
absence an error, `--no-ner` skips the layer even when they are installed.

Article 9 special categories (health, biometrics, genetics, ethnic origin, political
opinion, religion, trade union membership, sexual orientation) are detected by the same
layer at a deliberately low threshold, so expect visible false positives —
over-redaction is the safe failure for this category.

The same binary serves the detection HTTP contract the gateway will call:

```
uv run --project detector --group serve tessera serve      # 127.0.0.1:8000
```

`POST /detect` takes `{"text": "...", "layers": ["deterministic", "ner"]}` — `layers` is
optional and may only narrow what the server runs — and every response reports
`layers_run`, so a deterministic-only result is never mistakable for a full scan. Asking
for a layer the server cannot run is a 503 naming the reason rather than a quiet
downgrade. `GET /health` reports whether the NER layer is loaded and why it is not. The
committed OpenAPI document at `docs/api/openapi.json` is the schema both implementations
share (REQ-44); `make openapi` regenerates it and CI fails if it drifts.

## Gateway

Point a client's base URL at the gateway and personal data stops leaving the process:

```
cd gateway && cargo run -- tessera.example.toml     # 127.0.0.1:8080
```

It accepts the OpenAI shape at `/v1/chat/completions` and the Anthropic shape at
`/v1/messages`, including Anthropic's separate `system` field and both providers'
content-part arrays. Detected spans become typed placeholders — `[PERSON_1]`, `[IBAN_2]` —
and an identical value always gets the same placeholder within a request, because two
placeholders for one person would tell the model there were two. Identifiers outside the
message content are masked too — OpenAI's `user` and per-message `name`, Anthropic's
`metadata.user_id`. The response is restored before it reaches the client, and an upstream
error keeps its own status and body so a rate limit still reads as a rate limit.

Provider credentials pass through on a per-provider allowlist: `Authorization` and OpenAI's
routing headers go to OpenAI, `x-api-key` and `anthropic-version` go to Anthropic, and
nothing else goes anywhere. A caller holding both sets of credentials does not have one
provider's key posted to the other, and a client's cookies are nobody's business but the
client's. Coming back, the provider's status and its rate-limit headers are preserved, so a
429 still reads as a 429 with its `Retry-After`.

**Every failure refuses the request**, and refuses it *before* the upstream call wherever
the problem is visible there. A detector that errors or exceeds its timeout; a body whose
shape the gateway has no rule for, including tool definitions, tool traffic, Anthropic's
extended thinking, OpenAI's `logprobs`, whose token strings are the masked output again, and
OpenAI's audio output, whose transcript no restoration can reconcile with the recording; an identifier field present in a form that cannot be masked; a
span the detector reports at a position that cannot be applied — inverted, past the end of the
text, or overlapping another; and a placeholder in the response that no
mapping knows — each of these ends the request. Once a stream has begun there is nothing
left to refuse, so it ends mid-flight instead; the rule it protects is the same. Nothing
unmasked is forwarded, and no placeholder is ever handed to the client in place of a value.
No error body or log line carries the submitted text.

Placeholders carry the type the detector reported, but only when it is one this gateway
declares — twenty-two of them, the catalog's eight deterministic identifiers plus the
fourteen the NER layer can label. A type outside that list is masked as `[REDACTED_1]`
instead. Syntax cannot tell a type name from a value shaped like one: a detector returning
`WEBER` as the type of a span covering `WEBER` would otherwise put that value in the token
the provider receives. The gateway keeps its own copy of the list rather than asking the
detector, since the detector's answer is what the check defends against, and CI fails if the
two drift apart.

### Sessions

Within a request an identical value always gets the same placeholder. Across the turns of
a conversation it does too, if the client sends `X-Tessera-Session: <id>` — otherwise each
request gets its own table, which is the behaviour without the header.

The id does not select a session on its own. A session table is a restoration oracle: put
`[PERSON_1]` in a prompt, get it echoed by the model, and the gateway would restore it to
a real name on the way back. So the store keys on a salted fingerprint of the caller's own
credential as well as the id, and a guessed id lands in an empty namespace. The boundary
is the credential, not the id: callers who share one API key share one namespace, and
within it any id is reachable by anyone holding that key. The raw id
never reaches a log either — a client may well name its session after the person in it.

The table holds real values in memory between requests. It is the only place in the
gateway where that is true — the detection cache below retains data across requests too,
but never a value, only a span's type and offsets — so the session table is bounded
three ways: `session_idle_secs`, `max_sessions` and `max_session_values`. Reaching
`max_session_values` or the idle TTL costs coreference,
never protection. The client holds restored text and sends the history again, so a
session that was evicted is rebuilt from scratch by the next request — `[PERSON_3]`
becomes `[PERSON_1]` and nothing else changes. Past `max_session_values` a value is
still masked and still restored; it is simply not remembered.

Reaching `max_sessions` is the one bound that can cost a request rather than a
coreference. A session table is only ever reclaimed from a conversation that has
no request holding it; when every table in a full store is in flight, a request
asking for a *new* session is refused with a 503 rather than served by evicting a
live one. Evicting one would leave that conversation with two unsynchronized
tables, and two concurrent requests can then give one placeholder to two
different values — which is a wrong name in a response, not a lost coreference.
A request for a session the store already holds is never refused.

Values are never evicted from within a live session: one that came back from the model
would end a request with nothing to restore to.

Detection itself is cached, separately from the session table above and whether or not
a session is attached: a text is scanned once per detector version and credential, and
every repeat after that — the whole history a client resends on each turn — is served
from memory instead of calling the detector, which is what turns a conversation's cost
from growing with the square of its length back to growing with it linearly. What is
remembered is a span's type and two offsets, never the text or the value, keyed on
digests of the detector's version, a salted fingerprint of the credential, and the text
itself — so a hit never reveals, even through timing, that one tenant sent what another
tenant sent before it. The session stabilises a placeholder that detection produced,
cached or not; it is never asked to find personal data on its own.

The cache is bounded on two dimensions, the same relationship `max_sessions` and
`max_session_values` have to each other: `detection_cache_entries` (default 10 000)
bounds how many texts it remembers, and `max_spans_per_entry` (default 250) bounds how
many spans one remembered text's detection may carry — without the second, a single
span-dense text could outweigh thousands of ordinary ones. 250 is sized against measured
density rather than assumed: real text runs roughly 1.0 to 2.5 spans per 1 000
characters, so the default covers prose to about 100 KB, logs to about 188 KB and source
to 250 KB — every realistic single tool result. A detection over the cap is masked,
restored and returned exactly like any other; it is simply not stored, so an oversized
result never becomes a refusal, only a permanent miss. At the shipped defaults the worst
case is about 118 MB. Unlike the session table, the cache has no idle TTL — an entry
outlives its conversation and stays reachable for as long as the process runs, until the
detector's
version changes or the cache fills and something else is used more recently. And unlike
the session table, losing an entry costs time, not protection: a full cache evicts
rather than refusing, and a poisoned lock degrades to calling the detector rather than
failing the request. Set `detection_cache_entries = 0` to disable the cache entirely —
the gateway then calls the detector for every text, with no cache in the loop at all.

A request refused before the upstream call leaves its session exactly as it was. Asking for a
session the gateway cannot honour — a malformed id, no credential to namespace it, or
`session_idle_secs = 0` — is refused before the detector runs rather than served without
the coreference it asked for.

### Streaming

`stream: true` is served for both providers. A placeholder does not respect event
boundaries — `[PERSON_1]` arrives as `[PER` in one event and `SON_1]` in the next, over HTTP
chunks that break anywhere, including the middle of a UTF-8 character — so the gateway holds
back the text from the last unclosed `[` and emits it once the token is whole or has grown
too long to be one. Everything before that point flows on immediately. Matching is exact:
tolerating altered spacing, casing or markdown around a token would also be a way to put a
real name where the model wrote something else.

Restored text is not the length of the masked text, so a delta is rewritten whole, and
text-bearing events are emitted one behind — the event that carries no text ends the run and
releases what is held into the event waiting behind it. The terminal events, the quota
headers and fields like `id:` reach the client as the provider sent them.

If a token turns out to have no mapping, bytes have already gone out and the request cannot
be refused. The stream ends instead, with an `error` event naming the failure — the client
gets a truncated answer, never a placeholder in place of a name. Streamed tool calls
(`tool_calls`, `input_json_delta`) end the stream for the same reason they are refused on
the buffered path: their arguments are not masked yet. Extended thinking is refused before
the upstream call rather than at its first streamed block, so the refusal costs no tokens.

Whatever was already restored is served before the error event, whether the stream ends
because the connection broke or because a token could not be restored. It was safe to send a
moment earlier, and the failure does not change that; what stays behind is the hold-back
buffer, which may hold the token that failed.

On a cache miss the gateway asks the detector for every layer it has, so that request
costs what the [latency](#latency) section reports; a hit costs a lookup instead.
`detector_timeout_secs` defaults to 30 seconds because a tight timeout would turn
protection into a denial of service. Configuration is
TOML and rejects unknown keys — a typo in a security control should fail loudly rather
than leave a default in place.

### Audit

Every request appends to `audit_path`, one JSON object per line, and the gateway
does not start without it — a control that can be switched off by omitting a
line is worth nothing in a compliance report.

A request leaves two records. The first is written **and fsynced before the
provider is called**, so evidence that a request was pseudonymized cannot be
lost by the crash that follows it; if it cannot be written, the request is
refused with a 503 rather than served unrecorded. The second is written when the
request ends — including minutes later, when a stream does. The examples below
group the fields for reading; on disk each line serializes its keys in
alphabetical order, which nothing depends on.

```json
{"ts":"2026-08-11T09:14:22.418Z","event":"masked","request":"7f3a9c1e04b25d68","provider":"anthropic","route":"/v1/messages","tenant":"a41f9c02…","session":"3bd7e105…","stream":true,"texts":4,"spans":9,"types":{"PERSON":2,"IBAN":1,"HEALTH":1},"redacted":0}
{"ts":"2026-08-11T09:14:37.902Z","event":"outcome","request":"7f3a9c1e04b25d68","tenant":"a41f9c02…","session":"3bd7e105…","upstream":true,"status":200,"result":"completed","error":null,"ms":15484}
```

A request refused before the provider is called leaves one line, and it answers
on its own both questions that matter — whether bytes left, and whose request it
was:

```json
{"ts":"2026-08-11T09:14:22.418Z","event":"outcome","request":"91c4a70b6de83f12","tenant":"a41f9c02…","session":null,"upstream":false,"status":503,"result":"refused","error":"audit_write_failed","ms":12}
```

That is why `tenant` and `session` appear on the outcome line as well as the
masked one: a refusal has no masked line to join to, and the redundancy on the
two-line case costs a few bytes.

The record counts and never quotes. `types` counts distinct values per type and
`spans` counts occurrences; the gap between them is a value named more than once
within the same request, not anything a session did — detection runs over every
text on every request whether or not one is attached, so the coreference a
session buys across turns leaves no trace here. Neither the values, hashes of
them, their offsets nor the placeholder names are written. `error` is drawn from
a fixed vocabulary rather than formatted from a message, so no expression in the
writer could interpolate submitted text. A type name the detector reports that
is not a legible type — the mapping lets one through whenever the value was
already masked earlier in the same request or in an earlier turn — is counted
under `unvalidated` rather than written out, since the name itself came from
outside the perimeter. Seeing that key means the detector and the gateway
disagree about what a type is; the gateway also says so in its own log.
`redacted` counts the values the mapping masked as `[REDACTED_n]` because their
type was not one it declares — which `types` does not show, since it is built
from what the detector reported and not from what the placeholder ended up
carrying. A line naming `WEBER` and a `redacted` of 1 says the provider received
`[REDACTED_1]`, not `[WEBER_1]`.
`result` is one of `completed`,
`refused`, `stream_failed` or `aborted`; the last is what an unsignalled record
defaults to on drop — in practice, a client that disconnects while a stream is
still open, recorded as itself rather than as a success nobody observed.

`tenant` and `session` are salted digests, never a key and never the raw session
id — a client may well name its session after the person in it. The salt lives
in `<audit_path>.salt`, created on first run with owner-only permissions, so one
credential keeps one identity across restarts. It is evidence too: back it up
with the journal.

Losing it stops the gateway rather than renumbering the journal underneath you.
A salt that exists but is not exactly 32 bytes refuses to start, and so does a
salt that is *missing* beside a journal that already has lines — a partial
restore or a rebuilt container that dropped only the salt would otherwise look
like a first run and start writing a `tenant` that disagrees with every line
above it, with nothing marking the boundary. The error names both remedies:
restore the salt, or move the journal aside to begin a new one.

Only the first of a request's two records is fsynced, so a crash can leave the
journal ending in a half-written second one. The next start appends the newline
that record never got, says so in its log, and serves — no operator, no flag. The
interrupted line stays exactly as short as the crash left it, because a record
that was cut off is itself a fact about that run; what the newline buys is that
every record written after the restart is a whole line of its own, rather than
being glued onto the fragment and lost with it.

A disk that fills in the middle of a record does the same damage without a
restart to repair it, so the running gateway repairs it too: the request whose
record was cut short is refused, and the next record written — once there is room
again — starts on its own line, with a line in the log saying why.

Two limits stay honest. A salt *replaced* by a different valid 32-byte salt
cannot be detected by anything — it is indistinguishable from the real one. And
rotation still works: retention and rotation are the operator's, done externally
around a restart by moving the journal aside and keeping the salt, which starts
normally and carries the same digests across. The file itself is opened for
appending only.

## Evaluation

The public synthetic corpus (FR/DE plus code-switching, checksum-valid synthetic
identifiers, seeded generation) lives in `evaluation/` and is reproducible by anyone:

```
make corpus     # regenerates evaluation/corpus/public.jsonl byte-identically
make evaluate   # per-type precision/recall/F1 + the Tier 1 recall gate (>= 0.99)
```

Current results on the public corpus, with the NER weights installed:

| Type | Precision | Recall | F1 |
|---|---|---|---|
| CH_AVS | 1.000 | 1.000 | 1.000 |
| CREDIT_CARD | 1.000 | 1.000 | 1.000 |
| DE_STEUERNUMMER | 1.000 | 1.000 | 1.000 |
| DE_STEUER_ID | 1.000 | 1.000 | 1.000 |
| EMAIL | 1.000 | 1.000 | 1.000 |
| FR_NIF | 1.000 | 1.000 | 1.000 |
| FR_NIR | 1.000 | 1.000 | 1.000 |
| IBAN | 1.000 | 1.000 | 1.000 |
| LOCATION | 0.667 | 1.000 | 0.800 |
| ORG | 0.154 | 0.333 | 0.211 |
| PERSON | 0.785 | 0.671 | 0.723 |

Article 9 special categories, detected by the same layer at a lower threshold:

| Type | Precision | Recall | F1 |
|---|---|---|---|
| BIOMETRIC | 1.000 | 1.000 | 1.000 |
| ETHNICITY | 1.000 | 1.000 | 1.000 |
| GENETIC | 1.000 | 0.750 | 0.857 |
| HEALTH | 0.667 | 1.000 | 0.800 |
| PHILOSOPHICAL_BELIEF | 0.000 | 0.000 | 0.000 |
| POLITICAL_AFFILIATION | 0.800 | 1.000 | 0.889 |
| POLITICAL_OPINION | 0.000 | 0.000 | 0.000 |
| RELIGION | 0.500 | 1.000 | 0.667 |
| SEXUAL_ORIENTATION | 1.000 | 1.000 | 1.000 |
| SEX_LIFE | 0.000 | 0.000 | 0.000 |
| TRADE_UNION | 0.296 | 1.000 | 0.457 |

**Article 9 coverage: 0.9783 (45/46)** — nearly every special-category mention in the
corpus is caught by at least one Article 9 label, in both languages. Article 9 is split
across eleven detector types rather than eight: the regulation protects political opinions,
a stated view that names no party needs a label separate from `political party`, and the
regulation names philosophical beliefs beside religious ones and sex life beside sexual
orientation — each clause gets its own label.

> Perfect scores on the catalog types mean the deterministic layer covers its own
> catalog, nothing more: those corpus entries are checksum-valid identifiers with clean
> formatting. The numbers become meaningful as the corpus grows adversarial cases —
> noisy formatting, near-misses, uncovered types.
>
> Article 9 is gated on **coverage**, not on per-category recall, and the ETHNICITY row
> shows why: the model reads "maghrébine" as religion rather than ethnicity. That span is
> still redacted, which is what REQ-3's "misses are not tolerable" is actually about — a
> special-category mention reaching the model provider unredacted. Which of the eight
> labels wins is second-order, so the report shows it and the gate does not. Their
> precision is left ungated on purpose: at threshold 0.30 the layer over-reports by
> design, because over-redaction is the safe failure for this category.
>
> The NER rows are flattered in one direction and penalised in another. Names, cities
> and companies sit in fixed template slots rather than in the shapes real text
> produces, which makes them easier to find than they would be in a real document; at
> the same time a model that correctly spots an entity the synthetic gold does not
> enumerate is scored as wrong. So `make evaluate` enforces the Tier 1 recall gate
> (≥ 0.99), the Article 9 coverage gate (≥ 0.95) and the LOCATION over-masking gate
> (≥ 0.8), while the strict per-type precisions are reported and warned about rather than
> enforced. PERSON precision is the honest weak spot and is not gated.
>
> REQ-38's 0.8 precision target is an irritation metric — below it, clients say the service
> ruins their text — so the binding gate measures **over-masking**: predictions that land on
> no personal data at all. LOCATION scores 1.000 there (12/12 predictions cover a real gold
> span) while its strict per-type precision reads 0.667, and the gap is entirely French
> surnames that are also place names — Lenoir, Fontaine, Mercier — where the model marks the
> very span the gold calls PERSON. That span is redacted either way; only the placeholder's
> type differs. ORG stays advisory because its over-masking is real rather than a labelling
> disagreement: "Le laboratoire" and "service juridique" match no gold entity at all.
>
> ORG precision fell from 0.950 to 0.154 when this corpus gained entity-free
> business prose: the model finds organizations in "die Apotheke" and "convention
> collective" that the gold does not enumerate. That is the negative examples doing their
> job — the earlier number was measured on a corpus with almost nothing to get wrong.
> The privately annotated corpus on real texts is the measure that counts, and it is
> reported separately.

## Latency

```
make bench      # per-layer p95 across three document sizes
```

Measured on an Apple M3 Pro (11 cores, CPU only), with the pinned fp32 ONNX weights, over
text concatenated deterministically from the public corpus:

| Size | Deterministic | Chunk + tokenize | NER tier 2 | NER tier 3 | Total (median) | Total (p95) |
|---|---|---|---|---|---|---|
| sentence (80 chars) | 0.0 ms | 0.0 ms | 42 ms | 66 ms | 109 ms | 116 ms |
| paragraph (1 200 chars, one chunk) | 0.6 ms | 0.3 ms | 467 ms | 491 ms | 950 ms | 1 108 ms |
| document (6 000 chars, several chunks) | 3.3 ms | 1.9 ms | 2 556 ms | 3 180 ms | 5 524 ms | 6 086 ms |

Per-layer figures are medians and account for the total: preprocessing is shared across the
inference passes and timed once, so the parts sum to within a few percent of the whole. The
p95 column is the number REQ-38 asks about; on a developer machine under load it carries
contention as well as detector behaviour — the document row has measured between 4 854 ms
and 9 008 ms p95 across runs while its median moved under 10%. Treat the medians as the
stable signal and p95 as an upper bound until these run on dedicated hardware.

> REQ-38 targets p95 under 80 ms without the LLM layer, and the detector does not meet it
> with the NER layer enabled — not by a margin that tuning closes. The deterministic layer
> is effectively free at every size; the entire budget goes to the model, and it costs
> roughly a second per 1 200 characters on this CPU. Measuring one-sentence documents alone
> would have reported 116 ms and hidden that, which is why the harness uses a size ladder.
>
> The split says two useful things. Chunking and tokenization are free — 1.9 ms on a 6 000
> character document — so the cost is inference and nothing else. And three quasi-identifier
> labels cost about as much as eleven Article 9 labels: the price is paid per inference pass,
> not per label, so adding categories to an existing tier is nearly free while adding a tier
> is not.
>
> Every route to the target measured so far costs something. Collapsing the two inference
> passes into one comes in faster but loses the Article 9 spans the split exists to protect.
> Running the passes on two threads gains only 16%, because onnxruntime already saturates
> the cores and the passes compete rather than overlap. The int8 graph is the most promising
> — roughly half the latency — but it halves every confidence score too (`Diabetes`
> 0.948 → 0.476, `IG Metall` 0.98 → 0.54), preserving ranking while invalidating every
> threshold calibrated against fp32, so adopting it means recalibrating and re-measuring the
> quality gates.
>
> The number is published here rather than gated in CI: timings on shared runners are noise,
> and a target that is not met should be visible rather than quietly enforced somewhere it
> never runs. CI runs the harness only to prove it still works.

## Status

Early development. The detector, the gateway — sessions, streaming, the audit
journal — and the two-container stack above all work end to end. Not ready for
production use: the gateway authenticates no caller, and nothing here has been
run in anger.

## License

[Apache-2.0](LICENSE). Contributions are accepted under the
[Developer Certificate of Origin](CONTRIBUTING.md).
