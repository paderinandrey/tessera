"""What reaches the provider when a document's leaves are a tool call's fields (#103).

**An absolute inventory, which the joined-path gate next door is not.**
`test_joined_detection.LOST_TO_JOINING` holds misses *relative* to reading the
leaves apart, so an entity missed on both paths is absent from it by
construction. That is the gentler half of the picture, and the README's claim —
every annotated entity with a word reaching the provider is named individually
— is published only for the separate path over `public.jsonl`.

**And a different corpus, because the old one has the wrong leaf shape.**
`_documents()` over `public.jsonl` groups four sentences of about ninety
characters. Measured against the one real payload this repository holds, in
`mapping::a_real_joined_call_is_many_short_leaves_rather_than_a_few_long_ones`,
production joins 79 leaves across ten schemas with a median of 37 characters
and 42 of them no longer than 40. The group's joined *length* is ordinary; its
granularity is not, and #102 reached the opposite reading by comparing against
gold values alone, which is the most favourable payload there is rather than
the representative one.

`evaluation/corpus/documents.jsonl` is that shape: leaf counts taken from the
real payload's schemas, leaf lengths matched to its distribution, and the same
synthetic value generators the sentence corpus uses.
"""

import json
from pathlib import Path
from typing import Any

import pytest

from tessera_detector.evaluation import unmasked_words
from tessera_detector.pipeline import Detector, build_detector
from tessera_detector.spans import Span

pytestmark = pytest.mark.ner

CORPUS = Path(__file__).resolve().parents[2] / "evaluation" / "corpus" / "documents.jsonl"
# What `mapping::Shape::of` puts between leaves.
JOIN = "\n\n"


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
    """Each gold entity in joined coordinates, keyed by the leaf it came from.

    Keyed by `document:leaf:offset` for the reason `test_joined_detection`
    gives: a value alone cannot say which occurrence was lost, and this corpus
    repeats `ver.di` and `métisse` across documents and within them.
    """
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
    """Both absolute inventories in one walk, so they cannot drift apart.

    Absolute: an entity is in a set when *any* word of it is left unmasked on
    that path, regardless of what the other path does. Words rather than spans
    or characters, which is the question `unmasked_words` exists to answer.
    """
    joined_out: set[tuple[str, str, str]] = set()
    separate_out: set[tuple[str, str, str]] = set()
    annotated = 0
    for document in _documents():
        text, truth, separate, joined = _rebased(detector, document)
        annotated += len(truth)
        for origin, span in truth:
            member = (origin, span.entity_type, text[span.start : span.end])
            if unmasked_words(text, span.start, span.end, joined):
                joined_out.add(member)
            if unmasked_words(text, span.start, span.end, separate):
                separate_out.add(member)
    return frozenset(joined_out), frozenset(separate_out), annotated


# **The two inventories, and the reason both are here.** The counts coincide —
# 33 of 183 annotated entities reach the provider on each path — and a gate
# written on the count alone would have reported that joining costs nothing.
# Eight members differ in each direction. That is the same cancellation
# `test_joined_detection` refuses one level up, met again at the level of the
# absolute figure, and it is why #103 asks for members rather than a number.
#
# **Re-record, do not relax.** A number that goes down is an improvement and
# these assertions fail on it too: an upper bound would silently accommodate a
# predicate that stops seeing things, which has happened twice in the gate next
# door.
UNMASKED_JOINED = frozenset(
    {
        ('doc-0003:3:0', 'SEX_LIFE', 'une interruption de grossesse'),
        ('doc-0006:14:0', 'TRADE_UNION', 'ver.di'),
        ('doc-0007:14:0', 'DE_STEUERNUMMER', '126/734/94551'),
        ('doc-0007:16:19', 'PERSON', 'Förster'),
        ('doc-0007:25:29', 'GENETIC', 'séquençage ADN'),
        ('doc-0007:26:68', 'HEALTH', 'eine Hepatitis-B-Infektion'),
        ('doc-0007:4:0', 'BIOMETRIC', 'Fingerabdruck'),
        ('doc-0012:0:0', 'POLITICAL_AFFILIATION', 'socialiste'),
        ('doc-0012:2:0', 'TRADE_UNION', 'ver.di'),
        ('doc-0014:0:0', 'PHILOSOPHICAL_BELIEF', 'athée'),
        ('doc-0016:0:0', 'BIOMETRIC', 'empreinte digitale'),
        ('doc-0016:11:0', 'SEX_LIFE', 'un suivi en PMA'),
        ('doc-0016:6:0', 'DE_STEUERNUMMER', '125/601/58393'),
        ('doc-0017:11:15', 'PERSON', 'Hövel'),
        ('doc-0017:7:0', 'SEX_LIFE', 'eine Kinderwunschbehandlung'),
        ('doc-0020:5:0', 'DE_STEUERNUMMER', '554/835/49146'),
        ('doc-0022:1:0', 'DE_STEUERNUMMER', '145/452/53574'),
        ('doc-0023:1:0', 'DE_STEUERNUMMER', '884/134/90923'),
        ('doc-0023:2:0', 'POLITICAL_AFFILIATION', 'écologiste'),
        ('doc-0026:18:14', 'PERSON', 'L\u2019H\u00f4pital'),
        ('doc-0027:13:0', 'GENETIC', 'DNA-Analyse'),
        ('doc-0030:1:0', 'ETHNICITY', 'noir de peau'),
        ('doc-0031:0:0', 'SEX_LIFE', 'un suivi en PMA'),
        ('doc-0032:1:0', 'SEXUAL_ORIENTATION', 'bisexuelle'),
        ('doc-0036:12:21', 'PERSON', 'Wende'),
        ('doc-0036:13:19', 'PERSON', 'Weimer'),
        ('doc-0036:2:0', 'BIOMETRIC', 'empreinte digitale'),
        ('doc-0036:5:0', 'DE_STEUERNUMMER', '761/926/11328'),
        ('doc-0036:9:0', 'BIOMETRIC', 'Gesichtsscan'),
        ('doc-0037:0:18', 'PERSON', 'Pölitz'),
        ('doc-0037:28:62', 'HEALTH', 'un diabète de type 2'),
        ('doc-0037:5:0', 'TRADE_UNION', 'ver.di'),
        ('doc-0037:9:34', 'PERSON', 'dell\u2019Orto'),
    }
)

