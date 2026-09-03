"""What joining the leaves of one document costs detection (#42).

**This is the `Slot::Json` path and not a conversation.** An earlier version of
this file said a message is never read alone in production, and that is wrong:
`proxy::mask_all` gives every `Slot::Text` its own `detector.detect` call, so a
plain chat message *is* read alone. What gets joined is the leaves **within a
single JSON document** — a tool call's arguments, a structured `content` — which
`mapping::Shape::of` concatenates and `Joined::split` returns to their leaves
afterwards.

So the question these tests answer is narrower than the one they were written
for, and it is a question production actually asks: when several short field
values are read as one text, does the detector find more, less, or the same?

Scored against the corpus's annotations rather than against a run on isolated
text — the difference is not pedantry. Comparing to an isolated run counts a
false positive that disappeared as a loss, and that is how the first attempt at
this measurement reached the opposite conclusion.
"""

import json
from pathlib import Path

import pytest

from tessera_detector.evaluation import EvalEntity, overmasking_counts
from tessera_detector.pipeline import Detector, build_detector
from tessera_detector.spans import Span

pytestmark = pytest.mark.ner

CORPUS = Path(__file__).resolve().parents[2] / "evaluation" / "corpus" / "public.jsonl"
# What `mapping::Shape::of` puts between leaves.
JOIN = "\n\n"
# Leaves per synthetic document. A tool call's arguments carry a handful of
# short values; four is that shape, and the remainder of the corpus is scored
# as a smaller document rather than dropped.
LEAVES = 4


@pytest.fixture(scope="module")
def detector() -> Detector:
    built = build_detector()
    if not built.ner_available:
        pytest.skip(f"NER is not provisioned ({built.ner_off_reason})")
    return built


