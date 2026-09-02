# A checksum span with higher specificity must not lose to one it contains — design

Closes #39.

> One branch of one function, so the design and its test plan are one document
> rather than two. Decisions were taken without the user in the room, on a
> standing instruction; the one worth their attention is **how far this claims
> to go**, in the last section.

## The defect

`resolve` folds over conflicting pairs: it finds the first overlapping pair by
index, replaces the two with one, re-sorts, and repeats. A fold is
order-dependent unless its operation is associative, and this one is not — so a
span that appears in no output can still decide which of two others survives.

Reproduced with three spans, no model and no text:

```python
cc  = Span("CREDIT_CARD", 45, 66, 1.0, "catalog:credit_card", tier=1)
nir = Span("FR_NIR",      45, 66, 1.0, "catalog:fr_nir",      tier=1)
org = Span("ORG",         41, 66, 0.9, "ner:gliner",          tier=2)

resolve([cc, nir])        # -> FR_NIR[45:66]      rule: specificity
resolve([cc, nir, org])   # -> CREDIT_CARD[41:66] rules: untouchable-inner-merge, nesting-outer-wins
```

Specificity is `FR_NIR` 80, `CREDIT_CARD` 40, `ORG` 10. The `ORG` span loses to
both and appears in neither output, yet it changes which of the other two wins.

**Why.** Sorted, `ORG` comes first, so the first conflicting pair is
`(ORG, CREDIT_CARD)`. Rule 3's `untouchable-inner-merge` fires — the inner
carries a checksum, the outer does not — and produces `CREDIT_CARD[41:66]`. That
synthesised span now **strictly contains** `FR_NIR[45:66]`, and in the
containment branch both are untouchable, so neither exception applies and
`nesting-outer-wins` returns the outer. **Specificity is never consulted again.**
Eighty loses to forty because forty was merged first.

## The hole, stated exactly

Rule 3 has two exceptions to *outer wins*, and both are guarded by
`not untouchable(outer)`:

- an untouchable inner inside a non-untouchable outer merges and keeps the
  sensitive type;
- a more specific inner inside a non-untouchable outer keeps its identity.

So when the outer is **itself** untouchable, the inner's specificity is never
asked. That is deliberate for the case the comment names — a checksum outer must
not be replaced by a less specific inner — but it says nothing about an inner
that is **more** specific and equally untouchable, which is this defect.

## The change

One branch, and the condition in it is a **comparison rather than a test**.

The first draft asked whether the inner's specificity was strictly greater. That
closes the reported case and leaves the same defect one level down: a catalog may
rate two checksum types equally, and rule 4 then settles them by sensitivity — so
side by side a tier-1 `IBAN` beats a tier-2 `FR_NIR`, while nested the strict `>`
was false and the outer won, whichever one a merge happened to widen. A bridge
span still flipped the answer.

So the branch defers to `_outranks`, which is the ordering rule 4 already applies
to an unnested pair: specificity, then confidence, then sensitivity. **Nesting
should not change the verdict**, and stating it that way is what makes the fix
about the principle rather than about the case.

Both spans must carry checksums. Rule 1 is untouched: a checksum span still never
loses to a non-checksum one, and the outer's extent still survives, so nothing
shrinks and nothing that was masked stops being masked.

Rule 1 is untouched: a checksum span still never loses to a non-checksum one,
and the outer's extent still survives, so nothing shrinks and nothing that was
masked stops being masked.

## What breaks, and for whom **[revised in review]**

The first draft said only that a type changes "toward the one the catalog rates
more specific". That was true of the first draft's condition and is too narrow
for what shipped. `resolve` is public, and three things about its output change.

**A nested checksum span can now be renamed by confidence or sensitivity, not
only by specificity.** `_outranks` is the full ordering, so where two checksum
types are rated equally a caller now gets the more sensitive — or, under a
custom `untouchable`, the more confident — reading's `entity_type`, `recognizer`
and `tier`. That is the fix, and it is a different output for the same input.

**A merged span's confidence can now be lower than before, except under rule
2.** `_union` took the maximum of the two; it takes the surviving reading's.
Every other field already came from that reading, so this removes an exception
rather than adding one — but a caller reading `confidence` off a merged span
sees a different number.

The first version of this paragraph said rule 2 was unaffected "since it picks
the more confident span as the winner first". **That is only true when both
spans answer the `untouchable` predicate the same way.** Rule 2 picks its winner
by untouchability *before* confidence, so a caller supplying a custom predicate
can have a 0.6 catalog reading take the identity from a 0.9 model reading of the
same type — and the winner's confidence would then have lowered a number the
module docstring documents as the maximum.

So rule 2 passes its confidence explicitly, and the difference is the other side
of the same argument rather than an exception to it: **a same-type merge joins
two readings that agree**, so the number is evidence about one conclusion and
the maximum is right. Everywhere else the losing reading is gone, and its
confidence goes with it.

**A merged span's `boosted` can now be false where it was true, and true where
it was false.** It was `a.boosted or b.boosted`, the same shape the confidence
had, and it was left standing when the confidence was fixed — so a merge could
report that its number was raised by surrounding context when the boost belonged
to the reading it dropped. That produces a record the deterministic layer cannot
emit: a boost never applies at confidence 1.0, and the merge could return
`confidence=1.0, boosted=True`.

