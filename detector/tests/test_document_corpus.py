"""What reaches the provider when a JSON document's leaves are read together (#105)."""

import json
import re
from pathlib import Path
from typing import Any

import pytest

from tessera_detector.evaluation import (
    JOINED_LEAF_SEPARATOR,
    inside_one_leaf,
    leaf_ranges,
    unmasked_words,
)
from tessera_detector.pipeline import Detector, build_detector
from tessera_detector.spans import Span

pytestmark = pytest.mark.ner

# RFC 8259 section 6, in ASCII: `json.loads` also accepts `NaN` and `Infinity`,
# and `\d` matches non-ASCII digits, neither of which a JSON number can hold.
JSON_NUMBER = re.compile(r"-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?")

CORPUS = Path(__file__).resolve().parents[2] / "evaluation" / "corpus" / "documents.jsonl"
JOIN = JOINED_LEAF_SEPARATOR


@pytest.fixture(scope="module")
def detector() -> Detector:
    built = build_detector()
    if not built.ner_available:
        pytest.skip(f"NER is not provisioned ({built.ner_off_reason})")
    return built


def _documents() -> list[dict[str, Any]]:
    return [
        json.loads(line)
        for line in CORPUS.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]


def _truth_with_origin(document: dict[str, Any]) -> list[tuple[str, Span]]:
    out: list[tuple[str, Span]] = []
    at = 0
    for index, leaf in enumerate(document["leaves"]):
        for entity in leaf["entities"]:
            out.append(
                (
                    f"{document['id']}:{index}:{entity['start']}",
                    Span(
                        entity_type=entity["entity_type"],
                        start=entity["start"] + at,
                        end=entity["end"] + at,
                        confidence=1.0,
                        recognizer="corpus",
                        tier=1,
                    ),
                )
            )
        at += len(leaf["text"]) + len(JOIN)
    return out


def _rebased(
    detector: Detector, document: dict[str, Any]
) -> tuple[str, list[tuple[str, Span]], list[Span], list[Span]]:
    leaves = [leaf["text"] for leaf in document["leaves"]]
    text = JOIN.join(leaves)
    separate: list[Span] = []
    at = 0
    for leaf in leaves:
        separate += [
            span.model_copy(update={"start": span.start + at, "end": span.end + at})
            for span in detector.detect(leaf)
        ]
        at += len(leaf) + len(JOIN)
    return text, _truth_with_origin(document), separate, detector.detect(text)


def _reaching_the_provider(detector: Detector) -> tuple[
    frozenset[tuple[str, str, str]], frozenset[tuple[str, str, str]], int
]:
    joined_out: set[tuple[str, str, str]] = set()
    separate_out: set[tuple[str, str, str]] = set()
    crossing: list[tuple[str, str, str]] = []
    annotated = 0
    for document in _documents():
        text, truth, separate, joined = _rebased(detector, document)
        ranges = leaf_ranges([leaf["text"] for leaf in document["leaves"]])
        crossing += [
            (document["id"], span.entity_type, text[span.start : span.end])
            for span in joined
            if not inside_one_leaf(span, ranges)
        ]
        annotated += len(truth)
        for origin, span in truth:
            member = (origin, span.entity_type, text[span.start : span.end])
            if unmasked_words(text, span.start, span.end, joined):
                joined_out.add(member)
            if unmasked_words(text, span.start, span.end, separate):
                separate_out.add(member)
    assert not crossing, (
        "joined spans cross a leaf boundary, so production refuses these "
        f"documents outright and the inventories do not describe them: {crossing}"
    )
    return frozenset(joined_out), frozenset(separate_out), annotated


