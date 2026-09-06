"""Does asking one label per call recover what competition suppresses?

#46 says multi-label inference suppresses scores, so entities fall under
thresholds calibrated without competition, and names `Texier` — claimed by
`location` at 0.585 while `person`, asked alone, scores it 0.704. That
mechanism is real and this script confirms it. **The remedy it suggests is
refuted by the same corpus**, which is why the script exists rather than a
paragraph asserting it.

The detector asks one question per tier: three tier-2 labels in one call, eleven
tier-3 labels in another. The alternative is one call per label — no competition,
and no possibility of one label suppressing another's score.

Measured over the 130-document public corpus, scored **by position**, because a
placeholder's name is not what protects anyone:

    grouped (today)      found 185/196   over-masked 34 spans    9.4s
    one label per call   found 182/196   over-masked 38 spans   34.1s

Worse on all three axes, and the membership is what makes it decisive rather
than close: the single-label run gains exactly one entity — `Texier`, this
issue's own example — and loses four, one of them an Article 9 special category
that a gate covers.

**Scores are relative.** With a label set the model contrasts; with one label it
has nothing to contrast against, and a weak-but-correct label can come out lower
rather than higher. Competition suppresses some scores and supports others, and
this corpus says it supports more than it suppresses.

The consequence that outlives this script: **every threshold in the catalog is
calibrated against competition**, because PERSON's 0.5 was swept on the grouped
path (#45). Any change to the asking shape invalidates the calibration, and this
one starts four-for-one behind before the re-sweep begins.

Not run in CI: it needs the NER weights and takes about forty-five seconds. It
is here to be re-run when somebody proposes changing the asking shape again.

    uv run --group ner python evaluation/inference_shape.py
"""

from __future__ import annotations

import json
import sys
import time
from pathlib import Path

from tessera_detector.ner import InferencePass
from tessera_detector.pipeline import build_detector

CORPUS = Path(__file__).resolve().parent / "corpus" / "public.jsonl"

Entity = tuple[str, int, int, str, str]


def _covered(spans: list, start: int, end: int) -> bool:
    """By position: any prediction covering the characters counts.

    Not by type. A span that masks a name while calling it an organization has
    still masked the name, and the caller is no worse off for the label being
    wrong. Requiring the gold type makes the difference this script is about
    invisible — that is one of the three ways #44's earlier measurements
    contradicted each other.
    """
    return any(span.start <= start and span.end >= end for span in spans)


def _run(detector, rows: list[dict]) -> tuple[set[Entity], int, float]:
    found: set[Entity] = set()
    over = 0
    started = time.perf_counter()
    for row in rows:
        spans = detector.detect(row["text"])
        gold = [(entity["start"], entity["end"]) for entity in row["entities"]]
        for entity in row["entities"]:
            start, end = entity["start"], entity["end"]
            if _covered(spans, start, end):
                found.add(
                    (row["id"], start, end, entity["entity_type"], row["text"][start:end])
                )
        for span in spans:
            if not any(start < span.end and span.start < end for start, end in gold):
                over += 1
    return found, over, time.perf_counter() - started


def main() -> int:
    rows = [json.loads(line) for line in CORPUS.read_text().splitlines()]
    total = sum(len(row["entities"]) for row in rows)

    detector = build_detector()
    if detector.recognizer is None:
        print(f"NER is not provisioned ({detector.ner_off_reason}); run `make model`")
        return 2

    recognizer = detector.recognizer
    grouped = recognizer.passes
    single = tuple(
        InferencePass(tier=kind.tier, labels=(kind.label,), threshold=kind.threshold)
        for kind in sorted(recognizer.types, key=lambda kind: (kind.tier, kind.label))
    )

    results = {}
    for name, passes in (("grouped", grouped), ("one label per call", single)):
        recognizer.passes = passes
        results[name] = _run(detector, rows)
        found, over, elapsed = results[name]
        print(
            f"{name:20} found {len(found):3}/{total}   "
            f"over-masked {over:3} spans   {elapsed:6.1f}s   ({len(passes)} calls per text)"
        )
    recognizer.passes = grouped

    grouped_found, _, _ = results["grouped"]
    single_found, _, _ = results["one label per call"]

    print("\none label per call gains:")
    for entity in sorted(single_found - grouped_found):
        print(f"  {entity[0]:11} {entity[3]:12} {entity[4]!r}")
    print("\none label per call loses:")
    for entity in sorted(grouped_found - single_found):
        print(f"  {entity[0]:11} {entity[3]:12} {entity[4]!r}")

    # **The verdict is an exit status**, so a future run that reverses this
    # cannot be reported as confirming it. A script that printed a table and
    # always returned zero would let the reader take whichever half they came
    # for.
    better = len(single_found) > len(grouped_found)
    print(
        f"\none label per call is {'better' if better else 'not better'} on coverage: "
        f"{len(single_found)} against {len(grouped_found)}"
    )
    return 0 if better else 1


if __name__ == "__main__":
    sys.exit(main())
