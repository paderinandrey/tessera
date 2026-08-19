import functools
import sys
from collections.abc import Mapping
from importlib.metadata import PackageNotFoundError
from pathlib import Path

import pytest

from tessera_detector.models import (
    MODEL_NAME,
    ModelUnavailable,
    _transitive_requirements,
    dependency_digest,
    weights_digest,
)
from tessera_detector.pipeline import DEFAULT_MODEL_ID, PACKAGE_NAME, Detector, build_detector
from tessera_detector.spans import Span


class FakeRecognizer:
    def __init__(self, spans: list[Span], specificity: Mapping[str, int] | None = None) -> None:
        self._spans = spans
        self.specificity: Mapping[str, int] = specificity or {"PERSON": 30}

    def detect(self, text: str) -> list[Span]:
        return list(self._spans)


def person(start: int, end: int, confidence: float = 0.9) -> Span:
    return Span(
        entity_type="PERSON",
        start=start,
        end=end,
        confidence=confidence,
        recognizer="ner:fake",
        tier=2,
    )


def test_without_recognizer_only_deterministic_spans() -> None:
    detector = Detector()
    spans = detector.detect("mail: anna.keller@example.ch")
    assert detector.ner_available is False
    assert [s.entity_type for s in spans] == ["EMAIL"]


def test_detector_catalog_text_is_the_deterministic_layers_own() -> None:
    # Finding I: version.detector_version needs this, not a second read of
    # the packaged identifiers.yaml — delegated to the object that actually
    # parsed it rather than duplicated onto Detector itself.
    assert Detector().catalog_text == Detector().deterministic.catalog_text


def test_detector_catalog_text_reflects_a_custom_catalog() -> None:
    catalog = """
version: 1
identifiers:
  - id: naked
    entity_type: NAKED
    tier: 2
    confidence: 0.5
    pattern: 'x+'
"""
    assert Detector(catalog_text=catalog).catalog_text == catalog


def test_ner_spans_join_the_result() -> None:
    detector = Detector(recognizer=FakeRecognizer([person(0, 5)]))
    spans = detector.detect("Keller a écrit à anna.keller@example.ch")
    assert detector.ner_available is True
    assert sorted(s.entity_type for s in spans) == ["EMAIL", "PERSON"]


def test_checksum_span_survives_an_overlapping_ner_span() -> None:
    # The e-mail is a catalog span; a PERSON claiming the same range must not
    # displace it — cross-layer conflicts are resolved once, over the union.
    text = "mail: anna.keller@example.ch"
    detector = Detector(recognizer=FakeRecognizer([person(6, 28, confidence=0.99)]))
    spans = detector.detect(text)
    assert [(s.entity_type, s.start, s.end) for s in spans] == [("EMAIL", 6, 28)]


def test_deterministic_layer_no_longer_resolves_on_its_own() -> None:
    from tessera_detector.deterministic import DeterministicDetector

    raw = DeterministicDetector().detect("mail: anna.keller@example.ch")
    assert [s.entity_type for s in raw] == ["EMAIL"]
    assert DeterministicDetector().specificity["EMAIL"] == 70


def test_build_detector_off_never_loads_a_model(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("TESSERA_NER_MODEL", "/definitely/not/here")
    detector = build_detector(ner=False)
    assert detector.ner_available is False


def test_build_detector_auto_falls_back_without_weights(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.delenv("TESSERA_NER_MODEL", raising=False)
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: tmp_path))
    detector = build_detector()
    assert detector.ner_available is False
    assert [s.entity_type for s in detector.detect("mail: anna.keller@example.ch")] == ["EMAIL"]


def test_build_detector_required_raises_without_weights(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.delenv("TESSERA_NER_MODEL", raising=False)
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: tmp_path))
    with pytest.raises(ModelUnavailable):
        build_detector(ner=True)


