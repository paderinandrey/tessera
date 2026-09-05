"""Deterministic resolution of overlapping spans (REQ-8).

Rules, in precedence order:

1. Checksum spans are untouchable: never silently dropped in favour of a
   non-checksum span.
2. Same type overlapping or nested: union bounds, max confidence.
3. Different types, strict containment: outer wins, with two exceptions. Per rule 1,
   an untouchable inner nested in a non-untouchable outer merges to the union bounds
   and keeps the more sensitive type, so the identifier keeps its identity in audit.
   And a strictly more specific inner type does the same: an ORG reading of "la CGT"
   must not erase the trade-union reading of "CGT", because the narrower type is what
   marks the span sensitive. That holds whether or not the outer is untouchable — a
   checksum outer is protected from a *less* specific inner, which says nothing about
   one that outranks it, and an outer can be a span nobody found, since a merge
   synthesises them (#39).
4. Different types, partial overlap or equal range: higher specificity names the
   span, then higher confidence; on a full tie the more sensitive type (lower
   tier) names it. **The extent is always the union**, whichever reading wins.
   Returning the winner whole let a span that arrives beside another and beats it
   take the text with it, leaving the loser's remainder unmasked — so adding a
   detection could subtract masking. No rule here may shrink what the layers
   below marked; widening is over-masking, which REQ-38 counts as irritation,
   and the alternative is exposure.

Every applied rule is recorded in the trace — the sandbox surfaces these decisions.
"""

from collections.abc import Callable, Mapping
from dataclasses import dataclass, field

from .spans import Span


def _is_checksum(span: Span) -> bool:
    return span.recognizer.startswith("catalog:") and span.confidence >= 1.0


@dataclass(frozen=True, slots=True)
class Decision:
    rule: str
    kept: Span
    dropped: tuple[Span, ...]


@dataclass(slots=True)
class Resolution:
    spans: list[Span]
    trace: list[Decision] = field(default_factory=list)


def _sort_key(span: Span) -> tuple[int, int, str, str, float, int, bool]:
    # Total order: resolution outcomes must not depend on set() iteration order.
    return (
        span.start,
        span.start - span.end,  # longer first
        span.entity_type,
        span.recognizer,
        -span.confidence,
        span.tier,
        span.boosted,
    )


def _overlaps(a: Span, b: Span) -> bool:
    return a.start < b.end and b.start < a.end


def _strictly_contains(outer: Span, inner: Span) -> bool:
    return (outer.start, outer.end) != (inner.start, inner.end) and (
        outer.start <= inner.start and outer.end >= inner.end
    )


def _more_sensitive(a: Span, b: Span) -> Span:
    if a.tier != b.tier:
        return a if a.tier < b.tier else b
    return a


def _union(
    a: Span,
    b: Span,
    take_type_from: Span,
    *,
    confidence: float | None = None,
    boosted: bool | None = None,
) -> Span:
    """The two spans' extent, under the surviving reading's identity.

    **Confidence comes from the reading that survived**, not from whichever
    input happened to be surer of itself. It was `max` of the two, so a merge
    could hand its output a number belonging to a span that is not in it — and
    `_outranks` then compared a later span against that borrowed number, letting
    a bridge decide the reported type by proxy after it had been dropped. Every
    other field already comes from `take_type_from`; this one was the exception.

    `boosted` travels with it, for the same reason and one field over: it says
    *this* confidence was raised by surrounding context, so inheriting it from a
    dropped reading produces a record the deterministic layer cannot emit —
    a boost never applies at 1.0, and a merge could report `confidence=1.0,
    boosted=True`.

    **Rule 2 passes both explicitly, and the difference is not an
    exception to that argument but the other side of it.** A same-type merge
    joins two readings that *agree*, so the number is evidence about one
    conclusion and the maximum is the documented answer. Its winner is chosen by
    untouchability before confidence, so under a custom predicate a 0.6 catalog
    reading can take the identity from a 0.9 model reading of the same type —
    and taking the winner's number there would lower a value rule 2 promises is
    the maximum. Everywhere else the losing reading is *gone*, and its
    confidence goes with it.
    """
    return Span(
        entity_type=take_type_from.entity_type,
        start=min(a.start, b.start),
        end=max(a.end, b.end),
        confidence=take_type_from.confidence if confidence is None else confidence,
        recognizer=take_type_from.recognizer,
        tier=take_type_from.tier,
        boosted=take_type_from.boosted if boosted is None else boosted,
    )


def _outranks(challenger: Span, holder: Span, specificity: Mapping[str, int]) -> str | None:
    """Which discriminator makes `challenger`'s reading name the span, or `None`.

    The name is returned rather than a bare `True` because `Resolution.trace` is
    the decision-evidence interface: a merge decided by sensitivity that reports
    `specific-inner-merge` is a false explanation of a correct answer, and the
    sandbox reads these.

    The ordering rule 4 applies to an unnested pair — specificity, then
    confidence, then sensitivity — asked as one question so that the nesting
    branch can reach the same verdict. It was a bespoke `>` on specificity
    there, which meant two checksum types a catalog rates equally were settled
    by sensitivity when they sat side by side and by *which one a merge
    happened to widen* when one contained the other.
    """
    challenger_specificity = specificity.get(challenger.entity_type, 0)
    holder_specificity = specificity.get(holder.entity_type, 0)
    if challenger_specificity != holder_specificity:
        return "specificity" if challenger_specificity > holder_specificity else None
    if challenger.confidence != holder.confidence:
        return "confidence" if challenger.confidence > holder.confidence else None
    return "sensitivity" if challenger.tier < holder.tier else None


