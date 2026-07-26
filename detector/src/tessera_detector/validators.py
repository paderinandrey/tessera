"""Checksum validators for the identifier catalog (REQ-2).

Each validator answers one question: is this candidate a checksum-valid instance of
the identifier type? A failed checksum drops the candidate entirely — it never merely
lowers confidence. Validators must not raise on garbage input.
"""

from collections.abc import Callable

from stdnum import iban, luhn
from stdnum.ch import ssn as _ch_ssn
from stdnum.de import idnr as _de_idnr
from stdnum.fr import nir as _fr_nir

_SEPARATORS = str.maketrans("", "", " .-")


def _clean(value: str) -> str:
    return value.translate(_SEPARATORS)


def _credit_card(value: str) -> bool:
    digits = _clean(value)
    return 13 <= len(digits) <= 19 and digits.isdigit() and luhn.is_valid(digits)


VALIDATORS: dict[str, Callable[[str], bool]] = {
    "iban": lambda v: iban.is_valid(_clean(v)),
    "credit_card": _credit_card,
    "ch_avs": lambda v: _ch_ssn.is_valid(_clean(v)),
    "fr_nir": lambda v: _fr_nir.is_valid(_clean(v)),
    "de_idnr": lambda v: _de_idnr.is_valid(_clean(v)),
}
