# Evaluation

The measured numbers are in the [README](../README.md#what-it-detects). This is what the gates check and what the remaining misses are.

**Every annotated entity with a word reaching the provider is named.** The most direct
statement of what this gateway is for, and until now nothing measured it: every other gate is
about a *type*, and a type-matched gate cannot see an entity found under another label, found
with the wrong bounds, or not found at all — three different rows in three different tables,
none of which says "these characters went out". `make evaluate` asks by position and by
content, and fails on anything not already written down.

Three are real, and all three are threshold misses:

```
GENETIC  'test génétique'   its own label at 0.288, bar 0.30
ORG      'Tessier SA'       its own label at 0.697, bar 0.75
PERSON   'Texier'           claimed by `location` at 0.585, whose bar is 0.7
```

Two are near misses by 0.012 and 0.053. The third is [#46][i46]: a quasi-identifier wins the
argmax and then fails a bar the loser would have cleared — asked alone, `person` scores
`Texier` at 0.704.

The other five are an annotation convention. The gold span includes a leading article the
detector does not predict — `un diabète de type 2` masked as `diabète de type 2` — and `un`
is not personal data, the same argument the `PERSON` trimming rule makes for `Der Kunde`.

**Named individually rather than counted, and that is the gate.** A bound of "no more than
three" lets a fixed leak pay for a new one: one entity starts being detected, another stops,
the total holds and CI stays green. Each entry records the entity *and the words it leaves*,
so a shortfall that grows stops matching and fails. An entry that disappears is an
improvement and passes quietly.

It also replaced the first design, which forgave any word spelled like an article, in any
position, under any type — so a `PERSON` annotated `Le Thi Mai` with only `Thi Mai` predicted
would have read as fully masked, and `Le` is a Vietnamese family name this repository already
protects by name in the trimming rule. Forgiving a convention is a decision somebody writes
down about a specific entity, not a spelling rule.

[i46]: https://github.com/paderinandrey/tessera/issues/46

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
> enforced.
>
> **PERSON's weak spot is recall, not precision.** The table above read 0.785 /
> 0.671 until the span-trimming rule landed (#20): the model returns
> `Der Kunde Karz` where the gold annotates `Karz`, and every such span counted
> as a false positive while masking the name correctly. Trimming the role noun
> and the honorific off the front moved precision to 1.000 — 65 predictions, 0
> that land outside a gold span — and left eleven names the model does not find
> at all. That is the number to push on, and it is not gated either: a recall
> gate on a synthetic corpus in fixed template slots would measure the
> generator.
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
