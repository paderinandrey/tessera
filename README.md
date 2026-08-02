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
| LOCATION | 0.870 | 1.000 | 0.930 |
| ORG | 0.950 | 0.760 | 0.844 |
| PERSON | 0.644 | 0.543 | 0.589 |

> Perfect scores on the catalog types mean the deterministic layer covers its own
> catalog, nothing more: those corpus entries are checksum-valid identifiers with clean
> formatting. The numbers become meaningful as the corpus grows adversarial cases —
> noisy formatting, near-misses, uncovered types.
>
> The NER rows are flattered in one direction and penalised in another. Names, cities
> and companies sit in fixed template slots rather than in the shapes real text
> produces, which makes them easier to find than they would be in a real document; at
> the same time a model that correctly spots an entity the synthetic gold does not
> enumerate is scored as wrong. So `make evaluate` enforces the Tier 1 recall gate
> (≥ 0.99) and the LOCATION precision gate (≥ 0.8), while ORG precision is reported
> and warned about rather than enforced — a number that good on this corpus is not
> evidence about real prose. PERSON precision is the honest weak spot and is not yet
> gated. The privately annotated corpus on real texts is the measure that counts, and
> it is reported separately.

## Status

Early development — Wave 0 (detector core). Not ready for production use.

## License

[Apache-2.0](LICENSE). Contributions are accepted under the
[Developer Certificate of Origin](CONTRIBUTING.md).
