"""Checksum validators for the identifier catalog (REQ-2).

Each validator answers one question: is this candidate a checksum-valid instance of
the identifier type? A failed checksum drops the candidate entirely — it never merely
lowers confidence. Validators must not raise on garbage input.
"""

from collections.abc import Callable

import schwifty
import schwifty.exceptions
from stdnum import luhn
from stdnum.ch import ssn as _ch_ssn
from stdnum.de import idnr as _de_idnr
from stdnum.fr import nir as _fr_nir

_SEPARATORS = str.maketrans("", "", " .-")


def _clean(value: str) -> str:
    return value.translate(_SEPARATORS)


def _credit_card(value: str) -> bool:
    digits = _clean(value)
    return 13 <= len(digits) <= 19 and digits.isdigit() and luhn.is_valid(digits)


def _iban(value: str) -> bool:
    # schwifty checks country, length, BBAN structure and the mod-97 checksum but not
    # bank-registry membership: a real IBAN of a bank missing from a registry snapshot
    # must still be detected (recall over registry freshness). Case-insensitive.
    try:
        schwifty.IBAN(_clean(value))
    except schwifty.exceptions.SchwiftyException:
        return False
    return True


VALIDATORS: dict[str, Callable[[str], bool]] = {
    "iban": _iban,
    "credit_card": _credit_card,
    "ch_avs": lambda v: _ch_ssn.is_valid(_clean(v)),
    "fr_nir": lambda v: _fr_nir.is_valid(_clean(v)),
    "de_idnr": lambda v: _de_idnr.is_valid(_clean(v)),
}
