"""Does asking one label per call recover what competition suppresses?

#46 says multi-label inference suppresses scores, so entities fall under
thresholds calibrated without competition, and names `Texier` — claimed by
`location` at 0.585 while `person`, asked alone, scores it 0.704. That
mechanism is real and this script confirms it.

The detector used to ask one question per tier: three tier-2 labels in one call,
eleven tier-3 labels in another. It now asks a third, `person` alone, for the
reason below. The alternative this script was written to test is one call per
label — no competition, and no possibility of one label suppressing another's
score.

Measured over the 130-document public corpus, scored **by position**, because a
placeholder's name is not what protects anyone:

    grouped (one call per tier)   found 179/196   over-masked 28 spans   10.9s   2 calls
    shipped (#97)                 found 184/196   over-masked 35 spans   12.5s   3 calls
    one label per call            found 180/196   over-masked 37 spans   38.0s  14 calls

**The shipped shape is neither of the two this script was written to compare**,
and that is #97's answer to #46. `person` is asked in a call of its own *as well
as* in tier 2's, and the union beats both arms on coverage at near-grouped cost.
The script used to read its grouped arm off `recognizer.passes`, which stopped
being the grouped shape when #97 landed — so it would have compared the shipped
hybrid against figures recorded for grouped and called the result a confirmation.
Raised by review on #100. The grouped arm is composed from `types` now, and
`shipped` is an arm of its own.

**The conclusion this script used to carry — "grouped dominates" — was true of a
corpus that could not show the case against it.** It was measured before the
generator drew apostrophe-bearing surnames, of which the corpus then held zero in
196 annotated values. Grouped loses whole surnames when a tier-2 competitor takes
the argmax and then fails its own bar, which is #46's mechanism and was invisible
at the time. The figures above are a re-measurement on the current corpus, so
they also differ from the ones this docstring used to quote (185/34/9.4s and
182/38/34.1s) for that second reason.

**Against the shipped shape, one label per call gains nothing.** It loses four:
`test génétique`, `Humbert et Fils`, and `Haase` and `Marin` — the last two
because competition *supports* a score as readily as it suppresses one, which is
the same pair #97 lost when its first attempt removed the grouped call. The
sweep cannot buy its way past either: matching 184 found costs 46 over-masked
spans against 35, and 186 costs 57.

    thresholds -0.2      found 186/196   over-masked 57 spans
    thresholds -0.1      found 184/196   over-masked 46 spans
    thresholds +0.1      found 177/196   over-masked 27 spans

Every threshold in the catalog was swept on the grouped path (#45), so the
single-label arm gets offsets of its own rather than being judged at bars
calibrated for a different shape — this is a frontier, not a calibration, and
the only question is whether it reaches the shipped arm's coverage at any bar.

**Scores are relative.** With a label set the model contrasts; with one label it
has nothing to contrast against, and a weak-but-correct label can come out lower
rather than higher. Competition suppresses some scores and supports others.

The consequence that outlives this script: **every threshold in the catalog is
calibrated against competition.** Any change to the asking shape invalidates the
calibration, and a re-sweep is the price of proposing one — not an optional
refinement, since without one this script's own first answer was wrong. #97 did
not pay it, which is why `person`'s own call keeps the catalog's bars: it adds a
reading rather than replacing the one the bars were swept against.

Not run in CI: it needs the NER weights, and the row times above and below sum
to about **three minutes** on an M-series laptop plus the model load — the swept
rows are the slow part, because a lower bar produces many more spans for the
resolver to fold. Taken from the rows the run prints rather than from a
stopwatch, and lower than the nine minutes this line used to claim: that figure
predates both the current corpus and the shipped arm, and is not a number this
run can confirm.

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
    # **Built rather than read off the recognizer.** `recognizer.passes` used to
    # be the grouped shape, so this line used to be `grouped = recognizer.passes`
    # — and #97 made that false: `person` is now asked in a call of its own as
    # well as in tier 2's, so the tuple holds a third shape and the arm labelled
    # `grouped` would have been running it while being compared against the
    # grouped figures recorded above. Raised by review on #100. Composed from
    # `types` here, which is where the tiers actually live.
    by_tier: dict[int, list[str]] = {}
    for kind in sorted(recognizer.types, key=lambda kind: (kind.tier, kind.label)):
        by_tier.setdefault(kind.tier, []).append(kind.label)
    grouped = tuple(
        InferencePass(
            tier=tier,
            labels=tuple(labels),
            threshold=min(k.threshold for k in recognizer.types if k.tier == tier),
        )
        for tier, labels in sorted(by_tier.items())
    )
    shipped = recognizer.passes
    single = tuple(
        InferencePass(tier=kind.tier, labels=(kind.label,), threshold=kind.threshold)
        for kind in sorted(recognizer.types, key=lambda kind: (kind.tier, kind.label))
    )

    results = {}
    for name, passes in (
        ("grouped", grouped),
        ("shipped (#97)", shipped),
        ("one label per call", single),
    ):
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
    recognizer.passes = shipped

    # Compared against what is shipped, not against `grouped`. The grouped shape
    # stopped being production in #97, and measuring a candidate against a
    # baseline nobody runs is how a script keeps confirming a conclusion that has
    # already moved — the defect review on #100 found one line above.
    shipped_found, _, _ = results["shipped (#97)"]
    single_found, _, _ = results["one label per call"]

    print("\none label per call gains over the shipped shape:")
    for entity in sorted(single_found - shipped_found):
        print(f"  {entity[0]:11} {entity[3]:12} {entity[4]!r}")
    print("\none label per call loses against it:")
    for entity in sorted(shipped_found - single_found):
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
    ceiling = results["shipped (#97)"][1]
    dominates = any(
        found >= len(shipped_found) and over <= ceiling for found, over in frontier
    )
    print(
        "\na single-label configuration dominates the shipped shape; the asking shape is "
        "worth changing again"
        if dominates
        else f"\nno single-label configuration measured finds >= {len(shipped_found)} "
        f"while over-masking <= {ceiling}"
    )
    return 0 if dominates else 1


if __name__ == "__main__":
    sys.exit(main())
