from tessera_detector.resolution import resolve
from tessera_detector.spans import Span

SPEC = {"IBAN": 90, "FR_NIR": 80, "CREDIT_CARD": 40, "ORG": 20}


def span(
    entity_type: str = "IBAN",
    start: int = 0,
    end: int = 10,
    confidence: float = 1.0,
    recognizer: str = "catalog:x",
    tier: int = 1,
    boosted: bool = False,
) -> Span:
    return Span(
        entity_type=entity_type,
        start=start,
        end=end,
        confidence=confidence,
        recognizer=recognizer,
        tier=tier,
        boosted=boosted,
    )


def test_non_overlapping_pass_through_sorted() -> None:
    a, b = span(start=20, end=30), span(start=0, end=10)
    result = resolve([a, b], specificity=SPEC)
    assert result.spans == [b, a]
    assert result.trace == []


def test_identical_duplicates_deduped() -> None:
    a = span(confidence=0.9, recognizer="ner:gliner", tier=2, entity_type="ORG")
    b = span(confidence=0.7, recognizer="ner:other", tier=2, entity_type="ORG")
    result = resolve([a, b], specificity=SPEC)
    (kept,) = result.spans
    assert kept.confidence == 0.9


def test_same_type_overlap_merges_to_union() -> None:
    a = span(start=0, end=10, confidence=0.8, recognizer="ner:a", tier=2, entity_type="ORG")
    b = span(start=5, end=15, confidence=0.9, recognizer="ner:b", tier=2, entity_type="ORG")
    result = resolve([a, b], specificity=SPEC)
    (kept,) = result.spans
    assert (kept.start, kept.end) == (0, 15)
    assert kept.confidence == 0.9
    assert kept.entity_type == "ORG"


def test_nested_different_types_outer_wins() -> None:
    outer = span(entity_type="ORG", start=0, end=20, confidence=0.6, recognizer="ner:g", tier=3)
    inner = span(entity_type="ORG2", start=5, end=10, confidence=0.9, recognizer="ner:g", tier=3)
    result = resolve([outer, inner], specificity={"ORG": 20, "ORG2": 20})
    (kept,) = result.spans
    assert (kept.start, kept.end) == (0, 20)
    assert kept.entity_type == "ORG"


def test_untouchable_inner_survives_inside_non_checksum_outer() -> None:
    # Rule 1 beats rule 3: a checksum identifier nested in an NER span keeps its
    # Tier 1 identity; bounds take the union so the whole region is masked.
    outer = span(entity_type="ORG", start=0, end=30, confidence=0.7, recognizer="ner:g", tier=3)
    inner = span(entity_type="IBAN", start=5, end=27, confidence=1.0, recognizer="catalog:iban")
    result = resolve([outer, inner], specificity=SPEC)
    (kept,) = result.spans
    assert (kept.start, kept.end) == (0, 30)
    assert kept.entity_type == "IBAN"
    assert kept.tier == 1


def test_partial_overlap_higher_specificity_wins() -> None:
    # This test was named for a partial overlap and built an equal range, so
    # the branch it claimed to cover — the one where the extents differ — was
    # reached by nothing in the suite. Both shapes are asserted now, and the
    # bounds are asserted with the type, because the type alone is what let
    # rule 4 drop the loser's characters unnoticed.
    nir = span(entity_type="FR_NIR", start=0, end=15, recognizer="catalog:fr_nir")
    card = span(entity_type="CREDIT_CARD", start=0, end=15, recognizer="catalog:credit_card")
    result = resolve([card, nir], specificity=SPEC)
    (kept,) = result.spans
    assert kept.entity_type == "FR_NIR"
    assert (kept.start, kept.end) == (0, 15)

    # Neither span contains the other, which is what makes this rule 4 rather
    # than the containment branch above — a first draft of this case wrote
    # `0:15` inside `0:25` and was answered by rule 3, passing for a reason it
    # did not claim.
    overhanging_card = span(
        entity_type="CREDIT_CARD", start=10, end=25, recognizer="catalog:credit_card"
    )
    result = resolve([overhanging_card, nir], specificity=SPEC)
    (kept,) = result.spans
    assert [decision.rule for decision in result.trace] == ["specificity"]
    assert kept.entity_type == "FR_NIR"
    assert (kept.start, kept.end) == (0, 25), "the card's last ten characters lost their mask"


