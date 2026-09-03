"""Measure what a namespaced placeholder does to detection (#32).

A later turn's text carries placeholders from earlier turns. This asks whether
`[PERSON_1.3f7a2b91c4d5]` changes what the detector finds, against the bare
`[PERSON_1]` that ships today.

The question is not whether either shape is harmless — a bare token in text is
what ships now — but whether the namespaced one is *worse*.

Run from the repository root, with the model provisioned:
    make model
    uv sync --project detector --group ner --group serve
    uv run --project detector python evaluation/placeholder_shape.py
"""

import json
import sys
from pathlib import Path

from tessera_detector.pipeline import build_detector

CORPUS = Path(__file__).parent / "corpus" / "public.jsonl"
SHAPES = {
    "bare": "[PERSON_1]",
    "namespaced": "[PERSON_1.3f7a2b91c4d5]",
}


def documents():
    # One JSON object per line, the shape `evaluate.py` already reads. A glob
    # over `*.json` finds nothing here, and the gate would report an absent
    # measurement rather than a number.
    for line in CORPUS.read_text(encoding="utf-8").splitlines():
        if line.strip():
            record = json.loads(line)
            yield record["id"], record["text"]


def spans(detector, text):
    return [(s.entity_type, s.start, s.end) for s in detector.detect(text)]


def covered(entity, found, offset):
    """Same type, and some span covers the entity's characters.

    **Coverage rather than equality, and the distinction is the measurement.**
    An entity found under wider bounds is still masked; an entity not found is
    exposed. Comparing exact spans calls both of them losses, and the resolver
    deliberately widens a span when a model reading overlaps a catalog one — so
    an equality metric reports a detection failure for a boundary that moved on
    purpose. It did, on the run that produced this file, and the four "losses"
    that stopped the slice included an `FR_NIR` that was found the whole time.
    """
    entity_type, start, end = entity
    return any(
        found_type == entity_type and found_start <= start + offset and found_end >= end + offset
        for found_type, found_start, found_end in found
    )


def exact(entity, found, offset):
    entity_type, start, end = entity
    return (entity_type, start + offset, end + offset) in found


def main():
    detector = build_detector()
    # Without weights `build_detector` returns a deterministic-only detector,
    # and this measurement would report "not worse" without ever exercising the
    # layer most likely to react to a changed token shape. A gate that cannot
    # fail is not a gate.
    if not detector.ner_available:
        print(
            f"NER is not provisioned ({detector.ner_off_reason}); "
            "this measurement cannot gate",
            file=sys.stderr,
        )
        return 2

    totals = {shape: {"missed": 0, "drifted": 0, "inside": 0} for shape in SHAPES}
    documents_seen = 0
    baseline_spans = 0

    for _, text in documents():
        documents_seen += 1
        base = spans(detector, text)
        baseline_spans += len(base)
        for shape, token in SHAPES.items():
            # The token goes where a real one would: in front of the text, as
            # an earlier turn's mask echoed back into this one.
            offset = len(token) + 1
            found = spans(detector, f"{token} {text}")
            totals[shape]["inside"] += sum(1 for _, start, _ in found if start < offset)
            for entity in base:
                if not covered(entity, found, offset):
                    totals[shape]["missed"] += 1
                elif not exact(entity, found, offset):
                    totals[shape]["drifted"] += 1

    print(
        json.dumps(
            {"documents": documents_seen, "baseline_spans": baseline_spans, "per_shape": totals},
            indent=2,
        )
    )

    bare, namespaced = totals["bare"], totals["namespaced"]
    # **Only `missed` gates.** An entity found under wider bounds is masked; one
    # not found is exposed, and those are not the same cost. `drifted` and
    # `inside` are reported because they are real and someone should see them,
    # not because a boundary that moved should stop a slice.
    worse = namespaced["missed"] > bare["missed"]
    print(f"\nnamespaced misses more than bare: {worse}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
