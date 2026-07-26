"""Deterministic resolution of overlapping spans (REQ-8).

Rules, in precedence order:

1. Checksum spans are untouchable: never silently dropped in favour of a
   non-checksum span.
2. Same type overlapping or nested: union bounds, max confidence.
3. Different types, strict containment: outer wins. Exception per rule 1 — an
   untouchable inner nested in a non-untouchable outer merges to the union bounds
   and keeps the more sensitive type, so the identifier keeps its identity in audit.
4. Different types, partial overlap or equal range: higher specificity wins, then
   higher confidence; on a full tie the spans merge with the more sensitive type
   (lower tier).

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


def _union(a: Span, b: Span, take_type_from: Span) -> Span:
    return Span(
        entity_type=take_type_from.entity_type,
        start=min(a.start, b.start),
        end=max(a.end, b.end),
        confidence=max(a.confidence, b.confidence),
        recognizer=take_type_from.recognizer,
        tier=take_type_from.tier,
        boosted=a.boosted or b.boosted,
    )


def _resolve_pair(
    a: Span,
    b: Span,
    specificity: Mapping[str, int],
    untouchable: Callable[[Span], bool],
) -> tuple[Span, str]:
    if a.entity_type == b.entity_type:
        winner = a if a.confidence >= b.confidence else b
        return _union(a, b, take_type_from=winner), "same-type-merge"

    for outer, inner in ((a, b), (b, a)):
        if _strictly_contains(outer, inner):
            if untouchable(inner) and not untouchable(outer):
                return _union(outer, inner, _more_sensitive(inner, outer)), (
                    "untouchable-inner-merge"
                )
            return outer, "nesting-outer-wins"

    # Rule 1 precedes rule 4: a lone checksum span never loses to a non-checksum one.
    if untouchable(a) != untouchable(b):
        return (a if untouchable(a) else b), "untouchable-wins"

    spec_a = specificity.get(a.entity_type, 0)
    spec_b = specificity.get(b.entity_type, 0)
    if spec_a != spec_b:
        return (a if spec_a > spec_b else b), "specificity"
    if a.confidence != b.confidence:
        return (a if a.confidence > b.confidence else b), "confidence"
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
