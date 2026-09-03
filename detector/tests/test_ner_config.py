import pytest

from tessera_detector.ner import load_ner_types

TEMPLATE = """
entities:
  - entity_type: PERSON
    label: person
    threshold: {threshold}
    tier: {tier}
    specificity: 30
"""


def test_default_config_declares_the_quasi_identifiers() -> None:
    quasi = [t for t in load_ner_types() if t.tier == 2]
    assert [t.entity_type for t in quasi] == ["PERSON", "LOCATION", "ORG"]
    assert [t.label for t in quasi] == ["person", "location", "organization"]
    assert all(0.0 < t.threshold <= 1.0 for t in quasi)
    # Below every catalog identifier (40-90), so an identifier wins a partial overlap.
    assert all(t.specificity < 40 for t in quasi)


def test_every_configured_type_stays_below_the_identifier_catalog() -> None:
    assert all(t.specificity < 40 for t in load_ner_types())


def test_threshold_is_required() -> None:
    config = (
        "entities:\n  - entity_type: PERSON\n    label: person\n"
        "    tier: 2\n    specificity: 30\n"
    )
    with pytest.raises(ValueError, match="threshold"):
        load_ner_types(config)


@pytest.mark.parametrize("bad", ["-0.1", "1.5", "true", "'high'"])
def test_threshold_must_be_a_number_in_range(bad: str) -> None:
    with pytest.raises(ValueError, match="threshold"):
        load_ner_types(TEMPLATE.format(threshold=bad, tier=2))


@pytest.mark.parametrize("bad", ["0", "4"])
def test_tier_must_be_between_one_and_three(bad: str) -> None:
    with pytest.raises(ValueError, match="tier"):
        load_ner_types(TEMPLATE.format(threshold=0.7, tier=bad))


def test_specificity_is_required() -> None:
    config = (
        "entities:\n  - entity_type: PERSON\n    label: person\n"
        "    threshold: 0.7\n    tier: 2\n"
    )
    with pytest.raises(ValueError, match="specificity"):
        load_ner_types(config)


@pytest.mark.parametrize("bad", ["-1", "true", "'high'", "1.5"])
def test_specificity_must_be_a_non_negative_integer(bad: str) -> None:
    config = f"""
entities:
  - entity_type: PERSON
    label: person
    threshold: 0.7
    tier: 2
    specificity: {bad}
"""
    with pytest.raises(ValueError, match="specificity"):
        load_ner_types(config)


def test_duplicate_entity_types_are_rejected() -> None:
    config = TEMPLATE.format(threshold=0.7, tier=2) * 1 + """
  - entity_type: PERSON
    label: personne
    threshold: 0.7
    tier: 2
    specificity: 30
"""
    with pytest.raises(ValueError, match="PERSON"):
        load_ner_types(config)


def test_duplicate_labels_are_rejected() -> None:
    # Two types sharing a zero-shot label collapse in the recognizer's
    # label->type map, making the earlier type undetectable.
    config = """
entities:
  - entity_type: PERSON
    label: person
    threshold: 0.7
    tier: 2
    specificity: 30
  - entity_type: ORG
    label: person
    threshold: 0.7
    tier: 2
    specificity: 10
"""
    with pytest.raises(ValueError, match="person"):
        load_ner_types(config)


# Labels are the model's interface, and the phrasing decides whether it fires:
# "trade union" scores 0.95 on "IG Metall" where "trade union membership" scores
# 0.70, and "political affiliation" mislabels a union as a party.
ARTICLE_9_TYPES = {
    "HEALTH": "medical condition",
    "BIOMETRIC": "biometric data",
    "GENETIC": "genetic data",
    "ETHNICITY": "ethnic origin",
    "POLITICAL_AFFILIATION": "political party",
    "POLITICAL_OPINION": "political opinion",
    "RELIGION": "religion",
    "TRADE_UNION": "trade union",
    "SEXUAL_ORIENTATION": "sexual orientation",
    "PHILOSOPHICAL_BELIEF": "philosophical belief",
    "SEX_LIFE": "sex life",
}


def test_article_9_categories_are_configured() -> None:
    by_type = {t.entity_type: t for t in load_ner_types()}
    for entity_type, label in ARTICLE_9_TYPES.items():
        assert entity_type in by_type, f"{entity_type} missing from ner.yaml"
        assert by_type[entity_type].label == label


def test_article_9_uses_the_aggressive_threshold() -> None:
    # REQ-3's acceptance criterion: misses are not tolerable here, so the
    # threshold is far below the quasi-identifiers'.
    by_type = {t.entity_type: t for t in load_ner_types()}
    for entity_type in ARTICLE_9_TYPES:
        assert by_type[entity_type].threshold == 0.30


def test_article_9_sits_in_its_own_tier() -> None:
    by_type = {t.entity_type: t for t in load_ner_types()}
    assert {by_type[t].tier for t in ARTICLE_9_TYPES} == {3}
    assert by_type["PERSON"].tier == 2


def test_article_9_outranks_quasi_identifiers_but_not_identifiers() -> None:
    by_type = {t.entity_type: t for t in load_ner_types()}
    article_9 = {by_type[t].specificity for t in ARTICLE_9_TYPES}
    assert article_9 == {35}
    assert max(by_type[t].specificity for t in ("PERSON", "LOCATION", "ORG")) < 35
    # 40 is the lowest specificity in the identifier catalog.
    assert 35 < 40


def test_a_multi_word_trim_entry_is_refused() -> None:
    # The rule walks a span one token at a time, so `Der Kunde` written as one
    # entry never matches — and sits in the catalog looking as though it does.
    # A list entry that cannot fire is worse than an absent one: it reads as
    # coverage.
    config = """
entities:
  - entity_type: PERSON
    label: person
    threshold: 0.7
    tier: 2
    specificity: 30
    trim_leading:
      - Der Kunde
"""

    with pytest.raises(ValueError, match="more than one word"):
        load_ner_types(config)


def test_a_word_listed_as_both_a_trim_word_and_an_article_is_refused() -> None:
    # An entry in both lists is subtracted out of the article safeguard and
    # trimmed unconditionally — `le` in both makes `Le Thi Mai` lose its family
    # name, which is the disclosure the split exists to prevent. A catalog
    # mistake that disables a safety rule stops the service rather than the
    # name.
    config = """
entities:
  - entity_type: PERSON
    label: person
    threshold: 0.7
    tier: 2
    specificity: 30
    trim_leading:
      - le
    trim_leading_articles:
      - Le
"""

    with pytest.raises(ValueError, match="both a trim_leading word"):
        load_ner_types(config)