class FakeGlinerRecognizer:
    """Stands in for the real GLiNER model: constructing the real one needs
    gigabytes of ONNX weights this suite does not carry. Patched onto
    `tessera_detector.ner.GlinerRecognizer`, which `build_detector` only
    imports lazily — importing the `.ner` module itself needs no `gliner`
    install, only instantiating the real class does, so this works without
    the optional `ner` dependency group."""

    specificity: Mapping[str, int] = {}

    def __init__(self, model_path: Path, dependency_digest: str = "fake-deps") -> None:
        self.model_path = model_path
        self.dependency_digest = dependency_digest

    def detect(self, text: str) -> list[Span]:
        return []


def _weights(path: Path, onnx_bytes: bytes) -> Path:
    (path / "onnx").mkdir(parents=True)
    (path / "onnx" / "model.onnx").write_bytes(onnx_bytes)
    (path / "config.json").write_bytes(b"{}")
    return path


def test_a_detector_without_ner_reports_the_pinned_constant() -> None:
    # No weights are loaded, so there is nothing to digest instead — the
    # pinned snapshot's own name is the honest answer here, and it is also
    # never a cache key: a deterministic-only run can't satisfy the
    # gateway's "complete run" check. This constructs Detector directly, not
    # through build_detector, so PACKAGE_NAME's dependency digest (Finding
    # I) never enters into it — see the build_detector tests below for that.
    assert Detector().model_id == DEFAULT_MODEL_ID


def test_the_deterministic_root_actually_covers_the_validator_libraries() -> None:
    # Finding I: PACKAGE_NAME has to be the right root, not merely produce
    # *a* digest. validators.py imports schwifty and stdnum (the import
    # name for the python-stdnum distribution); proving both are reachable
    # from PACKAGE_NAME's own declared dependency tree is what tells a
    # correct root apart from a typo or the wrong package that would still
    # look plausible without this.
    tree = _transitive_requirements(PACKAGE_NAME)
    assert "schwifty" in tree
    assert "python-stdnum" in tree


def test_build_detector_folds_the_deterministic_dependency_digest_when_disabled(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # Finding I: the version used to omit the deterministic layer's own
    # dependencies entirely — a rebuild moving schwifty or python-stdnum to
    # a different release changes which IBANs and tax numbers validate,
    # with the reported version unchanged. This must hold even when NER is
    # explicitly turned off: the deterministic layer still runs.
    monkeypatch.setattr("tessera_detector.pipeline.dependency_digest", lambda root: "det-deps-v1")
    detector = build_detector(ner=False)
    assert detector.model_id == f"{DEFAULT_MODEL_ID}#det-deps-v1"


def test_build_detector_folds_the_deterministic_dependency_digest_without_weights(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.delenv("TESSERA_NER_MODEL", raising=False)
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: tmp_path))
    monkeypatch.setattr("tessera_detector.pipeline.dependency_digest", lambda root: "det-deps-v1")
    detector = build_detector()
    assert detector.model_id == f"{DEFAULT_MODEL_ID}#det-deps-v1"


