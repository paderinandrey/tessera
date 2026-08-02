from collections.abc import Mapping
from pathlib import Path

import pytest

from tessera_detector.models import ModelUnavailable
from tessera_detector.pipeline import Detector, build_detector
from tessera_detector.spans import Span


class FakeRecognizer:
    def __init__(self, spans: list[Span], specificity: Mapping[str, int] | None = None) -> None:
        self._spans = spans
        self.specificity: Mapping[str, int] = specificity or {"PERSON": 30}

    def detect(self, text: str) -> list[Span]:
        return list(self._spans)


def person(start: int, end: int, confidence: float = 0.9) -> Span:
    return Span(
        entity_type="PERSON",
        start=start,
        end=end,
        confidence=confidence,
        recognizer="ner:fake",
        tier=2,
    )


def test_without_recognizer_only_deterministic_spans() -> None:
    detector = Detector()
    spans = detector.detect("mail: anna.keller@example.ch")
    assert detector.ner_available is False
    assert [s.entity_type for s in spans] == ["EMAIL"]


def test_ner_spans_join_the_result() -> None:
    detector = Detector(recognizer=FakeRecognizer([person(0, 5)]))
    spans = detector.detect("Keller a écrit à anna.keller@example.ch")
    assert detector.ner_available is True
    assert sorted(s.entity_type for s in spans) == ["EMAIL", "PERSON"]


def test_checksum_span_survives_an_overlapping_ner_span() -> None:
    # The e-mail is a catalog span; a PERSON claiming the same range must not
    # displace it — cross-layer conflicts are resolved once, over the union.
    text = "mail: anna.keller@example.ch"
    detector = Detector(recognizer=FakeRecognizer([person(6, 28, confidence=0.99)]))
    spans = detector.detect(text)
    assert [(s.entity_type, s.start, s.end) for s in spans] == [("EMAIL", 6, 28)]


def test_deterministic_layer_no_longer_resolves_on_its_own() -> None:
    from tessera_detector.deterministic import DeterministicDetector

    raw = DeterministicDetector().detect("mail: anna.keller@example.ch")
    assert [s.entity_type for s in raw] == ["EMAIL"]
    assert DeterministicDetector().specificity["EMAIL"] == 70


def test_build_detector_off_never_loads_a_model(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("TESSERA_NER_MODEL", "/definitely/not/here")
    detector = build_detector(ner=False)
    assert detector.ner_available is False


def test_build_detector_auto_falls_back_without_weights(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.delenv("TESSERA_NER_MODEL", raising=False)
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: tmp_path))
    detector = build_detector()
    assert detector.ner_available is False
    assert [s.entity_type for s in detector.detect("mail: anna.keller@example.ch")] == ["EMAIL"]


def test_build_detector_required_raises_without_weights(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.delenv("TESSERA_NER_MODEL", raising=False)
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: tmp_path))
    with pytest.raises(ModelUnavailable):
        build_detector(ner=True)
