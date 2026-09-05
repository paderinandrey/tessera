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

**The predeclared test fails, and the failure is the useful part.** Re-running
the selection returns 0.5 on 86.4% of resamples — below the bar — with 0.6
taking 1.1%, 0.7 and 0.4 none outright, and 12.4% undecided between 0.4 and 0.5.
So the sweep's *exact value* is not recoverable from resampled data and must not
be described as calibrated. The script exits non-zero on that verdict.

What is recoverable is the **plateau**: 98.2% of resamples select 0.5 or cannot
separate it from 0.4 — two thresholds tied on joined recall in every cached
group, and therefore in every possible resample, separated only by two
over-masked spans on the separate path. The instability is entirely a tie-break
between them; nothing near 0.7 survives.

That distinction is reported as two verdicts rather than folded into one,
because the second is a criterion written *after* seeing the first fail, and
that is a thing to declare rather than to quietly substitute. It is defensible
only because it is the decision the change actually makes — lower the bar from
0.7 — and not the decision the strict test asks about, which is whether 0.5
beats 0.4. It does not, reliably, and neither does 0.4 beat 0.5.

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
CHOSEN = 0.5
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
        entities = [EvalEntity(entity_type=s.entity_type, start=s.start, end=s.end) for s in truth]

        def overmasked(predictions: list[Span], gold: list[EvalEntity] = entities) -> int:
            counts = overmasking_counts(gold, predictions, types=redacted)
            return sum(whole for _, whole in counts.values()) - sum(
                kept for kept, _ in counts.values()
            )

        rows.append(
            {
                "truth": len(truth),
                "joined_found": sum(1 for e in truth if joined._covered(e, together)),
                "separate_found": sum(1 for e in truth if joined._covered(e, separate)),
                "lost_to_joining": sum(
                    1
                    for e in truth
                    if joined._covered(e, separate) and not joined._covered(e, together)
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
        totals = {
            threshold: {
                key: sum(measured[threshold][i][key] for i in sample)
                for key in measured[threshold][0]
            }
            for threshold in THRESHOLDS
        }
        best = min(selection_key(totals[t]) for t in THRESHOLDS)
        winners = [t for t in THRESHOLDS if selection_key(totals[t]) == best]
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

    print("\n  pairwise margins, conditioned on the observed winner:")
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
    plateau_text = "/".join(str(t) for t in sorted(plateau))
    print(
        f"\n  the selection lands on the plateau ({plateau_text}) on {on_plateau:.1%} of "
        f"resamples — the thresholds tied with {CHOSEN} on joined recall in every group, "
        f"and so in every possible resample"
    )
    if on_plateau < DECISION:
        print(f"  even the plateau is below {DECISION:.0%}: the sweep found noise.")

    print()
    if stability < DECISION:
        print(
            f"  FAIL (predeclared): re-running the selection picks {CHOSEN} on "
            f"{stability:.1%} of resamples, below {DECISION:.0%}."
        )
        print(f"  {CHOSEN} is NOT calibrated as an exact value and must not be called one.")
        print("  The plateau result above stands on its own and does not rescue this verdict.")
        return 1
    print(f"  {CHOSEN} is selected on {stability:.1%} of resamples, clearing {DECISION:.0%}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
