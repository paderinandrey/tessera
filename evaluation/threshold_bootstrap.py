"""Is a swept threshold a shape in the data, or noise a best-of-four found?

`PERSON`'s threshold was chosen by sweeping four values over the 130-document
public corpus and taking the best — a procedure that can find noise, on a
sample small enough for it to. The reported result and the CI gate then come
from the same rows, so the number is training-set performance and not an
independent measurement. Raised in review on #48.

This does not fix that; nothing on one corpus can. It bounds it: a **paired
bootstrap over documents** asks how much of the gap between two thresholds
survives resampling the corpus. A difference that vanishes under resampling was
noise; one whose confidence interval excludes zero is a shape in these
documents. Whether these documents resemble a client's text is a different
question and the private corpus is what answers it.

Predeclared, before running:

  statistic   entities the joined path covers, and entities lost to joining
  selection   **most entities covered on the joined path**, ties broken by fewer
              over-masked spans on the separate path — the rule that picked 0.5
              from the sweep, written out so it can be re-run rather than
              assumed. A resample whose best is shared by several thresholds
              selects none of them and is reported undecided
  resamples   2000 groups sampled with replacement, seeded
  decision    the threshold is calibrated if the selection rule returns it on
              >= 95% of resamples

**The predeclared test fails, and what fails has changed.** When this was
written, re-running the selection returned 0.5 on 86.4% of resamples — below the
bar, but with 98.2% of resamples selecting 0.5 or unable to separate it from 0.4.
The instability was a tie-break on a plateau and nothing near 0.7 survived.

**Re-measured after #97 gave `person` a call of its own, the selection moves off
0.5 entirely:**

    0.4:  88.9%      0.5:  9.8%      0.6:  0.1%      0.7:  0.0%
    undecided: 1.2%

0.4 is no longer tied with 0.5; it wins outright, because the shape change moved
which entities the joined path covers. Per group over the whole corpus:

    0.4   joined_found 182   separate_found 184   lost 4   separate_overmasked 38
    0.5   joined_found 180   separate_found 184   lost 5   separate_overmasked 36

**What 0.4 buys and where it spends.** Two more entities covered on the *joined*
path for two more over-masked spans on the *separate* path. `separate_found` is
184 either way — on the path an ordinary request takes, 0.4 finds nothing extra
and over-masks twice more. The selection rule prefers it because the rule was
written around #44's joined-path concern, which is the one case where the gain
lands. That is a reason to read the rule's verdict rather than apply it: this
script measures, and whether to spend separate-path precision on joined-path
recall is not a question a sort key should answer by itself.

It also does not reach what #97 left behind: `person` scores `L(U+2019)Hopital`
at 0.019 and `dell(U+2019)Orto` at 0.268 asked alone, both below 0.4.

**One column above changed definition rather than behaviour**, and the two are
easy to confuse because this run reports both kinds of change at once. `lost`
used to be computed here as "covered apart and not covered together", which is
not the gate's `_lost` — that one asks which *words* a truth leaves unmasked on
each path. Measured both ways under the current shape, to separate the effects:

    predicate                   0.4   0.5   0.6   0.7
    the gate's `_lost`            4     5    10    15
    the old full-coverage one     3     4     9    14

The old predicate gives 4 at 0.5, which is the historical table's value exactly,
so **the shape change did not move this column at all** — the whole difference is
the definition. `selection_key` reads only `joined_found` and
`separate_overmasked`, so the selection percentages are unaffected either way.
Raised by review on #101.

**The bar moved to 0.4**, following the rule rather than overruling it, and the
run above now judges 0.4: selected on 88.9%, which is still under the predeclared
95%, so the new value is no more *calibrated* than the old one was. What changed
is which uncalibrated value the rule prefers.

The argument for following it is the asymmetry rather than the sort key. 0.4
costs PERSON precision on the separate path — 0.959 to 0.934, three false
positives to five, with recall and the per-document leak inventory unchanged — and
buys two entities on the joined path, which is also production: `proxy::mask_all`
gives a `Slot::Text` its own call, while a `Slot::Json` document's leaves are
concatenated and read as one text. An entity nobody masked is the disclosure this
gateway exists to prevent; an over-masked span costs the model a placeholder where
a harmless word was. See `ner.yaml` for that argument beside the number.

**The selection is re-run inside every resample, not conditioned on its own
result.** A first version fixed 0.5 and bootstrapped the pairwise differences
around it, which asks "given that 0.5 won, how large is its margin" and cannot
answer "would 0.5 have won again". Conditioning on the observed winner hides
exactly the selection bias a best-of-four invites: with four candidates on 130
documents, some threshold wins by chance, and the margin around whichever one
did looks reassuring either way. Raised in review on #48, and it was right.

The pairwise comparisons are still reported, because a margin is worth knowing
once the selection is shown to be stable — but they are the second question.

Detection runs once per threshold and its per-group counts are cached; the
bootstrap resamples the cache, so no threshold is measured twice and the
resampling adds no model time.

Run from the repository root:

    uv run --project detector --group ner python evaluation/threshold_bootstrap.py

`--group ner` is not optional: `gliner` is declared only in that dependency
group, so without it the run reaches `GlinerRecognizer` and fails on the import
even where `TESSERA_NER_MODEL` supplies the weights.
"""

