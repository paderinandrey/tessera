"""#97: an apostrophe surname that `person` scores well above its bar, asked
alone, and that a tier-2 competitor took the argmax for and then failed its own
bar — so nothing was emitted and the whole surname went to the provider.

The corpus rows are quoted rather than invented, because the mechanism is
context-dependent: the same surname is found in one sentence and missed in
another, and a frame written here would be testing a sentence nobody sends.
"""

import pytest

from tessera_detector.models import find_model
from tessera_detector.pipeline import build_detector

pytestmark = pytest.mark.ner


@pytest.fixture(scope="module")
def detector():
    if find_model() is None:
        pytest.skip("no NER weights: run `make model` or set TESSERA_NER_MODEL")
    try:
        built = build_detector(ner=True)
    except ImportError:
        pytest.skip("gliner not installed: run `uv sync --group ner`")
    return built


# `person` asked alone clears its 0.5 bar on every one of these, by the margin
# in the comment. Each was emitted as nothing at all before the split.
RECOVERED = [
    # organization took it at 0.603 and could not clear ORG's 0.75.
    (
        "Die Rechnung pour D'Angelo référence la Steuer-ID 34 941 704 688 "
        "et l'IBAN DE67 3704 0044 0018 9195 77.",
        "D'Angelo",
    ),
    # person itself scored 0.303 in the group and 0.812 alone.
    ("Sehr geehrter Herr dell\u2019Orto, Ihre Steuer-ID 69 298 351 490 wurde erfasst.",
     "dell\u2019Orto"),
    # person 0.455 in the group, 0.799 alone — under the bar by 0.045.
    ("Le salarié O\u2019Brien est adhérent de la CFDT et conteste la sanction.",
     "O\u2019Brien"),
    # location took it at 0.652 and could not clear LOCATION's 0.7.
    ("Der Antrag von O'Brien nennt die Angabe homosexuell.", "O'Brien"),
]

# Low asked alone too, so the pass split cannot reach them and the gate still
# tracks them. Listed so a future change that does recover them is noticed here
# rather than only in the corpus figures.
STILL_MISSED = [
    ("Steuernummer 419/130/29933 des Mandanten dell\u2019Orto liegt der "
     "Börner AG & Co. KGaA vor.", "dell\u2019Orto"),
    ("Le dossier médical de L\u2019Hôpital mentionne une interruption de grossesse en 2023.",
     "L\u2019Hôpital"),
]


def _covering(detector, text: str, value: str) -> list:
    start = text.index(value)
    end = start + len(value)
    return [s for s in detector.detect(text) if s.start <= start and s.end >= end]


@pytest.mark.parametrize(("text", "value"), RECOVERED, ids=[v for _, v in RECOVERED])
def test_an_apostrophe_surname_is_masked(detector, text: str, value: str) -> None:
    covering = _covering(detector, text, value)
    assert covering, f"{value!r} reaches the provider in full"


@pytest.mark.parametrize(("text", "value"), RECOVERED, ids=[v for _, v in RECOVERED])
def test_an_apostrophe_surname_is_masked_as_a_person(detector, text: str, value: str) -> None:
    # Separate from the test above because they fail for different reasons: the
    # first is egress, this one is the label. A competitor clearing its own bar
    # would satisfy the first and still be wrong.
    covering = _covering(detector, text, value)
    assert [s for s in covering if s.entity_type == "PERSON"], (
        f"{value!r} is covered but not as a PERSON: "
        f"{[(s.entity_type, round(s.confidence, 3)) for s in covering]}"
    )


@pytest.mark.parametrize(("text", "value"), STILL_MISSED, ids=[v for _, v in STILL_MISSED])
def test_the_two_low_scoring_surnames_are_still_missed(
    detector, text: str, value: str
) -> None:
    # Asserting the gap rather than hiding it: these are in `KNOWN_UNMASKED`,
    # and if one starts being found, the entry there is stale and this says so.
    assert not _covering(detector, text, value), (
        f"{value!r} is now found — remove its KNOWN_UNMASKED entry and re-record"
    )
