"""Does asking one label per call recover what competition suppresses?

#46 says multi-label inference suppresses scores, so entities fall under
thresholds calibrated without competition, and names `Texier` — claimed by
`location` at 0.585 while `person`, asked alone, scores it 0.704. That
mechanism is real and this script confirms it.

The detector asks one question per tier: three tier-2 labels in one call, eleven
tier-3 labels in another. The alternative is one call per label — no
competition, and no possibility of one label suppressing another's score.

Measured over the 130-document public corpus, scored **by position**, because a
placeholder's name is not what protects anyone:

    grouped (today)      found 185/196   over-masked 34 spans    9.4s
    one label per call   found 182/196   over-masked 38 spans   34.1s

**Those two rows on their own are not a fair comparison, and reporting them
alone was this script's first version.** Every threshold in the catalog was
swept on the grouped path (#45), so judging the other shape at those numbers
compares a calibrated configuration against an uncalibrated one. The
single-label arm therefore gets a sweep of its own:

    thresholds -0.2      found 187/196   over-masked 58 spans
    thresholds -0.1      found 186/196   over-masked 46 spans
    thresholds +0.1      found 180/196   over-masked 28 spans

**It can beat the grouped arm on coverage** — 186 and 187 against 185 — which
the unswept comparison hid. What it cannot do is beat it at *equal* coverage:
matching 185 lands it near 44 over-masked spans against 34, and it spends 3.6x
the wall clock getting there. Grouped dominates rather than merely wins, which
is a weaker claim than the first version made and a true one.

The membership at the unswept point is still worth reading. The single-label arm
gains exactly one entity — `Texier`, the example #46 is built on — and loses
four, one of them an Article 9 special category a gate covers.

**Scores are relative.** With a label set the model contrasts; with one label it
has nothing to contrast against, and a weak-but-correct label can come out lower
rather than higher. Competition suppresses some scores and supports others.

The consequence that outlives this script: **every threshold in the catalog is
calibrated against competition.** Any change to the asking shape invalidates the
calibration, and a re-sweep is the price of proposing one — not an optional
refinement, since without one this script's own first answer was wrong.

Not run in CI: it needs the NER weights and takes about **nine minutes** on an
M-series laptop — the swept rows are the slow part, because a lower bar produces
many more spans for the resolver to fold. Measured rather than guessed; the
first version of this line said three, from timing one row and multiplying.

It is here to be re-run when somebody proposes changing the asking shape.

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

    # **The single-label arm gets a sweep, because otherwise the comparison is
    # not fair and the deficit is partly the unfairness.** Every threshold in
    # the catalog was swept on the grouped path (#45), so judging the other
    # shape at those numbers compares a calibrated configuration against an
    # uncalibrated one. Offsets rather than a full re-sweep: this is a frontier,
    # not a calibration, and the question it has to answer is only whether the
    # single-label arm can reach the grouped arm's coverage at any bar.
    print()
    frontier: list[tuple[int, int]] = [
        (len(results['one label per call'][0]), results['one label per call'][1])
    ]
    for offset in (-0.2, -0.1, 0.1):
        recognizer.passes = tuple(
            InferencePass(
                tier=one.tier,
                labels=one.labels,
                threshold=max(0.01, min(0.99, one.threshold + offset)),
            )
            for one in single
        )
        shifted = tuple(
            kind.__class__(**{**{f: getattr(kind, f) for f in kind.__slots__},
                              "threshold": max(0.01, min(0.99, kind.threshold + offset))})
            for kind in recognizer.types
        )
        keep_types, keep_index = recognizer.types, recognizer._by_label
        recognizer.types = shifted
        recognizer._by_label = {kind.label: kind for kind in shifted}
        found, over, elapsed = _run(detector, rows)
        frontier.append((len(found), over))
        print(
            f"{'  ' + f'thresholds {offset:+.1f}':20} found {len(found):3}/{total}   "
            f"over-masked {over:3} spans   {elapsed:6.1f}s"
        )
        recognizer.types, recognizer._by_label = keep_types, keep_index
    recognizer.passes = grouped

    grouped_found, _, _ = results["grouped"]
    single_found, _, _ = results["one label per call"]

    print("\none label per call gains:")
    for entity in sorted(single_found - grouped_found):
        print(f"  {entity[0]:11} {entity[3]:12} {entity[4]!r}")
    print("\none label per call loses:")
    for entity in sorted(grouped_found - single_found):
        print(f"  {entity[0]:11} {entity[3]:12} {entity[4]!r}")

    # **The verdict is an exit status**, so a future run that reverses it
    # cannot be reported as confirming it — and this script has two halves that
    # point opposite ways, because the swept arm does beat grouped on coverage
    # alone.
    #
    # So the question it answers is **domination**, not coverage: is there a
    # single-label configuration that finds at least as much while over-masking
    # no more? Coverage alone is the number somebody would quote to justify the
    # change, and it is the one that needs its price attached.
    ceiling = results["grouped"][1]
    dominates = any(
        found >= len(grouped_found) and over <= ceiling for found, over in frontier
    )
    print(
        "\na single-label configuration dominates grouped; the asking shape is worth changing"
        if dominates
        else f"\nno single-label configuration measured finds >= {len(grouped_found)} "
        f"while over-masking <= {ceiling}"
    )
    return 0 if dominates else 1


if __name__ == "__main__":
    sys.exit(main())
