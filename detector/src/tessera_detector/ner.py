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
    seen_labels: set[str] = set()
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
        # The recognizer maps a model result back to its type by label, so a
        # shared label would make every earlier type undetectable.
        if label in seen_labels:
            raise ValueError(f"ner type {entity_type!r} reuses the label {label!r}")
        seen_labels.add(label)
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


# Character budgets for the first pass, with an overlap wide enough that a name
# split by one cut survives in the neighbouring chunk. Characters are only a
# proxy for tokens, so a second, token-exact pass bounds every chunk below.
CHUNK_SIZE = 1200
CHUNK_OVERLAP = 200
# Tokens the label prompt and the special tokens consume before any text does;
# subtracted from the model's window so a dense chunk cannot silently truncate.
PROMPT_TOKEN_RESERVE = 64
TOKEN_OVERLAP = 32


def token_windows(
    offsets: list[tuple[int, int]], *, budget: int, overlap: int
) -> list[tuple[int, int]]:
    """Group token offsets into (char_start, char_end) windows of at most budget tokens.

    Character counts only approximate token counts: dense text can exceed the
    model's window inside one chunk, and inference then truncates the tail
    silently — the characters between the truncation point and the next chunk's
    start would never be looked at.
    """
    if budget <= 0 or overlap < 0 or overlap >= budget:
        raise ValueError(f"invalid token window: budget={budget!r}, overlap={overlap!r}")
    if not offsets:
        return []
    windows: list[tuple[int, int]] = []
    start = 0
    while start < len(offsets):
        end = min(start + budget, len(offsets))
        windows.append((offsets[start][0], offsets[end - 1][1]))
        if end >= len(offsets):
            break
        start = max(end - overlap, start + 1)
    return windows


class GlinerRecognizer:
    def __init__(self, model_path: Path, types: tuple[NerType, ...] | None = None) -> None:
        # Imported lazily: the base install does not carry the ner group, and
        # `import tessera_detector.ner` must keep working without it.
        from gliner import GLiNER

        self.model_path = model_path
        self.types = types or load_ner_types()
        self.specificity: Mapping[str, int] = {t.entity_type: t.specificity for t in self.types}
        self._by_label = {t.label: t for t in self.types}
        # The weights come from the onnx-community mirror (see models.HF_REPO_ID):
        # the upstream urchade repo ships PyTorch weights only. The mirror's ONNX
        # graph lives under onnx/model.onnx rather than at the repo root, so the
        # default onnx_model_file="model.onnx" must be overridden to match.
        self._model = GLiNER.from_pretrained(
            str(model_path), load_onnx_model=True, onnx_model_file="onnx/model.onnx"
        )
        self._tokenizer = self._model.data_processor.transformer_tokenizer
        self._token_budget = int(self._model.config.max_len) - PROMPT_TOKEN_RESERVE

    def _windows(self, chunk: str) -> list[tuple[int, int]]:
        encoded = self._tokenizer(chunk, return_offsets_mapping=True, add_special_tokens=False)
        offsets = [(int(s), int(e)) for s, e in encoded["offset_mapping"] if e > s]
        return token_windows(offsets, budget=self._token_budget, overlap=TOKEN_OVERLAP)

    def detect(self, text: str) -> list[Span]:
        if not text:
            return []
        labels = [t.label for t in self.types]
        floor = min(t.threshold for t in self.types)
        spans: list[Span] = []
        for offset, chunk in chunks(text, size=CHUNK_SIZE, overlap=CHUNK_OVERLAP):
            for window_start, window_end in self._windows(chunk):
                piece = chunk[window_start:window_end]
                base = offset + window_start
                for entity in self._model.predict_entities(piece, labels, threshold=floor):
                    ner_type = self._by_label.get(entity["label"])
                    score = float(entity["score"])
                    if ner_type is None or score < ner_type.threshold:
                        continue
                    spans.append(
                        Span(
                            entity_type=ner_type.entity_type,
                            start=base + int(entity["start"]),
                            end=base + int(entity["end"]),
                            confidence=min(score, 0.99),
                            recognizer="ner:gliner",
                            tier=ner_type.tier,
                        )
                    )
        return spans


__all__ = ["GlinerRecognizer", "NerType", "chunks", "load_ner_types", "token_windows"]
