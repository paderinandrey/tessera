import pytest
from pydantic import ValidationError

from tessera_detector.spans import Span


def make_span(**overrides: object) -> Span:
    base: dict[str, object] = {
        "entity_type": "IBAN",
        "start": 10,
        "end": 32,
        "confidence": 1.0,
        "recognizer": "catalog:iban",
        "tier": 1,
    }
    base.update(overrides)
    return Span.model_validate(base)


def test_span_fields_roundtrip() -> None:
    span = make_span()
    assert span.entity_type == "IBAN"
    assert (span.start, span.end) == (10, 32)
    assert span.confidence == 1.0
    assert span.recognizer == "catalog:iban"
    assert span.tier == 1
    assert span.boosted is False  # default


def test_span_serializes_to_stable_shape() -> None:
    data = make_span().model_dump()
    assert data == {
        "entity_type": "IBAN",
        "start": 10,
        "end": 32,
        "confidence": 1.0,
        "recognizer": "catalog:iban",
        "tier": 1,
        "boosted": False,
    }


def test_end_must_be_after_start() -> None:
    with pytest.raises(ValidationError):
        make_span(start=5, end=5)


def test_start_must_be_non_negative() -> None:
    with pytest.raises(ValidationError):
        make_span(start=-1, end=4)


def test_confidence_bounded() -> None:
    with pytest.raises(ValidationError):
        make_span(confidence=1.5)
    with pytest.raises(ValidationError):
        make_span(confidence=-0.1)


def test_span_never_carries_text() -> None:
    # The schema must not have a field holding the matched value: spans reference
    # the original text by offsets only, so span objects stay safe to log.
    assert "text" not in Span.model_fields
    assert "value" not in Span.model_fields