def test_specificity_tie_higher_confidence_wins() -> None:
    a = span(entity_type="A", start=0, end=10, confidence=0.9, recognizer="ner:x", tier=2)
    b = span(entity_type="B", start=5, end=15, confidence=0.6, recognizer="ner:x", tier=2)
    result = resolve([a, b], specificity={"A": 50, "B": 50})
    (kept,) = result.spans
    assert kept.entity_type == "A"
    # The surer reading names the span; it does not decide how much of the
    # text stays masked. Asserting the type alone let 10..15 fall out.
    assert (kept.start, kept.end) == (0, 15)
    assert kept.confidence == 0.9


def test_full_tie_merges_with_more_sensitive_type() -> None:
    a = span(entity_type="A", start=0, end=10, confidence=0.8, recognizer="ner:x", tier=2)
    b = span(entity_type="B", start=5, end=15, confidence=0.8, recognizer="ner:x", tier=1)
    result = resolve([a, b], specificity={"A": 50, "B": 50})
    (kept,) = result.spans
    assert (kept.start, kept.end) == (0, 15)
    assert kept.entity_type == "B"  # tier 1 is more sensitive
    assert kept.tier == 1


def test_trace_records_applied_rules() -> None:
    outer = span(entity_type="ORG", start=0, end=20, confidence=0.6, recognizer="ner:g", tier=3)
    inner = span(entity_type="ORG2", start=5, end=10, confidence=0.9, recognizer="ner:g", tier=3)
    result = resolve([outer, inner], specificity={"ORG": 20, "ORG2": 20})
    (decision,) = result.trace
    assert decision.rule == "nesting-outer-wins"
    assert decision.kept.entity_type == "ORG"
    assert [d.entity_type for d in decision.dropped] == ["ORG2"]


def test_chain_of_overlaps_reaches_fixpoint() -> None:
    # a overlaps b, the a+b union then overlaps c: resolution must converge.
    a = span(entity_type="ORG", start=0, end=10, confidence=0.8, recognizer="ner:x", tier=3)
    b = span(entity_type="ORG", start=8, end=18, confidence=0.7, recognizer="ner:x", tier=3)
    c = span(entity_type="ORG", start=17, end=25, confidence=0.9, recognizer="ner:x", tier=3)
    result = resolve([a, b, c], specificity=SPEC)
    (kept,) = result.spans
    assert (kept.start, kept.end) == (0, 25)


def test_untouchable_beats_higher_specificity_on_partial_overlap() -> None:
    # Rule 1 precedes rule 4: a lone checksum span never loses to a non-checksum
    # span, even when the latter's type ranks higher in specificity.
    nir = span(entity_type="FR_NIR", start=0, end=15, recognizer="catalog:fr_nir")
    model = span(
        entity_type="VERY_SPECIFIC", start=0, end=15, confidence=0.99, recognizer="ner:x", tier=2
    )
    result = resolve([model, nir], specificity={"VERY_SPECIFIC": 95, "FR_NIR": 80})
    (kept,) = result.spans
    assert kept.entity_type == "FR_NIR"

    # And the shape the name promises: the model's reading runs past the
    # checksum's. Rule 1 keeps the identifier's identity; it must not throw
    # away the ten characters only the model marked.
    overhanging = span(
        entity_type="VERY_SPECIFIC", start=5, end=25, confidence=0.99, recognizer="ner:x", tier=2
    )
    result = resolve([overhanging, nir], specificity={"VERY_SPECIFIC": 95, "FR_NIR": 80})
    (kept,) = result.spans
    assert kept.entity_type == "FR_NIR"
    assert (kept.start, kept.end) == (0, 25)


