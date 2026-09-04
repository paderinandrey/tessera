# Rule 4 could shrink the mask, so adding a detection could unmask text — design

Found by review on #48, where it was raised against a threshold change. The
threshold is not what causes it and lowering one is not what fixes it, so it is
here on its own.

## The defect

`_resolve_pair`'s rule 4 handles two spans of different types that overlap
without either containing the other. Three of its four branches returned the
**winning span**, whole:

```python
if untouchable(a) != untouchable(b):
    return (a if untouchable(a) else b), "untouchable-wins"
if spec_a != spec_b:
    return (a if spec_a > spec_b else b), "specificity"
if a.confidence != b.confidence:
    return (a if a.confidence > b.confidence else b), "confidence"
return _union(a, b, _more_sensitive(a, b)), "tie-merge-sensitive"   # this one was right
```

The winner's extent is not a superset of the loser's — that is what makes the
overlap partial — so the loser's remainder stops being masked. Reproduced with
two spans, no model and no text:

```python
resolve([LOCATION[10:40]])                 # -> LOCATION[10:40]
resolve([LOCATION[10:40], PERSON[30:45]])  # -> PERSON[30:45]      rule: specificity
```

Characters 10..30 were masked and are not. **Adding a detection subtracted
masking**, which inverts what the layer is for.

## Why it is worth fixing when nothing reaches it

On the 130-document public corpus, rule 4 fires 23 times and drops **zero**
characters, at threshold 0.7 and at 0.5 alike. Every one of those 23 is an
*equal* range, where the winner's extent already is the union. The branch that
can shrink a mask is reached by nothing the repository measures.

That is the argument for the change rather than against it. An unreached branch
is an untested branch: the whole suite — 351 tests — passes with the fix applied
and passes with it reverted. Whatever first sends a partially overlapping pair
of different types through rule 4 would have taken the loss silently, and by
then the dropped reading is gone and no layer above can tell.

It also means the correction is free. The aggregate evaluation output is
byte-identical before and after.

## The change

**Every branch of rule 4 returns the union of the two extents; the winner
supplies only the identity.** The rule already worked that way on a full tie,
and rules 2 and 3 take the union everywhere. Rule 4 was the one place the fold
could return less text than it was given.

The trade is explicit: widening costs **over-masking**, which REQ-38 counts as
the irritation metric and the evaluation gates; narrowing costs **exposure**.
Between the two this repository takes over-masking every time, and every other
rule in the module already did.

**Rule 1 is untouched in the sense that matters.** A lone checksum span still
never loses its *identity* to a non-checksum one. Its extent now grows to cover
both readings, which is a wider mask under the identifier's name.

## What breaks, and for whom

`resolve` is public. **A span returned from rule 4 can now be wider than either
input**, where before it was exactly one of them. Three consequences:

- a caller reading `.start` / `.end` off a resolved span gets a larger range for
  the same input, wherever two different types overlap partially;
- the placeholder substituted for that span covers more of the caller's text, so
  a request that had a word between two entities in the clear may not any more.
  That is more masking, never less;
- `Resolution.trace` is unchanged: the three rule names are the same and fire on
  the same inputs. Only the `kept` span's bounds differ.

**Nothing that was masked stops being masked, in either direction.** That is the
invariant the change installs, and the test below states it as one.

**Nothing was exposed in practice.** The corpus never reaches the branch, and no
release has shipped a configuration that does — but that is a fact about the
inputs measured so far, not a property of the code, which is the distinction
worth writing down.

## Testing

The suite claimed to cover this and did not, in a way worth recording: **two of
the three rule-4 tests were named for a partial overlap and built an equal
range.** `test_partial_overlap_higher_specificity_wins` used `0:15` against
`0:15`; `test_untouchable_beats_higher_specificity_on_partial_overlap` did the
same. The third, `test_specificity_tie_higher_confidence_wins`, did overlap
partially — and asserted only `entity_type`, so the ten characters it dropped
went unexamined. A name is not a shape, and an assertion on the type is not an
assertion on the mask.

A first draft of the repair had the same defect one level down: it put `0:15`
inside `0:25` to make the extents differ, which is *containment* and is answered
by rule 3. It passed, for a rule it did not name. The cases now assert the
applied rule alongside the bounds, so a test that reaches a different branch
than it claims fails rather than passes.

- each of the three branches gets the shape its name promises, asserting bounds
  and the rule that produced them;
- `test_a_new_span_can_never_unmask_what_another_span_marked` states the
  invariant over all three branches rather than one example — the three failed
  the same way, and a test per example is how two of them came to be named for a
  shape they did not build;
- `test_the_winner_still_names_a_widened_span` pins the other half: taking the
  union *and* the loser's identity would close the exposure and corrupt the
  audit record.

Mutations, one branch at a time, restored by inverse substitution:

- **`specificity` returns the winner whole** → three tests fail, including the
  one named for that branch;
- **`untouchable-wins` returns the checksum span whole** → the rule-1 test fails
  on bounds, with the invariant test;
- **`confidence` returns the surer span whole** → the tie test fails on bounds,
  with the invariant test.

Before the fix, all three mutations were the shipped code and all 351 tests
passed.

## The published metrics were stale, and not because of this

The README's table reported `PERSON 0.785 / 0.671 / 0.723`. The current
configuration measures `1.000 / 0.855 / 0.922`, and the difference is #20's span
trimming, which landed without updating the table. Refreshed here because the
measurement was being run anyway and because shipping a resolution change beside
metrics from two changes ago is worse than the diff noise. Precision moved to
1.000, so the note calling PERSON precision "the honest weak spot" now names the
wrong number; recall is the weak spot and the note says so.
