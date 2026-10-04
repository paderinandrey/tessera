import hashlib
from pathlib import Path

import pytest
from source_digest import ROOT, source_digest, source_paths


def _relative() -> set[str]:
    return {path.relative_to(ROOT).as_posix() for path in source_paths()}


def test_digest_is_stable() -> None:
    assert source_digest() == source_digest()


@pytest.mark.parametrize(
    "path",
    [
        # The thresholds the published figures are measured at. If either YAML
        # leaves this set, an edited bar stops invalidating a measurement and
        # the gate goes green on numbers the detector no longer produces.
        "detector/src/tessera_detector/catalog/ner.yaml",
        "detector/src/tessera_detector/catalog/identifiers.yaml",
        # The resolution rules, which decide which label wins a span.
        "detector/src/tessera_detector/resolution.py",
        "detector/src/tessera_detector/pipeline.py",
        # Computes the figures themselves, including the IoU threshold.
        "detector/src/tessera_detector/evaluation.py",
        "evaluation/evaluate.py",
    ],
)
def test_digest_covers(path: str) -> None:
    assert path in _relative()


def test_digest_covers_every_package_source() -> None:
    # Named as a set rather than counted: a file added to the package joins the
    # digest without anyone remembering to list it here.
    package = ROOT / "detector" / "src" / "tessera_detector"
    expected = {
        p.relative_to(ROOT).as_posix()
        for p in package.rglob("*")
        if p.is_file() and "__pycache__" not in p.parts and p.suffix != ".pyc"
    }
    assert expected <= _relative()


def test_digest_excludes_compiled_artefacts() -> None:
    # They are gitignored and rebuilt per interpreter, so including them would
    # invalidate a measurement for reasons that are not about the detector.
    assert not [p for p in _relative() if "__pycache__" in p or p.endswith(".pyc")]


def test_digest_follows_content(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    package = tmp_path / "pkg"
    package.mkdir()
    (package / "catalog.yaml").write_text("threshold: 0.75\n", encoding="utf-8")
    evaluator = tmp_path / "evaluate.py"
    evaluator.write_text("x = 1\n", encoding="utf-8")
    monkeypatch.setattr("source_digest.ROOT", tmp_path)
    monkeypatch.setattr("source_digest.PACKAGE", package)
    monkeypatch.setattr("source_digest.EVALUATOR", evaluator)

    before = source_digest()
    (package / "catalog.yaml").write_text("threshold: 0.76\n", encoding="utf-8")
    assert source_digest() != before


def test_digest_follows_the_file_set(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    # Two trees holding the same bytes under different names must not agree:
    # the path is part of what is hashed, so a rule moved between modules is a
    # change the digest sees.
    package = tmp_path / "pkg"
    package.mkdir()
    (package / "a.py").write_text("x = 1\n", encoding="utf-8")
    evaluator = tmp_path / "evaluate.py"
    evaluator.write_text("y = 2\n", encoding="utf-8")
    monkeypatch.setattr("source_digest.ROOT", tmp_path)
    monkeypatch.setattr("source_digest.PACKAGE", package)
    monkeypatch.setattr("source_digest.EVALUATOR", evaluator)

    before = source_digest()
    (package / "a.py").rename(package / "b.py")
    assert source_digest() != before


def test_digest_is_not_a_bare_concatenation(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Separating path from content keeps two different trees from colliding by
    # splitting the same bytes differently across names.
    package = tmp_path / "pkg"
    package.mkdir()
    (package / "ab").write_text("", encoding="utf-8")
    evaluator = tmp_path / "evaluate.py"
    evaluator.write_text("", encoding="utf-8")
    monkeypatch.setattr("source_digest.ROOT", tmp_path)
    monkeypatch.setattr("source_digest.PACKAGE", package)
    monkeypatch.setattr("source_digest.EVALUATOR", evaluator)
    first = source_digest()

    (package / "ab").unlink()
    (package / "a").write_text("b", encoding="utf-8")
    assert source_digest() != first
    assert first != hashlib.sha256(b"").hexdigest()
