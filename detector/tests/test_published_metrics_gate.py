import hashlib
import json
from pathlib import Path

import check_published_metrics as gate
import pytest

from tessera_detector.pipeline import build_detector
from tessera_detector.version import detector_version

MODEL_ID = "probe-weights#probe-deps"


def _measurement() -> dict:
    corpus = gate.CORPUS.read_bytes()
    readme = gate.README.read_text(encoding="utf-8")
    rows = gate.published_rows(readme)
    coverage = gate.COVERAGE.search(readme)
    tier1 = gate.TIER1.search(readme)
    assert coverage is not None and tier1 is not None
    ratio, covered, gold = coverage.groups()
    return {
        "corpus_sha256": hashlib.sha256(corpus).hexdigest(),
        "detector_version": detector_version(
            MODEL_ID, build_detector(ner=False).catalog_text
        ),
        "model_id": MODEL_ID,
        "per_type": {
            entity_type: {
                "precision": float(precision),
                "recall": float(recall),
                "f1": float(f1),
                "tp": 0,
                "fp": 0,
                "fn": 0,
            }
            for entity_type, (precision, recall, f1) in rows.items()
        },
        "article_9_coverage": {
            "ratio": float(ratio),
            "covered": int(covered),
            "gold": int(gold),
        },
        "tier1_recall": 1.0,
        "targets": {"tier1_recall": float(tier1.group(1))},
        "unmasked": {"occurrences": 0, "entities": 0},
    }


def _run(tmp_path: Path, measurement: dict | None, monkeypatch: pytest.MonkeyPatch) -> int:
    path = tmp_path / "metrics.json"
    if measurement is not None:
        path.write_text(json.dumps(measurement), encoding="utf-8")
    monkeypatch.setattr("sys.argv", ["check_published_metrics.py", str(path)])
    return gate.main()


def test_accepts_a_measurement_from_this_tree(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Builds the figures from what the README publishes, so this asserts the
    # gate's own agreement logic rather than today's recall numbers.
    assert _run(tmp_path, _measurement(), monkeypatch) == 0


def test_refuses_a_missing_measurement(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    assert _run(tmp_path, None, monkeypatch) == 1


def test_refuses_another_corpus(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    measurement = _measurement()
    measurement["corpus_sha256"] = hashlib.sha256(b"not the corpus").hexdigest()
    assert _run(tmp_path, measurement, monkeypatch) == 1


def test_refuses_another_detector(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    # The case the corpus digest cannot see: same corpus, different thresholds
    # or rules. Recorded here as a `detector_version` that does not match this
    # tree, which is what an edited `ner.yaml` produces.
    measurement = _measurement()
    measurement["detector_version"] = "0" * 32
    assert _run(tmp_path, measurement, monkeypatch) == 1


def test_refuses_a_measurement_without_a_model_id(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    measurement = _measurement()
    del measurement["model_id"]
    assert _run(tmp_path, measurement, monkeypatch) == 1


def test_refuses_a_disagreeing_row(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    measurement = _measurement()
    measurement["per_type"]["PERSON"]["recall"] = 0.123
    assert _run(tmp_path, measurement, monkeypatch) == 1


def test_refuses_a_row_the_readme_does_not_publish(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    measurement = _measurement()
    measurement["per_type"]["INVENTED_TYPE"] = {
        "precision": 1.0,
        "recall": 1.0,
        "f1": 1.0,
        "tp": 1,
        "fp": 0,
        "fn": 0,
    }
    assert _run(tmp_path, measurement, monkeypatch) == 1


def test_refuses_a_tier1_recall_under_the_published_target(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    measurement = _measurement()
    measurement["tier1_recall"] = 0.5
    assert _run(tmp_path, measurement, monkeypatch) == 1


def test_refuses_a_target_the_readme_does_not_publish(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    measurement = _measurement()
    measurement["targets"]["tier1_recall"] = 0.5
    assert _run(tmp_path, measurement, monkeypatch) == 1


def test_refuses_disagreeing_article_9_coverage(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    measurement = _measurement()
    measurement["article_9_coverage"]["covered"] += 1
    assert _run(tmp_path, measurement, monkeypatch) == 1
