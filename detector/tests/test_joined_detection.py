"""What joining the leaves of one document costs detection (#42).

**This is the `Slot::Json` path and not a conversation.** An earlier version of
this file said a message is never read alone in production, and that is wrong:
`proxy::mask_all` gives every `Slot::Text` its own `detector.detect` call, so a
plain chat message *is* read alone. What gets joined is the leaves **within a
single JSON document** — a tool call's arguments, a structured `content` — which
`mapping::Shape::of` concatenates and `Joined::split` returns to their leaves
afterwards.

So the question these tests answer is narrower than the one they were written
for, and it is a question production actually asks: when several short field
values are read as one text, does the detector find more, less, or the same?

Scored against the corpus's annotations rather than against a run on isolated
text — the difference is not pedantry. Comparing to an isolated run counts a
false positive that disappeared as a loss, and that is how the first attempt at
this measurement reached the opposite conclusion.
"""

import json
from pathlib import Path

import pytest

from tessera_detector.evaluation import EvalEntity, overmasking_counts, unmasked_words
from tessera_detector.pipeline import Detector, build_detector
from tessera_detector.spans import Span

pytestmark = pytest.mark.ner

CORPUS = Path(__file__).resolve().parents[2] / "evaluation" / "corpus" / "public.jsonl"
# What `mapping::Shape::of` puts between leaves.
JOIN = "\n\n"
# Leaves per synthetic document. A tool call's arguments carry a handful of
# short values; four is that shape, and the remainder of the corpus is scored
# as a smaller document rather than dropped.
LEAVES = 4


@pytest.fixture(scope="module")
def detector() -> Detector:
    built = build_detector()
    if not built.ner_available:
        pytest.skip(f"NER is not provisioned ({built.ner_off_reason})")
    return built


