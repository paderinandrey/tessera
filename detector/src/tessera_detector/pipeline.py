"""Detection pipeline: deterministic catalog plus optional NER, resolved once.

Conflicts between layers only resolve correctly when resolution sees every span
at once — an NER guess must be able to lose to an overlapping checksum span — so
neither layer resolves on its own (REQ-1, REQ-8).
"""

from collections.abc import Mapping
from importlib.metadata import PackageNotFoundError
from typing import Protocol

from .deterministic import DeterministicDetector
from .models import (
    HF_REVISION,
    MODEL_NAME,
    ModelUnavailable,
    dependency_digest,
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

# This package's own installed distribution name — the root whose declared
# `[project.dependencies]` (pydantic, python-stdnum, schwifty, pyyaml)
# `dependency_digest` walks to cover the deterministic layer's own
# third-party validators. Naming schwifty and stdnum here by hand would
# repeat, a fifth time in this slice, the exact mistake REQUIRED_ARTIFACTS,
# the quantization filenames and the earlier inference-package list already
# made once each (see models.py): correct today, silently under-covering
# the day someone adds a validator dependency. Naming the package instead
# means whatever `[project.dependencies]` declares is picked up
# automatically, the same way `_weighed_files` replaced a hand-maintained
# artifact list with a directory walk.
PACKAGE_NAME = "tessera-detector"


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

    @property
    def catalog_text(self) -> str:
        """The identifiers.yaml text `self.deterministic` actually parsed —
        a caller's own catalog, or the packaged default `DeterministicDetector`
        already resolved when none was given. `detector_version` needs this,
        not the package's own copy: an application supplying `catalog_text`
        gets detection from those rules, and the version has to change when
        that external file does, the same way it already changes for the
        weights, the source and both dependency digests. Delegated rather
        than duplicated, so there is exactly one place this can drift from
        `self.rules`.
        """
        return self.deterministic.catalog_text

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
    # The deterministic layer runs on every request this function can ever
    # produce a Detector for — ner=False, no weights, and no runtime alike —
    # so its own dependency digest belongs in every branch below, not only
    # the one where NER is active. A rebuild that moves schwifty or
    # python-stdnum to a different release changes which IBANs and tax
    # numbers validate, with weights, catalogs and this package's own
    # source all untouched; the version has to see that regardless of
    # whether NER happens to be running too. Computed once, here, the same
    # reasoning weights_digest and recognizer.dependency_digest already
    # apply to their own inputs: metadata reads, not per-request work.
    deterministic_deps = dependency_digest(PACKAGE_NAME)
    if ner is False:
        return Detector(
            catalog_text=catalog_text,
            ner_off_reason=DISABLED,
            model_id=f"{DEFAULT_MODEL_ID}#{deterministic_deps}",
        )
    path = find_model()
    if path is None:
        if ner is True:
            raise ModelUnavailable(
                f"no NER weights found; run `make model` or set TESSERA_NER_MODEL "
                f"(looked in {model_cache_dir()})"
            )
        return Detector(
            catalog_text=catalog_text,
            ner_off_reason=NO_WEIGHTS,
            model_id=f"{DEFAULT_MODEL_ID}#{deterministic_deps}",
        )
    # Imported lazily: this path only runs once weights exist, and the ner
    # dependency group (gliner) need not be installed until it does. Weights
    # outlive virtualenvs — a base-synced environment with a populated cache
    # must degrade like a missing install, not crash on the import.
    try:
        from .ner import GlinerRecognizer

        recognizer = GlinerRecognizer(path)
    except PackageNotFoundError:
        # `PackageNotFoundError` subclasses `ModuleNotFoundError`, which
        # subclasses `ImportError` — so the broader `except ImportError`
        # below would catch it too, and report NO_RUNTIME: "the ner
        # dependency group is not installed." That is not what this is.
        # `GlinerRecognizer.__init__` reaches this deep into `dependency_digest`
        # only after `from gliner import GLiNER` and `GLiNER.from_pretrained`
        # have both already succeeded — the runtime is genuinely present.
        # A `PackageNotFoundError` here means a distribution in its
        # dependency tree has unreadable metadata (stripped `.dist-info` in
        # a slim image, a renamed root), which is `dependency_digest`'s own
        # fail-loud posture, not a missing-runtime signal. Narrowing the
        # `try` to just the `from .ner import GlinerRecognizer` line cannot
        # fix this: the genuine "ner group absent" case this handler exists
        # for is `GLiNER.from_pretrained`'s own `ImportError`, raised from
        # inside `GlinerRecognizer.__init__` on the very next line, not from
        # the module import — narrowing the `try` there would silently stop
        # catching that case too. Distinguishing by exception type rather
        # than by code position is what lets both live in the same `try`:
        # listed first, so Python's first-match-wins ordering peels this
        # off before the `except ImportError` below ever sees it.
        raise
    except ImportError as error:
        if ner is True:
            raise ModelUnavailable(
                f"NER weights are installed but the ner dependency group is not "
                f"(`uv sync --group ner`): {error}"
            ) from error
        return Detector(
            catalog_text=catalog_text,
            ner_off_reason=NO_RUNTIME,
            model_id=f"{DEFAULT_MODEL_ID}#{deterministic_deps}",
        )
    # Named by what is actually loaded, not by the pinned snapshot's constant:
    # `path` may be the cache or an operator's `TESSERA_NER_MODEL` override,
    # and the same path can hold different bytes across a redeploy. Hashed
    # once, here, rather than per request — see `weights_digest`.
    #
    # `#` separates all three digests unambiguously: each is hex, so none
    # can ever contain the character its neighbours split on — no
    # anti-concatenation hashing needed the way `version_from` needs it for
    # arbitrary-length catalog bytes. `deterministic_deps` covers what
    # `[project.dependencies]` names (schwifty, python-stdnum and friends,
    # see `PACKAGE_NAME`); `recognizer.dependency_digest` covers a fourth
    # thing none of weights, catalogs or that list do: a rebuild that moves
    # GLiNER, onnxruntime or the tokenizer library to a different release
    # can change spans with everything else untouched (see its own
    # docstring in models.py).
    return Detector(
        catalog_text=catalog_text,
        recognizer=recognizer,
        model_id=(
            f"{MODEL_NAME}@{weights_digest(path)}#{deterministic_deps}"
            f"#{recognizer.dependency_digest}"
        ),
    )


__all__ = [
    "DEFAULT_MODEL_ID",
    "DISABLED",
    "NO_RUNTIME",
    "NO_WEIGHTS",
    "PACKAGE_NAME",
    "Detector",
    "NerRecognizer",
    "build_detector",
]
