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
- **Separate entity types per category**, not one umbrella type. A DPIA needs to know
  which category was found, and per-category thresholds stay tunable.

**Revised during implementation: eleven types, not eight.** Article 9's text names pairs
where one word does not cover the other — religious *or philosophical* beliefs, sex life
*or* sexual orientation, and political *opinions* rather than party membership. Each
clause needs its own zero-shot label to have any chance, so the eight categories became
eleven detector types feeding the same legal group.
- The gate target is **≥ 0.95**, binding when the model is available. High enough to
  catch a real regression, honest about not being checksum arithmetic.

**Revised during implementation:** the gate measures **coverage of the group**, not
per-category recall. Measurement showed the model reads "maghrébine" as religion rather
than ethnicity and "IG Metall" as an organization unless the label is phrased as "trade
union". A span found under the wrong Article 9 label is still redacted, so per-category
recall would fail the build over a taxonomy disagreement with no operational
consequence. What REQ-3's "misses are not tolerable" is actually about is a
special-category mention reaching the model provider unredacted — that is what the gate
now measures. Per-category numbers stay in the report, where a DPO can see them.

## Runtime approach

The categories are added to `catalog/ner.yaml` and ride the existing single
inference pass. No second pass, no second model: the recognizer already carries per-type
thresholds, and `predict_entities` is called with the lowest threshold across all types
and then filters each result against its own type's threshold.

**Revised during implementation: one inference pass per tier, not one for everything.**
The single-pass design above loses Article 9 data, and the coverage gate caught it.
GLiNER gives a span exactly one label, so the tiers bid against each other: `ver.di`
is claimed by `organization` at 0.505, beating `trade union` at 0.445, and is then
discarded by ORG's own 0.75 threshold. The Article 9 mention disappears because a
quasi-identifier won the argmax and then failed its own bar — and span resolution cannot
repair it, because only one span ever existed. Grouping the labels by tier and running
one `predict_entities` call per group costs one inference per tier and keeps the
categories from competing. A separate *model* per tier remains rejected: that would add
a second download and cache for no evidence of better quality.

## Types

| Entity type | Zero-shot label | Threshold | Tier | Specificity |
|---|---|---|---|---|
| HEALTH | medical condition | 0.30 | 3 | 35 |
| BIOMETRIC | biometric data | 0.30 | 3 | 35 |
| GENETIC | genetic data | 0.30 | 3 | 35 |
| ETHNICITY | ethnic origin | 0.30 | 3 | 35 |
| POLITICAL_AFFILIATION | political party | 0.30 | 3 | 35 |
| POLITICAL_OPINION | political opinion | 0.30 | 3 | 35 |
| RELIGION | religion | 0.30 | 3 | 35 |
| TRADE_UNION | trade union | 0.30 | 3 | 35 |
| SEXUAL_ORIENTATION | sexual orientation | 0.30 | 3 | 35 |
| PHILOSOPHICAL_BELIEF | philosophical belief | 0.30 | 3 | 35 |
| SEX_LIFE | sex life | 0.30 | 3 | 35 |

**Tier 3** is the "separate tier" the requirement asks for — the free slot in the span
schema, and a meaningful grouping: tier 1 is checksum-backed identifiers, tier 2 is
quasi-identifiers, tier 3 is meaning without form.

**Specificity 35** sits above the quasi-identifiers (10–30) and below every catalog
identifier (40–90). That ordering decides the overlaps that actually happen: in "Die
Gewerkschaft ver.di" a trade-union mention outranks a plain ORG, while a
checksum-validated identifier still outranks everything.

The labels are the model's interface and their phrasing decides whether it fires at all:
measured against the real weights, "trade union" scores 0.95 on "IG Metall" where "trade
union membership" scores 0.70, and "political affiliation" mislabels a union as a party
outright. The two labels were chosen from that measurement, not from the wording of the
regulation.

No new loader code. The configuration already validates thresholds, tiers, specificity,
and the uniqueness of both entity types and labels.

## Metrics

`make evaluate` gains a third gate beside Tier 1 recall and LOCATION precision:

- **Article 9 coverage ≥ 0.95** — the share of gold Article 9 entities matched (IoU ≥ 0.5)
  by a prediction carrying *any* Article 9 type. Binding when the model is available,
  skipped with an explicit note when it is not, exactly like the existing NER gates.
- **No blank language/category bucket** — every (language, category) pair with gold in the
  corpus must have at least one covered span. The pooled ratio cannot see a category going
  dark in one language; a per-bucket ratio cannot be met at these bucket sizes, where a
  single miss out of two reads as 0.5. The two together say what one cannot.
- **Article 9 precision is reported, never gated.** The requirement is explicit that
  false positives are tolerable and misses are not; gating precision would push the
  threshold back up and defeat the point.

The report groups the Article 9 types under their own heading rather than interleaving
them with identifiers, so the two kinds of number are not read as one.

## Corpus

New FR/DE templates carry explicit mentions of each category — a diagnosis, a
confession, a union membership, a party affiliation — with the mention annotated as gold.
Every name and circumstance is synthetic, as everywhere in the public corpus.

Two corpus rules emerged from measuring rather than from theory. **Slot values are keyed
by language**: a French HR note reading "est ver.di-Mitglied" is not code-switching, it is
nonsense, and a model judged on nonsense tells you nothing. **The gold annotates the datum,
not the sentence around it**: the qualifier ("Mitglied der", "de confession", "adhérent de
la") lives in the template text while the annotated span is the union, party, faith or
condition itself — which is both what the model spans and what carries the sensitive
content.

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

- Configuration: every type loads with the expected label, thresholds, tier and
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
