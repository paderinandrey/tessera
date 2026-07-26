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
from .validators import VALIDATORS

_DEFAULT_CATALOG = resources.files("tessera_detector") / "catalog" / "identifiers.yaml"


@dataclass(frozen=True, slots=True)
class Rule:
    id: str
    entity_type: str
    tier: int
    pattern: re.Pattern[str]
    validator: str | None
    specificity: int
    confidence: float


def _load_rules(catalog_text: str) -> tuple[Rule, ...]:
    catalog = yaml.safe_load(catalog_text)
    rules = []
    for entry in catalog["identifiers"]:
        validator = entry.get("validator")
        if validator is not None and validator not in VALIDATORS:
            raise ValueError(f"identifier {entry['id']!r} names unknown validator {validator!r}")
        confidence = entry.get("confidence", 1.0)
        if not 0.0 <= confidence <= 1.0:
            raise ValueError(
                f"identifier {entry['id']!r} declares confidence {confidence} "
                "outside the [0.0, 1.0] range"
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
            )
        )
    return tuple(rules)


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
                start, end = norm.to_original(match.start(), match.start() + len(validated))
                spans.append(
                    Span(
                        entity_type=rule.entity_type,
                        start=start,
                        end=end,
                        confidence=rule.confidence,
                        recognizer=f"catalog:{rule.id}",
                        tier=rule.tier,
                    )
                )
        specificity = {rule.entity_type: rule.specificity for rule in self.rules}
        return resolve(spans, specificity=specificity).spans
