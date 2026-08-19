import hashlib
import sys
import types
from importlib import metadata
from pathlib import Path

import pytest

from tessera_detector.models import (
    HF_REVISION,
    MODEL_NAME,
    _normalize_distribution_name,
    _transitive_requirements,
    _weighed_files,
    dependency_digest,
    find_model,
    model_cache_dir,
    weights_digest,
)


def test_cache_dir_is_under_the_user_cache(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("TESSERA_NER_MODEL", raising=False)
    parts = model_cache_dir().parts
    assert parts[-3:-1] == ("tessera", "models")
    # The revision is in the directory name so a bump cannot serve stale weights.
    assert parts[-1] == f"{MODEL_NAME}@{HF_REVISION[:12]}"


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
    cache = root / ".cache" / "tessera" / "models" / f"{MODEL_NAME}@{HF_REVISION[:12]}"
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
    (tmp_path / ".cache" / "tessera" / "models" / f"{MODEL_NAME}@{HF_REVISION[:12]}").mkdir(
        parents=True
    )
    assert find_model() is None


def test_env_var_without_the_graph_is_an_error(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Explicitly pointed somewhere unusable: say so instead of degrading quietly.
    monkeypatch.setenv("TESSERA_NER_MODEL", str(tmp_path))
    with pytest.raises(ValueError, match="TESSERA_NER_MODEL"):
        find_model()


def test_a_revision_bump_invalidates_the_old_cache(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Weights cached before a bump must not be served afterwards: identical
    # Tessera commits have to run identical weights for the metrics to mean
    # anything.
    monkeypatch.delenv("TESSERA_NER_MODEL", raising=False)
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: tmp_path))
    stale = tmp_path / ".cache" / "tessera" / "models" / f"{MODEL_NAME}@0000deadbeef"
    (stale / "onnx").mkdir(parents=True)
    (stale / "onnx" / "model.onnx").write_bytes(b"old graph")
    (stale / "config.json").write_text("{}", encoding="utf-8")
    assert find_model() is None


def _weights(path: Path, onnx_bytes: bytes, config_bytes: bytes = b"{}") -> Path:
    (path / "onnx").mkdir(parents=True)
    (path / "onnx" / "model.onnx").write_bytes(onnx_bytes)
    (path / "config.json").write_bytes(config_bytes)
    return path


def test_weights_digest_is_stable_for_the_same_bytes(tmp_path: Path) -> None:
    a = _weights(tmp_path / "a", b"graph-bytes")
    b = _weights(tmp_path / "b", b"graph-bytes")
    assert weights_digest(a) == weights_digest(b)


def test_weights_digest_changes_with_the_onnx_graph(tmp_path: Path) -> None:
    a = _weights(tmp_path / "a", b"graph-v1")
    b = _weights(tmp_path / "b", b"graph-v2")
    assert weights_digest(a) != weights_digest(b)


def test_weights_digest_changes_with_config_alone(tmp_path: Path) -> None:
    # The graph can be byte-identical while config.json's label map or
    # thresholds differ; identity has to track every required artifact, not
    # just the largest one.
    a = _weights(tmp_path / "a", b"graph-bytes", config_bytes=b'{"v": 1}')
    b = _weights(tmp_path / "b", b"graph-bytes", config_bytes=b'{"v": 2}')
    assert weights_digest(a) != weights_digest(b)


def test_weights_digest_does_not_depend_on_path(tmp_path: Path) -> None:
    # Named for what the reviewer's finding calls out directly: a path alone
    # is a weak identifier, since the same path can hold different bytes
    # across a redeploy. This is the converse check — different paths,
    # same bytes, same identity — which a path-based identifier would fail.
    a = _weights(tmp_path / "nested" / "a", b"graph-bytes")
    b = _weights(tmp_path / "elsewhere" / "b", b"graph-bytes")
    assert weights_digest(a) == weights_digest(b)


def test_weights_digest_changes_with_a_tokenizer_file_alone(tmp_path: Path) -> None:
    # Follow-up finding: REQUIRED_ARTIFACTS names only the graph and
    # config.json, but GLiNER.from_pretrained loads the whole directory —
    # the tokenizer's offset mapping is what the gateway's caches spans
    # come out of. A digest that only covered REQUIRED_ARTIFACTS would
    # report the same identity for two directories whose tokenizer differs,
    # exactly the failure class P1 closed for the graph alone.
    a = _weights(tmp_path / "a", b"graph-bytes")
    (a / "tokenizer.json").write_bytes(b"tokenizer-v1")
    b = _weights(tmp_path / "b", b"graph-bytes")
    (b / "tokenizer.json").write_bytes(b"tokenizer-v2")
    assert weights_digest(a) != weights_digest(b)


def test_weights_digest_ignores_download_bookkeeping(tmp_path: Path) -> None:
    # huggingface_hub's own `.cache/huggingface/download/*.metadata` sidecars
    # carry a download timestamp and are never read by the loader; including
    # them would make two downloads of byte-identical weights report
    # different identities depending on when they happened to be fetched.
    a = _weights(tmp_path / "a", b"graph-bytes")
    (a / ".cache" / "huggingface" / "download").mkdir(parents=True)
    (a / ".cache" / "huggingface" / "download" / "config.json.metadata").write_text(
        "1111111111.111111"
    )
    b = _weights(tmp_path / "b", b"graph-bytes")
    (b / ".cache" / "huggingface" / "download").mkdir(parents=True)
    (b / ".cache" / "huggingface" / "download" / "config.json.metadata").write_text(
        "9999999999.999999"
    )
    assert weights_digest(a) == weights_digest(b)


def test_weights_digest_ignores_unused_onnx_quantizations(tmp_path: Path) -> None:
    # The mirror ships fp16 and int8 graphs alongside the fp32 one ner.py
    # actually loads (ONNX_MODEL_FILE). Hashing them would nearly double
    # weights_digest's dominant cost for a change that cannot affect a
    # single span, since ner.py never opens them.
    a = _weights(tmp_path / "a", b"graph-bytes")
    (a / "onnx" / "model_int8.onnx").write_bytes(b"int8-v1")
    b = _weights(tmp_path / "b", b"graph-bytes")
    (b / "onnx" / "model_int8.onnx").write_bytes(b"int8-v2")
    assert weights_digest(a) == weights_digest(b)


def test_weighed_files_walks_beyond_required_artifacts(tmp_path: Path) -> None:
    # Direct check on the walk itself, named for the root cause the finding
    # called out: REQUIRED_ARTIFACTS answers "did the download finish", not
    # "what determines the output", and a digest built from the former
    # under-covers the latter. Adding a file the walk was never told about
    # must still be picked up — the whole point of walking instead of
    # enumerating a second list that can drift from the first.
    weights = _weights(tmp_path / "weights", b"graph-bytes")
    (weights / "tokenizer.json").write_bytes(b"tok")
    (weights / "brand_new_artifact.bin").write_bytes(b"surprise")
    names = {f.relative_to(weights).as_posix() for f in _weighed_files(weights)}
    assert "tokenizer.json" in names
    assert "brand_new_artifact.bin" in names


def test_dependency_digest_is_stable_for_the_same_root() -> None:
    # "pydantic" is a base dependency (pyproject.toml), always installed.
    assert dependency_digest("pydantic") == dependency_digest("pydantic")


def test_dependency_digest_does_not_depend_on_process_history() -> None:
    # The defect this replaced: a `sys.modules` diff around the load found
    # a different answer depending on what the process had already
    # imported by the time it ran — a second construction in one process
    # found everything already imported and reported nothing new. Reading
    # installed package metadata instead has no process state to depend
    # on. Simulated directly rather than by importing something and hoping
    # it was not already loaded: stuff a name into `sys.modules` between
    # the two calls, standing in for whatever the process happens to
    # import between two constructions in real life.
    first = dependency_digest("pydantic")
    marker = "a_module_dependency_digest_must_not_notice"
    sys.modules[marker] = types.ModuleType(marker)
    try:
        second = dependency_digest("pydantic")
    finally:
        del sys.modules[marker]
    assert first == second


def test_dependency_digest_changes_with_the_root() -> None:
    # "httpx" is a base dependency of the serve group, always installed
    # alongside pydantic in this suite's environment.
    assert dependency_digest("pydantic") != dependency_digest("httpx")


def test_dependency_digest_raises_for_a_missing_root() -> None:
    # The finding this replaced a first attempt at: a missing root, a typo
    # in the root name, and a genuinely dependency-free package used to
    # produce the identical digest — sha256 of the empty set — because a
    # PackageNotFoundError on the root was swallowed the same way an
    # unresolvable transitive dependency was. "Could not determine the
    # dependencies" must not read as "the dependencies are these: none".
    with pytest.raises(metadata.PackageNotFoundError):
        dependency_digest("not-a-real-package-xyz")


def test_dependency_digest_of_a_dependency_free_package_is_a_real_digest() -> None:
    # PyYAML declares no runtime dependencies at all (metadata.requires
    # returns None for it) — the legitimate case the fix above must not
    # collide with. It must not raise, and its digest must not be the
    # empty-set value either, or it would still be indistinguishable from
    # the failure case this fix exists to separate out.
    empty = hashlib.sha256(b"").hexdigest()
    digest = dependency_digest("PyYAML")
    assert digest != empty
    assert digest == dependency_digest("PyYAML")


@pytest.mark.ner
def test_dependency_digest_tolerates_platform_gated_requirements() -> None:
    # torch declares its own CUDA-sibling packages behind platform markers
    # (`; platform_system == "Linux"`, and similar) that are never
    # satisfied by this project's own shipped configuration — the pinned
    # torch build is CPU-only (see the CPU-index pin in pyproject.toml) —
    # nor by this dev machine. Those requirements must be recognized as
    # "correctly not part of this install" and skipped, not treated as
    # unresolvable distributions that make the whole digest raise.
    pytest.importorskip("torch")
    assert dependency_digest("torch")


def test_transitive_requirements_includes_a_real_dependency() -> None:
    # pydantic-core is a real, non-optional dependency of pydantic —
    # walking one level deep is the whole point of "transitive".
    assert "pydantic-core" in _transitive_requirements("pydantic")


def test_transitive_requirements_excludes_extras() -> None:
    # pydantic declares `email-validator` only behind `extra == "email"`,
    # which nothing in this project activates — a plain install never
    # pulls it in, and this must not either.
    assert "email-validator" not in _transitive_requirements("pydantic")


def test_normalize_distribution_name_folds_case_and_separators() -> None:
    # PEP 503: "PyYAML", "pyyaml" and "py_yaml" all name the same
    # distribution. Without this, the same package reached under two
    # spellings would be walked (and hashed) as if it were two different
    # ones.
    assert _normalize_distribution_name("PyYAML") == _normalize_distribution_name("pyyaml")
    assert _normalize_distribution_name("typing_extensions") == _normalize_distribution_name(
        "typing-extensions"
    )


