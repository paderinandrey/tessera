"""Deterministic detection layer: catalog-driven patterns plus checksum validation.

The engine knows nothing about concrete identifiers — types, tiers, patterns and
validator names come from the YAML catalog. Matching runs over normalized text;
resulting spans are reported in original coordinates (REQ-6).
"""

import re
from dataclasses import dataclass
from importlib import resources

import yaml

from .normalize import normalize
from .resolution import resolve
from .spans import Span
from .validators import CHECKSUM_VALIDATORS, VALIDATORS

_DEFAULT_CATALOG = resources.files("tessera_detector") / "catalog" / "identifiers.yaml"


@dataclass(frozen=True, slots=True)
class Boost:
    value: float
    window: int
    triggers: re.Pattern[str]


@dataclass(frozen=True, slots=True)
class Rule:
    id: str
    entity_type: str
    tier: int
    pattern: re.Pattern[str]
    validator: str | None
    specificity: int
    confidence: float
    threshold: float
    boost: Boost | None


def _load_rules(catalog_text: str) -> tuple[Rule, ...]:
    catalog = yaml.safe_load(catalog_text)
    rules = []
    for entry in catalog["identifiers"]:
        validator = entry.get("validator")
        if validator is not None and validator not in VALIDATORS:
            raise ValueError(f"identifier {entry['id']!r} names unknown validator {validator!r}")
        confidence = entry.get("confidence", 1.0)
        threshold = entry.get("threshold", 0.5)
        if (
            isinstance(threshold, bool)
            or not isinstance(threshold, int | float)
            or not 0.0 <= threshold <= 1.0
        ):
            raise ValueError(
                f"identifier {entry['id']!r} declares threshold {threshold!r} "
                "outside the [0.0, 1.0] range"
            )
        if not 0.0 <= confidence <= 1.0:
            raise ValueError(
                f"identifier {entry['id']!r} declares confidence {confidence} "
                "outside the [0.0, 1.0] range"
            )
        if validator in CHECKSUM_VALIDATORS and confidence < 1.0:
            # The reverse guard: checksum-backed spans are untouchable in
            # resolution, and that invariant must not be configurable away.
            raise ValueError(
                f"identifier {entry['id']!r} uses checksum validator {validator!r} "
                "and cannot declare a confidence below 1.0"
            )
        if validator is None and confidence >= 1.0:
            # Confidence 1.0 marks spans untouchable in resolution — a status
            # reserved for checksum-validated rules, never granted by omission.
            raise ValueError(
                f"identifier {entry['id']!r} has no validator and must declare "
                "an explicit confidence below 1.0"
            )
        flags = re.IGNORECASE if entry.get("case_insensitive") else re.NOFLAG
        rules.append(
            Rule(
                id=entry["id"],
                entity_type=entry["entity_type"],
                tier=entry["tier"],
                pattern=re.compile(entry["pattern"], flags),
                validator=validator,
                specificity=entry.get("specificity", 50),
                confidence=confidence,
                threshold=threshold,
                boost=_load_boost(entry.get("boost")),
            )
        )
    return tuple(rules)


def _load_boost(raw: object) -> Boost | None:
    if raw is None:
        return None
    if not isinstance(raw, dict):
        raise ValueError(f"boost must be a mapping, got {type(raw).__name__}")
    value, window, triggers = raw.get("value"), raw.get("window"), raw.get("triggers")
    # isinstance(bool, int) holds in Python, and PyYAML loads bare true/false as bool.
    if isinstance(value, bool) or not isinstance(value, int | float) or not 0.0 <= value <= 1.0:
        # The range check also rejects NaN (all comparisons are false) and inf.
        raise ValueError("boost 'value' must be a finite number in the [0.0, 1.0] range")
    if isinstance(window, bool) or not isinstance(window, int) or window < 1:
        # A zero window would make the [-window:] slice scan the whole prefix.
        raise ValueError("boost 'window' must be a positive integer")
    if not isinstance(triggers, list) or not triggers:
        raise ValueError("boost requires a non-empty 'triggers' list")
    # Both triggers and the context window are canonicalized to \w+ token runs, so
    # "st.-nr" and "St.-Nr." meet in the same form and punctuation never glues
    # separate terms into one window token. Boundary lookarounds keep "st nr" from
    # firing inside "post nr".
    canonical = [_canonical(str(t)) for t in triggers]
    if any(not t for t in canonical):
        raise ValueError("boost 'triggers' must contain word characters")
    alternatives = "|".join(rf"(?<!\w){re.escape(t)}(?!\w)" for t in canonical)
    return Boost(
        value=float(value),
        window=window,
        triggers=re.compile(alternatives),
    )


def _canonical(text: str) -> str:
    return " ".join(re.findall(r"\w+", text.lower()))


def _context_boosted(boost: Boost, text: str, start: int, end: int) -> bool:
    """Look for a trigger within the +-window tokens around a candidate (REQ-7)."""
    before = " ".join(re.findall(r"\w+", text[:start].lower())[-boost.window :])
    after = " ".join(re.findall(r"\w+", text[end:].lower())[: boost.window])
    return boost.triggers.search(f"{before} {after}") is not None


_TOKEN_SEPARATORS = " -."


def _validate_shrinking(rule: Rule, candidate: str) -> str | None:
    """Validate a candidate, shrinking greedy tails.

    A greedy pattern can swallow a following letter token ("BE68 ... 7034 BIC"); the
    full candidate then fails its checksum and the entity would vanish. On failure,
    trailing separator-delimited tokens are stripped and revalidated — but only tokens
    containing a letter: a trailing digit group means the run may be a window of a
    longer number and must not produce a span (digit-run guard philosophy).
    """
    if rule.validator is None:
        return candidate
    validate = VALIDATORS[rule.validator]
    shrunk = False
    while True:
        if (not shrunk or rule.pattern.fullmatch(candidate)) and validate(candidate):
            return candidate
        cut = max(candidate.rfind(sep) for sep in _TOKEN_SEPARATORS)
        if cut <= 0 or not any(ch.isalpha() for ch in candidate[cut + 1 :]):
            return None
        candidate = candidate[:cut]
        shrunk = True


class DeterministicDetector:
    def __init__(self, catalog_text: str | None = None) -> None:
        if catalog_text is None:
            catalog_text = _DEFAULT_CATALOG.read_text(encoding="utf-8")
        self.rules = _load_rules(catalog_text)

    def detect(self, text: str) -> list[Span]:
        if not text:
            return []
        norm = normalize(text)
        spans = []
        for rule in self.rules:
            for match in rule.pattern.finditer(norm.text):
                validated = _validate_shrinking(rule, match.group(0))
                if validated is None:
                    continue
                n_end = match.start() + len(validated)
                confidence, boosted = rule.confidence, False
                if (
                    rule.boost is not None
                    and confidence < 1.0
                    and _context_boosted(rule.boost, norm.text, match.start(), n_end)
                ):
                    # A boost must never fabricate checksum status (confidence 1.0).
                    confidence = min(confidence + rule.boost.value, 0.99)
                    boosted = True
                if confidence < rule.threshold:
                    continue
                start, end = norm.to_original(match.start(), n_end)
                spans.append(
                    Span(
                        entity_type=rule.entity_type,
                        start=start,
                        end=end,
                        confidence=confidence,
                        recognizer=f"catalog:{rule.id}",
                        tier=rule.tier,
                        boosted=boosted,
                    )
                )
        specificity = {rule.entity_type: rule.specificity for rule in self.rules}
        return resolve(spans, specificity=specificity).spans
