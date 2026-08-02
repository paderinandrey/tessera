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


def test_default_config_declares_the_three_types() -> None:
    types = load_ner_types()
    assert [t.entity_type for t in types] == ["PERSON", "LOCATION", "ORG"]
    assert [t.label for t in types] == ["person", "location", "organization"]
    assert all(0.0 < t.threshold <= 1.0 for t in types)
    # Below every catalog identifier (40-90), so an identifier wins a partial overlap.
    assert all(t.specificity < 40 for t in types)
    assert all(t.tier == 2 for t in types)


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
