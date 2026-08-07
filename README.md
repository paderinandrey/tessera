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
               streamed. Sessions and audit are the next slices.
  evaluation/  Public synthetic corpus and metrics harness (planned). The manually
               annotated corpus stays private and never enters this repository.
```

## CLI

Point the detector at a folder (or file) of texts to see what would be redacted:

```
uv run --project detector tessera scan path/to/texts            # human-readable, values masked
uv run --project detector tessera scan path/to/texts --json     # machine-readable
```

Found values are masked by default (`FR76…89`) so a saved report is not itself
a PII leak; `--show-values` prints them verbatim.

The NER layer (PERSON, LOCATION, ORG) runs automatically when its weights are present
(`make model`, several hundred megabytes, cached under `~/.cache/tessera/models`).
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
shape the gateway has no rule for, including tool definitions, tool traffic and Anthropic's
extended thinking, whose masking is a later slice; an identifier field present in a form that cannot be masked; a
span the detector reports that cannot be applied; and a placeholder in the response that no
mapping knows — each of these ends the request. Once a stream has begun there is nothing
left to refuse, so it ends mid-flight instead; the rule it protects is the same. Nothing
unmasked is forwarded, and no placeholder is ever handed to the client in place of a value.
No error body or log line carries the submitted text.

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

If the connection breaks mid-stream, whatever was already restored is served before the
error event. It was safe to send a moment earlier, and the break does not change that.

The gateway asks the detector for every layer it has, so a request costs what the
[latency](#latency) section reports; `detector_timeout_secs` defaults to 30 seconds
because a tight timeout would turn protection into a denial of service. Configuration is
TOML and rejects unknown keys — a typo in a security control should fail loudly rather
than leave a default in place.

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

Early development — Wave 0 (detector core). Not ready for production use.

## License

[Apache-2.0](LICENSE). Contributions are accepted under the
[Developer Certificate of Origin](CONTRIBUTING.md).
