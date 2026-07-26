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
    validator: str
    specificity: int


def _load_rules(catalog_text: str) -> tuple[Rule, ...]:
    catalog = yaml.safe_load(catalog_text)
    rules = []
    for entry in catalog["identifiers"]:
        validator = entry["validator"]
        if validator not in VALIDATORS:
            raise ValueError(f"identifier {entry['id']!r} names unknown validator {validator!r}")
        flags = re.IGNORECASE if entry.get("case_insensitive") else re.NOFLAG
        rules.append(
            Rule(
                id=entry["id"],
                entity_type=entry["entity_type"],
                tier=entry["tier"],
                pattern=re.compile(entry["pattern"], flags),
                validator=validator,
                specificity=entry.get("specificity", 50),
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
    shrunk = False
    while True:
        if (not shrunk or rule.pattern.fullmatch(candidate)) and VALIDATORS[rule.validator](
            candidate
        ):
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
                        confidence=1.0,
                        recognizer=f"catalog:{rule.id}",
                        tier=rule.tier,
                    )
                )
        specificity = {rule.entity_type: rule.specificity for rule in self.rules}
        return resolve(spans, specificity=specificity).spans