UNMASKED_JOINED = frozenset(
    {
        ('doc-0004:1:0', 'SEX_LIFE', 'une interruption de grossesse'),
        ('doc-0010:0:0', 'TRADE_UNION', 'ver.di'),
        ('doc-0012:17:19', 'PERSON', 'Förster'),
        ('doc-0012:5:0', 'BIOMETRIC', 'Fingerabdruck'),
        ('doc-0013:2:37', 'BIOMETRIC', 'reconnaissance faciale'),
        ('doc-0013:6:29', 'GENETIC', 'séquençage ADN'),
        ('doc-0013:7:68', 'HEALTH', 'eine Hepatitis-B-Infektion'),
        ('doc-0014:14:0', 'RELIGION', 'jüdisch'),
        ('doc-0014:20:0', 'TRADE_UNION', 'ver.di'),
        ('doc-0014:25:0', 'ORG', 'Marie et Fils'),
        ('doc-0014:3:0', 'PHILOSOPHICAL_BELIEF', 'agnostisch'),
        ('doc-0015:14:0', 'SEX_LIFE', 'un suivi en PMA'),
        ('doc-0015:3:0', 'BIOMETRIC', 'empreinte digitale'),
        ('doc-0017:0:0', 'POLITICAL_AFFILIATION', 'écologiste'),
        ('doc-0020:4:0', 'SEX_LIFE', 'eine Kinderwunschbehandlung'),
        ('doc-0021:0:15', 'PERSON', 'Hövel'),
        ('doc-0032:18:0', 'PHILOSOPHICAL_BELIEF', 'agnostique'),
        ('doc-0032:19:0', 'ETHNICITY', 'métisse'),
        ('doc-0032:5:0', 'POLITICAL_AFFILIATION', 'écologiste'),
        ('doc-0032:7:42', 'PHILOSOPHICAL_BELIEF', 'atheistisch'),
        ('doc-0033:2:0', 'ETHNICITY', 'métisse'),
        ('doc-0034:3:14', 'PERSON', 'L\u2019H\u00f4pital'),
        ('doc-0034:3:39', 'PHILOSOPHICAL_BELIEF', 'agnostique'),
        ('doc-0034:4:0', 'LOCATION', 'Kelheim'),
        ('doc-0035:12:0', 'ETHNICITY', 'noir de peau'),
        ('doc-0036:1:0', 'RELIGION', 'jüdisch'),
        ('doc-0038:0:0', 'SEX_LIFE', 'un suivi en PMA'),
        ('doc-0044:2:0', 'BIOMETRIC', 'Gesichtsscan'),
        ('doc-0045:1:21', 'PERSON', 'Wende'),
        ('doc-0046:4:0', 'POLITICAL_OPINION', 'eurokritisch'),
        ('doc-0048:0:0', 'TRADE_UNION', 'ver.di'),
        ('doc-0050:0:34', 'PERSON', 'dell\u2019Orto'),
        ('doc-0052:10:46', 'ETHNICITY', 'kurdischer Herkunft'),
        ('doc-0052:14:62', 'HEALTH', 'un diabète de type 2'),
        ('doc-0053:5:25', 'BIOMETRIC', 'Gesichtsscan'),
        ('doc-0053:5:42', 'PERSON', 'Zimmer'),
        ('doc-0054:12:16', 'PERSON', 'Jungfer'),
        ('doc-0054:22:0', 'LOCATION', 'Lebrun-sur-Étienne'),
        ('doc-0054:27:0', 'PHILOSOPHICAL_BELIEF', 'athée'),
        ('doc-0054:6:37', 'BIOMETRIC', 'reconnaissance faciale'),
        ('doc-0054:6:65', 'PERSON', 'Dijoux'),
        ('doc-0055:11:38', 'HEALTH', 'un diabète de type 2'),
        ('doc-0055:13:63', 'HEALTH', 'une sclérose en plaques'),
        ('doc-0055:6:0', 'LOCATION', 'Poirier'),
    }
)

