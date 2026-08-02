import pytest

from tessera_detector.evaluation import EvalEntity, Metrics, evaluate_document, summarize
from tessera_detector.spans import Span


def pred_span(entity_type: str, start: int, end: int) -> Span:
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
    result = evaluate_document([gold("IBAN", 0, 20)], [pred_span("IBAN", 0, 20)])
    assert (result["IBAN"].tp, result["IBAN"].fp, result["IBAN"].fn) == (1, 0, 0)


def test_type_mismatch_is_both_fp_and_fn() -> None:
    result = evaluate_document([gold("IBAN", 0, 20)], [pred_span("CREDIT_CARD", 0, 20)])
    assert result["IBAN"].fn == 1
    assert result["CREDIT_CARD"].fp == 1


def test_iou_overlap_above_half_matches() -> None:
    # Predicted span covers 15 of 20 gold chars and nothing else: IoU = 0.75.
    result = evaluate_document([gold("IBAN", 0, 20)], [pred_span("IBAN", 5, 20)])
    assert result["IBAN"].tp == 1


def test_small_overlap_does_not_match() -> None:
    # 5 shared chars over a 35-char union: IoU ~ 0.14.
    result = evaluate_document([gold("IBAN", 0, 20)], [pred_span("IBAN", 15, 35)])
    assert (result["IBAN"].tp, result["IBAN"].fp, result["IBAN"].fn) == (0, 1, 1)


def test_each_gold_matches_at_most_once() -> None:
    # Two predictions over one gold entity: one TP, one FP.
    result = evaluate_document(
        [gold("IBAN", 0, 20)], [pred_span("IBAN", 0, 20), pred_span("IBAN", 2, 20)]
    )
    assert (result["IBAN"].tp, result["IBAN"].fp, result["IBAN"].fn) == (1, 1, 0)


def test_matching_maximizes_pairs_over_greedy_iou() -> None:
    # Gold A (0,20) prefers p1 (4,24), IoU 2/3, over p2 (0,13), IoU 0.65 — but p1
    # is gold B's (8,28) only eligible match. Maximum matching pairs A-p2 and B-p1.
    golds = [gold("IBAN", 0, 20), gold("IBAN", 8, 28)]
    preds = [pred_span("IBAN", 4, 24), pred_span("IBAN", 0, 13)]
    result = evaluate_document(golds, preds)
    assert (result["IBAN"].tp, result["IBAN"].fp, result["IBAN"].fn) == (2, 0, 0)


def test_matching_is_order_independent() -> None:
    golds = [gold("IBAN", 0, 20), gold("IBAN", 8, 28)]
    preds = [pred_span("IBAN", 4, 24), pred_span("IBAN", 0, 13)]
    forward = evaluate_document(golds, preds)
    reversed_golds = evaluate_document(list(reversed(golds)), preds)
    assert (forward["IBAN"].tp, forward["IBAN"].fp, forward["IBAN"].fn) == (
        reversed_golds["IBAN"].tp,
        reversed_golds["IBAN"].fp,
        reversed_golds["IBAN"].fn,
    )


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


def test_precision_gate_passes_when_above_target() -> None:
    from tessera_detector.evaluation import precision_gate_failures

    per_type = {"ORG": Metrics(tp=9, fp=1), "LOCATION": Metrics(tp=8, fp=2)}
    assert precision_gate_failures(per_type, types={"ORG", "LOCATION"}, target=0.8) == []


def test_precision_gate_reports_each_type_below_target() -> None:
    from tessera_detector.evaluation import precision_gate_failures

    per_type = {"ORG": Metrics(tp=5, fp=5), "LOCATION": Metrics(tp=9, fp=1)}
    failures = precision_gate_failures(per_type, types={"ORG", "LOCATION"}, target=0.8)
    assert [name for name, _ in failures] == ["ORG"]


def test_precision_gate_ignores_types_absent_from_the_run() -> None:
    from tessera_detector.evaluation import precision_gate_failures

    assert precision_gate_failures({"IBAN": Metrics(tp=3)}, types={"ORG"}, target=0.8) == []


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


def test_group_recall_counts_a_cross_category_match_as_covered() -> None:
    from tessera_detector.evaluation import group_recall

    # An ethnicity mention read as religion is still redacted: the operational
    # question is whether the group caught the span at all.
    gold = [EvalEntity(entity_type="ETHNICITY", start=10, end=20)]
    pred = [pred_span("RELIGION", 10, 20)]
    assert group_recall(gold, pred, types={"ETHNICITY", "RELIGION"}) == (1, 1)


def test_group_recall_ignores_predictions_outside_the_group() -> None:
    from tessera_detector.evaluation import group_recall

    gold = [EvalEntity(entity_type="HEALTH", start=0, end=8)]
    pred = [pred_span("PERSON", 0, 8)]
    assert group_recall(gold, pred, types={"HEALTH"}) == (0, 1)


def test_group_recall_needs_a_real_overlap() -> None:
    from tessera_detector.evaluation import group_recall

    gold = [EvalEntity(entity_type="HEALTH", start=0, end=20)]
    pred = [pred_span("HEALTH", 18, 40)]
    assert group_recall(gold, pred, types={"HEALTH"}) == (0, 1)


def test_group_recall_with_no_gold_of_the_group() -> None:
    from tessera_detector.evaluation import group_recall

    assert group_recall([], [], types={"HEALTH"}) == (0, 0)


def test_group_coverage_reports_each_gold_entity() -> None:
    from tessera_detector.evaluation import group_coverage

    gold = [
        EvalEntity(entity_type="HEALTH", start=0, end=10),
        EvalEntity(entity_type="RELIGION", start=20, end=30),
    ]
    pred = [pred_span("ETHNICITY", 0, 10)]
    assert group_coverage(gold, pred, types={"HEALTH", "RELIGION", "ETHNICITY"}) == [
        ("HEALTH", True),
        ("RELIGION", False),
    ]


def test_group_coverage_ignores_gold_outside_the_group() -> None:
    from tessera_detector.evaluation import group_coverage

    gold = [EvalEntity(entity_type="IBAN", start=0, end=10)]
    assert group_coverage(gold, [], types={"HEALTH"}) == []


def test_overmasking_counts_only_predictions_matching_no_gold() -> None:
    from tessera_detector.evaluation import overmasking_counts

    # A location prediction landing on a person's name is a disagreement about
    # the label, not the service masking text it should have left alone.
    gold = [EvalEntity(entity_type="PERSON", start=0, end=8)]
    pred = [pred_span("LOCATION", 0, 8), pred_span("LOCATION", 40, 50)]
    assert overmasking_counts(gold, pred, types={"LOCATION"}) == {"LOCATION": (1, 2)}


def test_overmasking_counts_ignores_ungated_types() -> None:
    from tessera_detector.evaluation import overmasking_counts

    gold = [EvalEntity(entity_type="PERSON", start=0, end=8)]
    assert overmasking_counts(gold, [pred_span("HEALTH", 40, 50)], types={"LOCATION"}) == {}


def test_overmasking_ignores_a_grazing_overlap() -> None:
    from tessera_detector.evaluation import overmasking_counts

    # One shared character out of forty is not "landing on real data".
    gold = [EvalEntity(entity_type="PERSON", start=0, end=8)]
    assert overmasking_counts(gold, [pred_span("LOCATION", 7, 47)], types={"LOCATION"}) == {
        "LOCATION": (0, 1)
    }
