"""NER layer: zero-shot type configuration and the model adapter (REQ-1, REQ-4).

Types are data: entity type, the label handed to the model, its threshold, tier and
specificity all come from ner.yaml, so adding a type never touches this module.
"""

from dataclasses import dataclass
from importlib import resources

import yaml

_DEFAULT_CONFIG = resources.files("tessera_detector") / "catalog" / "ner.yaml"


@dataclass(frozen=True, slots=True)
class NerType:
    entity_type: str
    label: str
    threshold: float
    tier: int
    specificity: int


def load_ner_types(config_text: str | None = None) -> tuple[NerType, ...]:
    if config_text is None:
        config_text = _DEFAULT_CONFIG.read_text(encoding="utf-8")
    config = yaml.safe_load(config_text)
    types: list[NerType] = []
    seen: set[str] = set()
    for entry in config["entities"]:
        entity_type = entry["entity_type"]
        if entity_type in seen:
            raise ValueError(f"ner config declares {entity_type!r} twice")
        seen.add(entity_type)
        if "threshold" not in entry:
            raise ValueError(f"ner type {entity_type!r} declares no threshold")
        threshold = entry["threshold"]
        if (
            isinstance(threshold, bool)
            or not isinstance(threshold, int | float)
            or not 0.0 < threshold <= 1.0
        ):
            raise ValueError(
                f"ner type {entity_type!r} declares threshold {threshold!r} "
                "outside the (0.0, 1.0] range"
            )
        tier = entry["tier"]
        if isinstance(tier, bool) or not isinstance(tier, int) or not 1 <= tier <= 3:
            raise ValueError(f"ner type {entity_type!r} declares tier {tier!r} outside 1..3")
        label = entry["label"]
        if not isinstance(label, str) or not label.strip():
            raise ValueError(f"ner type {entity_type!r} declares an empty label")
        types.append(
            NerType(
                entity_type=entity_type,
                label=label,
                threshold=float(threshold),
                tier=tier,
                specificity=entry["specificity"],
            )
        )
    return tuple(types)


__all__ = ["NerType", "load_ner_types"]