def _documents() -> list[list[dict]]:
    rows = [
        json.loads(line)
        for line in CORPUS.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    # Every row, including the last partial group: the corpus is 130 rows and
    # four does not divide it, so a plain stride silently dropped two documents
    # — and a document with no annotations is exactly where a false-positive
    # regression would hide from the precision half.
    return [rows[at : at + LEAVES] for at in range(0, len(rows), LEAVES)]


def _inside_one_leaf(span: Span, ranges: list[tuple[int, int]]) -> bool:
    """Whether production could return this span to a leaf.

    Mirrors `mapping::Joined::split`: a span that straddles two leaves or lands
    in a separator is refused there and the request fails, so it is not a
    detection and must not be scored as one.
    """
    return any(start <= span.start and span.end <= end for start, end in ranges)


def _leaf_ranges(group: list[dict]) -> list[tuple[int, int]]:
    ranges: list[tuple[int, int]] = []
    at = 0
    for document in group:
        ranges.append((at, at + len(document["text"])))
        at += len(document["text"]) + len(JOIN)
    return ranges


def _rebased(detector: Detector, group: list[dict]) -> tuple[list[Span], list[Span], list[Span]]:
    """Truth, separate predictions and joined predictions, all in joined coordinates."""
    truth: list[Span] = []
    separate: list[Span] = []
    at = 0
    for document in group:
        truth += [
            Span(
                entity_type=entity["entity_type"],
                start=entity["start"] + at,
                end=entity["end"] + at,
                confidence=1.0,
                recognizer="corpus",
                tier=1,
            )
            for entity in document["entities"]
        ]
        separate += [
            span.model_copy(update={"start": span.start + at, "end": span.end + at})
            for span in detector.detect(document["text"])
        ]
        at += len(document["text"]) + len(JOIN)

    joined = detector.detect(JOIN.join(document["text"] for document in group))
    return truth, separate, joined


def _covered(truth: Span, predictions: list[Span]) -> bool:
    return any(
        prediction.entity_type == truth.entity_type
        and prediction.start <= truth.start
        and prediction.end >= truth.end
        for prediction in predictions
    )


def _score(detector: Detector) -> dict[str, int]:
    totals = dict.fromkeys(
        ("truth", "separate_found", "joined_found", "separate_masked", "joined_masked",
         "separate_overmasked", "joined_overmasked", "crossing"),
        0,
    )
    for group in _documents():
        truth, separate, joined = _rebased(detector, group)
        ranges = _leaf_ranges(group)

        # A span that straddles two leaves or lands in a separator is not a
        # detection in production: `Joined::split` refuses it and the request
        # fails. Counting it as found would let a regression that breaks every
        # such request pass both gates below.
        totals["crossing"] += sum(1 for span in joined if not _inside_one_leaf(span, ranges))

        totals["truth"] += len(truth)
        entities = [
            EvalEntity(entity_type=span.entity_type, start=span.start, end=span.end)
            for span in truth
        ]
        types = {span.entity_type for span in truth}
        for name, predictions in (("separate", separate), ("joined", joined)):
            totals[f"{name}_found"] += sum(1 for entity in truth if _covered(entity, predictions))
            # The established over-masking metric: a prediction is a cost when
            # most of what it hides holds no personal data. Its *type* being
            # wrong is not a cost — the span is redacted either way.
            counts = overmasking_counts(entities, predictions, types=types)
            landed = sum(kept for kept, _ in counts.values())
            total = sum(whole for _, whole in counts.values())
            totals[f"{name}_masked"] += total
            totals[f"{name}_overmasked"] += total - landed
    return totals


def test_a_span_across_a_leaf_boundary_is_not_a_detection() -> None:
    # The predicate on its own, because the corpus produces no crossing span
    # and a counter nothing can move reads as coverage without being any.
    # Needs no model, so it runs wherever the file does.
    ranges = [(0, 10), (12, 20)]

    def at(start: int, end: int) -> Span:
        return Span(
            entity_type="PERSON",
            start=start,
            end=end,
            confidence=0.9,
            recognizer="ner:gliner",
            tier=2,
        )

    assert _inside_one_leaf(at(0, 10), ranges)
    assert _inside_one_leaf(at(13, 19), ranges)
    assert not _inside_one_leaf(at(8, 15), ranges), "a span straddling two leaves was accepted"
    assert not _inside_one_leaf(at(10, 12), ranges), "a span inside a separator was accepted"


def test_no_joined_span_crosses_a_leaf_boundary(detector: Detector) -> None:
    # Production refuses these outright — `Joined::split` returns
    # `BadSpan("across a joined boundary")` and the request 502s — so the two
    # gates below are only meaningful while this holds.
    #
    # It has never been non-zero on this corpus, including with a
    # single-character separator, so it is a guard against a future regression
    # rather than a pinned behaviour. The predicate itself is pinned above.
    assert _score(detector)["crossing"] == 0


def test_joining_does_not_cost_recall(detector: Detector) -> None:
    # Reading a document's leaves as one text must not find fewer of the
    # entities the corpus annotates than reading them apart. If it does, the
    # one-detection-per-document design is trading privacy for throughput and
    # nobody has been told.
    #
    # Measured equal at the time of writing. The assertion is an inequality
    # because the number moves with the weights and the claim does not.
    totals = _score(detector)

    assert totals["joined_found"] >= totals["separate_found"], (
        "joining leaves found fewer annotated entities than reading them apart: "
        f"{totals['joined_found']} against {totals['separate_found']} of {totals['truth']}"
    )


def test_joining_does_not_cost_precision(detector: Detector) -> None:
    # The other half, and the surprise: joining hid *less* text that holds no
    # personal data. A regression here is a regression in over-masking, which
    # costs the caller text they never asked to have hidden.
    totals = _score(detector)

    assert totals["joined_overmasked"] <= totals["separate_overmasked"], (
        "joining over-masked more spans than reading leaves apart: "
        f"{totals['joined_overmasked']} against {totals['separate_overmasked']}"
    )
