import json
from pathlib import Path

import pytest

from tessera_detector.deterministic import DeterministicDetector
from tessera_detector.resolution import resolve
from tessera_detector.validators import CHECKSUM_VALIDATORS, VALIDATORS


def _specificity() -> dict[str, int]:
    """The catalog's own ordering, read rather than restated — a copy here would
    make the test pass while the file it is about said something else."""
    rules = DeterministicDetector(None).rules
    return {rule.entity_type: rule.specificity for rule in rules}

CORPUS_PATH = Path(__file__).resolve().parents[2] / "evaluation" / "corpus" / "public.jsonl"

# Synthetic, checksum-valid documentation/test values — not attributable to any person.
VALID = {
    "iban": [
        "DE89370400440532013000",
        "DE89 3704 0044 0532 0130 00",
        # Structural validation, not bank-registry membership: the canonical
        # Belgian example uses a fictional bank and must still validate.
        "BE68 5390 0754 7034",
        # Case must not matter.
        "be68 5390 0754 7034",
        # Letters in the BBAN body.
        "GB29 NWBK 6016 1331 9268 19",
    ],
    "credit_card": ["4111111111111111", "4111 1111 1111 1111"],
    "ch_avs": ["7569217076985", "756.9217.0769.85"],
    "fr_nir": [
        "295109912611193",
        "2 95 10 99 126 111 93",
        # Corsican department codes 2A/2B are part of the NIR grammar.
        "199072A00400109",
        "1 99 07 2A 004 001 09",
        "185082B12345611",
        # Provisional identifiers use sex/status codes 3, 4, 7, 8.
        "395109912611143",
        "7 95 10 99 126 111 37",
        # Lowercase Corsican codes identify the same departments.
        "1 99 07 2a 004 001 09",
        # Overseas departments group as three-digit department + two-digit commune.
        "190019710100191",
        "1 90 01 971 01 001 91",
    ],
    "de_idnr": ["36574261809", "36 574 261 809"],
    "fr_nif": ["0701987765493", "07 01 987 765 493"],
}

# Same values with the checksum broken (last digit changed).
INVALID = {
    "iban": ["DE89370400440532013001", "BE69 5390 0754 7034"],
    "credit_card": ["4111111111111112"],
    "ch_avs": ["7569217076984", "756.9217.0769.84"],
    "fr_nir": ["295109912611194", "199072A00400110"],
    "de_idnr": ["36574261808"],
    "fr_nif": ["0701987765432"],
}


def test_all_expected_validators_registered() -> None:
    assert set(VALID) <= set(VALIDATORS)


@pytest.mark.parametrize(
    ("name", "value"),
    [(name, value) for name, values in VALID.items() for value in values],
)
def test_valid_values_pass(name: str, value: str) -> None:
    assert VALIDATORS[name](value) is True


@pytest.mark.parametrize(
    ("name", "value"),
    [(name, value) for name, values in INVALID.items() for value in values],
)
def test_checksum_broken_values_fail(name: str, value: str) -> None:
    assert VALIDATORS[name](value) is False


def test_garbage_never_raises() -> None:
    for validate in VALIDATORS.values():
        assert validate("") is False
        assert validate("not-a-number") is False
        assert validate("1234") is False


def _digit_changes(value: str) -> list[str]:
    """Every value one digit away from this one."""
    out = []
    for position, character in enumerate(value):
        if not character.isdigit():
            continue
        for digit in "0123456789":
            if digit != character:
                out.append(value[:position] + digit + value[position + 1 :])
    return out