import copy
import random
import sys
from pathlib import Path

import yaml

# The joined-path scoring lives with the test that gates it, so this script and
# that gate cannot drift into measuring two different things.
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "detector" / "tests"))

import test_joined_detection as joined

from tessera_detector.evaluation import EvalEntity, overmasking_counts
from tessera_detector.models import find_model
from tessera_detector.ner import GlinerRecognizer, NerType, load_ner_types
from tessera_detector.pipeline import Detector
from tessera_detector.spans import Span

CATALOG = (
    Path(__file__).resolve().parents[1]
    / "detector"
    / "src"
    / "tessera_detector"
    / "catalog"
    / "ner.yaml"
)
TUNED_TYPE = "PERSON"
THRESHOLDS = (0.4, 0.5, 0.6, 0.7)


def _shipped_threshold() -> float:
    """What the catalog actually declares, rather than a second copy of it.

    This was `CHOSEN = 0.5`, a literal — so the day the catalog moved, every
    line below would have reported about a value nothing ships, including the
    verdict. The same defect review found in `inference_shape.py`'s grouped arm
    on #100, in a script whose whole job is to judge this number.
    """
    catalog = yaml.safe_load(CATALOG.read_text(encoding="utf-8"))
    for entry in catalog["entities"]:
        if entry["entity_type"] == TUNED_TYPE:
            return float(entry["threshold"])
    raise SystemExit(f"no {TUNED_TYPE} entry in {CATALOG}")


CHOSEN = _shipped_threshold()
if CHOSEN not in THRESHOLDS:
    raise SystemExit(
        f"{TUNED_TYPE} ships {CHOSEN}, which is not among the swept values "
        f"{THRESHOLDS} — add it, or this script judges a number it never measured"
    )
RESAMPLES = 2000
# The rule the sweep applied, as a sort key: **most entities covered on the
# joined path**, then fewest over-masked spans on the separate path. Written as
# code because a rule that lives only in prose cannot be re-run, and re-running
# it is the whole point.
#
# An earlier version minimised `lost_to_joining` instead. Those are not the same
# objective: a threshold that makes *both* paths miss an entity lowers
# `lost_to_joining` while joined recall gets worse, so it could win a resample
# for losing detections everywhere. The sweep argued from joined recall and this
# now asks the same question. Raised in review on #48.
def selection_key(totals: dict[str, int]) -> tuple[int, int]:
    return (-totals["joined_found"], totals["separate_overmasked"])

SEED = 20260905
# Predeclared: below this the difference is not distinguishable from resampling
# noise and the threshold is not calibrated, only fitted.
DECISION = 0.95


def types_at(threshold: float) -> tuple[NerType, ...]:
    """The packaged NER types with one threshold replaced.

    Through the parsed catalog rather than by rewriting a line of the file: a
    first version of this script edited `ner.yaml` by line number, and when the
    file grew a comment the edit landed on prose, the threshold never moved,
    and the run reported a comparison it had not made.
    """
    catalog = yaml.safe_load(CATALOG.read_text(encoding="utf-8"))
    entries = copy.deepcopy(catalog)
    for entry in entries["entities"]:
        if entry["entity_type"] == TUNED_TYPE:
            entry["threshold"] = threshold
            break
    else:
        raise SystemExit(f"no {TUNED_TYPE} entry in {CATALOG}")
    types = load_ner_types(yaml.safe_dump(entries))
    actual = next(t.threshold for t in types if t.entity_type == TUNED_TYPE)
    assert actual == threshold, f"threshold did not take: {actual} != {threshold}"
    return types