def test_resolution_is_order_independent() -> None:
    # Equal-range, equally scored spans must resolve identically regardless of
    # input order (set() iteration order is hash-seed dependent).
    a = span(entity_type="AAA", start=0, end=10, confidence=0.8, recognizer="ner:x", tier=2)
    b = span(entity_type="BBB", start=0, end=10, confidence=0.8, recognizer="ner:y", tier=2)
    kept_ab = resolve([a, b], specificity={"AAA": 50, "BBB": 50}).spans[0]
    kept_ba = resolve([b, a], specificity={"AAA": 50, "BBB": 50}).spans[0]
    assert kept_ab == kept_ba


def test_same_type_merge_prefers_untouchable_metadata() -> None:
    # A same-type merge must keep the checksum span's audit identity even when a
    # confidence-1.0 model span sorts first and ties on confidence.
    model = span(entity_type="IBAN", start=0, end=30, confidence=1.0, recognizer="ner:g", tier=1)
    checksum = span(entity_type="IBAN", start=5, end=27, recognizer="catalog:iban")
    result = resolve([model, checksum], specificity=SPEC)
    (kept,) = result.spans
    assert (kept.start, kept.end) == (0, 30)
    assert kept.recognizer == "catalog:iban"


def test_more_specific_inner_survives_a_broader_outer() -> None:
    # ORG spans "la CGT" while TRADE_UNION spans "CGT": nesting alone would
    # erase the Article 9 classification, which is the one that decides
    # whether the span is treated as special-category data.
    outer = span("ORG", 22, 28, confidence=0.9)
    inner = span("TRADE_UNION", 25, 28, confidence=0.5)
    resolution = resolve([outer, inner], specificity={"ORG": 10, "TRADE_UNION": 35})
    (kept,) = resolution.spans
    assert kept.entity_type == "TRADE_UNION"
    assert (kept.start, kept.end) == (22, 28)


def test_broader_outer_still_wins_over_a_less_specific_inner() -> None:
    outer = span("TRADE_UNION", 22, 28, confidence=0.9)
    inner = span("ORG", 25, 28, confidence=0.5)
    resolution = resolve([outer, inner], specificity={"ORG": 10, "TRADE_UNION": 35})
    (kept,) = resolution.spans
    assert kept.entity_type == "TRADE_UNION"
    assert (kept.start, kept.end) == (22, 28)


def test_checksum_outer_keeps_its_identity_over_a_more_specific_inner() -> None:
    # Rule 1 is unconditional: a checksum span is never silently replaced, and
    # a custom catalog can put a higher specificity on the inner type.
    outer = span("IBAN", 0, 27, confidence=1.0, recognizer="catalog:iban")
    inner = span("TRADE_UNION", 10, 20, confidence=0.6, recognizer="ner:gliner", tier=3)
    resolution = resolve([outer, inner], specificity={"IBAN": 90, "TRADE_UNION": 95})
    (kept,) = resolution.spans
    assert kept.entity_type == "IBAN"
    assert kept.recognizer == "catalog:iban"


def test_a_third_span_that_loses_every_rule_does_not_change_which_checksum_wins() -> None:
    # #39. `resolve` folds over conflicting pairs, so a merge can synthesise an
    # outer that then wins by nesting — and the containment branch never asks
    # about specificity when both sides carry a checksum.
    #
    # `ORG` loses to both on specificity and appears in neither output. Sorted,
    # it comes first, so it merges with `CREDIT_CARD` and the union swallows the
    # more specific `FR_NIR`. Eighty lost to forty because forty was merged
    # first, and a French social security number was recorded as a payment card.
    #
    # The extent is asserted as well as the type: the union widening to cover
    # the NER span is deliberate, and a test that checked only the type could
    # not tell that apart from a replacement.
    card = span(entity_type="CREDIT_CARD", start=45, end=66, recognizer="catalog:credit_card")
    nir = span(entity_type="FR_NIR", start=45, end=66, recognizer="catalog:fr_nir")
    org = span(
        entity_type="ORG", start=41, end=66, confidence=0.9, recognizer="ner:gliner", tier=2
    )

    (alone,) = resolve([card, nir], specificity=SPEC).spans
    assert (alone.entity_type, alone.start, alone.end) == ("FR_NIR", 45, 66)

    (kept,) = resolve([card, nir, org], specificity=SPEC).spans
    assert (kept.entity_type, kept.start, kept.end) == ("FR_NIR", 41, 66)