def test_a_checksum_validator_rejects_every_single_digit_change() -> None:
    """**The claim `CHECKSUM_VALIDATORS` makes, checked.**

    That set is not a label. A rule backed by a member of it must keep
    confidence 1.0, and `resolution._is_checksum` turns that into *untouchable*
    — a span no model guess can displace and which wins its overlaps outright.
    Membership hands out an exemption from the resolver's normal ordering.

    Nothing verified that a member earns it. `VALIDATORS` is a plain dict and
    `CHECKSUM_VALIDATORS` is *everything in it except one name*, so a
    structural-only validator added tomorrow joins the checksum set by default
    — the failure direction is the silent one, and it grants untouchable status
    to a pattern match.

    So the property, rather than the label: **change one digit and a checksum
    validator says no.** Measured over every corpus value each one accepts:
    3510, 1287, 468, 810, 234 and 594 mutations, **none accepted**.

    Not a test of `stdnum` and `schwifty`. It is a test that the right function
    is bound to the right name and that the name means what the set says.
    """
    corpus = json.loads(
        "[" + ",".join(CORPUS_PATH.read_text().splitlines()) + "]"
    )
    values = {
        row["text"][entity["start"] : entity["end"]]
        for row in corpus
        for entity in row["entities"]
    }

    for name in sorted(CHECKSUM_VALIDATORS):
        validator = VALIDATORS[name]
        accepted = sorted(v for v in values if validator(v))
        # Without this a validator that accepts nothing passes for having been
        # asked nothing, which is the shape of every gate this repository has
        # had to fix twice.
        assert accepted, f"{name} accepts no value in the corpus, so it was not tested"

        for value in accepted:
            survived = [near for near in _digit_changes(value) if validator(near)]
            assert not survived, (
                f"{name} accepted {survived[:3]} after a one-digit change to {value!r}, "
                "so it is not verifying a checksum and must not be in CHECKSUM_VALIDATORS"
            )


def test_the_structural_validator_is_excluded_because_it_cannot_do_that() -> None:
    """The other side of the boundary, which is what makes the set a decision.

    `de_stnr` accepts **every** one-digit change — 792 of 792 over the corpus —
    because the German Steuernummer has no checksum to fail. That is precisely
    why it is subtracted from `CHECKSUM_VALIDATORS` and why its catalog rules
    declare a sub-1.0 confidence gated by context triggers.

    Asserting it here means the exclusion is a measured fact rather than a
    comment, and adding `de_stnr` to the set fails the test above rather than
    quietly promoting a pattern match to untouchable.
    """
    assert "de_stnr" not in CHECKSUM_VALIDATORS
    corpus = json.loads(
        "[" + ",".join(CORPUS_PATH.read_text().splitlines()) + "]"
    )
    values = {
        row["text"][entity["start"] : entity["end"]]
        for row in corpus
        for entity in row["entities"]
    }
    validator = VALIDATORS["de_stnr"]
    accepted = sorted(v for v in values if validator(v))
    assert accepted

    survivors = sum(
        1 for value in accepted for near in _digit_changes(value) if validator(near)
    )
    total = sum(len(_digit_changes(value)) for value in accepted)
    assert survivors == total, (
        f"de_stnr rejected {total - survivors} of {total} one-digit changes; if it has "
        "grown a checksum it belongs in CHECKSUM_VALIDATORS"
    )


def test_two_untouchable_rules_can_claim_one_range_and_specificity_decides() -> None:
    """**What untouchable actually rests on, which is not the checksum alone.**

    Found while measuring the property above: a French NIR is a Luhn-valid
    digit run of card length, so `catalog:credit_card` and `catalog:fr_nir`
    **both** match `1 71 07 10 830 660 47`, on the identical range, both at
    confidence 1.0, and both untouchable. Two exemptions from the resolver's
    ordering, claiming one span.

    What separates them is `specificity` — 80 for FR_NIR against 40 for
    CREDIT_CARD — and nothing said so. Nothing leaks either way, because both
    mask the same characters. What changes is the **type**: the model sees
    `[CREDIT_CARD_1]` where the truth is a social security number, and the
    audit journal records a payment card. For a product whose evidence layer
    is the thing a regulator reads, that is the wrong answer with the right
    coverage.

    So the ordering is pinned where it is load-bearing rather than left to be
    the reason a corpus check happens to pass.
    """
    detector = DeterministicDetector(None)
    text = "Le client Marty (NIR 1 71 07 10 830 660 47) a demandé la clôture de son dossier."

    claimants = {
        (span.entity_type, span.recognizer, span.confidence)
        for span in detector.detect(text)
        if span.start == 21 and span.end == 42
    }
    assert claimants == {
        ("CREDIT_CARD", "catalog:credit_card", 1.0),
        ("FR_NIR", "catalog:fr_nir", 1.0),
    }, f"the overlap this test is about no longer happens: {claimants}"

    # And the one that survives is the specific one. Lower FR_NIR's specificity
    # below CREDIT_CARD's in identifiers.yaml and this flips.
    resolved = [s for s in resolve(detector.detect(text), specificity=_specificity()).spans
                if s.start == 21 and s.end == 42]
    assert [s.entity_type for s in resolved] == ["FR_NIR"]
