import json
from pathlib import Path

from tessera_detector.evaluation import joined_in_context, returned_to_values
from tessera_detector.pipeline import DEFAULT_MODEL_ID, Detector
from tessera_detector.spans import Span

VECTORS = (
    Path(__file__).resolve().parents[2] / "gateway" / "src" / "testdata" / "joined_in_context.json"
)


def _at(entity_type: str, start: int, end: int) -> Span:
    return Span(
        entity_type=entity_type,
        start=start,
        end=end,
        confidence=1.0,
        recognizer="test",
        tier=2,
    )


def test_the_join_matches_the_gateways() -> None:
    for case in json.loads(VECTORS.read_text(encoding="utf-8")):
        joined = joined_in_context([(key, value) for key, value in case["leaves"]])
        assert joined.text == case["text"]
        assert [list(r) for r in joined.ranges] == case["ranges"]
        assert joined.contexts == case["contexts"]


def test_a_span_in_a_member_name_is_dropped_and_one_reaching_into_the_value_is_cut() -> None:
    joined = joined_in_context([("religion", "athée"), ("Weber", "Weber")])
    assert joined.text == "religion: athée\n\nWeber: Weber"
    kept, refused = returned_to_values(
        joined, [_at("RELIGION", 0, 15), _at("PERSON", 17, 22), _at("PERSON", 24, 29)]
    )
    assert [(s.entity_type, s.start, s.end) for s in kept] == [
        ("RELIGION", 10, 15),
        ("PERSON", 24, 29),
    ]
    assert refused == []


def test_a_span_from_one_value_into_the_next_member_name_is_refused() -> None:
    joined = joined_in_context([("a", "Weber"), ("b", "Bern")])
    kept, refused = returned_to_values(joined, [_at("PERSON", 3, 11)])
    assert kept == []
    assert [(s.start, s.end) for s in refused] == [(3, 11)]


def test_a_member_name_lifts_a_13_digit_steuernummer_over_its_bar() -> None:
    detector = Detector(model_id=DEFAULT_MODEL_ID)
    joined = joined_in_context([("steuernummer", "9181081508155")])
    kept, _ = returned_to_values(joined, detector.detect(joined.text))
    assert [(s.entity_type, joined.text[s.start : s.end]) for s in kept] == [
        ("DE_STEUERNUMMER", "9181081508155")
    ]
    assert detector.detect("9181081508155") == []