def counts_at(threshold: float, model_path: Path) -> list[dict[str, int]]:
    recognizer = GlinerRecognizer(model_path, types=types_at(threshold))
    detector = Detector(recognizer=recognizer, model_id=f"bootstrap@{threshold}")
    redacted = joined._redacted_types(detector)
    rows = []
    for group in joined._documents():
        truth, separate, together = joined._rebased(detector, group)
        text = joined.JOIN.join(document["text"] for document in group)
        entities = [EvalEntity(entity_type=s.entity_type, start=s.start, end=s.end) for s in truth]

        def overmasked(predictions: list[Span], gold: list[EvalEntity] = entities) -> int:
            counts = overmasking_counts(gold, predictions, types=redacted)
            return sum(whole for _, whole in counts.values()) - sum(
                kept for kept, _ in counts.values()
            )

        rows.append(
            {
                "truth": len(truth),
                "joined_found": sum(1 for e in truth if joined._covered(text, e, together)),
                "separate_found": sum(1 for e in truth if joined._covered(text, e, separate)),
                # `joined._lost`, not a re-implementation of it. This used to ask
                # `_covered(separate) and not _covered(together)`, which is a
                # different question — `_lost` is about the *words* a truth leaves
                # unmasked on each path, and the docstring above claims this
                # script cannot drift from the gate. It had.
                "lost_to_joining": sum(
                    1 for e in truth if joined._lost(text, e, separate, together)
                ),
                "joined_overmasked": overmasked(together),
                "separate_overmasked": overmasked(separate),
            }
        )
    return rows


def bootstrap(
    chosen: list[dict[str, int]], rival: list[dict[str, int]], key: str
) -> tuple[float, float, int, int]:
    rng = random.Random(SEED)
    groups = len(chosen)
    higher = tied = 0
    differences = []
    for _ in range(RESAMPLES):
        sample = [rng.randrange(groups) for _ in range(groups)]
        a = sum(chosen[i][key] for i in sample)
        b = sum(rival[i][key] for i in sample)
        differences.append(a - b)
        if a > b:
            higher += 1
        elif a == b:
            tied += 1
    differences.sort()
    low = differences[int(0.025 * RESAMPLES)]
    high = differences[int(0.975 * RESAMPLES) - 1]
    return higher / RESAMPLES, tied / RESAMPLES, low, high


