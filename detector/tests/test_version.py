"""The version is what the gateway keys its span cache by, so it must change
whenever the same text would produce different spans."""

from pathlib import Path

import pytest

from tessera_detector.deterministic import DeterministicDetector
from tessera_detector.version import detector_version, source_digest, version_from


def test_the_same_inputs_give_the_same_version():
    first = version_from("gliner@abc123", [b"identifiers", b"ner"])
    second = version_from("gliner@abc123", [b"identifiers", b"ner"])
    assert first == second


def test_changing_the_weights_changes_the_version():
    pinned = version_from("gliner@abc123", [b"identifiers", b"ner"])
    bumped = version_from("gliner@def456", [b"identifiers", b"ner"])
    assert pinned != bumped


def test_editing_either_catalog_changes_the_version():
    # A threshold edit changes what is detected without touching HF_REVISION.
    # If the version missed that, the gateway would serve spans from the old
    # thresholds until its cache aged out.
    base = version_from("gliner@abc123", [b"identifiers", b"ner"])
    first_edited = version_from("gliner@abc123", [b"identifiers!", b"ner"])
    second_edited = version_from("gliner@abc123", [b"identifiers", b"ner!"])
    assert base != first_edited
    assert base != second_edited
    assert first_edited != second_edited


def test_catalog_order_is_not_a_concatenation_accident():
    # Hashing each catalog before folding it in means a byte moving across the
    # boundary between two catalogs cannot leave the version unchanged.
    assert version_from("m", [b"ab", b"c"]) != version_from("m", [b"a", b"bc"])


def test_the_real_catalogs_report_a_stable_version():
    catalog_text = DeterministicDetector().catalog_text
    assert detector_version("m", catalog_text) == detector_version("m", catalog_text)
    assert len(detector_version("m", catalog_text)) == 32


def test_a_different_model_id_changes_the_version():
    # `model_id` has no default: this is what a caller who forgets it would
    # have silently lost — two different loaded weights folding into the
    # same version.
    assert detector_version("gliner@abc123", "cat") != detector_version("gliner@def456", "cat")


def test_a_different_catalog_text_changes_the_version():
    # Finding I: `catalog_text` has no default either, for the same reason
    # `model_id` doesn't. Before this, `detector_version` re-read the
    # packaged identifiers.yaml regardless of what a `Detector` actually
    # loaded, so an application's own catalog changed what was detected
    # while the version stayed put.
    assert detector_version("m", "catalog-a") != detector_version("m", "catalog-b")


def test_the_same_catalog_text_gives_the_same_version():
    assert detector_version("m", "catalog-a") == detector_version("m", "catalog-a")


def test_source_digest_is_stable_for_the_same_tree(tmp_path: Path) -> None:
    (tmp_path / "a.py").write_text("x = 1\n")
    (tmp_path / "b.py").write_text("y = 2\n")
    assert source_digest(tmp_path) == source_digest(tmp_path)


def test_editing_a_source_file_changes_source_digest(tmp_path: Path) -> None:
    # Finding B, proved directly: chunking, the token window, conflict
    # resolution all live in this package's .py files, and model_id plus
    # the catalogs cover none of it. An edit anywhere in the tree — not
    # just the file this test happens to touch — has to move the digest.
    (tmp_path / "pipeline.py").write_text("def detect(): return []\n")
    before = source_digest(tmp_path)
    (tmp_path / "pipeline.py").write_text("def detect(): return ['changed']\n")
    after = source_digest(tmp_path)
    assert before != after


def test_source_digest_covers_every_py_file_not_just_one(tmp_path: Path) -> None:
    (tmp_path / "a.py").write_text("x = 1\n")
    (tmp_path / "b.py").write_text("y = 2\n")
    base = source_digest(tmp_path)
    (tmp_path / "b.py").write_text("y = 3\n")
    assert source_digest(tmp_path) != base


def test_source_digest_ignores_non_python_files(tmp_path: Path) -> None:
    (tmp_path / "a.py").write_text("x = 1\n")
    (tmp_path / "notes.txt").write_text("irrelevant")
    base = source_digest(tmp_path)
    (tmp_path / "notes.txt").write_text("still irrelevant, but different")
    assert source_digest(tmp_path) == base


def test_the_real_package_reports_a_stable_source_digest() -> None:
    # No root passed: this is the production path, over the actual
    # installed tessera_detector sources.
    assert source_digest() == source_digest()
    assert len(source_digest()) == 64


def test_detector_version_changes_when_the_source_tree_does(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    import tessera_detector.version as version_module

    monkeypatch.setattr(version_module, "source_digest", lambda: "source-a")
    first = detector_version("m", "cat")
    monkeypatch.setattr(version_module, "source_digest", lambda: "source-b")
    second = detector_version("m", "cat")
    assert first != second