UNMASKED_SEPARATE = frozenset(
    {
        ('doc-0004:1:0', 'SEX_LIFE', 'une interruption de grossesse'),
        ('doc-0010:0:0', 'TRADE_UNION', 'ver.di'),
        ('doc-0012:5:0', 'BIOMETRIC', 'Fingerabdruck'),
        ('doc-0013:6:29', 'GENETIC', 'séquençage ADN'),
        ('doc-0013:7:68', 'HEALTH', 'eine Hepatitis-B-Infektion'),
        ('doc-0014:13:0', 'ORG', 'Röhrdanz GmbH & Co. OHG'),
        ('doc-0014:20:0', 'TRADE_UNION', 'ver.di'),
        ('doc-0014:3:0', 'PHILOSOPHICAL_BELIEF', 'agnostisch'),
        ('doc-0015:14:0', 'SEX_LIFE', 'un suivi en PMA'),
        ('doc-0015:3:0', 'BIOMETRIC', 'empreinte digitale'),
        ('doc-0020:4:0', 'SEX_LIFE', 'eine Kinderwunschbehandlung'),
        ('doc-0021:0:15', 'PERSON', 'Hövel'),
        ('doc-0028:0:0', 'ETHNICITY', 'métisse'),
        ('doc-0032:19:0', 'ETHNICITY', 'métisse'),
        ('doc-0033:2:0', 'ETHNICITY', 'métisse'),
        ('doc-0034:13:18', 'GENETIC', 'Erbgutuntersuchung'),
        ('doc-0034:18:0', 'GENETIC', 'DNA-Analyse'),
        ('doc-0034:3:14', 'PERSON', 'L\u2019H\u00f4pital'),
        ('doc-0035:12:0', 'ETHNICITY', 'noir de peau'),
        ('doc-0035:6:0', 'POLITICAL_OPINION', 'monarchistisch'),
        ('doc-0038:0:0', 'SEX_LIFE', 'un suivi en PMA'),
        ('doc-0041:3:0', 'BIOMETRIC', 'empreinte digitale'),
        ('doc-0044:2:0', 'BIOMETRIC', 'Gesichtsscan'),
        ('doc-0045:1:21', 'PERSON', 'Wende'),
        ('doc-0046:4:0', 'POLITICAL_OPINION', 'eurokritisch'),
        ('doc-0048:0:0', 'TRADE_UNION', 'ver.di'),
        ('doc-0052:14:62', 'HEALTH', 'un diabète de type 2'),
        ('doc-0054:22:0', 'LOCATION', 'Lebrun-sur-Étienne'),
        ('doc-0055:11:38', 'HEALTH', 'un diabète de type 2'),
        ('doc-0055:13:63', 'HEALTH', 'une sclérose en plaques'),
    }
)


