from tessera_detector.pipeline import DEFAULT_MODEL_ID, Detector
from tessera_detector.spans import Span


def detect(text: str) -> list[Span]:
    return Detector(model_id=DEFAULT_MODEL_ID).detect(text)


def test_steuernummer_with_trigger_detected_and_boosted() -> None:
    # The 13-digit form's base confidence 0.6 sits below the 0.7 threshold and
    # only a context trigger lifts it over.
    text = "Die Steuernummer 9181081508155 des Mandanten liegt vor."
    (span,) = detect(text)
    assert span.entity_type == "DE_STEUERNUMMER"
    assert text[span.start : span.end] == "9181081508155"
    assert span.boosted is True
    assert span.confidence == 0.8
    assert span.tier == 1


def test_steuernummer_without_trigger_stays_below_threshold() -> None:
    assert detect("Referenz 9181081508155 im Anhang.") == []


def test_slash_form_steuernummer_needs_no_trigger() -> None:
    # A JSON field carries the trigger in its key, which is never scanned (#104).
    text = "Referenz 181/815/08155 im Anhang."
    (span,) = detect(text)
    assert span.entity_type == "DE_STEUERNUMMER"
    assert text[span.start : span.end] == "181/815/08155"
    assert span.boosted is False
    assert span.confidence == 0.7
    (bare,) = detect("181/815/08155")
    assert bare.entity_type == "DE_STEUERNUMMER"


def test_both_steuernummer_forms_keep_the_public_recognizer_value() -> None:
    slash = detect("Referenz 181/815/08155 im Anhang.")
    unified = detect("Steuernummer 9181081508155 bitte angeben.")
    assert [s.recognizer for s in slash] == ["catalog:de_steuernummer"]
    assert [s.recognizer for s in unified] == ["catalog:de_steuernummer"]


def test_slash_form_steuernummer_is_still_boosted_by_a_trigger() -> None:
    (span,) = detect("Die Steuernummer 181/815/08155 des Mandanten liegt vor.")
    assert span.boosted is True
    assert span.confidence == 0.9


def test_trigger_outside_window_does_not_boost() -> None:
    # The trigger sits more than six tokens away from the candidate.
    text = (
        "Die Steuernummer wurde gestern per Post an die neue Adresse geschickt, "
        "siehe 9181081508155."
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


def test_trigger_matches_complete_terms_only() -> None:
    # "Post-Nr." contains the "st-nr" trigger as a substring; a boundary-aware
    # match must not boost the structurally valid 13-digit candidate.
    assert detect("Post-Nr. 9181081508155 im Anhang.") == []


def test_13_digit_steuernummer_beats_card_rule() -> None:
    # 9181081508003 is a structurally valid Steuernummer that also passes Luhn.
    # Cards start at 14 digits (13-digit Visas are extinct), so the trigger-boosted
    # tax number must win instead of an untouchable CREDIT_CARD span.
    text = "Steuernummer 9181081508003 des Mandanten."
    (span,) = detect(text)
    assert span.entity_type == "DE_STEUERNUMMER"


def test_window_counts_punctuation_separated_terms() -> None:
    # Comma-glued terms are separate tokens: the trigger sits 7 tokens away.
    assert detect("Steuernummer,a,b,c,d,e,f,g,9181081508155") == []


def test_multi_token_trigger_must_be_contiguous_on_one_side() -> None:
    # "st" before and "nr" after the candidate must not concatenate into the
    # canonical "st nr" trigger — it never occurs contiguously in the input.
    assert detect("st 9181081508155 nr") == []


def test_boost_addition_rounds_at_threshold() -> None:
    # 0.1 + 0.7 must reach a 0.8 threshold despite binary float addition.
    catalog = """
version: 1
identifiers:
  - id: rounded
    entity_type: ROUNDED
    tier: 2
    confidence: 0.1
    threshold: 0.8
    boost:
      value: 0.7
      window: 3
      triggers: ["marker"]
    pattern: 'xx+'
"""
    (span,) = Detector(catalog, model_id=DEFAULT_MODEL_ID).detect("marker xx")
    assert span.confidence == 0.8


def test_context_scan_is_char_bounded() -> None:
    # The context window is local by definition: a single enormous token between
    # trigger and candidate pushes the trigger beyond the bounded character scan.
    blob = "y" * 2000
    assert detect(f"Steuernummer {blob} 9181081508155") == []


def test_boost_never_reduces_confidence() -> None:
    # Base 0.995 with a trigger must stay 0.995, not drop to the 0.99 cap.
    catalog = """
version: 1
identifiers:
  - id: high_base
    entity_type: HB
    tier: 2
    confidence: 0.995
    threshold: 0.993
    boost:
      value: 0.1
      window: 3
      triggers: ["marker"]
    pattern: 'xx+'
"""
    (span,) = Detector(catalog, model_id=DEFAULT_MODEL_ID).detect("marker xx")
    assert span.confidence == 0.995


def test_truncated_token_at_scan_cutoff_is_dropped() -> None:
    # The scan slice starts inside "post-nr": the truncated "st-nr" fragment
    # must not canonicalize into a complete "st nr" trigger.
    text = "post-nr " + "y" * 377 + " 9181081508155"
    assert detect(text) == []
