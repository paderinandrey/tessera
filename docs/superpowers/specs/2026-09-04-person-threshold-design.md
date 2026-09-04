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
model offers `Der Kunde` at 0.608 where at 0.7 it offered nothing, and the
trimming rule keeps it whole — because emptying a span could unmask a name, and
that clause is what a person actually called `Herr` depends on. So it masks a
phrase nobody needed masked and does not save `Karz` beside it.

At 0.4 there are two such spans. At 0.6 there are none, and half the recovery.
**0.6 is the other defensible answer** and the reason both were put to a human
rather than chosen here.

## What it does not fix

**#46's example.** `Der Kunde Karz` still leaves `Karz` unmasked — the model
reads it as a location when asked with three labels, and no threshold on
`PERSON` reaches a span the model gave to another type. #46 narrows to that:
not "names fall under the bar" but "a name is claimed by another label and then
fails *its* bar". Four cheap remedies for that are measured and rejected in the
issue.

**The remaining three of twelve.** Eight of the joined path's losses were names
scoring between 0.5 and 0.7. Three are not, and the gate below is tightened so
they are not forgotten behind a number with slack in it.

## Testing

`RECALL_GAP` in `test_joined_detection.py` goes from 9 to 3 — the bound that
stops the joined-versus-separate gap widening, tightened to what the fix
achieves. Leaving it at 9 would have let this change's own benefit be given back
silently.

The role-only span is a test of its own, so the cost is a fixture rather than a
surprise.

Mutation: restore the threshold to 0.7 and the recall bound fails with `9 <= 3`.

Gates unchanged: Tier 1 recall 1.0000, Article 9 coverage 0.9783, LOCATION
over-masking precision 1.0000, and no corpus drift.
