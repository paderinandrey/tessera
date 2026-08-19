"""Detection pipeline: deterministic catalog plus optional NER, resolved once.

Conflicts between layers only resolve correctly when resolution sees every span
at once — an NER guess must be able to lose to an overlapping checksum span — so
neither layer resolves on its own (REQ-1, REQ-8).
"""

from collections.abc import Mapping
from typing import Protocol

from .deterministic import DeterministicDetector
from .models import (
    HF_REVISION,
    MODEL_NAME,
    ModelUnavailable,
    find_model,
    model_cache_dir,
    weights_digest,
)
from .resolution import resolve
from .spans import Span

# What a detector reports when no NER weights are loaded at all: the pinned
# snapshot's own name, since no loaded weights exist to digest instead. A
# deterministic-only run never satisfies the gateway's "complete run" check
# (`layers_run` cannot include "ner"), so this string is never a cache key —
# only a label in a response nobody caches against.
DEFAULT_MODEL_ID = f"{MODEL_NAME}@{HF_REVISION}"


class NerRecognizer(Protocol):
    specificity: Mapping[str, int]

    def detect(self, text: str) -> list[Span]: ...


# Why the NER layer is not running. Each points at a different remedy, so
# reports must not collapse them: weights cannot fix a missing runtime, and
# neither is worth mentioning when the caller turned the layer off on purpose.
NO_WEIGHTS = "no weights"
NO_RUNTIME = "no runtime"
DISABLED = "disabled"


class Detector:
    def __init__(
        self,
        catalog_text: str | None = None,
        recognizer: NerRecognizer | None = None,
        ner_off_reason: str = NO_WEIGHTS,
        model_id: str = DEFAULT_MODEL_ID,
    ) -> None:
        self.deterministic = DeterministicDetector(catalog_text)
        self.recognizer = recognizer
        self.ner_off_reason = None if recognizer is not None else ner_off_reason
        # What `detector_version()` digests as the model half of its input.
        # The pinned constant by default; `build_detector` overrides this to
        # the actual loaded weights' digest whenever NER is running, so an
        # operator's `TESSERA_NER_MODEL` override is named honestly rather
        # than reported as the snapshot it replaced.
        self.model_id = model_id

    @property
    def ner_available(self) -> bool:
        return self.recognizer is not None

    def deterministic_only(self, text: str) -> list[Span]:
        """Resolved spans from the catalog layer alone.

        Narrowing to one layer must not also drop conflict resolution: the
        caller asked for fewer detectors, not for raw overlapping spans.
        """
        spans = self.deterministic.detect(text)
        return resolve(spans, specificity=self.deterministic.specificity).spans

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
        return Detector(catalog_text=catalog_text, ner_off_reason=DISABLED)
    path = find_model()
    if path is None:
        if ner is True:
            raise ModelUnavailable(
                f"no NER weights found; run `make model` or set TESSERA_NER_MODEL "
                f"(looked in {model_cache_dir()})"
            )
        return Detector(catalog_text=catalog_text, ner_off_reason=NO_WEIGHTS)
    # Imported lazily: this path only runs once weights exist, and the ner
    # dependency group (gliner) need not be installed until it does. Weights
    # outlive virtualenvs — a base-synced environment with a populated cache
    # must degrade like a missing install, not crash on the import.
    try:
        from .ner import GlinerRecognizer

        recognizer = GlinerRecognizer(path)
    except ImportError as error:
        if ner is True:
            raise ModelUnavailable(
                f"NER weights are installed but the ner dependency group is not "
                f"(`uv sync --group ner`): {error}"
            ) from error
        return Detector(catalog_text=catalog_text, ner_off_reason=NO_RUNTIME)
    # Named by what is actually loaded, not by the pinned snapshot's constant:
    # `path` may be the cache or an operator's `TESSERA_NER_MODEL` override,
    # and the same path can hold different bytes across a redeploy. Hashed
    # once, here, rather than per request — see `weights_digest`.
    return Detector(
        catalog_text=catalog_text,
        recognizer=recognizer,
        model_id=f"{MODEL_NAME}@{weights_digest(path)}",
    )


__all__ = [
    "DEFAULT_MODEL_ID",
    "DISABLED",
    "NO_RUNTIME",
    "NO_WEIGHTS",
    "Detector",
    "NerRecognizer",
    "build_detector",
]