def _resolve_pair(
    a: Span,
    b: Span,
    specificity: Mapping[str, int],
    untouchable: Callable[[Span], bool],
) -> tuple[Span, str]:
    if a.entity_type == b.entity_type:
        # The checksum span's audit identity survives the merge (rule 1).
        if untouchable(a) != untouchable(b):
            winner = a if untouchable(a) else b
        else:
            winner = a if a.confidence >= b.confidence else b
        # The maximum confidence and the boost that produced it travel
        # together: `boosted` says *this* number was raised by context, so
        # taking it from anywhere but the reading that supplied the number
        # makes the record disagree with itself. An `or` across both was right
        # by accident when the maximum came from the boosted side and wrong
        # when it did not.
        surest = a if a.confidence >= b.confidence else b
        return _union(
            a,
            b,
            take_type_from=winner,
            confidence=surest.confidence,
            boosted=surest.boosted,
        ), "same-type-merge"

    for outer, inner in ((a, b), (b, a)):
        if _strictly_contains(outer, inner):
            if untouchable(inner) and not untouchable(outer):
                return _union(outer, inner, _more_sensitive(inner, outer)), (
                    "untouchable-inner-merge"
                )
            # A more specific inner type keeps its identity: "la CGT" read as
            # an organization must not erase the trade-union reading of "CGT",
            # because that classification is what marks the span sensitive.
            # Rule 1 still comes first: a checksum outer is never replaced,
            # whatever specificity a catalog assigns the inner type.
            if not untouchable(outer) and specificity.get(inner.entity_type, 0) > specificity.get(
                outer.entity_type, 0
            ):
                return _union(outer, inner, take_type_from=inner), "specific-inner-merge"
            # **Both** untouchable, and the inner is the more specific of the
            # two. Rule 1 is why the inner's own checksum is required and not
            # merely its specificity: a catalog IBAN is not replaced by a
            # model's guess however high a catalog rates that guess's type.
            # `test_checksum_outer_keeps_its_identity_over_a_more_specific_inner`
            # holds that, and caught this branch written without the condition.
            #
            # The guard above excludes both cases to protect a checksum outer
            # from an inner that does *not* outrank it, which is right and says
            # nothing about one that does. `_outranks` is the same ordering
            # rule 4 uses on an unnested pair, so nesting no longer changes the
            # verdict — a bespoke `>` on specificity alone left two equally
            # rated checksum types being settled by which one a merge happened
            # to widen.
            #
            # It matters because the outer here is often not a span anyone
            # found: a merge can synthesise one. `ORG` overlapping a
            # `CREDIT_CARD` produces a card-typed union that then contains an
            # `FR_NIR` reading of the same digits, and without this branch the
            # eighty loses to the forty for no reason but the order the pairs
            # were folded in. See #39.
            if untouchable(outer) and untouchable(inner):
                discriminator = _outranks(inner, outer, specificity)
                if discriminator is not None:
                    return _union(outer, inner, take_type_from=inner), (
                        f"nested-{discriminator}-merge"
                    )
            return outer, "nesting-outer-wins"

    # Rule 4. **Every branch returns the union of the two extents**, and the
    # winner supplies only the identity. Three of them used to return the
    # winning span whole, which drops the loser's characters wherever the two
    # overlap partially rather than exactly — so adding a span could *unmask*
    # text: a `PERSON[30:45]` arriving beside a `LOCATION[10:40]` won on
    # specificity and left 10..30 in the clear. The layer above cannot repair
    # that, because by then the dropped reading is gone.
    #
    # Taking the union widens instead, which costs over-masking (REQ-38's
    # irritation metric) and never exposure — the trade rules 2 and 3 already
    # make everywhere else. Rule 4 was the one place the fold could shrink
    # what the layers below had marked.
    #
    # Ranges that are exactly equal are the only shape the public corpus feeds
    # this branch, and on those the union is the same span, which is why this
    # correction moves no measured number. That is the argument for making it
    # now rather than when something reaches it.
    if untouchable(a) != untouchable(b):
        # Rule 1 precedes rule 4: a lone checksum span never loses its identity
        # to a non-checksum one. Its *extent* still grows to cover both.
        return _union(a, b, take_type_from=(a if untouchable(a) else b)), "untouchable-wins"

    spec_a = specificity.get(a.entity_type, 0)
    spec_b = specificity.get(b.entity_type, 0)
    if spec_a != spec_b:
        return _union(a, b, take_type_from=(a if spec_a > spec_b else b)), "specificity"
    if a.confidence != b.confidence:
        return _union(
            a, b, take_type_from=(a if a.confidence > b.confidence else b)
        ), "confidence"
    return _union(a, b, _more_sensitive(a, b)), "tie-merge-sensitive"


def resolve(
    spans: list[Span],
    *,
    specificity: Mapping[str, int] | None = None,
    untouchable: Callable[[Span], bool] = _is_checksum,
) -> Resolution:
    specificity = specificity or {}
    result = Resolution(spans=sorted(set(spans), key=_sort_key))
    while True:
        conflict = next(
            (
                (i, j)
                for i in range(len(result.spans))
                for j in range(i + 1, len(result.spans))
                if _overlaps(result.spans[i], result.spans[j])
            ),
            None,
        )
        if conflict is None:
            return result
        i, j = conflict
        a, b = result.spans[i], result.spans[j]
        kept, rule = _resolve_pair(a, b, specificity, untouchable)
        dropped = tuple(s for s in (a, b) if s != kept)
        result.trace.append(Decision(rule=rule, kept=kept, dropped=dropped))
        result.spans = sorted(
            [s for k, s in enumerate(result.spans) if k not in (i, j)] + [kept],
            key=_sort_key,
        )