def test_a_checksum_outer_keeps_its_type_over_a_less_specific_checksum_inner() -> None:
    # The other side of the branch above, and it was missing: dropping the
    # specificity comparison entirely left every test in this file passing, so
    # nothing held that the new merge fires only when the inner *outranks* the
    # outer. Without that, the more specific of two checksum readings would
    # lose whenever it happened to be the outer one.
    outer = span(entity_type="IBAN", start=0, end=27, recognizer="catalog:iban")
    inner = span(entity_type="CREDIT_CARD", start=10, end=20, recognizer="catalog:credit_card")

    (kept,) = resolve([outer, inner], specificity=SPEC).spans

    assert (kept.entity_type, kept.start, kept.end) == ("IBAN", 0, 27)


def test_a_bridge_cannot_flip_the_type_when_two_checksums_tie_on_specificity() -> None:
    # The same defect one level down, and the reason the fix is a comparison
    # rather than a condition. A catalog may rate two checksum types equally;
    # rule 4 then settles the pair by sensitivity, so the tier-1 IBAN wins.
    #
    # Nested, the containment branch used a strict specificity `>` and never
    # reached that tie-break, so a broader ORG that sorts first could
    # synthesise an FR_NIR outer and the answer flipped — a span in neither
    # output deciding the reported type, exactly what this fix is for.
    tie = {"IBAN": 80, "FR_NIR": 80, "ORG": 10}
    iban = span(entity_type="IBAN", start=45, end=66, recognizer="catalog:iban", tier=1)
    nir = span(entity_type="FR_NIR", start=45, end=66, recognizer="catalog:fr_nir", tier=2)
    org = span(
        entity_type="ORG", start=41, end=66, confidence=0.9, recognizer="ner:gliner", tier=3
    )

    (alone,) = resolve([iban, nir], specificity=tie).spans
    assert alone.entity_type == "IBAN"

    (kept,) = resolve([iban, nir, org], specificity=tie).spans
    assert (kept.entity_type, kept.start, kept.end) == ("IBAN", 41, 66)


def test_confidence_settles_nested_untouchables_under_a_custom_predicate() -> None:
    # The default predicate calls a span untouchable only at confidence 1.0, so
    # two of them never differ and this step of the ordering is unreachable —
    # which is why removing it left every test passing. `resolve` takes the
    # predicate as a parameter, and a caller that admits lower confidences
    # needs the same answer nested as unnested.
    catalog_backed = lambda candidate: candidate.recognizer.startswith("catalog:")  # noqa: E731
    tie = {"IBAN": 80, "FR_NIR": 80}
    outer = span(entity_type="FR_NIR", start=0, end=20, confidence=0.6, recognizer="catalog:a")
    inner = span(entity_type="IBAN", start=5, end=15, confidence=0.9, recognizer="catalog:b")

    (kept,) = resolve(
        [outer, inner], specificity=tie, untouchable=catalog_backed
    ).spans

    assert (kept.entity_type, kept.start, kept.end) == ("IBAN", 0, 20)


