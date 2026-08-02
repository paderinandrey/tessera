"""Where the NER weights live. They are never committed — hundreds of megabytes.

Lookup order is TESSERA_NER_MODEL, then the user cache. A variable pointing at a
missing path is an error: a typo must not quietly downgrade the pipeline to the
deterministic layer alone.
"""

import os
from pathlib import Path

MODEL_NAME = "gliner_multi-v2.1"
# The upstream `urchade/gliner_multi-v2.1` repo publishes PyTorch weights only.
# This mirror re-exports the same model with ONNX graphs (`onnx/model.onnx` and
# quantized variants), which is what `GlinerRecognizer` loads via onnxruntime.
HF_REPO_ID = f"onnx-community/{MODEL_NAME}"
# What GlinerRecognizer needs to load. snapshot_download creates the directory
# before it finishes, so directory existence alone would let an interrupted
# download masquerade as installed weights and crash the loader instead of
# falling back to the deterministic layer.
REQUIRED_ARTIFACTS = ("onnx/model.onnx", "config.json")


class ModelUnavailable(Exception):
    """The NER layer was required but no weights were found."""


def model_cache_dir() -> Path:
    return Path.home() / ".cache" / "tessera" / "models" / MODEL_NAME


def _missing_artifacts(path: Path) -> list[str]:
    return [name for name in REQUIRED_ARTIFACTS if not (path / name).is_file()]


def find_model() -> Path | None:
    override = os.environ.get("TESSERA_NER_MODEL")
    if override:
        path = Path(override)
        if not path.exists():
            raise ValueError(f"TESSERA_NER_MODEL points at a missing path: {path}")
        missing = _missing_artifacts(path)
        if missing:
            raise ValueError(
                f"TESSERA_NER_MODEL points at {path}, which is missing {', '.join(missing)}"
            )
        return path
    cached = model_cache_dir()
    if not cached.is_dir() or _missing_artifacts(cached):
        return None
    return cached


__all__ = ["HF_REPO_ID", "MODEL_NAME", "ModelUnavailable", "find_model", "model_cache_dir"]
