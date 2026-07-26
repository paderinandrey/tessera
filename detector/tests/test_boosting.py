from tessera_detector.deterministic import DeterministicDetector
from tessera_detector.spans import Span


def detect(text: str) -> list[Span]:
    return DeterministicDetector().detect(text)


def test_steuernummer_with_trigger_detected_and_boosted() -> None:
    # No checksum exists for the Steuernummer: base confidence 0.6 sits below the
    # 0.7 threshold and only a context trigger lifts it over.
    text = "Die Steuernummer 181/815/08155 des Mandanten liegt vor."
    (span,) = detect(text)
    assert span.entity_type == "DE_STEUERNUMMER"
    assert text[span.start : span.end] == "181/815/08155"
    assert span.boosted is True
    assert span.confidence == 0.8
    assert span.tier == 1


def test_steuernummer_without_trigger_stays_below_threshold() -> None:
    assert detect("Referenz 181/815/08155 im Anhang.") == []


def test_trigger_outside_window_does_not_boost() -> None:
    # The trigger sits more than six tokens away from the candidate.
    text = (
        "Die Steuernummer wurde gestern per Post an die neue Adresse geschickt, "
        "siehe 181/815/08155."
    )
    assert detect(text) == []


def test_country_wide_form_with_trigger() -> None:
    text = "St.-Nr. 9181081508155 bitte angeben."
    (span,) = detect(text)
    assert span.entity_type == "DE_STEUERNUMMER"
    assert text[span.start : span.end] == "9181081508155"


def test_checksum_spans_never_boosted() -> None:
    # Checksum rules stay at confidence 1.0 with boosted False even near triggers.
    text = "Steuer-ID 36 574 261 809 und IBAN DE89 3704 0044 0532 0130 00."
    spans = detect(text)
    assert {s.entity_type for s in spans} == {"DE_STEUER_ID", "IBAN"}
    assert all(s.confidence == 1.0 and s.boosted is False for s in spans)


def test_boosted_confidence_never_reaches_untouchable() -> None:
    # A boost must not fabricate checksum status: confidence stays below 1.0.
    text = "Steuernummer 181/815/08155"
    (span,) = detect(text)
    assert span.confidence < 1.0
