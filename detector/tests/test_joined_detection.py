"""What joining leaves into one text costs detection (#42).

The gateway concatenates every string of a request and detects once, then
returns the spans to the leaves they came from. So a message is never read
alone in production: it is read with whatever else the request carried in front
of and behind it, and a transformer's predictions move with context.

That is a real property and nothing measured it. These tests do, against the
corpus's own annotations rather than against a run of the detector on isolated
text — the difference matters, because agreement with an isolated run counts a
false positive that disappeared as a loss.
"""

import json
from pathlib import Path

import pytest

from tessera_detector.pipeline import Detector, build_detector

pytestmark = pytest.mark.ner

CORPUS = Path(__file__).resolve().parents[2] / "evaluation" / "corpus" / "public.jsonl"
# The shape `mapping::Shape::of` joins with. Two newlines, not one: a separator
# a leaf could itself contain would let a span straddle a boundary invisibly.
JOIN = "\n\n"
CONVERSATION = 4


@pytest.fixture(scope="module")
def detector() -> Detector:
    built = build_detector()
    if not built.ner_available:
        pytest.skip(f"NER is not provisioned ({built.ner_off_reason})")
    return built


def documents() -> list[dict]:
    return [
        json.loads(line)
        for line in CORPUS.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]


def _covered(truth: tuple[str, int, int], spans: list[tuple[str, int, int]]) -> bool:
    entity_type, start, end = truth
    return any(
        found_type == entity_type and found_start <= start and found_end >= end
        for found_type, found_start, found_end in spans
    )


def _score(detector: Detector) -> dict[str, int]:
    """Recall and spurious counts for both strategies, against one truth list."""
    docs = documents()
    totals = {
        "truth": 0,
        "separate_found": 0,
        "joined_found": 0,
        "separate_spurious": 0,
        "joined_spurious": 0,
    }
    for start in range(0, len(docs) - CONVERSATION + 1, CONVERSATION):
        group = docs[start : start + CONVERSATION]
        joined_text = JOIN.join(document["text"] for document in group)

        truth: list[tuple[str, int, int]] = []
        separate: list[tuple[str, int, int]] = []
        offset = 0
        for document in group:
            truth += [
                (entity["entity_type"], entity["start"] + offset, entity["end"] + offset)
                for entity in document["entities"]
            ]
            separate += [
                (span.entity_type, span.start + offset, span.end + offset)
                for span in detector.detect(document["text"])
            ]
            offset += len(document["text"]) + len(JOIN)

        joined = [
            (span.entity_type, span.start, span.end) for span in detector.detect(joined_text)
        ]

        totals["truth"] += len(truth)
        for name, spans in (("separate", separate), ("joined", joined)):
            totals[f"{name}_found"] += sum(1 for entity in truth if _covered(entity, spans))
            totals[f"{name}_spurious"] += sum(
                1 for span in spans if not any(_covered(entity, [span]) for entity in truth)
            )
    return totals


def test_joining_does_not_cost_recall(detector: Detector) -> None:
    # The claim this file exists to hold. Reading a message with its neighbours
    # must not find fewer of the entities the corpus annotates than reading it
    # alone — if it does, the gateway's one-detection-per-request design is
    # trading privacy for throughput and nobody has been told.
    #
    # Measured equal at the time of writing: 164 of 196 either way. The
    # assertion is an inequality rather than the number, because the number
    # moves with the weights and the claim does not.
    totals = _score(detector)

    assert totals["joined_found"] >= totals["separate_found"], (
        "joining leaves found fewer annotated entities than reading them apart: "
        f"{totals['joined_found']} against {totals['separate_found']} of {totals['truth']}"
    )


def test_joining_does_not_cost_precision(detector: Detector) -> None:
    # The other half, and the surprise. Joining produced *fewer* spans covering
    # no annotation — 35 against 59 — so the context that moves predictions
    # around moves them toward the annotations more often than away.
    #
    # It is asserted because a regression here is a regression in over-masking,
    # which costs the caller text they never asked to have hidden.
    totals = _score(detector)

    assert totals["joined_spurious"] <= totals["separate_spurious"], (
        "joining produced more spans covering no annotation: "
        f"{totals['joined_spurious']} against {totals['separate_spurious']}"
    )
