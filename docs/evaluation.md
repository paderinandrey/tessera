# Evaluation

The measured numbers are in the [README](../README.md#what-it-detects). This is what the gates check and what the remaining misses are.

**Every annotated entity with a word reaching the provider is named.** The most direct
statement of what this gateway is for, and until now nothing measured it: every other gate is
about a *type*, and a type-matched gate cannot see an entity found under another label, found
with the wrong bounds, or not found at all — three different rows in three different tables,
none of which says "these characters went out". `make evaluate` asks by position and by
content, and fails on anything not already written down.

That is true of **one** of the two shapes production sends — every row of this corpus is a
sentence, which is what a plain message is. The other shape is below, under
[a JSON document is not a sentence](#a-json-document-is-not-a-sentence), and it is not
published here.

Four are real. Two are threshold misses:

```
GENETIC  'test génétique'   its own label at 0.288, bar 0.30
ORG      'Tessier SA'       its own label at 0.697, bar 0.75
```

Near misses by 0.012 and 0.053.

The other two are [#97][i97], and the mechanism is not the one that issue was
filed with. It was filed as "the detector splits a surname at its apostrophe and
finds neither half", from reading this gate's own output: the list beside each
entry is *the words of the gold value no prediction covers*, and the apostrophe
separates words in that tokenizer — so a surname nothing covered at all printed
as two. There was never a split.

What was happening is [#46][i46] inside tier 2. `person`, `location` and
`organization` shared one inference call, GLiNER returns one label per span, and
a competitor took the argmax and then failed *its own* bar while `person` would
have cleared its: `D'Angelo` scored 0.888 as a person asked alone and went out in
full because `organization` won at 0.603 against a bar of 0.75. `person` now gets
a call of its own in addition to its tier's, and four of the six occurrences came
back — along with `Texier`, which this list used to carry for exactly the same
reason.

```
PERSON   'L’Hôpital'        0.019 asked alone, in fr-0017
PERSON   'dell’Orto'        0.268 asked alone, in de-0004
```

These two are what a pass split cannot reach: low on their own merits rather than
argued down by a competitor. Both are tracked so the gate measures "no *new*
leak", which is bookkeeping rather than acceptance.

The remaining five are an annotation convention. The gold span includes a leading article the
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
[i97]: https://github.com/paderinandrey/tessera/issues/97

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

## A JSON document is not a sentence

`mask_all` gives a `Slot::Text` its own `detect` call, so everything above is measured on
the shape it is sent in. A `Slot::Json` document is different: `Shape::of` concatenates its
leaves, one call reads them together, and `Joined::split` returns the spans afterwards. A
tool call's arguments are that shape.

**The corpus above cannot stand in for it, and the reason is granularity rather than
length.** Measured on `gateway/src/testdata/claude_code_tools.json`, the one real payload
this repository holds, and pinned in
`mapping::a_real_joined_call_is_many_short_leaves_rather_than_a_few_long_ones`:

| | leaves | joined characters |
|---|---|---|
| `WebFetch` schema | 2 | 71 |
| `Read` schema | 8 | 373 |
| `Agent` schema | 20 | 721 |
| `Artifact` schema | 29 | 1 587 |
| four grouped corpus sentences | 4 | 344–399 |

Across those ten schemas the leaves run 1 to 276 characters, median 37, and 42 of 79 are no
longer than 40. A grouped corpus document's *joined length* is an ordinary size; its four
leaves of ninety characters are not what production joins. [#102][i102] read this the other
way round by comparing against the gold values alone — 46–53 characters — which is the most
favourable payload rather than the representative one.

`evaluation/corpus/documents.jsonl` is the representative one: leaf counts taken from those
ten schemas, leaf lengths matched to their distribution, and the same seeded value
generators the sentence corpus uses. 40 documents, 316 leaves, 183 annotations.
`detector/tests/test_document_corpus.py` scores it and `pytest -m ner` gates it.

**Absolute inventories, which the gate next door is not.**
`test_joined_detection.LOST_TO_JOINING` holds entities joining loses *relative* to reading
the leaves apart, so an entity missed on both paths is absent from it by construction —
that is why the stronger claim at the top of this page could be published while this
remained unmeasured. The two inventories here ask the plain question instead: whose words
reach the provider, on each path, regardless of the other.

```
183 annotated
 33 reach the provider when the leaves are read together
 33 reach the provider when each leaf is read alone
  8 are in the first and not the second — and 8 the other way round
```

**The totals coincide and the sets do not.** A gate on the count would have reported that
the leaf shape costs nothing; it costs something different. Both inventories are therefore
listed by member and asserted exactly, for the reason given above — a bound lets a fixed
leak pay for a new one, and an upper bound additionally accommodates a predicate that stops
seeing things.

Two findings came out of the first run. `DE_STEUERNUMMER` publishes 1.000 recall above
because its `confidence` is below its own `threshold` and every Steuernummer in the sentence
corpus sits next to the word that boosts it over the bar; standing alone in a field it is
not detected at all, and the JSON *key* that would carry that word is never scanned
([#104][i104]). And the apostrophe surnames [#97][i97] left behind are here too, in a shape
where nothing surrounds them.

**What is not done.** These figures are gated but not published: `metrics.json` and the
README tables are tied to `public.jsonl` by digest, and giving this corpus the same standing
is a separate change. [#103][i103] stays open for it.

[i102]: https://github.com/paderinandrey/tessera/pull/102
[i103]: https://github.com/paderinandrey/tessera/issues/103
[i104]: https://github.com/paderinandrey/tessera/issues/104
