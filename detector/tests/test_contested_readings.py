import pytest

from tessera_detector.ner import contested, load_ner_types
from tessera_detector.resolution import resolve
from tessera_detector.spans import Span

TYPES = load_ner_types()
SPECIFICITY = {t.entity_type: t.specificity for t in TYPES}


def _span(entity_type: str, start: int, end: int, confidence: float, tier: int) -> Span:
    return Span(
        entity_type=entity_type,
        start=start,
        end=end,
        confidence=confidence,
        recognizer="ner:gliner",
        tier=tier,
    )


def test_person_declares_the_bar_tier_3_must_clear_against_it() -> None:
    by_type = {t.entity_type: t for t in TYPES}
    assert by_type["PERSON"].outranks_tier_3_below == 0.30
    assert all(
        t.outranks_tier_3_below is None for t in TYPES if t.entity_type != "PERSON"
    )


def test_a_weak_special_category_reading_of_a_surname_is_reported_as_person() -> None:
    person = _span("PERSON", 0, 7, 0.971, 2)
    ethnicity = _span("ETHNICITY", 0, 7, 0.263, 3)

    assert contested([person, ethnicity], TYPES) == [person]


def test_a_special_category_reading_at_the_bar_keeps_its_type() -> None:
    person = _span("PERSON", 0, 10, 0.663, 2)
    orientation = _span("SEXUAL_ORIENTATION", 0, 10, 0.30, 3)

    assert contested([person, orientation], TYPES) == [person, orientation]


def test_an_uncontested_weak_special_category_reading_keeps_its_type() -> None:
    person = _span("PERSON", 0, 7, 0.971, 2)
    union = _span("TRADE_UNION", 20, 30, 0.232, 3)

    assert contested([person, union], TYPES) == [person, union]


@pytest.mark.parametrize("rival", ["ORG", "LOCATION"])
def test_only_a_type_declaring_the_bar_contests_a_reading(rival: str) -> None:
    other = _span(rival, 0, 10, 0.9, 2)
    union = _span("TRADE_UNION", 0, 10, 0.232, 3)

    assert contested([other, union], TYPES) == [other, union]


def test_the_relabelled_reading_keeps_its_own_extent() -> None:
    text = "Der Kunde Dumas ruft an."
    person = _span("PERSON", 10, 15, 0.776, 2)
    union = _span("TRADE_UNION", 0, 15, 0.258, 3)

    resolved = resolve(contested([person, union], TYPES), specificity=SPECIFICITY).spans

    assert [(s.entity_type, text[s.start : s.end]) for s in resolved] == [
        ("PERSON", "Der Kunde Dumas")
    ]


def test_the_surest_overlapping_person_names_the_reading() -> None:
    weaker = _span("PERSON", 0, 5, 0.6, 2)
    surer = _span("PERSON", 6, 12, 0.9, 2)
    union = _span("TRADE_UNION", 0, 12, 0.25, 3)

    relabelled = contested([weaker, surer, union], TYPES)[-1]

    assert (relabelled.entity_type, relabelled.confidence, relabelled.tier) == (
        "PERSON",
        0.9,
        2,
    )


@pytest.mark.parametrize("bad", ["0", "1.5", "true", "'high'"])
def test_the_bar_must_be_a_score(bad: str) -> None:
    config = f"""
entities:
  - entity_type: PERSON
    label: person
    threshold: 0.5
    tier: 2
    specificity: 30
    outranks_tier_3_below: {bad}
"""
    with pytest.raises(ValueError, match="outranks_tier_3_below"):
        load_ner_types(config)


def test_a_tier_3_type_cannot_declare_the_bar() -> None:
    config = """
entities:
  - entity_type: HEALTH
    label: medical condition
    threshold: 0.2
    tier: 3
    specificity: 35
    outranks_tier_3_below: 0.3
"""
    with pytest.raises(ValueError, match="cannot outrank tier 3"):
        load_ner_types(config)


def test_a_weak_tier_2_reading_is_not_contested() -> None:
    person = _span("PERSON", 0, 7, 0.971, 2)
    location = _span("LOCATION", 0, 7, 0.25, 2)

    assert contested([person, location], TYPES) == [person, location]
