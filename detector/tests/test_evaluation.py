import pytest

from tessera_detector.evaluation import EvalEntity, Metrics, evaluate_document, summarize
from tessera_detector.spans import Span


def pred(entity_type: str, start: int, end: int) -> Span:
    return Span(
        entity_type=entity_type,
        start=start,
        end=end,
        confidence=1.0,
        recognizer="catalog:x",
        tier=1,
    )


def gold(entity_type: str, start: int, end: int) -> EvalEntity:
    return EvalEntity(entity_type=entity_type, start=start, end=end)


def test_exact_match_counts_as_tp() -> None:
    result = evaluate_document([gold("IBAN", 0, 20)], [pred("IBAN", 0, 20)])
    assert (result["IBAN"].tp, result["IBAN"].fp, result["IBAN"].fn) == (1, 0, 0)


def test_type_mismatch_is_both_fp_and_fn() -> None:
    result = evaluate_document([gold("IBAN", 0, 20)], [pred("CREDIT_CARD", 0, 20)])
    assert result["IBAN"].fn == 1
    assert result["CREDIT_CARD"].fp == 1


def test_iou_overlap_above_half_matches() -> None:
    # Predicted span covers 15 of 20 gold chars and nothing else: IoU = 0.75.
    result = evaluate_document([gold("IBAN", 0, 20)], [pred("IBAN", 5, 20)])
    assert result["IBAN"].tp == 1


def test_small_overlap_does_not_match() -> None:
    # 5 shared chars over a 35-char union: IoU ~ 0.14.
    result = evaluate_document([gold("IBAN", 0, 20)], [pred("IBAN", 15, 35)])
    assert (result["IBAN"].tp, result["IBAN"].fp, result["IBAN"].fn) == (0, 1, 1)


def test_each_gold_matches_at_most_once() -> None:
    # Two predictions over one gold entity: one TP, one FP.
    result = evaluate_document(
        [gold("IBAN", 0, 20)], [pred("IBAN", 0, 20), pred("IBAN", 2, 20)]
    )
    assert (result["IBAN"].tp, result["IBAN"].fp, result["IBAN"].fn) == (1, 1, 0)


def test_missed_entity_is_fn() -> None:
    result = evaluate_document([gold("FR_NIR", 0, 15)], [])
    assert result["FR_NIR"].fn == 1


def test_metrics_math() -> None:
    m = Metrics(tp=8, fp=2, fn=2)
    assert m.precision == pytest.approx(0.8)
    assert m.recall == pytest.approx(0.8)
    assert m.f1 == pytest.approx(0.8)


def test_zero_division_yields_zero() -> None:
    m = Metrics()
    assert m.precision == 0.0
    assert m.recall == 0.0
    assert m.f1 == 0.0


def test_summarize_aggregates_and_reports_tier1() -> None:
    per_doc = [
        {"IBAN": Metrics(tp=1)},
        {"IBAN": Metrics(fn=1), "EMAIL": Metrics(tp=2, fp=1)},
    ]
    summary = summarize(per_doc, tier1_types={"IBAN"})
    assert summary.per_type["IBAN"].tp == 1
    assert summary.per_type["IBAN"].fn == 1
    assert summary.tier1_recall == pytest.approx(0.5)
    assert summary.per_type["EMAIL"].precision == pytest.approx(2 / 3)
