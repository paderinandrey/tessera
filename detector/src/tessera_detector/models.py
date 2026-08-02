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


class ModelUnavailable(Exception):
    """The NER layer was required but no weights were found."""


def model_cache_dir() -> Path:
    return Path.home() / ".cache" / "tessera" / "models" / MODEL_NAME


def find_model() -> Path | None:
    override = os.environ.get("TESSERA_NER_MODEL")
    if override:
        path = Path(override)
        if not path.exists():
            raise ValueError(f"TESSERA_NER_MODEL points at a missing path: {path}")
        return path
    cached = model_cache_dir()
    return cached if cached.exists() else None


__all__ = ["HF_REPO_ID", "MODEL_NAME", "ModelUnavailable", "find_model", "model_cache_dir"]
