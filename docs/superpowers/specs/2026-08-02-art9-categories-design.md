# GDPR Article 9 Special Categories — Design

**Goal:** detect mentions of the special categories GDPR Article 9 protects — health,
biometrics, genetics, racial or ethnic origin, political opinions, religion, trade union
membership, sexual orientation — through the zero-shot NER layer, with the aggressive
threshold the requirement asks for and a recall gate of its own.

**Traceability:** REQ-3 (Article 9 detection, Must), whose acceptance criterion is "a
separate tier with the most aggressive threshold (0.30): false positives are tolerable
here, misses are not"; REQ-1 (multi-layer detection); REQ-38 (target metrics).

## Decisions made during brainstorming

- Article 9 gets **its own recall gate**, reported separately, rather than joining the
  Tier 1 recall gate. The MVP roadmap's Definition of Done groups "checksum identifiers,
  Article 9" under one ≥ 0.99 number, but the two are different kinds of signal: one is
  arithmetic, the other is a probabilistic reading of meaning. Blending them would let a
  model's good day flatter the checksum layer, and a model's bad day break a gate that
  today rests on arithmetic.
- **Eight separate entity types**, not one umbrella type. A DPIA needs to know which
  category was found, and per-category thresholds stay tunable.
- The gate target is **recall ≥ 0.95**, binding when the model is available. High enough
  to catch a real regression, honest about not being checksum arithmetic.

## Runtime approach

The eight categories are added to `catalog/ner.yaml` and ride the existing single
inference pass. No second pass, no second model: the recognizer already carries per-type
thresholds, and `predict_entities` is called with the lowest threshold across all types
and then filters each result against its own type's threshold.

One consequence to state plainly: a 0.30 threshold lowers that floor for every type, so
the model returns more candidates per chunk and more of them are discarded by the
per-type filter. The results are unchanged — PERSON still needs 0.70 — but each document
costs somewhat more to process. A separate pass for Article 9 would isolate the floor at
the price of doubling inference, which is the expensive part; a separate model would add
a second download and cache for no evidence of better quality.

## Types

| Entity type | Zero-shot label | Threshold | Tier | Specificity |
|---|---|---|---|---|
| HEALTH | medical condition | 0.30 | 3 | 35 |
| BIOMETRIC | biometric data | 0.30 | 3 | 35 |
| GENETIC | genetic data | 0.30 | 3 | 35 |
| ETHNICITY | ethnic origin | 0.30 | 3 | 35 |
| POLITICAL_OPINION | political affiliation | 0.30 | 3 | 35 |
| RELIGION | religion | 0.30 | 3 | 35 |
| TRADE_UNION | trade union membership | 0.30 | 3 | 35 |
| SEXUAL_ORIENTATION | sexual orientation | 0.30 | 3 | 35 |

**Tier 3** is the "separate tier" the requirement asks for — the free slot in the span
schema, and a meaningful grouping: tier 1 is checksum-backed identifiers, tier 2 is
quasi-identifiers, tier 3 is meaning without form.

**Specificity 35** sits above the quasi-identifiers (10–30) and below every catalog
identifier (40–90). That ordering decides the overlaps that actually happen: in "Die
Gewerkschaft ver.di" a trade-union mention outranks a plain ORG, while a
checksum-validated identifier still outranks everything.

No new loader code. The configuration already validates thresholds, tiers, specificity,
and the uniqueness of both entity types and labels.

## Metrics

`make evaluate` gains a third gate beside Tier 1 recall and LOCATION precision:

- **Article 9 recall ≥ 0.95** — binding when the model is available, skipped with an
  explicit note when it is not, exactly like the existing NER gates.
- **Article 9 precision is reported, never gated.** The requirement is explicit that
  false positives are tolerable and misses are not; gating precision would push the
  threshold back up and defeat the point.

The report groups the Article 9 types under their own heading rather than interleaving
them with identifiers, so the two kinds of number are not read as one.

## Corpus

New FR/DE templates carry explicit mentions of each category — a diagnosis, a
confession, a union membership, a party affiliation — with the mention annotated as gold.
Every name and circumstance is synthetic, as everywhere in the public corpus.

The corpus also needs more entity-free templates in the same register: business prose
with clinical or institutional vocabulary that is *not* about a person ("the delivery is
delayed", "the committee approved the budget"). At a 0.30 threshold a corpus made only of
positive examples would hide the noise the threshold buys, and the reported precision
would be meaningless.

## CLI

Nothing changes in the CLI: types come from configuration and the report prints them
like any other. The README gains a note that Article 9 detection deliberately runs at a
low threshold, so scans will show false positives — over-redaction being the safe failure
for this category.

## Testing

- Configuration: the eight types load with the expected labels, thresholds, tier and
  specificity; every one is below the catalog's specificity floor of 40.
- Resolution: an Article 9 span beats an overlapping ORG span, and loses to an
  overlapping checksum identifier. Tested with a fake recognizer, no weights needed.
- Metrics: the recall gate fails when a category falls below target and passes when it
  does not; it is skipped without the model.
- Model-backed (marked `ner`, skipped without weights): a German health mention and a
  French union mention are detected, with offsets inside the expected text.
- Corpus: every Article 9 gold annotation slices to non-empty text and does not overlap
  another annotation.

## Out of scope

The optional LLM pass (REQ-1's third layer), per-tenant thresholds (REQ-9), the
near-miss review band (REQ-10), and the p95 latency measurement (REQ-38).
