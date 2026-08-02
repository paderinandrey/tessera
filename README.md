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
  gateway/     Rust reverse proxy (planned): HTTP/SSE, placeholder substitution and
               restoration, session mapping, audit. Wave 1.
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