UNMASKED_SEPARATE = frozenset(
    {
        ('doc-0003:3:0', 'SEX_LIFE', 'une interruption de grossesse'),
        ('doc-0006:14:0', 'TRADE_UNION', 'ver.di'),
        ('doc-0007:14:0', 'DE_STEUERNUMMER', '126/734/94551'),
        ('doc-0007:25:29', 'GENETIC', 'séquençage ADN'),
        ('doc-0007:26:68', 'HEALTH', 'eine Hepatitis-B-Infektion'),
        ('doc-0007:4:0', 'BIOMETRIC', 'Fingerabdruck'),
        ('doc-0008:1:0', 'PHILOSOPHICAL_BELIEF', 'agnostisch'),
        ('doc-0010:5:0', 'ORG', 'Röhrdanz GmbH & Co. OHG'),
        ('doc-0012:2:0', 'TRADE_UNION', 'ver.di'),
        ('doc-0016:0:0', 'BIOMETRIC', 'empreinte digitale'),
        ('doc-0016:11:0', 'SEX_LIFE', 'un suivi en PMA'),
        ('doc-0016:6:0', 'DE_STEUERNUMMER', '125/601/58393'),
        ('doc-0017:11:15', 'PERSON', 'Hövel'),
        ('doc-0017:7:0', 'SEX_LIFE', 'eine Kinderwunschbehandlung'),
        ('doc-0020:2:0', 'ETHNICITY', 'métisse'),
        ('doc-0020:5:0', 'DE_STEUERNUMMER', '554/835/49146'),
        ('doc-0022:1:0', 'DE_STEUERNUMMER', '145/452/53574'),
        ('doc-0023:1:0', 'DE_STEUERNUMMER', '884/134/90923'),
        ('doc-0026:18:14', 'PERSON', 'L\u2019H\u00f4pital'),
        ('doc-0026:6:0', 'ETHNICITY', 'métisse'),
        ('doc-0026:9:0', 'ETHNICITY', 'métisse'),
        ('doc-0027:13:0', 'GENETIC', 'DNA-Analyse'),
        ('doc-0027:8:18', 'GENETIC', 'Erbgutuntersuchung'),
        ('doc-0028:1:0', 'POLITICAL_OPINION', 'monarchistisch'),
        ('doc-0030:1:0', 'ETHNICITY', 'noir de peau'),
        ('doc-0031:0:0', 'SEX_LIFE', 'un suivi en PMA'),
        ('doc-0036:12:21', 'PERSON', 'Wende'),
        ('doc-0036:19:0', 'POLITICAL_OPINION', 'eurokritisch'),
        ('doc-0036:2:0', 'BIOMETRIC', 'empreinte digitale'),
        ('doc-0036:5:0', 'DE_STEUERNUMMER', '761/926/11328'),
        ('doc-0036:9:0', 'BIOMETRIC', 'Gesichtsscan'),
        ('doc-0037:28:62', 'HEALTH', 'un diabète de type 2'),
        ('doc-0037:5:0', 'TRADE_UNION', 'ver.di'),
    }
)


