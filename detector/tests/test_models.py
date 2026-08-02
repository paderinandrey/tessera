from pathlib import Path

import pytest

from tessera_detector.models import MODEL_NAME, find_model, model_cache_dir


def test_cache_dir_is_under_the_user_cache(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("TESSERA_NER_MODEL", raising=False)
    assert model_cache_dir().parts[-3:] == ("tessera", "models", MODEL_NAME)


def test_env_var_wins(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("TESSERA_NER_MODEL", str(tmp_path))
    assert find_model() == tmp_path


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
