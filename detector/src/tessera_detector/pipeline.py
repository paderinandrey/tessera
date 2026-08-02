"""Detection pipeline: deterministic catalog plus optional NER, resolved once.

Conflicts between layers only resolve correctly when resolution sees every span
at once — an NER guess must be able to lose to an overlapping checksum span — so
neither layer resolves on its own (REQ-1, REQ-8).
"""

from collections.abc import Mapping
from typing import Protocol

from .deterministic import DeterministicDetector
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


__all__ = ["Detector", "NerRecognizer"]
