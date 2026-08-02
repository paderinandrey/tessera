import sys
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


def test_auto_mode_falls_back_when_the_ner_runtime_is_absent(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Weights survive in the user-wide cache across virtualenv rebuilds; a base
    # install must degrade to deterministic-only, not crash importing gliner.
    monkeypatch.setenv("TESSERA_NER_MODEL", str(tmp_path))
    monkeypatch.setattr(
        "tessera_detector.pipeline.find_model", lambda: tmp_path
    )
    monkeypatch.setitem(sys.modules, "gliner", None)
    detector = build_detector()
    assert detector.ner_available is False


def test_required_mode_reports_a_missing_ner_runtime(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(
        "tessera_detector.pipeline.find_model", lambda: tmp_path
    )
    monkeypatch.setitem(sys.modules, "gliner", None)
    with pytest.raises(ModelUnavailable, match="ner"):
        build_detector(ner=True)


def article_9(start: int, end: int, confidence: float = 0.4) -> Span:
    return Span(
        entity_type="TRADE_UNION",
        start=start,
        end=end,
        confidence=confidence,
        recognizer="ner:fake",
        tier=3,
    )


def org(start: int, end: int, confidence: float = 0.8) -> Span:
    return Span(
        entity_type="ORG",
        start=start,
        end=end,
        confidence=confidence,
        recognizer="ner:fake",
        tier=2,
    )


def test_article_9_span_outranks_an_overlapping_org() -> None:
    # "Die Gewerkschaft ver.di": knowing it is a union membership mention is
    # the more sensitive reading, and specificity 35 beats ORG's 10.
    text = "Die Gewerkschaft ver.di hat geantwortet."
    recognizer = FakeRecognizer(
        [org(4, 23), article_9(4, 23)],
        specificity={"ORG": 10, "TRADE_UNION": 35},
    )
    spans = Detector(recognizer=recognizer).detect(text)
    assert [s.entity_type for s in spans] == ["TRADE_UNION"]


def test_checksum_identifier_still_outranks_an_article_9_span() -> None:
    text = "mail: anna.keller@example.ch"
    recognizer = FakeRecognizer(
        [article_9(6, 28, confidence=0.9)], specificity={"TRADE_UNION": 35}
    )
    spans = Detector(recognizer=recognizer).detect(text)
    assert [(s.entity_type, s.start, s.end) for s in spans] == [("EMAIL", 6, 28)]


def test_deterministic_only_still_resolves_conflicts() -> None:
    detector = Detector(recognizer=FakeRecognizer([person(0, 5)]))
    spans = detector.deterministic_only("Keller a écrit à anna.keller@example.ch")
    assert [s.entity_type for s in spans] == ["EMAIL"], "the NER span must not appear"
