from tessera_detector.resolution import resolve
from tessera_detector.spans import Span

SPEC = {"IBAN": 90, "FR_NIR": 80, "CREDIT_CARD": 40, "ORG": 20}


def span(
    entity_type: str = "IBAN",
    start: int = 0,
    end: int = 10,
    confidence: float = 1.0,
    recognizer: str = "catalog:x",
    tier: int = 1,
    boosted: bool = False,
) -> Span:
    return Span(
        entity_type=entity_type,
        start=start,
        end=end,
        confidence=confidence,
        recognizer=recognizer,
        tier=tier,
        boosted=boosted,
    )


def test_non_overlapping_pass_through_sorted() -> None:
    a, b = span(start=20, end=30), span(start=0, end=10)
    result = resolve([a, b], specificity=SPEC)
    assert result.spans == [b, a]
    assert result.trace == []


def test_identical_duplicates_deduped() -> None:
    a = span(confidence=0.9, recognizer="ner:gliner", tier=2, entity_type="ORG")
    b = span(confidence=0.7, recognizer="ner:other", tier=2, entity_type="ORG")
    result = resolve([a, b], specificity=SPEC)
    (kept,) = result.spans
    assert kept.confidence == 0.9


def test_same_type_overlap_merges_to_union() -> None:
    a = span(start=0, end=10, confidence=0.8, recognizer="ner:a", tier=2, entity_type="ORG")
    b = span(start=5, end=15, confidence=0.9, recognizer="ner:b", tier=2, entity_type="ORG")
    result = resolve([a, b], specificity=SPEC)
    (kept,) = result.spans
    assert (kept.start, kept.end) == (0, 15)
    assert kept.confidence == 0.9
    assert kept.entity_type == "ORG"


def test_nested_different_types_outer_wins() -> None:
    outer = span(entity_type="ORG", start=0, end=20, confidence=0.6, recognizer="ner:g", tier=3)
    inner = span(entity_type="ORG2", start=5, end=10, confidence=0.9, recognizer="ner:g", tier=3)
    result = resolve([outer, inner], specificity={"ORG": 20, "ORG2": 20})
    (kept,) = result.spans
    assert (kept.start, kept.end) == (0, 20)
    assert kept.entity_type == "ORG"


def test_untouchable_inner_survives_inside_non_checksum_outer() -> None:
    # Rule 1 beats rule 3: a checksum identifier nested in an NER span keeps its
    # Tier 1 identity; bounds take the union so the whole region is masked.
    outer = span(entity_type="ORG", start=0, end=30, confidence=0.7, recognizer="ner:g", tier=3)
    inner = span(entity_type="IBAN", start=5, end=27, confidence=1.0, recognizer="catalog:iban")
    result = resolve([outer, inner], specificity=SPEC)
    (kept,) = result.spans
    assert (kept.start, kept.end) == (0, 30)
    assert kept.entity_type == "IBAN"
    assert kept.tier == 1


def test_partial_overlap_higher_specificity_wins() -> None:
    nir = span(entity_type="FR_NIR", start=0, end=15, recognizer="catalog:fr_nir")
    card = span(entity_type="CREDIT_CARD", start=0, end=15, recognizer="catalog:credit_card")
    result = resolve([card, nir], specificity=SPEC)
    (kept,) = result.spans
    assert kept.entity_type == "FR_NIR"


def test_specificity_tie_higher_confidence_wins() -> None:
    a = span(entity_type="A", start=0, end=10, confidence=0.9, recognizer="ner:x", tier=2)
    b = span(entity_type="B", start=5, end=15, confidence=0.6, recognizer="ner:x", tier=2)
    result = resolve([a, b], specificity={"A": 50, "B": 50})
    (kept,) = result.spans
    assert kept.entity_type == "A"


def test_full_tie_merges_with_more_sensitive_type() -> None:
    a = span(entity_type="A", start=0, end=10, confidence=0.8, recognizer="ner:x", tier=2)
    b = span(entity_type="B", start=5, end=15, confidence=0.8, recognizer="ner:x", tier=1)
    result = resolve([a, b], specificity={"A": 50, "B": 50})
    (kept,) = result.spans
    assert (kept.start, kept.end) == (0, 15)
    assert kept.entity_type == "B"  # tier 1 is more sensitive
    assert kept.tier == 1


def test_trace_records_applied_rules() -> None:
    outer = span(entity_type="ORG", start=0, end=20, confidence=0.6, recognizer="ner:g", tier=3)
    inner = span(entity_type="ORG2", start=5, end=10, confidence=0.9, recognizer="ner:g", tier=3)
    result = resolve([outer, inner], specificity={"ORG": 20, "ORG2": 20})
    (decision,) = result.trace
    assert decision.rule == "nesting-outer-wins"
    assert decision.kept.entity_type == "ORG"
    assert [d.entity_type for d in decision.dropped] == ["ORG2"]


def test_chain_of_overlaps_reaches_fixpoint() -> None:
    # a overlaps b, the a+b union then overlaps c: resolution must converge.
    a = span(entity_type="ORG", start=0, end=10, confidence=0.8, recognizer="ner:x", tier=3)
    b = span(entity_type="ORG", start=8, end=18, confidence=0.7, recognizer="ner:x", tier=3)
    c = span(entity_type="ORG", start=17, end=25, confidence=0.9, recognizer="ner:x", tier=3)
    result = resolve([a, b, c], specificity=SPEC)
    (kept,) = result.spans
    assert (kept.start, kept.end) == (0, 25)


def test_untouchable_beats_higher_specificity_on_partial_overlap() -> None:
    # Rule 1 precedes rule 4: a lone checksum span never loses to a non-checksum
    # span, even when the latter's type ranks higher in specificity.
    nir = span(entity_type="FR_NIR", start=0, end=15, recognizer="catalog:fr_nir")
    model = span(
        entity_type="VERY_SPECIFIC", start=0, end=15, confidence=0.99, recognizer="ner:x", tier=2
    )
    result = resolve([model, nir], specificity={"VERY_SPECIFIC": 95, "FR_NIR": 80})
    (kept,) = result.spans
    assert kept.entity_type == "FR_NIR"


def test_resolution_is_order_independent() -> None:
    # Equal-range, equally scored spans must resolve identically regardless of
    # input order (set() iteration order is hash-seed dependent).
    a = span(entity_type="AAA", start=0, end=10, confidence=0.8, recognizer="ner:x", tier=2)
    b = span(entity_type="BBB", start=0, end=10, confidence=0.8, recognizer="ner:y", tier=2)
    kept_ab = resolve([a, b], specificity={"AAA": 50, "BBB": 50}).spans[0]
    kept_ba = resolve([b, a], specificity={"AAA": 50, "BBB": 50}).spans[0]
    assert kept_ab == kept_ba


def test_same_type_merge_prefers_untouchable_metadata() -> None:
    # A same-type merge must keep the checksum span's audit identity even when a
    # confidence-1.0 model span sorts first and ties on confidence.
    model = span(entity_type="IBAN", start=0, end=30, confidence=1.0, recognizer="ner:g", tier=1)
    checksum = span(entity_type="IBAN", start=5, end=27, recognizer="catalog:iban")
    result = resolve([model, checksum], specificity=SPEC)
    (kept,) = result.spans
    assert (kept.start, kept.end) == (0, 30)
    assert kept.recognizer == "catalog:iban"
