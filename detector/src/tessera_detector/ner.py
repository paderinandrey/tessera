"""NER layer: zero-shot type configuration and the model adapter (REQ-1, REQ-4).

Types are data: entity type, the label handed to the model, its threshold, tier and
specificity all come from ner.yaml, so adding a type never touches this module.
"""

from collections.abc import Mapping
from dataclasses import dataclass
from importlib import resources
from pathlib import Path

import yaml

from .spans import Span

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
        if "specificity" not in entry:
            raise ValueError(f"ner type {entity_type!r} declares no specificity")
        specificity = entry["specificity"]
        if isinstance(specificity, bool) or not isinstance(specificity, int) or specificity < 0:
            raise ValueError(
                f"ner type {entity_type!r} declares specificity {specificity!r} "
                "outside the non-negative integer range"
            )
        types.append(
            NerType(
                entity_type=entity_type,
                label=label,
                threshold=float(threshold),
                tier=tier,
                specificity=specificity,
            )
        )
    return tuple(types)


# Boundaries to prefer when cutting, best first: a chunk that ends mid-entity
# costs a detection, so cuts land on paragraph, line or word breaks.
_BOUNDARIES = ("\n\n", "\n", " ")


def chunks(text: str, *, size: int, overlap: int) -> list[tuple[int, str]]:
    if size <= 0 or overlap < 0 or overlap >= size:
        raise ValueError(f"invalid chunk window: size={size!r}, overlap={overlap!r}")
    if not text:
        return []
    if len(text) <= size:
        return [(0, text)]
    result: list[tuple[int, str]] = []
    start = 0
    while start < len(text):
        end = min(start + size, len(text))
        if end < len(text):
            # Only accept a boundary in the last quarter of the window: cutting
            # too early would shrink chunks until progress stalls.
            floor = start + (size * 3) // 4
            for boundary in _BOUNDARIES:
                cut = text.rfind(boundary, floor, end)
                if cut != -1:
                    end = cut + len(boundary)
                    break
        result.append((start, text[start:end]))
        if end >= len(text):
            break
        start = max(end - overlap, start + 1)
    return result


# The model's token window is far shorter than a real document; these are
# character budgets, chosen well inside it, with an overlap wide enough that a
# name split by one cut survives in the neighbouring chunk.
CHUNK_SIZE = 1200
CHUNK_OVERLAP = 200


class GlinerRecognizer:
    def __init__(self, model_path: Path, types: tuple[NerType, ...] | None = None) -> None:
        # Imported lazily: the base install does not carry the ner group, and
        # `import tessera_detector.ner` must keep working without it.
        from gliner import GLiNER

        self.model_path = model_path
        self.types = types or load_ner_types()
        self.specificity: Mapping[str, int] = {t.entity_type: t.specificity for t in self.types}
        self._by_label = {t.label: t for t in self.types}
        # `urchade/gliner_multi-v2.1` ships only PyTorch weights (no model.onnx),
        # and converting one would need the separate `onnx` package on top of the
        # declared ner group (gliner, onnxruntime, huggingface-hub) — so this loads
        # the standard PyTorch backend rather than `load_onnx_model=True`. See the
        # task report for the inspected API and the reasoning.
        self._model = GLiNER.from_pretrained(str(model_path))

    def detect(self, text: str) -> list[Span]:
        if not text:
            return []
        labels = [t.label for t in self.types]
        floor = min(t.threshold for t in self.types)
        spans: list[Span] = []
        for offset, chunk in chunks(text, size=CHUNK_SIZE, overlap=CHUNK_OVERLAP):
            for entity in self._model.predict_entities(chunk, labels, threshold=floor):
                ner_type = self._by_label.get(entity["label"])
                score = float(entity["score"])
                if ner_type is None or score < ner_type.threshold:
                    continue
                spans.append(
                    Span(
                        entity_type=ner_type.entity_type,
                        start=offset + int(entity["start"]),
                        end=offset + int(entity["end"]),
                        confidence=min(score, 0.99),
                        recognizer="ner:gliner",
                        tier=ner_type.tier,
                    )
                )
        return spans


__all__ = ["GlinerRecognizer", "NerType", "chunks", "load_ner_types"]