def test_a_bridge_cannot_lend_its_confidence_to_the_span_that_replaces_it() -> None:
    # The third layer of the same defect. `_union` took `max` confidence, so a
    # merge handed its output a number belonging to the span it dropped — and
    # `_outranks` then compared the next span against that borrowed number.
    # The bridge lost every rule, was in no output, and still chose the type.
    catalog_backed = lambda candidate: candidate.recognizer.startswith("catalog:")  # noqa: E731
    tie = {"AAA": 80, "BBB": 80, "ORG": 10}
    weaker = span(entity_type="AAA", start=5, end=15, confidence=0.6, recognizer="catalog:a")
    stronger = span(entity_type="BBB", start=5, end=15, confidence=0.8, recognizer="catalog:b")
    bridge = span(
        entity_type="ORG", start=0, end=15, confidence=0.9, recognizer="ner:gliner", tier=3
    )

    (alone,) = resolve([weaker, stronger], specificity=tie, untouchable=catalog_backed).spans
    assert alone.entity_type == "BBB"

    (kept,) = resolve(
        [weaker, stronger, bridge], specificity=tie, untouchable=catalog_backed
    ).spans
    assert (kept.entity_type, kept.start, kept.end) == ("BBB", 0, 15)


def test_the_trace_names_the_discriminator_that_actually_decided() -> None:
    # `Resolution.trace` is the decision-evidence interface and the sandbox
    # reads it. A merge settled by sensitivity that reports `specific-inner-merge`
    # is a false explanation of a correct answer.
    tie = {"IBAN": 80, "FR_NIR": 80}
    outer = span(entity_type="FR_NIR", start=0, end=20, recognizer="catalog:fr_nir", tier=2)
    inner = span(entity_type="IBAN", start=5, end=15, recognizer="catalog:iban", tier=1)

    (decision,) = resolve([outer, inner], specificity=tie).trace

    assert decision.rule == "nested-sensitivity-merge"
    assert decision.kept.entity_type == "IBAN"


def test_a_same_type_merge_keeps_the_more_confident_reading_under_a_custom_predicate() -> None:
    # Rule 2 promises max confidence, and it picks its winner by untouchability
    # first — so a custom predicate can make a 0.6 catalog span win the identity
    # over a 0.9 model span of the same type. Taking the winner's confidence
    # there lowers a number the documented rule says is the maximum.
    #
    # Different-type merges are the opposite case: the confidence belongs to the
    # reading that survived, because the other reading is gone. Same type means
    # both readings agree, and the merge is of evidence rather than between it.
    catalog_backed = lambda candidate: candidate.recognizer.startswith("catalog:")  # noqa: E731
    quiet = span(entity_type="IBAN", start=0, end=10, confidence=0.6, recognizer="catalog:a")
    loud = span(entity_type="IBAN", start=5, end=15, confidence=0.9, recognizer="ner:gliner")

    (kept,) = resolve([quiet, loud], specificity=SPEC, untouchable=catalog_backed).spans

    assert kept.recognizer == "catalog:a", "the untouchable reading lost its identity"
    assert kept.confidence == 0.9, "a same-type merge lowered the confidence"
    assert (kept.start, kept.end) == (0, 15)


def test_a_merge_does_not_inherit_a_boost_from_the_reading_it_dropped() -> None:
    # The same shape as the borrowed confidence, one field over, and it was left
    # behind when that one was fixed: `boosted` is still an `or` across both
    # inputs, so a merged span can report that its confidence was raised by
    # context when the boost belonged to the reading that lost.
    #
    # It produces a record the deterministic layer cannot: a boost never applies
    # at confidence 1.0 — "a boost must never fabricate checksum status" — so
    # `confidence=1.0, boosted=True` is a combination no catalog rule emits.
    boosted_outer = span(
        entity_type="ORG", start=0, end=20, confidence=0.85, recognizer="catalog:org", boosted=True
    )
    checksum_inner = span(
        entity_type="IBAN", start=5, end=15, confidence=1.0, recognizer="catalog:iban", tier=1
    )

    (kept,) = resolve([boosted_outer, checksum_inner], specificity=SPEC).spans

    assert (kept.entity_type, kept.confidence) == ("IBAN", 1.0)
    assert not kept.boosted, "a merged span claimed a boost belonging to the reading it dropped"


