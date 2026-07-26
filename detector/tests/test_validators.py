import pytest

from tessera_detector.validators import VALIDATORS

# Synthetic, checksum-valid documentation/test values — not attributable to any person.
VALID = {
    "iban": ["DE89370400440532013000", "DE89 3704 0044 0532 0130 00"],
    "credit_card": ["4111111111111111", "4111 1111 1111 1111"],
    "ch_avs": ["7569217076985", "756.9217.0769.85"],
    "fr_nir": ["295109912611193", "2 95 10 99 126 111 93"],
    "de_idnr": ["36574261809", "36 574 261 809"],
}

# Same values with the checksum broken (last digit changed).
INVALID = {
    "iban": ["DE89370400440532013001"],
    "credit_card": ["4111111111111112"],
    "ch_avs": ["7569217076984", "756.9217.0769.84"],
    "fr_nir": ["295109912611194"],
    "de_idnr": ["36574261808"],
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
