# A threshold measured against the shape the model is actually asked in — design

Closes #44. Narrows #46.

> The user chose 0.5 over 0.6 with both numbers in front of them. Everything
> else here was decided without them, on a standing instruction.

## What this was going to be, and why it is not

Both #44 and #46 describe one defect: **a threshold is a constant and the score
it judges is not.** The same name scores lower with more text around it, and
lower again when other labels share the inference call. `PERSON`'s bar was 0.7,
and the gateway joins a document's leaves into one text and asks three tier-2
labels at once — both shifts at once.

The obvious answer was a calibration set: labelled examples measured under each
asking-shape, thresholds fitted per shape. That is what both issues ask for and
what I set out to build.

**Sweeping the one number first collapsed the slice.** Measured over the
130-document public corpus, entities counted as found when any span covers them:

| threshold | joined found | joined over-masked | separate found | separate over-masked |
|---|---|---|---|---|
| 0.7 | 174 | 19 | 183 | 33 |
| 0.6 | 178 | 19 | 184 | 33 |
| **0.5** | **182** | **19** | **185** | **34** |
| 0.4 | 182 | 19 | 185 | 36 |

**0.5 recovers eight of the twelve detections joining costs**, adds nothing to
over-masking on the joined path and one span on the separate one. 0.4 buys
nothing further and costs more, so this is an optimum rather than the end of a
range — which is the check that distinguishes a measurement from a slide.

A calibration set may still be worth building. It is not worth building *for
this*, and finding that out cost one sweep.

## The change

`PERSON`'s threshold becomes 0.5 in `ner.yaml`, with the table above written
beside it. The arithmetic goes where an operator reads it, not where a design
document does.

Nothing else moves. The tier split, the label set, the trimming rule and the
resolver are untouched.

## The price, named rather than discovered

**One span in sixty-eight becomes a role phrase with no name in it.** At 0.5 the
model offers `Der Kunde` at 0.552 where at 0.7 it offered nothing, and the
trimming rule keeps it whole — because emptying a span could unmask a name, and
that clause is what a person actually called `Herr` depends on. So it masks a
phrase nobody needed masked.

**It is over-masking and not a miss, which an earlier version of this section
got wrong.** That version said the role phrase "does not save `Karz` beside it".
The document is `mixed-0008`, the name beside it is `Hoareau`, and it has its
own `PERSON` span at 0.842 — clear of every threshold in the sweep. So the cost
is entirely REQ-38's irritation metric and nothing reaches a provider that did
not before. Getting that backwards overstated the risk of the change I was
arguing for, which is the safe direction to be wrong in and still wrong.

At 0.4 there are two such spans. At 0.6 there are none, and half the recovery.
**0.6 is the other defensible answer** and the reason both were put to a human
rather than chosen here.

## Whether 0.5 is calibrated or merely fitted **[added in review]**

A best-of-four sweep on 130 documents can find noise, and the reported result
and the CI gate come from the same rows — so the number was training-set
performance, and the design said nothing about it. Raised in review.

Nothing measured on one corpus can fix that. What it can do is bound it, so
`evaluation/threshold_bootstrap.py` resamples the 33 document groups 2000 times
and **re-runs the selection inside each resample**, with the procedure and its
95% bar written down before running.

```
threshold   joined found   lost to joining   joined over   separate over
     0.7        174              12              19             33
     0.6        178               8              19             33
     0.5        182               4              19             34
     0.4        182               4              19             36
```

**The first version of this script did not re-run the selection.** It fixed 0.5
and bootstrapped the pairwise margins around it, which asks "given that 0.5 won,
how large is its margin" and cannot answer "would 0.5 win again". Conditioning
on the observed winner hides exactly the bias a best-of-four invites: some
candidate wins by chance, and the margin around whichever one did looks
reassuring either way. Raised in review, correctly, and it changed the answer.

**The predeclared test fails.**

```
the selection rule, re-run on each resample, picks:
  0.4   0.0%    0.5  86.4%    0.6  1.1%    0.7  0.0%
  undecided (a tie the rule cannot break)  12.4%
```

86.4% is below the 95% bar. **0.5 is not calibrated as an exact value**, the
earlier claim that "the number is measured rather than chosen" was wrong, and
`ner.yaml` now says so where the number is. **The script exits non-zero on this
verdict**, because one that printed a failure and returned success would be
presenting the criterion below as validation.

