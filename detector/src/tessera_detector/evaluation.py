"""Evaluation primitives: span matching and per-type metrics (REQ-37, REQ-38).

A gold entity counts as found when a predicted span of the same type overlaps it
with IoU >= 0.5; every gold entity matches at most one prediction and vice versa.
The one-to-one assignment maximizes the number of matched pairs, so counts do not
depend on entity order; higher IoU is only a preference among equal-size matchings.
"""

from collections import defaultdict
from dataclasses import dataclass

from .spans import Span

IOU_THRESHOLD = 0.5


@dataclass(frozen=True, slots=True)
class EvalEntity:
    entity_type: str
    start: int
    end: int


@dataclass(slots=True)
class Metrics:
    tp: int = 0
    fp: int = 0
    fn: int = 0

    @property
    def precision(self) -> float:
        total = self.tp + self.fp
        return self.tp / total if total else 0.0

    @property
    def recall(self) -> float:
        total = self.tp + self.fn
        return self.tp / total if total else 0.0

    @property
    def f1(self) -> float:
        denominator = self.precision + self.recall
        return 2 * self.precision * self.recall / denominator if denominator else 0.0


@dataclass(slots=True)
class Summary:
    per_type: dict[str, Metrics]
    tier1_recall: float


def _iou(a_start: int, a_end: int, b_start: int, b_end: int) -> float:
    intersection = min(a_end, b_end) - max(a_start, b_start)
    if intersection <= 0:
        return 0.0
    union = max(a_end, b_end) - min(a_start, b_start)
    return intersection / union


def evaluate_document(entities: list[EvalEntity], predictions: list[Span]) -> dict[str, Metrics]:
    candidates: list[list[int]] = []
    for entity in entities:
        eligible: list[tuple[float, int]] = []
        for i, span in enumerate(predictions):
            if span.entity_type != entity.entity_type:
                continue
            iou = _iou(entity.start, entity.end, span.start, span.end)
            if iou >= IOU_THRESHOLD:
                eligible.append((iou, i))
        eligible.sort(key=lambda pair: pair[0], reverse=True)
        candidates.append([index for _, index in eligible])

    owner: dict[int, int] = {}

    def claim(entity_index: int, visited: set[int]) -> bool:
        for prediction_index in candidates[entity_index]:
            if prediction_index in visited:
                continue
            visited.add(prediction_index)
            current = owner.get(prediction_index)
            if current is None or claim(current, visited):
                owner[prediction_index] = entity_index
                return True
        return False

    for entity_index in range(len(entities)):
        claim(entity_index, set())

    result: dict[str, Metrics] = defaultdict(Metrics)
    matched_entities = set(owner.values())
    for entity_index, entity in enumerate(entities):
        if entity_index in matched_entities:
            result[entity.entity_type].tp += 1
        else:
            result[entity.entity_type].fn += 1
    for i, span in enumerate(predictions):
        if i not in owner:
            result[span.entity_type].fp += 1
    return dict(result)


def summarize(per_document: list[dict[str, Metrics]], *, tier1_types: set[str]) -> Summary:
    per_type: dict[str, Metrics] = defaultdict(Metrics)
    for document in per_document:
        for entity_type, metrics in document.items():
            per_type[entity_type].tp += metrics.tp
            per_type[entity_type].fp += metrics.fp
            per_type[entity_type].fn += metrics.fn
    tier1 = Metrics()
    for entity_type in tier1_types:
        if entity_type in per_type:
            tier1.tp += per_type[entity_type].tp
            tier1.fn += per_type[entity_type].fn
    return Summary(per_type=dict(per_type), tier1_recall=tier1.recall)


def group_coverage(
    entities: list[EvalEntity], predictions: list[Span], *, types: set[str]
) -> list[tuple[str, bool]]:
    """Per gold entity of the group: its type, and whether the group covered it.

    Reported per entity rather than as a total so callers can bucket by
    language and category — a pooled ratio lets a category go dark in one
    language while the aggregate stays above target.
    """
    candidates = [span for span in predictions if span.entity_type in types]
    return [
        (
            entity.entity_type,
            any(
                _iou(entity.start, entity.end, span.start, span.end) >= IOU_THRESHOLD
                for span in candidates
            ),
        )
        for entity in entities
        if entity.entity_type in types
    ]


def group_recall(
    entities: list[EvalEntity], predictions: list[Span], *, types: set[str]
) -> tuple[int, int]:
    """Gold entities of a group covered by any prediction from that group, and the total.

    Per-type recall punishes confusion inside a group that has no operational
    consequence: an ethnicity mention read as religion is still redacted, and
    REQ-3's "misses are not tolerable" is about the span going unnoticed, not
    about picking the right member of the group.
    """
    gold = [entity for entity in entities if entity.entity_type in types]
    candidates = [span for span in predictions if span.entity_type in types]
    covered = sum(
        1
        for entity in gold
        if any(
            _iou(entity.start, entity.end, span.start, span.end) >= IOU_THRESHOLD
            for span in candidates
        )
    )
    return covered, len(gold)


def precision_gate_failures(
    per_type: dict[str, Metrics], *, types: set[str], target: float
) -> list[tuple[str, float]]:
    """Types present in the run whose precision falls below target (REQ-38)."""
    return [
        (entity_type, per_type[entity_type].precision)
        for entity_type in sorted(types)
        if entity_type in per_type and per_type[entity_type].precision < target
    ]


__all__ = [
    "EvalEntity",
    "Metrics",
    "Summary",
    "evaluate_document",
    "group_coverage",
    "group_recall",
    "precision_gate_failures",
    "summarize",
]