def test_build_detector_folds_the_deterministic_dependency_digest_without_the_runtime(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr("tessera_detector.pipeline.find_model", lambda: tmp_path)
    monkeypatch.setitem(sys.modules, "gliner", None)
    monkeypatch.setattr("tessera_detector.pipeline.dependency_digest", lambda root: "det-deps-v1")
    detector = build_detector()
    assert detector.model_id == f"{DEFAULT_MODEL_ID}#det-deps-v1"


def test_a_different_deterministic_dependency_digest_changes_the_model_id_when_ner_is_off(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr("tessera_detector.pipeline.dependency_digest", lambda root: "det-deps-v1")
    off_a = build_detector(ner=False)
    monkeypatch.setattr("tessera_detector.pipeline.dependency_digest", lambda root: "det-deps-v2")
    off_b = build_detector(ner=False)
    assert off_a.model_id != off_b.model_id


def test_build_detector_names_the_weights_actually_loaded(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    weights = _weights(tmp_path / "weights", b"weights-v1")
    monkeypatch.setenv("TESSERA_NER_MODEL", str(weights))
    monkeypatch.setattr("tessera_detector.ner.GlinerRecognizer", FakeGlinerRecognizer)

    detector = build_detector()

    assert detector.ner_available is True
    assert detector.model_id == (
        f"{MODEL_NAME}@{weights_digest(weights)}#{dependency_digest(PACKAGE_NAME)}#fake-deps"
    )
    assert detector.model_id != DEFAULT_MODEL_ID


def test_an_override_pointing_at_different_weights_changes_the_model_id(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # The P1 fix, proved directly: two overrides, two sets of bytes, must
    # produce two different identities — the failure mode was that both
    # reported the same one regardless.
    monkeypatch.setattr("tessera_detector.ner.GlinerRecognizer", FakeGlinerRecognizer)

    a = _weights(tmp_path / "a", b"weights-v1")
    monkeypatch.setenv("TESSERA_NER_MODEL", str(a))
    detector_a = build_detector()

    b = _weights(tmp_path / "b", b"weights-v2")
    monkeypatch.setenv("TESSERA_NER_MODEL", str(b))
    detector_b = build_detector()

    assert detector_a.model_id != detector_b.model_id


def test_the_same_weight_bytes_report_the_same_model_id_from_a_different_path(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # The companion the first mutation alone would miss: identity has to
    # track the bytes, not the path they happen to sit at, or a redeploy to
    # a new directory with byte-identical weights would look like a version
    # change with nothing behind it.
    monkeypatch.setattr("tessera_detector.ner.GlinerRecognizer", FakeGlinerRecognizer)

    a = _weights(tmp_path / "a", b"weights-v1")
    monkeypatch.setenv("TESSERA_NER_MODEL", str(a))
    detector_a = build_detector()

    b = _weights(tmp_path / "elsewhere", b"weights-v1")
    monkeypatch.setenv("TESSERA_NER_MODEL", str(b))
    detector_b = build_detector()

    assert detector_a.model_id == detector_b.model_id


def test_build_detector_folds_the_dependency_digest_into_the_model_id(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    weights = _weights(tmp_path / "weights", b"weights-v1")
    monkeypatch.setenv("TESSERA_NER_MODEL", str(weights))
    monkeypatch.setattr(
        "tessera_detector.ner.GlinerRecognizer",
        functools.partial(FakeGlinerRecognizer, dependency_digest="deps-v1"),
    )

    detector = build_detector()

    assert detector.model_id.endswith("#deps-v1")


def test_a_different_dependency_digest_changes_the_model_id(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Finding F: weights and catalogs unchanged, but the inference
    # dependencies moved — GLiNER, onnxruntime, or the tokenizer library
    # released a new version. The weight bytes are identical here on
    # purpose; only the dependency digest differs, which is what a real
    # rebuild against the same pinned weights would produce.
    weights = _weights(tmp_path / "weights", b"weights-v1")
    monkeypatch.setenv("TESSERA_NER_MODEL", str(weights))

    monkeypatch.setattr(
        "tessera_detector.ner.GlinerRecognizer",
        functools.partial(FakeGlinerRecognizer, dependency_digest="deps-v1"),
    )
    detector_a = build_detector()

    monkeypatch.setattr(
        "tessera_detector.ner.GlinerRecognizer",
        functools.partial(FakeGlinerRecognizer, dependency_digest="deps-v2"),
    )
    detector_b = build_detector()

    assert detector_a.model_id != detector_b.model_id


def test_auto_mode_falls_back_when_the_ner_runtime_is_absent(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Weights survive in the user-wide cache across virtualenv rebuilds; a base
    # install must degrade to deterministic-only, not crash importing gliner.
    monkeypatch.setenv("TESSERA_NER_MODEL", str(tmp_path))
    monkeypatch.setattr(
        "tessera_detector.pipeline.find_model", lambda: tmp_path
    )
    monkeypatch.setitem(sys.modules, "gliner", None)
    detector = build_detector()
    assert detector.ner_available is False


def test_required_mode_reports_a_missing_ner_runtime(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(
        "tessera_detector.pipeline.find_model", lambda: tmp_path
    )
    monkeypatch.setitem(sys.modules, "gliner", None)
    with pytest.raises(ModelUnavailable, match="ner"):
        build_detector(ner=True)


def _raise_package_not_found(model_path: Path) -> None:
    # Stands in for `GlinerRecognizer.__init__` failing inside
    # `dependency_digest`, after `gliner` itself has already imported and
    # loaded successfully — a stripped or corrupted `.dist-info`, not a
    # missing runtime.
    raise PackageNotFoundError("stripped-dist-info")


def test_auto_mode_does_not_swallow_a_metadata_failure(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Finding J: PackageNotFoundError subclasses ModuleNotFoundError, which
    # subclasses ImportError. Before this fix, the bare `except ImportError`
    # around GlinerRecognizer's construction caught this too and reported
    # NO_RUNTIME — the default, auto ner=None case silently started a
    # deterministic-only detector, a masking gap presented as a normal
    # startup. It must instead escape.
    monkeypatch.setattr("tessera_detector.pipeline.find_model", lambda: tmp_path)
    monkeypatch.setattr("tessera_detector.ner.GlinerRecognizer", _raise_package_not_found)
    with pytest.raises(PackageNotFoundError):
        build_detector()


def test_required_mode_also_does_not_swallow_a_metadata_failure(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # The same failure must escape under ner=True too, rather than being
    # reworded into ModelUnavailable's "the ner group is not installed" —
    # that message would be actively misleading for a metadata bug.
    monkeypatch.setattr("tessera_detector.pipeline.find_model", lambda: tmp_path)
    monkeypatch.setattr("tessera_detector.ner.GlinerRecognizer", _raise_package_not_found)
    with pytest.raises(PackageNotFoundError):
        build_detector(ner=True)


def article_9(start: int, end: int, confidence: float = 0.4) -> Span:
    return Span(
        entity_type="TRADE_UNION",
        start=start,
        end=end,
        confidence=confidence,
        recognizer="ner:fake",
        tier=3,
    )


def org(start: int, end: int, confidence: float = 0.8) -> Span:
    return Span(
        entity_type="ORG",
        start=start,
        end=end,
        confidence=confidence,
        recognizer="ner:fake",
        tier=2,
    )


def test_article_9_span_outranks_an_overlapping_org() -> None:
    # "Die Gewerkschaft ver.di": knowing it is a union membership mention is
    # the more sensitive reading, and specificity 35 beats ORG's 10.
    text = "Die Gewerkschaft ver.di hat geantwortet."
    recognizer = FakeRecognizer(
        [org(4, 23), article_9(4, 23)],
        specificity={"ORG": 10, "TRADE_UNION": 35},
    )
    spans = Detector(recognizer=recognizer).detect(text)
    assert [s.entity_type for s in spans] == ["TRADE_UNION"]


def test_checksum_identifier_still_outranks_an_article_9_span() -> None:
    text = "mail: anna.keller@example.ch"
    recognizer = FakeRecognizer(
        [article_9(6, 28, confidence=0.9)], specificity={"TRADE_UNION": 35}
    )
    spans = Detector(recognizer=recognizer).detect(text)
    assert [(s.entity_type, s.start, s.end) for s in spans] == [("EMAIL", 6, 28)]


def test_deterministic_only_still_resolves_conflicts() -> None:
    detector = Detector(recognizer=FakeRecognizer([person(0, 5)]))
    spans = detector.deterministic_only("Keller a écrit à anna.keller@example.ch")
    assert [s.entity_type for s in spans] == ["EMAIL"], "the NER span must not appear"
