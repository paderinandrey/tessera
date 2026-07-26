import pytest

from tessera_detector.validators import VALIDATORS

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