def main() -> int:
    model_path = find_model()
    if model_path is None:
        print("no NER weights: run `make model` or set TESSERA_NER_MODEL", file=sys.stderr)
        return 1

    measured = {}
    for threshold in THRESHOLDS:
        measured[threshold] = counts_at(threshold, model_path)
        rows = measured[threshold]
        totals = {key: sum(row[key] for row in rows) for key in rows[0]}
        print(f"threshold {threshold}: {totals}", flush=True)

    groups = len(measured[CHOSEN])
    totals_over_corpus = {
        threshold: {
            key: sum(row[key] for row in measured[threshold]) for key in measured[threshold][0]
        }
        for threshold in THRESHOLDS
    }
    print(f"\nbootstrap, {RESAMPLES} resamples of {groups} document groups")

    # The selection re-run inside each resample. This is the question; the
    # pairwise margins below are the follow-up.
    rng = random.Random(SEED)
    selected = dict.fromkeys(THRESHOLDS, 0)
    # A resample whose minimum is shared by several thresholds selects none of
    # them. `min` would award it to whichever comes first in `THRESHOLDS`, so
    # the reported rates would encode a list's order as a result — and 0.4 sits
    # first, which is exactly the neighbour the calibration question is about.
    # An undecided resample is data; a tie broken by tuple position is not.
    unresolved = 0
    # Resamples whose winner set lies entirely on the plateau, decided or not.
    # Counting only outright wins would drop exactly the ties between 0.4 and
    # 0.5 — which are plateau selections, and the most plateau-ish ones there
    # are — and understate the very quantity this line exists to report.
    plateau_selections = 0
    rounds: list[list[float]] = []
    for _ in range(RESAMPLES):
        sample = [rng.randrange(groups) for _ in range(groups)]
        resampled = {
            threshold: {
                key: sum(measured[threshold][i][key] for i in sample)
                for key in measured[threshold][0]
            }
            for threshold in THRESHOLDS
        }
        best = min(selection_key(resampled[t]) for t in THRESHOLDS)
        winners = [t for t in THRESHOLDS if selection_key(resampled[t]) == best]
        if len(winners) == 1:
            selected[winners[0]] += 1
        else:
            unresolved += 1
        rounds.append(winners)
    print("\n  the selection rule re-run on each resample picks:")
    for threshold in THRESHOLDS:
        print(f"    {threshold}: {selected[threshold] / RESAMPLES:6.1%}")
    print(f"    undecided (the rule cannot separate two or more): {unresolved / RESAMPLES:6.1%}")
    stability = selected[CHOSEN] / RESAMPLES

    # The plateau: thresholds tied with the chosen one on joined recall in
    # **every cached group**, which is what makes them tied in every possible
    # resample rather than on the total. An earlier version compared corpus
    # totals once and claimed the stronger property; equal totals can hide
    # opposite per-group differences that resampling then pulls apart. Raised in
    # review on #48, and the `for _ in (0,)` it was written with inspected
    # nothing at all.
    plateau = {
        threshold
        for threshold in THRESHOLDS
        if all(
            measured[threshold][i]["joined_found"] == measured[CHOSEN][i]["joined_found"]
            for i in range(groups)
        )
    }
    plateau_selections = sum(1 for winners in rounds if set(winners) <= plateau)
    on_plateau = plateau_selections / RESAMPLES
    # **The decision this script was written to defend, stated directly rather
    # than through the plateau.** The change it reports on is "lower the bar from
    # 0.7", and the plateau around the shipped value was a proxy for that — a
    # faithful one only while the shipped value and the winner were tied. They
    # are not any more, so the proxy now discards every resample won by the
    # *other* low threshold and reports a decision as unstable because a
    # different low value beat it. Asked as itself: how often does the selection
    # land below the bar this change lowered? Raised by review on #101.
    LOWERED_FROM = max(THRESHOLDS)
    lowered = sum(1 for winners in rounds if all(t < LOWERED_FROM for t in winners))
    on_lowered = lowered / RESAMPLES

    # **Around the shipped threshold, which is not always the winner.** This
    # heading used to say "conditioned on the observed winner" and `CHOSEN` used
    # to be a literal equal to it. Now `CHOSEN` is whatever the catalog ships, so
    # when the full-corpus winner is a different value the old heading described
    # a comparison this code does not make. Both are named instead. Raised by
    # review on #101.
    observed = min(THRESHOLDS, key=lambda t: selection_key(totals_over_corpus[t]))
    if observed != CHOSEN:
        print(
            f"\n  the full corpus selects {observed}; {CHOSEN} is what the catalog "
            "ships, and the margins below are around the shipped value"
        )
    print(f"\n  pairwise margins around the shipped threshold ({CHOSEN}):")
    verdicts = []
    for key, want in (("joined_found", "more"), ("lost_to_joining", "fewer")):
        for rival in THRESHOLDS:
            if rival == CHOSEN:
                continue
            higher, tied, low, high = bootstrap(measured[CHOSEN], measured[rival], key)
            share = higher if want == "more" else 1 - higher - tied
            print(
                f"  {key:18} {CHOSEN} vs {rival}: {want} in {share:.1%} of resamples "
                f"(tied {tied:.1%}), 95% CI of the difference [{low}, {high}]"
            )
            verdicts.append((key, rival, share, tied))

    print()
    for key, rival, _share, tied in verdicts:
        if tied > DECISION:
            print(f"  {CHOSEN} and {rival} are indistinguishable on {key} — a plateau, not a peak")

    # The decision the change actually makes is to lower the bar from 0.7, not
    # to prefer 0.5 over its neighbour on the same plateau. Reported second, and
    # it does not change the exit status: this criterion was written after the
    # predeclared one failed, and a script that exits 0 on a failed predeclared
    # test is presenting a post-hoc criterion as validation. Disclosure is not a
    # substitute for the verdict. Raised in review on #48.
    print(
        f"\n  the selection lands below {LOWERED_FROM} on {on_lowered:.1%} of resamples — "
        f"the decision this change makes, asked as itself"
    )
    if on_lowered < DECISION:
        print(
            f"  that is below {DECISION:.0%}: even lowering the bar is not stable here."
        )
    plateau_text = "/".join(str(t) for t in sorted(plateau))
    print(
        f"  and on the plateau around the shipped {CHOSEN} ({plateau_text}) on "
        f"{on_plateau:.1%} — the thresholds tied with it on joined recall in every group"
    )
    if plateau == {CHOSEN}:
        print(
            f"  {CHOSEN} has no plateau left: nothing is tied with it per group, so that "
            "figure is its own stability and not a wider result"
        )

    print()
    if stability < DECISION:
        print(
            f"  FAIL (predeclared): re-running the selection picks {CHOSEN} on "
            f"{stability:.1%} of resamples, below {DECISION:.0%}."
        )
        print(f"  {CHOSEN} is NOT calibrated as an exact value and must not be called one.")
        print(
            f"  The {on_lowered:.1%} above is a different and weaker claim — that the bar "
            f"belongs below {LOWERED_FROM} — and does not rescue this verdict."
        )
        return 1
    print(f"  {CHOSEN} is selected on {stability:.1%} of resamples, clearing {DECISION:.0%}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
