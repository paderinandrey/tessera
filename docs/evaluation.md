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
[a JSON document is not a sentence](#a-json-document-is-not-a-sentence); its figures are
published too, as advisory.

Two are real, and both are [#97][i97]. Two near misses used to sit beside them —
`test génétique` at 0.288 against Article 9's bar of 0.30, and `Tessier SA` at 0.697
against ORG's 0.75 — and lowering the Article 9 bars to 0.20 for [#109][i109] took both off
the list: the first is masked as `GENETIC`, the second as `TRADE_UNION`, a label that is
wrong and a span that is not. The same change first masked the surnames `Mercier` as
`ETHNICITY` and `Hermighausen` as `TRADE_UNION` ([#112][i112]). They read `PERSON` again
because a special-category reading that overlaps a `PERSON` reading must still clear 0.30
(`outranks_tier_3_below` in `ner.yaml`). Across both corpora the special-category readings
of surnames that overlap a `PERSON` reading score 0.225–0.299, except `L’Hôpital` in fr-0014
at 0.380, which stays `TRADE_UNION`; the real special categories score 0.354 or more. The
`PERSON` score does not separate the two groups — 0.55–0.97 against the surnames, 0.52–0.94
against the special categories — so letting the higher score win instead would label `homosexuell` and six JSON
values `PERSON`, moving Article 9 coverage to 45/46 on sentences and from 63/84 to 57/84 on documents
(measured before member names were shown, [#109][i109]).
`Tessier SA` stays `TRADE_UNION`: no `ORG` reading clears its bar, so there is nothing for
a rule to prefer.

For the two [#97][i97] entries, the mechanism is not the one that issue was
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

**Article 9 coverage: 1.0000 (46/46)** — every special-category mention in the
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
> precision is left ungated on purpose: at threshold 0.20 the layer over-reports by
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
tool definition and a tool call's arguments are both that shape. A value of at most three
words that sits in an instance object is shown after its member name — `religion: athée` —
and a span found in the name is dropped ([#109][i109]).

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

**Two populations: one measured, one approximated.** The figures above are tool
*definitions* — `Shape::Schema` documents the gateway joins and detects on every request
carrying tools, so a production shape rather than a proxy for one. A tool *argument* is a
`Shape::Instance` document and no captured one exists here. What stands in for it is the
number of top-level properties each schema declares, 2 to 15, 47 in all — **a proxy, not a
bound in either direction.** `Shape::Instance` recurses, so a property holding an array or
an object yields as many leaves as it holds, and `Artifact.capabilities` is an open object:
nothing caps an argument. A boolean yields no leaf at all, and a property declaring no
`type` — `Artifact.contract`, `SendMessage.message` — can do either. For this payload three
properties of 47 can yield any number of leaves and six can yield none, which makes the
proxy reasonable here and nothing more; the
gateway test names both sets so the error cannot grow unnoticed. An argument's real leaf
count and every argument leaf's *length* stay unmeasured — a value is the caller's data and
nothing here samples one — so the lengths below come from the definition population and are
an assumption.

**Text leaves only.** Every leaf in this corpus is a string. A JSON number is a
`Leaf::Number`, which is never masked: production refuses the whole request when a
deterministic span lands on one and forwards the digits otherwise, and both branches are
gated in the gateway (`proxy::tests::a_number_carrying_personal_data_refuses_the_request`,
`a_number_the_exemption_forwards_is_journalled_as_forwarded`). The measured payload's twelve
numeric leaves are schema bounds and count toward the distributions above as text. No
annotated value in the corpus has the shape of a JSON number — card numbers carry spaces, a
Steuernummer slashes — so none could have been sent as one, and the corpus test fails if one
is added. The check is the JSON number grammar itself, signs, fractions and exponents
included, so `-42` and `1e10` are caught as well as `42`.

`evaluation/corpus/documents.jsonl` draws its leaf counts from both lists and its lengths
from that distribution, with the same seeded value generators the sentence corpus uses. 60
documents, 378 leaves, 222 annotations. `detector/tests/test_document_corpus.py` scores it
and `pytest -m ner` gates it.

**Absolute inventories, which the gate next door is not.**
`test_joined_detection.LOST_TO_JOINING` holds entities joining loses *relative* to reading
the leaves apart, so an entity missed on both paths is absent from it by construction —
that is why the stronger claim at the top of this page could be published while this
remained unmeasured. The two inventories here ask the plain question instead: whose words
reach the provider, on each path, regardless of the other.

```
222 annotated
 26 reach the provider when the leaves are read together
 19 reach the provider when each leaf is read alone
 11 leak only when joined — and 4 only when apart
```

**The net is 7 and the loss is 11.** Four entities leak only when the leaves are read apart,
and each pays for a different entity that leaks only when they are joined, so a reader of
the two totals prices joining at 7 names when it costs 11 and buys back 4 — and they are
not the same names. Both paths show a short value after its member name, as production
does. Both inventories are therefore listed by member and asserted exactly,
for the reason given above: a bound lets a fixed leak pay for a new one, and an upper bound
additionally accommodates a predicate that stops seeing things.

> An earlier version of this corpus made the point by accident. Built from the definition
> leaf counts alone, it put both totals at 33 with a net of zero — which read as a stronger
> finding and was a property of that corpus. Mixing in the argument counts moved both
> numbers. The relation above is asserted on its own for that reason, separately from the
> two sets.

Two findings came out of the first run. `DE_STEUERNUMMER` published 1.000 recall above
because its `confidence` was below its own `threshold` and every Steuernummer in the sentence
corpus sits next to the word that boosts it over the bar; standing alone in a field it was
not detected at all, and the JSON *key* that would carry that word was not then shown to
the detector ([#104][i104]). The slash form now masks unaided, which took six entities out of both
inventories — 50 and 36 before, 44 and 30 after, nothing else moving. The 13-digit form
still needs its trigger: 1.5% of millisecond timestamps pass its format check, and a JSON
number with a deterministic type refuses the whole request. And the apostrophe-bearing
surnames [#97][i97] left behind are here too, in a shape where nothing surrounds them.

**Published, as advisory.** `make evaluate` reads every document the way the gateway reads a
`Slot::Json` and records two figures in `metrics.json` beside the sentence corpus's: the
joined-path leaks, counted by occurrence and by distinct entity, and Article 9 coverage
through the same `group_coverage` the sentence gate uses. The README publishes both and
`check-published-metrics` holds them to the measurement, tied to `documents.jsonl` by its own
digest. They are not held to the 0.95 Article 9 target, which they do not meet: 0.8095 with
member names shown, 0.7500 without them at the 0.20 bar, 0.6429 at 0.30. 12 of the 46
special-category values standing alone in a field still go out ([#109][i109]).

**Why a member name only before a short value.** Measured on this corpus with the leaves
read together, before the change: 35 leaks, and bare values with a descriptive name covered
18 of 29. A name before every leaf took leaks to 29, and the meaningless prefix `field: `
before every leaf to 31 — most of that gain was the `name: value` framing, not the name.
A name before values of three words or fewer took leaks to 26 and descriptive bare values to
23 of 29, while `field: ` before the same leaves stayed at 35, so that gain is the name's.
Prose carries its own context, and a name in front of it only dilutes the window. The cut
of three words is chosen on this corpus. Three values are lost that were found before —
`agnostisch` and `monarchistisch` under `input`, `agnostique` under `belief` — and each is
found when its leaf is read alone, with or without the name, so it is the joined text that
loses them rather than the name that misleads. The named inventory stays the gate, and a
test holds the evaluator's walk to it member for member, so the two cannot disagree.

[i102]: https://github.com/paderinandrey/tessera/pull/102
[i104]: https://github.com/paderinandrey/tessera/issues/104
[i109]: https://github.com/paderinandrey/tessera/issues/109
[i112]: https://github.com/paderinandrey/tessera/issues/112
