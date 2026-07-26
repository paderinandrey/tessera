from tessera_detector.deterministic import DeterministicDetector
from tessera_detector.spans import Span


def detect(text: str) -> list[Span]:
    return DeterministicDetector().detect(text)


def spans_of_type(spans: list[Span], entity_type: str) -> list[Span]:
    return [s for s in spans if s.entity_type == entity_type]


def test_iban_detected_in_original_coordinates() -> None:
    text = "Virement sur IBAN DE89 3704 0044 0532 0130 00 avant vendredi."
    (span,) = detect(text)
    assert span.entity_type == "IBAN"
    assert text[span.start : span.end] == "DE89 3704 0044 0532 0130 00"
    assert span.confidence == 1.0
    assert span.tier == 1
    assert span.recognizer == "catalog:iban"


def test_iban_with_nbsp_separators_detected() -> None:
    # NBSP-separated groups: normalization folds them for matching, offsets map back.
    text = "IBAN: DE89 3704 0044 0532 0130 00."
    (span,) = detect(text)
    assert span.entity_type == "IBAN"
    assert text[span.start : span.end] == "DE89 3704 0044 0532 0130 00"


def test_broken_checksum_produces_no_span_at_all() -> None:
    # REQ-2 acceptance: a failed checksum drops the candidate entirely,
    # it does not lower confidence.
    # (The last digit is 02, not 01: the 18-digit tail of the 01-variant happens to
    # pass Luhn and would legitimately surface as a CREDIT_CARD candidate.)
    assert detect("IBAN DE89 3704 0044 0532 0130 02 invalide") == []
    assert detect("Karte 4111 1111 1111 1112") == []
    assert detect("AVS 756.9217.0769.84") == []


def test_mixed_language_text_multiple_entities() -> None:
    text = (
        "Le client (AVS 756.9217.0769.85) hat die Karte 4111 1111 1111 1111 benutzt, "
        "NIR 2 95 10 99 126 111 93, Steuer-ID 36 574 261 809."
    )
    spans = detect(text)
    assert text[slice(*_bounds(spans_of_type(spans, "CH_AVS")))] == "756.9217.0769.85"
    assert text[slice(*_bounds(spans_of_type(spans, "CREDIT_CARD")))] == "4111 1111 1111 1111"
    assert text[slice(*_bounds(spans_of_type(spans, "FR_NIR")))] == "2 95 10 99 126 111 93"
    assert text[slice(*_bounds(spans_of_type(spans, "DE_STEUER_ID")))] == "36 574 261 809"
    assert all(s.tier == 1 and s.confidence == 1.0 for s in spans)


def _bounds(spans: list[Span]) -> tuple[int, int]:
    assert len(spans) == 1, f"expected exactly one span, got {spans}"
    return spans[0].start, spans[0].end


def test_clean_text_produces_nothing() -> None:
    assert detect("Bonjour, veuillez confirmer le rendez-vous de demain.") == []
    assert detect("") == []


def test_catalog_drives_entity_types() -> None:
    # The engine knows nothing about concrete identifiers: types come from the catalog.
    detector = DeterministicDetector()
    types = {rule.entity_type for rule in detector.rules}
    assert {"IBAN", "CREDIT_CARD", "CH_AVS", "FR_NIR", "DE_STEUER_ID"} <= types