def _documents() -> list[list[dict]]:
    rows = [
        json.loads(line)
        for line in CORPUS.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    # Every row, including the last partial group: the corpus is 130 rows and
    # four does not divide it, so a plain stride silently dropped two documents
    # — and a document with no annotations is exactly where a false-positive
    # regression would hide from the precision half.
    return [rows[at : at + LEAVES] for at in range(0, len(rows), LEAVES)]


def _inside_one_leaf(span: Span, ranges: list[tuple[int, int]]) -> bool:
    """Whether production could return this span to a leaf.

    Mirrors `mapping::Joined::split`: a span that straddles two leaves or lands
    in a separator is refused there and the request fails, so it is not a
    detection and must not be scored as one.
    """
    return any(start <= span.start and span.end <= end for start, end in ranges)


def _leaf_ranges(group: list[dict]) -> list[tuple[int, int]]:
    ranges: list[tuple[int, int]] = []
    at = 0
    for document in group:
        ranges.append((at, at + len(document["text"])))
        at += len(document["text"]) + len(JOIN)
    return ranges


def _rebased(detector: Detector, group: list[dict]) -> tuple[list[Span], list[Span], list[Span]]:
    """Truth, separate predictions and joined predictions, all in joined coordinates."""
    truth: list[Span] = []
    separate: list[Span] = []
    at = 0
    for document in group:
        truth += [
            Span(
                entity_type=entity["entity_type"],
                start=entity["start"] + at,
                end=entity["end"] + at,
                confidence=1.0,
                recognizer="corpus",
                tier=1,
            )
            for entity in document["entities"]
        ]
        separate += [
            span.model_copy(update={"start": span.start + at, "end": span.end + at})
            for span in detector.detect(document["text"])
        ]
        at += len(document["text"]) + len(JOIN)

    joined = detector.detect(JOIN.join(document["text"] for document in group))
    return truth, separate, joined


def _lost(text: str, truth: Span, separate: list[Span], joined: list[Span]) -> bool:
    """Whether joining leaves words of this entity that reading leaves apart does not.

    **A comparison of two sets, not two verdicts.** Every earlier version asked
    each strategy a yes/no question and compared the answers, and every one was
    wrong because the two strategies fail the same question for reasons that
    have nothing to do with joining:

    - **type-matched.** Both found 164; the gap was invisible. A placeholder's
      name protects nobody;
    - **a net difference.** One truth only joining finds cancelled one of the
      losses, so it read 3 against a real 4;
    - **full containment.** The corpus annotates `eine Hepatitis-B-Infektion`
      and the detector returns `Hepatitis-B-Infektion`, so *neither* strategy
      contained it — and a HEALTH entity joining genuinely loses was hidden,
      because the shortfall failed the separate path on the same test and the
      two errors cancelled inside the gate written to catch it. 4 against 5;
    - **words, minus a list of articles.** That saw the fifth, and bought it
      with an exemption that fired in any position under any type, so a `PERSON`
      annotated `Le Thi Mai` with only `Thi Mai` predicted would have read as
      masked — `Le` being a Vietnamese family name that `ner.py` protects by
      name. Found in review, and it is the trimming rule's own defect
      reintroduced in the code that measures it.

    Comparing the sets needs no list. A word both strategies leave — a gold
    article neither predicts — is in both sets and cancels itself. A word only
    joining leaves is the loss, and nothing has to decide what a word means.
    """
    apart = set(unmasked_words(text, truth.start, truth.end, separate))
    together = set(unmasked_words(text, truth.start, truth.end, joined))
    return bool(together - apart)


def _covered(text: str, truth: Span, predictions: list[Span]) -> bool:
    """Whether every word of the entity is completely masked.

    Reported rather than gated — `separate_found` and `joined_found` are context
    for the number below, and both carry the corpus's article shortfalls, which
    is why the gate is `_lost` and not a difference of these two.
    """
    return not unmasked_words(text, truth.start, truth.end, predictions)


def _redacted_types(detector: Detector) -> set[str]:
    """Every type the detector can emit, which is every type production masks.

    Taking the set from the *gold* labels of each group instead — the first
    version — made `overmasking_counts` discard every prediction outside it. A
    group with no annotations then contributed nothing at all, and a
    false-positive type that appears only when joining was invisible to the
    gate written to catch exactly that.
    """
    types = set(detector.deterministic.specificity)
    if detector.recognizer is not None:
        types |= set(detector.recognizer.specificity)
    return types


def _score(detector: Detector) -> dict[str, int]:
    redacted_types = _redacted_types(detector)
    totals = dict.fromkeys(
        ("truth", "separate_found", "joined_found", "separate_masked", "joined_masked",
         "separate_overmasked", "joined_overmasked", "crossing", "lost_to_joining"),
        0,
    )
    for group in _documents():
        truth, separate, joined = _rebased(detector, group)
        ranges = _leaf_ranges(group)
        # The joined text these coordinates are in. `_rebased` rebases truth and
        # the separate predictions into it, so one string answers for all three.
        text = JOIN.join(document["text"] for document in group)

        # A span that straddles two leaves or lands in a separator is not a
        # detection in production: `Joined::split` refuses it and the request
        # fails. Counting it as found would let a regression that breaks every
        # such request pass both gates below.
        totals["crossing"] += sum(1 for span in joined if not _inside_one_leaf(span, ranges))

        totals["truth"] += len(truth)
        # **Directional, not net.** `separate_found - joined_found` lets a
        # truth that only joining finds cancel a different truth that only
        # joining loses, so the gate can hold while names go unmasked in
        # `Slot::Json` leaves. These are the entities reading leaves apart
        # covers and joining does not — the ones a caller loses.
        totals["lost_to_joining"] += sum(
            1 for entity in truth if _lost(text, entity, separate, joined)
        )
        entities = [
            EvalEntity(entity_type=span.entity_type, start=span.start, end=span.end)
            for span in truth
        ]
        for name, predictions in (("separate", separate), ("joined", joined)):
            totals[f"{name}_found"] += sum(
                1 for entity in truth if _covered(text, entity, predictions)
            )
            # The established over-masking metric: a prediction is a cost when
            # most of what it hides holds no personal data. Its *type* being
            # wrong is not a cost — the span is redacted either way.
            counts = overmasking_counts(entities, predictions, types=redacted_types)
            landed = sum(kept for kept, _ in counts.values())
            total = sum(whole for _, whole in counts.values())
            totals[f"{name}_masked"] += total
            totals[f"{name}_overmasked"] += total - landed
    return totals


def test_a_span_across_a_leaf_boundary_is_not_a_detection() -> None:
    # The predicate on its own, because the corpus produces no crossing span
    # and a counter nothing can move reads as coverage without being any.
    # Needs no model, so it runs wherever the file does.
    ranges = [(0, 10), (12, 20)]

    def at(start: int, end: int) -> Span:
        return Span(
            entity_type="PERSON",
            start=start,
            end=end,
            confidence=0.9,
            recognizer="ner:gliner",
            tier=2,
        )

    assert _inside_one_leaf(at(0, 10), ranges)
    assert _inside_one_leaf(at(13, 19), ranges)
    assert not _inside_one_leaf(at(8, 15), ranges), "a span straddling two leaves was accepted"
    assert not _inside_one_leaf(at(10, 12), ranges), "a span inside a separator was accepted"


def test_no_joined_span_crosses_a_leaf_boundary(detector: Detector) -> None:
    # Production refuses these outright — `Joined::split` returns
    # `BadSpan("across a joined boundary")` and the request 502s — so the two
    # gates below are only meaningful while this holds.
    #
    # It has never been non-zero on this corpus, including with a
    # single-character separator, so it is a guard against a future regression
    # rather than a pinned behaviour. The predicate itself is pinned above.
    assert _score(detector)["crossing"] == 0


# What joining currently costs in recall, measured. **This is a defect being
# tracked, not a target** — see #44. The gate stops it widening; it does not
# bless it.
#
# It was nine, and lowering `PERSON`'s threshold from 0.7 to 0.5 took it to
# four: eight of the twelve detections joining lost were names scoring between
# those two numbers, which is what a threshold calibrated on isolated
# single-label text does to a joined three-label call. The remaining four are
# not that, and tightening the bound with the fix is what stops them being
# forgotten behind a number that used to have slack in it.
#
# **Five, directional, and counted by word.** Each of those three adjectives
# replaced a version of this gate that was wrong, and every one was wrong the
# same way — by aggregating over something that is not the caller's loss:
#
#   type-matched      both strategies found 164; the gap was invisible
#   net difference    read 3, because one truth only *joining* finds cancelled
#                     one of the losses. Raised by review on #48
#   full containment  read 4 against a real 5, and the hidden one is HEALTH —
#                     an Article 9 entity. The corpus annotates `eine
#                     Hepatitis-B-Infektion` and the detector returns
#                     `Hepatitis-B-Infektion`, so *neither* strategy contained
#                     it, the separate path failed the same test, and the two
#                     errors cancelled inside the gate written to catch this
#
# The fifth is not new behaviour: it was always there and this is the first
# predicate that can see it. Raising the number is the measurement improving,
# not the detector regressing.
LOST_TO_JOINING = 5


def test_joining_does_not_lose_more_recall_than_it_does_today(detector: Detector) -> None:
    # The entities reading leaves apart covers and joining does not, counted as
    # a set rather than as a difference of two totals. Counted by position, so a
    # relabelling is not a loss — these are characters left unmasked that
    # per-leaf detection would have hidden, in `Slot::Json` leaves, which is
    # where a tool call keeps its names.
    #
    # Three earlier versions of this test were wrong the same way, by
    # aggregating over something that is not the caller's loss:
    #
    # - it required the covering span to carry the *gold type*, under which both
    #   strategies found 164 and the gap was invisible. Position is the question
    #   a masking gateway asks; the placeholder's name protects nobody;
    # - it then gated `separate_found - joined_found`, a net figure that a truth
    #   found only by joining can pay for. It read 3 against a real 4;
    # - and it asked for *full containment*, which the corpus's leading articles
    #   fail on both paths at once — hiding a HEALTH entity joining genuinely
    #   loses, because the separate path failed the same test and the errors
    #   cancelled. It read 4 against a real 5.
    totals = _score(detector)

    # **Asserted exactly, and that is the fourth thing this gate got wrong.**
    # `<=` cannot catch a predicate that stops seeing things: every mutation of
    # `_covered` that makes it *blinder* lowers the count and passes an upper
    # bound. Two did — ignoring the article list, and reverting to full
    # containment — and both left the gate green while it measured less than it
    # claims to.
    #
    # A number that goes down is an improvement and this fails on it too. That
    # is the assertion working: an improvement is a measurement to re-record,
    # and the alternative is a bound that silently accommodates a gate going
    # dark. The same rule `a_real_tool_payload_fits_the_bounds_this_gateway_
    # ships_with` states one crate over — do not relax it, re-measure.
    assert totals["lost_to_joining"] == LOST_TO_JOINING, (
        "the entities joining leaves unmasked and per-leaf detection does not: "
        f"{totals['lost_to_joining']} against {LOST_TO_JOINING} recorded, of "
        f"{totals['truth']}. Up is a regression; down is an improvement and a "
        "constant to re-record — neither is something to widen a bound for."
    )


def test_joining_does_not_cost_precision(detector: Detector) -> None:
    # The other half, and it is what the recall gap buys: joining hid half as
    # much text that holds no personal data — 23 spans against 47. A regression
    # here costs the caller text they never asked to have hidden.
    #
    # The two gates together are the trade, and neither is the whole answer:
    # separate reading masks more, which catches more names and more of
    # everything else.
    totals = _score(detector)

    assert totals["joined_overmasked"] <= totals["separate_overmasked"], (
        "joining over-masked more spans than reading leaves apart: "
        f"{totals['joined_overmasked']} against {totals['separate_overmasked']}"
    )