def test_the_fixture_still_has_the_leaf_shape_the_real_payload_has() -> None:
    documents = _documents()
    lengths = sorted(len(leaf["text"]) for d in documents for leaf in d["leaves"])
    counts = sorted(len(d["leaves"]) for d in documents)
    total = len(lengths)

    def percentile(p: int) -> int:
        return lengths[total * p // 100]

    assert (len(documents), total) == (60, 378), "documents and leaves"
    assert sum(len(leaf["entities"]) for d in documents for leaf in d["leaves"]) == 222, (
        "annotated entities — near the sentence corpus's 196, which is what makes "
        "an inventory from one comparable in size to an inventory from the other"
    )
    assert (counts[0], counts[-1]) == (2, 29), "leaves per document"
    assert 15 in counts, (
        "the largest property-count proxy must appear as a document, or "
        "`ARGUMENT_FIELD_COUNTS` is no longer reaching the corpus"
    )
    assert (percentile(50), percentile(75), percentile(90)) == (32, 77, 115), (
        "leaf-length percentiles; the real payload gives 37, 78 and 128, and "
        "this fixture is built to sit near them rather than on them"
    )
    assert (lengths[0], lengths[-1]) == (1, 280), "the ends, measured at 1 and 276"
    assert sum(1 for length in lengths if length <= 40) == 211, (
        "leaves no longer than 40 characters — 56% here against 53% measured"
    )
    number_shaped = [
        (d["id"], leaf["text"][e["start"] : e["end"]])
        for d in documents
        for leaf in d["leaves"]
        for e in leaf["entities"]
        if JSON_NUMBER.fullmatch(leaf["text"][e["start"] : e["end"]])
    ]
    assert not number_shaped, (
        "an annotated value with the shape of a JSON number could be sent as one, "
        "and a `Leaf::Number` is not masked: production refuses the request on a "
        "deterministic span and forwards the digits on any other "
        "(`proxy::tests::a_number_carrying_personal_data_refuses_the_request`, "
        "`a_number_the_exemption_forwards_is_journalled_as_forwarded`). These "
        "inventories model text leaves only, so this corpus must not hold one: "
        f"{number_shaped}"
    )


def test_the_joined_path_leaves_these_entities_with_the_provider(
    detector: Detector,
) -> None:
    joined, _, annotated = _reaching_the_provider(detector)
    assert joined == UNMASKED_JOINED, (
        f"of {annotated} annotated entities, these reach the provider with a word "
        f"intact when the document's leaves are read together. Arrived: "
        f"{sorted(joined - UNMASKED_JOINED)}. Gone: {sorted(UNMASKED_JOINED - joined)}. "
        "Re-record the set with the reason."
    )


def test_the_published_figures_come_from_the_same_members(detector: Detector) -> None:
    import evaluate

    leaks, _, _ = evaluate.json_document_figures(detector)
    assert leaks == UNMASKED_JOINED, (
        "`evaluate.py` publishes a different set of joined-path leaks than this "
        f"inventory gates. Only in evaluate: {sorted(leaks - UNMASKED_JOINED)}. "
        f"Only here: {sorted(UNMASKED_JOINED - leaks)}."
    )


def test_the_separate_path_leaves_these_entities_with_the_provider(
    detector: Detector,
) -> None:
    _, separate, annotated = _reaching_the_provider(detector)
    assert separate == UNMASKED_SEPARATE, (
        f"of {annotated} annotated entities, these reach the provider with a word "
        f"intact when each leaf is read alone. Arrived: "
        f"{sorted(separate - UNMASKED_SEPARATE)}. Gone: "
        f"{sorted(UNMASKED_SEPARATE - separate)}. Re-record the set with the reason."
    )


def test_the_net_difference_between_the_paths_understates_what_joining_loses(
    detector: Detector,
) -> None:
    joined, separate, _ = _reaching_the_provider(detector)
    assert (len(joined), len(separate)) == (44, 30), "the two totals"
    assert len(joined - separate) == 20, (
        "entities joining leaks and reading apart does not — the directional "
        f"loss: {sorted(joined - separate)}"
    )
    assert len(separate - joined) == 6, (
        "and the ones going the other way, which is what makes the net "
        f"misleading: {sorted(separate - joined)}"
    )
    assert len(joined - separate) > len(joined) - len(separate), (
        "the directional loss must exceed the net, or there is nothing here a "
        "net figure would have hidden and this test has stopped saying anything"
    )


NEEDS_A_TRIGGER_WORD = frozenset({("de_steuernummer", r"\b(?<!\d[ ./])\d{13}(?![ ./]?\d)\b")})


def test_only_these_rules_cannot_mask_without_a_trigger_word() -> None:
    rules = build_detector().deterministic.rules
    needs = {
        (rule.id, rule.pattern.pattern) for rule in rules if rule.confidence < rule.threshold
    }
    assert needs == NEEDS_A_TRIGGER_WORD, (
        "catalog rules whose pattern cannot reach their own threshold unaided. "
        f"Arrived: {sorted(needs - NEEDS_A_TRIGGER_WORD)}. Gone: "
        f"{sorted(NEEDS_A_TRIGGER_WORD - needs)}. A member is a value a JSON field "
        "leaf loses: the key that would carry its trigger word is never scanned "
        "(#104)."
    )


def test_a_bare_steuernummer_leaf_is_masked_in_its_slash_form_only() -> None:
    detector = build_detector()
    slash = "125/601/58393"
    found = [
        span for span in detector.detect(slash) if slash[span.start : span.end] == slash
    ]
    assert found and found[0].entity_type == "DE_STEUERNUMMER", (
        "a bare slash-form Steuernummer reached the provider — the #104 fix "
        "no longer holds"
    )
    unified = "9181081508155"
    assert detector.detect(unified) == [], (
        "a bare 13-digit Steuernummer was detected. That form stays behind its "
        "trigger because 1.5% of millisecond timestamps pass `de_stnr` and a JSON "
        "number with a deterministic type refuses the request; re-read #104 "
        "before re-recording this"
    )
    for context in (f"Steuernummer {unified}", f'{{"steuernummer": "{unified}"}}'):
        rescued = [
            span
            for span in detector.detect(context)
            if context[span.start : span.end] == unified
        ]
        assert rescued and rescued[0].entity_type == "DE_STEUERNUMMER", (
            f"the trigger word did not rescue the 13-digit form in {context!r}"
        )
