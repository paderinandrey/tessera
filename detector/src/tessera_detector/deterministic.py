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


def _load_rules(catalog_text: str) -> tuple[Rule, ...]:
    catalog = yaml.safe_load(catalog_text)
    rules = []
    for entry in catalog["identifiers"]:
        validator = entry["validator"]
        if validator not in VALIDATORS:
            raise ValueError(f"identifier {entry['id']!r} names unknown validator {validator!r}")
        rules.append(
            Rule(
                id=entry["id"],
                entity_type=entry["entity_type"],
                tier=entry["tier"],
                pattern=re.compile(entry["pattern"]),
                validator=validator,
            )
        )
    return tuple(rules)


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
            validate = VALIDATORS[rule.validator]
            for match in rule.pattern.finditer(norm.text):
                if not validate(match.group(0)):
                    continue
                start, end = norm.to_original(match.start(), match.end())
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
        return sorted(spans, key=lambda s: (s.start, s.end))