def test_a_same_type_merge_reports_the_boost_belonging_to_the_confidence_it_took() -> None:
    # Rule 2 takes the maximum confidence, so the boost flag has to be the one
    # attached to *that* number. `or` across both inputs was right by accident
    # in one direction and wrong in the other: a merge whose maximum came from
    # an unboosted reading would still have claimed a boost.
    catalog_backed = lambda candidate: candidate.recognizer.startswith("catalog:")  # noqa: E731

    # The winner takes the identity by untouchability, and the maximum comes
    # from the reading it beat — which was boosted, so the record must say so.
    quiet = span(entity_type="IBAN", start=0, end=10, confidence=0.6, recognizer="catalog:a")
    loud = span(
        entity_type="IBAN", start=5, end=15, confidence=0.9, recognizer="ner:g", boosted=True
    )
    (kept,) = resolve([quiet, loud], specificity=SPEC, untouchable=catalog_backed).spans
    assert (kept.confidence, kept.boosted) == (0.9, True)

    # The other direction, which `or` got wrong: the winner is boosted at 0.85
    # and the maximum comes from an unboosted 0.9.
    boosted_winner = span(
        entity_type="IBAN", start=0, end=10, confidence=0.85, recognizer="catalog:a", boosted=True
    )
    plain = span(entity_type="IBAN", start=5, end=15, confidence=0.9, recognizer="ner:g")
    (kept,) = resolve(
        [boosted_winner, plain], specificity=SPEC, untouchable=catalog_backed
    ).spans
    assert (kept.confidence, kept.boosted) == (0.9, False)


def test_a_new_span_can_never_unmask_what_another_span_marked() -> None:
    """Rule 4's invariant, stated over the whole function rather than a branch.

    The defect this pins is not a wrong type but a *shorter mask*: rule 4 used
    to return the winning span whole, so a span arriving beside another and
    beating it on specificity, confidence or rule 1 took the text with it and
    left the loser's remainder in the clear. Adding a detection removed
    masking, which inverts what the layer is for.

    Checked as a property over every rule-4 shape instead of one example,
    because the three branches failed the same way and a test per example is
    how two of them came to be named for a shape they did not build.
    """
    location = span(entity_type="LOCATION", start=10, end=40, confidence=0.8,
                    recognizer="ner:g", tier=2)
    cases = [
        # (challenger, which branch it takes)
        (span(entity_type="PERSON", start=30, end=45, confidence=0.8,
              recognizer="ner:g", tier=2), "specificity"),
        (span(entity_type="OTHER", start=30, end=45, confidence=0.9,
              recognizer="ner:g", tier=2), "confidence"),
        (span(entity_type="IBAN", start=30, end=45, confidence=1.0,
              recognizer="catalog:iban", tier=1), "untouchable-wins"),
    ]
    specificity = {"PERSON": 30, "LOCATION": 20, "OTHER": 20, "IBAN": 20}
    for challenger, expected_rule in cases:
        alone = resolve([location], specificity=specificity).spans
        together = resolve([location, challenger], specificity=specificity)
        (kept,) = together.spans
        assert [decision.rule for decision in together.trace] == [expected_rule]
        for before in alone:
            assert kept.start <= before.start and kept.end >= before.end, (
                f"{expected_rule} unmasked characters that {before.entity_type} had marked: "
                f"[{before.start}:{before.end}] survives only as [{kept.start}:{kept.end}]"
            )
        assert kept.end >= challenger.end and kept.start <= location.start


def test_the_winner_still_names_a_widened_span() -> None:
    # Widening is about extent only. A rule 4 that took the union *and* the
    # loser's identity would fix the exposure and break the audit record, so
    # the two halves are pinned apart.
    location = span(entity_type="LOCATION", start=10, end=40, confidence=0.8,
                    recognizer="ner:g", tier=2)
    person = span(entity_type="PERSON", start=30, end=45, confidence=0.55,
                  recognizer="ner:g", tier=2, boosted=True)
    (kept,) = resolve([location, person], specificity={"PERSON": 30, "LOCATION": 20}).spans
    assert (kept.entity_type, kept.recognizer, kept.tier) == ("PERSON", "ner:g", 2)
    assert (kept.confidence, kept.boosted) == (0.55, True)
    assert (kept.start, kept.end) == (10, 45)
