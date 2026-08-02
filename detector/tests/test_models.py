from pathlib import Path

import pytest

from tessera_detector.models import MODEL_NAME, find_model, model_cache_dir


def test_cache_dir_is_under_the_user_cache(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("TESSERA_NER_MODEL", raising=False)
    assert model_cache_dir().parts[-3:] == ("tessera", "models", MODEL_NAME)


def test_env_var_wins(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    override = tmp_path / "elsewhere"
    (override / "onnx").mkdir(parents=True)
    (override / "onnx" / "model.onnx").write_bytes(b"graph")
    (override / "config.json").write_text("{}", encoding="utf-8")
    monkeypatch.setenv("TESSERA_NER_MODEL", str(override))
    assert find_model() == override


def test_env_var_pointing_nowhere_is_an_error(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # A typo in the variable must not silently disable the layer.
    monkeypatch.setenv("TESSERA_NER_MODEL", str(tmp_path / "nope"))
    with pytest.raises(ValueError, match="TESSERA_NER_MODEL"):
        find_model()


def test_missing_cache_yields_none(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("TESSERA_NER_MODEL", raising=False)
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: tmp_path))
    assert find_model() is None


def _install(root: Path) -> Path:
    cache = root / ".cache" / "tessera" / "models" / MODEL_NAME
    (cache / "onnx").mkdir(parents=True)
    (cache / "onnx" / "model.onnx").write_bytes(b"graph")
    (cache / "config.json").write_text("{}", encoding="utf-8")
    return cache


def test_complete_cache_is_found(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("TESSERA_NER_MODEL", raising=False)
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: tmp_path))
    cache = _install(tmp_path)
    assert find_model() == cache


def test_interrupted_download_is_not_treated_as_installed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # snapshot_download creates the directory before it finishes: an aborted
    # run must fall back to deterministic-only, not crash the loader later.
    monkeypatch.delenv("TESSERA_NER_MODEL", raising=False)
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: tmp_path))
    (tmp_path / ".cache" / "tessera" / "models" / MODEL_NAME).mkdir(parents=True)
    assert find_model() is None


def test_env_var_without_the_graph_is_an_error(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Explicitly pointed somewhere unusable: say so instead of degrading quietly.
    monkeypatch.setenv("TESSERA_NER_MODEL", str(tmp_path))
    with pytest.raises(ValueError, match="TESSERA_NER_MODEL"):
        find_model()