def test_the_fixture_still_has_the_leaf_shape_the_real_payload_has() -> None:
    """The corpus is only worth scoring while it keeps the shape it was built for.

    Without this the fixture could drift back towards a few long leaves — which
    is what `public.jsonl` grouped into fours already is — and the inventories
    below would go on passing while measuring the shape they were written to
    replace. The target figures come from
    `mapping::a_real_joined_call_is_many_short_leaves_rather_than_a_few_long_ones`,
    which measures `gateway/src/testdata/claude_code_tools.json`.

    Pinned exactly, because the corpus is committed and seeded: a tolerance
    would be a band around a constant.
    """
    documents = _documents()
    lengths = sorted(len(leaf["text"]) for d in documents for leaf in d["leaves"])
    counts = sorted(len(d["leaves"]) for d in documents)
    total = len(lengths)

    def percentile(p: int) -> int:
        # Nearest-rank, the same rule the Rust measurement states, so the two
        # sets of figures are comparable rather than merely adjacent.
        return lengths[total * p // 100]

    assert (len(documents), total) == (40, 316), "documents and leaves"
    assert sum(len(leaf["entities"]) for d in documents for leaf in d["leaves"]) == 183, (
        "annotated entities — near the sentence corpus's 196, which is what makes "
        "an inventory from one comparable in size to an inventory from the other"
    )
    # Leaf counts straight from the real payload's ten schemas, so the
    # four-leaf document the sentence corpus groups is present but is the floor
    # rather than the whole corpus.
    assert (counts[0], counts[-1]) == (2, 29), "leaves per document"
    # The distribution, against the measured p50=37, p75=78, p90=128, max=276.
    assert (percentile(50), percentile(75), percentile(90)) == (32, 77, 115), (
        "leaf-length percentiles; the real payload gives 37, 78 and 128, and "
        "this fixture is built to sit near them rather than on them"
    )
    assert (lengths[0], lengths[-1]) == (1, 280), "the ends, measured at 1 and 276"
    assert sum(1 for length in lengths if length <= 40) == 174, (
        "leaves no longer than 40 characters — 55% here against 53% measured"
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


def test_the_paths_leak_equally_many_entities_and_not_the_same_ones(
    detector: Detector,
) -> None:
    """The finding this corpus was built to make visible, asserted on its own.

    Both inventories hold 33 members. Eight are in one and not the other, in
    each direction, so neither path dominates and the equal totals are a
    coincidence of this corpus rather than a property of the detector. A
    reviewer reading only the two numbers above would conclude the leaf shape
    makes no difference; it makes a different one.
    """
    joined, separate, _ = _reaching_the_provider(detector)
    assert len(joined) == len(separate) == 33, "the totals that coincide"
    assert len(joined - separate) == len(separate - joined) == 8, (
        "entities one path leaks and the other does not, in each direction: "
        f"joined-only {sorted(joined - separate)}, separate-only "
        f"{sorted(separate - joined)}"
    )


# `confidence` below `threshold` means the pattern alone cannot mask: it needs a
# trigger word within `boost.window` tokens. In `public.jsonl` the Steuernummer
# template always supplies one — "Steuernummer {stnr} des Mandanten …" — so the
# published per-type recall for this identifier is 1.0 and tier 1 recall is
# 1.0, both measured over text that always carries the context the rule needs.
#
# A field payload does not carry it. `{"steuernummer": "125/601/58393"}` puts
# the trigger word in the **key**, and a key is never scanned: `mapping::walk`
# iterates the fields and yields only their values, because masking a key would
# break the call it dispatches. So the one place a JSON document keeps that
# context is the one place detection cannot see it, and the value goes to the
# provider whole.
NEEDS_A_TRIGGER_WORD = frozenset({"DE_STEUERNUMMER"})


def test_only_these_identifiers_cannot_mask_without_a_trigger_word() -> None:
    """The class, enumerated, so a second member cannot arrive unnoticed.

    Needs no model: this is the catalog's own arithmetic. A new identifier
    declaring `confidence` below its `threshold` is one more value that a bare
    field leaf loses, and nothing else in this repository would say so — the
    corpus the per-type recall is published from puts every identifier in a
    sentence that supplies its trigger.
    """
    rules = build_detector().deterministic.rules
    needs = {rule.entity_type for rule in rules if rule.confidence < rule.threshold}
    assert needs == NEEDS_A_TRIGGER_WORD, (
        "identifiers whose pattern cannot reach their own threshold unaided. "
        f"Arrived: {sorted(needs - NEEDS_A_TRIGGER_WORD)}. Gone: "
        f"{sorted(NEEDS_A_TRIGGER_WORD - needs)}. A new member is a value that a "
        "JSON field leaf loses, because the key that would carry its trigger "
        "word is not scanned — see the comment above and #104."
    )


def test_a_bare_steuernummer_leaf_reaches_the_provider() -> None:
    """The class's one member, demonstrated end to end rather than argued.

    Three texts, one value: alone it is not detected at all, and either form of
    surrounding context rescues it. The middle case is what `public.jsonl`
    measures; the first is what a tool call sends.
    """
    detector = build_detector()
    value = "125/601/58393"
    assert detector.detect(value) == [], (
        "a bare Steuernummer was detected, so the boost is no longer what "
        "carries it — re-read this test's premise before re-recording it"
    )
    for context in (f"Steuernummer {value}", f'{{"steuernummer": "{value}"}}'):
        found = [
            span
            for span in detector.detect(context)
            if context[span.start : span.end] == value
        ]
        assert found, f"the trigger word did not rescue the value in {context!r}"
        assert found[0].entity_type == "DE_STEUERNUMMER"
