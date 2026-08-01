"""Evaluation primitives: span matching and per-type metrics (REQ-37, REQ-38).

A gold entity counts as found when a predicted span of the same type overlaps it
with IoU >= 0.5; every gold entity matches at most one prediction and vice versa.
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
    result: dict[str, Metrics] = defaultdict(Metrics)
    matched_predictions: set[int] = set()
    for entity in entities:
        best: tuple[float, int] | None = None
        for i, span in enumerate(predictions):
            if i in matched_predictions or span.entity_type != entity.entity_type:
                continue
            iou = _iou(entity.start, entity.end, span.start, span.end)
            if iou >= IOU_THRESHOLD and (best is None or iou > best[0]):
                best = (iou, i)
        if best is None:
            result[entity.entity_type].fn += 1
        else:
            matched_predictions.add(best[1])
            result[entity.entity_type].tp += 1
    for i, span in enumerate(predictions):
        if i not in matched_predictions:
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


__all__ = ["EvalEntity", "Metrics", "Summary", "evaluate_document", "summarize"]