It follows the surviving reading now, except under rule 2, where it follows the
*confidence*: that rule takes the maximum, and the flag says **this** number was
boosted. The `or` was right by accident when the maximum came from the boosted
side and wrong when it did not.

**`Resolution.trace` gains three rule names** — `nested-specificity-merge`,
`nested-confidence-merge`, `nested-sensitivity-merge` — and they replace
**`nesting-outer-wins`**, which is what these inputs reported before this
change. An earlier draft of this notice said they replace
`specific-inner-merge`. That was true of an intermediate commit on this branch
and false of the released behaviour a consumer actually has: `specific-inner-merge`
requires a non-untouchable outer and is a separate branch that still fires
unchanged, while two untouchable spans in containment fell through to
`nesting-outer-wins`. A rule-string matcher told to migrate from the wrong prior
value would have missed exactly the decisions that changed. The
trace is the decision-evidence interface, so anything matching on rule strings
sees values it has not seen. Nothing in this repository does: `resolve`'s only
caller is `Detector.detect`, which reads `.spans` and never `.trace`. The
sandbox that the module docstring says surfaces these decisions lives outside
it, and this is the notice.

**What does not change:** no span narrows, nothing that was masked stops being
masked, and rule 1 still holds — a checksum span never loses to a non-checksum
one.

Measured end to end, on the input that surfaced it: `Le client Marty (NIR
1 71 07 10 830 660 47)` behind a 24-character placeholder returns `FR_NIR`
rather than `CREDIT_CARD`. A French social security number stops being recorded
as a payment card.

**Nothing was exposed before this and nothing is now.** Both types are Tier 1
and both are masked; the defect is an evidence-layer error, not a leak. That is
why it is a fix and not an incident.

Across the 130-document public corpus the aggregate detection numbers do not
move at all, because the corpus's other losses are NER instability to leading
context and have nothing to do with this rule.

## How far this claims to go **[decided]**

**Not to confluence.** `resolve` remains a fold and remains order-dependent in
general. Legitimately so, in part: a span can bridge two others, and removing it
un-merges them, which is correct rather than a defect. A randomised search over
twenty thousand small span sets, run against the fix, found three shapes where
an absent span still changes the output — and reading them showed all three are
bridges, not this bug.

So the invariant "a span absent from the output cannot influence it" is **false
by design** and must not be written as a test. What this fix closes is one named
gap, with one named consequence. A confluent resolver is a larger question about
five interacting rules, and if it is wanted it deserves its own slice rather
than being smuggled in behind a one-branch fix.

## Testing

The regression test is the three synthetic spans above, asserting the type and
the extent, because the extent is what tells a widening apart from a
replacement. It needs no model and no corpus, which is what makes it a
regression test rather than a corpus expectation.

Three mutations, and the third found a missing test rather than confirming one.

- **Remove the new branch** → the regression test fails with `CREDIT_CARD`.
- **Drop the `untouchable(inner)` requirement** →
  `test_checksum_outer_keeps_its_identity_over_a_more_specific_inner` fails. That
  one is not hypothetical: this branch was written without the condition first,
  and the existing suite caught it before any review did. Rule 1 is why the
  inner's own checksum is required and not merely its specificity — a catalog
  IBAN is not replaced by a model's guess however high a catalog rates the
  guess's type.
- **Drop the specificity comparison**, so the branch fires whenever both are
  untouchable → **every test passed.** This design predicted the existing suite
  would catch it and the prediction was wrong: nothing held that the merge fires
  only when the inner *outranks* the outer, so the more specific of two checksum
  readings would have lost whenever it happened to be the outer one. A test was
  added and the mutation now kills it.

Two more after review found the same thing twice.

- **Drop the tier step from `_outranks`** →
  `test_a_bridge_cannot_flip_the_type_when_two_checksums_tie_on_specificity`
  fails. That test is the review finding, reproduced before the fix.
- **Drop the confidence step** → **every test passed again.** The reason is
  worth recording: the default predicate calls a span untouchable only at
  confidence 1.0, so two untouchable spans never differ on it and the step is
  unreachable through `build_detector`. It exists for a caller supplying its own
  `untouchable`, which `resolve` takes as a parameter — so the test that pins it
  supplies one, and the mutation now kills it.

**A mutation that survives is a missing test — or a better rule.** It happened
five times in this fix. Three were missing tests: the design predicted coverage
that did not exist, a review found a gap first, and a step was reachable only
through a parameter nothing exercised.

The fifth was neither. Dropping rule 2's explicit `boosted` changed nothing that
any test could see, and asking why produced a **better rule than the one being
mutated**: rule 2 takes the maximum confidence, so the flag has to be the one
attached to that number, not the disjunction of both and not the winner's. The
`or` was right by accident in one direction. A surviving mutation is a question
about the rule, and sometimes the answer is that the rule was wrong rather than
untested.

**The lesson the whole branch produced.** A merged span is a claim about which
evidence survived, and every field on it needs that question asked separately.
Four came from the winner, `confidence` did not, and `boosted` was overlooked
even while `confidence` was being fixed for exactly this. The extent is the one
field that is still deliberately both — it is the only one where taking the
union is the point.
