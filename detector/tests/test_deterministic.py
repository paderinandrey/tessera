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
    assert detect("IBAN DE89 3704 0044 0532 0130 02 invalide") == []
    # The 18-digit tail of this variant passes Luhn; the digit-run boundary guard
    # must keep it from surfacing as a CREDIT_CARD candidate.
    assert detect("IBAN DE89 3704 0044 0532 0130 01 invalide") == []
    assert detect("Karte 4111 1111 1111 1112") == []
    assert detect("AVS 756.9217.0769.84") == []


def test_mixed_language_text_multiple_entities() -> None:
    text = (
        "Le client (AVS 756.9217.0769.85) hat die Karte 4111 1111 1111 1111 benutzt, "
        "NIR 2 95 10 99 126 111 93, Steuer-ID 36 574 261 809."
    )
    spans = detect(text)
    assert len(spans) == 4
    assert {s.entity_type for s in spans} == {"CH_AVS", "CREDIT_CARD", "FR_NIR", "DE_STEUER_ID"}
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


def test_corsican_nir_detected() -> None:
    text = "NIR 1 99 07 2A 004 001 09 enregistré."
    (span,) = detect(text)
    assert span.entity_type == "FR_NIR"
    assert text[span.start : span.end] == "1 99 07 2A 004 001 09"


def test_card_window_inside_longer_digit_run_rejected() -> None:
    # A 20-digit contract number contains a 16-digit window that passes Luhn;
    # a card candidate must not be carved out of a longer separated digit run.
    text = "Vertragsnummer 4111 1111 1111 1111 1234"
    assert detect(text) == []


def test_luhn_passing_tail_of_valid_iban_yields_only_iban() -> None:
    # The 18-digit BBAN of this checksum-valid IBAN also passes Luhn; the run
    # continues left into the check digits, so no card window may be emitted.
    text = "Zahlung auf DE62 3704 0044 0532 0130 01 bitte."
    (span,) = detect(text)
    assert span.entity_type == "IBAN"
    assert text[span.start : span.end] == "DE62 3704 0044 0532 0130 01"


def test_overseas_nir_detected() -> None:
    # Overseas birth places group as department(3) + commune(2), not 2 + 3.
    text = "NIR 1 90 01 971 01 001 91 (Guadeloupe)."
    (span,) = detect(text)
    assert span.entity_type == "FR_NIR"
    assert text[span.start : span.end] == "1 90 01 971 01 001 91"


def test_embedded_tails_of_longer_runs_rejected() -> None:
    # A checksum-valid identifier embedded as the tail of a longer separated digit
    # run is a window of a bigger number, not an entity (same guard as for cards).
    assert detect("Vertrag 9 2 95 10 99 126 111 93") == []
    assert detect("Konto 9 756.9217.0769.85") == []
    assert detect("Nr 1 36 574 261 809") == []


def test_identifiers_followed_by_digit_run_rejected() -> None:
    # Continuation to the right means the match is a window of a longer number.
    assert detect("NIR 2 95 10 99 126 111 93 456") == []
    assert detect("AVS 756.9217.0769.85.123") == []
    assert detect("Steuer-ID 36 574 261 809 77") == []


def test_iban_followed_by_letter_token_still_detected() -> None:
    # The greedy pattern swallows a following uppercase token ("BIC", bank codes);
    # the candidate must shrink back to the valid IBAN instead of vanishing.
    text = "Compte BE68 5390 0754 7034 BIC GKCCBEBB svp."
    (span,) = detect(text)
    assert span.entity_type == "IBAN"
    assert text[span.start : span.end] == "BE68 5390 0754 7034"


def test_lowercase_iban_detected() -> None:
    text = "iban: be68 5390 0754 7034 merci"
    (span,) = detect(text)
    assert span.entity_type == "IBAN"
    assert text[span.start : span.end] == "be68 5390 0754 7034"


def test_iban_with_letter_groups_in_bban() -> None:
    text = "Send to GB29 NWBK 6016 1331 9268 19 today."
    (span,) = detect(text)
    assert span.entity_type == "IBAN"
    assert text[span.start : span.end] == "GB29 NWBK 6016 1331 9268 19"


def test_iban_followed_by_digit_group_not_shrunk() -> None:
    # A trailing digit group means the run may be a longer account/contract
    # number; shrinking only discards letter-bearing tails (digit-run guard
    # philosophy), so no span is emitted here.
    text = "Ref BE68 5390 0754 7034 1234"
    assert detect(text) == []


def test_lowercase_corsican_nir_detected() -> None:
    text = "NIR 1 99 07 2a 004 001 09 valide."
    (span,) = detect(text)
    assert span.entity_type == "FR_NIR"
    assert text[span.start : span.end] == "1 99 07 2a 004 001 09"