**What survives is the plateau.** 98.2% of resamples select 0.5 or cannot
separate it from 0.4 — two thresholds tied on joined recall in every cached
group, and therefore in every possible resample. Nothing near 0.7 is ever
selected. So the decision this change makes, lowering the bar from 0.7, is
stable under resampling; the decision between 0.5 and 0.4 is a coin toss and
either is defensible. 0.5 is taken for the two fewer over-masked spans, which is
what a tie-break is worth and no more.

**That second criterion was written after the first one failed**, which is a
thing to declare rather than quietly substitute. It is defensible only because
it is the decision the change actually makes and not the one the strict test
asks about — and it is reported without changing the exit status, so nothing
downstream can read it as the predeclared test passing.

Three corrections to the procedure itself came out of the same review, and each
changed a number:

- **the selection key asked the wrong question.** It minimised
  `lost_to_joining`, and a threshold that makes *both* paths miss an entity
  lowers that while joined recall gets worse — so it could win a resample for
  losing detections everywhere. The sweep argued from joined recall, and the key
  does now;
- **ties were broken by list order.** `min` awarded a shared minimum to whatever
  came first in `THRESHOLDS`, which is 0.4 — the exact neighbour the calibration
  question is about. An undecided resample is data; a tie broken by tuple
  position is a list's order reported as a result. 0.4's 11.9% was that, and it
  is 0.0% with 12.4% undecided now;
- **the plateau was computed from corpus totals**, then described as a per-
  resample property. Equal totals can hide opposite per-group differences that
  resampling pulls apart. Membership requires equality in every cached group
  now, which is what makes "tied in every possible resample" true rather than
  asserted.

**What this still is not: held-out validation.** Resampling bounds sampling
variation *within* this corpus. It cannot say whether the corpus resembles a
client's text, and the corpus is synthetic with names in fixed template slots.
The private annotated corpus is the measure that answers that question, and it
is reported separately.

## What it does not fix

**#46's example.** `Der Kunde Karz` still leaves `Karz` unmasked — the model
reads it as a location when asked with three labels, and no threshold on
`PERSON` reaches a span the model gave to another type. #46 narrows to that:
not "names fall under the bar" but "a name is claimed by another label and then
fails *its* bar". Four cheap remedies for that are measured and rejected in the
issue.

**The remaining four of twelve.** Eight of the joined path's losses were names
scoring between 0.5 and 0.7. Four are not, and the gate below is tightened so
they are not forgotten behind a number with slack in it.

## Testing

**The recall gate is directional now, and that is a correction rather than a
tightening.** It read `separate_found - joined_found` — a net figure — and
reported 3, because one truth that only *joining* finds cancelled one of the four
that only joining loses. A net gate holds at its constant while names go
unmasked, as long as unrelated ones turn up in other documents, and the caller
whose tool-call JSON lost a name is not compensated by that. `LOST_TO_JOINING`
counts the entities reading leaves apart covers and joining does not: **4**.
Raised in review, with the arithmetic, and the arithmetic was right.

That is the second time this gate was wrong by aggregating over the wrong thing.
The first required the covering span to carry the gold *type*, under which both
strategies found 164 and the gap was invisible. Both versions made the gate
insensitive to exactly what it exists to catch.

**The role-only span is a model-backed test now.** It was
`assert trim("Der Kunde") == "Der Kunde"`, which calls the trimming helper, reads
no catalog and asks the model nothing — so it passed at 0.5, 0.6 and 0.7 alike
and duplicated `test_trimming_never_empties_a_span` one file over. It claimed to
make the threshold's price a fixture and could not have noticed the price
changing. Also raised in review, and correct.
`test_the_lower_threshold_prices_a_role_phrase_and_still_masks_the_name` runs the
recognizer over `mixed-0008` and asserts both halves: the role phrase is
admitted, and `Hoareau` beside it is masked.

Mutations:

- restore the threshold to 0.7 → the model-backed price test fails, naming the
  threshold as what moved. Verified, after a first attempt that **did not
  mutate anything**: the `sed` targeted a line number that the branch's own
  comment block had shifted, so it edited prose, the threshold never moved, and
  the test "passing at 0.7" was the test passing at 0.5. A probe is evidence
  only once it has reproduced the condition it claims to;
- restore the threshold to 0.7 → the directional recall bound fails with
  `12 <= 4`.

Gates unchanged: Tier 1 recall 1.0000, Article 9 coverage 0.9783, LOCATION
over-masking precision 1.0000, and no corpus drift.
