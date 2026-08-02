"""Detection pipeline: deterministic catalog plus optional NER, resolved once.

Conflicts between layers only resolve correctly when resolution sees every span
at once — an NER guess must be able to lose to an overlapping checksum span — so
neither layer resolves on its own (REQ-1, REQ-8).
"""

from collections.abc import Mapping
from typing import Protocol

from .deterministic import DeterministicDetector
from .models import ModelUnavailable, find_model, model_cache_dir
from .resolution import resolve
from .spans import Span


class NerRecognizer(Protocol):
    specificity: Mapping[str, int]

    def detect(self, text: str) -> list[Span]: ...


class Detector:
    def __init__(
        self,
        catalog_text: str | None = None,
        recognizer: NerRecognizer | None = None,
    ) -> None:
        self.deterministic = DeterministicDetector(catalog_text)
        self.recognizer = recognizer

    @property
    def ner_available(self) -> bool:
        return self.recognizer is not None

    def detect(self, text: str) -> list[Span]:
        spans = self.deterministic.detect(text)
        specificity = dict(self.deterministic.specificity)
        if self.recognizer is not None:
            spans.extend(self.recognizer.detect(text))
            specificity.update(self.recognizer.specificity)
        return resolve(spans, specificity=specificity).spans


def build_detector(
    *, ner: bool | None = None, catalog_text: str | None = None
) -> Detector:
    """ner=None auto-enables when weights exist, True requires them, False disables."""
    if ner is False:
        return Detector(catalog_text=catalog_text)
    path = find_model()
    if path is None:
        if ner is True:
            raise ModelUnavailable(
                f"no NER weights found; run `make model` or set TESSERA_NER_MODEL "
                f"(looked in {model_cache_dir()})"
            )
        return Detector(catalog_text=catalog_text)
    # Imported lazily: this path only runs once weights exist, and the ner
    # dependency group (gliner) need not be installed until it does.
    from .ner import GlinerRecognizer

    return Detector(catalog_text=catalog_text, recognizer=GlinerRecognizer(path))


__all__ = ["Detector", "NerRecognizer", "build_detector"]
