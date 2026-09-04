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
  comparison  each threshold against 0.5, paired per document group
  resamples   2000 groups sampled with replacement, seeded
  decision    0.5 is calibrated if it wins on >= 95% of resamples

Detection runs once per threshold and its per-group counts are cached; the
bootstrap resamples the cache, so no threshold is measured twice and the
resampling adds no model time.

Run from the repository root:

    uv run --project detector python evaluation/threshold_bootstrap.py
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

    print(f"\npaired bootstrap, {RESAMPLES} resamples of {len(measured[CHOSEN])} document groups")
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
    for key, rival, share, tied in verdicts:
        if tied > DECISION:
            print(f"  {CHOSEN} and {rival} are indistinguishable on {key} — a plateau, not a peak")
        elif share < DECISION:
            print(
                f"  FAIL: {CHOSEN} beats {rival} on {key} in only {share:.1%} of resamples, "
                f"below the predeclared {DECISION:.0%} — fitted rather than calibrated"
            )
            return 1
    print(f"  {CHOSEN} clears the predeclared {DECISION:.0%} bar wherever it is not a tie")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
